//! Ducking: while other audio plays, every player is lowered to a share of its
//! own volume, and restored when that audio stops.
//!
//! Other audio is measured with a Core Audio process tap (a copy of the audio of
//! chosen processes; nothing is rerouted). "Other" is every process except
//! tvctl's own players, or only the apps the user named. The volume change is
//! applied inside the player pages (`page.js`), so it works the same for every
//! plugin and leaves the page's own volume setting alone.

#[cfg(target_os = "macos")]
mod coreaudio;
#[cfg(not(target_os = "macos"))]
#[path = "unsupported.rs"]
mod coreaudio;

use std::path::{Path, PathBuf};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::auth;

/// Hook injected into every frame of every player page.
pub const PAGE_SCRIPT: &str = include_str!("page.js");

/// Monitoring tick.
const TICK: Duration = Duration::from_millis(50);
/// Audio louder than this (peak, dBFS) counts as playing.
const THRESHOLD_DB: f32 = -50.0;
/// Other audio must play this long before the players are lowered...
const ATTACK: Duration = Duration::from_millis(200);
/// ...where pauses shorter than this do not count as stopping...
const GAP: Duration = Duration::from_millis(300);
/// ...and must stay silent this long before they are restored.
const RELEASE: Duration = Duration::from_millis(1500);
const FADE_DOWN_MS: u64 = 200;
const FADE_UP_MS: u64 = 1000;
/// While lowered, the gain is re-sent this often (reloaded pages, new players).
const REASSERT: Duration = Duration::from_secs(1);
/// The set of tapped processes is recomputed at least this often.
const RESCAN: Duration = Duration::from_secs(2);
/// Which of them play (`sources` in the status) is refreshed this often.
const REFRESH_SOURCES: Duration = Duration::from_millis(500);

/// Saved ducking settings (`duck.json` in the data dir).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Config {
    pub enabled: bool,
    /// Volume while other audio plays, in percent of each player's own volume.
    pub level: u8,
    /// Only these apps count as other audio; all other processes when None.
    pub apps: Option<Vec<String>>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            enabled: false,
            level: 10,
            apps: None,
        }
    }
}

fn config_path(dir: &Path) -> PathBuf {
    dir.join("duck.json")
}

fn load_from(dir: &Path) -> Config {
    std::fs::read_to_string(config_path(dir))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_in(dir: &Path, config: &Config) -> Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let path = config_path(dir);
    std::fs::write(&path, serde_json::to_string_pretty(config)?)
        .with_context(|| format!("writing {}", path.display()))
}

pub fn load() -> Config {
    load_from(&auth::data_dir())
}

pub fn save(config: &Config) -> Result<()> {
    save_in(&auth::data_dir(), config)
}

/// Status when no server is running (nothing is monitored then).
pub fn offline_status(config: &Config) -> Value {
    json!({
        "enabled": config.enabled,
        "level": config.level,
        "apps": config.apps,
        "server_running": false,
        "monitoring": false,
        "ducking": false,
        "gain": 1.0,
        "peak_db": null,
        "sources": [],
    })
}

/// The JavaScript that sets the players' gain (installing the hook if needed).
pub fn gain_script(gain: f64, fade_ms: u64) -> String {
    format!("{PAGE_SCRIPT}\n;window.__tvctlDuck.set({gain}, {fade_ms});")
}

/// What the ducking monitor needs from the server.
pub trait Host: Sync {
    fn has_player(&self) -> bool;
    /// Runs `script` in the main frame of the player page.
    fn run_in_player(&self, script: &str);
    /// Pid of tvctl's Chrome (browser process), if it runs.
    fn chrome_pid(&self) -> Option<i32>;
}

/// An audio process with what is needed to match and describe it.
#[derive(Debug, Clone, Default)]
struct Process {
    object: u32,
    pid: i32,
    name: Option<String>,
    bundle_id: Option<String>,
    /// The app it belongs to: the outermost `.app` of its executable, or of its
    /// responsible process's executable.
    app: Option<String>,
    app_bundle_id: Option<String>,
    running_output: bool,
    /// Parent, grandparent, ... (nearest first).
    ancestors: Vec<i32>,
}

impl Process {
    fn json(&self) -> Value {
        json!({
            "pid": self.pid,
            "name": self.name,
            "bundle_id": self.bundle_id,
            "app": self.app,
            "app_bundle_id": self.app_bundle_id,
        })
    }

    /// Whether a `--apps` entry names this process: its name, app name or bundle
    /// id (case-insensitive), or a bundle id prefix (`com.google.Chrome` matches
    /// `com.google.Chrome.helper`).
    fn matches(&self, entry: &str) -> bool {
        let entry = entry.trim().to_lowercase();
        if entry.is_empty() {
            return false;
        }
        let names = [&self.name, &self.app].map(|n| n.as_deref().map(str::to_lowercase));
        let ids =
            [&self.bundle_id, &self.app_bundle_id].map(|n| n.as_deref().map(str::to_lowercase));
        names.iter().flatten().any(|n| *n == entry)
            || ids
                .iter()
                .flatten()
                .any(|id| *id == entry || id.starts_with(&format!("{entry}.")))
    }
}

/// tvctl's own audio processes are never "other" audio.
struct Own {
    server_pid: i32,
    chrome_pid: Option<i32>,
}

impl Own {
    fn current(host: &dyn Host) -> Own {
        Own {
            server_pid: std::process::id() as i32,
            chrome_pid: host.chrome_pid(),
        }
    }

    fn contains(&self, p: &Process) -> bool {
        if p.pid == self.server_pid || p.ancestors.contains(&self.server_pid) {
            return true;
        }
        self.chrome_pid
            .is_some_and(|chrome| p.pid == chrome || p.ancestors.contains(&chrome))
    }
}

/// The outermost `.app` bundle containing `path`.
fn app_bundle(path: &Path) -> Option<PathBuf> {
    let mut app = None;
    let mut current = PathBuf::new();
    for c in path.components() {
        current.push(c);
        if current.extension().is_some_and(|e| e == "app") {
            app = Some(current.clone());
            break;
        }
    }
    app
}

fn scan() -> Vec<Process> {
    coreaudio::processes()
        .into_iter()
        .map(|a| {
            let mut ancestors = Vec::new();
            let mut pid = a.pid;
            while ancestors.len() < 16
                && let Some(parent) = coreaudio::parent_pid(pid).filter(|p| *p > 1)
            {
                ancestors.push(parent);
                pid = parent;
            }
            let responsible = coreaudio::responsible_pid(a.pid);
            let app_dir = coreaudio::process_path(a.pid)
                .and_then(|p| app_bundle(&p))
                .or_else(|| {
                    responsible
                        .and_then(coreaudio::process_path)
                        .and_then(|p| app_bundle(&p))
                });
            Process {
                object: a.object,
                pid: a.pid,
                name: coreaudio::process_name(a.pid),
                bundle_id: a.bundle_id,
                app: app_dir
                    .as_ref()
                    .and_then(|d| d.file_stem())
                    .map(|s| s.to_string_lossy().into_owned()),
                app_bundle_id: app_dir.as_deref().and_then(coreaudio::bundle_id),
                running_output: a.running_output,
                ancestors,
            }
        })
        .collect()
}

/// `duck apps`: processes playing audio now. With a host (in the server),
/// tvctl's own players are marked.
pub fn apps(host: Option<&dyn Host>) -> Value {
    let own = host.map(Own::current);
    let mut list: Vec<&Process> = Vec::new();
    let all = scan();
    list.extend(all.iter().filter(|p| p.running_output));
    list.sort_by_key(|p| (p.app.clone(), p.pid));
    let apps: Vec<Value> = list
        .into_iter()
        .map(|p| {
            let mut v = p.json();
            if let Some(own) = &own {
                v["tvctl"] = json!(own.contains(p));
            }
            v
        })
        .collect();
    json!({ "apps": apps })
}

/// Which processes a config listens to: (tap target, processes that count).
fn select(config: &Config, procs: &[Process], own: &Own) -> (coreaudio::Target, Vec<usize>) {
    let others = procs.iter().enumerate().filter(|(_, p)| !own.contains(p));
    match &config.apps {
        Some(apps) => {
            let chosen: Vec<usize> = others
                .filter(|(_, p)| apps.iter().any(|a| p.matches(a)))
                .map(|(i, _)| i)
                .collect();
            let mut ids: Vec<u32> = chosen.iter().map(|&i| procs[i].object).collect();
            ids.sort_unstable();
            (coreaudio::Target::Include(ids), chosen)
        }
        None => {
            let mut ids: Vec<u32> = procs
                .iter()
                .filter(|p| own.contains(p))
                .map(|p| p.object)
                .collect();
            ids.sort_unstable();
            (
                coreaudio::Target::Exclude(ids),
                others.map(|(i, _)| i).collect(),
            )
        }
    }
}

/// Decides when to lower and restore from a stream of loud/quiet ticks.
#[derive(Debug, Default)]
struct Detector {
    loud_since: Option<Instant>,
    last_loud: Option<Instant>,
    ducking: bool,
}

impl Detector {
    /// Returns the new state when it changes.
    fn update(&mut self, now: Instant, loud: bool) -> Option<bool> {
        if loud {
            let resumed = self.last_loud.is_none_or(|t| now - t > GAP);
            if resumed {
                self.loud_since = Some(now);
            }
            self.last_loud = Some(now);
        }
        let recent = self.last_loud.is_some_and(|t| now - t <= GAP);
        if !self.ducking {
            let span = self.loud_since.zip(self.last_loud).map(|(s, l)| l - s);
            if recent && span.is_some_and(|d| d >= ATTACK) {
                self.ducking = true;
                return Some(true);
            }
        } else if self.last_loud.is_none_or(|t| now - t >= RELEASE) {
            self.ducking = false;
            self.loud_since = None;
            return Some(false);
        }
        None
    }

    fn reset(&mut self) {
        *self = Detector::default();
    }
}

#[derive(Debug, Clone)]
struct Status {
    monitoring: bool,
    ducking: bool,
    gain: f64,
    peak_db: Option<f32>,
    sources: Vec<Value>,
    error: Option<String>,
}

impl Default for Status {
    fn default() -> Self {
        Status {
            monitoring: false,
            ducking: false,
            gain: 1.0,
            peak_db: None,
            sources: Vec::new(),
            error: None,
        }
    }
}

struct Inner {
    config: Config,
    changed: bool,
    status: Status,
}

/// Ducking state shared by the server's request handlers and the monitor thread.
pub struct Duck {
    inner: Mutex<Inner>,
    wake: Condvar,
}

impl Duck {
    pub fn new(config: Config) -> Duck {
        Duck {
            inner: Mutex::new(Inner {
                config,
                changed: true,
                status: Status::default(),
            }),
            wake: Condvar::new(),
        }
    }

    pub fn set_config(&self, config: Config) -> Value {
        let mut inner = self.inner.lock().unwrap();
        inner.config = config;
        inner.changed = true;
        self.wake.notify_all();
        Self::status_json(&inner)
    }

    pub fn status(&self) -> Value {
        Self::status_json(&self.inner.lock().unwrap())
    }

    fn status_json(inner: &Inner) -> Value {
        let s = &inner.status;
        let mut v = json!({
            "enabled": inner.config.enabled,
            "level": inner.config.level,
            "apps": inner.config.apps,
            "server_running": true,
            "monitoring": s.monitoring,
            "ducking": s.ducking,
            "gain": (s.gain * 100.0).round() / 100.0,
            "peak_db": s.peak_db.map(|d| (f64::from(d) * 10.0).round() / 10.0),
            "sources": s.sources,
        });
        if let Some(e) = &s.error {
            v["error"] = json!(e);
        }
        v
    }

    fn update(&self, f: impl FnOnce(&mut Status)) {
        f(&mut self.inner.lock().unwrap().status);
    }
}

/// The monitor loop (runs on its own thread for the server's lifetime). Taps
/// only while ducking is enabled and players are open.
pub fn run(duck: &Duck, host: &dyn Host) {
    let mut tap: Option<(coreaudio::Target, Option<coreaudio::Tap>)> = None;
    let mut detector = Detector::default();
    let mut applied = 1.0f64;
    let mut last_apply = Instant::now();
    let mut last_scan: Option<Instant> = None;
    let mut last_refresh = Instant::now();
    let mut chosen: Vec<Process> = Vec::new();
    let mut meter = f32::NEG_INFINITY;
    loop {
        let (config, changed) = {
            let mut inner = duck.inner.lock().unwrap();
            (inner.config.clone(), std::mem::take(&mut inner.changed))
        };
        if !config.enabled || !host.has_player() {
            tap = None;
            detector.reset();
            chosen.clear();
            last_scan = None;
            if applied != 1.0 {
                host.run_in_player(&gain_script(1.0, FADE_UP_MS));
                applied = 1.0;
            }
            duck.update(|s| *s = Status::default());
            let inner = duck.inner.lock().unwrap();
            if !inner.changed {
                let _ = duck.wake.wait_timeout(inner, Duration::from_millis(250));
            }
            continue;
        }

        let process_list_changed = coreaudio::take_process_list_changed();
        if changed || process_list_changed || last_scan.is_none_or(|t| t.elapsed() >= RESCAN) {
            last_scan = Some(Instant::now());
            let procs = scan();
            let (target, indices) = select(&config, &procs, &Own::current(host));
            chosen = indices.into_iter().map(|i| procs[i].clone()).collect();
            last_refresh = Instant::now();
            if tap.as_ref().is_none_or(|(t, _)| *t != target) {
                tap = None;
                let started = match &target {
                    coreaudio::Target::Include(ids) if ids.is_empty() => Ok(None),
                    t => {
                        duck.update(|s| s.monitoring = true);
                        coreaudio::Tap::start(t).map(Some)
                    }
                };
                match started {
                    Ok(t) => {
                        tap = Some((target, t));
                        duck.update(|s| s.error = None);
                    }
                    Err(e) => {
                        eprintln!("duck: {e:#}");
                        duck.update(|s| s.error = Some(format!("{e:#}")));
                    }
                }
            }
        }

        let peak = tap
            .as_ref()
            .and_then(|(_, t)| t.as_ref())
            .map_or(0.0, coreaudio::Tap::take_peak);
        let peak_db = if peak > 0.0 {
            20.0 * peak.log10()
        } else {
            f32::NEG_INFINITY
        };
        meter = peak_db.max(meter - 3.0);
        let now = Instant::now();
        detector.update(now, peak_db > THRESHOLD_DB);
        let target = if detector.ducking {
            f64::from(config.level.min(100)) / 100.0
        } else {
            1.0
        };
        if target != applied {
            let fade = if target < applied {
                FADE_DOWN_MS
            } else {
                FADE_UP_MS
            };
            host.run_in_player(&gain_script(target, fade));
            applied = target;
            last_apply = now;
        } else if target != 1.0 && last_apply.elapsed() >= REASSERT {
            host.run_in_player(&gain_script(target, 0));
            last_apply = now;
        }
        if last_refresh.elapsed() >= REFRESH_SOURCES {
            for p in &mut chosen {
                p.running_output = coreaudio::running_output(p.object);
            }
            last_refresh = Instant::now();
        }
        let sources: Vec<Value> = chosen
            .iter()
            .filter(|p| p.running_output)
            .map(Process::json)
            .collect();
        duck.update(|s| {
            s.monitoring = tap.as_ref().is_some_and(|(_, t)| t.is_some());
            s.ducking = detector.ducking;
            s.gain = applied;
            s.peak_db = (meter > -120.0).then_some(meter);
            s.sources = sources;
        });
        std::thread::sleep(TICK);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    /// Feeds ticks (50 ms apart) and returns the state after each.
    fn run_ticks(d: &mut Detector, start: Instant, loud: &[bool]) -> Vec<bool> {
        loud.iter()
            .enumerate()
            .map(|(i, &l)| {
                d.update(start + ms(50 * i as u64), l);
                d.ducking
            })
            .collect()
    }

    #[test]
    fn detector_needs_sustained_audio() {
        let t0 = Instant::now();
        let mut d = Detector::default();
        // One loud tick (a click) does not duck.
        let states = run_ticks(
            &mut d,
            t0,
            &[true, false, false, false, false, false, false, false],
        );
        assert!(states.iter().all(|s| !s));
        // 200 ms of audio does.
        let mut d = Detector::default();
        let states = run_ticks(&mut d, t0, &[true, true, true, true, true]);
        assert_eq!(states, [false, false, false, false, true]);
    }

    #[test]
    fn detector_tolerates_short_pauses_and_releases_after_silence() {
        let t0 = Instant::now();
        let mut d = Detector::default();
        // Loud, a 150 ms pause, loud again: attack keeps counting.
        let states = run_ticks(&mut d, t0, &[true, true, false, false, true]);
        assert_eq!(states.last(), Some(&true));
        // Stays lowered through a 1 s pause, restores after 1.5 s of silence.
        let quiet = |n: u64| t0 + ms(200 + n);
        assert_eq!(d.update(quiet(1000), false), None);
        assert!(d.ducking);
        assert_eq!(d.update(quiet(1500), false), Some(false));
        assert!(!d.ducking);
    }

    #[test]
    fn detector_restarts_attack_after_a_long_pause() {
        let t0 = Instant::now();
        let mut d = Detector::default();
        d.update(t0, true);
        d.update(t0 + ms(100), true);
        // 400 ms of silence resets the attack.
        assert_eq!(d.update(t0 + ms(500), true), None);
        assert_eq!(d.update(t0 + ms(650), true), None);
        assert_eq!(d.update(t0 + ms(700), true), Some(true));
    }

    fn chatgpt_helper() -> Process {
        Process {
            object: 7,
            pid: 100,
            name: Some("Codex (Service)".into()),
            bundle_id: Some("com.openai.codex.helper".into()),
            app: Some("ChatGPT".into()),
            app_bundle_id: Some("com.openai.chat".into()),
            ..Default::default()
        }
    }

    #[test]
    fn apps_match_names_bundles_and_prefixes() {
        let p = chatgpt_helper();
        for entry in [
            "ChatGPT",
            "chatgpt",
            "com.openai.chat",
            "com.openai.codex.helper",
            "com.openai",
            "codex (service)",
        ] {
            assert!(p.matches(entry), "{entry}");
        }
        for entry in ["", "Chat", "com.openai.ch", "Google Chrome"] {
            assert!(!p.matches(entry), "{entry}");
        }
    }

    #[test]
    fn app_bundle_is_the_outermost_app() {
        assert_eq!(
            app_bundle(Path::new(
                "/Applications/ChatGPT.app/Contents/Frameworks/X.framework/Helpers/Codex (Service).app/Contents/MacOS/Codex (Service)"
            )),
            Some(PathBuf::from("/Applications/ChatGPT.app"))
        );
        assert_eq!(app_bundle(Path::new("/usr/bin/afplay")), None);
    }

    fn own() -> Own {
        Own {
            server_pid: 10,
            chrome_pid: Some(20),
        }
    }

    #[test]
    fn own_processes_are_the_server_and_chrome_descendants() {
        let own = own();
        let chrome_audio = Process {
            pid: 21,
            ancestors: vec![20],
            bundle_id: Some("com.google.Chrome.helper".into()),
            ..Default::default()
        };
        assert!(own.contains(&chrome_audio));
        let users_chrome = Process {
            pid: 31,
            ancestors: vec![30],
            bundle_id: Some("com.google.Chrome.helper".into()),
            ..Default::default()
        };
        assert!(!own.contains(&users_chrome));
        // Safari's WebKit helpers are not ours.
        assert!(!own.contains(&Process {
            pid: 40,
            bundle_id: Some("com.apple.WebKit.GPU".into()),
            ..Default::default()
        }));
        assert!(own.contains(&Process {
            pid: 10,
            ..Default::default()
        }));
    }

    #[test]
    fn selection_never_includes_own_processes() {
        let chrome_audio = Process {
            object: 1,
            pid: 21,
            ancestors: vec![20],
            bundle_id: Some("com.google.Chrome.helper".into()),
            app: Some("Google Chrome".into()),
            ..Default::default()
        };
        let users_chrome = Process {
            object: 2,
            pid: 31,
            ancestors: vec![30],
            bundle_id: Some("com.google.Chrome.helper".into()),
            app: Some("Google Chrome".into()),
            ..Default::default()
        };
        let procs = vec![chrome_audio, users_chrome, chatgpt_helper()];
        let all = Config {
            enabled: true,
            ..Default::default()
        };
        let (target, chosen) = select(&all, &procs, &own());
        assert_eq!(target, coreaudio::Target::Exclude(vec![1]));
        assert_eq!(chosen, vec![1, 2]);

        let chrome_only = Config {
            apps: Some(vec!["Google Chrome".into()]),
            ..all.clone()
        };
        let (target, chosen) = select(&chrome_only, &procs, &own());
        assert_eq!(target, coreaudio::Target::Include(vec![2]));
        assert_eq!(chosen, vec![1]);

        let none = Config {
            apps: Some(vec!["Zoom".into()]),
            ..all
        };
        assert_eq!(
            select(&none, &procs, &own()).0,
            coreaudio::Target::Include(vec![])
        );
    }

    #[test]
    fn config_round_trip_and_defaults() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load_from(dir.path()), Config::default());
        let c = Config {
            enabled: true,
            level: 25,
            apps: Some(vec!["ChatGPT".into()]),
        };
        save_in(&dir.path().join("nested"), &c).unwrap();
        assert_eq!(load_from(&dir.path().join("nested")), c);
        std::fs::write(config_path(dir.path()), "{bad").unwrap();
        assert_eq!(load_from(dir.path()), Config::default());
    }

    #[test]
    fn gain_script_installs_and_sets() {
        let s = gain_script(0.1, 200);
        assert!(s.starts_with(PAGE_SCRIPT));
        assert!(s.ends_with("window.__tvctlDuck.set(0.1, 200);"));
        assert!(gain_script(1.0, 0).ends_with("set(1, 0);"));
    }
}
