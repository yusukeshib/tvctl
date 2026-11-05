//! The YouTube Lounge API, as a "screen" (TV): pairing, the bind session that
//! carries messages from senders (phones), and sending status messages back.
//!
//! Protocol as implemented by YouTube's TV app and by yt-cast-receiver (MIT).

use std::io::Read;
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

const BASE: &str = "https://www.youtube.com/api/lounge";

/// A message from or to the senders: `[AID, [name, payload...]]`.
#[derive(Debug, Clone, PartialEq)]
pub struct Message {
    /// Sequence number of an incoming message (0 for outgoing ones).
    pub aid: u64,
    pub name: String,
    /// The single argument (usually an object), or an array of several, or Null.
    pub payload: Value,
}

impl Message {
    pub fn new(name: &str, payload: Value) -> Message {
        Message {
            aid: 0,
            name: name.to_string(),
            payload,
        }
    }

    /// A payload field as a string (fields arrive as strings or numbers).
    pub fn field(&self, key: &str) -> Option<String> {
        match self.payload.get(key)? {
            Value::String(s) => Some(s.clone()),
            Value::Null => None,
            v => Some(v.to_string()),
        }
    }
}

/// Parses the messages in one array chunk: `[[AID, ["name", args...]], ...]`.
fn parse_chunk(chunk: &Value) -> Vec<Message> {
    let Some(items) = chunk.as_array() else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|item| {
            let aid = item.get(0)?.as_u64()?;
            let body = item.get(1)?.as_array()?;
            let name = body.first()?.as_str()?.to_string();
            let args = &body[1..];
            let payload = match args.len() {
                0 => Value::Null,
                1 => args[0].clone(),
                _ => Value::Array(args.to_vec()),
            };
            Some(Message { aid, name, payload })
        })
        .collect()
}

/// Splits the bind stream into messages. The stream is a sequence of
/// `<length>\n<JSON array>` chunks; the length is ignored and the JSON parsed
/// directly, which does not depend on how the length counts characters.
#[derive(Default)]
pub struct StreamParser {
    buf: String,
}

impl StreamParser {
    pub fn push(&mut self, data: &str) -> Vec<Message> {
        self.buf.push_str(data);
        let mut out = Vec::new();
        loop {
            let rest = self.buf.trim_start();
            let skipped = self.buf.len() - rest.len();
            if rest.is_empty() {
                self.buf.clear();
                break;
            }
            // Skip the length line (or any other line that is not JSON).
            if !rest.starts_with('[') {
                match rest.find('\n') {
                    Some(i) => {
                        self.buf.drain(..skipped + i + 1);
                        continue;
                    }
                    None => break,
                }
            }
            match parse_value(rest) {
                Parsed::Value(v, used) => {
                    out.extend(parse_chunk(&v));
                    self.buf.drain(..skipped + used);
                }
                Parsed::Incomplete => break,
                Parsed::Invalid => match rest.find('\n') {
                    Some(i) => {
                        self.buf.drain(..skipped + i + 1);
                    }
                    None => {
                        self.buf.clear();
                        break;
                    }
                },
            }
        }
        out
    }
}

enum Parsed {
    /// The value and the bytes it used.
    Value(Value, usize),
    Incomplete,
    Invalid,
}

/// Parses one JSON value at the start of `s`.
fn parse_value(s: &str) -> Parsed {
    let mut it = serde_json::Deserializer::from_str(s).into_iter::<Value>();
    match it.next() {
        Some(Ok(v)) => Parsed::Value(v, it.byte_offset()),
        Some(Err(e)) if e.is_eof() => Parsed::Incomplete,
        None => Parsed::Incomplete,
        Some(Err(_)) => Parsed::Invalid,
    }
}

/// Fixed identity of this screen.
#[derive(Debug, Clone)]
pub struct Screen {
    pub screen_id: String,
    pub device_id: String,
    pub name: String,
}

/// Values that change per bind session.
#[derive(Debug, Default)]
struct Bind {
    lounge_token: String,
    sid: String,
    gsessionid: String,
    rid: u64,
    /// Highest message number received (acknowledged with every request).
    aid: u64,
    /// Number of messages sent in this session.
    ofs: u64,
}

pub const APP: &str = "tvctl";
const THEME: &str = "cl";

fn device_info() -> String {
    json!({
        "brand": "tvctl",
        "model": "tvctl",
        "year": 0,
        "os": "macOS",
        "osVersion": "",
        "chipset": "",
        "clientName": "TVHTML5",
        "dialAdditionalDataSupportLevel": "unsupported",
        "mdxDialServerType": "MDX_DIAL_SERVER_TYPE_UNKNOWN",
    })
    .to_string()
}

fn random_hex(len: usize) -> String {
    let mut bytes = vec![0u8; len.div_ceil(2)];
    getrandom(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect::<String>()[..len].to_string()
}

/// Random bytes from the OS.
fn getrandom(buf: &mut [u8]) {
    use std::io::Read;
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        let _ = f.read_exact(buf);
    }
}

pub fn random_uuid() -> String {
    let h = random_hex(32);
    format!(
        "{}-{}-4{}-a{}-{}",
        &h[0..8],
        &h[8..12],
        &h[13..16],
        &h[17..20],
        &h[20..32]
    )
}

fn random_rid() -> u64 {
    let mut b = [0u8; 2];
    getrandom(&mut b);
    41000 + u64::from(u16::from_le_bytes(b)) % 9000
}

pub struct Lounge {
    screen: Screen,
    agent: ureq::Agent,
    /// For the long-poll; no overall timeout.
    stream_agent: ureq::Agent,
    bind: Mutex<Bind>,
}

fn agent(timeout: Option<Duration>) -> ureq::Agent {
    ureq::Agent::new_with_config(
        ureq::config::Config::builder()
            .timeout_global(timeout)
            .timeout_connect(Some(Duration::from_secs(15)))
            .http_status_as_error(false)
            .build(),
    )
}

fn body_text(resp: ureq::http::Response<ureq::Body>, what: &str) -> Result<String> {
    let status = resp.status();
    let text = resp
        .into_body()
        .read_to_string()
        .with_context(|| format!("{what}: reading the response"))?;
    if !status.is_success() {
        bail!(
            "{what}: HTTP {status}: {}",
            text.chars().take(200).collect::<String>()
        );
    }
    Ok(text)
}

/// Asks YouTube for a new screen id.
pub fn generate_screen_id() -> Result<String> {
    let resp = agent(Some(Duration::from_secs(20)))
        .get(format!("{BASE}/pairing/generate_screen_id"))
        .call()
        .context("generating a screen id")?;
    let id = body_text(resp, "generating a screen id")?
        .trim()
        .to_string();
    if id.is_empty() || id.len() > 200 {
        bail!("unexpected screen id {id:?}");
    }
    Ok(id)
}

/// Form fields of a request that sends `messages`.
fn send_form(messages: &[Message], ofs: u64) -> Vec<(String, String)> {
    let mut form = vec![
        ("count".to_string(), messages.len().to_string()),
        ("ofs".to_string(), ofs.to_string()),
    ];
    for (i, m) in messages.iter().enumerate() {
        form.push((format!("req{i}__sc"), m.name.clone()));
        if let Some(obj) = m.payload.as_object() {
            for (k, v) in obj {
                let v = match v {
                    Value::String(s) => s.clone(),
                    Value::Null => String::new(),
                    v => v.to_string(),
                };
                form.push((format!("req{i}_{k}"), v));
            }
        }
    }
    form
}

/// Query string parameters for a bind request.
#[derive(Clone, Copy, PartialEq, Debug)]
enum BindKind {
    Init,
    Send,
    Rpc,
}

impl Lounge {
    pub fn new(screen: Screen) -> Lounge {
        Lounge {
            screen,
            agent: agent(Some(Duration::from_secs(30))),
            stream_agent: agent(None),
            bind: Mutex::new(Bind::default()),
        }
    }

    fn query(&self, b: &mut Bind, kind: BindKind) -> Vec<(&'static str, String)> {
        let s = &self.screen;
        let mut q = vec![
            ("device", "LOUNGE_SCREEN".to_string()),
            ("id", s.device_id.clone()),
            ("obfuscatedGaiaId", String::new()),
            ("name", s.name.clone()),
            ("app", APP.to_string()),
            ("theme", THEME.to_string()),
            ("capabilities", "dsp,mic,dpa,ntb".to_string()),
            ("cst", "m".to_string()),
            ("mdxVersion", "2".to_string()),
            ("loungeIdToken", b.lounge_token.clone()),
            ("VER", "8".to_string()),
            ("v", "2".to_string()),
            ("zx", random_hex(12)),
            ("t", "1".to_string()),
        ];
        match kind {
            BindKind::Init => {
                q.push(("deviceInfo", device_info()));
                q.push(("RID", b.rid.to_string()));
                q.push(("CVER", "1".to_string()));
                b.rid += 1;
            }
            BindKind::Send => {
                q.push(("deviceInfo", device_info()));
                q.push(("SID", b.sid.clone()));
                q.push(("RID", b.rid.to_string()));
                q.push(("AID", b.aid.to_string()));
                q.push(("gsessionid", b.gsessionid.clone()));
                b.rid += 1;
            }
            BindKind::Rpc => {
                q.push(("RID", "rpc".to_string()));
                q.push(("SID", b.sid.clone()));
                q.push(("CI", "0".to_string()));
                q.push(("AID", b.aid.to_string()));
                q.push(("gsessionid", b.gsessionid.clone()));
                q.push(("TYPE", "xmlhttp".to_string()));
            }
        }
        q
    }

    fn bind_url(q: &[(&str, String)]) -> String {
        let mut url = url::Url::parse(&format!("{BASE}/bc/bind")).expect("valid");
        url.query_pairs_mut()
            .extend_pairs(q.iter().map(|(k, v)| (*k, v.as_str())));
        url.to_string()
    }

    /// Gets a lounge token for the screen; returns how long it is valid.
    fn lounge_token(&self) -> Result<(String, Duration)> {
        let resp = self
            .agent
            .post(format!("{BASE}/pairing/get_lounge_token_batch"))
            .send_form([("screen_ids", self.screen.screen_id.as_str())])
            .context("getting a lounge token")?;
        let v: Value = serde_json::from_str(&body_text(resp, "getting a lounge token")?)
            .context("lounge token response")?;
        let s = &v["screens"][0];
        let token = s["loungeToken"]
            .as_str()
            .context("no lounge token in the response")?
            .to_string();
        let refresh = s["refreshIntervalInMillis"]
            .as_u64()
            .or_else(|| s["refreshIntervalInMillis"].as_str()?.parse().ok())
            .map(Duration::from_millis)
            .unwrap_or(Duration::from_secs(13 * 24 * 3600));
        Ok((token, refresh))
    }

    /// Starts a bind session (new lounge token, SID and gsessionid). Returns the
    /// messages that came with it and how long the token is valid.
    pub fn begin(&self) -> Result<(Vec<Message>, Duration)> {
        let (token, refresh) = self.lounge_token()?;
        let url = {
            let mut b = self.bind.lock().unwrap();
            *b = Bind {
                lounge_token: token,
                rid: random_rid(),
                ..Bind::default()
            };
            Self::bind_url(&self.query(&mut b, BindKind::Init))
        };
        let resp = self
            .agent
            .post(url)
            .send_form([("count", "0")])
            .context("starting a lounge session")?;
        let text = body_text(resp, "starting a lounge session")?;
        let messages = StreamParser::default().push(&text);
        let mut b = self.bind.lock().unwrap();
        for m in &messages {
            b.aid = b.aid.max(m.aid);
            match m.name.as_str() {
                "c" => {
                    if let Some(sid) = m.payload.get(0).and_then(Value::as_str) {
                        b.sid = sid.to_string();
                    }
                }
                "S" => {
                    if let Some(g) = m.payload.as_str() {
                        b.gsessionid = g.to_string();
                    }
                }
                _ => {}
            }
        }
        if b.sid.is_empty() || b.gsessionid.is_empty() {
            bail!("the lounge session response had no SID or gsessionid: {text}");
        }
        Ok((messages, refresh))
    }

    /// Opens the long-poll that carries messages from senders; calls `on` for
    /// each until the server ends the response. Returns Err when the request
    /// fails (e.g. the session expired).
    pub fn poll(&self, mut on: impl FnMut(Vec<Message>)) -> Result<()> {
        let url = Self::bind_url(&self.query(&mut self.bind.lock().unwrap(), BindKind::Rpc));
        let resp = self.stream_agent.get(url).call().context("lounge poll")?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.into_body().read_to_string().unwrap_or_default();
            bail!(
                "lounge poll: HTTP {status}: {}",
                text.chars().take(200).collect::<String>()
            );
        }
        let mut reader = resp.into_body().into_reader();
        let mut parser = StreamParser::default();
        let mut buf = [0u8; 8192];
        let mut pending: Vec<u8> = Vec::new();
        loop {
            let n = reader.read(&mut buf).context("reading the lounge poll")?;
            if n == 0 {
                return Ok(());
            }
            pending.extend_from_slice(&buf[..n]);
            // Keep an incomplete UTF-8 sequence for the next read.
            let valid = match std::str::from_utf8(&pending) {
                Ok(_) => pending.len(),
                Err(e) => e.valid_up_to(),
            };
            let text = String::from_utf8_lossy(&pending[..valid]).into_owned();
            pending.drain(..valid);
            let messages = parser.push(&text);
            if !messages.is_empty() {
                {
                    let mut b = self.bind.lock().unwrap();
                    for m in &messages {
                        b.aid = b.aid.max(m.aid);
                    }
                }
                on(messages);
            }
        }
    }

    /// Sends messages to the senders.
    pub fn send(&self, messages: &[Message]) -> Result<()> {
        if messages.is_empty() {
            return Ok(());
        }
        let (url, form) = {
            let mut b = self.bind.lock().unwrap();
            if b.sid.is_empty() {
                bail!("no lounge session");
            }
            let url = Self::bind_url(&self.query(&mut b, BindKind::Send));
            let form = send_form(messages, b.ofs);
            b.ofs += messages.len() as u64;
            (url, form)
        };
        let resp = self
            .agent
            .post(url)
            .send_form(form.iter().map(|(k, v)| (k.as_str(), v.as_str())))
            .context("sending to the lounge")?;
        body_text(resp, "sending to the lounge").map(|_| ())
    }

    /// Lets a sender pair through DIAL with the code it sent.
    pub fn register_pairing_code(&self, code: &str) -> Result<()> {
        let s = &self.screen;
        let resp = self
            .agent
            .post(format!("{BASE}/pairing/register_pairing_code"))
            .send_form([
                ("access_type", "permanent"),
                ("app", APP),
                ("pairing_code", code),
                ("screen_id", s.screen_id.as_str()),
                ("screen_name", s.name.as_str()),
                ("device_id", s.device_id.as_str()),
            ])
            .context("registering the pairing code")?;
        body_text(resp, "registering the pairing code").map(|_| ())
    }

    /// A code to type into the YouTube app ("Link with TV code").
    pub fn pairing_code(&self) -> Result<String> {
        let s = &self.screen;
        let resp = self
            .agent
            .get(format!("{BASE}/pairing/get_pairing_code?ctx=pair"))
            .query("access_type", "permanent")
            .query("app", APP)
            .query(
                "lounge_token",
                self.bind.lock().unwrap().lounge_token.clone(),
            )
            .query("screen_id", s.screen_id.clone())
            .query("screen_name", s.name.clone())
            .query("device_id", s.device_id.clone())
            .call()
            .context("getting a TV code")?;
        let code = body_text(resp, "getting a TV code")?.trim().to_string();
        // Shown in groups of three digits, like the YouTube TV app.
        let digits: Vec<char> = code.chars().collect();
        Ok(digits
            .chunks(3)
            .map(|c| c.iter().collect::<String>())
            .collect::<Vec<_>>()
            .join(" "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_stream_chunks() {
        let mut p = StreamParser::default();
        let data = "123\n[[0,[\"c\",\"SID1\",\"\",8]],[1,[\"S\",\"GS1\"]],[2,[\"loungeStatus\",{\"devices\":\"[]\"}]]]\n";
        let m = p.push(data);
        assert_eq!(m.len(), 3);
        assert_eq!(m[0].name, "c");
        assert_eq!(m[0].payload, json!(["SID1", "", 8]));
        assert_eq!(m[1].payload, json!("GS1"));
        assert_eq!(m[2].aid, 2);
        assert_eq!(m[2].field("devices").as_deref(), Some("[]"));
    }

    #[test]
    fn parses_split_chunks_and_noop() {
        let mut p = StreamParser::default();
        assert!(p.push("40\n[[5,[\"setVol").is_empty());
        let m = p.push("ume\",{\"volume\":\"30\",\"muted\":false}]]]\n20\n[[6,[\"noop\"]]]\n");
        assert_eq!(m.len(), 2);
        assert_eq!(m[0].name, "setVolume");
        assert_eq!(m[0].field("volume").as_deref(), Some("30"));
        assert_eq!(m[0].field("muted").as_deref(), Some("false"));
        assert_eq!(m[1].name, "noop");
        assert_eq!(m[1].payload, Value::Null);
        // Multi-byte text split across reads is handled by the caller; a
        // complete chunk with non-ASCII text parses.
        let m = p.push("30\n[[7,[\"x\",{\"n\":\"Yusuke’s iPhone\"}]]]");
        assert_eq!(m[0].field("n").as_deref(), Some("Yusuke’s iPhone"));
    }

    #[test]
    fn send_form_fields() {
        let f = send_form(
            &[
                Message::new("onStateChange", json!({"state": 1, "currentTime": "1.5"})),
                Message::new("noArgs", Value::Null),
            ],
            4,
        );
        let get = |k: &str| f.iter().find(|(a, _)| a == k).map(|(_, v)| v.as_str());
        assert_eq!(get("count"), Some("2"));
        assert_eq!(get("ofs"), Some("4"));
        assert_eq!(get("req0__sc"), Some("onStateChange"));
        assert_eq!(get("req0_state"), Some("1"));
        assert_eq!(get("req0_currentTime"), Some("1.5"));
        assert_eq!(get("req1__sc"), Some("noArgs"));
    }

    #[test]
    fn bind_queries() {
        let l = Lounge::new(Screen {
            screen_id: "scr".into(),
            device_id: "dev".into(),
            name: "tvctl".into(),
        });
        let mut b = Bind {
            lounge_token: "tok".into(),
            sid: "sid".into(),
            gsessionid: "gs".into(),
            rid: 42000,
            aid: 7,
            ofs: 0,
        };
        let q = l.query(&mut b, BindKind::Rpc);
        let get =
            |q: &[(&str, String)], k: &str| q.iter().find(|(a, _)| *a == k).map(|(_, v)| v.clone());
        assert_eq!(get(&q, "RID").as_deref(), Some("rpc"));
        assert_eq!(get(&q, "AID").as_deref(), Some("7"));
        assert_eq!(get(&q, "TYPE").as_deref(), Some("xmlhttp"));
        assert_eq!(b.rid, 42000);
        let q = l.query(&mut b, BindKind::Send);
        assert_eq!(get(&q, "RID").as_deref(), Some("42000"));
        assert_eq!(get(&q, "loungeIdToken").as_deref(), Some("tok"));
        assert_eq!(b.rid, 42001);
        let url = Lounge::bind_url(&q);
        assert!(
            url.starts_with("https://www.youtube.com/api/lounge/bc/bind?device=LOUNGE_SCREEN&")
        );
    }

    #[test]
    fn random_ids() {
        let u = random_uuid();
        assert_eq!(u.len(), 36);
        assert_eq!(u.matches('-').count(), 4);
        assert_ne!(random_uuid(), u);
        assert!((41000..50000).contains(&random_rid()));
    }
}
