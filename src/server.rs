//! The player server: owns the native windows/webviews and serves requests from
//! the Unix socket. Runs the GUI event loop on the main thread.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::mpsc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::{Value, json};
use tao::dpi::{LogicalPosition, LogicalSize};
use tao::event::{Event, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy, EventLoopWindowTarget};
use tao::window::{Fullscreen, Window, WindowBuilder, WindowId};
use wry::{WebView, WebViewBuilder};

use crate::client;
use crate::layout::{self, Rect};
use crate::protocol::{Request, Response, Switch, WindowOpts};

const PLAYER_HTML: &str = include_str!("../assets/player.html");
const EMBED_JS: &str = include_str!("../assets/embed.js");

type Reply = mpsc::Sender<Result<Value, String>>;

enum UserEvent {
    Request(Request, Reply),
    /// Message posted by a player page via `window.ipc.postMessage`.
    Ipc {
        player: String,
        body: String,
    },
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
}

struct Player {
    window: Window,
    webview: WebView,
    page_ready: bool,
    /// Scripts queued until the page reports ready.
    queued: Vec<String>,
    on_top: bool,
    last_status: Value,
}

struct Pending {
    player: String,
    reply: Reply,
}

struct Server {
    proxy: EventLoopProxy<UserEvent>,
    http_port: u16,
    players: HashMap<String, Player>,
    windows: HashMap<WindowId, String>,
    pending: HashMap<u64, Pending>,
    next_req: u64,
    next_player: u64,
}

pub fn run() -> Result<()> {
    let socket = client::socket_path();
    if client::is_running() {
        bail!("server already running at {}", socket.display());
    }
    let _ = std::fs::remove_file(&socket);
    let listener =
        UnixListener::bind(&socket).with_context(|| format!("binding {}", socket.display()))?;

    let http = TcpListener::bind("127.0.0.1:0").context("binding page server")?;
    let http_port = http.local_addr()?.port();
    std::thread::spawn(move || serve_page(http));

    let event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
    let proxy = event_loop.create_proxy();

    let accept_proxy = proxy.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let proxy = accept_proxy.clone();
            std::thread::spawn(move || handle_connection(stream, proxy));
        }
    });

    eprintln!(
        "play server listening on {} (pid {})",
        socket.display(),
        std::process::id()
    );
    let mut server = Server {
        proxy,
        http_port,
        players: HashMap::new(),
        windows: HashMap::new(),
        pending: HashMap::new(),
        next_req: 1,
        next_player: 1,
    };
    event_loop.run(move |event, target, control_flow| {
        *control_flow = ControlFlow::Wait;
        match event {
            Event::UserEvent(UserEvent::Request(req, reply)) => {
                server.handle_request(req, reply, target, control_flow)
            }
            Event::UserEvent(UserEvent::Ipc { player, body }) => server.handle_ipc(&player, &body),
            Event::WindowEvent {
                window_id,
                event: WindowEvent::CloseRequested,
                ..
            } => {
                if let Some(id) = server.windows.get(&window_id).cloned() {
                    server.close_player(&id);
                }
            }
            Event::LoopDestroyed => {
                let _ = std::fs::remove_file(client::socket_path());
            }
            _ => {}
        }
    })
}

/// Serves the player page over HTTP so the YouTube embed gets a real origin/referrer.
fn serve_page(listener: TcpListener) {
    for mut stream in listener.incoming().flatten() {
        let mut buf = [0u8; 4096];
        let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
        let _ = stream.read(&mut buf);
        let header = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
            PLAYER_HTML.len()
        );
        let _ = stream.write_all(header.as_bytes());
        let _ = stream.write_all(PLAYER_HTML.as_bytes());
    }
}

fn handle_connection(stream: UnixStream, proxy: EventLoopProxy<UserEvent>) {
    let mut line = String::new();
    let Ok(read) = stream.try_clone() else { return };
    if BufReader::new(read).read_line(&mut line).is_err() {
        return;
    }
    let result = match serde_json::from_str::<Request>(&line) {
        Ok(req) => {
            let timeout = match req {
                Request::Open { .. } => Duration::from_secs(40),
                _ => Duration::from_secs(15),
            };
            let (tx, rx) = mpsc::channel();
            if proxy.send_event(UserEvent::Request(req, tx)).is_err() {
                Err("server is shutting down".to_string())
            } else {
                rx.recv_timeout(timeout)
                    .unwrap_or_else(|_| Err("timed out waiting for the player".to_string()))
            }
        }
        Err(e) => Err(format!("bad request: {e}")),
    };
    let mut stream = stream;
    let mut out = serde_json::to_string(&Response::from(result)).expect("serializable");
    out.push('\n');
    let _ = stream.write_all(out.as_bytes());
}

impl Server {
    fn handle_request(
        &mut self,
        req: Request,
        reply: Reply,
        target: &EventLoopWindowTarget<UserEvent>,
        control_flow: &mut ControlFlow,
    ) {
        match req {
            Request::Ping => {
                let _ = reply.send(Ok(json!({
                    "pid": std::process::id(),
                    "socket": client::socket_path(),
                    "log": client::log_path(),
                    "players": self.players.len(),
                })));
            }
            Request::Shutdown => {
                let ids: Vec<String> = self.players.keys().cloned().collect();
                for id in ids {
                    self.close_player(&id);
                }
                let _ = reply.send(Ok(json!({ "stopped": true })));
                *control_flow = ControlFlow::Exit;
            }
            Request::List => {
                let mut list: Vec<Value> = self
                    .players
                    .iter()
                    .map(|(id, p)| {
                        json!({
                            "id": id,
                            "title": p.last_status.get("title"),
                            "video_id": p.last_status.get("video_id"),
                            "window": window_info(p),
                        })
                    })
                    .collect();
                list.sort_by_key(|v| v["id"].as_str().and_then(|s| s.parse::<u64>().ok()));
                let _ = reply.send(Ok(json!({ "players": list })));
            }
            Request::Open {
                video_id,
                start,
                paused,
                subtitles,
                window,
            } => match self.create_player(target, &window) {
                Ok(id) => {
                    let action = json!({
                        "action": "open",
                        "video_id": video_id,
                        "start": start,
                        "paused": paused,
                        "subtitles": subtitles,
                    });
                    self.dispatch(&id, action, reply);
                }
                Err(e) => {
                    let _ = reply.send(Err(format!("{e:#}")));
                }
            },
            Request::Player { id, action } => {
                if self.players.contains_key(&id) {
                    self.dispatch(
                        &id,
                        serde_json::to_value(action).expect("serializable"),
                        reply,
                    );
                } else {
                    let _ = reply.send(Err(self.unknown(&id)));
                }
            }
            Request::Window { id, opts } => {
                let Some(player) = self.players.get_mut(&id) else {
                    let _ = reply.send(Err(self.unknown(&id)));
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
                            .send_event(UserEvent::Request(Request::Window { id, opts }, reply));
                    });
                    return;
                }
                apply_window_opts(player, &opts);
                self.dispatch(&id, json!({ "action": "status" }), reply);
            }
            Request::Close { id } => {
                if self.players.contains_key(&id) {
                    self.close_player(&id);
                    let _ = reply.send(Ok(json!({ "id": id, "closed": true })));
                } else {
                    let _ = reply.send(Err(self.unknown(&id)));
                }
            }
        }
    }

    fn unknown(&self, id: &str) -> String {
        let mut ids: Vec<&String> = self.players.keys().collect();
        ids.sort();
        format!("no player with id {id:?} (open players: {ids:?})")
    }

    fn create_player(
        &mut self,
        target: &EventLoopWindowTarget<UserEvent>,
        opts: &WindowOpts,
    ) -> Result<String> {
        let id = self.next_player.to_string();
        self.next_player += 1;
        let (w, h) = opts.size.unwrap_or((960.0, 540.0));
        let window = WindowBuilder::new()
            .with_title(format!("play [{id}]"))
            .with_inner_size(LogicalSize::new(w, h))
            .with_visible(false)
            .build(target)
            .context("creating window")?;
        let proxy = self.proxy.clone();
        let ipc_id = id.clone();
        let webview = WebViewBuilder::new()
            .with_url(format!("http://127.0.0.1:{}/?player={id}", self.http_port))
            .with_autoplay(true)
            .with_initialization_script_for_main_only(EMBED_JS, false)
            .with_background_color((0, 0, 0, 255))
            .with_ipc_handler(move |req| {
                let _ = proxy.send_event(UserEvent::Ipc {
                    player: ipc_id.clone(),
                    body: req.body().clone(),
                });
            })
            .build(&window)
            .context("creating webview")?;
        let mut player = Player {
            window,
            webview,
            page_ready: false,
            queued: Vec::new(),
            on_top: false,
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
        self.windows.insert(player.window.id(), id.clone());
        self.players.insert(id.clone(), player);
        Ok(id)
    }

    fn close_player(&mut self, id: &str) {
        if let Some(player) = self.players.remove(id) {
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
        let script = format!("window.__play.run({req}, {action})");
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
                    for script in std::mem::take(&mut player.queued) {
                        run_script(player, &script);
                    }
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
                    if result.get("loaded").is_some() {
                        player.last_status = result.clone();
                    }
                    Ok(json!({ "id": id, "player": result, "window": window_info(player) }))
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
    let sf = window.scale_factor();
    let (outer, _) = logical_outer(window);
    let inner = content_size(window);
    json!({
        "position": { "x": outer.x, "y": outer.y },
        "size": { "width": inner.width, "height": inner.height },
        "outer_size": { "width": outer.width, "height": outer.height },
        "fullscreen": window.fullscreen().is_some(),
        "on_top": player.on_top,
        "work_area": work_area(window),
        "scale_factor": sf,
    })
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
