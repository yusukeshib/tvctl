//! Harness for end-to-end tests: runs the real `tvctl` binary against an
//! isolated server (own socket, data dir, Chrome profile and plugin dir) and a
//! local HTTP site for the fake plugin.
#![allow(dead_code)]

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

pub struct Env {
    pub dir: tempfile::TempDir,
    /// Data dir (Chrome profile); isolated unless overridden.
    pub data: PathBuf,
    /// Base URL of the local site, e.g. `http://127.0.0.1:1234`.
    pub site: String,
}

const FAKE_JS: &str = include_str!("../fixtures/fake/chrome.js");

impl Env {
    /// Isolated environment with the fake plugin installed.
    pub fn new() -> Env {
        let dir = tempfile::Builder::new().prefix("mc").tempdir().unwrap();
        let site = start_site();
        let plugin = dir.path().join("plugins/fake");
        std::fs::create_dir_all(&plugin).unwrap();
        std::fs::write(plugin.join("chrome.js"), FAKE_JS).unwrap();
        std::fs::write(
            plugin.join("plugin.toml"),
            format!(
                r#"name = "fake"
description = "Fake service for tests"
domains = ["127.0.0.1"]
actions = ["play", "pause", "toggle", "next", "previous", "seek", "subtitles", "audio", "volume", "speed", "quality"]

[auth]
login_url = "{site}/login"
cookie_url = "{site}"
logged_in_cookies = ["fake_login"]
# The fake login page closes its own window; a plain Chrome would keep running.
login_mode = "controlled"
"#
            ),
        )
        .unwrap();
        let data = dir.path().join("data");
        // A second plugin that supports only some actions (on the same site via
        // `localhost`), to check that undeclared actions are refused.
        let limited = dir.path().join("plugins/limited");
        std::fs::create_dir_all(&limited).unwrap();
        std::fs::write(limited.join("chrome.js"), FAKE_JS).unwrap();
        std::fs::write(
            limited.join("plugin.toml"),
            "name = \"limited\"\ndomains = [\"localhost\"]\nactions = [\"play\"]\n",
        )
        .unwrap();
        Env { dir, site, data }
    }

    /// Like `new`, but uses the data dir in `TVCTL_E2E_PROFILE` when set, so
    /// tests can run with a logged-in Chrome profile (e.g. YouTube Premium, no ads).
    pub fn with_profile() -> Env {
        let mut env = Env::new();
        if let Some(dir) = std::env::var_os("TVCTL_E2E_PROFILE") {
            env.data = PathBuf::from(dir);
        }
        env
    }

    pub fn path(&self, rel: &str) -> PathBuf {
        self.dir.path().join(rel)
    }

    /// Runs `tvctl args...`; Ok(stdout JSON) or Err(stderr).
    pub fn run(&self, args: &[&str]) -> Result<Value, String> {
        self.run_with_env(args, &[])
    }

    /// Like `run`, with extra environment variables (e.g. test hooks).
    pub fn run_with_env(&self, args: &[&str], vars: &[(&str, &str)]) -> Result<Value, String> {
        let mut child = Command::new(env!("CARGO_BIN_EXE_tvctl"))
            .args(args)
            .envs(vars.iter().copied())
            .env("TVCTL_SOCKET", self.path("s.sock"))
            .env("TVCTL_DATA_DIR", &self.data)
            .env("TVCTL_PLUGIN_DIR", self.path("plugins"))
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(120);
        while child.try_wait().unwrap().is_none() {
            if Instant::now() > deadline {
                let _ = child.kill();
                panic!("tvctl {args:?} timed out");
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let out = child.wait_with_output().unwrap();
        if out.status.success() {
            let s = String::from_utf8_lossy(&out.stdout);
            Ok(serde_json::from_str(&s)
                .unwrap_or_else(|e| panic!("tvctl {args:?}: bad JSON {e}: {s}")))
        } else {
            Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
        }
    }

    /// Runs a command that must succeed.
    pub fn ok(&self, args: &[&str]) -> Value {
        self.run(args)
            .unwrap_or_else(|e| panic!("tvctl {args:?} failed: {e}"))
    }

    /// Runs a command that must fail; returns the error text.
    pub fn err(&self, args: &[&str]) -> String {
        match self.run(args) {
            Ok(v) => panic!("tvctl {args:?} should fail but returned {v}"),
            Err(e) => e,
        }
    }

    pub fn up(&self) {
        self.ok(&["server", "up"]);
    }

    pub fn url(&self, path: &str) -> String {
        format!("{}{path}", self.site)
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        let _ = self.run(&["server", "down"]);
    }
}

/// Serves the fake site: `/login` sets the login cookie and closes its window;
/// every other path is a plain page.
fn start_site() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            let mut buf = [0u8; 4096];
            let n = stream.read(&mut buf).unwrap_or(0);
            let head = String::from_utf8_lossy(&buf[..n]);
            let path = head.split_whitespace().nth(1).unwrap_or("/").to_string();
            let (extra, body) = if path.starts_with("/login") {
                (
                    "Set-Cookie: fake_login=1; Path=/; Max-Age=3600\r\n",
                    "<html><body>logged in<script>setTimeout(() => window.close(), 300)</script></body></html>",
                )
            } else {
                (
                    "",
                    "<html><body style=\"background:#a27\"><h1>fake site</h1></body></html>",
                )
            };
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\n{extra}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(resp.as_bytes());
        }
    });
    format!("http://{addr}")
}

pub fn f(v: &Value) -> f64 {
    v.as_f64().unwrap_or_else(|| panic!("not a number: {v}"))
}

/// Polls `check` until it returns true (for asynchronous window changes).
pub fn eventually(what: &str, timeout: Duration, mut check: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if check() {
            return;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    panic!("timed out waiting for {what}");
}

pub fn is_png(path: &Path) -> bool {
    std::fs::read(path)
        .map(|b| b.starts_with(b"\x89PNG\r\n\x1a\n") && b.len() > 1000)
        .unwrap_or(false)
}
