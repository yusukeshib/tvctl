//! `tvctl cast`: a YouTube cast receiver. Phones and computers on the same
//! network see this Mac in the YouTube app's Cast menu (DIAL); what they cast is
//! played in a tvctl player and controlled from the app (YouTube Lounge API).

mod dial;
mod lounge;
mod receiver;

use std::path::{Path, PathBuf};
use std::sync::{Condvar, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::auth;
use crate::protocol::{PlayerAction, WindowOpts};

/// Name used before names included the computer's name.
const OLD_DEFAULT_NAME: &str = "tvctl";

/// The computer's name (System Settings > General > Sharing), if known.
fn computer_name() -> Option<String> {
    let out = std::process::Command::new("/usr/sbin/scutil")
        .args(["--get", "ComputerName"])
        .output()
        .ok()?;
    let name = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (out.status.success() && !name.is_empty()).then_some(name)
}

fn name_for(computer: Option<&str>) -> String {
    match computer {
        Some(c) => format!("tvctl ({c})"),
        None => OLD_DEFAULT_NAME.to_string(),
    }
}

/// Default name in the Cast menu: `tvctl (<computer name>)`, so several Macs
/// can be told apart.
pub fn default_name() -> String {
    name_for(computer_name().as_deref())
}

/// The name the receiver uses for `config` (old or missing names get the default).
fn effective_name(config: &Config) -> String {
    if config.name.is_empty() || config.name == OLD_DEFAULT_NAME {
        default_name()
    } else {
        config.name.clone()
    }
}

/// The name for `cast on` without `--name`: the saved one if it was chosen,
/// else the default.
pub fn name_or_saved(name: Option<String>) -> String {
    if let Some(n) = name {
        return n;
    }
    let saved = load().name;
    if saved.is_empty() || saved == OLD_DEFAULT_NAME {
        default_name()
    } else {
        saved
    }
}

/// Saved settings (`cast.json` in the data dir). An empty name means the default.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Config {
    pub enabled: bool,
    /// Name shown in the YouTube app's Cast menu.
    pub name: String,
}

fn config_path(dir: &Path) -> PathBuf {
    dir.join("cast.json")
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

/// Ids that identify this receiver to YouTube (`cast-state.json`, written by the
/// server). Keeping them lets phones find the same "TV" again.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Identity {
    uuid: String,
    device_id: String,
    screen_id: String,
}

fn identity_path(dir: &Path) -> PathBuf {
    dir.join("cast-state.json")
}

fn load_identity(dir: &Path) -> Identity {
    let mut id: Identity = std::fs::read_to_string(identity_path(dir))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    if id.uuid.is_empty() {
        id.uuid = lounge::random_uuid();
    }
    if id.device_id.is_empty() {
        id.device_id = lounge::random_uuid();
    }
    id
}

fn save_identity(dir: &Path, id: &Identity) {
    let _ = std::fs::create_dir_all(dir);
    if let Err(e) = std::fs::write(
        identity_path(dir),
        serde_json::to_string_pretty(id).unwrap_or_default(),
    ) {
        eprintln!("cast: saving {}: {e}", identity_path(dir).display());
    }
}

/// Status when no server is running.
pub fn offline_status(config: &Config) -> Value {
    json!({
        "enabled": config.enabled,
        "name": config.name,
        "server_running": false,
        "running": false,
        "senders": [],
        "player": null,
        "video_id": null,
    })
}

/// What the receiver needs from the server: tvctl players.
pub trait Host: Sync {
    fn open(&self, url: &str, start: f64, window: WindowOpts) -> Result<Value, String>;
    fn player(&self, id: &str, action: PlayerAction) -> Result<Value, String>;
    fn close(&self, id: &str) -> Result<Value, String>;
}

/// Live state for `cast status`.
#[derive(Debug, Default, Clone)]
pub struct Status {
    pub running: bool,
    pub senders: Vec<String>,
    pub player: Option<String>,
    pub video_id: Option<String>,
    /// HTTP port of the DIAL server.
    pub dial_port: Option<u16>,
    pub error: Option<String>,
}

struct Inner {
    config: Config,
    changed: bool,
    status: Status,
    lounge: Option<std::sync::Arc<lounge::Lounge>>,
}

/// Cast state shared by the server's request handlers and the receiver thread.
pub struct Cast {
    inner: Mutex<Inner>,
    wake: Condvar,
}

impl Cast {
    pub fn new(config: Config) -> Cast {
        Cast {
            inner: Mutex::new(Inner {
                config,
                changed: true,
                status: Status::default(),
                lounge: None,
            }),
            wake: Condvar::new(),
        }
    }

    pub fn set_config(&self, config: Config) -> Value {
        let mut inner = self.inner.lock().unwrap();
        if inner.config != config {
            inner.config = config;
            inner.changed = true;
            self.wake.notify_all();
        }
        drop(inner);
        // Give the receiver a moment to start, so the status shows the result.
        for _ in 0..40 {
            let inner = self.inner.lock().unwrap();
            if !inner.changed
                && (!inner.config.enabled || inner.status.running || inner.status.error.is_some())
            {
                break;
            }
            drop(inner);
            std::thread::sleep(Duration::from_millis(250));
        }
        self.status()
    }

    pub fn status(&self) -> Value {
        let inner = self.inner.lock().unwrap();
        let s = &inner.status;
        let mut v = json!({
            "enabled": inner.config.enabled,
            "name": inner.config.name,
            "server_running": true,
            "running": s.running,
            "senders": s.senders,
            "player": s.player,
            "video_id": s.video_id,
            "dial_port": s.dial_port,
        });
        if let Some(e) = &s.error {
            v["error"] = json!(e);
        }
        v
    }

    /// A code for the YouTube app's "Link with TV code".
    pub fn tv_code(&self) -> Result<Value, String> {
        let lounge = self
            .inner
            .lock()
            .unwrap()
            .lounge
            .clone()
            .ok_or("the cast receiver is not running (`tvctl cast on`)")?;
        let code = lounge.pairing_code().map_err(|e| format!("{e:#}"))?;
        Ok(json!({ "code": code }))
    }

    fn update(&self, f: impl FnOnce(&mut Status)) {
        f(&mut self.inner.lock().unwrap().status);
    }
}

/// The receiver thread (runs for the server's lifetime): runs the receiver
/// while casting is enabled.
pub fn run(cast: &Cast, host: &dyn Host) {
    let dir = auth::data_dir();
    loop {
        let config = {
            let mut inner = cast.inner.lock().unwrap();
            while !inner.config.enabled {
                inner.changed = false;
                inner.status = Status::default();
                inner.lounge = None;
                inner = cast.wake.wait(inner).unwrap();
            }
            inner.changed = false;
            let mut c = inner.config.clone();
            c.name = effective_name(&c);
            inner.config.name = c.name.clone();
            c
        };
        let stop = || cast.inner.lock().unwrap().changed;
        if let Err(e) = receiver::run(&config, &dir, cast, host, &stop) {
            let msg = format!("{e:#}");
            eprintln!("cast: {msg}");
            cast.update(|s| {
                *s = Status {
                    error: Some(msg),
                    ..Status::default()
                }
            });
            cast.inner.lock().unwrap().lounge = None;
            // Retry later, or as soon as the settings change.
            let inner = cast.inner.lock().unwrap();
            if !inner.changed {
                let _ = cast.wake.wait_timeout(inner, Duration::from_secs(30));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_round_trip_and_default() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load_from(dir.path()), Config::default());
        let c = Config {
            enabled: true,
            name: "Living room".into(),
        };
        save_in(dir.path(), &c).unwrap();
        assert_eq!(load_from(dir.path()), c);
    }

    #[test]
    fn default_names() {
        assert_eq!(name_for(Some("Studio")), "tvctl (Studio)");
        assert_eq!(name_for(None), "tvctl");
        assert!(default_name().starts_with("tvctl"));
    }

    #[test]
    fn identity_is_created_and_kept() {
        let dir = tempfile::tempdir().unwrap();
        let a = load_identity(dir.path());
        assert!(!a.uuid.is_empty() && !a.device_id.is_empty() && a.screen_id.is_empty());
        save_identity(
            dir.path(),
            &Identity {
                screen_id: "s".into(),
                ..a.clone()
            },
        );
        let b = load_identity(dir.path());
        assert_eq!(
            (b.uuid, b.device_id, b.screen_id),
            (a.uuid, a.device_id, "s".into())
        );
    }
}
