//! DIAL: how YouTube apps find this receiver on the local network (SSDP) and
//! ask it to start the YouTube "app" with a pairing code (HTTP).

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, TcpListener, TcpStream, UdpSocket};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

const SSDP_ADDR: Ipv4Addr = Ipv4Addr::new(239, 255, 255, 250);
const SSDP_PORT: u16 = 1900;
const DIAL_SERVICE: &str = "urn:dial-multiscreen-org:service:dial:1";
const DIAL_DEVICE: &str = "urn:dial-multiscreen-org:device:dial:1";
const SERVER: &str = "macOS UPnP/1.1 tvctl";
/// Announcements are repeated this often (they are valid for 30 minutes).
const NOTIFY_EVERY: Duration = Duration::from_secs(600);

/// Handles `POST /apps/YouTube` with the request body.
pub type LaunchFn = Box<dyn Fn(&str) -> Result<()> + Send + Sync>;

/// What the DIAL server tells and does.
pub struct Device {
    pub uuid: String,
    pub name: String,
    /// Value of `<link rel="run" href>`, constant per receiver.
    pub pid: String,
    /// Called with the body of `POST /apps/YouTube` (form: pairingCode, theme, ...).
    pub launch: LaunchFn,
}

/// The running DIAL receiver: HTTP server and SSDP responder.
pub struct Dial {
    stop: Arc<AtomicBool>,
    port: u16,
}

impl Dial {
    pub fn start(device: Arc<Device>) -> Result<Dial> {
        let listener = TcpListener::bind("0.0.0.0:0").context("starting the DIAL HTTP server")?;
        let port = listener.local_addr()?.port();
        let ssdp = ssdp_socket().context(
            "listening for SSDP on UDP port 1900 (is another program using it exclusively?)",
        )?;
        let stop = Arc::new(AtomicBool::new(false));
        {
            let device = device.clone();
            let stop = stop.clone();
            std::thread::spawn(move || serve_http(listener, &device, &stop));
        }
        {
            let stop = stop.clone();
            std::thread::spawn(move || serve_ssdp(ssdp, &device, port, &stop));
        }
        Ok(Dial { stop, port })
    }

    pub fn port(&self) -> u16 {
        self.port
    }
}

impl Drop for Dial {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // Unblock accept().
        let _ = TcpStream::connect_timeout(
            &SocketAddr::from((Ipv4Addr::LOCALHOST, self.port)),
            Duration::from_millis(200),
        );
    }
}

// ---------- SSDP ----------

fn ssdp_socket() -> Result<UdpSocket> {
    use socket2::{Domain, Protocol, Socket, Type};
    let s = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    s.set_reuse_address(true)?;
    s.set_reuse_port(true)?;
    s.bind(&SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, SSDP_PORT).into())?;
    let mut joined = false;
    for ip in interface_addrs() {
        if s.join_multicast_v4(&SSDP_ADDR, &ip).is_ok() {
            joined = true;
        }
    }
    if !joined {
        s.join_multicast_v4(&SSDP_ADDR, &Ipv4Addr::UNSPECIFIED)?;
    }
    s.set_read_timeout(Some(Duration::from_millis(500)))?;
    Ok(s.into())
}

/// IPv4 addresses of the network interfaces that are up (not loopback).
#[cfg(target_os = "macos")]
fn interface_addrs() -> Vec<Ipv4Addr> {
    let mut out = Vec::new();
    // SAFETY: getifaddrs/freeifaddrs per their contract; the list is only read
    // before it is freed.
    unsafe {
        let mut list: *mut libc::ifaddrs = std::ptr::null_mut();
        if libc::getifaddrs(&mut list) != 0 {
            return out;
        }
        let mut cur = list;
        while !cur.is_null() {
            let ifa = &*cur;
            let up = ifa.ifa_flags & (libc::IFF_UP as u32) != 0;
            let loopback = ifa.ifa_flags & (libc::IFF_LOOPBACK as u32) != 0;
            if up
                && !loopback
                && !ifa.ifa_addr.is_null()
                && i32::from((*ifa.ifa_addr).sa_family) == libc::AF_INET
            {
                let sin = &*(ifa.ifa_addr as *const libc::sockaddr_in);
                let ip = Ipv4Addr::from(u32::from_be(sin.sin_addr.s_addr));
                if !out.contains(&ip) {
                    out.push(ip);
                }
            }
            cur = ifa.ifa_next;
        }
        libc::freeifaddrs(list);
    }
    out
}

#[cfg(not(target_os = "macos"))]
fn interface_addrs() -> Vec<Ipv4Addr> {
    Vec::new()
}

/// The local address used to reach `peer` (the interface a reply goes out on).
fn local_ip_toward(peer: SocketAddr) -> Option<Ipv4Addr> {
    let s = UdpSocket::bind("0.0.0.0:0").ok()?;
    s.connect(peer).ok()?;
    match s.local_addr().ok()?.ip() {
        std::net::IpAddr::V4(ip) if !ip.is_unspecified() => Some(ip),
        _ => None,
    }
}

/// Parses an SSDP request: method and upper-cased headers.
fn parse_ssdp(msg: &str) -> Option<(String, Vec<(String, String)>)> {
    let mut lines = msg.split("\r\n");
    let method = lines.next()?.split_whitespace().next()?.to_string();
    let headers = lines
        .filter_map(|l| {
            let (k, v) = l.split_once(':')?;
            Some((k.trim().to_ascii_uppercase(), v.trim().to_string()))
        })
        .collect();
    Some((method, headers))
}

fn search_targets(uuid: &str) -> [String; 5] {
    [
        DIAL_SERVICE.to_string(),
        DIAL_DEVICE.to_string(),
        "upnp:rootdevice".to_string(),
        "ssdp:all".to_string(),
        format!("uuid:{uuid}"),
    ]
}

/// The reply to an M-SEARCH for `st`, or None if this device does not match.
fn search_reply(msg: &str, uuid: &str, location: &str) -> Option<String> {
    let (method, headers) = parse_ssdp(msg)?;
    if method != "M-SEARCH" {
        return None;
    }
    let st = headers.iter().find(|(k, _)| k == "ST")?.1.clone();
    if !search_targets(uuid).contains(&st) {
        return None;
    }
    Some(format!(
        "HTTP/1.1 200 OK\r\nCACHE-CONTROL: max-age=1800\r\nEXT:\r\nLOCATION: {location}\r\nSERVER: {SERVER}\r\nST: {st}\r\nUSN: uuid:{uuid}::{st}\r\nBOOTID.UPNP.ORG: 1\r\nCONFIGID.UPNP.ORG: 1\r\n\r\n"
    ))
}

fn notify(socket: &UdpSocket, uuid: &str, port: u16, nts: &str) {
    for ip in interface_addrs() {
        let location = format!("http://{ip}:{port}/ssdp/device-desc.xml");
        for nt in &search_targets(uuid)[..3] {
            let msg = format!(
                "NOTIFY * HTTP/1.1\r\nHOST: 239.255.255.250:1900\r\nCACHE-CONTROL: max-age=1800\r\nLOCATION: {location}\r\nNT: {nt}\r\nNTS: {nts}\r\nSERVER: {SERVER}\r\nUSN: uuid:{uuid}::{nt}\r\n\r\n"
            );
            let _ = socket2::SockRef::from(socket).set_multicast_if_v4(&ip);
            let _ = socket.send_to(msg.as_bytes(), (SSDP_ADDR, SSDP_PORT));
        }
    }
}

fn serve_ssdp(socket: UdpSocket, device: &Device, port: u16, stop: &AtomicBool) {
    let reply_socket = UdpSocket::bind("0.0.0.0:0").ok();
    let mut last_notify: Option<Instant> = None;
    let mut buf = [0u8; 2048];
    while !stop.load(Ordering::SeqCst) {
        if last_notify.is_none_or(|t| t.elapsed() >= NOTIFY_EVERY) {
            notify(&socket, &device.uuid, port, "ssdp:alive");
            last_notify = Some(Instant::now());
        }
        let Ok((n, peer)) = socket.recv_from(&mut buf) else {
            continue;
        };
        let msg = String::from_utf8_lossy(&buf[..n]);
        let Some(ip) = local_ip_toward(peer) else {
            continue;
        };
        let location = format!("http://{ip}:{port}/ssdp/device-desc.xml");
        if let Some(reply) = search_reply(&msg, &device.uuid, &location)
            && let Some(s) = &reply_socket
        {
            super::trace(format_args!("SSDP search from {peer}, answered"));
            let _ = s.send_to(reply.as_bytes(), peer);
        }
    }
    notify(&socket, &device.uuid, port, "ssdp:byebye");
}

// ---------- HTTP ----------

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn device_desc(device: &Device, base: &str) -> String {
    format!(
        r#"<?xml version="1.0"?>
<root xmlns="urn:schemas-upnp-org:device-1-0">
  <specVersion><major>1</major><minor>0</minor></specVersion>
  <URLBase>{base}</URLBase>
  <device>
    <deviceType>{DIAL_DEVICE}</deviceType>
    <friendlyName>{}</friendlyName>
    <manufacturer>tvctl</manufacturer>
    <modelName>tvctl</modelName>
    <UDN>uuid:{}</UDN>
    <serviceList>
      <service>
        <serviceType>{DIAL_SERVICE}</serviceType>
        <serviceId>urn:dial-multiscreen-org:serviceId:dial</serviceId>
        <controlURL>/ssdp/notfound</controlURL>
        <eventSubURL>/ssdp/notfound</eventSubURL>
        <SCPDURL>/ssdp/notfound</SCPDURL>
      </service>
    </serviceList>
  </device>
</root>
"#,
        xml_escape(&device.name),
        xml_escape(&device.uuid)
    )
}

fn app_desc(device: &Device) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<service xmlns="urn:dial-multiscreen-org:schemas:dial" dialVer="1.7">
  <name>YouTube</name>
  <options allowStop="false"/>
  <state>running</state>
  <link rel="run" href="{}"/>
</service>
"#,
        xml_escape(&device.pid)
    )
}

struct HttpRequest {
    method: String,
    path: String,
    body: String,
}

fn read_request(stream: &TcpStream) -> Option<HttpRequest> {
    let mut reader = BufReader::new(stream.try_clone().ok()?);
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    let mut parts = line.split_whitespace();
    let method = parts.next()?.to_string();
    let path = parts.next()?.split('?').next()?.to_string();
    let mut length = 0usize;
    loop {
        let mut h = String::new();
        if reader.read_line(&mut h).ok()? == 0 || h.trim().is_empty() {
            break;
        }
        if let Some((k, v)) = h.split_once(':')
            && k.trim().eq_ignore_ascii_case("content-length")
        {
            length = v.trim().parse().unwrap_or(0).min(64 * 1024);
        }
    }
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body).ok()?;
    Some(HttpRequest {
        method,
        path,
        body: String::from_utf8_lossy(&body).into_owned(),
    })
}

/// Status line, extra headers and body of the response to `req`.
fn respond(device: &Device, req: &HttpRequest, base: &str) -> (&'static str, Vec<String>, String) {
    let xml = "Content-Type: application/xml; charset=utf-8".to_string();
    match (req.method.as_str(), req.path.trim_end_matches('/')) {
        ("GET", "/ssdp/device-desc.xml") => (
            "200 OK",
            vec![xml, format!("Application-URL: {base}/apps")],
            device_desc(device, base),
        ),
        ("GET", "/apps/YouTube") => ("200 OK", vec![xml], app_desc(device)),
        ("POST", "/apps/YouTube") => match (device.launch)(&req.body) {
            Ok(()) => (
                "201 Created",
                vec![format!("Location: {base}/apps/YouTube/{}", device.pid)],
                String::new(),
            ),
            Err(e) => {
                eprintln!("cast: launch failed: {e:#}");
                ("503 Service Unavailable", Vec::new(), String::new())
            }
        },
        ("DELETE", p) if p.starts_with("/apps/YouTube") => {
            ("405 Method Not Allowed", Vec::new(), String::new())
        }
        ("GET", "/apps") => ("204 No Content", Vec::new(), String::new()),
        _ => ("404 Not Found", Vec::new(), String::new()),
    }
}

fn serve_http(listener: TcpListener, device: &Arc<Device>, stop: &AtomicBool) {
    for stream in listener.incoming() {
        if stop.load(Ordering::SeqCst) {
            break;
        }
        let Ok(mut stream) = stream else { continue };
        let device = device.clone();
        std::thread::spawn(move || {
            let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
            let Some(req) = read_request(&stream) else {
                return;
            };
            let base = match stream.local_addr() {
                Ok(a) => format!("http://{a}"),
                Err(_) => return,
            };
            let (status, headers, body) = respond(&device, &req, &base);
            let peer = stream
                .peer_addr()
                .map(|a| a.to_string())
                .unwrap_or_default();
            super::trace(format_args!(
                "DIAL {} {} from {peer} -> {status}",
                req.method, req.path
            ));
            let mut out = format!(
                "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n",
                body.len()
            );
            for h in headers {
                out.push_str(&h);
                out.push_str("\r\n");
            }
            out.push_str("\r\n");
            out.push_str(&body);
            let _ = stream.write_all(out.as_bytes());
        });
    }
}

/// Parses `application/x-www-form-urlencoded` text.
pub fn parse_form(body: &str) -> Vec<(String, String)> {
    url::form_urlencoded::parse(body.trim().as_bytes())
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    fn device(launched: Arc<Mutex<Vec<String>>>) -> Device {
        Device {
            uuid: "u-1".into(),
            name: "My <TV>".into(),
            pid: "pid-1".into(),
            launch: Box::new(move |body| {
                launched.lock().unwrap().push(body.to_string());
                if body.contains("fail") {
                    anyhow::bail!("no");
                }
                Ok(())
            }),
        }
    }

    fn req(method: &str, path: &str, body: &str) -> HttpRequest {
        HttpRequest {
            method: method.into(),
            path: path.into(),
            body: body.into(),
        }
    }

    #[test]
    fn http_routes() {
        let launched = Arc::new(Mutex::new(Vec::new()));
        let d = device(launched.clone());
        let base = "http://10.0.0.2:4000";
        let (s, h, b) = respond(&d, &req("GET", "/ssdp/device-desc.xml", ""), base);
        assert_eq!(s, "200 OK");
        assert!(h.contains(&"Application-URL: http://10.0.0.2:4000/apps".to_string()));
        assert!(b.contains("<friendlyName>My &lt;TV&gt;</friendlyName>"));
        assert!(b.contains("<UDN>uuid:u-1</UDN>"));
        let (s, _, b) = respond(&d, &req("GET", "/apps/YouTube", ""), base);
        assert_eq!(s, "200 OK");
        assert!(b.contains("<state>running</state>") && b.contains("href=\"pid-1\""));
        let (s, h, _) = respond(
            &d,
            &req("POST", "/apps/YouTube", "pairingCode=1&theme=cl"),
            base,
        );
        assert_eq!(s, "201 Created");
        assert_eq!(
            h,
            vec!["Location: http://10.0.0.2:4000/apps/YouTube/pid-1".to_string()]
        );
        assert_eq!(
            launched.lock().unwrap().as_slice(),
            ["pairingCode=1&theme=cl"]
        );
        let (s, _, _) = respond(&d, &req("POST", "/apps/YouTube", "fail"), base);
        assert_eq!(s, "503 Service Unavailable");
        assert_eq!(
            respond(&d, &req("DELETE", "/apps/YouTube/pid-1", ""), base).0,
            "405 Method Not Allowed"
        );
        assert_eq!(
            respond(&d, &req("GET", "/apps/Netflix", ""), base).0,
            "404 Not Found"
        );
    }

    #[test]
    fn ssdp_search_replies() {
        let search = |st: &str| {
            format!(
                "M-SEARCH * HTTP/1.1\r\nHOST: 239.255.255.250:1900\r\nMAN: \"ssdp:discover\"\r\nMX: 1\r\nST: {st}\r\n\r\n"
            )
        };
        let loc = "http://10.0.0.2:4000/ssdp/device-desc.xml";
        let r = search_reply(&search(DIAL_SERVICE), "u-1", loc).unwrap();
        assert!(r.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(r.contains(&format!("LOCATION: {loc}\r\n")));
        assert!(r.contains(&format!("ST: {DIAL_SERVICE}\r\n")));
        assert!(r.contains(&format!("USN: uuid:u-1::{DIAL_SERVICE}\r\n")));
        assert!(search_reply(&search("ssdp:all"), "u-1", loc).is_some());
        assert!(search_reply(&search("uuid:u-1"), "u-1", loc).is_some());
        assert!(
            search_reply(
                &search("urn:schemas-upnp-org:device:MediaRenderer:1"),
                "u-1",
                loc
            )
            .is_none()
        );
        assert!(search_reply("NOTIFY * HTTP/1.1\r\nNT: x\r\n\r\n", "u-1", loc).is_none());
    }

    #[test]
    fn forms() {
        assert_eq!(
            parse_form("pairingCode=ab%2Fc&theme=cl\n"),
            vec![
                ("pairingCode".to_string(), "ab/c".to_string()),
                ("theme".to_string(), "cl".to_string())
            ]
        );
    }

    #[test]
    fn real_http_round_trip() {
        let launched = Arc::new(Mutex::new(Vec::new()));
        let d = Arc::new(device(launched));
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let stop = Arc::new(AtomicBool::new(false));
        {
            let stop = stop.clone();
            std::thread::spawn(move || serve_http(listener, &d, &stop));
        }
        let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
        s.write_all(
            b"POST /apps/YouTube HTTP/1.1\r\nHost: x\r\nContent-Length: 13\r\n\r\npairingCode=1",
        )
        .unwrap();
        let mut out = String::new();
        s.read_to_string(&mut out).unwrap();
        assert!(out.starts_with("HTTP/1.1 201 Created\r\n"), "{out}");
        assert!(out.contains(&format!(
            "Location: http://127.0.0.1:{port}/apps/YouTube/pid-1"
        )));
    }
}
