//! Talking to the server over its Unix socket, and starting/stopping it.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};

use crate::protocol::{Request, Response};

/// Socket path; override with `TVCTL_SOCKET`.
pub fn socket_path() -> PathBuf {
    std::env::var_os("TVCTL_SOCKET")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("tvctl.sock"))
}

/// Server log path (next to the socket).
pub fn log_path() -> PathBuf {
    socket_path().with_extension("log")
}

fn connect() -> std::io::Result<UnixStream> {
    UnixStream::connect(socket_path())
}

pub fn is_running() -> bool {
    connect().is_ok()
}

/// Sends a request and returns the result value, turning server errors into `Err`.
pub fn send(req: &Request) -> Result<Value> {
    send_to(&socket_path(), req)
}

fn send_to(socket: &Path, req: &Request) -> Result<Value> {
    let stream = UnixStream::connect(socket)
        .map_err(|_| anyhow!("tvctl server is not running (start it with `tvctl server up`)"))?;
    let mut writer = stream.try_clone()?;
    serde_json::to_writer(&mut writer, req)?;
    writer.write_all(b"\n")?;
    writer.flush()?;
    let mut line = String::new();
    BufReader::new(stream)
        .read_line(&mut line)
        .context("reading server response")?;
    if line.is_empty() {
        bail!("server closed the connection without a response");
    }
    let resp: Response = serde_json::from_str(&line).context("parsing server response")?;
    if resp.ok {
        Ok(resp.result.unwrap_or(Value::Null))
    } else {
        Err(anyhow!(
            resp.error.unwrap_or_else(|| "unknown error".into())
        ))
    }
}

/// `server up`: starts a persistent server (no idle shutdown), or makes the
/// running one persistent.
pub fn server_up() -> Result<Value> {
    if is_running() {
        let mut info = send(&Request::SetPersistent { persistent: true })?;
        info["already_running"] = json!(true);
        return Ok(info);
    }
    start(true)?;
    let mut info = send(&Request::Ping)?;
    info["already_running"] = json!(false);
    Ok(info)
}

/// Starts the server in the background and waits until it accepts connections.
fn start(persistent: bool) -> Result<()> {
    let _ = std::fs::remove_file(socket_path());
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path())
        .context("opening server log")?;
    let mut cmd = Command::new(std::env::current_exe()?);
    cmd.arg("__serve");
    if persistent {
        cmd.arg("--persistent");
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(log)
        .process_group(0)
        .spawn()
        .context("spawning server")?;
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if is_running() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    bail!("server did not start; see {}", log_path().display())
}

/// Sends a request, starting a (non-persistent) server first if none is running.
/// A request that reaches a server in the middle of its idle shutdown is retried
/// once on a fresh server.
pub fn send_autostart(req: &Request) -> Result<Value> {
    for attempt in 0..2 {
        if !is_running() {
            start(false)?;
        }
        match send(req) {
            Err(e) if attempt == 0 && is_shutdown_race(&e) => {
                let deadline = Instant::now() + Duration::from_secs(5);
                while is_running() && Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
            other => return other,
        }
    }
    unreachable!("the second attempt always returns")
}

fn is_shutdown_race(e: &anyhow::Error) -> bool {
    let msg = format!("{e:#}");
    msg.contains("shutting down")
        || msg.contains("without a response")
        || msg.contains("is not running")
}

/// `list` without a server: there are no players.
pub fn list() -> Result<Value> {
    if is_running() {
        send(&Request::List)
    } else {
        Ok(json!({ "players": [] }))
    }
}

pub fn server_down() -> Result<Value> {
    if !is_running() {
        return Ok(json!({ "running": false, "was_running": false }));
    }
    send(&Request::Shutdown)?;
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline && is_running() {
        std::thread::sleep(Duration::from_millis(50));
    }
    Ok(json!({ "running": is_running(), "was_running": true }))
}

/// `duck on/off`: saves the settings and applies them to a running server.
pub fn duck_set(config: crate::duck::Config) -> Result<Value> {
    crate::duck::save(&config)?;
    if is_running() {
        send(&Request::Duck { config })
    } else {
        Ok(crate::duck::offline_status(&config))
    }
}

pub fn duck_status() -> Result<Value> {
    if is_running() {
        send(&Request::DuckStatus)
    } else {
        Ok(crate::duck::offline_status(&crate::duck::load()))
    }
}

pub fn duck_apps() -> Result<Value> {
    if is_running() {
        send(&Request::DuckApps)
    } else {
        Ok(crate::duck::apps(None))
    }
}

pub fn server_status() -> Result<Value> {
    if is_running() {
        let mut info = send(&Request::Ping)?;
        info["running"] = json!(true);
        Ok(info)
    } else {
        Ok(json!({ "running": false, "socket": socket_path() }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;

    /// One-shot fake server: checks the request line and answers with `reply`.
    fn serve_once(
        reply: &'static str,
    ) -> (tempfile::TempDir, PathBuf, std::thread::JoinHandle<String>) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let handle = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut line = String::new();
            BufReader::new(stream.try_clone().unwrap())
                .read_line(&mut line)
                .unwrap();
            let mut stream = stream;
            stream.write_all(reply.as_bytes()).unwrap();
            line
        });
        (dir, path, handle)
    }

    #[test]
    fn ok_response() {
        let (_d, path, h) = serve_once("{\"ok\":true,\"result\":{\"x\":1}}\n");
        let v = send_to(&path, &Request::List).unwrap();
        assert_eq!(v["x"], 1);
        assert_eq!(h.join().unwrap().trim(), "{\"cmd\":\"list\"}");
    }

    #[test]
    fn error_response() {
        let (_d, path, _h) = serve_once("{\"ok\":false,\"error\":\"nope\"}\n");
        assert_eq!(
            send_to(&path, &Request::Ping).unwrap_err().to_string(),
            "nope"
        );
    }

    #[test]
    fn empty_and_garbage_responses() {
        let (_d, path, _h) = serve_once("");
        assert!(
            send_to(&path, &Request::Ping)
                .unwrap_err()
                .to_string()
                .contains("without a response")
        );
        let (_d, path, _h) = serve_once("garbage\n");
        assert!(
            send_to(&path, &Request::Ping)
                .unwrap_err()
                .to_string()
                .contains("parsing")
        );
    }

    #[test]
    fn no_server() {
        let e = send_to(Path::new("/nonexistent/tvctl.sock"), &Request::Ping).unwrap_err();
        assert!(e.to_string().contains("server up"), "{e}");
    }
}
