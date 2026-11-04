//! End-to-end tests against the real Crunchyroll web player (US Premium
//! account). They need a tvctl data dir whose Chrome profile is signed in to
//! Crunchyroll (`tvctl auth login crunchyroll`):
//!
//!     TVCTL_E2E_PROFILE="$HOME/Library/Application Support/tvctl" \
//!         cargo test --test e2e_crunchyroll -- --ignored --test-threads=1
//!
//! The tests play episodes on that account (they appear in its watch history)
//! and restore the player settings they change; subtitles end up off and the
//! audio Japanese, which become the profile's preferences.

mod common;

use std::time::Duration;

use common::{Env, eventually, f, is_png};
use serde_json::Value;

/// Frieren: Beyond Journey's End E1 "The Journey's End" (next: E2 "It Didn't
/// Have to Be Magic..."); many subtitle languages and dubs.
const EPISODE: &str = "https://www.crunchyroll.com/watch/G0DUND0K2/the-journeys-end";
const EPISODE_ID: &str = "G0DUND0K2";
/// The English dub of the same episode.
const EPISODE_EN_DUB_ID: &str = "G7PU4MZ1G";
const NEXT_ID: &str = "GEVUZXG7J";
/// Frieren's series page.
const SHOW: &str = "https://www.crunchyroll.com/series/GG5H5XQX4/frieren-beyond-journeys-end";
const SHOW_TITLE: &str = "Frieren: Beyond Journey's End";

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
#[ignore = "needs a Crunchyroll login, network and windows; run with --ignored"]
fn crunchyroll_controls() {
    let Some(env) = env() else { return };
    let v = env.ok(&[
        "open",
        EPISODE,
        "--start",
        "5:00",
        "--subtitles",
        "es",
        "--layout",
        "right",
    ]);
    let id = v["id"].as_str().unwrap().to_string();
    let id = id.as_str();
    assert_eq!(v["service"], "crunchyroll");
    assert_eq!(v["backend"], "chrome");
    assert_eq!(p(&v, "title"), SHOW_TITLE);
    assert_eq!(p(&v, "type"), "episode");
    assert_eq!(p(&v, "video_id"), EPISODE_ID);
    assert_eq!(p(&v, "episode")["title"], "The Journey's End");
    assert_eq!(p(&v, "episode")["number"], 1);
    assert_eq!(p(&v, "next_episode")["id"], NEXT_ID);
    assert_eq!(p(&v, "next_episode")["number"], 2);
    assert!((300.0..340.0).contains(&f(&p(&v, "current_time"))), "{v}");
    let es = p(&v, "subtitles")["language"].as_str().unwrap().to_string();
    assert!(es.starts_with("es-"), "{v}");
    eventually("playback after the seek", Duration::from_secs(15), || {
        p(&env.ok(&["status", id]), "state") == "playing"
    });

    // play / pause / toggle
    assert_eq!(p(&env.ok(&["pause", id]), "state"), "paused");
    assert_eq!(p(&env.ok(&["play", id]), "state"), "playing");
    assert_eq!(p(&env.ok(&["toggle", id]), "state"), "paused");
    assert_eq!(p(&env.ok(&["toggle", id]), "state"), "playing");

    // seeking
    let t = |args: &[&str]| f(&p(&env.ok(args), "current_time"));
    assert!((t(&["seek", id, "20:00"]) - 1200.0).abs() <= 1.0);
    assert!((t(&["back", id]) - 1190.0).abs() <= 2.0);
    assert!((t(&["forward", id, "30"]) - 1220.0).abs() <= 2.0);
    eventually("position to advance", Duration::from_secs(15), || {
        f(&p(&env.ok(&["status", id]), "current_time")) > 1221.0
    });

    // subtitles (burned in: switching reloads the stream)
    let list = env.ok(&["subtitles", id, "list"]);
    assert!(p(&list, "tracks").as_array().unwrap().len() > 5, "{list}");
    assert!(p(&env.ok(&["subtitles", id, "off"]), "subtitles").is_null());
    assert_eq!(
        p(&env.ok(&["subtitles", id, "de"]), "subtitles")["language"],
        "de-DE"
    );
    assert!(
        env.err(&["subtitles", id, "xx"])
            .contains("no subtitles for")
    );
    let s = env.ok(&["subtitles", id, "off"]);
    assert!(p(&s, "subtitles").is_null());
    assert!(f(&p(&s, "current_time")) > 1200.0, "{s}");

    // audio: each dub is its own episode id, switched in place
    let list = env.ok(&["audio", id, "list"]);
    assert!(p(&list, "audio_tracks").as_array().unwrap().len() > 1);
    let en = env.ok(&["audio", id, "en"]);
    assert_eq!(p(&en, "audio")["language"], "en-US");
    assert_eq!(p(&en, "video_id"), EPISODE_EN_DUB_ID);
    assert!(
        p(&en, "url").as_str().unwrap().contains(EPISODE_EN_DUB_ID),
        "{en}"
    );
    assert!(f(&p(&en, "current_time")) > 1200.0, "{en}");
    assert!(env.err(&["audio", id, "xx"]).contains("no audio track"));
    let ja = env.ok(&["audio", id, "ja"]);
    assert_eq!(p(&ja, "audio")["language"], "ja-JP");
    assert_eq!(p(&ja, "video_id"), EPISODE_ID);

    // volume and mute
    assert_eq!(p(&env.ok(&["volume", id, "40"]), "volume"), 40);
    assert_eq!(p(&env.ok(&["mute", id]), "muted"), true);
    let u = env.ok(&["unmute", id]);
    assert_eq!(
        (p(&u, "muted"), p(&u, "volume")),
        (Value::Bool(false), Value::from(40))
    );
    env.ok(&["volume", id, "100"]);

    // speed (Crunchyroll's menu rates)
    assert_eq!(p(&env.ok(&["speed", id, "0.75"]), "playback_rate"), 0.75);
    assert!(env.err(&["speed", id, "2"]).contains("not available"));
    assert_eq!(p(&env.ok(&["speed", id, "1"]), "playback_rate"), 1.0);

    // quality (data buckets)
    assert_eq!(
        p(&env.ok(&["quality", id, "moderate"]), "quality"),
        "moderate"
    );
    assert!(
        env.err(&["quality", id, "hd2160"])
            .contains("not available")
    );
    assert_eq!(
        p(&env.ok(&["quality", id, "highest"]), "quality"),
        "highest"
    );

    // unsupported actions
    let e = env.err(&["prev", id]);
    assert!(e.contains("is not supported by crunchyroll"), "{e}");

    let shot = env.path("crunchyroll.png");
    env.ok(&["screenshot", id, shot.to_str().unwrap()]);
    assert!(is_png(&shot));

    // next episode, in place
    let n = env.ok(&["next", id]);
    assert_eq!(p(&n, "video_id"), NEXT_ID);
    assert_eq!(p(&n, "episode")["number"], 2);
    env.ok(&["pause", id]);
    env.ok(&["close", id]);
}

/// Logged out (a fresh profile; no login needed), the watch page shows no
/// player: `open` must say so, and `auth status` must not report a login.
#[test]
#[ignore = "needs network and windows; run with --ignored"]
fn crunchyroll_logged_out() {
    let env = Env::new();
    env.up();
    let logged_in =
        |env: &Env| env.ok(&["auth", "status", "crunchyroll"])["services"][0]["logged_in"].clone();
    assert_eq!(logged_in(&env), false);
    let e = env.err(&["open", EPISODE]);
    assert!(e.contains("not signed in to Crunchyroll"), "{e}");
    assert_eq!(env.ok(&["list"]), serde_json::json!({ "players": [] }));
    // Visiting Crunchyroll logged out must not make it look logged in.
    assert_eq!(logged_in(&env), false);
}

#[test]
#[ignore = "needs a Crunchyroll login, network and windows; run with --ignored"]
fn crunchyroll_urls() {
    let Some(env) = env() else { return };
    // Non-title pages are refused, and the failed open closes its window.
    let e = env.err(&["open", "https://www.crunchyroll.com/videos/popular"]);
    assert!(e.contains("not a Crunchyroll episode or series URL"), "{e}");
    assert_eq!(env.ok(&["list"]), serde_json::json!({ "players": [] }));
    // An unknown id shows Crunchyroll's 404 page.
    let e = env.err(&[
        "open",
        "https://www.crunchyroll.com/watch/GZZZZZZZZ/nothing",
    ]);
    assert!(e.contains("page not found"), "{e}");
    assert_eq!(env.ok(&["list"]), serde_json::json!({ "players": [] }));
    assert_eq!(
        env.ok(&["auth", "status", "crunchyroll"])["services"][0]["logged_in"],
        true
    );
    // A series page plays like its Start/Continue button (an episode of the show).
    let v = env.ok(&["open", SHOW, "--paused"]);
    assert_eq!(p(&v, "title"), SHOW_TITLE);
    assert_eq!(p(&v, "state"), "paused");
    assert!(p(&v, "url").as_str().unwrap().contains("/watch/"), "{v}");
    assert!(p(&v, "episode")["title"].is_string(), "{v}");
    env.ok(&["close", v["id"].as_str().unwrap()]);
}
