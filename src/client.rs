//! Talking to the server over its Unix socket, and starting/stopping it.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};

use crate::protocol::{Request, Response};

/// Socket path; override with `PLAY_SOCKET`.
pub fn socket_path() -> PathBuf {
    std::env::var_os("PLAY_SOCKET")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("play.sock"))
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
    let stream = connect()
        .map_err(|_| anyhow!("play server is not running (start it with `play server up`)"))?;
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

pub fn server_up() -> Result<Value> {
    if is_running() {
        let mut info = send(&Request::Ping)?;
        info["already_running"] = json!(true);
        return Ok(info);
    }
    let _ = std::fs::remove_file(socket_path());
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path())
        .context("opening server log")?;
    Command::new(std::env::current_exe()?)
        .arg("__serve")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(log)
        .process_group(0)
        .spawn()
        .context("spawning server")?;
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if is_running() {
            let mut info = send(&Request::Ping)?;
            info["already_running"] = json!(false);
            return Ok(info);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    bail!("server did not start; see {}", log_path().display())
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

pub fn server_status() -> Result<Value> {
    if is_running() {
        let mut info = send(&Request::Ping)?;
        info["running"] = json!(true);
        Ok(info)
    } else {
        Ok(json!({ "running": false, "socket": socket_path() }))
    }
}
