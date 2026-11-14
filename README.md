# tvctl

A Rust CLI that lets agents (or people) play videos and music in a window on this
computer and control it from the command line, like a TV remote for your
screen. (It does not control physical TVs.) Commands print JSON to stdout
(except `config`, which prints shell code); errors go to stderr with exit code 1.

Services are plugins (`tvctl plugins` lists them). Currently: YouTube,
Spotify (web player), Netflix, Amazon Prime Video (amazon.com and amazon.co.jp)
and Crunchyroll.
macOS only for now.

## Install

```sh
cargo install tvctl        # or, from a checkout: cargo install --path .
```

## Shell integration

Add this to `~/.zshrc`, after `compinit`:

```sh
# If your shell setup does not already initialize completion:
# autoload -Uz compinit && compinit
eval "$(tvctl config zsh)"
```

For Bash, add this to `~/.bashrc` (no bash-completion dependency):

```sh
eval "$(tvctl config bash)"
```

Tab completes commands, options, enum values (layouts, on/off/toggle),
file paths, installed service names, and open and installed service names. Completion never
contacts or starts the server. The completion code is generated
on shell startup so it stays in sync after tvctl upgrades.

## ChatGPT / Codex plugin and agent skill

`skills/tvctl/SKILL.md` teaches an agent to install or update tvctl
(`cargo install tvctl` at the start of every session) and to use it.
`scripts/package-skill.sh` builds:

- `dist/tvctl-plugin.zip`: an [Agent Plugins 1.0](https://agent-plugins.org)
  package (`plugin.json` at the root, `skills/tvctl/SKILL.md`) for ChatGPT and
  Codex. To install it, give the zip to ChatGPT and ask it to install the plugin.
  To publish it, upload it on the Plugins page of the OpenAI Platform dashboard.
- `dist/tvctl-skill.zip`: the bare skill, for "Upload a skill" in ChatGPT or
  Claude, or copy `skills/tvctl` into any agent's skills directory.

tvctl opens windows on your Mac, so the agent must be able to run commands on
that Mac (for example the ChatGPT desktop app, Codex CLI, Claude Code, pi);
cloud sandboxes cannot show anything on your screen.

## Usage

```sh
tvctl server up                      # start the background server once
tvctl open https://youtu.be/jNQXAC9IVRw   # fullscreen by default
tvctl open https://youtu.be/jNQXAC9IVRw --windowed
tvctl open https://youtu.be/jNQXAC9IVRw --layout bottom-right --size 640x360
# => {"service": "youtube", "player": {...}, "window": {...}}

tvctl status
tvctl play / tvctl pause / tvctl toggle
tvctl next / tvctl prev              # next/previous track or playlist entry
tvctl seek 1:30                      # absolute (90, 1:30, 1m30s)
tvctl seek -10                       # relative (+10, -10)
tvctl forward 30 / tvctl back        # default 10 s
tvctl subtitles ja                   # Japanese; auto-translated if there is no Japanese track
tvctl subtitles list | on | off
tvctl audio list | ja                # audio track (e.g. dubbing), where supported
tvctl quality hd1080                 # values: `available_qualities` in status
tvctl volume 50 / tvctl mute / tvctl unmute
tvctl speed 1.5
tvctl window --layout left           # left/right/top/bottom/top-left/.../center/maximize
tvctl window --layout bottom-right --size 480x270   # anchored to a corner
tvctl window --size 1280x720 --position 100,50
tvctl fullscreen                     # toggle (or on/off)
tvctl screenshot shot.png            # PNG of the player window, even when covered
tvctl close

tvctl search youtube 日本 ニュース ライブ          # => {"results": [{"kind", "title", "url", ...}]}
tvctl search spotify 宇多田ヒカル --kind track --limit 5
tvctl search netflix stranger things
tvctl search primevideo 鬼滅の刃 --site jp     # amazon.co.jp; amazon.com without --site
tvctl search crunchyroll frieren

tvctl auth login youtube             # sign in once in tvctl's Chrome profile
tvctl auth login spotify
tvctl auth login primevideo             # amazon.com (US)
tvctl auth login primevideo --site jp   # amazon.co.jp (a separate account)
tvctl auth login netflix
tvctl auth login crunchyroll
tvctl auth status
tvctl plugins                        # services, domains, supported actions
tvctl plugins netflix                # one service: URL examples, login, notes

tvctl duck on                        # lower the player to 10% while other audio plays
tvctl duck on --apps ChatGPT --level 20   # only while ChatGPT plays, to 20%
tvctl duck status / tvctl duck apps / tvctl duck off
```

There is one player window. `open` opens it if there is none; otherwise the URL
replaces what it shows. `open` makes the window fullscreen unless `--windowed`,
`--size`, `--position` or `--layout` is given; leaving fullscreen returns it to
its place and size. Player commands take no id: they act on that
window. With no window open they fail (`no player is open`). After `close`, the
next window opens where the last one was.

`search` runs in a background tab of tvctl's Chrome (with its logins) and does
not touch the player; open a result with `tvctl open <url>`. `tvctl plugins`
shows which services have search and their `--kind` and `--site` values.
Netflix search picks the first non-Kids profile when Netflix asks who is watching.

Player, `search`, `auth` and `cast code` commands need the background server. Start it
with `tvctl server up`; it keeps running, with or without a player, until
`tvctl server down`, which closes the player and quits tvctl's Chrome.
tvctl's Chrome also quits (and leaves the Dock) a minute after the player
window closes; the next `open` starts it again.
Nothing else starts or stops it: without a server those commands fail with
an error telling you to run `tvctl server up`.
`tvctl server status` shows its state and what the player shows.

`--size` is the video area; layouts without `--size` fill their screen region.
`tvctl --help` describes the workflow, the JSON output and error format, and
examples; `tvctl <command> --help` describes each argument.

## Ducking

`tvctl duck on` lowers the player while other audio plays (for example a
voice assistant speaking) and restores it when that audio has been silent for
a moment. "Other audio" is every app except tvctl's own player, or only the
apps given with `--apps` (app names such as `ChatGPT`, process names, or bundle
ids; `tvctl duck apps` lists what is playing now). `--level` is the volume
while lowered, in percent of the player's own volume (default 10). The
setting is saved (`duck.json` in the data directory) and applies whenever
the player is open; `tvctl duck off` turns it off.

- Nothing is rerouted: tvctl measures the other apps' audio with a Core Audio
  process tap (macOS 14.2+), which only copies it. The first time, macOS asks
  for permission to record system audio, on behalf of the app that started
  tvctl (e.g. the terminal); allow it under System Settings > Privacy &
  Security > Screen & System Audio Recording.
- The volume is lowered inside the player page, so it works for every
  service, and `volume`/`status` keep showing the player's own volume.
  (Sound a page plays through Web Audio instead of media elements is not
  lowered.)
- `tvctl duck status` shows `ducking` (lowered now), `peak_db` (level of the
  monitored audio), `sources` (monitored apps playing now) and `error`.

## Casting from the YouTube app

`tvctl cast on` makes this Mac a YouTube cast target: phones and computers on
the same network list it (as `tvctl (<computer name>)`, or `--name`) in the YouTube app's Cast
menu. A cast video plays in the player window (replacing what it shows, and
logged in, Premium plays without ads); play, pause, seek, volume and the queue
(next/previous, the next video when one ends) are controlled from the app, and
the app shows the player's state. Player commands from the command line show in
the app too: `pause`, `seek` or `volume` change what the app shows, and `next` /
`prev` move through the cast queue. A new cast video replaces the previous one in
the same window; tapping Disconnect in the app closes it, while swiping the
app's player down to search for the next video does not stop it. Opening something else
with `tvctl open` takes the window over: the app shows the cast as stopped, and
Disconnect then leaves the window alone.

```sh
tvctl cast on               # saved; the receiver runs while the server runs
tvctl cast on --name "Living room"
tvctl cast status           # running, connected devices (senders), in_player, video
tvctl cast code             # code for the app's "Link with TV code"
tvctl cast off
```

- The receiver runs in the tvctl server; `tvctl cast on` does not start the
  server. After a reboot or `tvctl server down` it runs again as soon as
  `tvctl server up` starts the server. With casting on, `server up` fails (and
  stops the server it started) when the receiver cannot start.
- Discovery uses DIAL (SSDP on UDP port 1900 and an HTTP port); macOS may ask
  to allow incoming connections, and iOS asks the YouTube app for local network
  access. If the Mac does not appear, use `tvctl cast code` in the app under
  Settings > Watch on TV > Link with TV code.
- YouTube only (YouTube Music casting is refused). Autoplay of recommended
  videos after the queue ends is not supported.
- It uses YouTube's unpublished TV (Lounge) API, as YouTube TV apps do, so
  YouTube changes can break it. The protocol follows
  [yt-cast-receiver](https://github.com/patrickkfkan/yt-cast-receiver) (MIT).
- Settings are in `cast.json` and the receiver's ids in `cast-state.json` in
  the data directory.

## Blackout

In fullscreen, the mouse cursor hides after 10 seconds without movement.
Moving the mouse shows it again; leaving fullscreen restores the normal cursor.

A player that has been paused or has ended for 60 seconds turns black (the
whole window content). Moving the mouse over the window, clicking, scrolling or
typing in it brings the picture back without passing that input to the player,
as do playback resuming and any tvctl player command except `status`; if it stays paused, it
turns black again after another 60 seconds. `window.blackout` in the output
shows whether it is black now (a `screenshot` is black then too).
`TVCTL_BLACKOUT_SECS` sets the wait when the server starts (0 turns it off).

## Chrome

The player is an app window of Google Chrome, driven through the Chrome
DevTools Protocol, using a dedicated profile at
`~/Library/Application Support/tvctl/chrome-profile` (logins persist and DRM
works). Chrome refuses DevTools control of your everyday profile, so tvctl has
its own; sign in to each service once with `tvctl auth login`. The server
itself is a plain background process (no Dock icon or window).

Run `tvctl server up` from a terminal on the Mac's own screen. Chrome keeps the
key for its saved cookies in the macOS login keychain; a Chrome that cannot use
it (for example one started over SSH) loses tvctl's logins. So `server up`
fails, without starting a server, when it cannot use the login keychain.

`auth login` opens the login page in a plain Chrome (not controlled, because
Google refuses sign-in in automated browsers) on tvctl's profile. Sign in, then
quit that Chrome with Cmd+Q; tvctl then checks the login cookies.

## Services

| Plugin | Login | Supported actions |
|---|---|---|
| `youtube` | optional (Premium: no ads) | all |
| `spotify` | required | play, pause, toggle, next, previous, seek, volume, mute |
| `netflix` | required | all except previous, quality |
| `primevideo` | required (amazon.com and/or amazon.co.jp account with Prime Video) | all except previous |
| `crunchyroll` | required (most titles need Premium) | all except previous |

Spotify links: tracks, albums, playlists, artists and episodes on
`open.spotify.com`. A track link plays that track; other pages start from the top.
Spotify opens in its large cover-art view (shown inside the window, so window
layouts keep working). The plugin drives the web player's UI (no public API), so Spotify UI
changes can break it; the E2E tests detect that.

Netflix links: `netflix.com/watch/<id>` (and `?t=<seconds>`) and
`netflix.com/title/<id>` (plays like the page's Play button: a show resumes its
current episode). Status includes `episode` {title, number, season} and
`next_episode`; `next` switches to it in place. Subtitles prefer full subtitles
over closed captions (`type`: `PRIMARY` / `ASSISTIVE`), audio prefers regular
tracks over audio description; exact track ids from `list` also work. Speeds are
Netflix's menu rates (0.5 to 1.5). Netflix has no quality setting on the web.
Ads (plans with ads) are not skipped and are handled like Prime Video's
(`ad_showing`, refused actions, deferred `--start`/`--subtitles`). The plugin
uses Netflix's internal player API (seeking through it; setting the video
element's time directly breaks playback with error M7375).

Prime Video links: `amazon.com/gp/video/detail/<id>` and `amazon.com/dp/<ASIN>`
(US), `amazon.co.jp/gp/video/detail/<id>` (Japan, also with a language prefix
such as `/-/en/`), and `primevideo.com/detail/<id>` (opened on amazon.com).
The US and Japanese stores are separate accounts; sign in to each you use
(`auth login primevideo` and `auth login primevideo --site jp`); `auth status`
shows both under `sites`. A series page plays the episode its Play button would (usually the
next one to watch); a title resumes where it was left unless `--start` is given.
`next` starts the next episode (`next_episode` in status). Speeds are the
player's fixed rates (0.75 to 2); qualities are its data-usage settings `good`,
`better`, `best`. Ads are not skipped: while one plays, `ad_showing` is true,
`ad_remaining` gives the seconds left, seeking/subtitles/audio/speed are
refused, and `--start`/`--subtitles` of `open` are applied when it ends
(`pending`). Amazon limits simultaneous streams per account; a window that
cannot play reports the player's message (e.g. "Maximum Streams Playing") as an
error. The plugin uses the web player's internal stores (no public API), so
Prime Video changes can break it; the E2E tests detect that.

Crunchyroll links: `crunchyroll.com/watch/<id>/<slug>` (an episode or movie)
and `crunchyroll.com/series/<id>/<slug>` (plays like the series page's
Start/Continue button). An episode resumes where it was left unless `--start`
is given. The player fills the window; the rest of the watch page is hidden.
Status includes `episode` {id, title, number} and `next_episode`; `next`
switches to it in place. Subtitles are burned into the video (switching reloads
the stream for a moment), and each dub is its own episode id, so `audio <lang>`
changes `video_id` and the URL. Subtitle and audio choices become the profile's
preferences, as in the site's menus. Speeds are the menu's 0.5, 0.75 and 1;
qualities are `highest`, `moderate` (720p) and `data_saver` (480p). Tested with
Premium; on ad-supported plans `ad_showing` reports ads (untested). Crunchyroll's
Cloudflare bot check ("Verify you are human") can block tvctl's Chrome, mostly
after many page loads in a short time; `open` then says so. Pass the check in
the plain Chrome of `tvctl auth login crunchyroll` (open any episode there),
quit it, and retry. The plugin drives the web player's internal player object
(reached through its React controls), so site changes can break it; the E2E
tests detect that.

## Plugins

Each service is a directory `plugins/<name>/`:

- `plugin.toml`:
  - `name`, `description`, `domains` (subdomains match too)
  - `examples`: example URLs (each must be on one of the `domains`) and `notes`:
    what agents and users need to know (how to find URLs, limits, accepted
    values, ads, login hints). `tvctl --help` lists every installed service with
    its first example, and `tvctl plugins <name>` shows the rest, so agent
    instructions (`skills/tvctl/SKILL.md`) need no per-service text.
  - `actions`: supported player actions. The server refuses other actions with
    an error listing the supported ones, and `tvctl plugins` shows them, so
    agents can check before calling.
  - optional `[auth]`: `required` (playback needs a login), `login_url`,
    `cookie_url`, `logged_in_cookies`,
    `login_mode` (`plain` by default, or `controlled`).
    Services with several separately signed-in sites (e.g. regional stores)
    name the main site with `site` and add others as
    `[auth.sites.<name>]` with their own `login_url`, `cookie_url` and
    `logged_in_cookies`; `auth login <service> --site <name>` signs in to one,
    and the service counts as logged in when any site is.
- `chrome.js`: the adapter. It is loaded into every document of the player
  window while that plugin's content is shown, so it can restore its page state
  after reloads.

Adapters define `window.__tvctl.run(action)`, which returns a status object.
Actions: `open {url, start, paused, subtitles}`, `status`, `play`, `pause`,
`toggle`, `next`, `previous`, `seek {seconds, relative}`, `subtitles {lang}`, `audio {lang}`,
`volume {level, muted}`, `speed {rate}`, `quality {level}`. Unsupported actions
throw. `open` may return `{"navigate": url}` to be loaded
at another URL first.

Built-in plugins are compiled into the binary. Plugins in `$TVCTL_PLUGIN_DIR`
or `~/.config/tvctl/plugins/<name>/` are loaded when the server starts and
override built-ins with the same name.

## Environment

- `TVCTL_SOCKET`: server socket (default `$TMPDIR/tvctl.sock`; the log is
  next to it as `tvctl.log`)
- `TVCTL_DATA_DIR`: data directory (Chrome profile, cast and duck settings)
- `TVCTL_PLUGIN_DIR`: external plugin directory
- `TVCTL_CHROME`: Chrome executable
- `TVCTL_BLACKOUT_SECS`: how long a player stays paused or ended before it
  turns black (default 60; 0 turns blackout off)
- `TVCTL_CAST_TRACE`: when set for `server up`, the server log records the
  cast traffic (messages to and from the YouTube app, DIAL and SSDP requests)

For plugin development, the hidden command `tvctl __eval <JS>` evaluates
JavaScript in the player page and prints the (awaited) result.

## Tests

```sh
node --test tests/cursor.test.cjs                           # cursor idle timer tests
cargo test                                                   # unit tests
cargo test --test e2e_fake -- --ignored --test-threads=1     # all commands, offline
cargo test --test e2e_youtube -- --ignored --test-threads=1  # real YouTube (network)
TVCTL_E2E_PROFILE="$HOME/Library/Application Support/tvctl" \
  cargo test --test e2e_spotify -- --ignored --test-threads=1  # real Spotify (needs login)
TVCTL_E2E_PROFILE="$HOME/Library/Application Support/tvctl" \
  cargo test --test e2e_primevideo -- --ignored --test-threads=1  # real Prime Video (needs login)
TVCTL_E2E_PROFILE="$HOME/Library/Application Support/tvctl" \
  cargo test --test e2e_netflix -- --ignored --test-threads=1  # real Netflix (needs login)
TVCTL_E2E_PROFILE="$HOME/Library/Application Support/tvctl" \
  cargo test --test e2e_crunchyroll -- --ignored --test-threads=1  # real Crunchyroll (needs login)
```

The end-to-end tests open windows. `e2e_fake` uses a fake plugin and a local site.

## Releasing

Bump `version` in `Cargo.toml` and `plugin/plugin.json` (they must match) and
push to `main`. `.github/workflows/release.yml` then publishes the crate to
crates.io, tags `v<version>` and creates a GitHub release with
`dist/tvctl-plugin.zip` and `dist/tvctl-skill.zip`. Versions that already have
a tag are skipped; the workflow can also be started by hand (`workflow_dispatch`).
It needs the `CARGO_REGISTRY_TOKEN` repository secret.
`e2e_youtube` uses a fresh logged-out profile, so ads may play and the tests
wait for them. To use your logged-in profile instead, set
`TVCTL_E2E_PROFILE="$HOME/Library/Application Support/tvctl"`.
