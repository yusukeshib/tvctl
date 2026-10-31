//! End-to-end tests against the real Spotify Web Player. Spotify needs a logged-in
//! account, so these run only with a mediactl data dir whose Chrome profile is
//! signed in (`mediactl auth login spotify`):
//!
//!     MEDIACTL_E2E_PROFILE="$HOME/Library/Application Support/mediactl" \
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
    if std::env::var_os("MEDIACTL_E2E_PROFILE").is_none() {
        eprintln!("skipped: set MEDIACTL_E2E_PROFILE to a logged-in mediactl data dir");
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
    let id = v["id"].as_str().unwrap().to_string();
    let id = id.as_str();
    assert_eq!(v["service"], "spotify");
    assert_eq!(v["backend"], "chrome");
    assert_eq!(p(&v, "logged_in"), true);
    assert_eq!(p(&v, "state"), "playing");
    // The large cover-art view is shown inside the window, not as OS fullscreen.
    assert_eq!(p(&v, "cover_view"), true);
    assert_eq!(v["window"]["fullscreen"], false);
    let first = p(&v, "title");
    assert!(first.as_str().is_some_and(|t| !t.is_empty()), "{v}");

    // play / pause / toggle
    assert_eq!(p(&env.ok(&["pause", id]), "state"), "paused");
    assert_eq!(p(&env.ok(&["play", id]), "state"), "playing");
    assert_eq!(p(&env.ok(&["toggle", id]), "state"), "paused");
    assert_eq!(p(&env.ok(&["toggle", id]), "state"), "playing");

    // seeking (the position is reported in whole seconds)
    let t = |args: &[&str]| f(&p(&env.ok(args), "current_time"));
    assert!((t(&["seek", id, "2:00"]) - 120.0).abs() <= 1.0);
    assert!((t(&["back", id]) - 110.0).abs() <= 2.0);
    assert!((t(&["forward", id, "30"]) - 140.0).abs() <= 2.0);
    eventually("position to advance", Duration::from_secs(10), || {
        f(&p(&env.ok(&["status", id]), "current_time")) > 140.0
    });

    // volume and mute (including mute right after a volume change)
    assert_eq!(p(&env.ok(&["volume", id, "30"]), "volume"), 30);
    let m = env.ok(&["mute", id]);
    assert_eq!(
        (p(&m, "muted"), p(&m, "volume")),
        (Value::Bool(true), Value::from(0))
    );
    let u = env.ok(&["unmute", id]);
    assert_eq!(
        (p(&u, "muted"), p(&u, "volume")),
        (Value::Bool(false), Value::from(30))
    );
    assert_eq!(p(&env.ok(&["volume", id, "60"]), "volume"), 60);
    assert_eq!(p(&env.ok(&["mute", id]), "muted"), true);
    assert_eq!(p(&env.ok(&["unmute", id]), "volume"), 60);

    // next / previous track
    env.ok(&["next", id]);
    eventually("next track", Duration::from_secs(10), || {
        p(&env.ok(&["status", id]), "title") != first
    });
    env.ok(&["prev", id]);
    eventually("previous track", Duration::from_secs(10), || {
        p(&env.ok(&["status", id]), "title") == first
    });

    // the cover view survives track changes and window layout changes
    assert_eq!(p(&env.ok(&["status", id]), "cover_view"), true);
    let w = env.ok(&[
        "window",
        id,
        "--layout",
        "bottom-right",
        "--size",
        "640x480",
    ]);
    assert_eq!(p(&w, "cover_view"), true);
    assert_eq!(f(&w["window"]["size"]["width"]), 640.0);

    // unsupported actions are reported, not ignored
    for args in [
        ["subtitles", id, "ja"],
        ["quality", id, "hd720"],
        ["speed", id, "1.5"],
    ] {
        assert!(
            env.err(&args)
                .contains("not supported by the spotify plugin"),
            "{args:?}"
        );
    }
    assert!(env.err(&["audio", id, "ja"]).contains("not supported"));

    let shot = env.path("spotify.png");
    env.ok(&["screenshot", id, shot.to_str().unwrap()]);
    assert!(is_png(&shot));
    env.ok(&["pause", id]);
    env.ok(&["close", id]);
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
    env.ok(&["close", v["id"].as_str().unwrap()]);
    // --paused and --start are honored.
    let v = env.ok(&["open", TRACK, "--paused", "--start", "1:00"]);
    assert_eq!(p(&v, "state"), "paused");
    assert!((f(&p(&v, "current_time")) - 60.0).abs() <= 1.0);
    env.ok(&["close", v["id"].as_str().unwrap()]);
}
