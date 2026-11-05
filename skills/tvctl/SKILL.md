---
name: tvctl
description: Play and control videos and music from streaming services (YouTube, Netflix, Spotify and more; `tvctl --help` lists them) in windows on the user's Mac with the tvctl CLI (open a URL, play/pause, seek, skip, next episode, subtitles incl. Japanese, audio track, volume, speed, quality, window size/position/layout/fullscreen, screenshots, lowering players while other audio such as a voice assistant plays). Use when the user asks to play, watch, listen to, or control a video, show, movie or song on their screen.
---

# tvctl

`tvctl` plays videos and music in windows on this computer and controls them from
the command line. It needs a local macOS machine with a screen; it does not work
in remote sandboxes. Every command prints JSON to stdout; on failure it prints
`error: ...` to stderr and exits 1.

## 1. Install or update first (every time)

Before using tvctl in a session, make sure the latest version is installed:

```sh
cargo install tvctl
```

- This installs tvctl, or upgrades it when crates.io has a newer version (it is a
  no-op when already up to date). Do this at the start of every session that uses
  tvctl, and tell the user if it upgraded.
- Needs Rust (`cargo`). If `cargo` is missing, ask the user before installing
  Rust (https://rustup.rs).
- Requirements: macOS; Google Chrome for services that need a login or DRM.

## 2. Learn the services from tvctl itself

The supported services change between versions, so do not rely on memory:

```sh
tvctl --help                # workflow, output format, and the installed services
tvctl plugins <name>        # one service: URL domains and examples, supported
                            # commands, login, and notes on its limits and values
```

Before using a service in a session, read its `tvctl plugins <name>` output,
especially `notes` (e.g. how to find a title's URL, accepted speed/quality values,
ads, regional accounts) and `login` (whether a login is required and the exact
`tvctl auth login ...` commands). A URL is handled by the service whose `domains`
match its host. Commands a service does not support fail with an error listing
the supported ones.

## 3. Basic use

```sh
tvctl open '<URL>'          # opens a window; prints {"id": "1", ...}
tvctl status <ID>           # state, title, current_time/duration (seconds), window
tvctl close <ID>
```

Use the `id` from `open` (or `tvctl list`) in later commands. No setup is needed:
a background server starts on demand and stops a few seconds after the last
player closes. Quote URLs (`'...'`): `?` and `&` break in shells such as zsh.

| Goal | Command |
|---|---|
| Play / pause / toggle | `tvctl play <ID>`, `tvctl pause <ID>`, `tvctl toggle <ID>` |
| Next / previous item | `tvctl next <ID>`, `tvctl prev <ID>` |
| Seek | `tvctl seek <ID> 1:30` (absolute), `tvctl seek <ID> +30` / `-10` (relative) |
| Skip | `tvctl forward <ID> 5m`, `tvctl back <ID>` (default 10 s) |
| Subtitles | `tvctl subtitles <ID> ja`, `on`, `off`, `list` |
| Audio track | `tvctl audio <ID> list`, `tvctl audio <ID> ja` |
| Volume | `tvctl volume <ID> 40`, `tvctl mute <ID>`, `tvctl unmute <ID>` |
| Lower players while other audio plays | `tvctl duck on` (all other apps) or `tvctl duck on --apps ChatGPT --level 20`; `tvctl duck off`, `tvctl duck status`, `tvctl duck apps` (what plays now) |
| Speed | `tvctl speed <ID> 1.5` |
| Quality | `tvctl quality <ID> <value>` (values: `available_qualities` in status) |
| Window | `tvctl window <ID> --size 1280x720 --position 100,50` |
| Layout | `tvctl window <ID> --layout left` (left, right, top, bottom, top-left, top-right, bottom-left, bottom-right, center, maximize) |
| Small corner window | `tvctl window <ID> --layout bottom-right --size 480x270` |
| Fullscreen | `tvctl fullscreen <ID>` (toggle) or `on` / `off` |
| See the window | `tvctl screenshot <ID> /tmp/shot.png`, then view the PNG |

`open` accepts the same options up front, e.g.
`tvctl open '<URL>' --layout center --size 1280x720 --subtitles ja --start 1:00 --paused`.
`--size` is the video area in logical pixels; a layout without `--size` fills
that screen region. `tvctl <command> --help` describes every argument.

## 4. Reading results

- Read the JSON after every command; `player` holds the service's state.
- A `warning` field (top level or in `player`) explains something to act on or
  tell the user. A `pending` field lists settings applied later (e.g. after an ad).
- After seeking, `state` may briefly be `buffering`; check `status` again.
- To verify what is on screen, take a screenshot rather than assuming.
  Protected (DRM) video can look black or white in screenshots and through screen
  sharing while it plays normally on the Mac's own display.
- After 60 seconds paused or ended, the window turns black (`window.blackout`
  is true; screenshots are black). Playing, any player command except `status`, or the user
  moving the mouse over the window brings the picture back.
- If the user closes the window, the player id becomes invalid (`no player`).

## 5. Logins (needs the user)

```sh
tvctl auth status           # login state per service (and per site)
```

If a service needs a login (`login.required` in `tvctl plugins <name>`, or an
error saying it is not signed in), ask the user to run the exact command from
`login.commands`. `auth login` opens Chrome; the user signs in and then quits that
Chrome with Cmd+Q. Never handle their credentials. Logins belong to tvctl's own
Chrome profile, not the user's everyday browser.

## 6. Troubleshooting

- Follow the error message: tvctl errors say what to do (e.g. which login to run).
- `error: ... server is not running` for a player command: the player was closed;
  open it again.
- A `warning` about an SSH session (or `auth login` refused for that reason):
  the tvctl server was started over SSH, where Chrome cannot use the macOS
  keychain, so logins are not saved or loaded. Run `tvctl server down` and ask
  the user to run tvctl from a terminal on the Mac's own screen.
- `tvctl duck status` shows `"monitoring": true` but players are never lowered
  (`peak_db` stays null while other audio plays): macOS has not granted system
  audio recording. Ask the user to allow the app that runs tvctl (e.g. the
  terminal) under System Settings > Privacy & Security > Screen & System Audio
  Recording.
- Unexpected behavior: run `cargo install tvctl` to update, then retry.
- Server log: `$TMPDIR/tvctl.log`.
