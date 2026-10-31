mod client;
mod layout;
mod plugin;
mod protocol;
mod server;
mod timeparse;

use anyhow::{Result, anyhow};
use clap::{Args, Parser, Subcommand};
use serde_json::{Value, json};

use crate::layout::Layout;
use crate::protocol::{PlayerAction, Request, Switch, WindowOpts};
use crate::timeparse::Seek;

/// Play videos (YouTube, ...) in windows and control them from the command line.
///
/// Each supported service is a plugin (`play plugins` lists them).
///
/// Start the server once (`play server up`), open a video (`play open URL`, which
/// prints the new player's `id`), then control it with `play <command> <ID> ...`.
/// Every command prints JSON to stdout; errors go to stderr with exit code 1.
#[derive(Parser)]
#[command(version)]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Manage the background player server.
    Server {
        #[command(subcommand)]
        action: ServerCmd,
    },
    /// Open a URL in a new player window and print its id and status.
    Open {
        /// Video URL of a supported service (see `play plugins`).
        url: String,
        /// Force a backend instead of the plugin's default.
        #[arg(long, value_enum)]
        backend: Option<plugin::BackendKind>,
        /// Start position (e.g. 90, 1:30, 1m30s). Overrides `t=` in the URL.
        #[arg(long)]
        start: Option<String>,
        /// Load without starting playback.
        #[arg(long)]
        paused: bool,
        /// Turn subtitles on in this language (e.g. ja, en); auto-translates if needed.
        #[arg(long, value_name = "LANG")]
        subtitles: Option<String>,
        #[command(flatten)]
        window: GeometryArgs,
        /// Start in fullscreen.
        #[arg(long)]
        fullscreen: bool,
        /// Keep the window above other windows.
        #[arg(long)]
        on_top: bool,
    },
    /// List open players.
    List,
    /// List installed plugins (built-in and from the external plugin directory).
    Plugins,
    /// Show a player's playback and window status.
    Status { id: String },
    /// Resume playback.
    Play { id: String },
    /// Pause playback.
    Pause { id: String },
    /// Toggle between playing and paused.
    Toggle { id: String },
    /// Seek to a position: absolute (90, 1:30, 1m30s) or relative (+10, -30).
    Seek {
        id: String,
        #[arg(allow_hyphen_values = true)]
        position: String,
    },
    /// Skip forward (default 10 seconds).
    Forward {
        id: String,
        #[arg(default_value = "10")]
        duration: String,
    },
    /// Skip back (default 10 seconds).
    Back {
        id: String,
        #[arg(default_value = "10")]
        duration: String,
    },
    /// Subtitles: `on`, `off`, `list`, or a language code (e.g. `ja` for Japanese;
    /// auto-translated when the video has no track in that language).
    Subtitles { id: String, mode: String },
    /// Set volume (0-100).
    Volume {
        id: String,
        #[arg(value_parser = clap::value_parser!(u8).range(0..=100))]
        level: u8,
    },
    /// Mute audio.
    Mute { id: String },
    /// Unmute audio.
    Unmute { id: String },
    /// Set playback speed (e.g. 0.5, 1, 1.5, 2).
    Speed { id: String, rate: f64 },
    /// Request a playback quality: auto, small (240p), medium (360p), large (480p),
    /// hd720, hd1080, hd1440, hd2160, highres. `status` shows `available_qualities`.
    /// YouTube treats this as a hint and may ignore it.
    Quality {
        id: String,
        #[arg(value_parser = ["auto", "tiny", "small", "medium", "large", "hd720", "hd1080", "hd1440", "hd2160", "highres"])]
        level: String,
    },
    /// Move/resize the window, apply a layout, toggle fullscreen or always-on-top.
    Window {
        id: String,
        #[command(flatten)]
        geometry: GeometryArgs,
        #[arg(long, value_enum)]
        fullscreen: Option<Switch>,
        #[arg(long, value_enum)]
        on_top: Option<Switch>,
    },
    /// Fullscreen on/off/toggle (default toggle).
    Fullscreen {
        id: String,
        #[arg(value_enum, default_value = "toggle")]
        mode: Switch,
    },
    /// Close a player window.
    Close { id: String },
    #[command(name = "__serve", hide = true)]
    Serve,
}

#[derive(Subcommand)]
enum ServerCmd {
    /// Start the server in the background (no-op if already running).
    Up,
    /// Stop the server and close all players.
    Down,
    /// Show whether the server is running.
    Status,
}

#[derive(Args)]
struct GeometryArgs {
    /// Video area size, e.g. 1280x720.
    #[arg(long, value_parser = parse_pair::<'x'>)]
    size: Option<(f64, f64)>,
    /// Window top-left position on screen, e.g. 100,50.
    #[arg(long, value_parser = parse_pair::<','>)]
    position: Option<(f64, f64)>,
    /// Snap to a screen region. With --size, anchors the window to that side/corner.
    #[arg(long, value_enum)]
    layout: Option<Layout>,
}

fn parse_pair<const SEP: char>(s: &str) -> Result<(f64, f64), String> {
    let (a, b) = s
        .split_once(SEP)
        .ok_or_else(|| format!("expected two numbers separated by '{SEP}'"))?;
    let parse = |v: &str| v.trim().parse::<f64>().map_err(|e| format!("{v:?}: {e}"));
    Ok((parse(a)?, parse(b)?))
}

fn player(id: String, action: PlayerAction) -> Request {
    Request::Player { id, action }
}

fn seek_request(id: String, seek: Seek) -> Request {
    let (seconds, relative) = match seek {
        Seek::Absolute(s) => (s, false),
        Seek::Relative(s) => (s, true),
    };
    player(id, PlayerAction::Seek { seconds, relative })
}

fn run(cmd: Cmd) -> Result<Value> {
    let req = match cmd {
        Cmd::Serve => {
            server::run()?;
            return Ok(Value::Null);
        }
        Cmd::Server { action } => {
            return match action {
                ServerCmd::Up => client::server_up(),
                ServerCmd::Down => client::server_down(),
                ServerCmd::Status => client::server_status(),
            };
        }
        Cmd::Plugins => {
            let registry = plugin::Registry::load()?;
            return Ok(json!({
                "external_dir": plugin::external_dir(),
                "plugins": registry.plugins(),
            }));
        }
        Cmd::Open {
            url,
            backend,
            start,
            paused,
            subtitles,
            window,
            fullscreen,
            on_top,
        } => {
            let start = start.map(|s| timeparse::parse_duration(&s)).transpose()?;
            Request::Open {
                url,
                backend,
                start,
                paused,
                subtitles,
                window: WindowOpts {
                    size: window.size,
                    position: window.position,
                    layout: window.layout,
                    fullscreen: fullscreen.then_some(Switch::On),
                    on_top: on_top.then_some(Switch::On),
                },
            }
        }
        Cmd::List => Request::List,
        Cmd::Status { id } => player(id, PlayerAction::Status),
        Cmd::Play { id } => player(id, PlayerAction::Play),
        Cmd::Pause { id } => player(id, PlayerAction::Pause),
        Cmd::Toggle { id } => player(id, PlayerAction::Toggle),
        Cmd::Seek { id, position } => seek_request(id, timeparse::parse_seek(&position)?),
        Cmd::Forward { id, duration } => {
            seek_request(id, Seek::Relative(timeparse::parse_duration(&duration)?))
        }
        Cmd::Back { id, duration } => {
            seek_request(id, Seek::Relative(-timeparse::parse_duration(&duration)?))
        }
        Cmd::Subtitles { id, mode } => player(id, PlayerAction::Subtitles { lang: mode }),
        Cmd::Volume { id, level } => player(
            id,
            PlayerAction::Volume {
                level: Some(level),
                muted: None,
            },
        ),
        Cmd::Mute { id } => player(
            id,
            PlayerAction::Volume {
                level: None,
                muted: Some(true),
            },
        ),
        Cmd::Unmute { id } => player(
            id,
            PlayerAction::Volume {
                level: None,
                muted: Some(false),
            },
        ),
        Cmd::Speed { id, rate } => {
            if !(0.0625..=16.0).contains(&rate) {
                return Err(anyhow!("speed must be between 0.0625 and 16"));
            }
            player(id, PlayerAction::Speed { rate })
        }
        Cmd::Quality { id, level } => player(id, PlayerAction::Quality { level }),
        Cmd::Window {
            id,
            geometry,
            fullscreen,
            on_top,
        } => Request::Window {
            id,
            opts: WindowOpts {
                size: geometry.size,
                position: geometry.position,
                layout: geometry.layout,
                fullscreen,
                on_top,
            },
        },
        Cmd::Fullscreen { id, mode } => Request::Window {
            id,
            opts: WindowOpts {
                fullscreen: Some(mode),
                ..Default::default()
            },
        },
        Cmd::Close { id } => Request::Close { id },
    };
    client::send(&req)
}

fn main() {
    let cli = Cli::parse();
    let is_serve = matches!(cli.command, Cmd::Serve);
    match run(cli.command) {
        Ok(v) if !is_serve => {
            println!(
                "{}",
                serde_json::to_string_pretty(&v).expect("serializable")
            );
        }
        Ok(_) => {}
        Err(e) => {
            eprintln!("error: {e:#}");
            std::process::exit(1);
        }
    }
}
