//! End-to-end tests against the real Netflix web player (US account). They need
//! a tvctl data dir whose Chrome profile is signed in to Netflix
//! (`tvctl auth login netflix`):
//!
//!     TVCTL_E2E_PROFILE="$HOME/Library/Application Support/tvctl" \
//!         cargo test --test e2e_netflix -- --ignored --test-threads=1
//!
//! The tests play titles on that account (they appear in "Continue Watching")
//! and restore the player settings they change. Plans with ads may play ads;
//! the tests wait for them.

mod common;

use std::time::Duration;

use common::{Env, eventually, f, is_png};
use serde_json::Value;

/// Black Mirror S7 "Bête Noire" (next episode: "Hotel Reverie"); many subtitle
/// and audio languages.
const EPISODE: &str = "https://www.netflix.com/watch/81716297?trackId=264090261";
/// Black Mirror's title page.
const SHOW: &str = "https://www.netflix.com/title/70264888";

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

/// Waits until no ad plays and deferred `open` settings are applied.
fn no_ad(env: &Env) {
    eventually("the ad to end", Duration::from_secs(240), || {
        let s = env.ok(&["status"]);
        p(&s, "ad_showing") == false && p(&s, "pending").is_null()
    });
}

#[test]
#[ignore = "needs a Netflix login, network and windows; run with --ignored"]
fn netflix_controls() {
    let Some(env) = env() else { return };
    let v = env.ok(&[
        "open",
        EPISODE,
        "--start",
        "5:00",
        "--subtitles",
        "ja",
        "--layout",
        "right",
    ]);
    assert_eq!(v["service"], "netflix");
    assert_eq!(p(&v, "title"), "Black Mirror");
    assert_eq!(p(&v, "episode")["title"], "Bête Noire");
    assert_eq!(p(&v, "next_episode")["title"], "Hotel Reverie");
    no_ad(&env);
    let s = env.ok(&["status"]);
    assert!((300.0..340.0).contains(&f(&p(&s, "current_time"))), "{s}");
    assert_eq!(p(&s, "subtitles")["language"], "ja", "{s}");
    // Right after the --start seek the player may still be buffering.
    eventually("playback after the seek", Duration::from_secs(15), || {
        p(&env.ok(&["status"]), "state") == "playing"
    });

    // play / pause / toggle
    assert_eq!(p(&env.ok(&["pause"]), "state"), "paused");
    assert_eq!(p(&env.ok(&["play"]), "state"), "playing");
    assert_eq!(p(&env.ok(&["toggle"]), "state"), "paused");
    assert_eq!(p(&env.ok(&["toggle"]), "state"), "playing");

    // seeking
    let t = |args: &[&str]| f(&p(&env.ok(args), "current_time"));
    assert!((t(&["seek", "20:00"]) - 1200.0).abs() <= 1.0);
    assert!((t(&["back"]) - 1190.0).abs() <= 2.0);
    assert!((t(&["forward", "30"]) - 1220.0).abs() <= 2.0);
    eventually("position to advance", Duration::from_secs(15), || {
        f(&p(&env.ok(&["status"]), "current_time")) > 1221.0
    });

    // subtitles: full subtitles are preferred over closed captions
    let list = env.ok(&["subtitles", "list"]);
    assert!(p(&list, "tracks").as_array().unwrap().len() > 5);
    assert!(p(&env.ok(&["subtitles", "off"]), "subtitles").is_null());
    let es = p(&env.ok(&["subtitles", "es"]), "subtitles");
    assert_eq!(
        (es["language"].clone(), es["type"].clone()),
        ("es".into(), "PRIMARY".into())
    );
    assert!(env.err(&["subtitles", "xx"]).contains("no subtitles for"));
    env.ok(&["subtitles", "off"]);

    // audio tracks: regular audio is preferred over audio description
    let list = env.ok(&["audio", "list"]);
    assert!(p(&list, "audio_tracks").as_array().unwrap().len() > 1);
    let ja = p(&env.ok(&["audio", "ja"]), "audio");
    assert_eq!(ja["language"], "ja");
    let fr = p(&env.ok(&["audio", "fr"]), "audio");
    assert!(
        !fr["name"].as_str().unwrap().contains("Audio Description"),
        "{fr}"
    );
    assert_eq!(p(&env.ok(&["audio", "en"]), "audio")["language"], "en");

    // volume and mute
    assert_eq!(p(&env.ok(&["volume", "40"]), "volume"), 40);
    assert_eq!(p(&env.ok(&["mute"]), "muted"), true);
    let u = env.ok(&["unmute"]);
    assert_eq!(
        (p(&u, "muted"), p(&u, "volume")),
        (Value::Bool(false), Value::from(40))
    );
    env.ok(&["volume", "100"]);

    // speed (Netflix's menu rates)
    assert_eq!(p(&env.ok(&["speed", "1.5"]), "playback_rate"), 1.5);
    assert!(env.err(&["speed", "2"]).contains("not available"));
    assert_eq!(p(&env.ok(&["speed", "1"]), "playback_rate"), 1.0);

    // unsupported actions
    for args in [vec!["quality", "hd720"], vec!["prev"]] {
        let e = env.err(&args);
        assert!(e.contains("is not supported by netflix"), "{args:?}: {e}");
    }

    let shot = env.path("netflix.png");
    env.ok(&["screenshot", shot.to_str().unwrap()]);
    assert!(is_png(&shot));

    // next episode, in place
    let n = env.ok(&["next"]);
    assert_eq!(p(&n, "episode")["title"], "Hotel Reverie");
    assert_eq!(p(&n, "video_id"), "81716299");
    env.ok(&["pause"]);
    env.ok(&["close"]);
}

/// Logged out (a fresh profile; no login needed), Netflix shows a public title
/// page: `open` must say so, and `auth status` must not report a login.
#[test]
#[ignore = "needs network and windows; run with --ignored"]
fn netflix_logged_out() {
    let env = Env::new();
    env.up();
    assert_eq!(
        env.ok(&["auth", "status", "netflix"])["services"][0]["logged_in"],
        false
    );
    let e = env.err(&["open", SHOW]);
    assert!(e.contains("not signed in to Netflix"), "{e}");
    assert_eq!(
        env.ok(&["server", "status"])["player"],
        serde_json::Value::Null
    );
    // Visiting Netflix logged out must not make it look logged in.
    assert_eq!(
        env.ok(&["auth", "status", "netflix"])["services"][0]["logged_in"],
        false
    );
}

#[test]
#[ignore = "needs a Netflix login, network and windows; run with --ignored"]
fn netflix_urls() {
    let Some(env) = env() else { return };
    // Non-title pages are refused, and the failed open closes its window.
    let e = env.err(&["open", "https://www.netflix.com/browse"]);
    assert!(e.contains("not a Netflix title URL"), "{e}");
    assert_eq!(
        env.ok(&["server", "status"])["player"],
        serde_json::Value::Null
    );
    // A title that is not available (here: no such id) reports Netflix's error
    // page instead of waiting for a player.
    let e = env.err(&["open", "https://www.netflix.com/watch/12345678"]);
    assert!(
        e.contains("isn't available") && e.contains("(UI3003)"),
        "{e}"
    );
    assert!(!e.contains("Learn More"), "{e}");
    assert_eq!(
        env.ok(&["server", "status"])["player"],
        serde_json::Value::Null
    );
    // The login state is Netflix's, not just any netflix.com cookie.
    assert_eq!(
        env.ok(&["auth", "status", "netflix"])["services"][0]["logged_in"],
        true
    );
    // A title page plays like its Play button (an episode of the show).
    let v = env.ok(&["open", SHOW, "--paused"]);
    assert_eq!(p(&v, "title"), "Black Mirror");
    assert_eq!(p(&v, "state"), "paused");
    assert!(p(&v, "episode")["title"].is_string(), "{v}");
    env.ok(&["close"]);
}
