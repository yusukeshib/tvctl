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
use super::{Cast, Config, Host, Identity, load_identity, save_identity};
use crate::protocol::{PlayerAction, Switch, WindowOpts};

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

enum Event {
    Messages(u64, Vec<Message>),
    PollEnded(u64, String),
    Paired,
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
            Ok(_) => {}
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
        player.tick();
        player.publish(cast);
    }
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
}

/// Turns sender messages into player commands and reports state.
struct Controller<'a> {
    host: &'a dyn Host,
    lounge: Arc<Lounge>,
    senders: Vec<Remote>,
    list_id: Option<String>,
    video_ids: Vec<String>,
    current: Option<Video>,
    /// The tvctl player showing `current`.
    player: Option<String>,
    status: i64,
    position: f64,
    duration: f64,
    /// When `position` was measured.
    measured: Instant,
    rate: f64,
    volume: (u8, bool),
    /// Where the last cast window was, to open the next one there.
    window: Option<WindowOpts>,
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
            window: None,
            cpn: super::lounge::random_uuid().replace('-', "")[..16].to_string(),
        }
    }

    fn publish(&self, cast: &Cast) {
        let senders = self.senders.iter().map(|s| s.name.clone()).collect();
        cast.update(|s| {
            s.senders = senders;
            s.player = self.player.clone();
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
                    self.apply_volume();
                }
                self.send(vec![self.volume_message()]);
            }
            "getVolume" => self.send(vec![self.volume_message()]),
            "next" => {
                if let Some(v) = self.neighbor(1) {
                    self.play(v, 0.0);
                }
            }
            "previous" => {
                if let Some(v) = self.neighbor(-1) {
                    self.play(v, 0.0);
                }
            }
            "stopVideo" => self.stop(true),
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
        let disconnected = device.filter(|d| was(d) && !now.iter().any(|s| s.id == *d));
        if let Some(d) = connected {
            let name = now.iter().find(|s| s.id == d).map(|s| s.name.clone());
            eprintln!("cast: connected: {}", name.unwrap_or_default());
        }
        if let Some(d) = disconnected {
            let name = self
                .senders
                .iter()
                .find(|s| s.id == d)
                .map(|s| s.name.clone());
            eprintln!("cast: disconnected: {}", name.unwrap_or_default());
        }
        let explicit = event["ui"].as_str() == Some("true") || event["ui"] == true;
        self.senders = now;
        if disconnected.is_some() && explicit && self.senders.is_empty() {
            // "Disconnect" in the app: stop like a TV app.
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
                self.play(v, start);
                return;
            }
        } else if let Some(cur) = &mut self.current {
            match self.video_ids.iter().position(|v| *v == cur.id) {
                Some(i) => cur.index = Some(i),
                None => {
                    self.stop(true);
                    self.current = None;
                }
            }
        }
        self.send(vec![self.now_playing(), self.nav()]);
    }

    /// Runs a command on the cast player; false if there is none or it failed.
    fn command(&mut self, action: PlayerAction) -> bool {
        let Some(id) = self.player.clone() else {
            return false;
        };
        match self.host.player(&id, action) {
            Ok(v) => {
                self.update_from(&v);
                true
            }
            Err(e) => {
                eprintln!("cast: player {id}: {e}");
                if e.contains("no player") {
                    self.player = None;
                }
                false
            }
        }
    }

    fn apply_volume(&mut self) {
        let (level, muted) = self.volume;
        self.command(PlayerAction::Volume {
            level: Some(level),
            muted: Some(muted),
        });
    }

    fn set_status(&mut self, status: i64) {
        if self.status != status {
            self.status = status;
            self.send(vec![self.on_state_change()]);
        }
    }

    /// Records time, duration, rate and window from a player response.
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
        let w = &v["window"];
        if w.is_object() {
            let fullscreen = w["fullscreen"] == true;
            self.window = Some(WindowOpts {
                size: num(&w["size"]["width"]).zip(num(&w["size"]["height"])),
                position: num(&w["position"]["x"]).zip(num(&w["position"]["y"])),
                fullscreen: fullscreen.then_some(Switch::On),
                ..WindowOpts::default()
            });
        }
    }

    fn close_player(&mut self) {
        if let Some(id) = self.player.take() {
            let _ = self.host.close(&id);
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

    /// Plays `video` in a new player window where the last one was.
    fn play(&mut self, video: Video, start: f64) {
        eprintln!("cast: play {} at {start}", video.id);
        self.close_player();
        self.current = Some(video.clone());
        self.status = LOADING;
        self.position = start;
        self.duration = 0.0;
        self.measured = Instant::now();
        self.send(vec![self.on_state_change(), self.now_playing(), self.nav()]);
        let url = format!("https://www.youtube.com/watch?v={}", video.id);
        let window = self.window.clone().unwrap_or_default();
        match self.host.open(&url, start, window) {
            Ok(v) => {
                self.player = v["id"].as_str().map(String::from);
                self.update_from(&v);
                if self.volume != (100, false) {
                    self.apply_volume();
                }
                self.status = PLAYING;
            }
            Err(e) => {
                eprintln!("cast: opening {url}: {e}");
                self.status = STOPPED;
            }
        }
        self.send(vec![self.on_state_change(), self.now_playing(), self.nav()]);
    }

    /// Follows the player: state, position and its end.
    fn tick(&mut self) {
        let Some(id) = self.player.clone() else {
            return;
        };
        let v = match self.host.player(&id, PlayerAction::Status) {
            Ok(v) => v,
            Err(e) => {
                if e.contains("no player") {
                    // The window was closed.
                    self.player = None;
                    self.stop(false);
                }
                return;
            }
        };
        let expected = self.now_position();
        self.update_from(&v);
        let p = &v["player"];
        let status = match p["state"].as_str() {
            Some("playing") => PLAYING,
            Some("paused") => PAUSED,
            Some("ended") => {
                match self.neighbor(1) {
                    Some(next) => self.play(next, 0.0),
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

    #[derive(Default)]
    struct FakeHost {
        calls: Mutex<Vec<String>>,
        state: Mutex<String>,
    }

    impl Host for FakeHost {
        fn open(&self, url: &str, start: f64, window: WindowOpts) -> Result<Value, String> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("open {url} {start} {:?}", window.position));
            *self.state.lock().unwrap() = "playing".into();
            Ok(json!({
                "id": "7",
                "player": { "state": "playing", "current_time": start, "duration": 100, "volume": 100, "muted": false },
                "window": { "position": {"x": 10, "y": 20}, "size": {"width": 640, "height": 360}, "fullscreen": false },
            }))
        }
        fn player(&self, id: &str, action: PlayerAction) -> Result<Value, String> {
            let a = serde_json::to_value(&action).unwrap();
            if a["action"] != "status" {
                self.calls.lock().unwrap().push(format!("{id} {a}"));
            }
            Ok(json!({
                "id": id,
                "player": { "state": *self.state.lock().unwrap(), "current_time": 5, "duration": 100, "volume": 100, "muted": false },
                "window": { "position": {"x": 10, "y": 20}, "size": {"width": 640, "height": 360}, "fullscreen": false },
            }))
        }
        fn close(&self, id: &str) -> Result<Value, String> {
            self.calls.lock().unwrap().push(format!("close {id}"));
            Ok(json!({}))
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

    #[test]
    fn playlist_play_controls_and_queue() {
        let host = FakeHost::default();
        let mut c = controller(&host);
        c.handle(vec![msg(
            "setPlaylist",
            json!({"listId": "RQ1", "videoId": "b", "currentIndex": "1", "currentTime": "12", "videoIds": "a,b,c"}),
        )]);
        assert_eq!(c.player.as_deref(), Some("7"));
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
        assert_eq!(c.player.as_deref(), Some("7"));
        let calls = host.calls.lock().unwrap().clone();
        assert_eq!(
            calls,
            vec![
                "open https://www.youtube.com/watch?v=b 12 None".to_string(),
                r#"7 {"action":"pause"}"#.into(),
                r#"7 {"action":"seek","relative":false,"seconds":30.0}"#.into(),
                r#"7 {"action":"volume","level":40,"muted":false}"#.into(),
                "close 7".into(),
                "open https://www.youtube.com/watch?v=c 0 Some((10.0, 20.0))".into(),
                r#"7 {"action":"volume","level":40,"muted":false}"#.into(),
            ]
        );
    }

    #[test]
    fn ended_video_advances_and_explicit_disconnect_stops() {
        let host = FakeHost::default();
        let mut c = controller(&host);
        let sender = json!({"devices": r#"[{"type":"REMOTE_CONTROL","id":"p1","name":"Phone","theme":"cl"}]"#,
                            "connectionEventDetails": r#"{"deviceId":"p1"}"#});
        c.handle(vec![msg("loungeStatus", sender)]);
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
        assert!(
            host.calls
                .lock()
                .unwrap()
                .last()
                .unwrap()
                .starts_with("close")
        );
    }

    #[test]
    fn closed_window_stops() {
        struct Gone;
        impl Host for Gone {
            fn open(&self, _: &str, _: f64, _: WindowOpts) -> Result<Value, String> {
                Ok(json!({"id": "1", "player": {}}))
            }
            fn player(&self, _: &str, _: PlayerAction) -> Result<Value, String> {
                Err("no player with id \"1\"".into())
            }
            fn close(&self, _: &str) -> Result<Value, String> {
                Ok(json!({}))
            }
        }
        let host = Gone;
        let mut c = Controller::new(
            &host,
            Arc::new(Lounge::new(Screen {
                screen_id: String::new(),
                device_id: String::new(),
                name: String::new(),
            })),
        );
        c.play(
            Video {
                id: "x".into(),
                index: None,
            },
            0.0,
        );
        assert_eq!(c.player.as_deref(), Some("1"));
        c.tick();
        assert!(c.player.is_none());
        assert_eq!(c.status, STOPPED);
    }
}
