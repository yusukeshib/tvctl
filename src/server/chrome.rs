//! Chrome backend: plays in app windows of the user's Google Chrome (dedicated
//! profile, so logins persist and DRM works), driven through the DevTools
//! Protocol. Runs entirely off the GUI thread.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};

use super::Players;
use crate::auth;
use crate::blackout;
use crate::cdp::{CLOSED_EVENT, Cdp};
use crate::duck;
use crate::layout::{self, Rect};
use crate::plugin::{LoginMode, Plugin};
use crate::protocol::{Switch, WindowOpts};

const DEFAULT_SIZE: (f64, f64) = (960.0, 540.0);

struct ChromePlayer {
    plugin: Plugin,
    target_id: String,
    session_id: String,
    last_status: Mutex<Value>,
}

struct Instance {
    cdp: Cdp,
    chrome: PathBuf,
    profile: PathBuf,
    players: Arc<Mutex<HashMap<String, Arc<ChromePlayer>>>>,
}

pub struct Backend {
    players: Arc<Players>,
    instance: Mutex<Option<Arc<Instance>>>,
    /// Serializes window creation so new targets can be matched to requests.
    open_lock: Mutex<()>,
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
    pub fn new(players: Arc<Players>) -> Backend {
        Backend {
            players,
            instance: Mutex::new(None),
            open_lock: Mutex::new(()),
        }
    }

    /// Returns the running Chrome, launching (or reattaching to) it if needed.
    fn instance(&self) -> Result<Arc<Instance>> {
        let mut guard = self.instance.lock().unwrap();
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
            players: Arc::default(),
        })
    }

    /// Removes players whose windows were closed, or all of them if Chrome exits.
    fn watch_events(&self, inst: &Arc<Instance>) {
        let events = inst.cdp.subscribe();
        let players = inst.players.clone();
        let registry = self.players.clone();
        std::thread::spawn(move || {
            for ev in events {
                match ev["method"].as_str() {
                    Some("Target.targetDestroyed") => {
                        let target = ev["params"]["targetId"].as_str().unwrap_or_default();
                        let mut map = players.lock().unwrap();
                        let gone: Vec<String> = map
                            .iter()
                            .filter(|(_, p)| p.target_id == target)
                            .map(|(id, _)| id.clone())
                            .collect();
                        for id in gone {
                            map.remove(&id);
                            registry.remove(&id);
                        }
                    }
                    Some(CLOSED_EVENT) => {
                        for (id, _) in players.lock().unwrap().drain() {
                            registry.remove(&id);
                        }
                        break;
                    }
                    _ => {}
                }
            }
        });
    }

    fn player(&self, id: &str) -> Result<(Arc<Instance>, Arc<ChromePlayer>)> {
        let inst = self
            .instance
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| anyhow!("no player with id {id:?}"))?;
        let p = inst
            .players
            .lock()
            .unwrap()
            .get(id)
            .cloned()
            .ok_or_else(|| anyhow!("no player with id {id:?}"))?;
        Ok((inst, p))
    }

    /// Opens a new Chrome window for `url` and returns its page target id.
    fn new_window(&self, inst: &Instance, url: &str, app: bool) -> Result<String> {
        let _guard = self.open_lock.lock().unwrap();
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

    pub fn open(
        &self,
        id: &str,
        plugin: &Plugin,
        action: Value,
        opts: &WindowOpts,
    ) -> Result<Value> {
        let result = self.open_inner(id, plugin, action, opts);
        if result.is_err() {
            // The caller gets no id for a failed open, so it could not close the
            // window (which may keep a stream or session busy); close it here.
            if self.player(id).is_ok() {
                let _ = self.close(id);
            }
            self.players.remove(id);
        }
        result
    }

    fn open_inner(
        &self,
        id: &str,
        plugin: &Plugin,
        action: Value,
        opts: &WindowOpts,
    ) -> Result<Value> {
        let inst = self.instance()?;
        let url = action["url"].as_str().unwrap_or_default().to_string();
        let target_id = self.new_window(&inst, &url, true)?;
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
        // Load the adapter into every document of this window, so it survives
        // reloads and navigations (plugins can restore their page state on load).
        if let Some(adapter) = plugin.file("chrome.js") {
            inst.cdp.call(
                "Page.addScriptToEvaluateOnNewDocument",
                json!({ "source": format!("if (!window.__tvctl) {{\n{adapter}\n}}") }),
                Some(&session_id),
                Duration::from_secs(5),
            )?;
        }
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
            plugin: plugin.clone(),
            target_id,
            session_id,
            last_status: Mutex::new(Value::Null),
        });
        inst.players
            .lock()
            .unwrap()
            .insert(id.to_string(), player.clone());

        let mut initial = opts.clone();
        // Same defaults as the WebView backend: a layout without size fills its
        // region; otherwise the window gets the default size (centered unless placed).
        if initial.size.is_none() && initial.layout.is_none() {
            initial.size = Some(DEFAULT_SIZE);
        }
        if initial.position.is_none() && initial.layout.is_none() {
            initial.layout = Some(layout::Layout::Center);
        }
        apply_window_opts(&inst, &player, &initial)?;
        // Like the WebView backend, a new player comes to the front.
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
                Some(next) => {
                    inst.cdp.call(
                        "Page.navigate",
                        json!({ "url": next }),
                        Some(&player.session_id),
                        Duration::from_secs(10),
                    )?;
                    std::thread::sleep(Duration::from_millis(500));
                }
                None => break,
            }
        }
        self.respond(id, &inst, &player, result)
    }

    pub fn action(&self, id: &str, action: Value) -> Result<Value> {
        let (inst, player) = self.player(id)?;
        let result = run_adapter(&inst, &player, &action, Duration::from_secs(20))?;
        self.respond(id, &inst, &player, result)
    }

    pub fn window(&self, id: &str, opts: &WindowOpts) -> Result<Value> {
        let (inst, player) = self.player(id)?;
        apply_window_opts(&inst, &player, opts)?;
        let status = run_adapter(
            &inst,
            &player,
            &json!({ "action": "status" }),
            Duration::from_secs(10),
        )?;
        self.respond(id, &inst, &player, status)
    }

    pub fn close(&self, id: &str) -> Result<Value> {
        let (inst, player) = self.player(id)?;
        inst.cdp.call(
            "Target.closeTarget",
            json!({ "targetId": player.target_id }),
            None,
            Duration::from_secs(5),
        )?;
        inst.players.lock().unwrap().remove(id);
        self.players.remove(id);
        Ok(json!({ "id": id, "closed": true }))
    }

    /// Runs a script in every player page, ignoring failures (e.g. a page that is
    /// navigating).
    pub fn run_in_all(&self, script: &str) {
        let Some(inst) = self.instance.lock().unwrap().clone() else {
            return;
        };
        let players: Vec<Arc<ChromePlayer>> =
            inst.players.lock().unwrap().values().cloned().collect();
        for player in players {
            if let Err(e) = evaluate(&inst, &player, script, Duration::from_secs(2)) {
                eprintln!("chrome player script failed: {e:#}");
            }
        }
    }

    /// Evaluates a script in the player page and returns its (awaited) value.
    pub fn eval(&self, id: &str, script: &str) -> Result<Value> {
        let (inst, player) = self.player(id)?;
        evaluate(&inst, &player, script, Duration::from_secs(30))
    }

    /// Captures the page (works even when the window is covered).
    pub fn screenshot(&self, id: &str, path: &str) -> Result<Value> {
        use base64::Engine;
        let (inst, player) = self.player(id)?;
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
        Ok(json!({ "id": id, "path": path, "bytes": png.len() }))
    }

    pub fn list(&self) -> Vec<Value> {
        let Some(inst) = self.instance.lock().unwrap().clone() else {
            return Vec::new();
        };
        let players: Vec<(String, Arc<ChromePlayer>)> = inst
            .players
            .lock()
            .unwrap()
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        players
            .into_iter()
            .map(|(id, p)| {
                json!({
                    "id": id,
                    "service": p.plugin.name(),
                    "backend": "chrome",
                    "title": p.last_status.lock().unwrap().get("title"),
                    "window": window_info(&inst, &p).unwrap_or(Value::Null),
                })
            })
            .collect()
    }

    /// Closes Chrome if this server started or attached to it.
    pub fn shutdown(&self) {
        if let Some(inst) = self.instance.lock().unwrap().take() {
            let _ = inst
                .cdp
                .call("Browser.close", json!({}), None, Duration::from_secs(5));
        }
    }

    /// Opens the plugin's login page in a normal Chrome window and blocks until the
    /// user closes that window; then checks and records the login state.
    pub fn login(&self, plugin: &Plugin, site: Option<&str>) -> Result<Value> {
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
        let target = self.new_window(&inst, &login_url, false)?;
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
            let mut guard = self.instance.lock().unwrap();
            if let Some(inst) = guard.as_ref() {
                if !inst.players.lock().unwrap().is_empty() {
                    bail!("close the Chrome players first (login needs to restart Chrome)");
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

    /// Checks the login cookies of a plugin in the Chrome profile and records them.
    pub fn login_status(&self, plugin: &Plugin) -> Result<Value> {
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
        auth::record(plugin.name(), logged_in);
        let mut out = json!({ "service": plugin.name(), "logged_in": logged_in });
        if sites.len() > 1 {
            out["sites"] = Value::Object(sites);
        }
        Ok(out)
    }

    fn respond(
        &self,
        id: &str,
        inst: &Instance,
        player: &ChromePlayer,
        result: Value,
    ) -> Result<Value> {
        if result.is_object() {
            *player.last_status.lock().unwrap() = result.clone();
        }
        Ok(json!({
            "id": id,
            "service": player.plugin.name(),
            "backend": "chrome",
            "player": result,
            "window": window_info(inst, player)?,
        }))
    }
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

/// Waits until the page has navigated away from about:blank and parsed its DOM.
fn wait_loaded(inst: &Instance, player: &ChromePlayer) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let state = evaluate(
            inst,
            player,
            "location.protocol.startsWith('http') && document.readyState !== 'loading'",
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
    let adapter = player
        .plugin
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
    if matches!(opts.on_top, Some(Switch::On | Switch::Toggle)) {
        bail!("always-on-top is not supported by the chrome backend");
    }
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
        "on_top": false,
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
