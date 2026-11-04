//! Core Audio access for ducking (macOS): the processes that use audio, and
//! process taps that measure how loud a set of processes is.
//!
//! A tap copies the audio of some processes without changing where it plays.
//! Taps need macOS 14.2 and the "System Audio Recording" permission, which macOS
//! asks for (on behalf of the app that started tvctl) when the first tap starts.

use std::ffi::{CStr, c_void};
use std::path::{Path, PathBuf};
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, Once, OnceLock};

use anyhow::{Result, anyhow, bail};
use objc2::AnyThread;
use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyObject};
use objc2_core_audio::{
    AudioDeviceCreateIOProcID, AudioDeviceDestroyIOProcID, AudioDeviceIOProcID, AudioDeviceStart,
    AudioDeviceStop, AudioHardwareCreateAggregateDevice, AudioHardwareDestroyAggregateDevice,
    AudioObjectAddPropertyListener, AudioObjectGetPropertyData, AudioObjectGetPropertyDataSize,
    AudioObjectID, AudioObjectPropertyAddress, CATapDescription, CATapMuteBehavior,
    kAudioHardwarePropertyProcessObjectList, kAudioObjectPropertyElementMain,
    kAudioObjectPropertyScopeGlobal, kAudioProcessPropertyBundleID,
    kAudioProcessPropertyIsRunningOutput, kAudioProcessPropertyPID,
};
use objc2_core_audio_types::{AudioBufferList, AudioTimeStamp};
use objc2_core_foundation::{CFDictionary, CFRetained, CFString};
use objc2_foundation::{NSArray, NSBundle, NSDictionary, NSNumber, NSString};

/// `kAudioObjectSystemObject`.
const SYSTEM_OBJECT: AudioObjectID = 1;

/// A process that is a Core Audio client.
#[derive(Debug, Clone)]
pub struct AudioProcess {
    /// Core Audio process object id (what taps refer to).
    pub object: AudioObjectID,
    pub pid: i32,
    pub bundle_id: Option<String>,
    /// The process is playing audio (or holds an output stream open) now.
    pub running_output: bool,
}

fn address(selector: u32) -> AudioObjectPropertyAddress {
    AudioObjectPropertyAddress {
        mSelector: selector,
        mScope: kAudioObjectPropertyScopeGlobal,
        mElement: kAudioObjectPropertyElementMain,
    }
}

/// Reads an array property.
fn property_vec<T: Copy + Default>(object: AudioObjectID, selector: u32) -> Option<Vec<T>> {
    let addr = address(selector);
    let mut size = 0u32;
    // SAFETY: valid pointers to locals; Core Audio writes at most `size` bytes.
    let status = unsafe {
        AudioObjectGetPropertyDataSize(
            object,
            NonNull::from(&addr),
            0,
            std::ptr::null(),
            NonNull::from(&mut size),
        )
    };
    if status != 0 {
        return None;
    }
    let mut items = vec![T::default(); size as usize / size_of::<T>()];
    if items.is_empty() {
        return Some(items);
    }
    let status = unsafe {
        AudioObjectGetPropertyData(
            object,
            NonNull::from(&addr),
            0,
            std::ptr::null(),
            NonNull::from(&mut size),
            NonNull::new(items.as_mut_ptr().cast::<c_void>()).expect("non-null"),
        )
    };
    if status != 0 {
        return None;
    }
    items.truncate(size as usize / size_of::<T>());
    Some(items)
}

/// Reads a fixed-size property.
fn property<T: Copy + Default>(object: AudioObjectID, selector: u32) -> Option<T> {
    let addr = address(selector);
    let mut size = size_of::<T>() as u32;
    let mut value = T::default();
    // SAFETY: `value` has room for `size` bytes.
    let status = unsafe {
        AudioObjectGetPropertyData(
            object,
            NonNull::from(&addr),
            0,
            std::ptr::null(),
            NonNull::from(&mut size),
            NonNull::from(&mut value).cast::<c_void>(),
        )
    };
    (status == 0 && size as usize == size_of::<T>()).then_some(value)
}

/// Reads a CFString property (returned retained by Core Audio).
fn string_property(object: AudioObjectID, selector: u32) -> Option<String> {
    let ptr = property::<usize>(object, selector)?;
    let ptr = NonNull::new(ptr as *mut CFString)?;
    // SAFETY: Core Audio returns a +1 CFStringRef; CFRetained releases it.
    let s = unsafe { CFRetained::from_raw(ptr) };
    Some(s.to_string()).filter(|s| !s.is_empty())
}

/// All processes that currently are Core Audio clients.
pub fn processes() -> Vec<AudioProcess> {
    let objects =
        property_vec::<AudioObjectID>(SYSTEM_OBJECT, kAudioHardwarePropertyProcessObjectList)
            .unwrap_or_default();
    objects
        .into_iter()
        .filter_map(|object| {
            Some(AudioProcess {
                object,
                pid: property::<i32>(object, kAudioProcessPropertyPID).filter(|p| *p > 0)?,
                bundle_id: string_property(object, kAudioProcessPropertyBundleID),
                running_output: running_output(object),
            })
        })
        .collect()
}

/// Whether a process object is playing audio (or holds an output stream open) now.
pub fn running_output(object: AudioObjectID) -> bool {
    property::<u32>(object, kAudioProcessPropertyIsRunningOutput).is_some_and(|v| v != 0)
}

static PROCESS_LIST_CHANGED: AtomicBool = AtomicBool::new(true);

unsafe extern "C-unwind" fn on_process_list_changed(
    _object: AudioObjectID,
    _count: u32,
    _addresses: NonNull<AudioObjectPropertyAddress>,
    _data: *mut c_void,
) -> i32 {
    PROCESS_LIST_CHANGED.store(true, Ordering::SeqCst);
    0
}

/// True once after the set of Core Audio client processes changed (always true
/// the first time).
pub fn take_process_list_changed() -> bool {
    static LISTEN: Once = Once::new();
    LISTEN.call_once(|| {
        let addr = address(kAudioHardwarePropertyProcessObjectList);
        // SAFETY: the listener is a static function without client data.
        unsafe {
            AudioObjectAddPropertyListener(
                SYSTEM_OBJECT,
                NonNull::from(&addr),
                Some(on_process_list_changed),
                std::ptr::null_mut(),
            );
        }
    });
    PROCESS_LIST_CHANGED.swap(false, Ordering::SeqCst)
}

/// What a tap listens to (Core Audio process object ids).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// Only these processes.
    Include(Vec<AudioObjectID>),
    /// Every process except these.
    Exclude(Vec<AudioObjectID>),
}

type CreateTapFn = unsafe extern "C-unwind" fn(*const AnyObject, *mut AudioObjectID) -> i32;
type DestroyTapFn = unsafe extern "C-unwind" fn(AudioObjectID) -> i32;

/// The tap functions, looked up at run time so tvctl still starts on macOS
/// versions without them (linking them directly would fail at launch).
fn tap_functions() -> Option<(CreateTapFn, DestroyTapFn)> {
    static FNS: OnceLock<Option<(CreateTapFn, DestroyTapFn)>> = OnceLock::new();
    *FNS.get_or_init(|| {
        let create = lookup(c"AudioHardwareCreateProcessTap")?;
        let destroy = lookup(c"AudioHardwareDestroyProcessTap")?;
        // SAFETY: the symbols have these C signatures (CoreAudio/AudioHardware.h).
        unsafe {
            Some((
                std::mem::transmute::<*mut c_void, CreateTapFn>(create),
                std::mem::transmute::<*mut c_void, DestroyTapFn>(destroy),
            ))
        }
    })
}

fn lookup(name: &CStr) -> Option<*mut c_void> {
    // SAFETY: dlsym with a valid C string.
    let ptr = unsafe { libc::dlsym(libc::RTLD_DEFAULT, name.as_ptr()) };
    (!ptr.is_null()).then_some(ptr)
}

/// A running tap and the private aggregate device it is read through. Both
/// disappear when dropped (or when the process exits).
pub struct Tap {
    tap: AudioObjectID,
    device: AudioObjectID,
    io_proc: AudioDeviceIOProcID,
    peak: *const AtomicU32,
    destroy_tap: DestroyTapFn,
}

// SAFETY: the raw pointer is an `Arc<AtomicU32>` owned by this value.
unsafe impl Send for Tap {}

unsafe extern "C-unwind" fn measure_peak(
    _device: AudioObjectID,
    _now: NonNull<AudioTimeStamp>,
    input: NonNull<AudioBufferList>,
    _input_time: NonNull<AudioTimeStamp>,
    _output: NonNull<AudioBufferList>,
    _output_time: NonNull<AudioTimeStamp>,
    data: *mut c_void,
) -> i32 {
    // SAFETY: `data` is the tap's `AtomicU32`, alive until the IOProc is destroyed;
    // the buffer list is valid for this call and holds f32 samples.
    unsafe {
        let peak_slot = &*(data as *const AtomicU32);
        let list = input.as_ref();
        let buffers =
            std::slice::from_raw_parts(list.mBuffers.as_ptr(), list.mNumberBuffers as usize);
        let mut peak = 0f32;
        for b in buffers {
            if b.mData.is_null() {
                continue;
            }
            let samples = std::slice::from_raw_parts(
                b.mData as *const f32,
                b.mDataByteSize as usize / size_of::<f32>(),
            );
            for s in samples {
                peak = peak.max(s.abs());
            }
        }
        if peak > f32::from_bits(peak_slot.load(Ordering::Relaxed)) {
            peak_slot.store(peak.to_bits(), Ordering::Relaxed);
        }
    }
    0
}

fn any<T: objc2::Message>(object: Retained<T>) -> Retained<AnyObject> {
    // SAFETY: every Objective-C object is an AnyObject.
    unsafe { Retained::cast_unchecked(object) }
}

fn dictionary(
    entries: Vec<(&str, Retained<AnyObject>)>,
) -> Retained<NSDictionary<NSString, AnyObject>> {
    let keys: Vec<Retained<NSString>> =
        entries.iter().map(|(k, _)| NSString::from_str(k)).collect();
    let key_refs: Vec<&NSString> = keys.iter().map(|k| &**k).collect();
    let values: Vec<Retained<AnyObject>> = entries.into_iter().map(|(_, v)| v).collect();
    NSDictionary::from_retained_objects(&key_refs, &values)
}

impl Tap {
    /// Starts a mono tap of `target`. Blocks while macOS asks the user for the
    /// audio recording permission.
    pub fn start(target: &Target) -> Result<Tap> {
        let (create_tap, destroy_tap) = tap_functions()
            .ok_or_else(|| anyhow!("ducking needs macOS 14.2 or later (process taps)"))?;
        if AnyClass::get(c"CATapDescription").is_none() {
            bail!("ducking needs macOS 14.2 or later (process taps)");
        }
        let ids: Vec<Retained<NSNumber>> = match target {
            Target::Include(ids) | Target::Exclude(ids) => {
                ids.iter().map(|id| NSNumber::new_u32(*id)).collect()
            }
        };
        let ids = NSArray::from_retained_slice(&ids);
        // SAFETY: CATapDescription exists (checked above); arguments are valid.
        let description = unsafe {
            let d = match target {
                Target::Include(_) => {
                    CATapDescription::initMonoMixdownOfProcesses(CATapDescription::alloc(), &ids)
                }
                Target::Exclude(_) => CATapDescription::initMonoGlobalTapButExcludeProcesses(
                    CATapDescription::alloc(),
                    &ids,
                ),
            };
            d.setPrivate(true);
            d.setMuteBehavior(CATapMuteBehavior::Unmuted);
            d.setName(&NSString::from_str("tvctl duck"));
            d
        };
        let mut tap: AudioObjectID = 0;
        // SAFETY: valid description and out pointer.
        let status = unsafe { create_tap(Retained::as_ptr(&description).cast(), &mut tap) };
        if status != 0 {
            bail!("could not create the audio tap (OSStatus {status})");
        }
        // SAFETY: the description is a valid object.
        let tap_uid = unsafe { description.UUID().UUIDString() };
        let tap_entry = dictionary(vec![
            ("uid", any(tap_uid)),
            ("drift", any(NSNumber::new_bool(true))),
        ]);
        let uid = format!("tvctl-duck-{}-{tap}", std::process::id());
        let description = dictionary(vec![
            ("uid", any(NSString::from_str(&uid))),
            ("name", any(NSString::from_str("tvctl duck"))),
            ("private", any(NSNumber::new_bool(true))),
            ("taps", any(NSArray::from_retained_slice(&[tap_entry]))),
            ("tapautostart", any(NSNumber::new_bool(true))),
        ]);
        let mut device: AudioObjectID = 0;
        // SAFETY: NSDictionary is toll-free bridged to CFDictionary.
        let status = unsafe {
            let cf = &*(Retained::as_ptr(&description) as *const CFDictionary);
            AudioHardwareCreateAggregateDevice(cf, NonNull::from(&mut device))
        };
        if status != 0 {
            unsafe { destroy_tap(tap) };
            bail!("could not create the tap device (OSStatus {status})");
        }
        let peak = Arc::into_raw(Arc::new(AtomicU32::new(0)));
        let mut proc_id: AudioDeviceIOProcID = None;
        // SAFETY: `peak` stays valid until after the IOProc is destroyed in Drop.
        let mut status = unsafe {
            AudioDeviceCreateIOProcID(
                device,
                Some(measure_peak),
                peak as *mut c_void,
                NonNull::from(&mut proc_id),
            )
        };
        if status == 0 {
            status = unsafe { AudioDeviceStart(device, proc_id) };
        }
        let tap = Tap {
            tap,
            device,
            io_proc: proc_id,
            peak,
            destroy_tap,
        };
        if status != 0 {
            bail!("could not start the audio tap (OSStatus {status})");
        }
        Ok(tap)
    }

    /// The highest sample level (0..1) since the last call.
    pub fn take_peak(&self) -> f32 {
        // SAFETY: `peak` is alive while `self` is.
        f32::from_bits(unsafe { &*self.peak }.swap(0, Ordering::Relaxed))
    }
}

impl Drop for Tap {
    fn drop(&mut self) {
        // SAFETY: ids created in `start`; after the IOProc is destroyed Core Audio
        // no longer uses `peak`, so it can be freed.
        unsafe {
            if self.io_proc.is_some() {
                AudioDeviceStop(self.device, self.io_proc);
                AudioDeviceDestroyIOProcID(self.device, self.io_proc);
            }
            AudioHardwareDestroyAggregateDevice(self.device);
            (self.destroy_tap)(self.tap);
            drop(Arc::from_raw(self.peak));
        }
    }
}

/// Process name (executable name, up to 32 bytes).
pub fn process_name(pid: i32) -> Option<String> {
    let mut buf = [0u8; 256];
    // SAFETY: buffer of the given size.
    let n = unsafe { libc::proc_name(pid, buf.as_mut_ptr().cast(), buf.len() as u32) };
    (n > 0).then(|| String::from_utf8_lossy(&buf[..n as usize]).into_owned())
}

/// Executable path.
pub fn process_path(pid: i32) -> Option<PathBuf> {
    let mut buf = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    // SAFETY: buffer of the given size.
    let n = unsafe { libc::proc_pidpath(pid, buf.as_mut_ptr().cast(), buf.len() as u32) };
    (n > 0).then(|| PathBuf::from(String::from_utf8_lossy(&buf[..n as usize]).into_owned()))
}

/// Parent process id.
pub fn parent_pid(pid: i32) -> Option<i32> {
    // SAFETY: zeroed POD struct of the size passed.
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let n = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            (&mut info as *mut libc::proc_bsdinfo).cast(),
            size_of::<libc::proc_bsdinfo>() as i32,
        )
    };
    (n as usize == size_of::<libc::proc_bsdinfo>()).then_some(info.pbi_ppid as i32)
}

/// The process macOS holds responsible for `pid` (the app that launched it, for
/// helpers and command line tools). Private API, looked up at run time.
pub fn responsible_pid(pid: i32) -> Option<i32> {
    type Fn = unsafe extern "C" fn(i32) -> i32;
    static FN: OnceLock<Option<Fn>> = OnceLock::new();
    let f = (*FN.get_or_init(|| {
        let ptr = lookup(c"responsibility_get_pid_responsible_for_pid")?;
        // SAFETY: `pid_t responsibility_get_pid_responsible_for_pid(pid_t)`.
        Some(unsafe { std::mem::transmute::<*mut c_void, Fn>(ptr) })
    }))?;
    // SAFETY: plain C call.
    let r = unsafe { f(pid) };
    (r > 0).then_some(r)
}

/// `CFBundleIdentifier` of the bundle at `path` (cached).
pub fn bundle_id(path: &Path) -> Option<String> {
    static CACHE: OnceLock<Mutex<std::collections::HashMap<PathBuf, Option<String>>>> =
        OnceLock::new();
    let cache = CACHE.get_or_init(Default::default);
    if let Some(v) = cache.lock().unwrap().get(path) {
        return v.clone();
    }
    let id = NSBundle::bundleWithPath(&NSString::from_str(&path.to_string_lossy()))
        .and_then(|b| b.bundleIdentifier())
        .map(|s| s.to_string());
    cache.lock().unwrap().insert(path.to_path_buf(), id.clone());
    id
}
