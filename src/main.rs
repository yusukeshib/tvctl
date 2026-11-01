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

const ABOUT: &str = "Play videos and music (YouTube, Spotify, Netflix, Prime Video, ...) in windows and control them from the command line.";

const LONG_ABOUT: &str = "\
Play videos and music (YouTube, Spotify, Netflix, Prime Video, ...) in windows and control them from the command line.

Workflow:
  1. `tvctl open <URL>` opens a player window and prints its `id`.
  2. Control it with `tvctl <command> <ID> ...`; `tvctl list` shows open players.
  3. `tvctl close <ID>`. A background server starts on demand and stops a few
     seconds after the last player closes; no setup is needed.

Each service is a plugin. `tvctl plugins` shows, per plugin, the URL domains it
handles, its `actions` (supported player commands) and `unsupported_actions` per
backend. Unsupported commands fail with an error that lists the supported ones.";

const AFTER_LONG_HELP: &str = "\
Output:
  Every successful command prints one JSON document to stdout and exits 0.
  Player commands (open, status, play, seek, ...) print:
    {\"id\": \"1\", \"service\": \"youtube\", \"backend\": \"webview\" | \"chrome\",
     \"player\": {...}, \"window\": {...}}
  `player` comes from the plugin. Common fields: `state` (\"playing\", \"paused\",
  \"buffering\", \"ended\", \"unstarted\", \"cued\"), `title`, `current_time` and
  `duration` (seconds), `volume` (0-100), `muted`, `playback_rate`, `subtitles`,
  `quality`, `available_qualities`. Fields a plugin cannot report are null or absent.
  `window`: `position` {x, y} and `outer_size` of the window frame, `size` of the
  video area, `fullscreen`, `on_top`, `work_area` (usable screen area). Coordinates
  are logical pixels from the top-left of the main screen.

Errors:
  On failure, a message starting with `error:` is printed to stderr and the exit
  code is 1 (no JSON on stdout).

Examples:
  tvctl open 'https://www.youtube.com/watch?v=iG9CE55wbtY' --subtitles ja
  tvctl open 'https://youtu.be/jNQXAC9IVRw' --layout bottom-right --size 480x270 --on-top
  tvctl open 'https://open.spotify.com/album/5Z9iiGl2FcIfa3BMiv6OIw'
  tvctl open 'https://www.netflix.com/watch/81716297' --subtitles ja
  tvctl open 'https://www.amazon.com/gp/video/detail/B0GHXVBQLM' --subtitles ja
  tvctl status 1                  # read state, time, title, window
  tvctl seek 1 +30                # relative; `seek 1 1:30` is absolute
  tvctl subtitles 1 ja            # Japanese subtitles (auto-translated if needed)
  tvctl window 1 --layout left    # snap to the left half of the screen
  tvctl screenshot 1 /tmp/now.png # see what the window shows
  tvctl auth login youtube        # one-time login (needed for Spotify, Netflix, Prime Video)";

const ID: &str = "Player id, from the output of `open` or `list`";

#[derive(Parser)]
#[command(version, about = ABOUT, long_about = LONG_ABOUT, after_long_help = AFTER_LONG_HELP)]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Open a URL in a new player window; prints the new player's id and status.
    ///
    /// The plugin is chosen by the URL's domain (see `tvctl plugins`). Starts
    /// the background server if it is not running.
    Open {
        /// Video or music URL of a supported service, e.g. a YouTube watch/youtu.be/shorts
        /// URL, an open.spotify.com track/album/playlist URL, a netflix.com
        /// /watch or /title URL, or an amazon.com / amazon.co.jp /gp/video/detail
        /// (Prime Video) URL.
        url: String,
        /// Use this backend instead of choosing automatically. `webview`: native
        /// window, no login. `chrome`: tvctl's Chrome profile (logins, DRM).
        #[arg(long, value_enum)]
        backend: Option<plugin::BackendKind>,
        /// Start position: seconds (90), clock (1:30, 1:02:03) or units (1m30s).
        /// Overrides `t=` in the URL.
        #[arg(long, value_name = "TIME")]
        start: Option<String>,
        /// Load without starting playback.
        #[arg(long)]
        paused: bool,
        /// Turn subtitles on in this language (e.g. ja, en); auto-translated if the
        /// video has no track in that language.
        #[arg(long, value_name = "LANG")]
        subtitles: Option<String>,
        #[command(flatten)]
        window: GeometryArgs,
        /// Start in fullscreen.
        #[arg(long)]
        fullscreen: bool,
        /// Keep the window above other windows (webview backend only).
        #[arg(long)]
        on_top: bool,
    },
    /// List open players with their id, service, backend, title and window.
    List,
    /// Show a player's playback and window status.
    Status {
        #[arg(help = ID)]
        id: String,
    },
    /// Start or resume playback.
    Play {
        #[arg(help = ID)]
        id: String,
    },
    /// Pause playback.
    Pause {
        #[arg(help = ID)]
        id: String,
    },
    /// Toggle between playing and paused.
    Toggle {
        #[arg(help = ID)]
        id: String,
    },
    /// Go to the next item (track, album/playlist entry).
    Next {
        #[arg(help = ID)]
        id: String,
    },
    /// Go to the previous item.
    Prev {
        #[arg(help = ID)]
        id: String,
    },
    /// Seek to an absolute position or by a relative offset.
    Seek {
        #[arg(help = ID)]
        id: String,
        /// Absolute: 90, 1:30, 1:02:03, 1m30s. Relative: +10, -30, -1m.
        #[arg(allow_hyphen_values = true)]
        position: String,
    },
    /// Skip forward.
    Forward {
        #[arg(help = ID)]
        id: String,
        /// How far: 10, 1:00, 2m.
        #[arg(default_value = "10")]
        duration: String,
    },
    /// Skip back.
    Back {
        #[arg(help = ID)]
        id: String,
        /// How far: 10, 1:00, 2m.
        #[arg(default_value = "10")]
        duration: String,
    },
    /// Show, choose or turn off subtitles.
    Subtitles {
        #[arg(help = ID)]
        id: String,
        /// `on`, `off`, `list` (adds `tracks` and `translation_languages` to the
        /// output), or a language code such as `ja` (auto-translated if needed).
        mode: String,
    },
    /// List or choose the audio track (e.g. dubbed languages).
    Audio {
        #[arg(help = ID)]
        id: String,
        /// `list` (adds `audio_tracks` to the output) or a language code such as `ja`.
        lang: String,
    },
    /// Set the volume.
    Volume {
        #[arg(help = ID)]
        id: String,
        /// 0-100.
        #[arg(value_parser = clap::value_parser!(u8).range(0..=100))]
        level: u8,
    },
    /// Mute audio.
    Mute {
        #[arg(help = ID)]
        id: String,
    },
    /// Unmute audio.
    Unmute {
        #[arg(help = ID)]
        id: String,
    },
    /// Set the playback speed.
    Speed {
        #[arg(help = ID)]
        id: String,
        /// Rate, e.g. 0.5, 1, 1.5, 2 (0.0625-16).
        rate: f64,
    },
    /// Request a playback quality.
    Quality {
        #[arg(help = ID)]
        id: String,
        /// A value from `available_qualities` in `status`, or `auto`. YouTube:
        /// small (240p), medium (360p), large (480p), hd720, hd1080, hd1440, hd2160.
        level: String,
    },
    /// Move or resize the window, snap it to a screen region, or change fullscreen
    /// and always-on-top.
    Window {
        #[arg(help = ID)]
        id: String,
        #[command(flatten)]
        geometry: GeometryArgs,
        /// Fullscreen state.
        #[arg(long, value_enum)]
        fullscreen: Option<Switch>,
        /// Keep the window above other windows (webview backend only).
        #[arg(long, value_enum)]
        on_top: Option<Switch>,
    },
    /// Turn fullscreen on, off, or toggle it.
    Fullscreen {
        #[arg(help = ID)]
        id: String,
        /// on, off or toggle.
        #[arg(value_enum, default_value = "toggle")]
        mode: Switch,
    },
    /// Save a PNG of what the player window shows (works when it is covered).
    Screenshot {
        #[arg(help = ID)]
        id: String,
        /// Output file (.png).
        path: std::path::PathBuf,
    },
    /// Close a player window.
    Close {
        #[arg(help = ID)]
        id: String,
    },
    /// Log in to a service in tvctl's Chrome profile.
    Auth {
        #[command(subcommand)]
        action: AuthCmd,
    },
    /// List plugins: domains, backends, supported actions, login settings.
    Plugins,
    /// Manage the background server (normally automatic).
    Server {
        #[command(subcommand)]
        action: ServerCmd,
    },
    #[command(name = "__serve", hide = true)]
    Serve {
        /// Do not stop when idle (started by `server up`).
        #[arg(long)]
        persistent: bool,
    },
    /// Evaluate JavaScript in a Chrome-backend player page (plugin development).
    #[command(name = "__eval", hide = true)]
    Eval { id: String, script: String },
}

#[derive(Subcommand)]
enum ServerCmd {
    /// Start the server and keep it running when no players are open (avoids
    /// restarting Chrome between uses).
    ///
    /// Without this, `open` and `auth` start it on demand and it stops a few
    /// seconds after the last player closes (`TVCTL_IDLE_SECS`).
    Up,
    /// Stop the server and close all players.
    Down,
    /// Show whether the server is running, its pid, players and mode.
    Status,
}

#[derive(Subcommand)]
enum AuthCmd {
    /// Open the service's login page in tvctl's Chrome profile and wait.
    ///
    /// Sign in, then quit that Chrome (Cmd+Q). Prints `{"service", "logged_in"}`
    /// (plus `sites` with each site's state for services with several sites).
    /// Needs a person at the screen. Logged in, YouTube plays in Chrome (no ads with
    /// Premium); Spotify, Netflix and Prime Video require a login.
    Login {
        /// Plugin name, e.g. youtube, spotify, netflix, primevideo.
        service: String,
        /// For services with several sites (`auth.sites` in `tvctl plugins`), the
        /// site to sign in to, e.g. `jp` for primevideo on amazon.co.jp. Default:
        /// the main site.
        #[arg(long)]
        site: Option<String>,
    },
    /// Show login state for one service or all services that support login.
    Status {
        /// Plugin name; all when omitted.
        service: Option<String>,
    },
}

#[derive(Args)]
struct GeometryArgs {
    /// Size of the video area in logical pixels, e.g. 1280x720.
    #[arg(long, value_parser = parse_pair::<'x'>)]
    size: Option<(f64, f64)>,
    /// Top-left corner of the window on screen, e.g. 100,50.
    #[arg(long, value_parser = parse_pair::<','>)]
    position: Option<(f64, f64)>,
    /// Snap to a screen region (fills it). With --size, the window keeps that size
    /// and is anchored to that side or corner.
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
    Serve(bool),
    ServerUp,
    ServerDown,
    ServerStatus,
    Plugins,
    Remote(Request),
}

fn plan(cmd: Cmd) -> Result<Plan> {
    let req = match cmd {
        Cmd::Serve { persistent } => return Ok(Plan::Serve(persistent)),
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
            AuthCmd::Login { service, site } => Request::AuthLogin { service, site },
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
        Plan::Serve(persistent) => {
            server::run(persistent)?;
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
            if let Request::AuthLogin { service, site } = &req {
                let service = match site {
                    Some(s) => format!("{service} ({s})"),
                    None => service.clone(),
                };
                eprintln!(
                    "Sign in to {service} in the Chrome window that opens, then quit that Chrome (Cmd+Q) \
                     (for some services, closing the login window is enough)."
                );
            }
            match req {
                Request::Open { .. } | Request::AuthLogin { .. } | Request::AuthStatus { .. } => {
                    client::send_autostart(&req)
                }
                Request::List => client::list(),
                _ => client::send(&req),
            }
        }
    }
}

fn main() {
    let cli = Cli::parse();
    let is_serve = matches!(cli.command, Cmd::Serve { .. });
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
        let mut argv = vec!["tvctl"];
        argv.extend_from_slice(args);
        let cli = Cli::try_parse_from(argv).unwrap_or_else(|e| panic!("{args:?}: {e}"));
        match plan(cli.command).unwrap_or_else(|e| panic!("{args:?}: {e}")) {
            Plan::Remote(r) => serde_json::to_value(r).unwrap(),
            other => panic!("{args:?}: expected a remote request, got {other:?}"),
        }
    }

    fn rejects(args: &[&str]) {
        let mut argv = vec!["tvctl"];
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
            (vec!["__serve"], "Serve(false)"),
            (vec!["__serve", "--persistent"], "Serve(true)"),
        ] {
            let mut argv = vec!["tvctl"];
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
        // Quality values are plugin-specific and validated by the plugin.
        assert_eq!(
            req(&["quality", "1", "very-high"]),
            action("1", json!({ "action": "quality", "level": "very-high" }))
        );
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
            json!({ "cmd": "auth_login", "service": "youtube", "site": null })
        );
        assert_eq!(
            req(&["auth", "login", "primevideo", "--site", "jp"]),
            json!({ "cmd": "auth_login", "service": "primevideo", "site": "jp" })
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

    /// Agents rely on `--help`: every visible command and argument is described.
    #[test]
    fn help_is_complete() {
        use clap::CommandFactory;
        fn check(cmd: &clap::Command, path: &str) {
            for sub in cmd
                .get_subcommands()
                .filter(|c| !c.is_hide_set() && c.get_name() != "help")
            {
                let name = format!("{path} {}", sub.get_name());
                assert!(sub.get_about().is_some(), "{name}: missing about");
                for arg in sub.get_arguments().filter(|a| !a.is_hide_set()) {
                    let id = arg.get_id().as_str();
                    if id == "help" || id == "version" {
                        continue;
                    }
                    assert!(
                        arg.get_help().is_some() || arg.get_long_help().is_some(),
                        "{name}: argument {id} has no help"
                    );
                }
                check(sub, &name);
            }
        }
        let cli = Cli::command();
        cli.clone().debug_assert();
        check(&cli, "tvctl");
        let long = cli.clone().render_long_help().to_string();
        for needle in [
            "Workflow:",
            "Output:",
            "Errors:",
            "Examples:",
            "tvctl plugins",
            "exit",
        ] {
            assert!(long.contains(needle), "long help lacks {needle:?}");
        }
    }
}
