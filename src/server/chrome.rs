//! Chrome backend: plays in one app window of the user's Google Chrome
//! (dedicated profile, so logins persist and DRM works), driven through the
//! DevTools Protocol. `open` shows a URL in that window, replacing what it
//! showed; the window is created when there is none.

use std::fmt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};

use crate::auth;
use crate::blackout;
use crate::cdp::{CLOSED_EVENT, Cdp};
use crate::duck;
use crate::layout::{self, Rect};
use crate::plugin::{LoginMode, Plugin};
use crate::protocol::WindowOpts;

const DEFAULT_SIZE: (f64, f64) = (960.0, 540.0);

/// How long tvctl's Chrome keeps running without a player and other work: the
/// next player opens fast, and an unused Chrome leaves the Dock afterwards.
const IDLE_QUIT: Duration = Duration::from_secs(60);

/// A player command found no player, or the player shows other content than
/// the caller expected (see `Backend::open`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Gone {
    NoPlayer,
    Replaced,
}

impl fmt::Display for Gone {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Gone::NoPlayer => write!(f, "no player is open (open one with `tvctl open <URL>`)"),
            Gone::Replaced => write!(f, "the player shows something else now"),
        }
    }
}

impl std::error::Error for Gone {}

/// What the window shows.
struct Content {
    plugin: Arc<Plugin>,
    /// `Page.addScriptToEvaluateOnNewDocument` id of the plugin's adapter.
    adapter_script: String,
    /// Identifies this content: every `open` gets a new generation.
    generation: u64,
    last_status: Value,
}

/// The player window.
struct ChromePlayer {
    target_id: String,
    session_id: String,
    content: Mutex<Content>,
}

impl ChromePlayer {
    fn plugin(&self) -> Arc<Plugin> {
        self.content.lock().unwrap().plugin.clone()
    }

    fn generation(&self) -> u64 {
        self.content.lock().unwrap().generation
    }
}

type Slot = Arc<Mutex<Option<Arc<ChromePlayer>>>>;

struct Instance {
    cdp: Cdp,
    chrome: PathBuf,
    profile: PathBuf,
    player: Slot,
}

impl Instance {
    fn player(&self) -> Option<Arc<ChromePlayer>> {
        self.player.lock().unwrap().clone()
    }
}

pub struct Backend {
    shared: Arc<Shared>,
    /// Serializes `open` (one window) and window creation (so new targets can
    /// be matched to requests).
    open_lock: Mutex<()>,
    next_generation: AtomicU64,
    /// Size and position of the window when it last reported them (not in
    /// fullscreen); a new window opens there.
    last_frame: Mutex<Option<Frame>>,
}

/// Video area size and window position.
type Frame = ((f64, f64), (f64, f64));

/// State that the event watcher and the idle timers use too.
#[derive(Default)]
struct Shared {
    instance: Mutex<Option<Arc<Instance>>>,
    idle: Mutex<Idle>,
}

/// Work in progress on Chrome. Lock order: `Shared::instance`, then `Shared::idle`.
#[derive(Default)]
struct Idle {
    /// Operations that use Chrome without (yet) owning the player: `open`, logins.
    busy: usize,
    /// Bumped by every operation and every scheduled quit; a scheduled quit only
    /// runs if nothing happened since.
    generation: u64,
}

impl Idle {
    fn begin(&mut self) {
        self.busy += 1;
        self.generation += 1;
    }

    fn end(&mut self) {
        self.busy -= 1;
    }

    /// Starts a new idle period and returns its generation.
    fn schedule(&mut self) -> u64 {
        self.generation += 1;
        self.generation
    }

    fn may_quit(&self, generation: u64) -> bool {
        self.busy == 0 && self.generation == generation
    }
}

impl Shared {
    /// Quits Chrome after `IDLE_QUIT` unless something uses it in the meantime.
    fn schedule_quit(self: &Arc<Self>) {
        let generation = self.idle.lock().unwrap().schedule();
        let shared = self.clone();
        std::thread::spawn(move || {
            std::thread::sleep(IDLE_QUIT);
            shared.quit_if_idle(generation);
        });
    }

    /// `schedule_quit` when Chrome has no player.
    fn schedule_quit_if_unused(self: &Arc<Self>) {
        let unused = self
            .instance
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|inst| inst.player().is_none());
        if unused {
            self.schedule_quit();
        }
    }

    fn quit_if_idle(&self, generation: u64) {
        let mut guard = self.instance.lock().unwrap();
        if !self.idle.lock().unwrap().may_quit(generation) {
            return;
        }
        let Some(inst) = guard.as_ref() else {
            return;
        };
        if inst.player().is_some() {
            return;
        }
        let inst = guard.take().unwrap();
        if !inst.cdp.is_closed() {
            let _ = inst
                .cdp
                .call("Browser.close", json!({}), None, Duration::from_secs(5));
            // Keep the instance lock until Chrome is gone: a Chrome started on the
            // profile meanwhile would hand its window to the exiting one.
            if let Err(e) = wait_profile_unlocked(&inst.profile) {
                eprintln!("chrome: quitting idle Chrome: {e:#}");
            }
        }
    }
}

/// Marks Chrome busy (not idle) while alive; afterwards an unused Chrome quits
/// after `IDLE_QUIT`.
struct Busy(Arc<Shared>);

impl Busy {
    fn new(shared: &Arc<Shared>) -> Busy {
        shared.idle.lock().unwrap().begin();
        Busy(shared.clone())
    }
}

impl Drop for Busy {
    fn drop(&mut self) {
        self.0.idle.lock().unwrap().end();
        self.0.schedule_quit_if_unused();
    }
}

fn chrome_path() -> Result<PathBuf> {
    if let Some(p) = std::env::var_os("TVCTL_CHROME") {
        return Ok(PathBuf::from(p));
    }
    let candidates = [
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        "/usr/bin/google-chrome",
        "/usr/bin/google-chrome-stable",
    ];
    candidates
        .iter()
        .map(PathBuf::from)
        .find(|p| p.exists())
        .ok_or_else(|| anyhow!("Google Chrome not found (set TVCTL_CHROME to its executable)"))
}

fn profile_dir() -> PathBuf {
    auth::data_dir().join("chrome-profile")
}

/// Pid of the Chrome running on tvctl's profile, from Chrome's `SingletonLock`
/// (a symlink to `<hostname>-<pid>`).
pub fn browser_pid() -> Option<i32> {
    let target = std::fs::read_link(profile_dir().join("SingletonLock")).ok()?;
    parse_singleton_lock(&target.to_string_lossy())
}

fn parse_singleton_lock(target: &str) -> Option<i32> {
    target.rsplit_once('-')?.1.parse().ok().filter(|p| *p > 0)
}

fn read_devtools_port(profile: &Path) -> Option<String> {
    let s = std::fs::read_to_string(profile.join("DevToolsActivePort")).ok()?;
    let mut lines = s.lines();
    let port = lines.next()?.trim();
    let path = lines.next()?.trim();
    Some(format!("ws://127.0.0.1:{port}{path}"))
}

impl Backend {
    pub fn new() -> Backend {
        Backend {
            shared: Arc::default(),
            open_lock: Mutex::new(()),
            next_generation: AtomicU64::new(0),
            last_frame: Mutex::new(None),
        }
    }

    /// Returns the running Chrome, launching (or reattaching to) it if needed.
    /// Callers hold a `Busy` so that it does not quit as idle meanwhile.
    fn instance(&self) -> Result<Arc<Instance>> {
        let mut guard = self.shared.instance.lock().unwrap();
        if let Some(inst) = guard.as_ref().filter(|i| !i.cdp.is_closed()) {
            return Ok(inst.clone());
        }
        let inst = Arc::new(self.launch()?);
        self.watch_events(&inst);
        *guard = Some(inst.clone());
        Ok(inst)
    }

    fn launch(&self) -> Result<Instance> {
        let chrome = chrome_path()?;
        let profile = profile_dir();
        std::fs::create_dir_all(&profile).context("creating Chrome profile directory")?;

        // Reuse a Chrome left running with this profile (e.g. after a server restart).
        let cdp = match read_devtools_port(&profile).and_then(|url| Cdp::connect(&url).ok()) {
            Some(cdp) => cdp,
            None => {
                let _ = std::fs::remove_file(profile.join("DevToolsActivePort"));
                spawn_chrome(
                    &chrome,
                    &profile,
                    &[
                        "--remote-debugging-port=0",
                        "--no-startup-window",
                        "--no-first-run",
                        "--no-default-browser-check",
                        "--autoplay-policy=no-user-gesture-required",
                    ],
                )
                .map(reap)?;
                let deadline = Instant::now() + Duration::from_secs(20);
                loop {
                    if let Some(url) = read_devtools_port(&profile)
                        && let Ok(cdp) = Cdp::connect(&url)
                    {
                        break cdp;
                    }
                    if Instant::now() > deadline {
                        bail!(
                            "Chrome did not open a DevTools port (is another Chrome using {}?)",
                            profile.display()
                        );
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
        };
        cdp.call(
            "Target.setDiscoverTargets",
            json!({ "discover": true }),
            None,
            Duration::from_secs(5),
        )?;
        Ok(Instance {
            cdp,
            chrome,
            profile,
            player: Arc::default(),
        })
    }

    /// Forgets the player when its window is closed or Chrome exits.
    fn watch_events(&self, inst: &Arc<Instance>) {
        let events = inst.cdp.subscribe();
        let slot = inst.player.clone();
        let shared = self.shared.clone();
        std::thread::spawn(move || {
            for ev in events {
                match ev["method"].as_str() {
                    Some("Target.targetDestroyed") => {
                        let target = ev["params"]["targetId"].as_str().unwrap_or_default();
                        let mut guard = slot.lock().unwrap();
                        let closed = guard.as_ref().is_some_and(|p| p.target_id == target);
                        if closed {
                            *guard = None;
                        }
                        drop(guard);
                        // The user closed the player window.
                        if closed {
                            shared.schedule_quit_if_unused();
                        }
                    }
                    Some(CLOSED_EVENT) => {
                        *slot.lock().unwrap() = None;
                        break;
                    }
                    _ => {}
                }
            }
        });
    }

    /// The player, if it shows content `expect` (any content when None).
    fn player(&self, expect: Option<u64>) -> Result<(Arc<Instance>, Arc<ChromePlayer>), Gone> {
        let inst = self
            .shared
            .instance
            .lock()
            .unwrap()
            .clone()
            .ok_or(Gone::NoPlayer)?;
        let p = inst.player().ok_or(Gone::NoPlayer)?;
        if expect.is_some_and(|g| g != p.generation()) {
            return Err(Gone::Replaced);
        }
        Ok((inst, p))
    }

    /// The content generation and plugin name of the player, if any.
    pub fn current(&self) -> Option<(u64, String)> {
        let (_, p) = self.player(None).ok()?;
        let c = p.content.lock().unwrap();
        Some((c.generation, c.plugin.name().to_string()))
    }

    pub fn has_player(&self) -> bool {
        self.player(None).is_ok()
    }

    /// Opens a new Chrome window for `url` and returns its page target id.
    /// Callers hold `open_lock`.
    fn new_window(&self, inst: &Instance, url: &str, app: bool) -> Result<String> {
        let events = inst.cdp.subscribe();
        let window_arg = if app {
            format!("--app={url}")
        } else {
            "--new-window".to_string()
        };
        let mut args = vec![window_arg.as_str()];
        if !app {
            args.push(url);
        }
        spawn_chrome(&inst.chrome, &inst.profile, &args).map(reap)?;
        wait_event(&events, Duration::from_secs(15), |ev| {
            (ev["method"] == "Target.targetCreated" && ev["params"]["targetInfo"]["type"] == "page")
                .then(|| {
                    ev["params"]["targetInfo"]["targetId"]
                        .as_str()
                        .map(String::from)
                })
                .flatten()
        })
        .context("Chrome did not open a window")
    }

    /// Shows `action.url` in the player window (created if there is none),
    /// replacing what it showed, and runs the adapter's `open`. Returns the new
    /// content's generation with the response; player commands given that
    /// generation fail with `Gone::Replaced` once something else is opened.
    /// With `expect`, it only replaces content `expect` (checked under the
    /// open lock, so a concurrent `open` cannot be overwritten).
    pub fn open(
        &self,
        plugin: &Plugin,
        action: Value,
        opts: &WindowOpts,
        expect: Option<u64>,
    ) -> Result<(u64, Value)> {
        let _busy = Busy::new(&self.shared);
        let _open = self.open_lock.lock().unwrap();
        if expect.is_some() {
            self.player(expect)?;
        }
        let generation = self.next_generation.fetch_add(1, Ordering::SeqCst) + 1;
        let result = self.open_inner(generation, plugin, action, opts);
        if result.is_err() {
            // A failed open leaves no player: close the window (which may keep a
            // stream or session busy).
            let _ = self.close_unlocked(Some(generation));
        }
        result.map(|v| (generation, v))
    }

    fn open_inner(
        &self,
        generation: u64,
        plugin: &Plugin,
        action: Value,
        opts: &WindowOpts,
    ) -> Result<Value> {
        let inst = self.instance()?;
        let url = action["url"].as_str().unwrap_or_default().to_string();
        let plugin = Arc::new(plugin.clone());
        let player = match inst.player() {
            Some(player) => {
                // Same window: swap the adapter and load the URL in place.
                let old = player.content.lock().unwrap().adapter_script.clone();
                let _ = inst.cdp.call(
                    "Page.removeScriptToEvaluateOnNewDocument",
                    json!({ "identifier": old }),
                    Some(&player.session_id),
                    Duration::from_secs(5),
                );
                let adapter_script = add_adapter(&inst, &player.session_id, &plugin)?;
                *player.content.lock().unwrap() = Content {
                    plugin,
                    adapter_script,
                    generation,
                    last_status: Value::Null,
                };
                navigate(&inst, &player, &url)?;
                apply_window_opts(&inst, &player, opts)?;
                player
            }
            None => {
                let player = self.create_window(&inst, generation, plugin, &url)?;
                let mut initial = opts.clone();
                let last = *self.last_frame.lock().unwrap();
                if !initial.changes_geometry()
                    && let Some((size, position)) = last
                {
                    // Where the last player window was.
                    initial.size = Some(size);
                    initial.position = Some(position);
                }
                // A layout without size fills its region; otherwise the window
                // gets the default size (centered unless placed).
                if initial.size.is_none() && initial.layout.is_none() {
                    initial.size = Some(DEFAULT_SIZE);
                }
                if initial.position.is_none() && initial.layout.is_none() {
                    initial.layout = Some(layout::Layout::Center);
                }
                apply_window_opts(&inst, &player, &initial)?;
                player
            }
        };
        // What was opened comes to the front.
        let _ = inst.cdp.call(
            "Page.bringToFront",
            json!({}),
            Some(&player.session_id),
            Duration::from_secs(5),
        );

        // Plugins may ask to be navigated elsewhere first (e.g. to a canonical URL).
        let mut result = Value::Null;
        for _ in 0..3 {
            wait_loaded(&inst, &player)?;
            result = run_adapter(&inst, &player, &action, Duration::from_secs(40))?;
            match result.get("navigate").and_then(Value::as_str) {
                Some(next) => navigate(&inst, &player, next)?,
                None => break,
            }
        }
        self.respond(&inst, &player, result)
    }

    /// Opens the player window at `url` and makes it the player.
    fn create_window(
        &self,
        inst: &Instance,
        generation: u64,
        plugin: Arc<Plugin>,
        url: &str,
    ) -> Result<Arc<ChromePlayer>> {
        let target_id = self.new_window(inst, url, true)?;
        let attached = inst.cdp.call(
            "Target.attachToTarget",
            json!({ "targetId": target_id, "flatten": true }),
            None,
            Duration::from_secs(10),
        )?;
        let session_id = attached["sessionId"]
            .as_str()
            .context("no session id")?
            .to_string();
        // Scripts to evaluate on new documents only run while the Page domain is
        // enabled.
        inst.cdp.call(
            "Page.enable",
            json!({}),
            Some(&session_id),
            Duration::from_secs(5),
        )?;
        let adapter_script = add_adapter(inst, &session_id, &plugin)?;
        // The ducking hook, in every frame (`tvctl duck`).
        inst.cdp.call(
            "Page.addScriptToEvaluateOnNewDocument",
            json!({ "source": duck::PAGE_SCRIPT }),
            Some(&session_id),
            Duration::from_secs(5),
        )?;
        // Black after a while paused or ended (main frame only; the script checks).
        inst.cdp.call(
            "Page.addScriptToEvaluateOnNewDocument",
            json!({ "source": blackout::page_script() }),
            Some(&session_id),
            Duration::from_secs(5),
        )?;
        let player = Arc::new(ChromePlayer {
            target_id,
            session_id,
            content: Mutex::new(Content {
                plugin,
                adapter_script,
                generation,
                last_status: Value::Null,
            }),
        });
        *inst.player.lock().unwrap() = Some(player.clone());
        Ok(player)
    }

    /// Holds the open lock for operations on content `expect`, so the check and
    /// the operation cannot interleave with an `open` that replaces it.
    fn guard_for(&self, expect: Option<u64>) -> Option<std::sync::MutexGuard<'_, ()>> {
        expect.map(|_| self.open_lock.lock().unwrap())
    }

    /// Runs a player action on content `expect` (any content when None).
    pub fn action(&self, expect: Option<u64>, action: Value) -> Result<Value> {
        let _guard = self.guard_for(expect);
        let (inst, player) = self.player(expect)?;
        let result = run_adapter(&inst, &player, &action, Duration::from_secs(20))?;
        self.respond(&inst, &player, result)
    }

    pub fn window(&self, opts: &WindowOpts) -> Result<Value> {
        let (inst, player) = self.player(None)?;
        apply_window_opts(&inst, &player, opts)?;
        let status = run_adapter(
            &inst,
            &player,
            &json!({ "action": "status" }),
            Duration::from_secs(10),
        )?;
        self.respond(&inst, &player, status)
    }

    /// Closes the player window if it shows content `expect` (any when None).
    pub fn close(&self, expect: Option<u64>) -> Result<Value> {
        let _guard = self.guard_for(expect);
        self.close_unlocked(expect)
    }

    /// `close` for callers that hold the open lock already.
    fn close_unlocked(&self, expect: Option<u64>) -> Result<Value> {
        let (inst, player) = self.player(expect)?;
        inst.cdp.call(
            "Target.closeTarget",
            json!({ "targetId": player.target_id }),
            None,
            Duration::from_secs(5),
        )?;
        let mut slot = inst.player.lock().unwrap();
        if slot.as_ref().is_some_and(|p| Arc::ptr_eq(p, &player)) {
            *slot = None;
        }
        drop(slot);
        self.shared.schedule_quit_if_unused();
        Ok(json!({ "closed": true }))
    }

    /// Runs a script in the player page, ignoring failures (e.g. a page that is
    /// navigating).
    pub fn run_in_player(&self, script: &str) {
        let Ok((inst, player)) = self.player(None) else {
            return;
        };
        if let Err(e) = evaluate(&inst, &player, script, Duration::from_secs(2)) {
            eprintln!("chrome player script failed: {e:#}");
        }
    }

    /// Evaluates a script in the player page and returns its (awaited) value.
    pub fn eval(&self, script: &str) -> Result<Value> {
        let (inst, player) = self.player(None)?;
        evaluate(&inst, &player, script, Duration::from_secs(30))
    }

    /// Captures the page (works even when the window is covered).
    pub fn screenshot(&self, path: &str) -> Result<Value> {
        use base64::Engine;
        let (inst, player) = self.player(None)?;
        let shot = inst.cdp.call(
            "Page.captureScreenshot",
            json!({ "format": "png" }),
            Some(&player.session_id),
            Duration::from_secs(10),
        )?;
        let png = base64::engine::general_purpose::STANDARD
            .decode(shot["data"].as_str().unwrap_or_default())
            .context("decoding screenshot")?;
        std::fs::write(path, &png).with_context(|| format!("writing {path}"))?;
        Ok(json!({ "path": path, "bytes": png.len() }))
    }

    /// Service and title of the player (from its last response), if any.
    pub fn summary(&self) -> Value {
        let Ok((_, p)) = self.player(None) else {
            return Value::Null;
        };
        let c = p.content.lock().unwrap();
        json!({ "service": c.plugin.name(), "title": c.last_status.get("title") })
    }

    /// Closes Chrome if this server started or attached to it.
    pub fn shutdown(&self) {
        if let Some(inst) = self.shared.instance.lock().unwrap().take() {
            let _ = inst
                .cdp
                .call("Browser.close", json!({}), None, Duration::from_secs(5));
        }
    }

    /// Opens the plugin's login page in a normal Chrome window and blocks until the
    /// user closes that window; then checks the login state.
    pub fn login(&self, plugin: &Plugin, site: Option<&str>) -> Result<Value> {
        let _busy = Busy::new(&self.shared);
        let auth_cfg = plugin
            .manifest
            .auth
            .as_ref()
            .ok_or_else(|| anyhow!("{} has no login", plugin.name()))?;
        let login_url = auth_cfg
            .site(site)
            .with_context(|| format!("{} login", plugin.name()))?
            .login_url;
        if auth_cfg.login_mode == LoginMode::Plain {
            self.plain_login(&login_url)?;
            return self.login_status(plugin);
        }
        let inst = self.instance()?;
        let events = inst.cdp.subscribe();
        let target = {
            let _open = self.open_lock.lock().unwrap();
            self.new_window(&inst, &login_url, false)?
        };
        let _ = wait_event(&events, Duration::from_secs(30 * 60), |ev| {
            let closed = ev["method"] == CLOSED_EVENT
                || (ev["method"] == "Target.targetDestroyed"
                    && ev["params"]["targetId"] == target.as_str());
            closed.then_some(())
        });
        self.login_status(plugin)
    }

    /// Runs Chrome on the profile without a DevTools connection until the user
    /// quits it. The controlled Chrome has to stop first (one Chrome per profile).
    fn plain_login(&self, url: &str) -> Result<()> {
        {
            let mut guard = self.shared.instance.lock().unwrap();
            if let Some(inst) = guard.as_ref() {
                if inst.player().is_some() {
                    bail!("close the player first (`tvctl close`; login needs to restart Chrome)");
                }
                let _ = inst
                    .cdp
                    .call("Browser.close", json!({}), None, Duration::from_secs(5));
            }
            *guard = None;
        }
        let chrome = chrome_path()?;
        let profile = profile_dir();
        std::fs::create_dir_all(&profile)?;
        wait_profile_unlocked(&profile)?;
        let _ = std::fs::remove_file(profile.join("DevToolsActivePort"));
        let mut child = spawn_chrome(
            &chrome,
            &profile,
            &[
                "--no-first-run",
                "--no-default-browser-check",
                "--new-window",
                url,
            ],
        )?;
        eprintln!("login: waiting for the user to quit Chrome");
        child.wait().context("waiting for Chrome")?;
        wait_profile_unlocked(&profile)
    }

    /// Checks the login cookies of a plugin in the Chrome profile.
    pub fn login_status(&self, plugin: &Plugin) -> Result<Value> {
        let _busy = Busy::new(&self.shared);
        let auth_cfg = plugin
            .manifest
            .auth
            .as_ref()
            .ok_or_else(|| anyhow!("{} has no login", plugin.name()))?;
        let inst = self.instance()?;
        let cookies = inst.cdp.call(
            "Storage.getCookies",
            json!({}),
            None,
            Duration::from_secs(10),
        )?;
        let list = cookies["cookies"].as_array().cloned().unwrap_or_default();
        let mut sites = serde_json::Map::new();
        for (name, site) in auth_cfg.all_sites() {
            sites.insert(name, Value::Bool(has_login_cookie(&list, &site)?));
        }
        // A service with several sites counts as logged in when any site is.
        let logged_in = sites.values().any(|v| v == &Value::Bool(true));
        let mut out = json!({ "service": plugin.name(), "logged_in": logged_in });
        if sites.len() > 1 {
            out["sites"] = Value::Object(sites);
        }
        Ok(out)
    }

    /// Searches a service: loads its search page in a hidden tab (no window, the
    /// player is untouched), runs the plugin's `search.js`, and closes the tab.
    pub fn search(
        &self,
        plugin: &Plugin,
        query: &str,
        kind: Option<&str>,
        limit: usize,
    ) -> Result<Value> {
        let _busy = Busy::new(&self.shared);
        let search = plugin
            .manifest
            .search
            .as_ref()
            .ok_or_else(|| anyhow!("{} has no search", plugin.name()))?;
        let script = plugin
            .file(crate::plugin::SEARCH_SCRIPT)
            .context("plugin has no search.js")?;
        let url = search.page_url(query, kind)?;
        let inst = self.instance()?;
        // A hidden target has no UI at all; Chrome allows it only in some debugging
        // setups, so fall back to a minimized background window.
        let created = inst
            .cdp
            .call(
                "Target.createTarget",
                json!({ "url": "about:blank", "hidden": true }),
                None,
                Duration::from_secs(10),
            )
            .or_else(|_| {
                inst.cdp.call(
                    "Target.createTarget",
                    json!({
                        "url": "about:blank",
                        "newWindow": true,
                        "background": true,
                        "windowState": "minimized",
                    }),
                    None,
                    Duration::from_secs(10),
                )
            })?;
        let target_id = created["targetId"]
            .as_str()
            .context("no target id")?
            .to_string();
        let result = (|| {
            let attached = inst.cdp.call(
                "Target.attachToTarget",
                json!({ "targetId": target_id, "flatten": true }),
                None,
                Duration::from_secs(10),
            )?;
            let tab = ChromePlayer {
                target_id: target_id.clone(),
                session_id: attached["sessionId"]
                    .as_str()
                    .context("no session id")?
                    .to_string(),
                content: Mutex::new(Content {
                    plugin: Arc::new(plugin.clone()),
                    adapter_script: String::new(),
                    generation: 0,
                    last_status: Value::Null,
                }),
            };
            // A hidden tab is never visible: keep timers and rendering running.
            let _ = inst.cdp.call(
                "Emulation.setFocusEmulationEnabled",
                json!({ "enabled": true }),
                Some(&tab.session_id),
                Duration::from_secs(5),
            );
            navigate(&inst, &tab, &url)?;
            wait_loaded(&inst, &tab)?;
            let expr = format!(
                "(async (limit, kind) => {{\n{script}\n}})({limit}, {})",
                json!(kind)
            );
            evaluate(&inst, &tab, &expr, Duration::from_secs(30))
        })();
        let _ = inst.cdp.call(
            "Target.closeTarget",
            json!({ "targetId": target_id }),
            None,
            Duration::from_secs(5),
        );
        self.shared.schedule_quit_if_unused();
        let results = result?;
        Ok(json!({ "service": plugin.name(), "query": query, "results": results }))
    }

    fn respond(&self, inst: &Instance, player: &ChromePlayer, result: Value) -> Result<Value> {
        let service = {
            let mut c = player.content.lock().unwrap();
            if result.is_object() {
                c.last_status = result.clone();
            }
            c.plugin.name().to_string()
        };
        let window = window_info(inst, player)?;
        if window["fullscreen"] == false
            && let (Some(w), Some(h), Some(x), Some(y)) = (
                window["size"]["width"].as_f64(),
                window["size"]["height"].as_f64(),
                window["position"]["x"].as_f64(),
                window["position"]["y"].as_f64(),
            )
        {
            *self.last_frame.lock().unwrap() = Some(((w, h), (x, y)));
        }
        Ok(json!({
            "service": service,
            "player": result,
            "window": window,
        }))
    }
}

/// Loads the plugin's adapter into every new document of the window, so it
/// survives reloads and navigations (plugins can restore their page state on
/// load). Returns the script's identifier.
fn add_adapter(inst: &Instance, session_id: &str, plugin: &Plugin) -> Result<String> {
    let adapter = plugin
        .file("chrome.js")
        .context("plugin has no chrome.js")?;
    let r = inst.cdp.call(
        "Page.addScriptToEvaluateOnNewDocument",
        json!({ "source": format!("if (!window.__tvctl) {{\n{adapter}\n}}") }),
        Some(session_id),
        Duration::from_secs(5),
    )?;
    Ok(r["identifier"].as_str().unwrap_or_default().to_string())
}

/// Marks a document as replaced; `wait_loaded` waits for one without the mark.
const STALE_MARK: &str = "window.__tvctl_stale = true";

/// Loads `url` in the player window as a new document.
fn navigate(inst: &Instance, player: &ChromePlayer, url: &str) -> Result<()> {
    let _ = evaluate(inst, player, STALE_MARK, Duration::from_secs(5));
    let r = inst.cdp.call(
        "Page.navigate",
        json!({ "url": url }),
        Some(&player.session_id),
        Duration::from_secs(10),
    )?;
    if let Some(e) = r["errorText"].as_str().filter(|e| !e.is_empty()) {
        bail!("loading {url}: {e}");
    }
    // Only a cross-document navigation has a loader; reload a same-document one
    // (e.g. the same URL with another #fragment) so the page starts fresh.
    if r["loaderId"].as_str().is_none_or(str::is_empty) {
        inst.cdp.call(
            "Page.reload",
            json!({}),
            Some(&player.session_id),
            Duration::from_secs(10),
        )?;
    }
    Ok(())
}

/// Whether `cookies` (from `Storage.getCookies`) hold a login cookie of `site`.
fn has_login_cookie(cookies: &[Value], site: &crate::plugin::AuthSite) -> Result<bool> {
    let host = url::Url::parse(&site.cookie_url)?
        .host_str()
        .unwrap_or_default()
        .to_string();
    Ok(cookies.iter().any(|c| {
        let domain = c["domain"]
            .as_str()
            .unwrap_or_default()
            .trim_start_matches('.');
        let name = c["name"].as_str().unwrap_or_default();
        (host == domain || host.ends_with(&format!(".{domain}")))
            && site.logged_in_cookies.iter().any(|n| n == name)
    }))
}

fn spawn_chrome(chrome: &Path, profile: &Path, args: &[&str]) -> Result<std::process::Child> {
    use std::os::unix::process::CommandExt;
    Command::new(chrome)
        .arg(format!("--user-data-dir={}", profile.display()))
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .context("starting Chrome")
}

/// Collects a Chrome process's exit status in the background (no zombies).
fn reap(mut child: std::process::Child) {
    std::thread::spawn(move || {
        let _ = child.wait();
    });
}

/// Waits until no Chrome holds the profile (Chrome removes `SingletonLock` on exit).
fn wait_profile_unlocked(profile: &Path) -> Result<()> {
    let lock = profile.join("SingletonLock");
    let deadline = Instant::now() + Duration::from_secs(15);
    while lock.symlink_metadata().is_ok() {
        if Instant::now() > deadline {
            bail!("another Chrome is still using {}", profile.display());
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    Ok(())
}

fn wait_event<T>(
    events: &Receiver<Value>,
    timeout: Duration,
    mut pick: impl FnMut(&Value) -> Option<T>,
) -> Result<T> {
    let deadline = Instant::now() + timeout;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        let ev = events
            .recv_timeout(left)
            .map_err(|_| anyhow!("timed out"))?;
        if let Some(v) = pick(&ev) {
            return Ok(v);
        }
        if ev["method"] == CLOSED_EVENT {
            bail!("Chrome exited");
        }
    }
}

fn evaluate(
    inst: &Instance,
    player: &ChromePlayer,
    expr: &str,
    timeout: Duration,
) -> Result<Value> {
    let r = inst.cdp.call(
        "Runtime.evaluate",
        json!({ "expression": expr, "awaitPromise": true, "returnByValue": true, "userGesture": true }),
        Some(&player.session_id),
        timeout,
    )?;
    if let Some(ex) = r.get("exceptionDetails") {
        let msg = ex["exception"]["description"]
            .as_str()
            .or_else(|| ex["text"].as_str())
            .unwrap_or("script error");
        // Keep only the first line ("Error: message"), dropping the stack.
        let msg = msg.lines().next().unwrap_or(msg);
        bail!("{}", msg.strip_prefix("Error: ").unwrap_or(msg));
    }
    Ok(r["result"]["value"].clone())
}

/// Waits until the page has navigated away from about:blank (or a document
/// replaced by `navigate`) and parsed its DOM.
fn wait_loaded(inst: &Instance, player: &ChromePlayer) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let state = evaluate(
            inst,
            player,
            "!window.__tvctl_stale && location.protocol.startsWith('http') && document.readyState !== 'loading'",
            Duration::from_secs(5),
        );
        if matches!(state, Ok(Value::Bool(true))) {
            return Ok(());
        }
        if Instant::now() > deadline {
            bail!("page did not load");
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

fn run_adapter(
    inst: &Instance,
    player: &ChromePlayer,
    action: &Value,
    timeout: Duration,
) -> Result<Value> {
    let plugin = player.plugin();
    let adapter = plugin
        .file("chrome.js")
        .context("plugin has no chrome.js")?;
    // The blackout script is installed here too: `Page.addScriptToEvaluateOnNewDocument`
    // misses the document that was loading when the window opened. A player
    // command is activity, which removes the blackout.
    let blackout = blackout::page_script();
    let wake = if action["action"] == "status" {
        ""
    } else {
        blackout::WAKE_SCRIPT
    };
    let expr = format!(
        "(async () => {{\nif (!window.__tvctl) {{\n{adapter}\n}}\n{blackout}\n{wake}\nreturn await window.__tvctl.run({action});\n}})()"
    );
    evaluate(inst, player, &expr, timeout)
}

struct Geometry {
    window_id: i64,
    bounds: Value,
    work_area: Rect,
    inner: (f64, f64),
    deco: (f64, f64),
    /// The page is black (see `blackout`).
    blackout: bool,
}

fn geometry(inst: &Instance, player: &ChromePlayer) -> Result<Geometry> {
    let w = inst.cdp.call(
        "Browser.getWindowForTarget",
        json!({ "targetId": player.target_id }),
        None,
        Duration::from_secs(5),
    )?;
    let m = evaluate(
        inst,
        player,
        &format!(
            "({{ax: screen.availLeft, ay: screen.availTop, aw: screen.availWidth, ah: screen.availHeight, iw: innerWidth, ih: innerHeight, ow: outerWidth, oh: outerHeight, blackout: {}}})",
            blackout::SHOWN_EXPR
        ),
        Duration::from_secs(5),
    )?;
    let f = |k: &str| m[k].as_f64().unwrap_or(0.0);
    Ok(Geometry {
        window_id: w["windowId"].as_i64().context("no window id")?,
        bounds: w["bounds"].clone(),
        work_area: Rect {
            x: f("ax"),
            y: f("ay"),
            width: f("aw"),
            height: f("ah"),
        },
        inner: (f("iw"), f("ih")),
        deco: ((f("ow") - f("iw")).max(0.0), (f("oh") - f("ih")).max(0.0)),
        blackout: m["blackout"].as_bool().unwrap_or(false),
    })
}

fn set_bounds(inst: &Instance, window_id: i64, bounds: Value) -> Result<()> {
    inst.cdp.call(
        "Browser.setWindowBounds",
        json!({ "windowId": window_id, "bounds": bounds }),
        None,
        Duration::from_secs(5),
    )?;
    Ok(())
}

fn apply_window_opts(inst: &Instance, player: &ChromePlayer, opts: &WindowOpts) -> Result<()> {
    let mut g = geometry(inst, player)?;
    let state = g.bounds["windowState"]
        .as_str()
        .unwrap_or("normal")
        .to_string();
    if opts.changes_geometry() && state != "normal" {
        set_bounds(inst, g.window_id, json!({ "windowState": "normal" }))?;
        std::thread::sleep(Duration::from_millis(900));
        g = geometry(inst, player)?;
    }
    let outer = (
        g.bounds["width"].as_f64().unwrap_or(0.0),
        g.bounds["height"].as_f64().unwrap_or(0.0),
    );
    if let Some(l) = opts.layout {
        let r = layout::compute(l, g.work_area, opts.size, outer, g.deco);
        set_bounds(
            inst,
            g.window_id,
            json!({ "left": r.x.round(), "top": r.y.round(), "width": r.width.round(), "height": r.height.round() }),
        )?;
    } else {
        let mut b = json!({});
        if let Some((w, h)) = opts.size {
            b["width"] = json!((w + g.deco.0).round());
            b["height"] = json!((h + g.deco.1).round());
        }
        if let Some((x, y)) = opts.position {
            b["left"] = json!(x.round());
            b["top"] = json!(y.round());
        }
        if b.as_object().is_some_and(|o| !o.is_empty()) {
            set_bounds(inst, g.window_id, b)?;
        }
    }
    if let Some(sw) = opts.fullscreen {
        let on = sw.resolve(state == "fullscreen");
        set_bounds(
            inst,
            g.window_id,
            json!({ "windowState": if on { "fullscreen" } else { "normal" } }),
        )?;
    }
    Ok(())
}

fn window_info(inst: &Instance, player: &ChromePlayer) -> Result<Value> {
    let g = geometry(inst, player)?;
    let b = &g.bounds;
    Ok(json!({
        "position": { "x": b["left"], "y": b["top"] },
        "size": { "width": g.inner.0, "height": g.inner.1 },
        "outer_size": { "width": b["width"], "height": b["height"] },
        "fullscreen": b["windowState"] == "fullscreen",
        "blackout": g.blackout,
        "work_area": g.work_area,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn devtools_port_file() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(read_devtools_port(dir.path()), None);
        std::fs::write(
            dir.path().join("DevToolsActivePort"),
            "9222\n/devtools/browser/abc\n",
        )
        .unwrap();
        assert_eq!(
            read_devtools_port(dir.path()).as_deref(),
            Some("ws://127.0.0.1:9222/devtools/browser/abc")
        );
        std::fs::write(dir.path().join("DevToolsActivePort"), "9222\n").unwrap();
        assert_eq!(read_devtools_port(dir.path()), None);
    }

    #[test]
    fn idle_quit_needs_quiet_since_scheduling() {
        let mut idle = Idle::default();
        let g = idle.schedule();
        assert!(idle.may_quit(g));

        // Work started after scheduling cancels the quit, even once it ends.
        idle.begin();
        assert!(!idle.may_quit(g));
        idle.end();
        assert!(!idle.may_quit(g));

        // A newer schedule supersedes older ones; nothing quits while busy.
        idle.begin();
        let g = idle.schedule();
        assert!(!idle.may_quit(g));
        idle.end();
        assert!(idle.may_quit(g));
    }

    #[test]
    fn singleton_lock_pid() {
        assert_eq!(parse_singleton_lock("my-mac.local-11661"), Some(11661));
        assert_eq!(parse_singleton_lock("host-0"), None);
        assert_eq!(parse_singleton_lock("nonsense"), None);
    }

    #[test]
    fn login_cookies_per_site() {
        let site = |url: &str, name: &str| crate::plugin::AuthSite {
            login_url: format!("{url}/login"),
            cookie_url: url.into(),
            logged_in_cookies: vec![name.into()],
        };
        let us = site("https://www.amazon.com", "at-main");
        let jp = site("https://www.amazon.co.jp", "at-acbjp");
        let cookies = vec![
            json!({ "domain": ".amazon.co.jp", "name": "at-acbjp" }),
            // The right name on another site does not count.
            json!({ "domain": ".example.com", "name": "at-main" }),
        ];
        assert!(!has_login_cookie(&cookies, &us).unwrap());
        assert!(has_login_cookie(&cookies, &jp).unwrap());
    }
}
