//! End-to-end tests with the fake plugin: real server, real player window
//! (Google Chrome), no network. They open windows on the desktop, so they are
//! ignored by default:
//!
//!     cargo test --test e2e_fake -- --ignored --test-threads=1

mod common;

use std::time::Duration;

use common::{Env, eventually, f, is_png};
use serde_json::{Value, json};

fn open(env: &Env, path: &str, extra: &[&str]) -> Value {
    let url = env.url(path);
    let mut args = vec!["open", url.as_str()];
    args.extend_from_slice(extra);
    env.ok(&args)
}

fn player(v: &Value, key: &str) -> Value {
    v["player"][key].clone()
}

/// The (x, y, w, h) outer frame of the window.
fn frame(v: &Value) -> (f64, f64, f64, f64) {
    let w = &v["window"];
    (
        f(&w["position"]["x"]),
        f(&w["position"]["y"]),
        f(&w["outer_size"]["width"]),
        f(&w["outer_size"]["height"]),
    )
}

/// Asserts the window's outer frame equals `expected` (x, y, w, h) within 2 px.
fn assert_frame(v: &Value, expected: (f64, f64, f64, f64), what: &str) {
    let got = frame(v);
    let close = |a: f64, b: f64| (a - b).abs() <= 2.0;
    assert!(
        close(got.0, expected.0)
            && close(got.1, expected.1)
            && close(got.2, expected.2)
            && close(got.3, expected.3),
        "{what}: frame {got:?}, expected {expected:?}"
    );
}

fn work_area(v: &Value) -> (f64, f64, f64, f64) {
    let wa = &v["window"]["work_area"];
    (f(&wa["x"]), f(&wa["y"]), f(&wa["width"]), f(&wa["height"]))
}

/// Entries in the player window's history (1 in a new window).
fn history_length(env: &Env) -> u64 {
    env.ok(&["__eval", "history.length"]).as_u64().unwrap()
}

fn no_player(env: &Env) -> bool {
    env.ok(&["server", "status"])["player"].is_null()
}

#[test]
#[ignore = "opens windows; run with --ignored"]
fn server_lifecycle() {
    let env = Env::new();
    assert_eq!(env.ok(&["server", "status"])["running"], false);
    let up = env.ok(&["server", "up"]);
    assert_eq!(up["already_running"], false);
    assert!(up["pid"].as_u64().unwrap() > 0);
    assert!(up["player"].is_null(), "{up}");
    assert_eq!(env.ok(&["server", "up"])["already_running"], true);
    let st = env.ok(&["server", "status"]);
    assert_eq!(st["running"], true);
    assert!(st["player"].is_null());
    open(&env, "/video/1", &[]);
    assert_eq!(
        env.ok(&["server", "status"])["player"],
        json!({ "service": "fake", "title": "Fake video" })
    );
    let down = env.ok(&["server", "down"]);
    assert_eq!(down, json!({ "running": false, "was_running": true }));
    assert_eq!(env.ok(&["server", "down"])["was_running"], false);
    assert!(env.err(&["status"]).contains("not running"));
    // Shutting down closed the player.
    env.up();
    assert!(no_player(&env));
}

/// A server started from an SSH session cannot keep Chrome logins (no keychain):
/// `auth login` is refused, and login state and player responses carry a warning.
#[test]
#[ignore = "opens windows; run with --ignored"]
fn ssh_session_warnings() {
    let env = Env::new();
    let ssh = [("SSH_CONNECTION", "10.0.0.1 50000 10.0.0.2 22")];
    let up = env.run_with_env(&["server", "up"], &ssh).unwrap();
    assert_eq!(up["ssh_session"], true, "{up}");
    assert!(up["warning"].as_str().unwrap().contains("SSH session"));
    // Later clients (even from the Mac's own screen) reach that same server.
    let st = env.ok(&["server", "status"]);
    assert_eq!(st["ssh_session"], true);
    let e = env.err(&["auth", "login", "fake"]);
    assert!(
        e.contains("started from an SSH session") && e.contains("tvctl server down"),
        "{e}"
    );
    assert!(env.ok(&["auth", "status"])["warning"].is_string());
    let v = open(&env, "/video/1", &[]);
    assert!(v["warning"].as_str().unwrap().contains("keychain"), "{v}");
    env.ok(&["server", "down"]);
    // Started without SSH, there is no warning.
    let up = env.ok(&["server", "up"]);
    if std::env::var_os("SSH_CONNECTION").is_none() {
        assert_eq!(up["ssh_session"], false);
        assert!(up["warning"].is_null());
        assert!(open(&env, "/video/1", &[])["warning"].is_null());
    }
}

fn running(env: &Env) -> bool {
    env.ok(&["server", "status"])["running"] == true
}

/// Only `server up` starts the server: without it, commands that need one fail
/// and leave it stopped.
#[test]
fn commands_do_not_start_the_server() {
    let env = Env::new();
    for args in [
        vec!["open", "https://www.youtube.com/watch?v=x"],
        vec!["status"],
        vec!["close"],
        vec!["auth", "status"],
        vec!["auth", "login", "fake"],
    ] {
        let e = env.err(&args);
        assert!(
            e.contains("not running") && e.contains("server up"),
            "{args:?}: {e}"
        );
        assert!(!running(&env), "{args:?} started the server");
    }
    // Cast settings are saved without starting the server.
    let s = env.ok(&["cast", "on", "--name", "test"]);
    assert_eq!(
        (s["enabled"].clone(), s["server_running"].clone()),
        (json!(true), json!(false))
    );
    assert_eq!(s["in_player"], false);
    assert!(env.err(&["cast", "code"]).contains("not running"));
    assert!(!running(&env), "cast started the server");
    env.ok(&["cast", "off"]);
}

/// Opening and closing the player never stops the server, even when the user
/// closes the window.
#[test]
#[ignore = "opens windows; run with --ignored"]
fn server_stays_up_without_a_player() {
    let env = Env::new();
    env.up();
    open(&env, "/video/1", &[]);
    assert_eq!(env.ok(&["close"]), json!({ "closed": true }));
    std::thread::sleep(Duration::from_millis(1500));
    assert!(running(&env), "stopped after close");
    // The fake login page closes its own window, as a user closing it would.
    let _ = env.run(&["open", &env.url("/login")]);
    eventually("the window closed", Duration::from_secs(15), || {
        no_player(&env)
    });
    std::thread::sleep(Duration::from_millis(1500));
    assert!(running(&env), "stopped after the user closed the window");
    assert_eq!(env.ok(&["server", "up"])["already_running"], true);
}

#[test]
#[ignore = "opens windows; run with --ignored"]
fn plugins_listing() {
    let env = Env::new();
    let v = env.ok(&["plugins"]);
    let names: Vec<&str> = v["plugins"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["name"].as_str().unwrap())
        .collect();
    assert!(
        names.contains(&"fake") && names.contains(&"youtube"),
        "{names:?}"
    );
    let fake = v["plugins"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"] == "fake")
        .unwrap();
    assert!(fake.get("backends").is_none(), "{fake}");
    assert!(fake["source"].as_str().unwrap().ends_with("plugins/fake"));
}

#[test]
#[ignore = "opens windows; run with --ignored"]
fn open_errors() {
    let env = Env::new();
    env.up();
    assert!(
        env.err(&["open", "https://unknown.example/v"])
            .contains("no plugin handles unknown.example")
    );
    assert!(
        env.err(&["open", "ftp://127.0.0.1/v"])
            .contains("not a web URL")
    );
    let url = env.url("/v");
    assert!(
        env.err(&["open", &url, "--start", "later"])
            .contains("invalid time")
    );
    // An open whose adapter fails leaves no player (its window is closed),
    // also when it replaced what the window showed.
    for _ in 0..2 {
        let e = env.err(&["open", &env.url("/v?fail=1")]);
        assert!(e.contains("fake open failure"), "{e}");
        assert!(no_player(&env));
        open(&env, "/video/1", &[]);
    }
    env.ok(&["close"]);
    for cmd in [
        vec!["status"],
        vec!["play"],
        vec!["seek", "10"],
        vec!["window", "--layout", "left"],
        vec!["screenshot", "/tmp/x.png"],
        vec!["close"],
        vec!["__eval", "1"],
    ] {
        let e = env.err(&cmd);
        assert!(e.contains("no player is open"), "{cmd:?}: {e}");
    }
}

/// Every player command.
#[test]
#[ignore = "opens windows; run with --ignored"]
fn playback() {
    let env = Env::new();
    env.up();

    // open with every option
    let v = open(
        &env,
        "/video/1",
        &[
            "--start",
            "1:30",
            "--paused",
            "--subtitles",
            "ja",
            "--size",
            "640x360",
            "--position",
            "100,120",
        ],
    );
    assert!(v.get("id").is_none() && v.get("backend").is_none(), "{v}");
    assert_eq!(v["service"], "fake");
    assert_eq!(player(&v, "opened")["url"], env.url("/video/1"));
    assert_eq!(player(&v, "opened")["start"], 90.0);
    assert_eq!(player(&v, "opened")["paused"], true);
    assert_eq!(player(&v, "state"), "paused");
    assert_eq!(player(&v, "current_time"), 90.0);
    assert_eq!(player(&v, "subtitles"), "ja");
    assert_eq!(f(&v["window"]["size"]["width"]), 640.0);
    assert_eq!(f(&v["window"]["size"]["height"]), 360.0);
    assert_eq!(f(&v["window"]["position"]["x"]), 100.0);
    assert_eq!(f(&v["window"]["position"]["y"]), 120.0);
    assert!(v["window"].get("on_top").is_none(), "{v}");

    // status / play / pause / toggle
    let st = env.ok(&["status"]);
    assert_eq!(player(&st, "title"), "Fake video");
    assert_eq!(player(&env.ok(&["play"]), "state"), "playing");
    assert_eq!(player(&env.ok(&["pause"]), "state"), "paused");
    assert_eq!(player(&env.ok(&["toggle"]), "state"), "playing");
    assert_eq!(player(&env.ok(&["toggle"]), "state"), "paused");

    // next / previous
    assert_eq!(player(&env.ok(&["next"]), "item"), 1);
    assert_eq!(player(&env.ok(&["next"]), "item"), 2);
    assert_eq!(player(&env.ok(&["prev"]), "item"), 1);
    env.ok(&["seek", "90"]);

    // seeking: absolute, relative, clamped, forward/back defaults
    let t = |args: &[&str]| f(&player(&env.ok(args), "current_time"));
    assert_eq!(t(&["seek", "1:00"]), 60.0);
    assert_eq!(t(&["seek", "+10"]), 70.0);
    assert_eq!(t(&["seek", "-1m"]), 10.0);
    assert_eq!(t(&["seek", "-100"]), 0.0);
    assert_eq!(t(&["seek", "9999"]), 600.0);
    assert_eq!(t(&["seek", "1m30s"]), 90.0);
    assert_eq!(t(&["forward"]), 100.0);
    assert_eq!(t(&["forward", "30"]), 130.0);
    assert_eq!(t(&["back"]), 120.0);
    assert_eq!(t(&["back", "1:00"]), 60.0);

    // subtitles
    let list = env.ok(&["subtitles", "list"]);
    assert_eq!(player(&list, "tracks").as_array().unwrap().len(), 2);
    assert_eq!(
        player(&env.ok(&["subtitles", "off"]), "subtitles"),
        Value::Null
    );
    assert_eq!(player(&env.ok(&["subtitles", "on"]), "subtitles"), "en");
    assert_eq!(player(&env.ok(&["subtitles", "ja"]), "subtitles"), "ja");
    assert!(
        env.err(&["subtitles", "xx"])
            .contains("no subtitles for \"xx\"")
    );

    // audio tracks
    assert_eq!(
        player(&env.ok(&["audio", "list"]), "audio_tracks"),
        json!(["en", "ja"])
    );
    assert_eq!(player(&env.ok(&["audio", "ja"]), "audio"), "ja");
    assert!(env.err(&["audio", "fr"]).contains("no audio track"));

    // volume / mute / speed / quality
    assert_eq!(player(&env.ok(&["volume", "30"]), "volume"), 30);
    assert_eq!(player(&env.ok(&["mute"]), "muted"), true);
    assert_eq!(player(&env.ok(&["unmute"]), "muted"), false);
    assert_eq!(player(&env.ok(&["speed", "1.5"]), "playback_rate"), 1.5);
    assert_eq!(player(&env.ok(&["quality", "hd1080"]), "quality"), "hd1080");
    assert!(env.err(&["quality", "hd2160"]).contains("not available"));

    // screenshot
    let shot = env.path("shot.png");
    let s = env.ok(&["screenshot", shot.to_str().unwrap()]);
    assert_eq!(s["path"], shot.to_str().unwrap());
    assert!(is_png(&shot), "screenshot is not a PNG");

    // close
    assert_eq!(env.ok(&["close"]), json!({ "closed": true }));
    assert!(env.err(&["status"]).contains("no player is open"));
    assert!(no_player(&env));
}

/// Window size, position, every layout preset and fullscreen.
#[test]
#[ignore = "opens windows; run with --ignored"]
fn window() {
    let env = Env::new();
    env.up();
    let v = open(&env, "/video/1", &["--windowed"]);
    // Default window: 960x540 video area, centered.
    assert_eq!(f(&v["window"]["size"]["width"]), 960.0);
    assert_eq!(f(&v["window"]["size"]["height"]), 540.0);
    let (x, y, w, h) = work_area(&v);
    let ow = f(&v["window"]["outer_size"]["width"]);
    let oh = f(&v["window"]["outer_size"]["height"]);
    assert_frame(
        &v,
        (x + (w - ow) / 2.0, y + (h - oh) / 2.0, ow, oh),
        "default center",
    );

    // size and position
    let v = env.ok(&["window", "--size", "800x450", "--position", "40,60"]);
    assert_eq!(f(&v["window"]["size"]["width"]), 800.0);
    assert_eq!(f(&v["window"]["size"]["height"]), 450.0);
    assert_eq!(f(&v["window"]["position"]["x"]), 40.0);
    assert_eq!(f(&v["window"]["position"]["y"]), 60.0);

    // every region preset
    let (hw, hh) = (w / 2.0, h / 2.0);
    for (layout, expected) in [
        ("left", (x, y, hw, h)),
        ("right", (x + hw, y, hw, h)),
        ("top", (x, y, w, hh)),
        ("bottom", (x, y + hh, w, hh)),
        ("top-left", (x, y, hw, hh)),
        ("top-right", (x + hw, y, hw, hh)),
        ("bottom-left", (x, y + hh, hw, hh)),
        ("bottom-right", (x + hw, y + hh, hw, hh)),
        ("maximize", (x, y, w, h)),
    ] {
        let v = env.ok(&["window", "--layout", layout]);
        assert_frame(&v, expected, layout);
    }

    // anchored size (picture-in-picture style) and center
    let v = env.ok(&["window", "--layout", "bottom-right", "--size", "480x270"]);
    assert_eq!(f(&v["window"]["size"]["width"]), 480.0);
    assert_eq!(f(&v["window"]["size"]["height"]), 270.0);
    let (ow, oh) = (
        f(&v["window"]["outer_size"]["width"]),
        f(&v["window"]["outer_size"]["height"]),
    );
    assert_frame(&v, (x + w - ow, y + h - oh, ow, oh), "bottom-right 480x270");
    let v = env.ok(&["window", "--layout", "center"]);
    assert_frame(
        &v,
        (x + (w - ow) / 2.0, y + (h - oh) / 2.0, ow, oh),
        "center",
    );

    // fullscreen on / off / toggle, and a layout leaves fullscreen
    let fullscreen = || env.ok(&["status"])["window"]["fullscreen"].clone();
    env.ok(&["fullscreen", "on"]);
    eventually("fullscreen on", Duration::from_secs(5), || {
        fullscreen() == true
    });
    env.ok(&["fullscreen", "off"]);
    eventually("fullscreen off", Duration::from_secs(5), || {
        fullscreen() == false
    });
    std::thread::sleep(Duration::from_secs(1));
    env.ok(&["fullscreen"]);
    eventually("fullscreen toggled on", Duration::from_secs(5), || {
        fullscreen() == true
    });
    std::thread::sleep(Duration::from_secs(1));
    let v = env.ok(&["window", "--layout", "left"]);
    assert_eq!(v["window"]["fullscreen"], false);
    assert_frame(&v, (x, y, hw, h), "left after fullscreen");
    // The WebView backend's always-on-top is gone.
    assert!(env.err(&["window", "--on-top", "on"]).contains("on-top"));

    // open with window options moves the existing window
    let v = open(&env, "/video/2", &["--layout", "top-right"]);
    assert_frame(&v, (x + hw, y, hw, hh), "open --layout top-right");
    open(&env, "/video/3", &["--layout", "left", "--fullscreen"]);
    eventually("open --fullscreen", Duration::from_secs(5), || {
        fullscreen() == true
    });
    std::thread::sleep(Duration::from_secs(1));
    // Leaving fullscreen returns to the place given with --fullscreen.
    env.ok(&["fullscreen", "off"]);
    eventually("fullscreen off", Duration::from_secs(5), || {
        fullscreen() == false
    });
    assert_frame(
        &env.ok(&["status"]),
        (x, y, hw, h),
        "left after open --fullscreen",
    );
    env.ok(&["close"]);
}

/// There is one player window: `open` replaces what it shows in place.
#[test]
#[ignore = "opens windows; run with --ignored"]
fn open_replaces_content_in_the_same_window() {
    let env = Env::new();
    env.up();
    let a = open(
        &env,
        "/video/1",
        &["--size", "640x360", "--position", "100,120"],
    );
    assert_eq!(history_length(&env), 1);
    env.ok(&["seek", "30"]);

    // Same window (one more history entry), same place and size, new content.
    let b = open(&env, "/video/2", &["--windowed"]);
    assert_eq!(history_length(&env), 2);
    assert_eq!(frame(&b), frame(&a));
    assert_eq!(player(&b, "opened")["url"], env.url("/video/2"));
    assert_eq!(player(&b, "current_time"), 0.0);
    assert_eq!(f(&player(&env.ok(&["status"]), "current_time")), 0.0);

    // The same URL again reloads it (the adapter opens it afresh).
    env.ok(&["seek", "40"]);
    let again = open(&env, "/video/2", &["--windowed"]);
    assert_eq!(player(&again, "current_time"), 0.0);

    // Another plugin in the same window: its declarations apply.
    let limited = env.url("/v").replace("127.0.0.1", "localhost");
    let c = env.ok(&["open", &limited, "--windowed"]);
    assert_eq!(c["service"], "limited");
    assert_eq!(frame(&c), frame(&a));
    assert!(
        env.err(&["pause"])
            .contains("pause is not supported by limited")
    );
    // Its adapter is loaded into new documents of the window (after a reload).
    env.ok(&["__eval", "setTimeout(() => location.reload(), 0), 1"]);
    eventually("the adapter after reload", Duration::from_secs(10), || {
        env.run(&["status"])
            .is_ok_and(|v| v["player"]["title"] == "Fake video")
    });

    // `open` goes fullscreen by default; `--windowed` leaves it, back where
    // the window was.
    let windowed = frame(&c);
    open(&env, "/video/3", &[]);
    eventually("open goes fullscreen", Duration::from_secs(5), || {
        env.ok(&["status"])["window"]["fullscreen"] == true
    });
    std::thread::sleep(Duration::from_secs(1));
    open(&env, "/video/3", &["--windowed"]);
    eventually("open --windowed", Duration::from_secs(5), || {
        env.ok(&["status"])["window"]["fullscreen"] == false
    });
    std::thread::sleep(Duration::from_secs(1));
    assert_frame(&env.ok(&["status"]), windowed, "windowed after fullscreen");

    // After `close`, the next window opens where the last one was.
    let placed = env.ok(&["window", "--size", "500x300", "--position", "50,70"]);
    env.ok(&["close"]);
    let d = open(&env, "/video/4", &["--windowed"]);
    assert_eq!(history_length(&env), 1);
    assert_frame(&d, frame(&placed), "reopened where the last window was");
    // Window options win over the remembered place.
    env.ok(&["close"]);
    let e = open(&env, "/video/5", &["--size", "640x360"]);
    assert_eq!(f(&e["window"]["size"]["width"]), 640.0);
}

#[test]
#[ignore = "opens windows; run with --ignored"]
fn undeclared_actions_are_refused() {
    let env = Env::new();
    env.up();
    let url = env.url("/v").replace("127.0.0.1", "localhost");
    assert_eq!(env.ok(&["open", &url])["service"], "limited");
    // Declared actions work; `status` is always available.
    env.ok(&["status"]);
    env.ok(&["play"]);
    // Undeclared ones are refused by the server with the supported list.
    let e = env.err(&["seek", "10"]);
    assert!(
        e.contains("seek is not supported by limited; supported: status, play"),
        "{e}"
    );
    let e = env.err(&["prev"]);
    assert!(e.contains("previous is not supported"), "{e}");
    // `plugins` exposes the declarations.
    let plugins = env.ok(&["plugins"]);
    let limited = plugins["plugins"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"] == "limited")
        .unwrap();
    assert_eq!(limited["actions"], json!(["play"]));
    assert!(limited.get("unsupported_actions").is_none(), "{limited}");
}

#[test]
fn duck_settings_are_saved_without_a_server() {
    let env = Env::new();
    let s = env.ok(&["duck", "status"]);
    assert_eq!(s["enabled"], false, "{s}");
    assert_eq!(s["server_running"], false, "{s}");
    let s = env.ok(&["duck", "on", "--level", "20", "--apps", "ChatGPT,zoom.us"]);
    assert_eq!(s["enabled"], true, "{s}");
    let s = env.ok(&["duck", "status"]);
    assert_eq!(s["level"], 20, "{s}");
    assert_eq!(s["apps"], json!(["ChatGPT", "zoom.us"]), "{s}");
    // `off` keeps the other settings for the next `on`.
    let s = env.ok(&["duck", "off"]);
    assert_eq!(s["enabled"], false, "{s}");
    assert_eq!(s["level"], 20, "{s}");
    assert!(env.ok(&["duck", "apps"])["apps"].is_array());
    assert!(env.err(&["duck", "on", "--level", "101"]).contains("101"));
}

/// The ducking hook lowers what a page plays without changing the volume the
/// page sees. (The audio tap itself needs a person to grant a permission, so it
/// is not tested here.)
#[test]
#[ignore = "opens windows; run with --ignored"]
fn duck_hook_scales_media_volume_invisibly() {
    let env = Env::new();
    env.up();
    assert_eq!(env.ok(&["duck", "status"])["server_running"], true);
    open(&env, "/video/1", &[]);
    // Reload so the page starts with the hook installed before its own scripts.
    env.ok(&["__eval", "setTimeout(() => location.reload(), 0), 1"]);
    eventually("the hook after reload", Duration::from_secs(10), || {
        env.run(&["__eval", "typeof window.__tvctlDuck"]) == Ok(json!("object"))
    });
    let script = r#"(() => {
      const a = document.createElement("audio");
      document.body.appendChild(a);
      a.volume = 0.5;
      const d = window.__tvctlDuck;
      const r = [];
      d.set(0.1, 0);
      r.push(a.volume, d.actual(a));
      a.volume = 0.8;
      r.push(a.volume, d.actual(a));
      const b = new Audio();
      b.volume = 0.6;
      r.push(b.volume, d.actual(b));
      d.set(1, 0);
      r.push(a.volume, d.actual(a), d.actual(b));
      let error = null;
      try { a.volume = 2; } catch (e) { error = e.name; }
      r.push(error);
      return r.map((x) => typeof x === "number" ? Math.round(x * 1000) / 1000 : x);
    })()"#;
    assert_eq!(
        env.ok(&["__eval", script]),
        json!([
            0.5,
            0.05,
            0.8,
            0.08,
            0.6,
            0.06,
            0.8,
            0.8,
            0.6,
            "IndexSizeError"
        ])
    );
    // A fade ends at the target.
    let faded = r#"(async () => {
      const a = document.querySelector("audio");
      window.__tvctlDuck.set(0.25, 200);
      await new Promise((r) => setTimeout(r, 500));
      return [a.volume, Math.round(window.__tvctlDuck.actual(a) * 1000) / 1000];
    })()"#;
    assert_eq!(env.ok(&["__eval", faded]), json!([0.8, 0.2]));
}

#[test]
#[ignore = "opens an isolated Chrome window"]
fn fullscreen_cursor_idle() {
    let env = Env::new();
    env.up();
    open(&env, "/video/1", &["--windowed"]);
    let hidden = || {
        env.ok(&[
            "__eval",
            "getComputedStyle(document.body).cursor === 'none'",
        ]) == true
    };
    std::thread::sleep(Duration::from_secs(11));
    assert!(!hidden(), "windowed cursor must stay visible");
    env.ok(&["fullscreen", "on"]);
    eventually(
        "native fullscreen media query",
        Duration::from_secs(5),
        || env.ok(&["__eval", "matchMedia('(display-mode: fullscreen)').matches"]) == true,
    );
    // Let the native fullscreen animation (and its mouse events) finish.
    std::thread::sleep(Duration::from_secs(2));
    // Use a known position so synthetic stationary moves cannot reset the timer.
    env.ok(&[
        "__eval",
        "window.dispatchEvent(new MouseEvent('mousemove', {screenX: 100, screenY: 100}))",
    ]);
    std::thread::sleep(Duration::from_secs(9));
    assert!(!hidden(), "cursor must not hide before ten seconds");
    eventually(
        "idle fullscreen cursor hidden",
        Duration::from_secs(4),
        hidden,
    );
    env.ok(&[
        "__eval",
        "window.dispatchEvent(new MouseEvent('mousemove', {screenX: 101, screenY: 100}))",
    ]);
    assert!(!hidden(), "movement must restore the cursor immediately");
    eventually("cursor hides again", Duration::from_secs(12), hidden);
    env.ok(&["fullscreen", "off"]);
    eventually("native fullscreen exited", Duration::from_secs(15), || {
        env.ok(&["status"])["window"]["fullscreen"] == false
    });
    eventually("windowed cursor restored", Duration::from_secs(15), || {
        !hidden()
    });
    env.ok(&["close"]);
}

fn blackout(env: &Env) -> bool {
    env.ok(&["status"])["window"]["blackout"]
        .as_bool()
        .expect("window.blackout")
}

/// A paused player turns black after `TVCTL_BLACKOUT_SECS`; playing again or a
/// player command removes it, and the wait starts again.
#[test]
#[ignore = "opens windows; run with --ignored"]
fn blackout_while_paused() {
    let env = Env::new();
    env.run_with_env(&["server", "up"], &[("TVCTL_BLACKOUT_SECS", "1")])
        .unwrap();
    let v = open(&env, "/video/1", &["--paused"]);
    assert_eq!(v["window"]["blackout"], false);
    let wait = Duration::from_secs(10);
    eventually("blackout while paused", wait, || blackout(&env));

    // Playing removes it and keeps it off.
    env.ok(&["play"]);
    eventually("no blackout after play", wait, || !blackout(&env));
    std::thread::sleep(Duration::from_millis(2500));
    assert!(!blackout(&env), "black while playing");

    // Any player command removes it; still paused, it comes back.
    env.ok(&["pause"]);
    eventually("blackout after pause", wait, || blackout(&env));
    env.ok(&["seek", "+5"]);
    eventually("no blackout after a command", wait, || !blackout(&env));
    eventually("blackout again", wait, || blackout(&env));

    // Mouse input on the overlay removes it; synthetic moves of a resting
    // pointer (same position) do not.
    let fire = |ty: &str, x: i32| {
        let script = format!(
            "document.getElementById('__tvctl_blackout').dispatchEvent(new MouseEvent('{ty}', \
             {{bubbles: true, cancelable: true, screenX: {x}, screenY: 100}})), \
             window.__tvctlBlackout.shown"
        );
        env.ok(&["__eval", &script])
    };
    assert_eq!(fire("mousemove", 100), json!(true));
    assert_eq!(fire("mousemove", 101), json!(true));
    assert_eq!(fire("mousemove", 120), json!(false));
    eventually("blackout after the mouse", wait, || blackout(&env));
    assert_eq!(fire("mousedown", 120), json!(false));

    // A new open in the same window starts without blackout and still gets it.
    let v = open(&env, "/video/2", &["--paused"]);
    assert_eq!(v["window"]["blackout"], false);
    eventually("blackout in the replaced page", wait, || blackout(&env));
}

#[test]
#[ignore = "opens windows; run with --ignored"]
fn eval() {
    let env = Env::new();
    env.up();
    open(&env, "/video/1", &[]);
    assert_eq!(env.ok(&["__eval", "Promise.resolve(6 * 7)"]), json!(42));
    assert_eq!(env.ok(&["__eval", "document.title"]), json!(""));
    assert!(
        env.err(&["__eval", "throw new Error('nope')"])
            .contains("nope")
    );
}

#[test]
#[ignore = "opens windows; run with --ignored"]
fn plugin_can_request_navigation() {
    let env = Env::new();
    env.up();
    // In a new window, and in the existing one.
    for _ in 0..2 {
        let v = open(&env, "/video/2?redirect=1", &[]);
        let at = player(&v, "url");
        let at = at.as_str().unwrap();
        assert!(at.contains("landed=1") && !at.contains("redirect"), "{at}");
    }
}

#[test]
#[ignore = "opens windows; run with --ignored"]
fn login() {
    let env = Env::new();
    env.up();
    let st = env.ok(&["auth", "status"]);
    assert!(
        st["services"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["service"] == "fake" && s["logged_in"] == false),
        "{st}"
    );
    // The fake login page sets the cookie and closes its window, which ends `auth login`.
    assert_eq!(
        env.ok(&["auth", "login", "fake"]),
        json!({ "service": "fake", "logged_in": true })
    );
    assert_eq!(
        env.ok(&["auth", "status", "fake"])["services"][0]["logged_in"],
        true
    );
    assert!(
        env.err(&["auth", "login", "nosuch"])
            .contains("no plugin named")
    );
}

#[test]
#[ignore = "opens windows; run with --ignored"]
fn window_closed_by_the_user_is_forgotten() {
    let env = Env::new();
    env.up();
    // The fake login page closes its own window, as a user closing it would.
    // The page may close before `open` finishes, so its result is not checked.
    let _ = env.run(&["open", &env.url("/login")]);
    eventually(
        "the window to close itself",
        Duration::from_secs(10),
        || no_player(&env),
    );
    assert!(env.err(&["status"]).contains("no player is open"));
    // The next open makes a new window.
    let v = open(&env, "/video/1", &[]);
    assert_eq!(player(&v, "loaded"), true);
    assert_eq!(history_length(&env), 1);
}
