//! The player server. Accepts requests on a Unix socket and drives the one
//! player window (in tvctl's Chrome), the cast receiver and ducking. Plugins
//! decide how a URL is played.

mod chrome;

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::cast::{self, HostError};
use crate::client;
use crate::duck;
use crate::plugin::{Plugin, Registry};
use crate::protocol::{PlayerAction, Request, Response, WindowOpts};

pub use chrome::Gone;

struct Router {
    registry: Arc<Registry>,
    chrome: chrome::Backend,
    /// Started from an SSH session (see `SSH_SESSION_WARNING`).
    ssh_session: bool,
    duck: duck::Duck,
    cast: cast::Cast,
}

fn host_error(e: anyhow::Error) -> HostError {
    match e.downcast_ref::<Gone>() {
        Some(_) => HostError::Gone,
        None => HostError::Failed(err(e)),
    }
}

impl cast::Host for Router {
    fn open(&self, url: &str, start: f64, expect: Option<u64>) -> Result<(u64, Value), HostError> {
        let (plugin, url) = self
            .registry
            .for_url(url)
            .map_err(|e| HostError::Failed(err(e)))?;
        let action = json!({
            "action": "open",
            "url": url.as_str(),
            "start": start,
            "paused": false,
            "subtitles": null,
        });
        self.chrome
            .open(plugin, action, &WindowOpts::default(), expect)
            .map_err(host_error)
    }

    fn player(&self, token: u64, action: PlayerAction) -> Result<Value, HostError> {
        let action = serde_json::to_value(action).expect("serializable");
        self.chrome.action(Some(token), action).map_err(host_error)
    }

    fn close(&self, token: u64) -> Result<(), HostError> {
        self.chrome
            .close(Some(token))
            .map(|_| ())
            .map_err(host_error)
    }
}

impl duck::Host for Router {
    fn has_player(&self) -> bool {
        self.chrome.has_player()
    }

    fn run_in_player(&self, script: &str) {
        self.chrome.run_in_player(script);
    }

    fn chrome_pid(&self) -> Option<i32> {
        chrome::browser_pid()
    }
}

/// macOS keeps the key that Chrome encrypts cookies with in the login keychain,
/// which processes of an SSH session cannot use: Chrome started by such a
/// server neither saves new logins nor loads saved ones.
const SSH_SESSION_WARNING: &str = "the tvctl server was started from an SSH session, so its Chrome cannot \
     use the macOS keychain: logins are not saved and saved logins are not loaded. Run \
     `tvctl server down`, then run tvctl from a terminal on this Mac's own screen (not over SSH)";

/// Whether this process belongs to an SSH session.
fn in_ssh_session() -> bool {
    ["SSH_CONNECTION", "SSH_CLIENT", "SSH_TTY"]
        .iter()
        .any(|v| std::env::var_os(v).is_some_and(|s| !s.is_empty()))
}

fn err(e: anyhow::Error) -> String {
    format!("{e:#}")
}

impl Router {
    /// Handles a request; logins and player responses of a server started over
    /// SSH get a warning (logins there do not persist, so `auth login` is
    /// refused).
    fn handle(&self, req: Request) -> Result<Value, String> {
        if !self.ssh_session {
            return self.handle_inner(req);
        }
        let warn = match &req {
            Request::AuthLogin { .. } => return Err(SSH_SESSION_WARNING.to_string()),
            Request::AuthStatus { .. } | Request::Ping => true,
            Request::Open { .. } => false,
            _ => return self.handle_inner(req),
        };
        let mut v = self.handle_inner(req)?;
        if (warn || v.get("player").is_some())
            && let Some(obj) = v.as_object_mut()
        {
            obj.insert("warning".into(), SSH_SESSION_WARNING.into());
        }
        Ok(v)
    }

    fn handle_inner(&self, req: Request) -> Result<Value, String> {
        match req {
            Request::Ping => Ok(self.info()),
            Request::Shutdown => {
                self.chrome.shutdown();
                // The reply is sent before the process exits.
                std::thread::spawn(|| {
                    std::thread::sleep(std::time::Duration::from_millis(200));
                    let _ = std::fs::remove_file(client::socket_path());
                    std::process::exit(0);
                });
                Ok(json!({ "stopped": true }))
            }
            Request::Open {
                url,
                start,
                paused,
                subtitles,
                window,
            } => {
                let (plugin, url) = self.registry.for_url(&url).map_err(err)?;
                let action = json!({
                    "action": "open",
                    "url": url.as_str(),
                    "start": start,
                    "paused": paused,
                    "subtitles": subtitles,
                });
                self.chrome
                    .open(plugin, action, &window, None)
                    .map(|(_, v)| v)
                    .map_err(err)
            }
            Request::Player { action } => self.player(action),
            Request::Window { opts } => self.chrome.window(&opts).map_err(err),
            Request::Close => self.chrome.close(None).map_err(err),
            Request::Screenshot { path } => self.chrome.screenshot(&path).map_err(err),
            Request::Eval { script } => self.chrome.eval(&script).map_err(err),
            Request::AuthLogin { service, site } => self
                .chrome
                .login(self.auth_plugin(&service)?, site.as_deref())
                .map_err(err),
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
            Request::Duck { config } => Ok(self.duck.set_config(config)),
            Request::DuckStatus => Ok(self.duck.status()),
            Request::DuckApps => Ok(duck::apps(Some(self))),
            Request::Cast { config } => Ok(self.cast.set_config(config)),
            Request::CastStatus => Ok(self.cast.status()),
            Request::CastCode => self.cast.tv_code(),
        }
    }

    /// A player command from the command line. `next`/`prev` on a cast video
    /// move through the cast queue, so the YouTube app follows.
    fn player(&self, action: PlayerAction) -> Result<Value, String> {
        let (generation, plugin) = self.chrome.current().ok_or(Gone::NoPlayer.to_string())?;
        let value = serde_json::to_value(&action).expect("serializable");
        let name = value["action"].as_str().unwrap_or_default();
        if let Some(m) = self.registry.get(&plugin).map(|p| &p.manifest)
            && !m.supports(name)
        {
            let supported: Vec<&str> = std::iter::once("status")
                .chain(m.actions.iter().map(String::as_str))
                .collect();
            return Err(format!(
                "{name} is not supported by {plugin}; supported: {}",
                supported.join(", ")
            ));
        }
        let step = match action {
            PlayerAction::Next => Some(1),
            PlayerAction::Previous => Some(-1),
            _ => None,
        };
        if let Some(step) = step
            && self.cast.owns(generation)
            && let Some(result) = self.cast.step(step)
        {
            return result;
        }
        self.chrome.action(None, value).map_err(err)
    }

    fn info(&self) -> Value {
        json!({
            "pid": std::process::id(),
            "socket": client::socket_path(),
            "log": client::log_path(),
            "player": self.chrome.summary(),
            "ssh_session": self.ssh_session,
        })
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

/// Runs the server until a `Shutdown` request (`tvctl server down`). Opening or
/// closing the player never starts or stops it.
pub fn run() -> Result<()> {
    let socket = client::socket_path();
    if client::is_running() {
        bail!("server already running at {}", socket.display());
    }
    let registry = Arc::new(Registry::load().context("loading plugins")?);
    let _ = std::fs::remove_file(&socket);
    let listener =
        UnixListener::bind(&socket).with_context(|| format!("binding {}", socket.display()))?;
    let router = Arc::new(Router {
        registry,
        chrome: chrome::Backend::new(),
        ssh_session: in_ssh_session(),
        duck: duck::Duck::new(duck::load()),
        cast: cast::Cast::new(cast::load()),
    });
    let duck_host = router.clone();
    std::thread::spawn(move || duck::run(&duck_host.duck, &*duck_host));
    let cast_host = router.clone();
    std::thread::spawn(move || cast::run(&cast_host.cast, &*cast_host));
    eprintln!(
        "tvctl server listening on {} (pid {})",
        socket.display(),
        std::process::id()
    );
    for stream in listener.incoming().flatten() {
        let router = router.clone();
        std::thread::spawn(move || handle_connection(stream, &router));
    }
    Ok(())
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
