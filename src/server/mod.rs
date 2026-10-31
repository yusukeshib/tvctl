//! The player server. Accepts requests on a Unix socket and routes them to the
//! backend that owns each player. Plugins decide which backend a URL uses.

mod chrome;
mod webview;

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::auth;
use crate::client;
use crate::plugin::{BackendKind, Plugin, Registry};
use crate::protocol::{Request, Response};

pub type Reply = std::sync::mpsc::Sender<Result<Value, String>>;

/// Player ids shared by all backends.
pub struct Players {
    next: AtomicU64,
    owners: Mutex<HashMap<String, BackendKind>>,
    /// Last time a player was added or removed (for the idle shutdown).
    changed: Mutex<Instant>,
}

impl Default for Players {
    fn default() -> Self {
        Players {
            next: AtomicU64::new(0),
            owners: Mutex::default(),
            changed: Mutex::new(Instant::now()),
        }
    }
}

impl Players {
    pub fn allocate(&self, backend: BackendKind) -> String {
        let id = (self.next.fetch_add(1, Ordering::SeqCst) + 1).to_string();
        self.owners.lock().unwrap().insert(id.clone(), backend);
        *self.changed.lock().unwrap() = Instant::now();
        id
    }

    pub fn remove(&self, id: &str) {
        if self.owners.lock().unwrap().remove(id).is_some() {
            *self.changed.lock().unwrap() = Instant::now();
        }
    }

    fn owner(&self, id: &str) -> Result<BackendKind, String> {
        let owners = self.owners.lock().unwrap();
        owners.get(id).copied().ok_or_else(|| {
            let mut ids: Vec<&String> = owners.keys().collect();
            ids.sort_by_key(|s| s.parse::<u64>().unwrap_or(u64::MAX));
            format!("no player with id {id:?} (open players: {ids:?})")
        })
    }

    fn count(&self) -> usize {
        self.owners.lock().unwrap().len()
    }
}

struct Router {
    registry: Arc<Registry>,
    players: Arc<Players>,
    webview: webview::Handle,
    chrome: chrome::Backend,
    /// Started with `server up`: no idle shutdown.
    persistent: AtomicBool,
    /// Requests being handled right now.
    busy: AtomicUsize,
    last_request: Mutex<Instant>,
    /// The server stops after this long without players and requests.
    idle_timeout: Duration,
}

/// Idle grace period: `$MEDIACTL_IDLE_SECS` or 5 seconds.
fn idle_timeout() -> Duration {
    let secs = std::env::var("MEDIACTL_IDLE_SECS")
        .ok()
        .and_then(|s| s.parse::<f64>().ok())
        .unwrap_or(5.0);
    Duration::from_secs_f64(secs.max(0.0))
}

fn err(e: anyhow::Error) -> String {
    format!("{e:#}")
}

impl Router {
    fn handle(&self, req: Request) -> Result<Value, String> {
        match req {
            Request::Ping => Ok(self.info()),
            Request::SetPersistent { persistent } => {
                self.persistent.store(persistent, Ordering::SeqCst);
                Ok(self.info())
            }
            Request::Shutdown => {
                self.chrome.shutdown();
                self.webview
                    .call(webview::Command::Shutdown, Duration::from_secs(5))
            }
            Request::List => {
                let mut list = self
                    .webview
                    .call(webview::Command::List, Duration::from_secs(5))?;
                if let Some(items) = list.as_array_mut() {
                    items.extend(self.chrome.list());
                    items.sort_by_key(|v| v["id"].as_str().and_then(|s| s.parse::<u64>().ok()));
                }
                Ok(json!({ "players": list }))
            }
            Request::Open {
                url,
                start,
                paused,
                subtitles,
                window,
                backend,
            } => {
                let (plugin, url) = self.registry.for_url(&url).map_err(err)?;
                let backend = choose_backend(plugin, backend, auth::is_logged_in(plugin.name()))?;
                let action = json!({
                    "action": "open",
                    "url": url.as_str(),
                    "start": start,
                    "paused": paused,
                    "subtitles": subtitles,
                });
                let id = self.players.allocate(backend);
                match backend {
                    // On failure the window stays open (showing the error) and keeps its id.
                    BackendKind::Webview => self.webview.call(
                        webview::Command::Open {
                            id,
                            plugin: Box::new(plugin.clone()),
                            action,
                            window: Box::new(window),
                        },
                        Duration::from_secs(40),
                    ),
                    BackendKind::Chrome => {
                        self.chrome.open(&id, plugin, action, &window).map_err(err)
                    }
                }
            }
            Request::Player { id, action } => {
                let action = serde_json::to_value(action).expect("serializable");
                match self.players.owner(&id)? {
                    BackendKind::Webview => self.webview.call(
                        webview::Command::Player { id, action },
                        Duration::from_secs(15),
                    ),
                    BackendKind::Chrome => self.chrome.action(&id, action).map_err(err),
                }
            }
            Request::Window { id, opts } => match self.players.owner(&id)? {
                BackendKind::Webview => self.webview.call(
                    webview::Command::Window { id, opts },
                    Duration::from_secs(15),
                ),
                BackendKind::Chrome => self.chrome.window(&id, &opts).map_err(err),
            },
            Request::Close { id } => match self.players.owner(&id)? {
                BackendKind::Webview => self
                    .webview
                    .call(webview::Command::Close { id }, Duration::from_secs(5)),
                BackendKind::Chrome => self.chrome.close(&id).map_err(err),
            },
            Request::Screenshot { id, path } => match self.players.owner(&id)? {
                BackendKind::Webview => self.webview.call(
                    webview::Command::Screenshot { id, path },
                    Duration::from_secs(15),
                ),
                BackendKind::Chrome => self.chrome.screenshot(&id, &path).map_err(err),
            },
            Request::Eval { id, script } => match self.players.owner(&id)? {
                BackendKind::Webview => Err("__eval is only available for chrome players".into()),
                BackendKind::Chrome => self.chrome.eval(&id, &script).map_err(err),
            },
            Request::AuthLogin { service } => {
                self.chrome.login(self.auth_plugin(&service)?).map_err(err)
            }
            Request::AuthStatus { service } => {
                let plugins: Vec<&Plugin> = match service {
                    Some(s) => vec![self.auth_plugin(&s)?],
                    None => self
                        .registry
                        .plugins()
                        .iter()
                        .filter(|p| p.manifest.auth.is_some())
                        .collect(),
                };
                let statuses: Result<Vec<Value>, String> = plugins
                    .into_iter()
                    .map(|p| self.chrome.login_status(p).map_err(err))
                    .collect();
                Ok(json!({ "services": statuses? }))
            }
        }
    }

    fn info(&self) -> Value {
        json!({
            "pid": std::process::id(),
            "socket": client::socket_path(),
            "log": client::log_path(),
            "players": self.players.count(),
            "persistent": self.persistent.load(Ordering::SeqCst),
            "idle_timeout_secs": self.idle_timeout.as_secs_f64(),
        })
    }

    /// True when a non-persistent server has had no players and no requests for
    /// the idle timeout.
    fn is_idle(&self) -> bool {
        if self.persistent.load(Ordering::SeqCst)
            || self.busy.load(Ordering::SeqCst) > 0
            || self.players.count() > 0
        {
            return false;
        }
        let last = (*self.last_request.lock().unwrap()).max(*self.players.changed.lock().unwrap());
        last.elapsed() >= self.idle_timeout
    }

    fn auth_plugin(&self, name: &str) -> Result<&Plugin, String> {
        let plugin = self
            .registry
            .get(name)
            .ok_or_else(|| format!("no plugin named {name:?}"))?;
        if plugin.manifest.auth.is_none() {
            return Err(format!("{name} does not support login"));
        }
        Ok(plugin)
    }
}

/// Forced backend if given; else the plugin's logged-in preference when the user
/// has logged in; else the plugin's first backend.
fn choose_backend(
    plugin: &Plugin,
    forced: Option<BackendKind>,
    logged_in: bool,
) -> Result<BackendKind, String> {
    let supported = &plugin.manifest.backends;
    if let Some(b) = forced {
        return if supported.contains(&b) {
            Ok(b)
        } else {
            Err(format!(
                "{} does not support the {b:?} backend (supported: {supported:?})",
                plugin.name()
            ))
        };
    }
    if let Some(b) = plugin
        .manifest
        .auth
        .as_ref()
        .and_then(|a| a.prefer_backend_when_logged_in)
        && supported.contains(&b)
        && logged_in
    {
        return Ok(b);
    }
    supported
        .first()
        .copied()
        .ok_or_else(|| "plugin has no backends".into())
}

/// Runs the server. Unless `persistent`, it stops once it has been idle (no
/// players, no requests) for the idle timeout.
pub fn run(persistent: bool) -> Result<()> {
    let socket = client::socket_path();
    if client::is_running() {
        bail!("server already running at {}", socket.display());
    }
    let registry = Arc::new(Registry::load().context("loading plugins")?);
    let _ = std::fs::remove_file(&socket);
    let listener =
        UnixListener::bind(&socket).with_context(|| format!("binding {}", socket.display()))?;
    let players = Arc::new(Players::default());

    let (event_loop, webview) = webview::create(registry.clone(), players.clone())?;
    let chrome = chrome::Backend::new(players.clone());
    let router = Arc::new(Router {
        registry,
        players,
        webview,
        chrome,
        persistent: AtomicBool::new(persistent),
        busy: AtomicUsize::new(0),
        last_request: Mutex::new(Instant::now()),
        idle_timeout: idle_timeout(),
    });
    let monitor = router.clone();
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(Duration::from_millis(250));
            if monitor.is_idle() {
                eprintln!("idle for {:?}; stopping", monitor.idle_timeout);
                let _ = monitor.handle(Request::Shutdown);
                break;
            }
        }
    });
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let router = router.clone();
            std::thread::spawn(move || handle_connection(stream, &router));
        }
    });
    eprintln!(
        "mediactl server listening on {} (pid {})",
        socket.display(),
        std::process::id()
    );
    webview::run(event_loop)
}

fn handle_connection(stream: UnixStream, router: &Router) {
    let mut line = String::new();
    let Ok(read) = stream.try_clone() else { return };
    if BufReader::new(read).read_line(&mut line).is_err() {
        return;
    }
    let result = match serde_json::from_str::<Request>(&line) {
        // Status checks (`server status`, `server up` probes) do not keep an idle
        // server alive; everything else counts as activity.
        Ok(req @ Request::Ping) => router.handle(req),
        Ok(req) => {
            router.busy.fetch_add(1, Ordering::SeqCst);
            *router.last_request.lock().unwrap() = Instant::now();
            let r = router.handle(req);
            *router.last_request.lock().unwrap() = Instant::now();
            router.busy.fetch_sub(1, Ordering::SeqCst);
            r
        }
        Err(e) => Err(format!("bad request: {e}")),
    };
    let mut stream = stream;
    let mut out = serde_json::to_string(&Response::from(result)).expect("serializable");
    out.push('\n');
    let _ = stream.write_all(out.as_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn plugin(toml: &str, files: &[&str]) -> Plugin {
        let mut m: BTreeMap<String, String> = files
            .iter()
            .map(|f| (f.to_string(), String::new()))
            .collect();
        m.insert("plugin.toml".into(), toml.into());
        Plugin::from_files(m, "test".into()).unwrap()
    }

    const BOTH: &str =
        "name = \"x\"\ndomains = [\"x.com\"]\nbackends = [\"webview\", \"chrome\"]\n";
    const AUTH: &str = "[auth]\nlogin_url = \"https://x.com/l\"\ncookie_url = \"https://x.com\"\nlogged_in_cookies = [\"s\"]\nprefer_backend_when_logged_in = \"chrome\"\n";

    #[test]
    fn backend_defaults_to_first() {
        let p = plugin(BOTH, &["webview.html", "chrome.js"]);
        assert_eq!(choose_backend(&p, None, false), Ok(BackendKind::Webview));
        // Without an [auth] preference, login state does not matter.
        assert_eq!(choose_backend(&p, None, true), Ok(BackendKind::Webview));
    }

    #[test]
    fn backend_follows_login_preference() {
        let p = plugin(&format!("{BOTH}{AUTH}"), &["webview.html", "chrome.js"]);
        assert_eq!(choose_backend(&p, None, false), Ok(BackendKind::Webview));
        assert_eq!(choose_backend(&p, None, true), Ok(BackendKind::Chrome));
        // A forced backend wins over the preference.
        assert_eq!(
            choose_backend(&p, Some(BackendKind::Webview), true),
            Ok(BackendKind::Webview)
        );
    }

    #[test]
    fn forced_backend_must_be_supported() {
        let p = plugin(
            "name = \"x\"\ndomains = [\"x.com\"]\nbackends = [\"webview\"]\n",
            &["webview.html"],
        );
        assert_eq!(
            choose_backend(&p, Some(BackendKind::Webview), false),
            Ok(BackendKind::Webview)
        );
        let e = choose_backend(&p, Some(BackendKind::Chrome), false).unwrap_err();
        assert!(e.contains("does not support"), "{e}");
    }

    #[test]
    fn builtin_youtube_prefers_chrome_when_logged_in() {
        let r = Registry::load().unwrap();
        let yt = r.get("youtube").unwrap();
        assert_eq!(choose_backend(yt, None, false), Ok(BackendKind::Webview));
        assert_eq!(choose_backend(yt, None, true), Ok(BackendKind::Chrome));
    }

    #[test]
    fn player_ids_are_unique_and_owned() {
        let players = Players::default();
        let a = players.allocate(BackendKind::Webview);
        let b = players.allocate(BackendKind::Chrome);
        assert_ne!(a, b);
        assert_eq!(players.owner(&a), Ok(BackendKind::Webview));
        assert_eq!(players.owner(&b), Ok(BackendKind::Chrome));
        assert_eq!(players.count(), 2);
        players.remove(&a);
        let e = players.owner(&a).unwrap_err();
        assert!(
            e.contains("no player") && e.contains(&format!("{b:?}")),
            "{e}"
        );
        assert_eq!(players.count(), 1);
        // Ids are never reused.
        let c = players.allocate(BackendKind::Webview);
        assert!(c != a && c != b);
    }
}
