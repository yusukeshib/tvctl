//! The player server. Accepts requests on a Unix socket and routes them to the
//! backend that owns each player. Plugins decide which backend a URL uses.

mod webview;

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::client;
use crate::plugin::{BackendKind, Registry};
use crate::protocol::{Request, Response};

pub type Reply = std::sync::mpsc::Sender<Result<Value, String>>;

/// Player ids shared by all backends.
#[derive(Default)]
pub struct Players {
    next: AtomicU64,
    owners: Mutex<HashMap<String, BackendKind>>,
}

impl Players {
    pub fn allocate(&self, backend: BackendKind) -> String {
        let id = (self.next.fetch_add(1, Ordering::SeqCst) + 1).to_string();
        self.owners.lock().unwrap().insert(id.clone(), backend);
        id
    }

    pub fn remove(&self, id: &str) {
        self.owners.lock().unwrap().remove(id);
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
}

impl Router {
    fn handle(&self, req: Request) -> Result<Value, String> {
        match req {
            Request::Ping => Ok(json!({
                "pid": std::process::id(),
                "socket": client::socket_path(),
                "log": client::log_path(),
                "players": self.players.count(),
            })),
            Request::Shutdown => self
                .webview
                .call(webview::Command::Shutdown, Duration::from_secs(5)),
            Request::List => {
                let mut list = self
                    .webview
                    .call(webview::Command::List, Duration::from_secs(5))?;
                if let Some(items) = list.as_array_mut() {
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
                let (plugin, url) = self.registry.for_url(&url).map_err(|e| format!("{e:#}"))?;
                let backend = choose_backend(&plugin.manifest.backends, backend)?;
                let action = json!({
                    "action": "open",
                    "url": url.as_str(),
                    "start": start,
                    "paused": paused,
                    "subtitles": subtitles,
                });
                match backend {
                    BackendKind::Webview => {
                        // On failure the window stays open (showing the error) and keeps its id.
                        let id = self.players.allocate(backend);
                        let cmd = webview::Command::Open {
                            id,
                            plugin: Box::new(plugin.clone()),
                            action,
                            window: Box::new(window),
                        };
                        self.webview.call(cmd, Duration::from_secs(40))
                    }
                    BackendKind::Chrome => Err("the chrome backend is not available yet".into()),
                }
            }
            Request::Player { id, action } => match self.players.owner(&id)? {
                BackendKind::Webview => self.webview.call(
                    webview::Command::Player {
                        id,
                        action: serde_json::to_value(action).expect("serializable"),
                    },
                    Duration::from_secs(15),
                ),
                BackendKind::Chrome => unreachable!("no chrome players yet"),
            },
            Request::Window { id, opts } => match self.players.owner(&id)? {
                BackendKind::Webview => self.webview.call(
                    webview::Command::Window { id, opts },
                    Duration::from_secs(15),
                ),
                BackendKind::Chrome => unreachable!("no chrome players yet"),
            },
            Request::Close { id } => match self.players.owner(&id)? {
                BackendKind::Webview => self
                    .webview
                    .call(webview::Command::Close { id }, Duration::from_secs(5)),
                BackendKind::Chrome => unreachable!("no chrome players yet"),
            },
        }
    }
}

fn choose_backend(
    supported: &[BackendKind],
    forced: Option<BackendKind>,
) -> Result<BackendKind, String> {
    match forced {
        Some(b) if supported.contains(&b) => Ok(b),
        Some(b) => Err(format!(
            "this plugin does not support the {b:?} backend (supported: {supported:?})"
        )),
        None => supported
            .first()
            .copied()
            .ok_or_else(|| "plugin has no backends".into()),
    }
}

pub fn run() -> Result<()> {
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
    let router = Arc::new(Router {
        registry,
        players,
        webview,
    });
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let router = router.clone();
            std::thread::spawn(move || handle_connection(stream, &router));
        }
    });
    eprintln!(
        "play server listening on {} (pid {})",
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
        Ok(req) => router.handle(req),
        Err(e) => Err(format!("bad request: {e}")),
    };
    let mut stream = stream;
    let mut out = serde_json::to_string(&Response::from(result)).expect("serializable");
    out.push('\n');
    let _ = stream.write_all(out.as_bytes());
}
