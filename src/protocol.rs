//! Client/server wire protocol: one JSON request line, one JSON response line.

use serde::{Deserialize, Serialize};

use crate::layout::Layout;
use crate::plugin::BackendKind;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Request {
    Ping,
    Shutdown,
    /// Turn the idle shutdown off (`true`, as with `server up`) or on.
    SetPersistent {
        persistent: bool,
    },
    List,
    Open {
        url: String,
        start: Option<f64>,
        paused: bool,
        subtitles: Option<String>,
        window: WindowOpts,
        /// Force a backend instead of choosing automatically.
        backend: Option<BackendKind>,
    },
    Player {
        id: String,
        action: PlayerAction,
    },
    Window {
        id: String,
        opts: WindowOpts,
    },
    Close {
        id: String,
    },
    /// Evaluate JavaScript in a Chrome-backend player page (plugin development).
    Eval {
        id: String,
        script: String,
    },
    /// Save a PNG of the player window's content to `path` (absolute).
    Screenshot {
        id: String,
        path: String,
    },
    /// Open the service's login page in the Chrome profile; blocks until closed.
    AuthLogin {
        service: String,
        /// One of the service's login sites (`[auth.sites]`); the main one when None.
        #[serde(default)]
        site: Option<String>,
    },
    /// Check login state (all services with a login when `service` is None).
    AuthStatus {
        service: Option<String>,
    },
    /// Apply ducking settings (already saved by the client); returns the status.
    Duck {
        config: crate::duck::Config,
    },
    DuckStatus,
    /// Processes playing audio now.
    DuckApps,
}

/// Actions executed by the plugin adapter (forwarded to JavaScript as-is).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum PlayerAction {
    Status,
    Play,
    Pause,
    Toggle,
    /// Next item (track, playlist entry).
    Next,
    /// Previous item.
    Previous,
    Seek {
        seconds: f64,
        relative: bool,
    },
    /// `on`, `off`, `list`, or a language code such as `ja`.
    Subtitles {
        lang: String,
    },
    /// Audio track: `list` or a language code such as `ja`.
    Audio {
        lang: String,
    },
    Volume {
        level: Option<u8>,
        muted: Option<bool>,
    },
    Speed {
        rate: f64,
    },
    /// Quality level such as `auto`, `hd720`, `hd1080` (plugin-specific).
    Quality {
        level: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum Switch {
    On,
    Off,
    Toggle,
}

impl Switch {
    pub fn resolve(self, current: bool) -> bool {
        match self {
            Switch::On => true,
            Switch::Off => false,
            Switch::Toggle => !current,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WindowOpts {
    /// Video area size (logical pixels).
    pub size: Option<(f64, f64)>,
    /// Outer window top-left position (logical pixels).
    pub position: Option<(f64, f64)>,
    pub layout: Option<Layout>,
    pub fullscreen: Option<Switch>,
    pub on_top: Option<Switch>,
}

impl WindowOpts {
    pub fn changes_geometry(&self) -> bool {
        self.size.is_some() || self.position.is_some() || self.layout.is_some()
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Response {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl From<Result<serde_json::Value, String>> for Response {
    fn from(r: Result<serde_json::Value, String>) -> Self {
        match r {
            Ok(v) => Response {
                ok: true,
                result: Some(v),
                error: None,
            },
            Err(e) => Response {
                ok: false,
                result: None,
                error: Some(e),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Plugin adapters receive these objects verbatim; their shape is the contract.
    #[test]
    fn player_actions_match_the_plugin_contract() {
        let cases = [
            (PlayerAction::Status, json!({ "action": "status" })),
            (PlayerAction::Play, json!({ "action": "play" })),
            (PlayerAction::Pause, json!({ "action": "pause" })),
            (PlayerAction::Toggle, json!({ "action": "toggle" })),
            (PlayerAction::Next, json!({ "action": "next" })),
            (PlayerAction::Previous, json!({ "action": "previous" })),
            (
                PlayerAction::Seek {
                    seconds: -5.0,
                    relative: true,
                },
                json!({ "action": "seek", "seconds": -5.0, "relative": true }),
            ),
            (
                PlayerAction::Subtitles { lang: "ja".into() },
                json!({ "action": "subtitles", "lang": "ja" }),
            ),
            (
                PlayerAction::Audio {
                    lang: "list".into(),
                },
                json!({ "action": "audio", "lang": "list" }),
            ),
            (
                PlayerAction::Volume {
                    level: Some(5),
                    muted: None,
                },
                json!({ "action": "volume", "level": 5, "muted": null }),
            ),
            (
                PlayerAction::Speed { rate: 2.0 },
                json!({ "action": "speed", "rate": 2.0 }),
            ),
            (
                PlayerAction::Quality {
                    level: "hd720".into(),
                },
                json!({ "action": "quality", "level": "hd720" }),
            ),
        ];
        for (action, expected) in cases {
            let v = serde_json::to_value(&action).unwrap();
            assert_eq!(v, expected);
            let back: PlayerAction = serde_json::from_value(v.clone()).unwrap();
            assert_eq!(serde_json::to_value(back).unwrap(), v);
        }
    }

    #[test]
    fn requests_round_trip() {
        let reqs = [
            Request::Ping,
            Request::Shutdown,
            Request::List,
            Request::Close { id: "1".into() },
            Request::Screenshot {
                id: "1".into(),
                path: "/tmp/x.png".into(),
            },
            Request::AuthLogin {
                service: "youtube".into(),
                site: None,
            },
            Request::AuthLogin {
                service: "primevideo".into(),
                site: Some("jp".into()),
            },
            Request::AuthStatus { service: None },
            Request::Duck {
                config: crate::duck::Config {
                    enabled: true,
                    level: 10,
                    apps: Some(vec!["ChatGPT".into()]),
                },
            },
            Request::DuckStatus,
            Request::DuckApps,
            Request::Window {
                id: "1".into(),
                opts: WindowOpts {
                    layout: Some(Layout::TopRight),
                    ..Default::default()
                },
            },
        ];
        for r in reqs {
            let v = serde_json::to_value(&r).unwrap();
            let back: Request = serde_json::from_value(v.clone()).unwrap();
            assert_eq!(serde_json::to_value(back).unwrap(), v);
        }
        assert!(serde_json::from_str::<Request>("{\"cmd\":\"explode\"}").is_err());
    }

    #[test]
    fn responses() {
        let ok = serde_json::to_value(Response::from(Ok(json!(1)))).unwrap();
        assert_eq!(ok, json!({ "ok": true, "result": 1 }));
        let err = serde_json::to_value(Response::from(Err("bad".to_string()))).unwrap();
        assert_eq!(err, json!({ "ok": false, "error": "bad" }));
    }

    #[test]
    fn switches_and_geometry_flags() {
        assert!(Switch::On.resolve(false));
        assert!(!Switch::Off.resolve(true));
        assert!(Switch::Toggle.resolve(false));
        assert!(!Switch::Toggle.resolve(true));
        assert!(!WindowOpts::default().changes_geometry());
        assert!(
            !WindowOpts {
                fullscreen: Some(Switch::On),
                on_top: Some(Switch::On),
                ..Default::default()
            }
            .changes_geometry()
        );
        assert!(
            WindowOpts {
                size: Some((1.0, 1.0)),
                ..Default::default()
            }
            .changes_geometry()
        );
        assert!(
            WindowOpts {
                position: Some((1.0, 1.0)),
                ..Default::default()
            }
            .changes_geometry()
        );
        assert!(
            WindowOpts {
                layout: Some(Layout::Left),
                ..Default::default()
            }
            .changes_geometry()
        );
    }
}
