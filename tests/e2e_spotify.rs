//! End-to-end tests against the real Spotify Web Player. Spotify needs a logged-in
//! account, so these run only with a tvctl data dir whose Chrome profile is
//! signed in (`tvctl auth login spotify`):
//!
//!     TVCTL_E2E_PROFILE="$HOME/Library/Application Support/tvctl" \
//!         cargo test --test e2e_spotify -- --ignored --test-threads=1
//!
//! The tests change playback and volume of that account's web player.

mod common;

use std::time::Duration;

use common::{Env, eventually, f, is_png};
use serde_json::Value;

/// Rick Astley, "Whenever You Need Somebody" (10 tracks).
const ALBUM: &str = "https://open.spotify.com/album/5Z9iiGl2FcIfa3BMiv6OIw";
/// "Never Gonna Give You Up - 2022 Remaster" (3:33).
const TRACK: &str = "https://open.spotify.com/track/4cOdK2wGLETKBW3PvgPWqT";

fn p(v: &Value, key: &str) -> Value {
    v["player"][key].clone()
}

fn env() -> Option<Env> {
    if std::env::var_os("TVCTL_E2E_PROFILE").is_none() {
        eprintln!("skipped: set TVCTL_E2E_PROFILE to a logged-in tvctl data dir");
        return None;
    }
    let env = Env::with_profile();
    env.up();
    Some(env)
}

#[test]
#[ignore = "needs a Spotify login, network and windows; run with --ignored"]
fn spotify_album_controls() {
    let Some(env) = env() else { return };
    let v = env.ok(&["open", ALBUM, "--layout", "right"]);
    assert_eq!(v["service"], "spotify");
    assert_eq!(p(&v, "logged_in"), true);
    assert_eq!(p(&v, "state"), "playing");
    // The large cover-art view is shown inside the window, not as OS fullscreen.
    assert_eq!(p(&v, "cover_view"), true);
    assert_eq!(v["window"]["fullscreen"], false);
    let first = p(&v, "title");
    assert!(first.as_str().is_some_and(|t| !t.is_empty()), "{v}");

    // play / pause / toggle
    assert_eq!(p(&env.ok(&["pause"]), "state"), "paused");
    assert_eq!(p(&env.ok(&["play"]), "state"), "playing");
    assert_eq!(p(&env.ok(&["toggle"]), "state"), "paused");
    assert_eq!(p(&env.ok(&["toggle"]), "state"), "playing");

    // seeking (the position is reported in whole seconds)
    let t = |args: &[&str]| f(&p(&env.ok(args), "current_time"));
    assert!((t(&["seek", "2:00"]) - 120.0).abs() <= 1.0);
    assert!((t(&["back"]) - 110.0).abs() <= 2.0);
    assert!((t(&["forward", "30"]) - 140.0).abs() <= 2.0);
    eventually("position to advance", Duration::from_secs(10), || {
        f(&p(&env.ok(&["status"]), "current_time")) > 140.0
    });

    // volume and mute (including mute right after a volume change)
    assert_eq!(p(&env.ok(&["volume", "30"]), "volume"), 30);
    let m = env.ok(&["mute"]);
    assert_eq!(
        (p(&m, "muted"), p(&m, "volume")),
        (Value::Bool(true), Value::from(0))
    );
    let u = env.ok(&["unmute"]);
    assert_eq!(
        (p(&u, "muted"), p(&u, "volume")),
        (Value::Bool(false), Value::from(30))
    );
    assert_eq!(p(&env.ok(&["volume", "60"]), "volume"), 60);
    assert_eq!(p(&env.ok(&["mute"]), "muted"), true);
    assert_eq!(p(&env.ok(&["unmute"]), "volume"), 60);

    // next / previous track
    env.ok(&["next"]);
    eventually("next track", Duration::from_secs(10), || {
        p(&env.ok(&["status"]), "title") != first
    });
    env.ok(&["prev"]);
    eventually("previous track", Duration::from_secs(10), || {
        p(&env.ok(&["status"]), "title") == first
    });

    // the cover view survives track changes and window layout changes
    assert_eq!(p(&env.ok(&["status"]), "cover_view"), true);
    let w = env.ok(&["window", "--layout", "bottom-right", "--size", "640x480"]);
    assert_eq!(p(&w, "cover_view"), true);
    assert_eq!(f(&w["window"]["size"]["width"]), 640.0);

    // unsupported actions are reported, not ignored
    for args in [["subtitles", "ja"], ["quality", "hd720"], ["speed", "1.5"]] {
        let e = env.err(&args);
        assert!(
            e.contains(&format!("{} is not supported by spotify", args[0])),
            "{args:?}: {e}"
        );
    }
    let e = env.err(&["audio", "ja"]);
    assert!(e.contains("audio is not supported by spotify"), "{e}");

    let shot = env.path("spotify.png");
    env.ok(&["screenshot", shot.to_str().unwrap()]);
    assert!(is_png(&shot));
    env.ok(&["pause"]);
    env.ok(&["close"]);
}

#[test]
#[ignore = "needs a Spotify login, network and windows; run with --ignored"]
fn spotify_track_links() {
    let Some(env) = env() else { return };
    // A track link plays that track.
    let v = env.ok(&["open", TRACK]);
    assert_eq!(p(&v, "state"), "playing");
    assert!(
        p(&v, "title")
            .as_str()
            .unwrap()
            .contains("Never Gonna Give You Up"),
        "{v}"
    );
    assert!((f(&p(&v, "duration")) - 213.0).abs() <= 1.0);
    env.ok(&["close"]);
    // --paused and --start are honored.
    let v = env.ok(&["open", TRACK, "--paused", "--start", "1:00"]);
    assert_eq!(p(&v, "state"), "paused");
    assert!((f(&p(&v, "current_time")) - 60.0).abs() <= 1.0);
    env.ok(&["close"]);
}
