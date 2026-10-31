# mediactl

A Rust CLI that lets agents (or people) play videos in windows and control them
from the command line. Every command prints JSON to stdout; errors go to stderr
with exit code 1.

Services are plugins (`mediactl plugins` lists them). Currently: YouTube and
Spotify (web player).
macOS only for now.

## Install

```sh
cargo install --path .
```

## Usage

```sh
mediactl open https://youtu.be/jNQXAC9IVRw --layout bottom-right --size 640x360
# => {"id": "1", "service": "youtube", "backend": "webview", "player": {...}, "window": {...}}

mediactl status 1
mediactl play 1 / mediactl pause 1 / mediactl toggle 1
mediactl next 1 / mediactl prev 1        # next/previous track or playlist entry
mediactl seek 1 1:30                   # absolute (90, 1:30, 1m30s)
mediactl seek 1 -10                    # relative (+10, -10)
mediactl forward 1 30 / mediactl back 1   # default 10 s
mediactl subtitles 1 ja                # Japanese; auto-translated if there is no Japanese track
mediactl subtitles 1 list | on | off
mediactl audio 1 list | ja             # audio track (e.g. dubbing), where supported
mediactl quality 1 hd1080              # auto, small, medium, large, hd720, hd1080, ...
mediactl volume 1 50 / mediactl mute 1 / mediactl unmute 1
mediactl speed 1 1.5
mediactl window 1 --layout left        # left/right/top/bottom/top-left/.../center/maximize
mediactl window 1 --layout bottom-right --size 480x270   # anchored to a corner
mediactl window 1 --size 1280x720 --position 100,50 --on-top on
mediactl fullscreen 1                  # toggle (or on/off)
mediactl screenshot 1 shot.png         # PNG of the player window, even when covered
mediactl list
mediactl close 1

mediactl auth login youtube            # sign in once in mediactl's Chrome profile
mediactl auth login spotify
mediactl auth status
mediactl plugins
```

The background server starts on demand (`open`, `auth`) and stops a few seconds
after the last player closes (`MEDIACTL_IDLE_SECS`, default 5), quitting
mediactl's Chrome too. `mediactl server up` keeps it running without players
(avoids restarting Chrome between uses); `mediactl server down` stops it and
closes all players; `mediactl server status` shows its state.

`--size` is the video area; layouts without `--size` fill their screen region.
See `mediactl --help` and `mediactl <command> --help` for details.

## Backends

- **webview**: a native window with a WebView (tao + wry). No dependencies. Used
  for services with public embed APIs (YouTube logged out).
- **chrome**: an app window of Google Chrome, driven through the Chrome DevTools
  Protocol, using a dedicated profile at
  `~/Library/Application Support/mediactl/chrome-profile`. Used where a login or
  DRM is needed. Chrome refuses DevTools control of your everyday profile, so
  mediactl has its own; sign in to each service once with `mediactl auth login`.
  Always-on-top is not available in this backend.

A plugin lists its backends in order of preference. If a plugin declares
`prefer_backend_when_logged_in` and you are logged in, that backend is used
(YouTube: Chrome, so Premium accounts get no ads). `open --backend` overrides.

`auth login` opens the login page in a plain Chrome (not controlled, because
Google refuses sign-in in automated browsers) on mediactl's profile. Sign in, then
quit that Chrome with Cmd+Q; mediactl then checks the login cookies.

## Services

| Plugin | Backends | Login | Supported actions |
|---|---|---|---|
| `youtube` | webview (logged out), chrome (logged in) | optional (Premium: no ads) | all; `audio` only in chrome |
| `spotify` | chrome | required | play, pause, toggle, next, previous, seek, volume, mute |

Spotify links: tracks, albums, playlists, artists and episodes on
`open.spotify.com`. A track link plays that track; other pages start from the top.
Spotify opens in its large cover-art view (shown inside the window, so window
layouts keep working). The plugin drives the web player's UI (no public API), so Spotify UI
changes can break it; the E2E tests detect that.

## Plugins

Each service is a directory `plugins/<name>/`:

- `plugin.toml`:
  - `name`, `description`, `domains` (subdomains match too), `backends`
  - optional `[auth]`: `login_url`, `cookie_url`, `logged_in_cookies`,
    `prefer_backend_when_logged_in`, `login_mode` (`plain` by default, or `controlled`)
- `webview.html`: the page for the WebView backend.
- `frame.js` (optional): injected into that WebView's subframes.
- `chrome.js`: the adapter for the Chrome backend. It is loaded into every
  document of the player window, so it can restore its page state after reloads.

Adapters define `window.__mediactl.run(action)`, which returns a status object.
Actions: `open {url, start, paused, subtitles}`, `status`, `play`, `pause`,
`toggle`, `next`, `previous`, `seek {seconds, relative}`, `subtitles {lang}`, `audio {lang}`,
`volume {level, muted}`, `speed {rate}`, `quality {level}`. Unsupported actions
throw. In the Chrome backend, `open` may return `{"navigate": url}` to be loaded
at another URL first.

Built-in plugins are compiled into the binary. Plugins in `$MEDIACTL_PLUGIN_DIR`
or `~/.config/mediactl/plugins/<name>/` are loaded when the server starts and
override built-ins with the same name.

## Environment

- `MEDIACTL_SOCKET`: server socket (default `$TMPDIR/mediactl.sock`; the log is
  next to it as `mediactl.log`)
- `MEDIACTL_DATA_DIR`: data directory (Chrome profile, login state)
- `MEDIACTL_PLUGIN_DIR`: external plugin directory
- `MEDIACTL_CHROME`: Chrome executable
- `MEDIACTL_IDLE_SECS`: idle time before an on-demand server stops (default 5)

For plugin development, the hidden command `mediactl __eval <ID> <JS>` evaluates
JavaScript in a Chrome-backend player page and prints the (awaited) result.

## Tests

```sh
cargo test                                                   # unit tests
cargo test --test e2e_fake -- --ignored --test-threads=1     # all commands, both backends, offline
cargo test --test e2e_youtube -- --ignored --test-threads=1  # real YouTube (network)
MEDIACTL_E2E_PROFILE="$HOME/Library/Application Support/mediactl" \
  cargo test --test e2e_spotify -- --ignored --test-threads=1  # real Spotify (needs login)
```

The end-to-end tests open windows. `e2e_fake` uses a fake plugin and a local site.
`e2e_youtube` uses a fresh logged-out profile, so ads may play and the tests
wait for them. To use your logged-in profile instead, set
`MEDIACTL_E2E_PROFILE="$HOME/Library/Application Support/mediactl"`.
