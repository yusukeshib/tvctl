//! End-to-end tests against real YouTube (network, real windows). Ignored by
//! default:
//!
//!     cargo test --test e2e_youtube -- --ignored --test-threads=1
//!
//! By default the Chrome backend runs logged out in a fresh profile, so ads may
//! play and the tests wait for them. To use a logged-in profile (no ads with
//! Premium), point MEDIACTL_E2E_PROFILE at a mediactl data dir, e.g.
//! `MEDIACTL_E2E_PROFILE="$HOME/Library/Application Support/mediactl"`.

mod common;

use std::time::Duration;

use common::{Env, eventually, f, is_png};
use serde_json::Value;

/// TED talk with many manual subtitle tracks, including Japanese.
const TED: &str = "https://www.youtube.com/watch?v=iG9CE55wbtY";
/// "Me at the zoo": English captions only (Japanese needs auto-translation).
const ZOO: &str = "https://youtu.be/jNQXAC9IVRw";

fn p(v: &Value, key: &str) -> Value {
    v["player"][key].clone()
}

fn wait_playing_main_video(env: &Env, id: &str) {
    eventually("main video playing", Duration::from_secs(90), || {
        let st = env.ok(&["status", id]);
        p(&st, "state") == "playing" && p(&st, "ad_showing") != true
    });
}

fn suite(backend: &str) {
    let env = Env::with_profile();
    env.up();
    let url = format!("{TED}&t=60");
    let v = env.ok(&["open", &url, "--backend", backend, "--layout", "right"]);
    let id = v["id"].as_str().unwrap().to_string();
    let id = id.as_str();
    assert_eq!(v["service"], "youtube");
    assert_eq!(v["backend"], backend);
    wait_playing_main_video(&env, id);
    let st = env.ok(&["status", id]);
    assert_eq!(p(&st, "video_id"), "iG9CE55wbtY");
    assert!(
        p(&st, "title")
            .as_str()
            .unwrap()
            .contains("Do schools kill creativity")
    );
    assert!(f(&p(&st, "duration")) > 1000.0);
    assert!(
        f(&p(&st, "current_time")) >= 59.0,
        "t=60 in the URL is honored"
    );
    if backend == "chrome" {
        assert_eq!(p(&st, "fills_window"), true);
    }

    assert_eq!(p(&env.ok(&["pause", id]), "state"), "paused");
    let t = f(&p(&env.ok(&["seek", id, "5:00"]), "current_time"));
    assert!((t - 300.0).abs() < 1.0, "{t}");
    let t = f(&p(&env.ok(&["forward", id, "30"]), "current_time"));
    assert!((t - 330.0).abs() < 2.0, "{t}");
    let t = f(&p(&env.ok(&["back", id]), "current_time"));
    assert!((t - 320.0).abs() < 2.0, "{t}");
    // `play` waits a bounded time; buffering after a seek can take longer.
    let st = p(&env.ok(&["play", id]), "state");
    assert!(st == "playing" || st == "buffering", "{st}");
    wait_playing_main_video(&env, id);
    let st = env.ok(&["toggle", id]);
    assert_eq!(p(&st, "state"), "paused");
    env.ok(&["play", id]);

    let list = env.ok(&["subtitles", id, "list"]);
    let langs: Vec<&str> = list["player"]["tracks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["language"].as_str().unwrap())
        .collect();
    assert!(langs.contains(&"ja") && langs.contains(&"en"), "{langs:?}");
    assert_eq!(
        p(&env.ok(&["subtitles", id, "ja"]), "subtitles")["language"],
        "ja"
    );
    assert_eq!(
        p(&env.ok(&["subtitles", id, "off"]), "subtitles"),
        Value::Null
    );

    assert_eq!(p(&env.ok(&["volume", id, "20"]), "volume"), 20);
    assert_eq!(p(&env.ok(&["mute", id]), "muted"), true);
    assert_eq!(p(&env.ok(&["unmute", id]), "muted"), false);
    assert_eq!(p(&env.ok(&["speed", id, "1.5"]), "playback_rate"), 1.5);
    let q = env.ok(&["quality", id, "small"]);
    assert_eq!(p(&q, "quality"), "small");
    assert!(
        env.err(&["quality", id, "highres"])
            .contains("not available")
    );

    let shot = env.path("yt.png");
    env.ok(&["screenshot", id, shot.to_str().unwrap()]);
    assert!(is_png(&shot));
    env.ok(&["close", id]);

    // Auto-translated Japanese subtitles on a video without a Japanese track.
    let v = env.ok(&["open", ZOO, "--backend", backend, "--subtitles", "ja"]);
    let id = v["id"].as_str().unwrap().to_string();
    wait_playing_main_video(&env, &id);
    eventually("translated subtitles", Duration::from_secs(20), || {
        let s = p(&env.ok(&["status", &id]), "subtitles");
        s["translated_to"] == "ja" && s["language"] == "en"
    });
}

#[test]
#[ignore = "needs network and opens windows; run with --ignored"]
fn youtube_webview() {
    suite("webview");
}

#[test]
#[ignore = "needs network and opens windows; run with --ignored"]
fn youtube_chrome() {
    suite("chrome");
}

#[test]
#[ignore = "needs network and opens windows; run with --ignored"]
fn youtube_chrome_audio_tracks_and_fill() {
    let env = Env::with_profile();
    env.up();
    let v = env.ok(&["open", TED, "--backend", "chrome"]);
    let id = v["id"].as_str().unwrap().to_string();
    wait_playing_main_video(&env, &id);
    let a = env.ok(&["audio", &id, "list"]);
    assert!(
        a["player"]["audio_tracks"]
            .as_array()
            .is_some_and(|t| !t.is_empty())
    );
    assert!(!env.err(&["audio", &id, "zz"]).is_empty());
    // Only the player is visible, and it stays that way over time (YouTube may
    // reload or re-render the page, e.g. around ads).
    for _ in 0..5 {
        assert_eq!(p(&env.ok(&["status", &id]), "fills_window"), true);
        std::thread::sleep(Duration::from_secs(2));
    }
    env.ok(&["seek", &id, "10:00"]);
    assert_eq!(p(&env.ok(&["status", &id]), "fills_window"), true);
    let st = env.ok(&["status", &id]);
    assert_eq!(st["window"]["fullscreen"], false);
    // Matches the profile in use (logged out unless MEDIACTL_E2E_PROFILE is set).
    assert_eq!(
        p(&st, "logged_in"),
        std::env::var_os("MEDIACTL_E2E_PROFILE").is_some()
    );
}

#[test]
#[ignore = "needs network and opens windows; run with --ignored"]
fn youtube_url_forms() {
    let env = Env::with_profile();
    env.up();
    for url in [
        "https://www.youtube.com/watch?v=jNQXAC9IVRw&t=5",
        "https://youtu.be/jNQXAC9IVRw?t=5",
        "https://www.youtube.com/shorts/jNQXAC9IVRw",
        "https://www.youtube.com/embed/jNQXAC9IVRw?start=5",
        "https://m.youtube.com/watch?v=jNQXAC9IVRw&t=5s",
    ] {
        let v = env.ok(&["open", url, "--paused"]);
        assert_eq!(p(&v, "video_id"), "jNQXAC9IVRw", "{url}");
        env.ok(&["close", v["id"].as_str().unwrap()]);
    }
    let bad = env.err(&["open", "https://www.youtube.com/feed/trending"]);
    assert!(bad.contains("could not find a video id"), "{bad}");
}
