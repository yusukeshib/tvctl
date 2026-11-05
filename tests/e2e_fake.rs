//! End-to-end tests with the fake plugin: real server, real windows (native
//! WebView and Google Chrome), no network. They open windows on the desktop, so
//! they are ignored by default:
//!
//!     cargo test --test e2e_fake -- --ignored --test-threads=1

mod common;

use std::time::Duration;

use common::{Env, eventually, f, is_png};
use serde_json::{Value, json};

fn open(env: &Env, backend: &str, extra: &[&str]) -> Value {
    let url = env.url("/video/1");
    let mut args = vec!["open", url.as_str(), "--backend", backend];
    args.extend_from_slice(extra);
    env.ok(&args)
}

fn player(v: &Value, key: &str) -> Value {
    v["player"][key].clone()
}

/// Asserts the window's outer frame equals `expected` (x, y, w, h) within 2 px.
fn assert_frame(v: &Value, expected: (f64, f64, f64, f64), what: &str) {
    let w = &v["window"];
    let got = (
        f(&w["position"]["x"]),
        f(&w["position"]["y"]),
        f(&w["outer_size"]["width"]),
        f(&w["outer_size"]["height"]),
    );
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

#[test]
#[ignore = "opens windows; run with --ignored"]
fn server_lifecycle() {
    let env = Env::new();
    assert_eq!(env.ok(&["server", "status"])["running"], false);
    // Without a server there are no players; listing does not start one.
    assert_eq!(env.ok(&["list"]), json!({ "players": [] }));
    assert_eq!(env.ok(&["server", "status"])["running"], false);
    let up = env.ok(&["server", "up"]);
    assert_eq!(up["already_running"], false);
    assert_eq!(up["persistent"], true);
    assert!(up["pid"].as_u64().unwrap() > 0);
    assert_eq!(env.ok(&["server", "up"])["already_running"], true);
    let st = env.ok(&["server", "status"]);
    assert_eq!(st["running"], true);
    assert_eq!(st["players"], 0);
    assert_eq!(env.ok(&["list"]), json!({ "players": [] }));
    let down = env.ok(&["server", "down"]);
    assert_eq!(down, json!({ "running": false, "was_running": true }));
    assert_eq!(env.ok(&["server", "down"])["was_running"], false);
    assert!(env.err(&["status", "1"]).contains("not running"));
}

/// A server started from an SSH session cannot keep Chrome logins (no keychain):
/// `auth login` is refused, and login state and Chrome players carry a warning.
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
    let chrome = open(&env, "chrome", &[]);
    assert!(
        chrome["warning"].as_str().unwrap().contains("keychain"),
        "{chrome}"
    );
    let webview = open(&env, "webview", &[]);
    assert!(webview["warning"].is_null(), "{webview}");
    env.ok(&["server", "down"]);
    // Started without SSH, there is no warning.
    let up = env.ok(&["server", "up"]);
    if std::env::var_os("SSH_CONNECTION").is_none() {
        assert_eq!(up["ssh_session"], false);
        assert!(up["warning"].is_null());
        assert!(open(&env, "chrome", &[])["warning"].is_null());
    }
}

fn running(env: &Env) -> bool {
    env.ok(&["server", "status"])["running"] == true
}

#[test]
#[ignore = "opens windows; run with --ignored"]
fn server_starts_on_open_and_stops_when_idle() {
    let env = Env::new();
    // Player commands do not start a server.
    assert!(env.err(&["status", "1"]).contains("not running"));
    assert!(!running(&env));
    for backend in ["webview", "chrome"] {
        let v = open(&env, backend, &[]);
        let id = v["id"].as_str().unwrap().to_string();
        let st = env.ok(&["server", "status"]);
        assert_eq!(st["persistent"], false, "{backend}");
        assert_eq!(st["players"], 1);
        // Stays up while a player is open, even past the idle timeout.
        std::thread::sleep(Duration::from_millis(1500));
        assert!(running(&env), "{backend}: stopped with an open player");
        env.ok(&["close", &id]);
        eventually("idle shutdown", Duration::from_secs(10), || !running(&env));
    }
}

#[test]
#[ignore = "opens windows; run with --ignored"]
fn server_stops_when_the_user_closes_the_last_window() {
    let env = Env::new();
    // The fake login page closes its own window, as a user closing it would.
    let _ = env.run(&["open", &env.url("/login"), "--backend", "chrome"]);
    eventually(
        "idle shutdown after the window closed",
        Duration::from_secs(15),
        || !running(&env),
    );
}

#[test]
#[ignore = "opens windows; run with --ignored"]
fn persistent_server_stays_up() {
    let env = Env::new();
    // `server up` on an on-demand server makes it persistent.
    let v = open(&env, "webview", &[]);
    let up = env.ok(&["server", "up"]);
    assert_eq!(
        (up["already_running"].clone(), up["persistent"].clone()),
        (json!(true), json!(true))
    );
    env.ok(&["close", v["id"].as_str().unwrap()]);
    std::thread::sleep(Duration::from_millis(2500));
    assert!(running(&env));
    // Auth commands also start a server on demand; it then stops when idle.
    env.ok(&["server", "down"]);
    assert_eq!(
        env.ok(&["auth", "status", "fake"])["services"][0]["logged_in"],
        false
    );
    assert_eq!(env.ok(&["server", "status"])["persistent"], false);
    eventually(
        "idle shutdown after auth status",
        Duration::from_secs(10),
        || !running(&env),
    );
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
    assert_eq!(fake["backends"], json!(["webview", "chrome"]));
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
    // A Chrome player whose adapter fails to open closes its window (the caller
    // gets no id to close it with).
    let failing = env.url("/v?fail=1");
    let e = env.err(&["open", &failing, "--backend", "chrome"]);
    assert!(e.contains("fake open failure"), "{e}");
    assert_eq!(env.ok(&["list"]), serde_json::json!({ "players": [] }));
    for cmd in [
        vec!["status", "9"],
        vec!["play", "9"],
        vec!["seek", "9", "10"],
        vec!["window", "9", "--layout", "left"],
        vec!["screenshot", "9", "/tmp/x.png"],
        vec!["close", "9"],
    ] {
        assert!(env.err(&cmd).contains("no player with id \"9\""), "{cmd:?}");
    }
}

/// Every player command on one backend.
fn playback_suite(backend: &str) {
    let env = Env::new();
    env.up();

    // open with every option
    let v = open(
        &env,
        backend,
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
    let id = v["id"].as_str().unwrap().to_string();
    let id = id.as_str();
    assert_eq!(v["service"], "fake");
    assert_eq!(v["backend"], backend);
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

    // status / play / pause / toggle
    let st = env.ok(&["status", id]);
    assert_eq!(st["id"], id);
    assert_eq!(player(&st, "title"), "Fake video");
    assert_eq!(player(&env.ok(&["play", id]), "state"), "playing");
    assert_eq!(player(&env.ok(&["pause", id]), "state"), "paused");
    assert_eq!(player(&env.ok(&["toggle", id]), "state"), "playing");
    assert_eq!(player(&env.ok(&["toggle", id]), "state"), "paused");

    // next / previous
    assert_eq!(player(&env.ok(&["next", id]), "item"), 1);
    assert_eq!(player(&env.ok(&["next", id]), "item"), 2);
    assert_eq!(player(&env.ok(&["prev", id]), "item"), 1);
    env.ok(&["seek", id, "90"]);

    // seeking: absolute, relative, clamped, forward/back defaults
    let t = |args: &[&str]| f(&player(&env.ok(args), "current_time"));
    assert_eq!(t(&["seek", id, "1:00"]), 60.0);
    assert_eq!(t(&["seek", id, "+10"]), 70.0);
    assert_eq!(t(&["seek", id, "-1m"]), 10.0);
    assert_eq!(t(&["seek", id, "-100"]), 0.0);
    assert_eq!(t(&["seek", id, "9999"]), 600.0);
    assert_eq!(t(&["seek", id, "1m30s"]), 90.0);
    assert_eq!(t(&["forward", id]), 100.0);
    assert_eq!(t(&["forward", id, "30"]), 130.0);
    assert_eq!(t(&["back", id]), 120.0);
    assert_eq!(t(&["back", id, "1:00"]), 60.0);

    // subtitles
    let list = env.ok(&["subtitles", id, "list"]);
    assert_eq!(player(&list, "tracks").as_array().unwrap().len(), 2);
    assert_eq!(
        player(&env.ok(&["subtitles", id, "off"]), "subtitles"),
        Value::Null
    );
    assert_eq!(player(&env.ok(&["subtitles", id, "on"]), "subtitles"), "en");
    assert_eq!(player(&env.ok(&["subtitles", id, "ja"]), "subtitles"), "ja");
    assert!(
        env.err(&["subtitles", id, "xx"])
            .contains("no subtitles for \"xx\"")
    );

    // audio tracks
    assert_eq!(
        player(&env.ok(&["audio", id, "list"]), "audio_tracks"),
        json!(["en", "ja"])
    );
    assert_eq!(player(&env.ok(&["audio", id, "ja"]), "audio"), "ja");
    assert!(env.err(&["audio", id, "fr"]).contains("no audio track"));

    // volume / mute / speed / quality
    assert_eq!(player(&env.ok(&["volume", id, "30"]), "volume"), 30);
    assert_eq!(player(&env.ok(&["mute", id]), "muted"), true);
    assert_eq!(player(&env.ok(&["unmute", id]), "muted"), false);
    assert_eq!(player(&env.ok(&["speed", id, "1.5"]), "playback_rate"), 1.5);
    assert_eq!(
        player(&env.ok(&["quality", id, "hd1080"]), "quality"),
        "hd1080"
    );
    assert!(
        env.err(&["quality", id, "hd2160"])
            .contains("not available")
    );

    // screenshot
    let shot = env.path("shot.png");
    let s = env.ok(&["screenshot", id, shot.to_str().unwrap()]);
    assert_eq!(s["path"], shot.to_str().unwrap());
    assert!(is_png(&shot), "screenshot is not a PNG");

    // list and close
    let list = env.ok(&["list"]);
    let players = list["players"].as_array().unwrap();
    assert_eq!(players.len(), 1);
    assert_eq!(players[0]["id"], id);
    assert_eq!(players[0]["backend"], backend);
    assert_eq!(players[0]["service"], "fake");
    assert_eq!(players[0]["title"], "Fake video");
    assert_eq!(env.ok(&["close", id]), json!({ "id": id, "closed": true }));
    assert!(env.err(&["status", id]).contains("no player"));
    assert_eq!(env.ok(&["list"]), json!({ "players": [] }));
}

/// Window size, position, every layout preset, fullscreen and always-on-top.
fn window_suite(backend: &str) {
    let env = Env::new();
    env.up();
    let v = open(&env, backend, &[]);
    let id = v["id"].as_str().unwrap().to_string();
    let id = id.as_str();
    // Default: 960x540 video area, centered.
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
    let v = env.ok(&["window", id, "--size", "800x450", "--position", "40,60"]);
    assert_eq!(f(&v["window"]["size"]["width"]), 800.0);
    assert_eq!(f(&v["window"]["size"]["height"]), 450.0);
    assert_eq!(f(&v["window"]["position"]["x"]), 40.0);
    assert_eq!(f(&v["window"]["position"]["y"]), 60.0);

    // every region preset
    let (hw, hh) = (w / 2.0, h / 2.0);
    for (layout, frame) in [
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
        let v = env.ok(&["window", id, "--layout", layout]);
        assert_frame(&v, frame, layout);
    }

    // anchored size (picture-in-picture style) and center
    let v = env.ok(&[
        "window",
        id,
        "--layout",
        "bottom-right",
        "--size",
        "480x270",
    ]);
    assert_eq!(f(&v["window"]["size"]["width"]), 480.0);
    assert_eq!(f(&v["window"]["size"]["height"]), 270.0);
    let (ow, oh) = (
        f(&v["window"]["outer_size"]["width"]),
        f(&v["window"]["outer_size"]["height"]),
    );
    assert_frame(&v, (x + w - ow, y + h - oh, ow, oh), "bottom-right 480x270");
    let v = env.ok(&["window", id, "--layout", "center"]);
    assert_frame(
        &v,
        (x + (w - ow) / 2.0, y + (h - oh) / 2.0, ow, oh),
        "center",
    );

    // fullscreen on / off / toggle, and a layout leaves fullscreen
    env.ok(&["fullscreen", id, "on"]);
    eventually("fullscreen on", Duration::from_secs(5), || {
        env.ok(&["status", id])["window"]["fullscreen"] == true
    });
    env.ok(&["fullscreen", id, "off"]);
    eventually("fullscreen off", Duration::from_secs(5), || {
        env.ok(&["status", id])["window"]["fullscreen"] == false
    });
    std::thread::sleep(Duration::from_secs(1));
    env.ok(&["fullscreen", id]);
    eventually("fullscreen toggled on", Duration::from_secs(5), || {
        env.ok(&["status", id])["window"]["fullscreen"] == true
    });
    std::thread::sleep(Duration::from_secs(1));
    let v = env.ok(&["window", id, "--layout", "left"]);
    assert_eq!(v["window"]["fullscreen"], false);
    assert_frame(&v, (x, y, hw, h), "left after fullscreen");

    // always-on-top: WebView supports it; Chrome reports it as unsupported.
    if backend == "webview" {
        assert_eq!(
            env.ok(&["window", id, "--on-top", "on"])["window"]["on_top"],
            true
        );
        assert_eq!(
            env.ok(&["window", id, "--on-top", "toggle"])["window"]["on_top"],
            false
        );
    } else {
        assert!(
            env.err(&["window", id, "--on-top", "on"])
                .contains("not supported")
        );
        assert_eq!(
            env.ok(&["window", id, "--on-top", "off"])["window"]["on_top"],
            false
        );
    }

    // open with window options
    let v = open(&env, backend, &["--layout", "top-right"]);
    assert_frame(&v, (x + hw, y, hw, hh), "open --layout top-right");
    let v = open(&env, backend, &["--fullscreen"]);
    let id2 = v["id"].as_str().unwrap().to_string();
    eventually("open --fullscreen", Duration::from_secs(5), || {
        env.ok(&["status", &id2])["window"]["fullscreen"] == true
    });
    env.ok(&["close", &id2]);
}

#[test]
#[ignore = "opens windows; run with --ignored"]
fn webview_playback() {
    playback_suite("webview");
}

#[test]
#[ignore = "opens windows; run with --ignored"]
fn chrome_playback() {
    playback_suite("chrome");
}

#[test]
#[ignore = "opens windows; run with --ignored"]
fn webview_window() {
    window_suite("webview");
}

#[test]
#[ignore = "opens windows; run with --ignored"]
fn chrome_window() {
    window_suite("chrome");
}

#[test]
#[ignore = "opens windows; run with --ignored"]
fn multiple_players_across_backends() {
    let env = Env::new();
    env.up();
    let a = open(&env, "webview", &[]);
    let b = open(&env, "chrome", &[]);
    let c = open(&env, "chrome", &[]);
    let ids: Vec<&str> = [&a, &b, &c]
        .iter()
        .map(|v| v["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["1", "2", "3"]);
    env.ok(&["seek", "1", "10"]);
    env.ok(&["seek", "2", "20"]);
    env.ok(&["seek", "3", "30"]);
    for (id, t) in [("1", 10.0), ("2", 20.0), ("3", 30.0)] {
        assert_eq!(
            f(&player(&env.ok(&["status", id]), "current_time")),
            t,
            "player {id}"
        );
    }
    let list = env.ok(&["list"]);
    let backends: Vec<&str> = list["players"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["backend"].as_str().unwrap())
        .collect();
    assert_eq!(backends, ["webview", "chrome", "chrome"]);
    env.ok(&["close", "2"]);
    assert_eq!(f(&player(&env.ok(&["status", "3"]), "current_time")), 30.0);
    assert_eq!(env.ok(&["server", "status"])["players"], 2);
    // Shutting down closes everything.
    env.ok(&["server", "down"]);
    env.up();
    assert_eq!(env.ok(&["list"]), json!({ "players": [] }));
}

#[test]
#[ignore = "opens windows; run with --ignored"]
fn undeclared_actions_are_refused() {
    let env = Env::new();
    env.up();
    let url = env.url("/v").replace("127.0.0.1", "localhost");
    let w = env.ok(&["open", &url, "--backend", "webview"]);
    let c = env.ok(&["open", &url, "--backend", "chrome"]);
    assert_eq!(
        (w["service"].as_str(), c["service"].as_str()),
        (Some("limited"), Some("limited"))
    );
    let (w, c) = (w["id"].as_str().unwrap(), c["id"].as_str().unwrap());
    // Declared actions work; `status` is always available.
    env.ok(&["play", w]);
    env.ok(&["pause", w]);
    env.ok(&["status", c]);
    env.ok(&["play", c]);
    // Undeclared ones are refused by the server with the supported list.
    let e = env.err(&["seek", w, "10"]);
    assert!(
        e.contains(
            "seek is not supported by limited (Webview backend); supported: status, play, pause"
        ),
        "{e}"
    );
    let e = env.err(&["prev", w]);
    assert!(e.contains("previous is not supported"), "{e}");
    // Backend-specific gaps from [unsupported_actions].
    let e = env.err(&["pause", c]);
    assert!(
        e.contains("pause is not supported by limited (Chrome backend); supported: status, play"),
        "{e}"
    );
    // `plugins` exposes the declarations.
    let plugins = env.ok(&["plugins"]);
    let limited = plugins["plugins"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"] == "limited")
        .unwrap();
    assert_eq!(limited["actions"], json!(["play", "pause"]));
    assert_eq!(
        limited["unsupported_actions"],
        json!({ "chrome": ["pause"] })
    );
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
    let c = open(&env, "chrome", &[]);
    let id = c["id"].as_str().unwrap();
    // Reload so the page starts with the hook installed before its own scripts.
    env.ok(&["__eval", id, "setTimeout(() => location.reload(), 0), 1"]);
    eventually("the hook after reload", Duration::from_secs(10), || {
        env.run(&["__eval", id, "typeof window.__tvctlDuck"]) == Ok(json!("object"))
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
        env.ok(&["__eval", id, script]),
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
    assert_eq!(env.ok(&["__eval", id, faded]), json!([0.8, 0.2]));
}

fn blackout(env: &Env, id: &str) -> bool {
    env.ok(&["status", id])["window"]["blackout"]
        .as_bool()
        .expect("window.blackout")
}

/// A paused player turns black after `TVCTL_BLACKOUT_SECS`; playing again or a
/// player command removes it, and the wait starts again.
fn blackout_suite(backend: &str) {
    let env = Env::new();
    env.run_with_env(&["server", "up"], &[("TVCTL_BLACKOUT_SECS", "1")])
        .unwrap();
    let v = open(&env, backend, &["--paused"]);
    let id = v["id"].as_str().unwrap();
    assert_eq!(v["window"]["blackout"], false);
    let wait = Duration::from_secs(10);
    eventually("blackout while paused", wait, || blackout(&env, id));

    // Playing removes it and keeps it off.
    env.ok(&["play", id]);
    eventually("no blackout after play", wait, || !blackout(&env, id));
    std::thread::sleep(Duration::from_millis(2500));
    assert!(!blackout(&env, id), "black while playing");

    // Any player command removes it; still paused, it comes back.
    env.ok(&["pause", id]);
    eventually("blackout after pause", wait, || blackout(&env, id));
    env.ok(&["seek", id, "+5"]);
    eventually("no blackout after a command", wait, || !blackout(&env, id));
    eventually("blackout again", wait, || blackout(&env, id));

    if backend == "chrome" {
        // Mouse input on the overlay removes it; synthetic moves of a resting
        // pointer (same position) do not.
        let fire = |ty: &str, x: i32| {
            let script = format!(
                "document.getElementById('__tvctl_blackout').dispatchEvent(new MouseEvent('{ty}', \
                 {{bubbles: true, cancelable: true, screenX: {x}, screenY: 100}})), \
                 window.__tvctlBlackout.shown"
            );
            env.ok(&["__eval", id, &script])
        };
        assert_eq!(fire("mousemove", 100), json!(true));
        assert_eq!(fire("mousemove", 101), json!(true));
        assert_eq!(fire("mousemove", 120), json!(false));
        eventually("blackout after the mouse", wait, || blackout(&env, id));
        assert_eq!(fire("mousedown", 120), json!(false));
    }
}

#[test]
#[ignore = "opens windows; run with --ignored"]
fn webview_blackout() {
    blackout_suite("webview");
}

#[test]
#[ignore = "opens windows; run with --ignored"]
fn chrome_blackout() {
    blackout_suite("chrome");
}

#[test]
#[ignore = "opens windows; run with --ignored"]
fn eval_is_chrome_only() {
    let env = Env::new();
    env.up();
    let c = open(&env, "chrome", &[]);
    let w = open(&env, "webview", &[]);
    let cid = c["id"].as_str().unwrap();
    assert_eq!(
        env.ok(&["__eval", cid, "Promise.resolve(6 * 7)"]),
        json!(42)
    );
    assert_eq!(env.ok(&["__eval", cid, "document.title"]), json!(""));
    assert!(
        env.err(&["__eval", cid, "throw new Error('nope')"])
            .contains("nope")
    );
    assert!(
        env.err(&["__eval", w["id"].as_str().unwrap(), "1"])
            .contains("only available for chrome")
    );
}

#[test]
#[ignore = "opens windows; run with --ignored"]
fn chrome_plugin_can_request_navigation() {
    let env = Env::new();
    env.up();
    let url = env.url("/video/2?redirect=1");
    let v = env.ok(&["open", &url, "--backend", "chrome"]);
    let at = player(&v, "url");
    let at = at.as_str().unwrap();
    assert!(at.contains("landed=1") && !at.contains("redirect"), "{at}");
}

#[test]
#[ignore = "opens windows; run with --ignored"]
fn login_switches_to_the_preferred_backend() {
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
    // Logged out: the first backend (webview) is used.
    assert_eq!(env.ok(&["open", &env.url("/v")])["backend"], "webview");
    // The fake login page sets the cookie and closes its window, which ends `auth login`.
    assert_eq!(
        env.ok(&["auth", "login", "fake"]),
        json!({ "service": "fake", "logged_in": true })
    );
    assert_eq!(
        env.ok(&["auth", "status", "fake"])["services"][0]["logged_in"],
        true
    );
    // Logged in: the preferred backend (chrome) is chosen automatically...
    assert_eq!(env.ok(&["open", &env.url("/v")])["backend"], "chrome");
    // ...unless one is forced.
    assert_eq!(
        env.ok(&["open", &env.url("/v"), "--backend", "webview"])["backend"],
        "webview"
    );
    // The state survives a server restart.
    env.ok(&["server", "down"]);
    env.up();
    assert_eq!(env.ok(&["open", &env.url("/v")])["backend"], "chrome");
    assert!(
        env.err(&["auth", "login", "nosuch"])
            .contains("no plugin named")
    );
}

#[test]
#[ignore = "opens windows; run with --ignored"]
fn chrome_window_closed_by_the_user_is_forgotten() {
    let env = Env::new();
    env.up();
    let v = open(&env, "chrome", &[]);
    let id = v["id"].as_str().unwrap().to_string();
    // Close the window from the page itself, as a user closing it would.
    // The page may close before `open` finishes, so its result is not checked.
    let _ = env.run(&["open", &env.url("/login"), "--backend", "chrome"]);
    eventually(
        "login window to close itself",
        Duration::from_secs(10),
        || env.ok(&["list"])["players"].as_array().unwrap().len() == 1,
    );
    assert!(env.ok(&["status", &id])["player"]["loaded"] == true);
}
