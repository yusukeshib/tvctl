//! Client/server wire protocol: one JSON request line, one JSON response line.

use serde::{Deserialize, Serialize};

use crate::layout::Layout;
use crate::plugin::BackendKind;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Request {
    Ping,
    Shutdown,
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
}

/// Actions executed by the plugin adapter (forwarded to JavaScript as-is).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum PlayerAction {
    Status,
    Play,
    Pause,
    Toggle,
    Seek {
        seconds: f64,
        relative: bool,
    },
    /// `on`, `off`, `list`, or a language code such as `ja`.
    Subtitles {
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
