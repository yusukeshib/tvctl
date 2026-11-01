//! End-to-end tests against the real Prime Video web player on amazon.com (US)
//! and amazon.co.jp (Japan). They need a tvctl data dir whose Chrome profile is
//! signed in to both Amazon accounts with Prime Video
//! (`tvctl auth login primevideo` and `tvctl auth login primevideo --site jp`):
//!
//!     TVCTL_E2E_PROFILE="$HOME/Library/Application Support/tvctl" \
//!         cargo test --test e2e_primevideo -- --ignored --test-threads=1
//!
//! The tests play titles on that account (they appear in "Continue watching")
//! and restore the player settings they change (subtitles, audio, volume,
//! speed, quality). Titles with ads may play ads; the tests wait for them.

mod common;

use std::time::Duration;

use common::{Env, eventually, f, is_png};
use serde_json::Value;

/// "Project Hail Mary" (movie; many subtitle and audio languages; ad breaks
/// from 30:34, so the tests stay before that).
const MOVIE: &str = "https://www.amazon.com/gp/video/detail/B0GHXVBQLM";
/// "Reacher" (series; the detail page plays an episode that has a next one).
const SERIES: &str = "https://www.amazon.com/gp/video/detail/B09ML1GHXS";

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

/// `open` that waits out Amazon's simultaneous-stream limit, which several
/// quick opens on one account can hit while closed streams are released.
fn open(env: &Env, args: &[&str]) -> Value {
    let mut all = vec!["open"];
    all.extend_from_slice(args);
    for _ in 0..12 {
        match env.run(&all) {
            Ok(v) => return v,
            Err(e) if e.contains("concurrency_limit") => {
                std::thread::sleep(Duration::from_secs(30))
            }
            Err(e) => panic!("tvctl {all:?} failed: {e}"),
        }
    }
    panic!("tvctl {all:?}: stream limit did not clear");
}

/// Waits until no ad plays (pre-roll ads can be a couple of minutes long).
fn no_ad(env: &Env, id: &str) {
    eventually("the ad to end", Duration::from_secs(240), || {
        let s = env.ok(&["status", id]);
        p(&s, "ad_showing") == false && p(&s, "pending").is_null()
    });
}

#[test]
#[ignore = "needs a Prime Video login, network and windows; run with --ignored"]
fn primevideo_movie_controls() {
    let Some(env) = env() else { return };
    let v = open(
        &env,
        &[
            "https://www.amazon.com/gp/video/detail/B0GHXVBQLM?ref_=atv_dp_amz_c_pZqpc5_1_2",
            "--start",
            "5:00",
            "--subtitles",
            "ja",
            "--layout",
            "right",
        ],
    );
    let id = v["id"].as_str().unwrap().to_string();
    let id = id.as_str();
    assert_eq!(v["service"], "primevideo");
    assert_eq!(v["backend"], "chrome");
    assert_eq!(p(&v, "title"), "Project Hail Mary");
    assert_eq!(p(&v, "state"), "playing");
    assert!(f(&p(&v, "duration")) > 9000.0, "{v}");
    // A pre-roll ad defers --start and --subtitles until it ends.
    no_ad(&env, id);
    let s = env.ok(&["status", id]);
    assert!((300.0..330.0).contains(&f(&p(&s, "current_time"))), "{s}");
    assert_eq!(p(&s, "subtitles")["language"], "ja-jp", "{s}");

    // play / pause / toggle
    assert_eq!(p(&env.ok(&["pause", id]), "state"), "paused");
    assert_eq!(p(&env.ok(&["play", id]), "state"), "playing");
    assert_eq!(p(&env.ok(&["toggle", id]), "state"), "paused");
    assert_eq!(p(&env.ok(&["toggle", id]), "state"), "playing");

    // seeking (before the first ad break)
    let t = |args: &[&str]| f(&p(&env.ok(args), "current_time"));
    assert!((t(&["seek", id, "10:00"]) - 600.0).abs() <= 1.0);
    assert!((t(&["back", id]) - 590.0).abs() <= 2.0);
    assert!((t(&["forward", id, "30"]) - 620.0).abs() <= 2.0);
    eventually("position to advance", Duration::from_secs(15), || {
        f(&p(&env.ok(&["status", id]), "current_time")) > 621.0
    });

    // subtitles
    let list = env.ok(&["subtitles", id, "list"]);
    let langs: Vec<_> = p(&list, "tracks")
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["language"].as_str().unwrap().to_string())
        .collect();
    assert!(langs.contains(&"ja-jp".to_string()) && langs.contains(&"en-us".to_string()));
    assert!(p(&env.ok(&["subtitles", id, "off"]), "subtitles").is_null());
    assert_eq!(
        p(&env.ok(&["subtitles", id, "en"]), "subtitles")["language"],
        "en-us"
    );
    assert!(
        env.err(&["subtitles", id, "xx"])
            .contains("no subtitles for")
    );
    env.ok(&["subtitles", id, "off"]);

    // audio tracks
    let list = env.ok(&["audio", id, "list"]);
    assert!(p(&list, "audio_tracks").as_array().unwrap().len() > 1);
    assert_eq!(
        p(&env.ok(&["audio", id, "ja"]), "audio")["language"],
        "ja-jp"
    );
    assert_eq!(
        p(&env.ok(&["audio", id, "en"]), "audio")["id"],
        "en-us_dialog_0"
    );

    // volume and mute
    assert_eq!(p(&env.ok(&["volume", id, "40"]), "volume"), 40);
    assert_eq!(p(&env.ok(&["mute", id]), "muted"), true);
    let u = env.ok(&["unmute", id]);
    assert_eq!(
        (p(&u, "muted"), p(&u, "volume")),
        (Value::Bool(false), Value::from(40))
    );
    env.ok(&["volume", id, "100"]);

    // speed (the player offers fixed rates)
    assert_eq!(p(&env.ok(&["speed", id, "1.5"]), "playback_rate"), 1.5);
    assert!(env.err(&["speed", id, "1.1"]).contains("not available"));
    assert_eq!(p(&env.ok(&["speed", id, "1"]), "playback_rate"), 1.0);

    // quality (data usage caps)
    let q = env.ok(&["quality", id, "good"]);
    assert_eq!(p(&q, "quality"), "good");
    assert_eq!(
        p(&q, "available_qualities"),
        serde_json::json!(["good", "better", "best"])
    );
    assert!(env.err(&["quality", id, "hd720"]).contains("not available"));
    assert_eq!(p(&env.ok(&["quality", id, "best"]), "quality"), "best");

    // a movie has no next episode; previous is not supported at all
    assert!(env.err(&["next", id]).contains("no next episode"));
    let e = env.err(&["prev", id]);
    assert!(e.contains("previous is not supported by primevideo"), "{e}");

    let shot = env.path("primevideo.png");
    env.ok(&["screenshot", id, shot.to_str().unwrap()]);
    assert!(is_png(&shot));
    env.ok(&["pause", id]);
    env.ok(&["close", id]);
}

#[test]
#[ignore = "needs a Prime Video login, network and windows; run with --ignored"]
fn primevideo_series_next_episode() {
    let Some(env) = env() else { return };
    let v = open(&env, &[SERIES]);
    let id = v["id"].as_str().unwrap().to_string();
    let id = id.as_str();
    assert_eq!(p(&v, "type"), "TV Show");
    let next = p(&v, "next_episode");
    assert!(next["title_id"].is_string(), "{v}");
    let n = env.ok(&["next", id]);
    assert_eq!(p(&n, "title_id"), next["title_id"]);
    assert_eq!(p(&n, "title"), next["title"]);
    assert_eq!(p(&n, "state"), "playing");
    env.ok(&["pause", id]);
    env.ok(&["close", id]);
}

#[test]
#[ignore = "needs a Prime Video login, network and windows; run with --ignored"]
fn primevideo_japan() {
    let Some(env) = env() else { return };
    let auth = env.ok(&["auth", "status", "primevideo"]);
    let sites = &auth["services"][0]["sites"];
    assert_eq!(
        (sites["us"].clone(), sites["jp"].clone()),
        (Value::Bool(true), Value::Bool(true)),
        "{auth}"
    );
    // Reacher on amazon.co.jp (the path has a display-language prefix).
    let v = open(
        &env,
        &[
            "https://www.amazon.co.jp/-/en/gp/video/detail/B09R26WLZ5",
            "--start",
            "5:00",
            "--subtitles",
            "ja",
        ],
    );
    let id = v["id"].as_str().unwrap().to_string();
    let id = id.as_str();
    assert!(
        p(&v, "url")
            .as_str()
            .unwrap()
            .starts_with("https://www.amazon.co.jp/"),
        "{v}"
    );
    assert_eq!(p(&v, "type"), "TV Show");
    no_ad(&env, id);
    let s = env.ok(&["status", id]);
    assert!((300.0..330.0).contains(&f(&p(&s, "current_time"))), "{s}");
    assert_eq!(p(&s, "subtitles")["language"], "ja-jp", "{s}");
    assert_eq!(p(&env.ok(&["pause", id]), "state"), "paused");
    assert!((f(&p(&env.ok(&["seek", id, "10:00"]), "current_time")) - 600.0).abs() <= 1.0);
    assert!(p(&env.ok(&["subtitles", id, "off"]), "subtitles").is_null());
    env.ok(&["close", id]);
}

#[test]
#[ignore = "needs a Prime Video login, network and windows; run with --ignored"]
fn primevideo_urls() {
    let Some(env) = env() else { return };
    // Non-title Amazon pages are refused.
    let e = env.err(&["open", "https://www.amazon.com/s?k=usb+cable"]);
    assert!(e.contains("not a Prime Video title URL"), "{e}");
    // The failed open closed its window.
    assert_eq!(env.ok(&["list"]), serde_json::json!({ "players": [] }));
    // primevideo.com and /dp/ links open the amazon.com detail page; --paused works.
    let v = open(
        &env,
        &[
            "https://www.primevideo.com/detail/0HO80FV4Z40LP5NE2D9K7AQ0KD/ref=atv_sr_def_c_unkc__1_1_1",
            "--paused",
        ],
    );
    assert_eq!(p(&v, "title"), "Project Hail Mary");
    assert_eq!(p(&v, "state"), "paused");
    assert!(
        p(&v, "url")
            .as_str()
            .unwrap()
            .starts_with("https://www.amazon.com/gp/video/detail/0HO80FV4Z40LP5NE2D9K7AQ0KD"),
        "{v}"
    );
    env.ok(&["close", v["id"].as_str().unwrap()]);
    let v = open(
        &env,
        &[&MOVIE.replace("/gp/video/detail/", "/dp/"), "--paused"],
    );
    assert_eq!(p(&v, "title"), "Project Hail Mary");
    env.ok(&["close", v["id"].as_str().unwrap()]);
}
