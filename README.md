# play

A Rust CLI that lets agents (or people) play YouTube videos in native windows and
control them from the command line. Every command prints JSON to stdout; errors
go to stderr with exit code 1.

macOS only for now. Videos play through the YouTube embedded player, so videos
whose owners disable embedding cannot be played (`open` returns the error).

## Install

```sh
cargo install --path .
```

## Usage

```sh
play server up                       # start the background server
play open https://youtu.be/jNQXAC9IVRw --layout bottom-right --size 640x360
# => {"id": "1", "player": {...}, "window": {...}}

play status 1
play pause 1 / play play 1 / play toggle 1
play seek 1 1:30        # absolute (90, 1:30, 1m30s)
play seek 1 -10         # relative (+10, -10)
play forward 1 30       # default 10 s
play back 1
play subtitles 1 ja     # Japanese; auto-translated if no Japanese track
play subtitles 1 list | on | off
play quality 1 hd1080   # auto, small, medium, large, hd720, hd1080, ...
play volume 1 50 / play mute 1 / play unmute 1
play speed 1 1.5
play window 1 --layout left          # left/right/top/bottom/top-left/.../center/maximize
play window 1 --size 1280x720 --position 100,50 --on-top on
play fullscreen 1                    # toggle (or on/off)
play list
play close 1
play server down
```

See `play --help` and `play <command> --help` for details.

## Design

- `play server up` spawns `play __serve` in the background. The server runs the
  GUI event loop (tao + wry). It listens on a Unix socket (`$TMPDIR/play.sock`,
  or `PLAY_SOCKET` if set) and writes logs to `play.log` next to the socket.
- Each player is a window with a WebView. The WebView loads `assets/player.html`
  from a loopback HTTP server so that the YouTube embed gets a real origin, and
  drives it through the YouTube IFrame API.
- `assets/embed.js` is injected into the embed iframe to reach internal player
  functions that the IFrame API no longer exposes, such as quality selection.
- Caption tracks only become available once playback starts. A subtitle request
  made before that is applied when playback begins (status shows `pending`).
