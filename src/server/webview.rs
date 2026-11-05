//! Native WebView backend (tao + wry). Owns the GUI event loop on the main thread.
//!
//! Each player is a window whose WebView loads the plugin's `webview.html` from a
//! loopback HTTP server (so embeds get a real origin) and runs the plugin's
//! `window.__tvctl.run(action)` through a small host bridge.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::Arc;
use std::sync::mpsc;
use std::time::Duration;

use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::{Value, json};
use tao::dpi::{LogicalPosition, LogicalSize};
use tao::event::{Event, WindowEvent};
use tao::event_loop::{
    ControlFlow, EventLoop, EventLoopBuilder, EventLoopProxy, EventLoopWindowTarget,
};
use tao::window::{Fullscreen, Window, WindowBuilder, WindowId};
use wry::{WebView, WebViewBuilder};

use super::{Players, Reply};
use crate::blackout;
use crate::client;
use crate::duck;
use crate::layout::{self, Rect};
use crate::plugin::{Plugin, Registry};
use crate::protocol::{Switch, WindowOpts};

/// Main-frame bridge between the host and the plugin's `window.__tvctl`.
const HOST_BRIDGE: &str = r#"(() => {
  if (window.__tvctlHost) return;
  const post = (m) => window.ipc.postMessage(JSON.stringify(m));
  const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
  window.__tvctlHost = {
    run: async (req, action) => {
      // A player command is activity: it removes the blackout.
      if (action.action !== "status" && window.__tvctlBlackout) window.__tvctlBlackout.wake();
      try {
        const end = Date.now() + 15000;
        while (!(window.__tvctl && window.__tvctl.run)) {
          if (Date.now() > end) throw new Error("plugin page did not define window.__tvctl");
          await sleep(50);
        }
        post({ type: "reply", req, ok: true, result: await window.__tvctl.run(action) });
      } catch (e) {
        post({ type: "reply", req, ok: false, error: String((e && e.message) || e) });
      }
    },
  };
  document.addEventListener("DOMContentLoaded", () => post({ type: "ready" }));
})();"#;

pub enum Command {
    Open {
        id: String,
        plugin: Box<Plugin>,
        action: Value,
        window: Box<WindowOpts>,
    },
    Player {
        id: String,
        action: Value,
    },
    Window {
        id: String,
        opts: WindowOpts,
    },
    Close {
        id: String,
    },
    Screenshot {
        id: String,
        path: String,
    },
    /// Runs a script in the main frame of every player page that has loaded.
    RunInAll {
        script: String,
    },
    List,
    Shutdown,
}

enum UserEvent {
    Command(Command, Reply),
    /// Message posted by a player page via `window.ipc.postMessage`.
    Ipc {
        player: String,
        body: String,
    },
}

/// Sends commands to the event loop from other threads.
pub struct Handle {
    proxy: EventLoopProxy<UserEvent>,
}

impl Handle {
    pub fn call(&self, cmd: Command, timeout: Duration) -> Result<Value, String> {
        let (tx, rx) = mpsc::channel();
        self.proxy
            .send_event(UserEvent::Command(cmd, tx))
            .map_err(|_| "server is shutting down".to_string())?;
        rx.recv_timeout(timeout)
            .unwrap_or_else(|_| Err("timed out waiting for the player".to_string()))
    }
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum IpcMessage {
    Ready,
    Reply {
        req: u64,
        ok: bool,
        result: Option<Value>,
        error: Option<String>,
    },
    /// The page turned black or back (`blackout`).
    Blackout {
        shown: bool,
    },
}

struct Player {
    service: String,
    window: Window,
    webview: WebView,
    page_ready: bool,
    /// Scripts queued until the page reports ready.
    queued: Vec<String>,
    on_top: bool,
    /// The page is black (see `blackout`).
    blackout: bool,
    last_status: Value,
}

struct Pending {
    player: String,
    reply: Reply,
}

struct State {
    proxy: EventLoopProxy<UserEvent>,
    players_registry: Arc<Players>,
    http_port: u16,
    players: HashMap<String, Player>,
    windows: HashMap<WindowId, String>,
    pending: HashMap<u64, Pending>,
    next_req: u64,
}

pub struct Loop {
    event_loop: EventLoop<UserEvent>,
    state: State,
}

/// Creates the event loop (must be called on the main thread) and its handle.
pub fn create(registry: Arc<Registry>, players: Arc<Players>) -> Result<(Loop, Handle)> {
    let http = TcpListener::bind("127.0.0.1:0").context("binding page server")?;
    let http_port = http.local_addr()?.port();
    std::thread::spawn(move || serve_plugin_files(http, &registry));

    let event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
    let proxy = event_loop.create_proxy();
    let handle = Handle {
        proxy: proxy.clone(),
    };
    let state = State {
        proxy,
        players_registry: players,
        http_port,
        players: HashMap::new(),
        windows: HashMap::new(),
        pending: HashMap::new(),
        next_req: 1,
    };
    Ok((Loop { event_loop, state }, handle))
}

/// Runs the event loop; never returns.
pub fn run(l: Loop) -> ! {
    let Loop {
        event_loop,
        mut state,
    } = l;
    event_loop.run(move |event, target, control_flow| {
        *control_flow = ControlFlow::Wait;
        match event {
            Event::UserEvent(UserEvent::Command(cmd, reply)) => {
                state.handle(cmd, reply, target, control_flow)
            }
            Event::UserEvent(UserEvent::Ipc { player, body }) => state.handle_ipc(&player, &body),
            Event::WindowEvent {
                window_id,
                event: WindowEvent::CloseRequested,
                ..
            } => {
                if let Some(id) = state.windows.get(&window_id).cloned() {
                    state.close_player(&id);
                }
            }
            Event::LoopDestroyed => {
                let _ = std::fs::remove_file(client::socket_path());
            }
            _ => {}
        }
    })
}

/// Serves `GET /plugins/<name>/<file>` from the plugin registry.
fn serve_plugin_files(listener: TcpListener, registry: &Registry) {
    for mut stream in listener.incoming().flatten() {
        let mut buf = [0u8; 4096];
        let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
        let n = stream.read(&mut buf).unwrap_or(0);
        let head = String::from_utf8_lossy(&buf[..n]);
        let path = head.split_whitespace().nth(1).unwrap_or("/");
        let path = path.split('?').next().unwrap_or(path);
        let body = path
            .strip_prefix("/plugins/")
            .and_then(|rest| rest.split_once('/'))
            .and_then(|(name, file)| Some((file, registry.get(name)?.file(file)?)));
        let (status, content_type, body) = match body {
            Some((file, content)) => ("200 OK", content_type(file), content),
            None => ("404 Not Found", "text/plain", "not found"),
        };
        let header = format!(
            "HTTP/1.1 {status}\r\nContent-Type: {content_type}; charset=utf-8\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
            body.len()
        );
        let _ = stream.write_all(header.as_bytes());
        let _ = stream.write_all(body.as_bytes());
    }
}

fn content_type(file: &str) -> &'static str {
    match file.rsplit('.').next() {
        Some("html") => "text/html",
        Some("js") => "text/javascript",
        Some("css") => "text/css",
        Some("json") => "application/json",
        _ => "text/plain",
    }
}

impl State {
    fn handle(
        &mut self,
        cmd: Command,
        reply: Reply,
        target: &EventLoopWindowTarget<UserEvent>,
        control_flow: &mut ControlFlow,
    ) {
        match cmd {
            Command::Shutdown => {
                let ids: Vec<String> = self.players.keys().cloned().collect();
                for id in ids {
                    self.close_player(&id);
                }
                let _ = reply.send(Ok(json!({ "stopped": true })));
                *control_flow = ControlFlow::Exit;
            }
            Command::List => {
                let list: Vec<Value> = self
                    .players
                    .iter()
                    .map(|(id, p)| {
                        json!({
                            "id": id,
                            "service": p.service,
                            "backend": "webview",
                            "title": p.last_status.get("title"),
                            "window": window_info(p),
                        })
                    })
                    .collect();
                let _ = reply.send(Ok(Value::Array(list)));
            }
            Command::Open {
                id,
                plugin,
                action,
                window,
            } => match self.create_player(target, &id, &plugin, &window) {
                Ok(()) => self.dispatch(&id, action, reply),
                Err(e) => {
                    self.players_registry.remove(&id);
                    let _ = reply.send(Err(format!("{e:#}")));
                }
            },
            Command::Player { id, action } => {
                if self.players.contains_key(&id) {
                    self.dispatch(&id, action, reply);
                } else {
                    let _ = reply.send(Err(format!("no player with id {id:?}")));
                }
            }
            Command::Window { id, opts } => {
                let Some(player) = self.players.get_mut(&id) else {
                    let _ = reply.send(Err(format!("no player with id {id:?}")));
                    return;
                };
                // Leaving native fullscreen is animated on macOS; resizing during the
                // animation is ignored, so apply geometry once it has finished.
                if opts.changes_geometry()
                    && player.window.fullscreen().is_some()
                    && opts.fullscreen != Some(Switch::On)
                {
                    player.window.set_fullscreen(None);
                    let proxy = self.proxy.clone();
                    let opts = WindowOpts {
                        fullscreen: None,
                        ..opts
                    };
                    std::thread::spawn(move || {
                        std::thread::sleep(Duration::from_millis(900));
                        let _ = proxy
                            .send_event(UserEvent::Command(Command::Window { id, opts }, reply));
                    });
                    return;
                }
                apply_window_opts(player, &opts);
                self.dispatch(&id, json!({ "action": "status" }), reply);
            }
            Command::Screenshot { id, path } => {
                let Some(player) = self.players.get(&id) else {
                    let _ = reply.send(Err(format!("no player with id {id:?}")));
                    return;
                };
                let window_number = native_window_number(&player.window);
                // screencapture blocks; keep the event loop responsive.
                std::thread::spawn(move || {
                    let _ = reply.send(capture_window(window_number, &id, &path));
                });
            }
            Command::RunInAll { script } => {
                let mut count = 0;
                for player in self.players.values().filter(|p| p.page_ready) {
                    run_script(player, &script);
                    count += 1;
                }
                let _ = reply.send(Ok(json!({ "players": count })));
            }
            Command::Close { id } => {
                if self.players.contains_key(&id) {
                    self.close_player(&id);
                    let _ = reply.send(Ok(json!({ "id": id, "closed": true })));
                } else {
                    let _ = reply.send(Err(format!("no player with id {id:?}")));
                }
            }
        }
    }

    fn create_player(
        &mut self,
        target: &EventLoopWindowTarget<UserEvent>,
        id: &str,
        plugin: &Plugin,
        opts: &WindowOpts,
    ) -> Result<()> {
        let (w, h) = opts.size.unwrap_or((960.0, 540.0));
        let window = WindowBuilder::new()
            .with_title(format!("tvctl [{id}] {}", plugin.name()))
            .with_inner_size(LogicalSize::new(w, h))
            .with_visible(false)
            .build(target)
            .context("creating window")?;
        let proxy = self.proxy.clone();
        let ipc_id = id.to_string();
        let mut builder = WebViewBuilder::new()
            .with_url(format!(
                "http://127.0.0.1:{}/plugins/{}/webview.html",
                self.http_port,
                plugin.name()
            ))
            .with_autoplay(true)
            .with_background_color((0, 0, 0, 255))
            .with_initialization_script(HOST_BRIDGE)
            // The ducking hook, in every frame (`tvctl duck`).
            .with_initialization_script_for_main_only(duck::PAGE_SCRIPT, false)
            // Black after a while paused or ended.
            .with_initialization_script_for_main_only(blackout::page_script(), true)
            .with_ipc_handler(move |req| {
                let _ = proxy.send_event(UserEvent::Ipc {
                    player: ipc_id.clone(),
                    body: req.body().clone(),
                });
            });
        if let Some(frame_js) = plugin.file("frame.js") {
            builder = builder.with_initialization_script_for_main_only(frame_js, false);
        }
        let webview = builder.build(&window).context("creating webview")?;
        let mut player = Player {
            service: plugin.name().to_string(),
            window,
            webview,
            page_ready: false,
            queued: Vec::new(),
            on_top: false,
            blackout: false,
            last_status: Value::Null,
        };
        if opts.position.is_none() && opts.layout.is_none() {
            apply_window_opts(
                &mut player,
                &WindowOpts {
                    layout: Some(layout::Layout::Center),
                    ..opts.clone()
                },
            );
        } else {
            apply_window_opts(&mut player, opts);
        }
        player.window.set_visible(true);
        player.window.set_focus();
        self.windows.insert(player.window.id(), id.to_string());
        self.players.insert(id.to_string(), player);
        Ok(())
    }

    fn close_player(&mut self, id: &str) {
        if let Some(player) = self.players.remove(id) {
            self.players_registry.remove(id);
            self.windows.remove(&player.window.id());
            for p in self.pending.values() {
                if p.player == id {
                    let _ = p.reply.send(Err(format!("player {id} was closed")));
                }
            }
            self.pending.retain(|_, p| p.player != id);
            drop(player.webview);
            drop(player.window);
        }
    }

    /// Runs an action in the player page; the reply arrives later through IPC.
    fn dispatch(&mut self, id: &str, action: Value, reply: Reply) {
        let req = self.next_req;
        self.next_req += 1;
        self.pending.insert(
            req,
            Pending {
                player: id.to_string(),
                reply,
            },
        );
        let script = format!("window.__tvctlHost.run({req}, {action})");
        let player = self.players.get_mut(id).expect("checked by caller");
        if player.page_ready {
            run_script(player, &script);
        } else {
            player.queued.push(script);
        }
    }

    fn handle_ipc(&mut self, id: &str, body: &str) {
        let msg = match serde_json::from_str::<IpcMessage>(body) {
            Ok(m) => m,
            Err(e) => {
                eprintln!("bad ipc message from player {id}: {e}: {body}");
                return;
            }
        };
        match msg {
            IpcMessage::Ready => {
                if let Some(player) = self.players.get_mut(id) {
                    player.page_ready = true;
                    // A new document starts without the blackout.
                    player.blackout = false;
                    for script in std::mem::take(&mut player.queued) {
                        run_script(player, &script);
                    }
                }
            }
            IpcMessage::Blackout { shown } => {
                if let Some(player) = self.players.get_mut(id) {
                    player.blackout = shown;
                }
            }
            IpcMessage::Reply {
                req,
                ok,
                result,
                error,
            } => {
                let Some(pending) = self.pending.remove(&req) else {
                    return;
                };
                let Some(player) = self.players.get_mut(id) else {
                    return;
                };
                let response = if ok {
                    let result = result.unwrap_or(Value::Null);
                    if result.is_object() {
                        player.last_status = result.clone();
                    }
                    Ok(json!({
                        "id": id,
                        "service": player.service,
                        "backend": "webview",
                        "player": result,
                        "window": window_info(player),
                    }))
                } else {
                    Err(error.unwrap_or_else(|| "player error".into()))
                };
                let _ = pending.reply.send(response);
            }
        }
    }
}

fn run_script(player: &Player, script: &str) {
    if let Err(e) = player.webview.evaluate_script(script) {
        eprintln!("evaluate_script failed: {e}");
    }
}

fn logical_outer(window: &Window) -> (Rect, (f64, f64)) {
    let sf = window.scale_factor();
    let pos: LogicalPosition<f64> = window
        .outer_position()
        .map(|p| p.to_logical(sf))
        .unwrap_or(LogicalPosition::new(0.0, 0.0));
    let outer: LogicalSize<f64> = window.outer_size().to_logical(sf);
    let inner = content_size(window);
    (
        Rect {
            x: pos.x,
            y: pos.y,
            width: outer.width,
            height: outer.height,
        },
        (
            (outer.width - inner.width).max(0.0),
            (outer.height - inner.height).max(0.0),
        ),
    )
}

fn apply_window_opts(player: &mut Player, opts: &WindowOpts) {
    let window = &player.window;
    if let Some(sw) = opts.on_top {
        player.on_top = sw.resolve(player.on_top);
        window.set_always_on_top(player.on_top);
    }
    let (outer, deco) = logical_outer(window);
    if let Some(l) = opts.layout {
        let wa = work_area(window).unwrap_or(outer);
        let r = layout::compute(l, wa, opts.size, (outer.width, outer.height), deco);
        window.set_inner_size(LogicalSize::new(r.width - deco.0, r.height - deco.1));
        window.set_outer_position(LogicalPosition::new(r.x, r.y));
    } else {
        if let Some((w, h)) = opts.size {
            window.set_inner_size(LogicalSize::new(w, h));
        }
        if let Some((x, y)) = opts.position {
            window.set_outer_position(LogicalPosition::new(x, y));
        }
    }
    if let Some(sw) = opts.fullscreen {
        let on = sw.resolve(window.fullscreen().is_some());
        window.set_fullscreen(on.then_some(Fullscreen::Borderless(None)));
    }
}

fn window_info(player: &Player) -> Value {
    let window = &player.window;
    let (outer, _) = logical_outer(window);
    let inner = content_size(window);
    json!({
        "position": { "x": outer.x, "y": outer.y },
        "size": { "width": inner.width, "height": inner.height },
        "outer_size": { "width": outer.width, "height": outer.height },
        "fullscreen": window.fullscreen().is_some(),
        "on_top": player.on_top,
        "blackout": player.blackout,
        "work_area": work_area(window),
        "scale_factor": window.scale_factor(),
    })
}

#[cfg(target_os = "macos")]
fn native_window_number(window: &Window) -> isize {
    use objc2_app_kit::NSWindow;
    use tao::platform::macos::WindowExtMacOS;

    // SAFETY: tao returns a valid NSWindow pointer owned by `window`, used on the main thread.
    let ns_window: &NSWindow = unsafe { &*(window.ns_window() as *const NSWindow) };
    ns_window.windowNumber()
}

/// Captures one window (even when covered) with macOS `screencapture`.
#[cfg(target_os = "macos")]
fn capture_window(window_number: isize, id: &str, path: &str) -> Result<Value, String> {
    let out = std::process::Command::new("screencapture")
        .args(["-x", "-o", &format!("-l{window_number}"), path])
        .output()
        .map_err(|e| format!("running screencapture: {e}"))?;
    let bytes = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    if !out.status.success() || bytes == 0 {
        return Err(format!(
            "screencapture failed (grant Screen Recording permission to the terminal): {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(json!({ "id": id, "path": path, "bytes": bytes }))
}

#[cfg(not(target_os = "macos"))]
fn native_window_number(_window: &Window) -> isize {
    0
}

#[cfg(not(target_os = "macos"))]
fn capture_window(_n: isize, _id: &str, _path: &str) -> Result<Value, String> {
    Err("screenshots of WebView players are only supported on macOS".into())
}

/// Size of the area showing the video.
///
/// On macOS wry replaces the window's content view, so tao's `inner_size` keeps
/// reporting the original (detached) view; ask the window for its content view.
#[cfg(target_os = "macos")]
fn content_size(window: &Window) -> LogicalSize<f64> {
    use objc2_app_kit::NSWindow;
    use tao::platform::macos::WindowExtMacOS;

    // SAFETY: tao returns a valid NSWindow pointer owned by `window`, used on the main thread.
    let ns_window: &NSWindow = unsafe { &*(window.ns_window() as *const NSWindow) };
    match ns_window.contentView() {
        Some(view) => {
            let size = view.frame().size;
            LogicalSize::new(size.width, size.height)
        }
        None => window.inner_size().to_logical(window.scale_factor()),
    }
}

#[cfg(not(target_os = "macos"))]
fn content_size(window: &Window) -> LogicalSize<f64> {
    window.inner_size().to_logical(window.scale_factor())
}

/// Usable area of the window's screen (excluding menu bar and Dock), top-left origin.
#[cfg(target_os = "macos")]
fn work_area(window: &Window) -> Option<Rect> {
    use objc2::MainThreadMarker;
    use objc2_app_kit::{NSScreen, NSWindow};
    use tao::platform::macos::WindowExtMacOS;

    let mtm = MainThreadMarker::new()?;
    // SAFETY: tao returns a valid NSWindow pointer owned by `window`, used on the main thread.
    let ns_window: &NSWindow = unsafe { &*(window.ns_window() as *const NSWindow) };
    let screen = ns_window.screen().or_else(|| NSScreen::mainScreen(mtm))?;
    let primary = NSScreen::screens(mtm).firstObject()?;
    let visible = screen.visibleFrame();
    let primary_height = primary.frame().size.height;
    Some(Rect {
        x: visible.origin.x,
        y: primary_height - (visible.origin.y + visible.size.height),
        width: visible.size.width,
        height: visible.size.height,
    })
}

#[cfg(not(target_os = "macos"))]
fn work_area(window: &Window) -> Option<Rect> {
    let m = window.current_monitor()?;
    let sf = m.scale_factor();
    let pos: LogicalPosition<f64> = m.position().to_logical(sf);
    let size: LogicalSize<f64> = m.size().to_logical(sf);
    Some(Rect {
        x: pos.x,
        y: pos.y,
        width: size.width,
        height: size.height,
    })
}
