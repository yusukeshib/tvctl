---
name: tvctl
description: Play and control YouTube videos and Spotify music in windows on the user's Mac with the tvctl CLI (open a URL, play/pause, seek, skip, subtitles incl. Japanese, audio track, volume, speed, quality, window size/position/layout/fullscreen, screenshots). Use when the user asks to play, watch, listen to, or control a video or song on their screen.
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
- Check the installed version with `tvctl --version`.
- Requirements: macOS. Google Chrome for logged-in playback and Spotify.

## 2. Basic use

```sh
tvctl open '<URL>'          # opens a window; prints {"id": "1", ...}
tvctl status <ID>           # state, title, current_time/duration (seconds), window
tvctl close <ID>
```

Use the `id` from `open` (or `tvctl list`) in later commands. No setup is needed:
a background server starts on demand and stops a few seconds after the last
player closes.

Supported URLs: YouTube (watch, youtu.be, shorts, live) and Spotify
(open.spotify.com tracks, albums, playlists). `tvctl plugins` lists services and
the actions each supports; unsupported actions fail with an error listing the
supported ones.

## 3. Commands

| Goal | Command |
|---|---|
| Play / pause / toggle | `tvctl play <ID>`, `tvctl pause <ID>`, `tvctl toggle <ID>` |
| Next / previous item | `tvctl next <ID>`, `tvctl prev <ID>` |
| Seek | `tvctl seek <ID> 1:30` (absolute), `tvctl seek <ID> +30` / `-10` (relative) |
| Skip | `tvctl forward <ID> 5m`, `tvctl back <ID>` (default 10 s) |
| Subtitles | `tvctl subtitles <ID> ja` (Japanese; auto-translated if needed), `on`, `off`, `list` |
| Audio track | `tvctl audio <ID> list`, `tvctl audio <ID> ja` |
| Volume | `tvctl volume <ID> 40`, `tvctl mute <ID>`, `tvctl unmute <ID>` |
| Speed | `tvctl speed <ID> 1.5` |
| Quality | `tvctl quality <ID> hd720` (values: `available_qualities` in status) |
| Window | `tvctl window <ID> --size 1280x720 --position 100,50` |
| Layout | `tvctl window <ID> --layout left` (left, right, top, bottom, top-left, top-right, bottom-left, bottom-right, center, maximize) |
| Small corner window | `tvctl window <ID> --layout bottom-right --size 480x270` |
| Fullscreen | `tvctl fullscreen <ID>` (toggle) or `on` / `off` |
| See the window | `tvctl screenshot <ID> /tmp/shot.png`, then view the PNG |

`open` accepts the same options up front, e.g.
`tvctl open '<URL>' --layout center --size 1280x720 --subtitles ja --start 1:00 --paused`.
`--size` is the video area in logical pixels; a layout without `--size` fills
that screen region.

## 4. Tips

- Quote URLs (`'...'`): `?` and `&` break in shells such as zsh.
- After seeking, `state` may briefly be `buffering`; check `status` again.
- Japanese subtitles: if a video has no Japanese track, tvctl uses YouTube's
  auto-translation (`translated_to: "ja"` in the output). If `subtitles list`
  shows no tracks, the video has no subtitles.
- To verify what is on screen, take a screenshot rather than assuming.
- If the user closes the window, the player id becomes invalid (`no player`).

## 5. Logins (needs the user)

YouTube works without a login (ads may play). Spotify requires a login, and a
YouTube Premium login removes ads:

```sh
tvctl auth status
tvctl auth login youtube    # or spotify
```

`auth login` opens Chrome; the user must sign in and then quit that Chrome with
Cmd+Q. Ask the user to do this; never handle their credentials.

## 6. Troubleshooting

- `error: ... server is not running` for a player command: the player was closed;
  open it again.
- Unexpected behavior: run `cargo install tvctl` to update, then retry.
- Server log: `$TMPDIR/tvctl.log`.
