mod auth;
mod cdp;
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
/// Each supported service is a plugin (`mediactl plugins` lists them).
///
/// Start the server once (`mediactl server up`), open a video (`mediactl open URL`, which
/// prints the new player's `id`), then control it with `mediactl <command> <ID> ...`.
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
        /// Video URL of a supported service (see `mediactl plugins`).
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
    /// Log in to services in mediactl's Chrome profile (enables logged-in playback).
    Auth {
        #[command(subcommand)]
        action: AuthCmd,
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
    /// Next item (track or playlist entry).
    Next { id: String },
    /// Previous item.
    Prev { id: String },
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
    /// Audio track: `list` or a language code (e.g. `ja` for Japanese dubbing).
    Audio { id: String, lang: String },
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
    /// Save a PNG of what the player window shows (works when the window is covered).
    Screenshot {
        id: String,
        path: std::path::PathBuf,
    },
    #[command(name = "__serve", hide = true)]
    Serve,
    /// Evaluate JavaScript in a Chrome-backend player page (plugin development).
    #[command(name = "__eval", hide = true)]
    Eval { id: String, script: String },
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

#[derive(Subcommand)]
enum AuthCmd {
    /// Open the service's login page in mediactl's Chrome profile. Sign in, then
    /// quit that Chrome (Cmd+Q); prints the resulting login state.
    Login { service: String },
    /// Show login state for one or all services.
    Status { service: Option<String> },
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

/// What a command does: something local, or one request to the server.
#[derive(Debug)]
enum Plan {
    Serve,
    ServerUp,
    ServerDown,
    ServerStatus,
    Plugins,
    Remote(Request),
}

fn plan(cmd: Cmd) -> Result<Plan> {
    let req = match cmd {
        Cmd::Serve => return Ok(Plan::Serve),
        Cmd::Server { action } => {
            return Ok(match action {
                ServerCmd::Up => Plan::ServerUp,
                ServerCmd::Down => Plan::ServerDown,
                ServerCmd::Status => Plan::ServerStatus,
            });
        }
        Cmd::Plugins => return Ok(Plan::Plugins),
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
        Cmd::Auth { action } => match action {
            AuthCmd::Login { service } => Request::AuthLogin { service },
            AuthCmd::Status { service } => Request::AuthStatus { service },
        },
        Cmd::List => Request::List,
        Cmd::Audio { id, lang } => player(id, PlayerAction::Audio { lang }),
        Cmd::Status { id } => player(id, PlayerAction::Status),
        Cmd::Play { id } => player(id, PlayerAction::Play),
        Cmd::Pause { id } => player(id, PlayerAction::Pause),
        Cmd::Toggle { id } => player(id, PlayerAction::Toggle),
        Cmd::Next { id } => player(id, PlayerAction::Next),
        Cmd::Prev { id } => player(id, PlayerAction::Previous),
        Cmd::Eval { id, script } => Request::Eval { id, script },
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
        Cmd::Screenshot { id, path } => {
            // The server has a different working directory; send an absolute path.
            let path = std::path::absolute(&path)?;
            Request::Screenshot {
                id,
                path: path.to_string_lossy().into_owned(),
            }
        }
    };
    Ok(Plan::Remote(req))
}

fn run(cmd: Cmd) -> Result<Value> {
    match plan(cmd)? {
        Plan::Serve => {
            server::run()?;
            Ok(Value::Null)
        }
        Plan::ServerUp => client::server_up(),
        Plan::ServerDown => client::server_down(),
        Plan::ServerStatus => client::server_status(),
        Plan::Plugins => {
            let registry = plugin::Registry::load()?;
            Ok(json!({
                "external_dir": plugin::external_dir(),
                "plugins": registry.plugins(),
            }))
        }
        Plan::Remote(req) => {
            if let Request::AuthLogin { service } = &req {
                eprintln!(
                    "Sign in to {service} in the Chrome window that opens, then quit that Chrome (Cmd+Q) \
                     (for some services, closing the login window is enough)."
                );
            }
            client::send(&req)
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Parses a command line and returns the request JSON sent to the server.
    fn req(args: &[&str]) -> Value {
        let mut argv = vec!["mediactl"];
        argv.extend_from_slice(args);
        let cli = Cli::try_parse_from(argv).unwrap_or_else(|e| panic!("{args:?}: {e}"));
        match plan(cli.command).unwrap_or_else(|e| panic!("{args:?}: {e}")) {
            Plan::Remote(r) => serde_json::to_value(r).unwrap(),
            other => panic!("{args:?}: expected a remote request, got {other:?}"),
        }
    }

    fn rejects(args: &[&str]) {
        let mut argv = vec!["mediactl"];
        argv.extend_from_slice(args);
        let ok = Cli::try_parse_from(argv)
            .map(|c| plan(c.command).is_ok())
            .unwrap_or(false);
        assert!(!ok, "{args:?} should be rejected");
    }

    fn action(id: &str, action: Value) -> Value {
        json!({ "cmd": "player", "id": id, "action": action })
    }

    #[test]
    fn local_commands() {
        for (args, expect) in [
            (vec!["server", "up"], "ServerUp"),
            (vec!["server", "down"], "ServerDown"),
            (vec!["server", "status"], "ServerStatus"),
            (vec!["plugins"], "Plugins"),
            (vec!["__serve"], "Serve"),
        ] {
            let mut argv = vec!["mediactl"];
            argv.extend(args.iter());
            let p = plan(Cli::try_parse_from(argv).unwrap().command).unwrap();
            assert_eq!(format!("{p:?}"), expect);
        }
    }

    #[test]
    fn open() {
        assert_eq!(
            req(&["open", "https://youtu.be/x"]),
            json!({
                "cmd": "open", "url": "https://youtu.be/x", "backend": null, "start": null,
                "paused": false, "subtitles": null,
                "window": { "size": null, "position": null, "layout": null, "fullscreen": null, "on_top": null },
            })
        );
        assert_eq!(
            req(&[
                "open",
                "u",
                "--backend",
                "chrome",
                "--start",
                "1:30",
                "--paused",
                "--subtitles",
                "ja",
                "--size",
                "640x360",
                "--position",
                "10,20",
                "--layout",
                "bottom-right",
                "--fullscreen",
                "--on-top",
            ]),
            json!({
                "cmd": "open", "url": "u", "backend": "chrome", "start": 90.0, "paused": true, "subtitles": "ja",
                "window": { "size": [640.0, 360.0], "position": [10.0, 20.0], "layout": "bottom-right",
                            "fullscreen": "on", "on_top": "on" },
            })
        );
        rejects(&["open"]);
        rejects(&["open", "u", "--start", "abc"]);
        rejects(&["open", "u", "--size", "640"]);
        rejects(&["open", "u", "--position", "1x2"]);
        rejects(&["open", "u", "--layout", "middle"]);
        rejects(&["open", "u", "--backend", "firefox"]);
    }

    #[test]
    fn playback() {
        assert_eq!(
            req(&["status", "1"]),
            action("1", json!({ "action": "status" }))
        );
        assert_eq!(
            req(&["play", "1"]),
            action("1", json!({ "action": "play" }))
        );
        assert_eq!(
            req(&["pause", "2"]),
            action("2", json!({ "action": "pause" }))
        );
        assert_eq!(
            req(&["toggle", "3"]),
            action("3", json!({ "action": "toggle" }))
        );
        assert_eq!(
            req(&["next", "1"]),
            action("1", json!({ "action": "next" }))
        );
        assert_eq!(
            req(&["prev", "1"]),
            action("1", json!({ "action": "previous" }))
        );
        assert_eq!(
            req(&["__eval", "1", "1+1"]),
            json!({ "cmd": "eval", "id": "1", "script": "1+1" })
        );
        rejects(&["play"]);
    }

    #[test]
    fn seeking() {
        let seek = |s: f64, r: bool| {
            action(
                "1",
                json!({ "action": "seek", "seconds": s, "relative": r }),
            )
        };
        assert_eq!(req(&["seek", "1", "90"]), seek(90.0, false));
        assert_eq!(req(&["seek", "1", "1:02:03"]), seek(3723.0, false));
        assert_eq!(req(&["seek", "1", "+10"]), seek(10.0, true));
        assert_eq!(req(&["seek", "1", "-1m"]), seek(-60.0, true));
        assert_eq!(req(&["forward", "1"]), seek(10.0, true));
        assert_eq!(req(&["forward", "1", "30"]), seek(30.0, true));
        assert_eq!(req(&["back", "1"]), seek(-10.0, true));
        assert_eq!(req(&["back", "1", "1:00"]), seek(-60.0, true));
        rejects(&["seek", "1", "soon"]);
        rejects(&["forward", "1", "-5"]);
    }

    #[test]
    fn tracks_and_audio_settings() {
        assert_eq!(
            req(&["subtitles", "1", "ja"]),
            action("1", json!({ "action": "subtitles", "lang": "ja" }))
        );
        assert_eq!(
            req(&["subtitles", "1", "off"]),
            action("1", json!({ "action": "subtitles", "lang": "off" }))
        );
        assert_eq!(
            req(&["audio", "1", "list"]),
            action("1", json!({ "action": "audio", "lang": "list" }))
        );
        assert_eq!(
            req(&["quality", "1", "hd1080"]),
            action("1", json!({ "action": "quality", "level": "hd1080" }))
        );
        rejects(&["quality", "1", "8k"]);
        assert_eq!(
            req(&["volume", "1", "30"]),
            action(
                "1",
                json!({ "action": "volume", "level": 30, "muted": null })
            )
        );
        rejects(&["volume", "1", "101"]);
        assert_eq!(
            req(&["mute", "1"]),
            action(
                "1",
                json!({ "action": "volume", "level": null, "muted": true })
            )
        );
        assert_eq!(
            req(&["unmute", "1"]),
            action(
                "1",
                json!({ "action": "volume", "level": null, "muted": false })
            )
        );
        assert_eq!(
            req(&["speed", "1", "1.5"]),
            action("1", json!({ "action": "speed", "rate": 1.5 }))
        );
        rejects(&["speed", "1", "0"]);
        rejects(&["speed", "1", "32"]);
    }

    #[test]
    fn window_commands() {
        let opts = |v: Value| {
            let mut o = json!({ "size": null, "position": null, "layout": null, "fullscreen": null, "on_top": null });
            for (k, val) in v.as_object().unwrap() {
                o[k] = val.clone();
            }
            o
        };
        assert_eq!(
            req(&["window", "1", "--size", "800x450", "--position", "5,6"]),
            json!({ "cmd": "window", "id": "1", "opts": opts(json!({ "size": [800.0, 450.0], "position": [5.0, 6.0] })) })
        );
        for l in [
            "left",
            "right",
            "top",
            "bottom",
            "top-left",
            "top-right",
            "bottom-left",
            "bottom-right",
            "center",
            "maximize",
        ] {
            assert_eq!(
                req(&["window", "1", "--layout", l]),
                json!({ "cmd": "window", "id": "1", "opts": opts(json!({ "layout": l })) })
            );
        }
        assert_eq!(
            req(&["window", "1", "--fullscreen", "off", "--on-top", "toggle"]),
            json!({ "cmd": "window", "id": "1", "opts": opts(json!({ "fullscreen": "off", "on_top": "toggle" })) })
        );
        assert_eq!(
            req(&["fullscreen", "1"]),
            json!({ "cmd": "window", "id": "1", "opts": opts(json!({ "fullscreen": "toggle" })) })
        );
        assert_eq!(
            req(&["fullscreen", "1", "on"]),
            json!({ "cmd": "window", "id": "1", "opts": opts(json!({ "fullscreen": "on" })) })
        );
        rejects(&["fullscreen", "1", "maybe"]);
    }

    #[test]
    fn management_commands() {
        assert_eq!(req(&["list"]), json!({ "cmd": "list" }));
        assert_eq!(req(&["close", "4"]), json!({ "cmd": "close", "id": "4" }));
        assert_eq!(
            req(&["auth", "login", "youtube"]),
            json!({ "cmd": "auth_login", "service": "youtube" })
        );
        assert_eq!(
            req(&["auth", "status"]),
            json!({ "cmd": "auth_status", "service": null })
        );
        assert_eq!(
            req(&["auth", "status", "netflix"]),
            json!({ "cmd": "auth_status", "service": "netflix" })
        );
        let shot = req(&["screenshot", "1", "shot.png"]);
        let path = shot["path"].as_str().unwrap();
        assert!(
            std::path::Path::new(path).is_absolute() && path.ends_with("shot.png"),
            "{path}"
        );
    }
}
