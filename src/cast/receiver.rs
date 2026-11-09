//! The receiver: keeps the Lounge session and DIAL running and turns sender
//! messages into tvctl player commands, reporting the player's state back.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use serde_json::{Value, json};

use super::dial::{self, Device, Dial};
use super::lounge::{Lounge, Message, Screen, generate_screen_id};
use super::{Cast, Config, Host, HostError, Identity, load_identity, save_identity};
use crate::protocol::PlayerAction;

/// How often the player is checked.
const TICK: Duration = Duration::from_secs(1);
/// A reported position this far from where playback should be counts as a seek.
const POSITION_SLACK: f64 = 3.0;

// Player states in the Lounge protocol.
const IDLE: i64 = -1;
const PLAYING: i64 = 1;
const PAUSED: i64 = 2;
const LOADING: i64 = 3;
const STOPPED: i64 = 4;

pub(super) enum Event {
    Messages(u64, Vec<Message>),
    PollEnded(u64, String),
    Paired,
    /// `next`/`prev` from the command line on the cast video: play the queue
    /// entry this far away and reply with the player's response.
    Step(isize, Sender<Result<Value, String>>),
}

/// A long-poll thread for one bind session (`generation`).
struct Poll {
    generation: u64,
    alive: Arc<AtomicBool>,
}

impl Drop for Poll {
    fn drop(&mut self) {
        self.alive.store(false, Ordering::SeqCst);
    }
}

fn start_poll(lounge: &Arc<Lounge>, tx: &Sender<Event>, generation: u64) -> Poll {
    let alive = Arc::new(AtomicBool::new(true));
    let (lounge, tx, flag) = (lounge.clone(), tx.clone(), alive.clone());
    std::thread::spawn(move || {
        while flag.load(Ordering::SeqCst) {
            let r = lounge.poll(|m| {
                if flag.load(Ordering::SeqCst) {
                    let _ = tx.send(Event::Messages(generation, m));
                }
            });
            if let Err(e) = r {
                if flag.load(Ordering::SeqCst) {
                    let _ = tx.send(Event::PollEnded(generation, format!("{e:#}")));
                }
                break;
            }
        }
    });
    Poll { generation, alive }
}

/// Starts a Lounge session for the stored screen, creating a screen id if
/// there is none or the stored one stopped working.
fn connect(config: &Config, dir: &Path) -> Result<(Arc<Lounge>, Identity, Vec<Message>, Duration)> {
    let mut id = load_identity(dir);
    let mut fresh = false;
    if id.screen_id.is_empty() {
        id.screen_id = generate_screen_id()?;
        fresh = true;
    }
    save_identity(dir, &id);
    let make = |id: &Identity| {
        Arc::new(Lounge::new(Screen {
            screen_id: id.screen_id.clone(),
            device_id: id.device_id.clone(),
            name: config.name.clone(),
        }))
    };
    let lounge = make(&id);
    match lounge.begin() {
        Ok((messages, refresh)) => Ok((lounge, id, messages, refresh)),
        Err(e) if !fresh => {
            eprintln!("cast: the stored screen id failed ({e:#}); creating a new one");
            id.screen_id = generate_screen_id()?;
            save_identity(dir, &id);
            let lounge = make(&id);
            let (messages, refresh) = lounge.begin()?;
            Ok((lounge, id, messages, refresh))
        }
        Err(e) => Err(e),
    }
}

/// Runs the receiver until `stop()` returns true (Ok) or it cannot start (Err).
pub fn run(
    config: &Config,
    dir: &Path,
    cast: &Cast,
    host: &dyn Host,
    stop: &dyn Fn() -> bool,
) -> Result<()> {
    let (lounge, id, initial, mut refresh) = connect(config, dir)?;
    let (tx, rx) = mpsc::channel::<Event>();
    let device = {
        let lounge = lounge.clone();
        let tx = std::sync::Mutex::new(tx.clone());
        Arc::new(Device {
            uuid: id.uuid.clone(),
            name: config.name.clone(),
            pid: id.device_id.clone(),
            launch: Box::new(move |body| {
                let form = dial::parse_form(body);
                let get = |k: &str| form.iter().find(|(a, _)| a == k).map(|(_, v)| v.as_str());
                let Some(code) = get("pairingCode") else {
                    bail!("no pairingCode in the launch request");
                };
                if let Some(theme) = get("theme")
                    && theme != "cl"
                {
                    bail!("only the YouTube app can cast to tvctl (theme {theme:?})");
                }
                lounge.register_pairing_code(code)?;
                let _ = tx.lock().unwrap().send(Event::Paired);
                Ok(())
            }),
        })
    };
    let dial = Dial::start(device)?;
    {
        let mut inner = cast.inner.lock().unwrap();
        inner.lounge = Some(lounge.clone());
        inner.commands = Some(tx.clone());
        inner.status = super::Status {
            running: true,
            dial_port: Some(dial.port()),
            ..Default::default()
        };
    }
    eprintln!(
        "cast: receiver {:?} running (DIAL port {})",
        config.name,
        dial.port()
    );

    let mut generation = 0;
    let mut poll = start_poll(&lounge, &tx, generation);
    let mut began = Instant::now();
    let mut player = Controller::new(host, lounge.clone());
    player.handle(initial);
    player.publish(cast);
    let mut retry: Option<(Instant, Duration)> = None;

    while !stop() {
        // A new bind session after the poll failed or the token is due.
        let due = retry.is_some_and(|(at, _)| Instant::now() >= at) || began.elapsed() >= refresh;
        if due {
            match lounge.begin() {
                Ok((messages, r)) => {
                    refresh = r;
                    began = Instant::now();
                    generation += 1;
                    poll = start_poll(&lounge, &tx, generation);
                    retry = None;
                    cast.update(|s| s.error = None);
                    player.handle(messages);
                }
                Err(e) => {
                    let wait = retry.map_or(Duration::from_secs(5), |(_, w)| {
                        (w * 2).min(Duration::from_secs(60))
                    });
                    eprintln!("cast: reconnecting failed: {e:#}; retrying in {wait:?}");
                    cast.update(|s| s.error = Some(format!("reconnecting: {e:#}")));
                    retry = Some((Instant::now() + wait, wait));
                }
            }
        }
        match rx.recv_timeout(TICK) {
            Ok(Event::Messages(g, m)) if g == poll.generation => player.handle(m),
            Ok(Event::PollEnded(g, e)) if g == poll.generation && retry.is_none() => {
                eprintln!("cast: lounge connection ended: {e}");
                retry = Some((Instant::now(), Duration::from_secs(2)));
            }
            Ok(Event::Paired) => eprintln!("cast: a sender paired through DIAL"),
            Ok(Event::Step(step, reply)) => {
                let result = player.step(step);
                // Publish the new ownership first, so a following CLI `next` or
                // `prev` is routed to the cast queue too.
                player.publish(cast);
                let _ = reply.send(result);
            }
            Ok(_) => {}
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
        player.tick();
        player.publish(cast);
    }
    cast.inner.lock().unwrap().commands = None;
    drop(poll);
    drop(dial);
    let _ = lounge.send(&[Message::new("loungeScreenDisconnected", json!({}))]);
    Ok(())
}

#[derive(Debug, Clone, PartialEq)]
struct Video {
    id: String,
    index: Option<usize>,
}

#[derive(Debug, Clone)]
struct Remote {
    id: String,
    name: String,
    /// Whether the app's "Disconnect" stops the screen when it is the last
    /// sender (capability `dsdtr`, sent by the YouTube phone apps).
    disconnect_stops: bool,
}

/// `disconnectBehavior` of a sender that left to play the video on itself (e.g.
/// the iOS app when its player is swiped down to the miniplayer).
const PLAY_ON_PHONE: &str = "MDX_SESSION_DISCONNECT_BEHAVIOR_DISCONNECTED_BY_USER_PLAY_ON_PHONE";
/// `disconnectBehavior` of a disconnect the screen asked for.
const SCREEN_INITIATED: &str =
    "MDX_SESSION_DISCONNECT_BEHAVIOR_DISCONNECTED_BY_USER_SCREEN_INITIATED";

/// Turns sender messages into player commands and reports state.
struct Controller<'a> {
    host: &'a dyn Host,
    lounge: Arc<Lounge>,
    senders: Vec<Remote>,
    list_id: Option<String>,
    video_ids: Vec<String>,
    current: Option<Video>,
    /// Token (see `Host`) of `current` in the tvctl player, while it shows it.
    player: Option<u64>,
    status: i64,
    position: f64,
    duration: f64,
    /// When `position` was measured.
    measured: Instant,
    rate: f64,
    volume: (u8, bool),
    cpn: String,
}

fn num(v: &Value) -> Option<f64> {
    v.as_f64().or_else(|| v.as_str()?.parse().ok())
}

impl<'a> Controller<'a> {
    fn new(host: &'a dyn Host, lounge: Arc<Lounge>) -> Self {
        Controller {
            host,
            lounge,
            senders: Vec::new(),
            list_id: None,
            video_ids: Vec::new(),
            current: None,
            player: None,
            status: IDLE,
            position: 0.0,
            duration: 0.0,
            measured: Instant::now(),
            rate: 1.0,
            volume: (100, false),
            cpn: super::lounge::random_uuid().replace('-', "")[..16].to_string(),
        }
    }

    fn publish(&self, cast: &Cast) {
        let senders = self.senders.iter().map(|s| s.name.clone()).collect();
        cast.update(|s| {
            s.senders = senders;
            s.player = self.player;
            s.video_id = self.current.as_ref().map(|v| v.id.clone());
        });
    }

    fn send(&self, messages: Vec<Message>) {
        if let Err(e) = self.lounge.send(&messages) {
            eprintln!(
                "cast: sending {:?}: {e:#}",
                messages.iter().map(|m| &m.name).collect::<Vec<_>>()
            );
        }
    }

    /// Where playback is now (extrapolated while playing).
    fn now_position(&self) -> f64 {
        if self.status == PLAYING {
            (self.position + self.measured.elapsed().as_secs_f64() * self.rate)
                .min(self.duration.max(self.position))
        } else {
            self.position
        }
    }

    fn loaded_time(&self) -> f64 {
        if matches!(self.status, PLAYING | PAUSED | LOADING) {
            self.duration
        } else {
            0.0
        }
    }

    fn on_state_change(&self) -> Message {
        Message::new(
            "onStateChange",
            json!({
                "state": self.status,
                "currentTime": self.now_position(),
                "duration": self.duration,
                "loadedTime": self.loaded_time(),
                "seekableStartTime": 0,
                "seekableEndTime": self.duration,
                "cpn": self.cpn,
            }),
        )
    }

    fn now_playing(&self) -> Message {
        let Some(v) = &self.current else {
            return Message::new("nowPlaying", json!({}));
        };
        let mut p = json!({
            "videoId": v.id,
            "currentTime": self.now_position(),
            "duration": self.duration,
            "cpn": self.cpn,
            "loadedTime": self.loaded_time(),
            "state": self.status,
            "seekableStartTime": 0,
            "seekableEndTime": self.duration,
        });
        if let Some(l) = &self.list_id {
            p["listId"] = json!(l);
        }
        if let Some(i) = v.index {
            p["currentIndex"] = json!(i);
        }
        Message::new("nowPlaying", p)
    }

    fn nav(&self) -> Message {
        Message::new(
            "onHasPreviousNextChanged",
            json!({
                "hasPrevious": self.neighbor(-1).is_some(),
                "hasNext": self.neighbor(1).is_some(),
            }),
        )
    }

    fn volume_message(&self) -> Message {
        Message::new(
            "onVolumeChanged",
            json!({ "volume": self.volume.0, "muted": self.volume.1 }),
        )
    }

    fn autoplay_unsupported() -> Message {
        Message::new(
            "onAutoplayModeChanged",
            json!({ "autoplayMode": "UNSUPPORTED" }),
        )
    }

    /// The queue entry `step` away from the current one.
    fn neighbor(&self, step: isize) -> Option<Video> {
        let i = self.current.as_ref()?.index?;
        let j = i.checked_add_signed(step)?;
        let id = self.video_ids.get(j)?;
        Some(Video {
            id: id.clone(),
            index: Some(j),
        })
    }

    fn handle(&mut self, messages: Vec<Message>) {
        for m in messages {
            super::trace(format_args!("<- {} {}", m.name, m.payload));
            if let Err(e) = self.handle_one(&m) {
                eprintln!("cast: handling {}: {e:#}", m.name);
            }
        }
    }

    fn handle_one(&mut self, m: &Message) -> Result<()> {
        match m.name.as_str() {
            "loungeStatus" => self.lounge_status(m),
            "getNowPlaying" => self.send(vec![self.now_playing()]),
            "setPlaylist" | "updatePlaylist" => self.playlist(m),
            "play" => {
                if self.command(PlayerAction::Play) {
                    self.set_status(PLAYING);
                }
            }
            "pause" => {
                if self.command(PlayerAction::Pause) {
                    self.set_status(PAUSED);
                }
            }
            "seekTo" => {
                if let Some(t) = m.field("newTime").and_then(|s| s.parse::<f64>().ok()) {
                    self.position = t;
                    self.measured = Instant::now();
                    if self.command(PlayerAction::Seek {
                        seconds: t,
                        relative: false,
                    }) {
                        self.send(vec![self.on_state_change()]);
                    }
                }
            }
            "setVolume" => {
                let level = m
                    .field("volume")
                    .and_then(|s| s.parse::<f64>().ok())
                    .map_or(self.volume.0, |v| v.round().clamp(0.0, 100.0) as u8);
                let muted = m
                    .field("muted")
                    .map_or(self.volume.1, |s| s.trim() == "true");
                if (level, muted) != self.volume {
                    self.volume = (level, muted);
                    let _ = self.apply_volume();
                }
                self.send(vec![self.volume_message()]);
            }
            "getVolume" => self.send(vec![self.volume_message()]),
            "next" => {
                if let Some(v) = self.neighbor(1) {
                    let _ = self.play(v, 0.0);
                }
            }
            "previous" => {
                if let Some(v) = self.neighbor(-1) {
                    let _ = self.play(v, 0.0);
                }
            }
            "stopVideo" => {
                eprintln!("cast: stop requested by the app");
                self.stop(true);
            }
            "setAutoplayMode" => self.send(vec![Self::autoplay_unsupported()]),
            _ => {}
        }
        Ok(())
    }

    fn lounge_status(&mut self, m: &Message) {
        let devices: Vec<Value> = m
            .field("devices")
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        let now: Vec<Remote> = devices
            .iter()
            .filter(|d| d["type"] == "REMOTE_CONTROL")
            .filter_map(|d| {
                Some(Remote {
                    id: d["id"].as_str()?.to_string(),
                    name: d["name"]
                        .as_str()
                        .or_else(|| d["clientName"].as_str())
                        .unwrap_or("sender")
                        .to_string(),
                    disconnect_stops: d["capabilities"]
                        .as_str()
                        .is_some_and(|c| c.split(',').any(|c| c == "dsdtr")),
                })
            })
            .collect();
        let event: Value = m
            .field("connectionEventDetails")
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or(Value::Null);
        let device = event["deviceId"].as_str();
        let was = |id: &str| self.senders.iter().any(|s| s.id == id);
        let connected = device.filter(|d| now.iter().any(|s| s.id == *d) && !was(d));
        let disconnected = device
            .filter(|d| !now.iter().any(|s| s.id == *d))
            .and_then(|d| self.senders.iter().find(|s| s.id == d).cloned());
        if let Some(d) = connected {
            let name = now.iter().find(|s| s.id == d).map(|s| s.name.clone());
            eprintln!("cast: connected: {}", name.unwrap_or_default());
        }
        let behavior = event["disconnectBehavior"].as_str().unwrap_or_default();
        if let Some(d) = &disconnected {
            eprintln!(
                "cast: disconnected: {} ({})",
                d.name,
                behavior.trim_start_matches("MDX_SESSION_DISCONNECT_BEHAVIOR_")
            );
        }
        // As YouTube's TV app does: the last sender's "Disconnect" stops the
        // screen, but leaving to play on the phone (or a disconnect the screen
        // asked for) does not, nor does an implicit disconnect (no `ui`).
        let ui = event["ui"].as_str() == Some("true") || event["ui"] == true;
        let stops = disconnected.as_ref().is_some_and(|d| d.disconnect_stops)
            && ui
            && behavior != PLAY_ON_PHONE
            && behavior != SCREEN_INITIATED
            && now.is_empty();
        self.senders = now;
        if stops {
            eprintln!("cast: stop: the last sender disconnected from the app");
            self.stop(true);
            self.list_id = None;
            self.video_ids.clear();
            self.current = None;
            return;
        }
        let mut out = vec![self.nav(), Self::autoplay_unsupported()];
        if connected.is_some() {
            out.push(self.now_playing());
            out.push(self.on_state_change());
            out.push(self.volume_message());
        }
        self.send(out);
    }

    fn playlist(&mut self, m: &Message) {
        let Some(list) = m.field("listId").filter(|s| !s.is_empty()) else {
            self.list_id = None;
            self.video_ids.clear();
            return;
        };
        self.list_id = Some(list);
        self.video_ids = m
            .field("videoIds")
            .map(|s| {
                s.split(',')
                    .filter(|v| !v.is_empty())
                    .map(String::from)
                    .collect()
            })
            .unwrap_or_default();
        if m.name == "setPlaylist" {
            let video = m.field("videoId").map(|id| Video {
                index: m.field("currentIndex").and_then(|s| s.parse().ok()),
                id,
            });
            if let Some(v) = video
                && Some(&v) != self.current.as_ref()
            {
                let start = m
                    .field("currentTime")
                    .and_then(|s| s.parse::<f64>().ok())
                    .unwrap_or(0.0);
                let _ = self.play(v, start);
                return;
            }
        } else if let Some(cur) = &mut self.current {
            match self.video_ids.iter().position(|v| *v == cur.id) {
                Some(i) => cur.index = Some(i),
                None => {
                    eprintln!("cast: stop: the updated queue no longer has {}", cur.id);
                    self.stop(true);
                    self.current = None;
                }
            }
        }
        self.send(vec![self.now_playing(), self.nav()]);
    }

    /// Runs a command on the cast player; false if there is none or it failed.
    fn command(&mut self, action: PlayerAction) -> bool {
        self.command_value(action).is_some()
    }

    /// `command`, returning the player's response.
    fn command_value(&mut self, action: PlayerAction) -> Option<Value> {
        let token = self.player?;
        match self.host.player(token, action) {
            Ok(v) => {
                self.update_from(&v);
                Some(v)
            }
            Err(HostError::Gone) => {
                // Closed, or replaced from the command line.
                self.player = None;
                self.stop(false);
                None
            }
            Err(e) => {
                eprintln!("cast: player: {e}");
                None
            }
        }
    }

    /// Applies the app's volume to the player; returns the player's response.
    fn apply_volume(&mut self) -> Option<Value> {
        let (level, muted) = self.volume;
        self.command_value(PlayerAction::Volume {
            level: Some(level),
            muted: Some(muted),
        })
    }

    fn set_status(&mut self, status: i64) {
        if self.status != status {
            self.status = status;
            self.send(vec![self.on_state_change()]);
        }
    }

    /// Records time, duration and rate from a player response.
    fn update_from(&mut self, v: &Value) {
        let p = &v["player"];
        if let Some(t) = num(&p["current_time"]) {
            self.position = t;
            self.measured = Instant::now();
        }
        if let Some(d) = num(&p["duration"]).filter(|d| *d > 0.0) {
            self.duration = d;
        }
        if let Some(r) = num(&p["playback_rate"]).filter(|r| *r > 0.0) {
            self.rate = r;
        }
    }

    /// Closes the player window if it still shows the cast video.
    fn close_player(&mut self) {
        if let Some(token) = self.player.take() {
            let _ = self.host.close(token);
        }
    }

    fn stop(&mut self, close: bool) {
        if close {
            self.close_player();
        }
        self.status = STOPPED;
        self.position = 0.0;
        self.send(vec![self.on_state_change(), self.now_playing()]);
    }

    /// `next`/`prev` from the command line: like the app's buttons.
    fn step(&mut self, step: isize) -> Result<Value, String> {
        let what = if step > 0 { "next" } else { "previous" };
        let video = self
            .neighbor(step)
            .ok_or_else(|| format!("no {what} video in the cast queue"))?;
        self.advance(video)
    }

    /// Plays the queue's `video` in place of the cast video, but only while the
    /// player still shows it (not over something opened from the command line).
    fn advance(&mut self, video: Video) -> Result<Value, String> {
        match self.player {
            Some(token) => self.play_in(video, 0.0, Some(token)),
            None => Err("the player no longer shows the cast video".into()),
        }
    }

    /// Plays `video` from the app: it takes the player over whatever it shows.
    fn play(&mut self, video: Video, start: f64) -> Result<Value, String> {
        self.play_in(video, start, None)
    }

    /// Plays `video` in the player (replacing what it shows; the window keeps
    /// its place) and returns the player's response. With `expect`, only if the
    /// player still shows that content.
    fn play_in(&mut self, video: Video, start: f64, expect: Option<u64>) -> Result<Value, String> {
        eprintln!("cast: play {} at {start}", video.id);
        self.player = None;
        self.current = Some(video.clone());
        self.status = LOADING;
        self.position = start;
        self.duration = 0.0;
        self.measured = Instant::now();
        self.send(vec![self.on_state_change(), self.now_playing(), self.nav()]);
        let url = format!("https://www.youtube.com/watch?v={}", video.id);
        let result = match self.host.open(&url, start, expect) {
            Ok((token, v)) => {
                self.player = Some(token);
                self.update_from(&v);
                let mut v = v;
                if self.volume != (100, false)
                    && let Some(after) = self.apply_volume()
                {
                    v = after;
                }
                self.status = PLAYING;
                Ok(v)
            }
            Err(HostError::Gone) => {
                // Something else was opened from the command line meanwhile.
                eprintln!("cast: not playing {url}: the player shows something else");
                self.status = STOPPED;
                Err("the player no longer shows the cast video".into())
            }
            Err(e) => {
                eprintln!("cast: opening {url}: {e}");
                self.status = STOPPED;
                Err(format!("opening {url}: {e}"))
            }
        };
        self.send(vec![self.on_state_change(), self.now_playing(), self.nav()]);
        result
    }

    /// Follows the player: state, position and its end.
    fn tick(&mut self) {
        let Some(token) = self.player else {
            return;
        };
        let v = match self.host.player(token, PlayerAction::Status) {
            Ok(v) => v,
            Err(HostError::Gone) => {
                // The window was closed, or something else was opened in it.
                self.player = None;
                self.stop(false);
                return;
            }
            Err(_) => return,
        };
        let expected = self.now_position();
        self.update_from(&v);
        let p = &v["player"];
        let status = match p["state"].as_str() {
            Some("playing") => PLAYING,
            Some("paused") => PAUSED,
            Some("ended") => {
                match self.neighbor(1) {
                    Some(next) => {
                        let _ = self.advance(next);
                    }
                    None => {
                        if self.status != STOPPED {
                            self.stop(false);
                        }
                    }
                }
                return;
            }
            Some("buffering" | "unstarted" | "cued") => LOADING,
            _ => self.status,
        };
        if let Some(level) = num(&p["volume"]).map(|l| l.round() as u8) {
            let muted = p["muted"] == true;
            if (level, muted) != self.volume {
                self.volume = (level, muted);
                self.send(vec![self.volume_message()]);
            }
        }
        let jumped = (self.position - expected).abs() > POSITION_SLACK;
        if status != self.status || jumped {
            self.status = status;
            self.send(vec![self.on_state_change()]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// The one player window: `open` replaces what it shows (new token).
    #[derive(Default)]
    struct FakeHost {
        calls: Mutex<Vec<String>>,
        state: Mutex<String>,
        /// Token of what the player shows; None when the window is closed.
        showing: Mutex<Option<u64>>,
        next: Mutex<u64>,
        /// The page's (level, muted); a new page starts at (100, false).
        volume: Mutex<(u64, bool)>,
    }

    impl FakeHost {
        /// Something else is opened in the player (e.g. from the command line).
        fn replace(&self) {
            let mut next = self.next.lock().unwrap();
            *next += 1;
            *self.showing.lock().unwrap() = Some(*next);
        }

        fn check(&self, token: u64) -> Result<(), HostError> {
            if *self.showing.lock().unwrap() == Some(token) {
                Ok(())
            } else {
                Err(HostError::Gone)
            }
        }

        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl Host for FakeHost {
        fn open(
            &self,
            url: &str,
            start: f64,
            expect: Option<u64>,
        ) -> Result<(u64, Value), HostError> {
            if let Some(token) = expect {
                self.check(token)?;
            }
            self.calls
                .lock()
                .unwrap()
                .push(format!("open {url} {start}"));
            self.replace();
            *self.state.lock().unwrap() = "playing".into();
            *self.volume.lock().unwrap() = (100, false);
            let token = *self.next.lock().unwrap();
            Ok((
                token,
                json!({
                    "player": { "state": "playing", "current_time": start, "duration": 100, "volume": 100, "muted": false },
                }),
            ))
        }
        fn player(&self, token: u64, action: PlayerAction) -> Result<Value, HostError> {
            self.check(token)?;
            let a = serde_json::to_value(&action).unwrap();
            if a["action"] != "status" {
                self.calls.lock().unwrap().push(format!("{token} {a}"));
            }
            let mut volume = self.volume.lock().unwrap();
            if a["action"] == "volume" {
                *volume = (a["level"].as_u64().unwrap(), a["muted"] == true);
            }
            Ok(json!({
                "player": { "state": *self.state.lock().unwrap(), "current_time": 5, "duration": 100, "volume": volume.0, "muted": volume.1 },
            }))
        }
        fn close(&self, token: u64) -> Result<(), HostError> {
            self.check(token)?;
            self.calls.lock().unwrap().push(format!("close {token}"));
            *self.showing.lock().unwrap() = None;
            Ok(())
        }
    }

    fn controller(host: &FakeHost) -> Controller<'_> {
        Controller::new(
            host,
            Arc::new(Lounge::new(Screen {
                screen_id: String::new(),
                device_id: String::new(),
                name: String::new(),
            })),
        )
    }

    fn msg(name: &str, payload: Value) -> Message {
        Message::new(name, payload)
    }

    fn video(id: &str) -> Video {
        Video {
            id: id.into(),
            index: None,
        }
    }

    #[test]
    fn playlist_play_controls_and_queue() {
        let host = FakeHost::default();
        let mut c = controller(&host);
        c.handle(vec![msg(
            "setPlaylist",
            json!({"listId": "RQ1", "videoId": "b", "currentIndex": "1", "currentTime": "12", "videoIds": "a,b,c"}),
        )]);
        assert_eq!(c.player, Some(1));
        assert_eq!(c.status, PLAYING);
        assert_eq!(c.neighbor(1).unwrap().id, "c");
        assert_eq!(c.neighbor(-1).unwrap().id, "a");
        // The same video again does not reopen.
        c.handle(vec![msg(
            "setPlaylist",
            json!({"listId": "RQ1", "videoId": "b", "currentIndex": "1", "videoIds": "a,b,c"}),
        )]);
        c.handle(vec![
            msg("pause", Value::Null),
            msg("seekTo", json!({"newTime": "30"})),
            msg("setVolume", json!({"volume": "40", "muted": "false"})),
            msg("next", Value::Null),
        ]);
        assert_eq!(c.current.as_ref().unwrap().id, "c");
        // The end of the last video stops without closing the window.
        *host.state.lock().unwrap() = "ended".into();
        c.tick();
        assert_eq!(c.status, STOPPED);
        assert_eq!(c.player, Some(2));
        // The next video replaces the previous one in the same window (no close).
        assert_eq!(
            host.calls(),
            vec![
                "open https://www.youtube.com/watch?v=b 12".to_string(),
                r#"1 {"action":"pause"}"#.into(),
                r#"1 {"action":"seek","relative":false,"seconds":30.0}"#.into(),
                r#"1 {"action":"volume","level":40,"muted":false}"#.into(),
                "open https://www.youtube.com/watch?v=c 0".into(),
                r#"2 {"action":"volume","level":40,"muted":false}"#.into(),
            ]
        );
    }

    #[test]
    fn ended_video_advances_and_explicit_disconnect_stops() {
        let host = FakeHost::default();
        let mut c = controller(&host);
        c.handle(vec![msg("loungeStatus", phone_connects())]);
        assert_eq!(c.senders.len(), 1);
        c.handle(vec![msg(
            "setPlaylist",
            json!({"listId": "RQ1", "videoId": "a", "currentIndex": "0", "videoIds": "a,b"}),
        )]);
        *host.state.lock().unwrap() = "ended".into();
        c.tick();
        assert_eq!(c.current.as_ref().unwrap().id, "b");
        let gone =
            json!({"devices": "[]", "connectionEventDetails": r#"{"deviceId":"p1","ui":"true"}"#});
        c.handle(vec![msg("loungeStatus", gone)]);
        assert!(c.senders.is_empty());
        assert!(c.player.is_none());
        assert_eq!(c.status, STOPPED);
        assert_eq!(host.calls().last().unwrap(), "close 2");
    }

    fn phone_connects() -> Value {
        json!({"devices": r#"[{"type":"REMOTE_CONTROL","id":"p1","name":"Phone","theme":"cl","capabilities":"que,dsdtr,mus"}]"#,
               "connectionEventDetails": r#"{"deviceId":"p1"}"#})
    }

    #[test]
    fn leaving_to_play_on_the_phone_keeps_playing() {
        // The iOS app sends this when its player is swiped down to the miniplayer.
        let host = FakeHost::default();
        let mut c = controller(&host);
        c.handle(vec![msg("loungeStatus", phone_connects())]);
        c.handle(vec![msg(
            "setPlaylist",
            json!({"listId": "RQ1", "videoId": "a", "currentIndex": "0", "videoIds": "a"}),
        )]);
        let gone = json!({"devices": "[]", "connectionEventDetails": format!(
            r#"{{"deviceId":"p1","ui":"true","disconnectBehavior":"{PLAY_ON_PHONE}"}}"#
        )});
        c.handle(vec![msg("loungeStatus", gone)]);
        assert!(c.senders.is_empty());
        assert_eq!(c.player, Some(1));
        assert_eq!(c.status, PLAYING);
        assert!(!host.calls().iter().any(|c| c.starts_with("close")));
    }

    #[test]
    fn disconnect_without_dsdtr_keeps_playing() {
        let host = FakeHost::default();
        let mut c = controller(&host);
        let browser = json!({"devices": r#"[{"type":"REMOTE_CONTROL","id":"w1","name":"Chrome","capabilities":"que,mus"}]"#,
                             "connectionEventDetails": r#"{"deviceId":"w1"}"#});
        c.handle(vec![msg("loungeStatus", browser)]);
        c.play(video("x"), 0.0).unwrap();
        let gone =
            json!({"devices": "[]", "connectionEventDetails": r#"{"deviceId":"w1","ui":"true"}"#});
        c.handle(vec![msg("loungeStatus", gone)]);
        assert_eq!(c.player, Some(1));
        assert_eq!(c.status, PLAYING);
    }

    #[test]
    fn closed_window_stops() {
        let host = FakeHost::default();
        let mut c = controller(&host);
        c.play(video("x"), 0.0).unwrap();
        assert_eq!(c.player, Some(1));
        *host.showing.lock().unwrap() = None;
        c.tick();
        assert!(c.player.is_none());
        assert_eq!(c.status, STOPPED);
    }

    #[test]
    fn replaced_content_stops_and_is_left_alone() {
        let host = FakeHost::default();
        let mut c = controller(&host);
        c.play(video("x"), 0.0).unwrap();
        // Something else opened in the player from the command line.
        host.replace();
        c.tick();
        assert!(c.player.is_none());
        assert_eq!(c.status, STOPPED);
        // Commands and Disconnect from the app do not touch the other content.
        c.handle(vec![
            msg("pause", Value::Null),
            msg("stopVideo", Value::Null),
        ]);
        assert_eq!(
            host.calls(),
            vec!["open https://www.youtube.com/watch?v=x 0"]
        );
        assert_eq!(*host.showing.lock().unwrap(), Some(2));
    }

    #[test]
    fn command_line_steps_through_the_queue() {
        let host = FakeHost::default();
        let mut c = controller(&host);
        c.handle(vec![msg(
            "setPlaylist",
            json!({"listId": "RQ1", "videoId": "b", "currentIndex": "1", "videoIds": "a,b,c"}),
        )]);
        let v = c.step(1).unwrap();
        assert_eq!(v["player"]["state"], "playing");
        assert_eq!(c.current.as_ref().unwrap().id, "c");
        assert_eq!(c.player, Some(2));
        let e = c.step(1).unwrap_err();
        assert_eq!(e, "no next video in the cast queue");
        c.step(-1).unwrap();
        assert_eq!(c.current.as_ref().unwrap().id, "b");
        assert_eq!(
            host.calls().last().unwrap(),
            "open https://www.youtube.com/watch?v=b 0"
        );
    }

    #[test]
    fn queue_advance_does_not_replace_other_content() {
        let host = FakeHost::default();
        let mut c = controller(&host);
        c.handle(vec![msg(
            "setPlaylist",
            json!({"listId": "RQ1", "videoId": "a", "currentIndex": "0", "videoIds": "a,b,c"}),
        )]);
        // Something else is opened from the command line before the receiver
        // notices; neither the end of the video nor a CLI step may replace it.
        host.replace();
        let e = c.step(1).unwrap_err();
        assert!(e.contains("no longer shows the cast video"), "{e}");
        assert_eq!(*host.showing.lock().unwrap(), Some(2));
        let mut c = controller(&host);
        c.play(video("a"), 0.0).unwrap();
        c.current = Some(Video {
            id: "a".into(),
            index: Some(0),
        });
        c.video_ids = vec!["a".into(), "b".into()];
        host.replace();
        *host.state.lock().unwrap() = "ended".into();
        let token = c.player;
        // The race: `ended` read before the takeover is seen.
        assert_eq!(
            c.advance(video("b")).unwrap_err(),
            "the player no longer shows the cast video"
        );
        assert!(token.is_some() && c.player.is_none());
        assert_eq!(c.status, STOPPED);
        assert_eq!(*host.showing.lock().unwrap(), Some(4));
        // A new video cast from the app still takes the player over.
        c.play(video("z"), 0.0).unwrap();
        assert_eq!(*host.showing.lock().unwrap(), Some(5));
    }

    #[test]
    fn step_reports_the_restored_volume() {
        let host = FakeHost::default();
        let mut c = controller(&host);
        c.handle(vec![
            msg(
                "setPlaylist",
                json!({"listId": "RQ1", "videoId": "a", "currentIndex": "0", "videoIds": "a,b"}),
            ),
            msg("setVolume", json!({"volume": "30", "muted": "true"})),
        ]);
        let v = c.step(1).unwrap();
        assert_eq!(v["player"]["volume"], 30);
        assert_eq!(v["player"]["muted"], true);
    }
}
