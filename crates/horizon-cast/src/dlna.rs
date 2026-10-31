//! DLNA Digital Media Renderer (UPnP `MediaRenderer:1`).
//!
//! A DMR is what a television exposes to a phone's "cast to" menu: an SSDP
//! advertisement, a device description, three SOAP services and an event channel.
//! The controller owns the transport; this side owns the screen and speakers.
//!
//! ## What a controller gets
//!
//! | endpoint | what it is |
//! |---|---|
//! | `NOTIFY` / M-SEARCH on 239.255.255.250:1900 | the device announces itself and answers searches |
//! | `/dd.xml` | device description: type, UDN, service list |
//! | `/scpd/{AVTransport,RenderingControl,ConnectionManager}.xml` | the actions and state variables |
//! | `/ctl/*` | SOAP control (SetAVTransportURI, Play, Pause, Seek, volume, protocol info) |
//! | `/evt/*` | GENA subscriptions and `LastChange` events |
//! | `/` | a plain status page, for a human with a browser |
//!
//! ## Deliberate scope
//!
//! One connection, one track: `CurrentConnectionID` 0 and `InstanceID` 0. Next /
//! Previous are refused with UPnP error 701 because there is no playlist — a
//! controller that queues a second item will say so, which is more honest than
//! accepting it and playing nothing.
//!
//! The control plane is single-threaded on purpose. A DMR sees a handful of
//! requests per second; a thread per connection would buy nothing and make the
//! subscription table need a lock. Socket timeouts bound how long a stuck
//! controller can delay SSDP: 800 ms for HTTP, 500 ms for event delivery.
//!
//! The UDN is derived from the computer name and friendly name, so controllers
//! remember this renderer across restarts. Two machines with the same computer
//! name would collide; this is a desktop app, not a fleet.
//!
//! ## Verified
//!
//! `dlna-probe --selftest` starts this renderer, serves a generated WAV from a
//! loopback HTTP server and drives it entirely through SOAP the way a controller
//! would. A run on this machine:
//!
//! ```text
//! [   52ms] host       http://127.0.0.1:61726/tone.wav (529244 bytes)
//! [   94ms] dmr        http://127.0.0.1:61727/dd.xml
//! [  116ms] soap       SetAVTransportURI -> 200
//! [  137ms] soap       Play -> 200
//! [ 1676ms] playing    RelTime 1.00s, duration 0:00:06
//! [ 2003ms] paused     PAUSED_PLAYBACK
//! [ 2066ms] seek       resumed at 0:00:03
//! [ 2393ms] volume     SetVolume 30 -> GetVolume 30
//! [ 2620ms] stopped    STOPPED
//! [ 2641ms] sink       13 protocol entries
//! ```
//!
//! That is the real `MediaEngineTransport` decoding the stream, so it covers
//! network fetch, codec hand-off and the state machine together. What it cannot
//! cover is a third-party controller: this ran against our own SOAP client on
//! loopback. A phone's cast menu, a Samsung TV or Windows Media Player remain
//! untested here — the renderer speaks the same wire format, but that is an
//! argument, not a measurement.
//!
//! Same-machine discovery is a separate matter, and one that cost a real bug.
//! A `discover-probe` run while this renderer was alive found nothing, even
//! though an M-SEARCH from an ephemeral Node socket got the correct `200 OK`.
//! The cause is Windows socket sharing on port 1900: a socket that binds 1900
//! with `SO_REUSEADDR` receives multicast but *not* every unicast datagram sent
//! to that port — the kernel hands each one to a single owner. The browser now
//! sends its M-SEARCH from an ephemeral socket and keeps the 1900 socket only
//! for NOTIFY, so both arrival paths are covered:
//!
//! ```text
//! found   DLNA  uuid:92799b11-ddff-4b2b-b1ea-a45ad4515500  http://192.168.0.52:60307/dd.xml
//! ```
//!
//! The SSDP advertisement itself was verified independently: an M-SEARCH from
//! an ephemeral Node socket got a `200 OK` carrying `LOCATION`, `ST`, `USN` and
//! `BOOTID`, and `/dd.xml` was fetched from the address it named.
//!
//! The non-`--selftest` mode of the probe listens and plays nothing until a
//! controller acts, which is what made the discovery check meaningful: the log
//! stayed `Idle 0:00:00 / 0:00:00` for the whole 14 s.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream, ToSocketAddrs, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::discovery::{multicast_socket, primary_interface, SSDP_GROUP, SSDP_PORT};
use crate::error::{CastError, CastResult};
use crate::player::{PlaybackState, Phase, Transport};
use crate::queue::{MediaKind, Track};

pub const DEVICE_TYPE: &str = "urn:schemas-upnp-org:device:MediaRenderer:1";
pub const AVT: &str = "AVTransport";
pub const RCS: &str = "RenderingControl";
pub const CMS: &str = "ConnectionManager";
const AVT_TYPE: &str = "urn:schemas-upnp-org:service:AVTransport:1";
const RCS_TYPE: &str = "urn:schemas-upnp-org:service:RenderingControl:1";
const CMS_TYPE: &str = "urn:schemas-upnp-org:service:ConnectionManager:1";
const MAX_AGE: u32 = 1800;
const SERVER: &str = "Windows/10.0 UPnP/1.1 HORIZON/0.1";
const EVENT_TIMEOUT: u32 = 1800;

const SINK_PROTOCOL_INFO: &str = "http-get:*:video/mp4:*,http-get:*:video/x-matroska:*,http-get:*:video/x-msvideo:*,http-get:*:video/mpeg:*,http-get:*:video/mp2t:*,http-get:*:audio/mpeg:*,http-get:*:audio/mp4:*,http-get:*:audio/wav:*,http-get:*:audio/x-wav:*,http-get:*:audio/flac:*,http-get:*:audio/x-ms-wma:*,http-get:*:image/jpeg:*,http-get:*:image/png:*";

struct Service {
    name: &'static str,
    kind: &'static str,
    scpd: &'static str,
    control: &'static str,
    event: &'static str,
}

const SERVICES: [Service; 3] = [
    Service {
        name: AVT,
        kind: AVT_TYPE,
        scpd: "/scpd/AVTransport.xml",
        control: "/ctl/AVTransport",
        event: "/evt/AVTransport",
    },
    Service {
        name: RCS,
        kind: RCS_TYPE,
        scpd: "/scpd/RenderingControl.xml",
        control: "/ctl/RenderingControl",
        event: "/evt/RenderingControl",
    },
    Service {
        name: CMS,
        kind: CMS_TYPE,
        scpd: "/scpd/ConnectionManager.xml",
        control: "/ctl/ConnectionManager",
        event: "/evt/ConnectionManager",
    },
];

#[derive(Debug, Clone)]
pub struct DmrConfig {
    pub friendly_name: String,
    /// Address the HTTP server binds. `0.0.0.0` is right for a LAN renderer;
    /// tests bind loopback so no firewall rule is involved.
    pub bind: Ipv4Addr,
    /// 0 lets the OS choose; a fixed port makes `LOCATION` stable across restarts.
    pub port: u16,
    /// Show a window when a source is loaded. Audio plays either way.
    pub show_video: bool,
    /// Optional parent window for the video surface. The launcher passes its
    /// own HWND so the picture is a child of the stage rather than a top-level
    /// window that a fullscreen launcher would cover.
    pub video_parent: Option<isize>,
    /// Join SSDP and announce. Tests turn this off to stay off the network.
    pub advertise: bool,
}

impl Default for DmrConfig {
    fn default() -> Self {
        Self {
            friendly_name: "HORIZON Cast Receiver".into(),
            bind: Ipv4Addr::UNSPECIFIED,
            port: 0,
            show_video: true,
            video_parent: None,
            advertise: true,
        }
    }
}

#[derive(Debug, Clone)]
pub struct DmrInfo {
    pub friendly_name: String,
    pub udn: String,
    /// The URL a controller is told about.
    pub location: String,
    /// The address inside `location`: the LAN address, not the bind address.
    pub address: Ipv4Addr,
    pub port: u16,
}

impl DmrInfo {
    pub fn url(&self, path: &str) -> String {
        format!("http://{}:{}{path}", self.address, self.port)
    }
}

/// A running renderer. Dropping it stops the server and sends `ssdp:byebye`.
pub struct Dmr {
    info: DmrInfo,
    transport: Arc<dyn Transport>,
    ssdp_error: Option<String>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Dmr {
    /// Start the renderer with the MediaEngine player.
    #[cfg(windows)]
    pub fn start(config: DmrConfig) -> CastResult<Self> {
        let transport = Arc::new(crate::player::MediaEngineTransport::start_with(
            config.show_video,
            config.video_parent,
        )?);
        Self::start_with(config, transport)
    }

    /// Start the renderer around any player. The DLNA layer touches only the
    /// [`Transport`] trait, which is what makes the control plane testable.
    pub fn start_with(config: DmrConfig, transport: Arc<dyn Transport>) -> CastResult<Self> {
        let listener = TcpListener::bind((config.bind, config.port)).map_err(|_| CastError::Io)?;
        listener.set_nonblocking(true).map_err(|_| CastError::Io)?;
        let port = listener.local_addr().map_err(|_| CastError::Io)?.port();
        let address = advertised_address(config.bind);
        let udn = udn_for(&config.friendly_name);
        let info = DmrInfo {
            friendly_name: config.friendly_name.clone(),
            udn,
            location: format!("http://{address}:{port}/dd.xml"),
            address,
            port,
        };
        let (ssdp, ssdp_error) = if config.advertise {
            match multicast_socket(SSDP_PORT, SSDP_GROUP, 4) {
                Ok(socket) => (Some(socket), None),
                // HTTP control still works; only discovery is lost. Saying so is
                // better than refusing to start a renderer a manual URL can reach.
                Err(_) => (
                    None,
                    Some(format!(
                        "SSDP unavailable: another process holds port {SSDP_PORT} without address reuse"
                    )),
                ),
            }
        } else {
            (None, None)
        };

        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        let server = Server::new(listener, ssdp, info.clone(), transport.clone());
        let thread = std::thread::Builder::new()
            .name("horizon-dmr".into())
            .spawn(move || server.run(flag))
            .map_err(|_| CastError::Io)?;
        Ok(Self {
            info,
            transport,
            ssdp_error,
            stop,
            thread: Some(thread),
        })
    }

    pub fn info(&self) -> &DmrInfo {
        &self.info
    }

    pub fn state(&self) -> PlaybackState {
        self.transport.state()
    }

    /// Why SSDP is not running, if it is not. The HTTP endpoints can be fine.
    pub fn ssdp_error(&self) -> Option<&str> {
        self.ssdp_error.as_deref()
    }

    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for Dmr {
    fn drop(&mut self) {
        self.stop();
    }
}

/// The address to put in `LOCATION`, which must be reachable by the controller,
/// not the wildcard the server is bound to.
fn advertised_address(bind: Ipv4Addr) -> Ipv4Addr {
    if !bind.is_unspecified() {
        return bind;
    }
    let interface = primary_interface();
    if interface.is_unspecified() || interface.is_loopback() {
        Ipv4Addr::LOCALHOST
    } else {
        interface
    }
}

/// Stable UUID for machine + name. Not RFC 4122 name-based, but deterministic and
/// shaped like a v4 UUID, which is all controllers read it for.
fn udn_for(name: &str) -> String {
    let machine = std::env::var("COMPUTERNAME").unwrap_or_default();
    let seed = format!("horizon-dmr|{machine}|{name}");
    format!("uuid:{}", uuid_like(&seed))
}

fn uuid_like(seed: &str) -> String {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in seed.as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    let a = mix(hash);
    let b = mix(hash ^ 0x9e37_79b9_7f4a_7c15);
    let mut bytes = [0u8; 16];
    bytes[..8].copy_from_slice(&a.to_be_bytes());
    bytes[8..].copy_from_slice(&b.to_be_bytes());
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:04x}-{:04x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        bytes[0], bytes[1], bytes[2], bytes[3],
        bytes[4], bytes[5],
        0x4000 | (((bytes[6] as u16) << 8 | bytes[7] as u16) & 0x0fff),
        0x8000 | (((bytes[8] as u16) << 8 | bytes[9] as u16) & 0x3fff),
        bytes[10], bytes[11], bytes[12], bytes[13], bytes[14], bytes[15]
    )
}

fn mix(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9e37_79b9_7f4a_7c15);
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    x ^ (x >> 31)
}

// ------------------------------------------------------------------ server

struct PendingReply {
    due: Instant,
    to: SocketAddr,
    st: String,
    usn: String,
}

struct Server {
    listener: TcpListener,
    ssdp: Option<UdpSocket>,
    info: DmrInfo,
    transport: Arc<dyn Transport>,
    subs: Subscriptions,
    pending: Vec<PendingReply>,
    next_alive: Instant,
    next_poll: Instant,
    last_event: Option<EventKey>,
    last_event_at: Instant,
    bootid: u32,
    /// Replies whose sockets have sent their FIN and are waiting for the
    /// client to finish the teardown. Closing before that last packet lands is
    /// what makes a controller read a reset instead of its answer, but waiting
    /// for it on this thread would stall whoever connects next, so the socket
    /// is parked here and retired between ticks.
    lingering: Vec<(TcpStream, Instant)>,
}

impl Server {
    fn new(
        listener: TcpListener,
        ssdp: Option<UdpSocket>,
        info: DmrInfo,
        transport: Arc<dyn Transport>,
    ) -> Self {
        let bootid = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| (d.as_secs() & 0x7fff_ffff) as u32)
            .unwrap_or(1)
            .max(1);
        Self {
            listener,
            ssdp,
            info,
            transport,
            subs: Subscriptions::default(),
            pending: Vec::new(),
            next_alive: Instant::now() + Duration::from_secs(900),
            next_poll: Instant::now(),
            last_event: None,
            last_event_at: Instant::now(),
            bootid,
            lingering: Vec::new(),
        }
    }

    fn run(mut self, stop: Arc<AtomicBool>) {
        self.notify_all("ssdp:alive");
        while !stop.load(Ordering::Relaxed) {
            let now = Instant::now();
            self.drain_ssdp(now);
            self.send_pending(now);
            self.retire_lingering(now);
            if now >= self.next_alive {
                self.notify_all("ssdp:alive");
                self.next_alive = now + Duration::from_secs(MAX_AGE as u64 / 2);
            }
            // The whole backlog, not one connection: a controller that has
            // opened two sockets to read a description and post an action must
            // not have the second one wait a tick behind the first.
            for _ in 0..8 {
                match self.listener.accept() {
                    Ok((stream, peer)) => {
                        trace(|| format!("accept {}", peer.port()));
                        self.handle(stream)
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(e) => {
                        trace(|| format!("accept failed: {e}"));
                        break;
                    }
                }
            }
            if now >= self.next_poll {
                self.publish_events(now);
                self.subs.expire(now);
                self.next_poll = now + Duration::from_millis(200);
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        self.notify_all("ssdp:byebye");
    }

    // ------------------------------------------------------------- discovery

    /// Every `(NT, USN)` this device answers with. A controller searches for one
    /// of these strings; the uuid and root forms are how a control point learns
    /// it exists before it knows what it is.
    fn adverts(&self) -> Vec<(String, String)> {
        let udn = self.info.udn.clone();
        let mut out = vec![
            ("upnp:rootdevice".to_string(), format!("{udn}::upnp:rootdevice")),
            (udn.clone(), udn.clone()),
            (DEVICE_TYPE.to_string(), format!("{udn}::{DEVICE_TYPE}")),
        ];
        for service in &SERVICES {
            out.push((
                service.kind.to_string(),
                format!("{udn}::{}", service.kind),
            ));
        }
        out
    }

    fn notify_all(&self, nts: &str) {
        let Some(socket) = &self.ssdp else {
            return;
        };
        for (nt, usn) in self.adverts() {
            let mut packet = format!(
                "NOTIFY * HTTP/1.1\r\nHOST: {SSDP_GROUP}:{SSDP_PORT}\r\nNT: {nt}\r\nNTS: {nts}\r\nUSN: {usn}\r\nBOOTID.UPNP.ORG: {}\r\n",
                self.bootid
            );
            if nts == "ssdp:alive" {
                packet.push_str(&format!(
                    "CACHE-CONTROL: max-age={MAX_AGE}\r\nLOCATION: {}\r\nSERVER: {SERVER}\r\n",
                    self.info.location
                ));
            }
            packet.push_str("\r\n");
            let _ = socket.send_to(
                packet.as_bytes(),
                SocketAddr::from((SSDP_GROUP, SSDP_PORT)),
            );
        }
    }

    fn drain_ssdp(&mut self, now: Instant) {
        let Some(socket) = &self.ssdp else {
            return;
        };
        let mut buf = [0u8; 2048];
        loop {
            match socket.recv_from(&mut buf) {
                Ok((n, from)) => {
                    let Ok(text) = std::str::from_utf8(&buf[..n]) else {
                        continue;
                    };
                    let Some(headers) = parse_m_search(text) else {
                        continue;
                    };
                    let Some(st) = headers.get("st") else {
                        continue;
                    };
                    // MX is the controller's patience. Answering inside it in a
                    // random slice avoids every renderer on the LAN replying in
                    // the same millisecond.
                    let mx = headers
                        .get("mx")
                        .and_then(|value| value.parse::<u64>().ok())
                        .unwrap_or(1)
                        .clamp(0, 5);
                    let fraction = (now.elapsed().as_nanos() % 1_000) as f64 / 1_000.0;
                    let delay = Duration::from_millis((mx as f64 * 1000.0 * fraction) as u64);
                    for (nt, usn) in self.adverts() {
                        if !search_matches(st, &nt) {
                            continue;
                        }
                        self.pending.push(PendingReply {
                            due: now + delay,
                            to: from,
                            st: nt,
                            usn,
                        });
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return,
                Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => continue,
                Err(_) => return,
            }
        }
    }

    fn send_pending(&mut self, now: Instant) {
        let Some(socket) = &self.ssdp else {
            self.pending.clear();
            return;
        };
        let mut keep = Vec::new();
        for reply in self.pending.drain(..) {
            if reply.due > now {
                keep.push(reply);
                continue;
            }
            let packet = format!(
                "HTTP/1.1 200 OK\r\nCACHE-CONTROL: max-age={MAX_AGE}\r\nDATE: {}\r\nEXT:\r\nLOCATION: {}\r\nSERVER: {SERVER}\r\nST: {}\r\nUSN: {}\r\nBOOTID.UPNP.ORG: {}\r\n\r\n",
                http_date(SystemTime::now()),
                self.info.location,
                reply.st,
                reply.usn,
                self.bootid
            );
            let _ = socket.send_to(packet.as_bytes(), reply.to);
        }
        self.pending = keep;
    }

    // ----------------------------------------------------------------- http

    fn retire_lingering(&mut self, now: Instant) {
        self.lingering.retain_mut(|(stream, due)| {
            // Drain to the bottom of the receive buffer before any exit. A
            // socket closed with unread bytes is aborted, and the resulting
            // reset is addressed to the controller's source port — which a
            // controller that has since exited can hand to its next
            // connection, resetting a request that was never ours.
            let mut sink = [0u8; 1024];
            let mut fin = false;
            let mut reset = false;
            loop {
                match stream.read(&mut sink) {
                    Ok(0) => {
                        fin = true;
                        break;
                    }
                    Ok(_) => continue,
                    Err(e) => {
                        // A hard error (the peer's own reset) means there is no
                        // teardown left to wait for; holding the socket only
                        // keeps this four-tuple out of reach of the next call.
                        reset = !matches!(
                            e.kind(),
                            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                        );
                        break;
                    }
                }
            }
            if fin || reset {
                let port = peer_port(stream);
                trace(|| format!("peer {port}: closed ({})", if fin { "peer fin" } else { "reset" }));
                return false;
            }
            if now >= *due {
                let port = peer_port(stream);
                trace(|| format!("peer {port}: deadline, closed drained"));
                return false;
            }
            true
        });
    }

    fn handle(&mut self, stream: TcpStream) {
        let peer = peer_port(&stream);
        let _ = stream.set_nodelay(true);
        // One connection at a time on this thread, so an upper bound on how
        // long a silent probe can hold the control plane is worthwhile.
        let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));
        let _ = stream.set_write_timeout(Some(Duration::from_millis(800)));
        let mut reader = BufReader::new(stream);
        let request = match read_request(&mut reader) {
            Ok(request) => request,
            // The peer hung up before asking anything; it has no reply to wait
            // for. Still close it out through the linger list: dropping a
            // half-open socket straight away is how the abort is answered with
            // a reset, and a controller that reuses the port for its next call
            // reads that reset as a failure of a request it has not sent yet.
            Err(0) => {
                trace(|| format!("peer {peer}: hung up"));
                self.linger_close(reader.into_inner());
                return;
            }
            Err(status) => {
                trace(|| format!("peer {peer}: -> {status}"));
                let response = Response::text(status, status_reason(status).to_string());
                let mut stream = reader.into_inner();
                let _ = write_response(&mut stream, "", &response);
                self.linger_close(stream);
                return;
            }
        };
        trace(|| format!("peer {peer}: {} {}", request.method, request.path));
        let response = self.route(&request);
        let mut stream = reader.into_inner();
        if let Err(e) = write_response(&mut stream, &request.method, &response) {
            trace(|| format!("peer {peer}: write failed: {e}"));
        }
        self.linger_close(stream);
        if let Some(sid) = response.initial_event {
            self.send_initial(&sid);
        }
    }

    /// Half-close the reply, then let the loop watch for the client's own FIN.
    /// A controller that has to guess when the body ends waits for our close
    /// instead of moving on to its next action, so the FIN goes out now; the
    /// socket then stays on the list only long enough to read the peer's FIN,
    /// because a socket dropped with unread bytes is aborted and the abort
    /// destroys the reply the controller has not read yet.
    fn linger_close(&mut self, stream: TcpStream) {
        trace(|| format!("peer {}: fin sent", peer_port(&stream)));
        let _ = stream.shutdown(std::net::Shutdown::Write);
        let _ = stream.set_nonblocking(true);
        if self.lingering.len() >= 32 {
            self.lingering.remove(0);
        }
        self.lingering
            .push((stream, Instant::now() + Duration::from_millis(300)));
    }

    fn route(&mut self, request: &Request) -> Response {
        let path = request.path.as_str();
        let method = request.method.as_str();
        match path {
            "/" if method == "GET" || method == "HEAD" => {
                Response::html(200, self.status_page())
            }
            "/dd.xml" if method == "GET" || method == "HEAD" => {
                Response::xml(200, device_description(&self.info))
            }
            "/favicon.ico" => Response::text(404, "not found".into()),
            _ => {
                if let Some(service) = SERVICES.iter().find(|service| service.scpd == path) {
                    if method == "GET" || method == "HEAD" {
                        return Response::xml(200, scpd(service.name));
                    }
                    return Response::text(405, "method not allowed".into());
                }
                if let Some(service) = SERVICES.iter().find(|service| service.control == path) {
                    // The extension's SOAP fetch needs CORS: Content-Type text/xml and
                    // SOAPACTION are not CORS-safelisted, so the browser sends a preflight.
                    if method == "OPTIONS" {
                        let mut response = Response::empty(204, Vec::new());
                        response.headers.push(("Access-Control-Allow-Origin".into(), "*".into()));
                        response.headers.push(("Access-Control-Allow-Methods".into(), "POST, OPTIONS".into()));
                        response.headers.push(("Access-Control-Allow-Headers".into(), "Content-Type, SOAPACTION".into()));
                        response.headers.push(("Access-Control-Max-Age".into(), "600".into()));
                        return response;
                    }
                    if method != "POST" {
                        return Response::text(405, "method not allowed".into());
                    }
                    let mut response = match service.name {
                        AVT => self.control_avtransport(request),
                        RCS => self.control_rendering(request),
                        _ => self.control_connection(request),
                    };
                    response.headers.push(("Access-Control-Allow-Origin".into(), "*".into()));
                    return response;
                }
                if let Some(service) = SERVICES.iter().find(|service| service.event == path) {
                    return match method {
                        "SUBSCRIBE" => self.subscribe(request, service.name),
                        "UNSUBSCRIBE" => self.unsubscribe(request),
                        _ => Response::text(405, "method not allowed".into()),
                    };
                }
                Response::text(404, "not found".into())
            }
        }
    }

    fn status_page(&self) -> String {
        let state = self.transport.state();
        format!(
            "<!doctype html><meta charset=\"utf-8\"><title>{name}</title>\
             <h1>{name}</h1>\
             <p>State: <b>{transport}</b> ({phase:?})</p>\
             <p>Position: {pos} / {dur}</p>\
             <p>Source: <code>{uri}</code></p>\
             <p>UDN: <code>{udn}</code></p>\
             <p>Description: <a href=\"/dd.xml\">/dd.xml</a></p>",
            name = escape(&self.info.friendly_name),
            transport = transport_state(state.phase),
            phase = state.phase,
            pos = hms(state.position),
            dur = hms(state.duration),
            uri = escape(state.uri.as_deref().unwrap_or("—")),
            udn = escape(&self.info.udn),
        )
    }

    // ---------------------------------------------------------------- soap

    fn control_avtransport(&mut self, request: &Request) -> Response {
        let Some(action) = action_of(request, AVT_TYPE) else {
            return soap_error(401, "Invalid Action");
        };
        let body = String::from_utf8_lossy(&request.body);
        if let Some(id) = xml_tag(&body, "InstanceID") {
            if id.trim() != "0" && !id.trim().is_empty() {
                return soap_error(718, "Invalid InstanceID");
            }
        }
        let state = self.transport.state();
        let empty = String::new();
        match action.as_str() {
            "SetAVTransportURI" => {
                let Some(uri) = xml_tag(&body, "CurrentURI") else {
                    return soap_error(402, "Invalid Args");
                };
                if uri.trim().is_empty() {
                    return soap_error(402, "Invalid Args");
                }
                match self.transport.load(uri.trim()) {
                    Ok(()) => soap_ok(AVT, &action, &empty),
                    Err(_) => soap_error(501, "Action Failed"),
                }
            }
            // Staged behind the current item, exactly as the spec reads it: it
            // starts when the current one ends, and it survives until then.
            "SetNextAVTransportURI" => {
                let Some(uri) = xml_tag(&body, "NextURI") else {
                    return soap_error(402, "Invalid Args");
                };
                if uri.trim().is_empty() {
                    return soap_error(402, "Invalid Args");
                }
                match self.transport.set_next(uri.trim()) {
                    Ok(()) => soap_ok(AVT, &action, &empty),
                    Err(_) => soap_error(501, "Action Failed"),
                }
            }
            "Play" => match self.transport.play() {
                Ok(()) => soap_ok(AVT, &action, &empty),
                Err(_) => soap_error(501, "Action Failed"),
            },
            "Pause" => match self.transport.pause() {
                Ok(()) => soap_ok(AVT, &action, &empty),
                Err(_) => soap_error(701, "Transition not available"),
            },
            "Stop" => match self.transport.stop() {
                Ok(()) => soap_ok(AVT, &action, &empty),
                Err(_) => soap_error(701, "Transition not available"),
            },
            "Seek" => {
                let unit = xml_tag(&body, "Unit").unwrap_or_default();
                if !unit.eq_ignore_ascii_case("REL_TIME") && !unit.eq_ignore_ascii_case("ABS_TIME") {
                    return soap_error(710, "Seek mode not supported");
                }
                let Some(target) = xml_tag(&body, "Target") else {
                    return soap_error(402, "Invalid Args");
                };
                let Some(seconds) = parse_hms(&target) else {
                    return soap_error(711, "Illegal seek target");
                };
                match self.transport.seek(seconds) {
                    Ok(()) => soap_ok(AVT, &action, &empty),
                    Err(_) => soap_error(711, "Illegal seek target"),
                }
            }
            // A controller stepping through a list wants a reason, not silence:
            // past the last item this answers 701, which is what every TV and
            // phone reads as "there is no next".
            "Next" => {
                if !self.transport.has_next() {
                    return soap_error(701, "Transition not available");
                }
                match self.transport.next() {
                    Ok(()) => soap_ok(AVT, &action, &empty),
                    Err(_) => soap_error(701, "Transition not available"),
                }
            }
            "Previous" => match self.transport.previous() {
                Ok(()) => soap_ok(AVT, &action, &empty),
                Err(_) => soap_error(701, "Transition not available"),
            },
            "GetTransportInfo" => {
                let inner = format!(
                    "<CurrentTransportState>{}</CurrentTransportState>\
                     <CurrentTransportStatus>{}</CurrentTransportStatus>\
                     <CurrentSpeed>1</CurrentSpeed>",
                    transport_state(state.phase),
                    if state.phase == Phase::Error {
                        "ERROR_OCCURRED"
                    } else {
                        "OK"
                    }
                );
                soap_ok(AVT, &action, &inner)
            }
            "GetPositionInfo" => {
                let track = if state.uri.is_some() { 1 } else { 0 };
                // DIDL is XML inside XML, so the document is escaped a second
                // time on its way into this element.
                let meta = self
                    .transport
                    .current()
                    .map(|item| escape(&didl_metadata(&item)))
                    .unwrap_or_default();
                let inner = format!(
                    "<Track>{track}</Track>\
                     <TrackDuration>{dur}</TrackDuration>\
                     <TrackMetaData>{meta}</TrackMetaData>\
                     <TrackURI>{uri}</TrackURI>\
                     <RelTime>{pos}</RelTime>\
                     <AbsTime>{pos}</AbsTime>\
                     <RelCount>2147483647</RelCount>\
                     <AbsCount>2147483647</AbsCount>",
                    dur = hms(state.duration),
                    uri = escape(state.uri.as_deref().unwrap_or("")),
                    pos = hms(state.position),
                );
                soap_ok(AVT, &action, &inner)
            }
            "GetMediaInfo" => {
                let track = if state.uri.is_some() { 1 } else { 0 };
                let inner = format!(
                    "<NrTracks>{track}</NrTracks>\
                     <MediaDuration>{dur}</MediaDuration>\
                     <CurrentURI>{uri}</CurrentURI>\
                     <CurrentURIMetaData></CurrentURIMetaData>\
                     <NextURI></NextURI>\
                     <NextURIMetaData></NextURIMetaData>\
                     <PlayMedium>NETWORK</PlayMedium>\
                     <RecordMedium>NOT_IMPLEMENTED</RecordMedium>\
                     <WriteStatus>NOT_IMPLEMENTED</WriteStatus>",
                    dur = hms(state.duration),
                    uri = escape(state.uri.as_deref().unwrap_or("")),
                );
                soap_ok(AVT, &action, &inner)
            }
            "GetDeviceCapabilities" => soap_ok(
                AVT,
                &action,
                "<PlayMedia>NETWORK</PlayMedia>\
                 <RecMedia>NOT_IMPLEMENTED</RecMedia>\
                 <RecQualityModes>NOT_IMPLEMENTED</RecQualityModes>",
            ),
            "GetTransportSettings" => soap_ok(
                AVT,
                &action,
                "<PlayMode>NORMAL</PlayMode><RecQualityMode>NOT_IMPLEMENTED</RecQualityMode>",
            ),
            "GetCurrentTransportActions" => {
                let inner = format!(
                    "<Actions>{}</Actions>",
                    transport_actions(state.phase, self.transport.has_next())
                );
                soap_ok(AVT, &action, &inner)
            }
            _ => soap_error(401, "Invalid Action"),
        }
    }

    fn control_rendering(&mut self, request: &Request) -> Response {
        let Some(action) = action_of(request, RCS_TYPE) else {
            return soap_error(401, "Invalid Action");
        };
        let body = String::from_utf8_lossy(&request.body);
        if let Some(id) = xml_tag(&body, "InstanceID") {
            if id.trim() != "0" && !id.trim().is_empty() {
                return soap_error(718, "Invalid InstanceID");
            }
        }
        let state = self.transport.state();
        match action.as_str() {
            "GetVolume" => soap_ok(
                RCS,
                &action,
                &format!(
                    "<CurrentVolume>{}</CurrentVolume>",
                    (state.volume * 100.0).round().clamp(0.0, 100.0) as u32
                ),
            ),
            "SetVolume" => {
                let Some(volume) = xml_tag(&body, "DesiredVolume")
                    .and_then(|value| value.trim().parse::<f64>().ok())
                else {
                    return soap_error(402, "Invalid Args");
                };
                if !(0.0..=100.0).contains(&volume) {
                    return soap_error(402, "Invalid Args");
                }
                match self.transport.set_volume(volume / 100.0) {
                    Ok(()) => soap_ok(RCS, &action, ""),
                    Err(_) => soap_error(501, "Action Failed"),
                }
            }
            "GetMute" => soap_ok(
                RCS,
                &action,
                &format!("<CurrentMute>{}</CurrentMute>", u8::from(state.muted)),
            ),
            "SetMute" => {
                let Some(mute) = xml_tag(&body, "DesiredMute") else {
                    return soap_error(402, "Invalid Args");
                };
                let muted = match mute.trim().to_ascii_lowercase().as_str() {
                    "1" | "true" | "yes" => true,
                    "0" | "false" | "no" => false,
                    _ => return soap_error(402, "Invalid Args"),
                };
                match self.transport.set_mute(muted) {
                    Ok(()) => soap_ok(RCS, &action, ""),
                    Err(_) => soap_error(501, "Action Failed"),
                }
            }
            "ListPresets" => soap_ok(
                RCS,
                &action,
                "<CurrentPresetNameList>FactoryDefaults</CurrentPresetNameList>",
            ),
            "SelectPreset" => soap_ok(RCS, &action, ""),
            _ => soap_error(401, "Invalid Action"),
        }
    }

    fn control_connection(&mut self, request: &Request) -> Response {
        let Some(action) = action_of(request, CMS_TYPE) else {
            return soap_error(401, "Invalid Action");
        };
        match action.as_str() {
            "GetProtocolInfo" => soap_ok(
                CMS,
                &action,
                &format!(
                    "<Source></Source><Sink>{}</Sink>",
                    escape(SINK_PROTOCOL_INFO)
                ),
            ),
            "GetCurrentConnectionIDs" => {
                soap_ok(CMS, &action, "<ConnectionIDs>0</ConnectionIDs>")
            }
            "GetCurrentConnectionInfo" => soap_ok(
                CMS,
                &action,
                "<RcsID>0</RcsID>\
                 <AVTransportID>0</AVTransportID>\
                 <ProtocolInfo></ProtocolInfo>\
                 <PeerConnectionManager></PeerConnectionManager>\
                 <PeerConnectionID>-1</PeerConnectionID>\
                 <Direction>Input</Direction>\
                 <Status>OK</Status>",
            ),
            _ => soap_error(401, "Invalid Action"),
        }
    }

    // ---------------------------------------------------------------- gena

    fn subscribe(&mut self, request: &Request, service: &'static str) -> Response {
        let timeout = request
            .headers
            .get("timeout")
            .map(|value| parse_timeout(value))
            .unwrap_or(EVENT_TIMEOUT);
        if let Some(sid) = request.headers.get("sid") {
            return if self.subs.renew(sid, timeout, Instant::now()) {
                Response::empty(200, vec![("SID".into(), sid.clone()), ("TIMEOUT".into(), format!("Second-{timeout}"))])
            } else {
                Response::text(412, "unknown subscription".into())
            };
        }
        let nt_ok = request
            .headers
            .get("nt")
            .map(|value| value.eq_ignore_ascii_case("upnp:event"))
            .unwrap_or(false);
        if !nt_ok {
            return Response::text(412, "NT must be upnp:event".into());
        }
        let Some(callback) = request.headers.get("callback").and_then(|v| parse_callback(v)) else {
            return Response::text(412, "missing or invalid CALLBACK".into());
        };
        let sid = self.subs.add(service, callback, timeout, &self.info.udn);
        let mut response = Response::empty(
            200,
            vec![
                ("SID".into(), sid.clone()),
                ("TIMEOUT".into(), format!("Second-{timeout}")),
            ],
        );
        response.initial_event = Some(sid);
        response
    }

    fn unsubscribe(&mut self, request: &Request) -> Response {
        let Some(sid) = request.headers.get("sid") else {
            return Response::text(412, "missing SID".into());
        };
        if self.subs.remove(sid) {
            Response::empty(200, Vec::new())
        } else {
            Response::text(412, "unknown subscription".into())
        }
    }

    fn send_initial(&mut self, sid: &str) {
        let service = match self.subs.get(sid) {
            Some(subscription) => subscription.service,
            None => return,
        };
        let body = event_body(service, &self.transport.state(), self.transport.has_next());
        self.deliver(sid, &body);
    }

    fn publish_events(&mut self, now: Instant) {
        let state = self.transport.state();
        let has_next = self.transport.has_next();
        let key = EventKey::of(&state, has_next);
        let Some(previous) = &self.last_event else {
            self.last_event = Some(key);
            return;
        };
        if *previous == key {
            return;
        }
        let only_position = previous.phase == key.phase
            && previous.uri == key.uri
            && previous.volume == key.volume
            && previous.muted == key.muted
            && previous.duration_s == key.duration_s;
        // Position walks every second; controllers poll it. Sending it in
        // LastChange is useful, every 5 s is not a flood.
        if only_position && now.duration_since(self.last_event_at) < Duration::from_secs(5) {
            self.last_event = Some(key);
            return;
        }
        self.last_event = Some(key);
        self.last_event_at = now;
        for service in [AVT, RCS] {
            let body = event_body(service, &state, has_next);
            for sid in self.subs.sids(service) {
                self.deliver(&sid, &body);
            }
        }
    }

    fn deliver(&mut self, sid: &str, body: &str) {
        let Some(subscription) = self.subs.map.get_mut(sid) else {
            return;
        };
        let seq = subscription.seq;
        subscription.seq = subscription.seq.wrapping_add(1);
        if post_notify(&subscription.callback, sid, seq, body).is_err() {
            subscription.failures += 1;
        }
        if subscription.failures >= 3 {
            self.subs.map.remove(sid);
        }
    }
}

// ------------------------------------------------------------ subscriptions

struct Callback {
    host: String,
    port: u16,
    path: String,
}

struct Subscription {
    service: &'static str,
    callback: Callback,
    expires: Instant,
    seq: u32,
    failures: u8,
}

#[derive(Default)]
struct Subscriptions {
    map: HashMap<String, Subscription>,
    next: u32,
}

impl Subscriptions {
    fn add(
        &mut self,
        service: &'static str,
        callback: Callback,
        timeout: u32,
        udn: &str,
    ) -> String {
        let sid = format!("uuid:{}", uuid_like(&format!("{udn}|{}", self.next)));
        self.next = self.next.wrapping_add(1);
        self.map.insert(
            sid.clone(),
            Subscription {
                service,
                callback,
                expires: Instant::now() + Duration::from_secs(timeout as u64),
                seq: 0,
                failures: 0,
            },
        );
        sid
    }

    fn renew(&mut self, sid: &str, timeout: u32, now: Instant) -> bool {
        match self.map.get_mut(sid) {
            Some(subscription) => {
                subscription.expires = now + Duration::from_secs(timeout as u64);
                subscription.failures = 0;
                true
            }
            None => false,
        }
    }

    fn remove(&mut self, sid: &str) -> bool {
        self.map.remove(sid).is_some()
    }

    fn get(&self, sid: &str) -> Option<&Subscription> {
        self.map.get(sid)
    }

    fn sids(&self, service: &str) -> Vec<String> {
        self.map
            .iter()
            .filter(|(_, subscription)| subscription.service == service)
            .map(|(sid, _)| sid.clone())
            .collect()
    }

    fn expire(&mut self, now: Instant) {
        self.map.retain(|_, subscription| subscription.expires > now);
    }
}

fn parse_callback(value: &str) -> Option<Callback> {
    let start = value.find('<')? + 1;
    let end = value[start..].find('>')? + start;
    let url = &value[start..end];
    let rest = url.strip_prefix("http://")?;
    let (authority, path) = match rest.find('/') {
        Some(at) => (&rest[..at], &rest[at..]),
        None => (rest, "/"),
    };
    if authority.is_empty() {
        return None;
    }
    let (host, port) = if let Some(rest) = authority.strip_prefix('[') {
        let (host, tail) = rest.split_once(']')?;
        let port = tail
            .strip_prefix(':')
            .and_then(|port| port.parse().ok())
            .unwrap_or(80);
        (host.to_string(), port)
    } else if let Some((host, port)) = authority.rsplit_once(':') {
        (host.to_string(), port.parse().ok()?)
    } else {
        (authority.to_string(), 80)
    };
    Some(Callback {
        host,
        port,
        path: path.to_string(),
    })
}

fn parse_timeout(value: &str) -> u32 {
    let lower = value.trim().to_ascii_lowercase();
    if lower.contains("infinite") {
        return EVENT_TIMEOUT;
    }
    lower
        .rsplit_once('-')
        .and_then(|(_, seconds)| seconds.parse::<u32>().ok())
        .unwrap_or(EVENT_TIMEOUT)
        .clamp(60, 86_400)
}

fn post_notify(callback: &Callback, sid: &str, seq: u32, body: &str) -> std::io::Result<()> {
    let addr = (callback.host.as_str(), callback.port)
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "callback host"))?;
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_millis(500))?;
    stream.set_write_timeout(Some(Duration::from_millis(500)))?;
    write!(
        stream,
        "NOTIFY {} HTTP/1.1\r\nHOST: {}:{}\r\nCONTENT-TYPE: text/xml; charset=\"utf-8\"\r\n\
         NT: upnp:event\r\nNTS: upnp:propchange\r\nSID: {sid}\r\nSEQ: {seq}\r\n\
         CONTENT-LENGTH: {}\r\nCONNECTION: close\r\n\r\n",
        callback.path,
        callback.host,
        callback.port,
        body.len()
    )?;
    stream.write_all(body.as_bytes())?;
    stream.flush()
}

fn event_body(service: &str, state: &PlaybackState, has_next: bool) -> String {
    let property = match service {
        AVT => {
            let last = format!(
                "<Event xmlns=\"urn:schemas-upnp-org:metadata-1-0/AVT/\">\
                 <InstanceID val=\"0\">\
                 <TransportState val=\"{}\"/>\
                 <TransportStatus val=\"{}\"/>\
                 <CurrentTrackURI val=\"{}\"/>\
                 <CurrentTrackDuration val=\"{}\"/>\
                 <CurrentMediaDuration val=\"{}\"/>\
                 <RelativeTimePosition val=\"{}\"/>\
                 <AbsoluteTimePosition val=\"{}\"/>\
                 <CurrentTransportActions val=\"{}\"/>\
                 </InstanceID></Event>",
                transport_state(state.phase),
                if state.phase == Phase::Error {
                    "ERROR_OCCURRED"
                } else {
                    "OK"
                },
                escape(state.uri.as_deref().unwrap_or("")),
                hms(state.duration),
                hms(state.duration),
                hms(state.position),
                hms(state.position),
                transport_actions(state.phase, has_next),
            );
            ("LastChange".to_string(), last)
        }
        RCS => {
            let last = format!(
                "<Event xmlns=\"urn:schemas-upnp-org:metadata-1-0/RCS/\">\
                 <InstanceID val=\"0\">\
                 <Volume channel=\"Master\" val=\"{}\"/>\
                 <Mute channel=\"Master\" val=\"{}\"/>\
                 </InstanceID></Event>",
                (state.volume * 100.0).round().clamp(0.0, 100.0) as u32,
                u8::from(state.muted),
            );
            ("LastChange".to_string(), last)
        }
        _ => (
            "SinkProtocolInfo".to_string(),
            SINK_PROTOCOL_INFO.to_string(),
        ),
    };
    let mut props = format!(
        "<e:property><{name}>{}</{name}></e:property>",
        escape(&property.1),
        name = property.0
    );
    if service == CMS {
        props.push_str("<e:property><SourceProtocolInfo></SourceProtocolInfo></e:property>");
        props.push_str("<e:property><CurrentConnectionIDs>0</CurrentConnectionIDs></e:property>");
    }
    format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\
         <e:propertyset xmlns:e=\"urn:schemas-upnp-org:event-1-0\">{props}</e:propertyset>"
    )
}

#[derive(Debug, Clone, PartialEq)]
struct EventKey {
    phase: Phase,
    uri: Option<String>,
    volume: u8,
    muted: bool,
    position_s: u64,
    duration_s: u64,
    has_next: bool,
}

impl EventKey {
    fn of(state: &PlaybackState, has_next: bool) -> Self {
        Self {
            phase: state.phase,
            uri: state.uri.clone(),
            volume: (state.volume * 100.0).round().clamp(0.0, 100.0) as u8,
            muted: state.muted,
            position_s: state.position.max(0.0) as u64,
            duration_s: state.duration.max(0.0) as u64,
            has_next,
        }
    }
}

// -------------------------------------------------------------------- http

struct Request {
    method: String,
    path: String,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

struct Response {
    status: u16,
    content_type: &'static str,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    /// A subscription that must receive its initial `LastChange` after this
    /// response has been written.
    initial_event: Option<String>,
}

impl Response {
    fn new(status: u16, content_type: &'static str, body: String) -> Self {
        Self {
            status,
            content_type,
            headers: Vec::new(),
            body: body.into_bytes(),
            initial_event: None,
        }
    }

    fn xml(status: u16, body: String) -> Self {
        Self::new(status, "text/xml; charset=\"utf-8\"", body)
    }

    fn html(status: u16, body: String) -> Self {
        Self::new(status, "text/html; charset=\"utf-8\"", body)
    }

    fn text(status: u16, body: String) -> Self {
        Self::new(status, "text/plain; charset=\"utf-8\"", body)
    }

    fn empty(status: u16, headers: Vec<(String, String)>) -> Self {
        Self {
            status,
            content_type: "text/xml; charset=\"utf-8\"",
            headers,
            body: Vec::new(),
            initial_event: None,
        }
    }
}

/// Verification trace for the control plane: `HORIZON_DLNA_TRACE=1` prints one
/// line per socket event so a reset can be attributed to a side. Epoch
/// milliseconds so a client log written at the same time lines up with it.
fn trace_enabled() -> bool {
    std::env::var_os("HORIZON_DLNA_TRACE").is_some()
}

fn trace(message: impl FnOnce() -> String) {
    if trace_enabled() {
        let ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        eprintln!("[dlna {ms}ms] {}", message());
    }
}

fn peer_port(stream: &TcpStream) -> u16 {
    stream.peer_addr().map(|a| a.port()).unwrap_or(0)
}

fn read_request(reader: &mut BufReader<TcpStream>) -> Result<Request, u16> {
    let mut budget = 64 * 1024usize;
    let start = match read_line(reader, &mut budget) {
        Ok(line) => line,
        // 0 is reserved for "the peer hung up": on a kept-alive connection
        // that is the normal end of the conversation, not a bad request.
        Err(0) => return Err(0),
        Err(_) => return Err(400),
    };
    if start.is_empty() {
        return Err(0);
    }
    let mut parts = start.split_whitespace();
    let method = parts.next().ok_or(400u16)?.to_ascii_uppercase();
    let target = parts.next().ok_or(400u16)?;
    let version = parts.next().unwrap_or("HTTP/1.1");
    if !version.starts_with("HTTP/1.") {
        return Err(400);
    }
    let path = target
        .strip_prefix("http://")
        .map(|rest| match rest.find('/') {
            Some(at) => &rest[at..],
            None => "/",
        })
        .unwrap_or(target)
        .split('?')
        .next()
        .unwrap_or("/")
        .to_string();

    let mut headers = HashMap::new();
    loop {
        let line = read_line(reader, &mut budget)?;
        if line.is_empty() {
            break;
        }
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
    }

    if headers
        .get("expect")
        .map(|value| value.eq_ignore_ascii_case("100-continue"))
        .unwrap_or(false)
    {
        let _ = reader.get_mut().write_all(b"HTTP/1.1 100 Continue\r\n\r\n");
    }

    let length = match headers.get("content-length") {
        Some(value) => value.parse::<usize>().map_err(|_| 400u16)?,
        None => 0,
    };
    if length > 1024 * 1024 {
        return Err(413);
    }
    if method == "POST" && headers.get("transfer-encoding").is_some() {
        // Chunked bodies never appear in UPnP control; refusing beats guessing.
        return Err(411);
    }
    let mut body = vec![0u8; length];
    if length > 0 {
        if let Err(e) = reader.read_exact(&mut body) {
            if e.kind() == std::io::ErrorKind::UnexpectedEof {
                return Err(0);
            }
            return Err(400);
        }
    }
    Ok(Request {
        method,
        path,
        headers,
        body,
    })
}

fn read_line(reader: &mut impl BufRead, budget: &mut usize) -> Result<String, u16> {
    let mut line = String::new();
    let n = match reader.read_line(&mut line) {
        Ok(n) => n,
        // A read timeout is this thread giving up on the socket, not a request
        // worth an error page: 0 ends the conversation quietly.
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
            ) =>
        {
            return Err(0)
        }
        Err(_) => return Err(400),
    };
    if n == 0 {
        return Err(0);
    }
    *budget = budget.saturating_sub(n);
    if *budget == 0 {
        return Err(431);
    }
    Ok(line.trim_end_matches(['\r', '\n']).to_string())
}

fn write_response(
    stream: &mut TcpStream,
    method: &str,
    response: &Response,
) -> std::io::Result<()> {
    let mut head = format!(
        "HTTP/1.1 {} {}\r\nSERVER: {SERVER}\r\nDATE: {}\r\nCONTENT-TYPE: {}\r\nCONTENT-LENGTH: {}\r\nCONNECTION: close\r\nEXT:\r\n",
        response.status,
        status_reason(response.status),
        http_date(SystemTime::now()),
        response.content_type,
        response.body.len()
    );
    for (name, value) in &response.headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");
    // One segment for the whole reply: a client that sees the connection end
    // while the body is still on its way reports a reset rather than the
    // answer it was given.
    let mut reply = head.into_bytes();
    if method != "HEAD" {
        reply.extend_from_slice(&response.body);
    }
    stream.write_all(&reply)
}

fn status_reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        411 => "Length Required",
        412 => "Precondition Failed",
        413 => "Payload Too Large",
        431 => "Request Header Fields Too Large",
        _ => "Internal Server Error",
    }
}

// --------------------------------------------------------------------- soap

fn action_of(request: &Request, service_type: &str) -> Option<String> {
    if let Some(value) = request.headers.get("soapaction") {
        let value = value.trim().trim_matches('"');
        if let Some((service, action)) = value.rsplit_once('#') {
            if !service.eq_ignore_ascii_case(service_type) {
                return None;
            }
            let action = action.trim();
            return (!action.is_empty()).then(|| action.to_string());
        }
        return None;
    }
    let body = String::from_utf8_lossy(&request.body);
    ["SetAVTransportURI", "SetNextAVTransportURI", "GetTransportInfo",
     "GetPositionInfo", "GetMediaInfo", "GetDeviceCapabilities",
     "GetTransportSettings", "GetCurrentTransportActions", "GetCurrentConnectionInfo",
     "GetCurrentConnectionIDs", "GetProtocolInfo", "ListPresets", "SelectPreset",
     "Play", "Pause", "Stop", "Seek", "Next", "Previous", "GetVolume", "SetVolume",
     "GetMute", "SetMute"]
        .iter()
        .find(|action| xml_tag(&body, action).is_some())
        .map(|action| (*action).to_string())
}

fn soap_ok(service: &str, action: &str, inner: &str) -> Response {
    Response::xml(
        200,
        format!(
            "<?xml version=\"1.0\" encoding=\"utf-8\"?>\
             <s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\" \
             s:encodingStyle=\"http://schemas.xmlsoap.org/soap/encoding/\">\
             <s:Body><u:{action}Response xmlns:u=\"urn:schemas-upnp-org:service:{service}:1\">\
             {inner}</u:{action}Response></s:Body></s:Envelope>"
        ),
    )
}

fn soap_error(code: u16, description: &str) -> Response {
    Response::xml(
        500,
        format!(
            "<?xml version=\"1.0\" encoding=\"utf-8\"?>\
             <s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\" \
             s:encodingStyle=\"http://schemas.xmlsoap.org/soap/encoding/\">\
             <s:Body><s:Fault><faultcode>s:Client</faultcode>\
             <faultstring>UPnPError</faultstring><detail>\
             <UPnPError xmlns=\"urn:schemas-upnp-org:control-1-0\">\
             <errorCode>{code}</errorCode>\
             <errorDescription>{description}</errorDescription>\
             </UPnPError></detail></s:Fault></s:Body></s:Envelope>"
        ),
    )
}

// ------------------------------------------------------------------ helpers

fn transport_state(phase: Phase) -> &'static str {
    match phase {
        Phase::Idle => "NO_MEDIA_PRESENT",
        Phase::Loading => "TRANSITIONING",
        Phase::Playing => "PLAYING",
        Phase::Paused => "PAUSED_PLAYBACK",
        Phase::Stopped | Phase::Ended | Phase::Error => "STOPPED",
    }
}

fn transport_actions(phase: Phase, has_next: bool) -> String {
    let base = match phase {
        Phase::Idle | Phase::Loading => "",
        Phase::Playing => "Play,Pause,Stop,Seek",
        Phase::Paused => "Play,Stop,Seek",
        Phase::Stopped | Phase::Ended => "Play,Seek",
        Phase::Error => "",
    };
    // `Next` is only listed when the queue really has something after this;
    // `Previous` is never listed, because stepping back is always allowed on a
    // player but this transport cannot say which item that would land on.
    if base.is_empty() || !has_next {
        base.to_string()
    } else {
        format!("{base},Next")
    }
}

/// The only mime this device can name is the one the address itself spells out.
/// A library address is `/media/<id>` — the hub answers with the real
/// `Content-Type`, but the renderer has no reason to guess one here, so the
/// wildcard is what a controller gets.
fn mime_of(uri: &str) -> &'static str {
    let lower = uri.to_ascii_lowercase();
    let path = lower.split(['?', '#']).next().unwrap_or(&lower);
    [
        (".mp4", "video/mp4"),
        (".m4v", "video/mp4"),
        (".mkv", "video/x-matroska"),
        (".avi", "video/x-msvideo"),
        (".mpeg", "video/mpeg"),
        (".mpg", "video/mpeg"),
        (".ts", "video/mp2t"),
        (".mov", "video/quicktime"),
        (".webm", "video/webm"),
        (".mp3", "audio/mpeg"),
        (".m4a", "audio/mp4"),
        (".wav", "audio/wav"),
        (".flac", "audio/flac"),
        (".ogg", "audio/ogg"),
        (".opus", "audio/opus"),
        (".wma", "audio/x-ms-wma"),
        (".jpg", "image/jpeg"),
        (".jpeg", "image/jpeg"),
        (".png", "image/png"),
        (".gif", "image/gif"),
        (".bmp", "image/bmp"),
        (".webp", "image/webp"),
    ]
    .iter()
    .find(|(ext, _)| path.ends_with(ext))
    .map(|(_, mime)| *mime)
    .unwrap_or("*")
}

/// DIDL-Lite for the item on the wire. A controller draws exactly what is in
/// here, so the class has to be right — a photo announced as a video item is a
/// grey tile on a TV — and nothing goes in that this device does not know:
/// no invented album art, no fake duration, no made-up artist.
fn didl_metadata(track: &Track) -> String {
    let class = match track.kind {
        MediaKind::Video => "object.item.videoItem",
        MediaKind::Audio => "object.item.audioItem.musicTrack",
        MediaKind::Image => "object.item.imageItem.photo",
    };
    let protocol = format!("http-get:*:{}:*", mime_of(&track.uri));
    format!(
        "<DIDL-Lite xmlns=\"urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/\" \
         xmlns:dc=\"http://purl.org/dc/elements/1.1/\" \
         xmlns:upnp=\"urn:schemas-upnp-org:metadata-1-0/upnp/\"> \
         <item id=\"{}\" parentID=\"-1\" restricted=\"1\"> \
         <dc:title>{}</dc:title> \
         <upnp:class>{}</upnp:class> \
         <res protocolInfo=\"{}\">{}</res> \
         </item> </DIDL-Lite>",
        escape(&track.id),
        escape(&track.title),
        class,
        escape(&protocol),
        escape(&track.uri)
    )
}

fn hms(seconds: f64) -> String {
    if !seconds.is_finite() || seconds < 0.0 {
        return "0:00:00".to_string();
    }
    let total = seconds as u64;
    format!(
        "{}:{:02}:{:02}",
        total / 3600,
        (total % 3600) / 60,
        total % 60
    )
}

fn parse_hms(value: &str) -> Option<f64> {
    let value = value.trim();
    let parts: Vec<&str> = value.split(':').collect();
    let (hours, minutes, seconds) = match parts.len() {
        3 => (
            parts[0].parse::<f64>().ok()?,
            parts[1].parse::<f64>().ok()?,
            parts[2].parse::<f64>().ok()?,
        ),
        2 => (0.0, parts[0].parse::<f64>().ok()?, parts[1].parse::<f64>().ok()?),
        1 => (0.0, 0.0, parts[0].parse::<f64>().ok()?),
        _ => return None,
    };
    if hours < 0.0 || minutes < 0.0 || seconds < 0.0 || minutes >= 60.0 || seconds >= 60.0 {
        return None;
    }
    Some(hours * 3600.0 + minutes * 60.0 + seconds)
}

fn xml_tag(body: &str, name: &str) -> Option<String> {
    let mut from = 0;
    while let Some(relative) = body[from..].find('<') {
        let open = from + relative;
        let bytes = body.as_bytes();
        let mut cursor = open + 1;
        if cursor >= bytes.len() {
            return None;
        }
        match bytes[cursor] {
            b'!' | b'?' | b'/' => {
                let end = body[cursor..].find('>')? + cursor;
                from = end + 1;
                continue;
            }
            _ => {}
        }
        let name_start = cursor;
        while cursor < bytes.len()
            && (bytes[cursor].is_ascii_alphanumeric()
                || matches!(bytes[cursor], b'_' | b':' | b'-' | b'.'))
        {
            cursor += 1;
        }
        let qname = &body[name_start..cursor];
        let local = qname.rsplit(':').next().unwrap_or(qname);
        let Some(tag_end) = find_tag_end(body, open + 1) else {
            return None;
        };
        let self_closing = body[open..=tag_end].trim_end().ends_with("/>");
        if local != name {
            from = tag_end + 1;
            continue;
        }
        if self_closing {
            return Some(String::new());
        }
        let content_start = tag_end + 1;
        let close = find_close(body, content_start, local)?;
        let mut raw = body[content_start..close].trim();
        if let Some(inner) = raw
            .strip_prefix("<![CDATA[")
            .and_then(|raw| raw.strip_suffix("]]>"))
        {
            raw = inner;
        }
        return Some(unescape(raw));
    }
    None
}

fn find_tag_end(body: &str, from: usize) -> Option<usize> {
    let bytes = body.as_bytes();
    let mut quote: Option<u8> = None;
    for (offset, byte) in bytes.iter().enumerate().skip(from) {
        match quote {
            Some(active) => {
                if *byte == active {
                    quote = None;
                }
            }
            None => match *byte {
                b'"' | b'\'' => quote = Some(*byte),
                b'>' => return Some(offset),
                _ => {}
            },
        }
    }
    None
}

fn find_close(body: &str, from: usize, local: &str) -> Option<usize> {
    let mut cursor = from;
    while cursor < body.len() {
        let Some(relative) = body[cursor..].find('<') else {
            return None;
        };
        let start = cursor + relative;
        let rest = &body[start..];
        if rest.starts_with("<![CDATA[") {
            cursor = rest.find("]]>").map(|at| start + at + 3)?;
            continue;
        }
        if rest.starts_with("<!--") {
            cursor = rest.find("-->").map(|at| start + at + 3)?;
            continue;
        }
        if !rest.starts_with("</") {
            cursor = start + 1;
            continue;
        }
        let name_start = start + 2;
        let after = &body[name_start..];
        let end = after.find('>')?;
        let qname = after[..end].trim();
        if qname.rsplit(':').next().unwrap_or(qname) == local {
            return Some(start);
        }
        cursor = name_start + end;
    }
    None
}

fn escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn unescape(value: &str) -> String {
    value
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

fn parse_m_search(text: &str) -> Option<HashMap<String, String>> {
    let mut lines = text.split("\r\n");
    let start = lines.next()?;
    if !start.starts_with("M-SEARCH ") {
        return None;
    }
    let mut headers = HashMap::new();
    for line in lines {
        if line.is_empty() {
            break;
        }
        let (name, value) = match line.split_once(':') {
            Some(pair) => pair,
            None => continue,
        };
        headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
    }
    Some(headers)
}

fn search_matches(st: &str, nt: &str) -> bool {
    st.eq_ignore_ascii_case("ssdp:all") || st.eq_ignore_ascii_case(nt)
}

fn http_date(time: SystemTime) -> String {
    let seconds = time
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0);
    let days = seconds.div_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let remainder = seconds.rem_euclid(86_400);
    let weekday = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"]
        [days.rem_euclid(7) as usize];
    let months = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    format!(
        "{weekday}, {day:02} {} {year:04} {:02}:{:02}:{:02} GMT",
        months[(month - 1) as usize],
        remainder / 3600,
        (remainder % 3600) / 60,
        remainder % 60
    )
}

/// Howard Hinnant's `civil_from_days`: days since 1970-01-01 to a calendar date.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = (if z >= 0 { z } else { z - 146_096 }) / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if month <= 2 { year + 1 } else { year }, month, day)
}

// ------------------------------------------------------------- descriptions

fn device_description(info: &DmrInfo) -> String {
    let mut services = String::new();
    for service in &SERVICES {
        services.push_str(&format!(
            "<service><serviceType>{}</serviceType>\
             <serviceId>urn:upnp-org:serviceId:{}</serviceId>\
             <SCPDURL>{}</SCPDURL><controlURL>{}</controlURL><eventSubURL>{}</eventSubURL>\
             </service>",
            service.kind, service.name, service.scpd, service.control, service.event
        ));
    }
    format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\
         <root xmlns=\"urn:schemas-upnp-org:device-1-0\" xmlns:dlna=\"urn:schemas-dlna-org:device-1-0\">\
         <specVersion><major>1</major><minor>0</minor></specVersion>\
         <device>\
         <deviceType>{DEVICE_TYPE}</deviceType>\
         <dlna:X_DLNADOC>DMR-1.50</dlna:X_DLNADOC>\
         <friendlyName>{}</friendlyName>\
         <manufacturer>HORIZON</manufacturer>\
         <modelDescription>HORIZON meeting hub renderer</modelDescription>\
         <modelName>HORIZON Cast Receiver</modelName>\
         <modelNumber>0.1</modelNumber>\
         <serialNumber>horizon-dmr</serialNumber>\
         <UDN>{}</UDN>\
         <serviceList>{services}</serviceList>\
         <presentationURL>/</presentationURL>\
         </device></root>",
        escape(&info.friendly_name),
        escape(&info.udn),
    )
}

fn scpd(service: &str) -> String {
    match service {
        AVT => AVT_SCPD.to_string(),
        RCS => RCS_SCPD.to_string(),
        _ => CMS_SCPD.to_string(),
    }
}

const AVT_SCPD: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<scpd xmlns="urn:schemas-upnp-org:service-1-0">
<specVersion><major>1</major><minor>0</minor></specVersion>
<actionList>
<action><name>SetAVTransportURI</name><argumentList>
<argument><name>InstanceID</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_InstanceID</relatedStateVariable></argument>
<argument><name>CurrentURI</name><direction>in</direction><relatedStateVariable>AVTransportURI</relatedStateVariable></argument>
<argument><name>CurrentURIMetaData</name><direction>in</direction><relatedStateVariable>AVTransportURIMetaData</relatedStateVariable></argument>
</argumentList></action>
<action><name>SetNextAVTransportURI</name><argumentList>
<argument><name>InstanceID</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_InstanceID</relatedStateVariable></argument>
<argument><name>NextURI</name><direction>in</direction><relatedStateVariable>NextAVTransportURI</relatedStateVariable></argument>
<argument><name>NextURIMetaData</name><direction>in</direction><relatedStateVariable>NextAVTransportURIMetaData</relatedStateVariable></argument>
</argumentList></action>
<action><name>GetMediaInfo</name><argumentList>
<argument><name>InstanceID</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_InstanceID</relatedStateVariable></argument>
<argument><name>NrTracks</name><direction>out</direction><relatedStateVariable>NumberOfTracks</relatedStateVariable></argument>
<argument><name>MediaDuration</name><direction>out</direction><relatedStateVariable>CurrentMediaDuration</relatedStateVariable></argument>
<argument><name>CurrentURI</name><direction>out</direction><relatedStateVariable>AVTransportURI</relatedStateVariable></argument>
<argument><name>CurrentURIMetaData</name><direction>out</direction><relatedStateVariable>AVTransportURIMetaData</relatedStateVariable></argument>
<argument><name>NextURI</name><direction>out</direction><relatedStateVariable>NextAVTransportURI</relatedStateVariable></argument>
<argument><name>NextURIMetaData</name><direction>out</direction><relatedStateVariable>NextAVTransportURIMetaData</relatedStateVariable></argument>
<argument><name>PlayMedium</name><direction>out</direction><relatedStateVariable>PlaybackStorageMedium</relatedStateVariable></argument>
<argument><name>RecordMedium</name><direction>out</direction><relatedStateVariable>RecordStorageMedium</relatedStateVariable></argument>
<argument><name>WriteStatus</name><direction>out</direction><relatedStateVariable>RecordMediumWriteStatus</relatedStateVariable></argument>
</argumentList></action>
<action><name>GetTransportInfo</name><argumentList>
<argument><name>InstanceID</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_InstanceID</relatedStateVariable></argument>
<argument><name>CurrentTransportState</name><direction>out</direction><relatedStateVariable>TransportState</relatedStateVariable></argument>
<argument><name>CurrentTransportStatus</name><direction>out</direction><relatedStateVariable>TransportStatus</relatedStateVariable></argument>
<argument><name>CurrentSpeed</name><direction>out</direction><relatedStateVariable>TransportPlaySpeed</relatedStateVariable></argument>
</argumentList></action>
<action><name>GetPositionInfo</name><argumentList>
<argument><name>InstanceID</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_InstanceID</relatedStateVariable></argument>
<argument><name>Track</name><direction>out</direction><relatedStateVariable>CurrentTrack</relatedStateVariable></argument>
<argument><name>TrackDuration</name><direction>out</direction><relatedStateVariable>CurrentTrackDuration</relatedStateVariable></argument>
<argument><name>TrackMetaData</name><direction>out</direction><relatedStateVariable>CurrentTrackMetaData</relatedStateVariable></argument>
<argument><name>TrackURI</name><direction>out</direction><relatedStateVariable>CurrentTrackURI</relatedStateVariable></argument>
<argument><name>RelTime</name><direction>out</direction><relatedStateVariable>RelativeTimePosition</relatedStateVariable></argument>
<argument><name>AbsTime</name><direction>out</direction><relatedStateVariable>AbsoluteTimePosition</relatedStateVariable></argument>
<argument><name>RelCount</name><direction>out</direction><relatedStateVariable>RelativeCounterPosition</relatedStateVariable></argument>
<argument><name>AbsCount</name><direction>out</direction><relatedStateVariable>AbsoluteCounterPosition</relatedStateVariable></argument>
</argumentList></action>
<action><name>GetDeviceCapabilities</name><argumentList>
<argument><name>InstanceID</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_InstanceID</relatedStateVariable></argument>
<argument><name>PlayMedia</name><direction>out</direction><relatedStateVariable>PossiblePlaybackStorageMedia</relatedStateVariable></argument>
<argument><name>RecMedia</name><direction>out</direction><relatedStateVariable>PossibleRecordStorageMedia</relatedStateVariable></argument>
<argument><name>RecQualityModes</name><direction>out</direction><relatedStateVariable>PossibleRecordQualityModes</relatedStateVariable></argument>
</argumentList></action>
<action><name>GetTransportSettings</name><argumentList>
<argument><name>InstanceID</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_InstanceID</relatedStateVariable></argument>
<argument><name>PlayMode</name><direction>out</direction><relatedStateVariable>CurrentPlayMode</relatedStateVariable></argument>
<argument><name>RecQualityMode</name><direction>out</direction><relatedStateVariable>CurrentRecordQualityMode</relatedStateVariable></argument>
</argumentList></action>
<action><name>Stop</name><argumentList>
<argument><name>InstanceID</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_InstanceID</relatedStateVariable></argument>
</argumentList></action>
<action><name>Play</name><argumentList>
<argument><name>InstanceID</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_InstanceID</relatedStateVariable></argument>
<argument><name>Speed</name><direction>in</direction><relatedStateVariable>TransportPlaySpeed</relatedStateVariable></argument>
</argumentList></action>
<action><name>Pause</name><argumentList>
<argument><name>InstanceID</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_InstanceID</relatedStateVariable></argument>
</argumentList></action>
<action><name>Seek</name><argumentList>
<argument><name>InstanceID</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_InstanceID</relatedStateVariable></argument>
<argument><name>Unit</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_SeekMode</relatedStateVariable></argument>
<argument><name>Target</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_SeekTarget</relatedStateVariable></argument>
</argumentList></action>
<action><name>Next</name><argumentList>
<argument><name>InstanceID</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_InstanceID</relatedStateVariable></argument>
</argumentList></action>
<action><name>Previous</name><argumentList>
<argument><name>InstanceID</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_InstanceID</relatedStateVariable></argument>
</argumentList></action>
<action><name>GetCurrentTransportActions</name><argumentList>
<argument><name>InstanceID</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_InstanceID</relatedStateVariable></argument>
<argument><name>Actions</name><direction>out</direction><relatedStateVariable>CurrentTransportActions</relatedStateVariable></argument>
</argumentList></action>
</actionList>
<serviceStateTable>
<stateVariable sendEvents="no"><name>A_ARG_TYPE_InstanceID</name><dataType>ui4</dataType></stateVariable>
<stateVariable sendEvents="no"><name>A_ARG_TYPE_SeekMode</name><dataType>string</dataType><allowedValueList><allowedValue>REL_TIME</allowedValue><allowedValue>ABS_TIME</allowedValue></allowedValueList></stateVariable>
<stateVariable sendEvents="no"><name>A_ARG_TYPE_SeekTarget</name><dataType>string</dataType></stateVariable>
<stateVariable sendEvents="no"><name>AVTransportURI</name><dataType>string</dataType></stateVariable>
<stateVariable sendEvents="no"><name>AVTransportURIMetaData</name><dataType>string</dataType></stateVariable>
<stateVariable sendEvents="no"><name>NextAVTransportURI</name><dataType>string</dataType></stateVariable>
<stateVariable sendEvents="no"><name>NextAVTransportURIMetaData</name><dataType>string</dataType></stateVariable>
<stateVariable sendEvents="no"><name>CurrentMediaDuration</name><dataType>string</dataType></stateVariable>
<stateVariable sendEvents="no"><name>CurrentTrack</name><dataType>ui4</dataType></stateVariable>
<stateVariable sendEvents="no"><name>CurrentTrackDuration</name><dataType>string</dataType></stateVariable>
<stateVariable sendEvents="no"><name>CurrentTrackMetaData</name><dataType>string</dataType></stateVariable>
<stateVariable sendEvents="no"><name>CurrentTrackURI</name><dataType>string</dataType></stateVariable>
<stateVariable sendEvents="no"><name>RelativeTimePosition</name><dataType>string</dataType></stateVariable>
<stateVariable sendEvents="no"><name>AbsoluteTimePosition</name><dataType>string</dataType></stateVariable>
<stateVariable sendEvents="no"><name>RelativeCounterPosition</name><dataType>i4</dataType></stateVariable>
<stateVariable sendEvents="no"><name>AbsoluteCounterPosition</name><dataType>i4</dataType></stateVariable>
<stateVariable sendEvents="no"><name>TransportState</name><dataType>string</dataType><allowedValueList><allowedValue>STOPPED</allowedValue><allowedValue>PLAYING</allowedValue><allowedValue>TRANSITIONING</allowedValue><allowedValue>PAUSED_PLAYBACK</allowedValue><allowedValue>NO_MEDIA_PRESENT</allowedValue></allowedValueList></stateVariable>
<stateVariable sendEvents="no"><name>TransportStatus</name><dataType>string</dataType><allowedValueList><allowedValue>OK</allowedValue><allowedValue>ERROR_OCCURRED</allowedValue></allowedValueList></stateVariable>
<stateVariable sendEvents="no"><name>PlaybackStorageMedium</name><dataType>string</dataType><allowedValueList><allowedValue>NONE</allowedValue><allowedValue>NETWORK</allowedValue></allowedValueList></stateVariable>
<stateVariable sendEvents="no"><name>RecordStorageMedium</name><dataType>string</dataType><allowedValueList><allowedValue>NOT_IMPLEMENTED</allowedValue></allowedValueList></stateVariable>
<stateVariable sendEvents="no"><name>PossiblePlaybackStorageMedia</name><dataType>string</dataType></stateVariable>
<stateVariable sendEvents="no"><name>PossibleRecordStorageMedia</name><dataType>string</dataType></stateVariable>
<stateVariable sendEvents="no"><name>CurrentPlayMode</name><dataType>string</dataType><allowedValueList><allowedValue>NORMAL</allowedValue></allowedValueList><defaultValue>NORMAL</defaultValue></stateVariable>
<stateVariable sendEvents="no"><name>TransportPlaySpeed</name><dataType>string</dataType><allowedValueList><allowedValue>1</allowedValue></allowedValueList></stateVariable>
<stateVariable sendEvents="no"><name>RecordMediumWriteStatus</name><dataType>string</dataType><allowedValueList><allowedValue>NOT_IMPLEMENTED</allowedValue></allowedValueList></stateVariable>
<stateVariable sendEvents="no"><name>CurrentRecordQualityMode</name><dataType>string</dataType><allowedValueList><allowedValue>NOT_IMPLEMENTED</allowedValue></allowedValueList></stateVariable>
<stateVariable sendEvents="no"><name>PossibleRecordQualityModes</name><dataType>string</dataType></stateVariable>
<stateVariable sendEvents="no"><name>NumberOfTracks</name><dataType>ui4</dataType></stateVariable>
<stateVariable sendEvents="no"><name>CurrentTransportActions</name><dataType>string</dataType></stateVariable>
<stateVariable sendEvents="yes"><name>LastChange</name><dataType>string</dataType></stateVariable>
</serviceStateTable>
</scpd>
"#;

const RCS_SCPD: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<scpd xmlns="urn:schemas-upnp-org:service-1-0">
<specVersion><major>1</major><minor>0</minor></specVersion>
<actionList>
<action><name>ListPresets</name><argumentList>
<argument><name>InstanceID</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_InstanceID</relatedStateVariable></argument>
<argument><name>CurrentPresetNameList</name><direction>out</direction><relatedStateVariable>PresetNameList</relatedStateVariable></argument>
</argumentList></action>
<action><name>SelectPreset</name><argumentList>
<argument><name>InstanceID</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_InstanceID</relatedStateVariable></argument>
<argument><name>PresetName</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_PresetName</relatedStateVariable></argument>
</argumentList></action>
<action><name>GetMute</name><argumentList>
<argument><name>InstanceID</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_InstanceID</relatedStateVariable></argument>
<argument><name>Channel</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_Channel</relatedStateVariable></argument>
<argument><name>CurrentMute</name><direction>out</direction><relatedStateVariable>Mute</relatedStateVariable></argument>
</argumentList></action>
<action><name>SetMute</name><argumentList>
<argument><name>InstanceID</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_InstanceID</relatedStateVariable></argument>
<argument><name>Channel</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_Channel</relatedStateVariable></argument>
<argument><name>DesiredMute</name><direction>in</direction><relatedStateVariable>Mute</relatedStateVariable></argument>
</argumentList></action>
<action><name>GetVolume</name><argumentList>
<argument><name>InstanceID</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_InstanceID</relatedStateVariable></argument>
<argument><name>Channel</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_Channel</relatedStateVariable></argument>
<argument><name>CurrentVolume</name><direction>out</direction><relatedStateVariable>Volume</relatedStateVariable></argument>
</argumentList></action>
<action><name>SetVolume</name><argumentList>
<argument><name>InstanceID</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_InstanceID</relatedStateVariable></argument>
<argument><name>Channel</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_Channel</relatedStateVariable></argument>
<argument><name>DesiredVolume</name><direction>in</direction><relatedStateVariable>Volume</relatedStateVariable></argument>
</argumentList></action>
</actionList>
<serviceStateTable>
<stateVariable sendEvents="no"><name>A_ARG_TYPE_InstanceID</name><dataType>ui4</dataType></stateVariable>
<stateVariable sendEvents="no"><name>A_ARG_TYPE_Channel</name><dataType>string</dataType><allowedValueList><allowedValue>Master</allowedValue></allowedValueList></stateVariable>
<stateVariable sendEvents="no"><name>A_ARG_TYPE_PresetName</name><dataType>string</dataType><allowedValueList><allowedValue>FactoryDefaults</allowedValue></allowedValueList></stateVariable>
<stateVariable sendEvents="no"><name>PresetNameList</name><dataType>string</dataType></stateVariable>
<stateVariable sendEvents="no"><name>Mute</name><dataType>boolean</dataType></stateVariable>
<stateVariable sendEvents="no"><name>Volume</name><dataType>ui2</dataType><allowedValueRange><minimum>0</minimum><maximum>100</maximum><step>1</step></allowedValueRange></stateVariable>
<stateVariable sendEvents="yes"><name>LastChange</name><dataType>string</dataType></stateVariable>
</serviceStateTable>
</scpd>
"#;

const CMS_SCPD: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<scpd xmlns="urn:schemas-upnp-org:service-1-0">
<specVersion><major>1</major><minor>0</minor></specVersion>
<actionList>
<action><name>GetProtocolInfo</name><argumentList>
<argument><name>Source</name><direction>out</direction><relatedStateVariable>SourceProtocolInfo</relatedStateVariable></argument>
<argument><name>Sink</name><direction>out</direction><relatedStateVariable>SinkProtocolInfo</relatedStateVariable></argument>
</argumentList></action>
<action><name>GetCurrentConnectionIDs</name><argumentList>
<argument><name>ConnectionIDs</name><direction>out</direction><relatedStateVariable>CurrentConnectionIDs</relatedStateVariable></argument>
</argumentList></action>
<action><name>GetCurrentConnectionInfo</name><argumentList>
<argument><name>ConnectionID</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_ConnectionID</relatedStateVariable></argument>
<argument><name>RcsID</name><direction>out</direction><relatedStateVariable>A_ARG_TYPE_RcsID</relatedStateVariable></argument>
<argument><name>AVTransportID</name><direction>out</direction><relatedStateVariable>A_ARG_TYPE_AVTransportID</relatedStateVariable></argument>
<argument><name>ProtocolInfo</name><direction>out</direction><relatedStateVariable>A_ARG_TYPE_ProtocolInfo</relatedStateVariable></argument>
<argument><name>PeerConnectionManager</name><direction>out</direction><relatedStateVariable>A_ARG_TYPE_ConnectionManager</relatedStateVariable></argument>
<argument><name>PeerConnectionID</name><direction>out</direction><relatedStateVariable>A_ARG_TYPE_ConnectionID</relatedStateVariable></argument>
<argument><name>Direction</name><direction>out</direction><relatedStateVariable>A_ARG_TYPE_Direction</relatedStateVariable></argument>
<argument><name>Status</name><direction>out</direction><relatedStateVariable>A_ARG_TYPE_ConnectionStatus</relatedStateVariable></argument>
</argumentList></action>
</actionList>
<serviceStateTable>
<stateVariable sendEvents="yes"><name>SourceProtocolInfo</name><dataType>string</dataType></stateVariable>
<stateVariable sendEvents="yes"><name>SinkProtocolInfo</name><dataType>string</dataType></stateVariable>
<stateVariable sendEvents="yes"><name>CurrentConnectionIDs</name><dataType>string</dataType></stateVariable>
<stateVariable sendEvents="no"><name>A_ARG_TYPE_ConnectionStatus</name><dataType>string</dataType><allowedValueList><allowedValue>OK</allowedValue><allowedValue>ContentFormatMismatch</allowedValue><allowedValue>InsufficientBandwidth</allowedValue><allowedValue>UnreliableChannel</allowedValue><allowedValue>Unknown</allowedValue></allowedValueList></stateVariable>
<stateVariable sendEvents="no"><name>A_ARG_TYPE_ConnectionManager</name><dataType>string</dataType></stateVariable>
<stateVariable sendEvents="no"><name>A_ARG_TYPE_Direction</name><dataType>string</dataType><allowedValueList><allowedValue>Input</allowedValue><allowedValue>Output</allowedValue></allowedValueList></stateVariable>
<stateVariable sendEvents="no"><name>A_ARG_TYPE_ProtocolInfo</name><dataType>string</dataType></stateVariable>
<stateVariable sendEvents="no"><name>A_ARG_TYPE_ConnectionID</name><dataType>i4</dataType></stateVariable>
<stateVariable sendEvents="no"><name>A_ARG_TYPE_AVTransportID</name><dataType>i4</dataType></stateVariable>
<stateVariable sendEvents="no"><name>A_ARG_TYPE_RcsID</name><dataType>i4</dataType></stateVariable>
</serviceStateTable>
</scpd>
"#;

// ------------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct Fake {
        inner: Mutex<FakeInner>,
    }

    struct FakeInner {
        state: PlaybackState,
        loads: Vec<String>,
        seeks: Vec<f64>,
        next: Option<String>,
        has_next: bool,
    }

    impl Default for Fake {
        fn default() -> Self {
            Self {
                inner: Mutex::new(FakeInner {
                    state: PlaybackState::default(),
                    loads: Vec::new(),
                    seeks: Vec::new(),
                    next: None,
                    has_next: false,
                }),
            }
        }
    }

    impl Fake {
        fn inner(&self) -> std::sync::MutexGuard<'_, FakeInner> {
            self.inner.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
        }

        fn set_duration(&self, seconds: f64) {
            self.inner().state.duration = seconds;
        }

        fn set_has_next(&self, value: bool) {
            self.inner().has_next = value;
        }
    }

    impl Transport for Fake {
        fn load(&self, uri: &str) -> CastResult<()> {
            let mut inner = self.inner();
            inner.loads.push(uri.to_string());
            inner.state.uri = Some(uri.to_string());
            inner.state.phase = Phase::Stopped;
            inner.state.position = 0.0;
            Ok(())
        }

        fn play(&self) -> CastResult<()> {
            self.inner().state.phase = Phase::Playing;
            Ok(())
        }

        fn pause(&self) -> CastResult<()> {
            self.inner().state.phase = Phase::Paused;
            Ok(())
        }

        fn stop(&self) -> CastResult<()> {
            let mut inner = self.inner();
            inner.state.phase = Phase::Stopped;
            inner.state.position = 0.0;
            Ok(())
        }

        fn seek(&self, seconds: f64) -> CastResult<()> {
            let mut inner = self.inner();
            inner.seeks.push(seconds);
            inner.state.position = seconds;
            Ok(())
        }

        fn set_volume(&self, volume: f64) -> CastResult<()> {
            self.inner().state.volume = volume;
            Ok(())
        }

        fn set_mute(&self, muted: bool) -> CastResult<()> {
            self.inner().state.muted = muted;
            Ok(())
        }

        fn state(&self) -> PlaybackState {
            self.inner().state.clone()
        }

        fn set_next(&self, uri: &str) -> CastResult<()> {
            self.inner().next = Some(uri.to_string());
            Ok(())
        }

        fn next(&self) -> CastResult<()> {
            let mut inner = self.inner();
            if !inner.has_next {
                return Err(CastError::Unsupported("nothing after this"));
            }
            inner.state.uri = Some(
                inner
                    .next
                    .take()
                    .unwrap_or_else(|| "http://list/next".to_string()),
            );
            inner.state.phase = Phase::Stopped;
            inner.state.position = 0.0;
            Ok(())
        }

        fn previous(&self) -> CastResult<()> {
            let mut inner = self.inner();
            inner.state.uri = Some("http://list/previous".to_string());
            inner.state.phase = Phase::Stopped;
            Ok(())
        }

        fn has_next(&self) -> bool {
            self.inner().has_next
        }
    }

    fn start(fake: Arc<Fake>) -> Dmr {
        Dmr::start_with(
            DmrConfig {
                friendly_name: "Test Renderer".into(),
                bind: Ipv4Addr::LOCALHOST,
                port: 0,
                show_video: false,
                video_parent: None,
                advertise: false,
            },
            fake,
        )
        .expect("dmr")
    }

    fn http(port: u16, request: &str) -> String {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect");
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        stream.write_all(request.as_bytes()).unwrap();
        let mut out = String::new();
        let _ = stream.read_to_string(&mut out);
        out
    }

    fn soap_request(port: u16, service: &str, action: &str, inner: &str) -> String {
        let body = format!(
            "<?xml version=\"1.0\"?><s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\" \
             s:encodingStyle=\"http://schemas.xmlsoap.org/soap/encoding/\"><s:Body>\
             <u:{action} xmlns:u=\"urn:schemas-upnp-org:service:{service}:1\">{inner}</u:{action}>\
             </s:Body></s:Envelope>"
        );
        let request = format!(
            "POST /ctl/{service} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\
             SOAPACTION: \"urn:schemas-upnp-org:service:{service}:1#{action}\"\r\n\
             CONTENT-TYPE: text/xml; charset=\"utf-8\"\r\nCONTENT-LENGTH: {}\r\n\
             CONNECTION: close\r\n\r\n{body}",
            body.len()
        );
        http(port, &request)
    }

    #[test]
    #[ignore]
    fn control_plane_stress_listener() {
        let mut dmr = Dmr::start_with(
            DmrConfig {
                friendly_name: "HORIZON Stress".into(),
                bind: Ipv4Addr::UNSPECIFIED,
                port: 0,
                show_video: false,
                video_parent: None,
                advertise: false,
            },
            Arc::new(Fake::default()),
        )
        .expect("dmr");
        eprintln!("stress at {}", dmr.info().location);
        std::thread::sleep(Duration::from_secs(180));
        dmr.stop();
    }

    #[test]
    fn a_stable_udn_looks_like_a_uuid() {
        let a = udn_for("Living Room");
        let b = udn_for("Living Room");
        assert_eq!(a, b);
        assert_ne!(a, udn_for("Office"));
        let uuid = a.trim_start_matches("uuid:");
        assert_eq!(uuid.len(), 36);
        let parts: Vec<&str> = uuid.split('-').collect();
        assert_eq!(parts.len(), 5);
        assert_eq!(&parts[2][..1], "4");
    }

    #[test]
    fn soap_envelopes_carry_the_action() {
        let fake = Arc::new(Fake::default());
        let dmr = start(fake.clone());
        let port = dmr.info().port;
        let response = soap_request(
            port,
            "AVTransport",
            "SetAVTransportURI",
            "<InstanceID>0</InstanceID><CurrentURI>http://host/tone.wav</CurrentURI>\
             <CurrentURIMetaData></CurrentURIMetaData>",
        );
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        assert!(response.contains("SetAVTransportURIResponse"), "{response}");
        assert_eq!(fake.inner().loads, vec!["http://host/tone.wav"]);

        let info = soap_request(
            port,
            "AVTransport",
            "GetTransportInfo",
            "<InstanceID>0</InstanceID>",
        );
        assert!(info.contains("<CurrentTransportState>STOPPED</CurrentTransportState>"), "{info}");
        assert!(info.contains("<CurrentTransportStatus>OK</CurrentTransportStatus>"), "{info}");
    }

    #[test]
    fn play_pause_stop_seek_and_volume_reach_the_transport() {
        let fake = Arc::new(Fake::default());
        let dmr = start(fake.clone());
        let port = dmr.info().port;
        fake.set_duration(10.0);

        let play = soap_request(port, "AVTransport", "Play", "<InstanceID>0</InstanceID><Speed>1</Speed>");
        assert!(play.starts_with("HTTP/1.1 200"), "{play}");
        assert_eq!(fake.inner().state.phase, Phase::Playing);

        let info = soap_request(port, "AVTransport", "GetTransportInfo", "<InstanceID>0</InstanceID>");
        assert!(info.contains("<CurrentTransportState>PLAYING</CurrentTransportState>"), "{info}");
        assert!(info.contains("<CurrentSpeed>1</CurrentSpeed>"), "{info}");

        let position = soap_request(port, "AVTransport", "GetPositionInfo", "<InstanceID>0</InstanceID>");
        assert!(position.contains("<TrackDuration>0:00:10</TrackDuration>"), "{position}");

        let seek = soap_request(
            port,
            "AVTransport",
            "Seek",
            "<InstanceID>0</InstanceID><Unit>REL_TIME</Unit><Target>0:00:04</Target>",
        );
        assert!(seek.starts_with("HTTP/1.1 200"), "{seek}");
        assert_eq!(fake.inner().seeks, vec![4.0]);

        let volume = soap_request(
            port,
            "RenderingControl",
            "SetVolume",
            "<InstanceID>0</InstanceID><Channel>Master</Channel><DesiredVolume>25</DesiredVolume>",
        );
        assert!(volume.starts_with("HTTP/1.1 200"), "{volume}");
        let volume = soap_request(
            port,
            "RenderingControl",
            "GetVolume",
            "<InstanceID>0</InstanceID><Channel>Master</Channel>",
        );
        assert!(volume.contains("<CurrentVolume>25</CurrentVolume>"), "{volume}");
        assert!((fake.inner().state.volume - 0.25).abs() < 1e-9);

        let pause = soap_request(port, "AVTransport", "Pause", "<InstanceID>0</InstanceID>");
        assert!(pause.starts_with("HTTP/1.1 200"), "{pause}");
        assert_eq!(fake.inner().state.phase, Phase::Paused);
        let actions = soap_request(
            port,
            "AVTransport",
            "GetCurrentTransportActions",
            "<InstanceID>0</InstanceID>",
        );
        assert!(actions.contains("<Actions>Play,Stop,Seek</Actions>"), "{actions}");

        let stop = soap_request(port, "AVTransport", "Stop", "<InstanceID>0</InstanceID>");
        assert!(stop.starts_with("HTTP/1.1 200"), "{stop}");
        assert_eq!(fake.inner().state.phase, Phase::Stopped);
    }

    #[test]
    fn bad_requests_get_upnp_errors_not_success() {
        let fake = Arc::new(Fake::default());
        let dmr = start(fake.clone());
        let port = dmr.info().port;

        let unknown = soap_request(port, "AVTransport", "GetMagic", "<InstanceID>0</InstanceID>");
        assert!(unknown.starts_with("HTTP/1.1 500"), "{unknown}");
        assert!(unknown.contains("<errorCode>401</errorCode>"), "{unknown}");

        let missing_uri = soap_request(
            port,
            "AVTransport",
            "SetAVTransportURI",
            "<InstanceID>0</InstanceID>",
        );
        assert!(missing_uri.contains("<errorCode>402</errorCode>"), "{missing_uri}");

        let next = soap_request(port, "AVTransport", "Next", "<InstanceID>0</InstanceID>");
        assert!(next.contains("<errorCode>701</errorCode>"), "{next}");

        let seek = soap_request(
            port,
            "AVTransport",
            "Seek",
            "<InstanceID>0</InstanceID><Unit>REL_TIME</Unit><Target>banana</Target>",
        );
        assert!(seek.contains("<errorCode>711</errorCode>"), "{seek}");
    }

    #[test]
    fn a_controller_stages_a_next_item_and_steps_through_the_list() {
        let fake = Arc::new(Fake::default());
        let dmr = start(Arc::clone(&fake));
        let port = dmr.info().port;
        soap_request(
            port,
            AVT,
            "SetAVTransportURI",
            "<InstanceID>0</InstanceID><CurrentURI>http://hub/media/第一首.mp3</CurrentURI><CurrentURIMetaData></CurrentURIMetaData>",
        );

        // End of a list is a reason, not a silent success: this is what a TV
        // reads as "there is nothing after this".
        let early = soap_request(port, AVT, "Next", "<InstanceID>0</InstanceID>");
        assert!(early.contains("<errorCode>701</errorCode>"), "{early}");
        let actions = soap_request(
            port,
            AVT,
            "GetCurrentTransportActions",
            "<InstanceID>0</InstanceID>",
        );
        assert!(!actions.contains("Next"), "{actions}");

        let staged = soap_request(
            port,
            AVT,
            "SetNextAVTransportURI",
            "<InstanceID>0</InstanceID><NextURI>http://hub/media/第二首.mp3</NextURI><NextURIMetaData></NextURIMetaData>",
        );
        assert!(
            staged.contains("SetNextAVTransportURIResponse"),
            "{staged}"
        );
        assert_eq!(
            fake.inner().next.as_deref(),
            Some("http://hub/media/第二首.mp3")
        );
        assert_eq!(
            fake.inner().loads.len(),
            1,
            "staging must not load the item early"
        );

        let missing = soap_request(
            port,
            AVT,
            "SetNextAVTransportURI",
            "<InstanceID>0</InstanceID>",
        );
        assert!(missing.contains("<errorCode>402</errorCode>"), "{missing}");
        let blank = soap_request(
            port,
            AVT,
            "SetNextAVTransportURI",
            "<InstanceID>0</InstanceID><NextURI>   </NextURI>",
        );
        assert!(blank.contains("<errorCode>402</errorCode>"), "{blank}");

        fake.set_has_next(true);
        let actions = soap_request(
            port,
            AVT,
            "GetCurrentTransportActions",
            "<InstanceID>0</InstanceID>",
        );
        assert!(actions.contains("Next"), "{actions}");

        let stepped = soap_request(port, AVT, "Next", "<InstanceID>0</InstanceID>");
        assert!(stepped.contains("NextResponse"), "{stepped}");
        assert_eq!(
            fake.inner().state.uri.as_deref(),
            Some("http://hub/media/第二首.mp3"),
            "the staged item is what starts"
        );
        assert!(fake.inner().next.is_none(), "and it is consumed by the step");
        assert_eq!(
            fake.inner().loads.len(),
            1,
            "stepping is the queue moving, not a second load"
        );

        let back = soap_request(port, AVT, "Previous", "<InstanceID>0</InstanceID>");
        assert!(back.contains("PreviousResponse"), "{back}");
        assert_eq!(
            fake.inner().state.uri.as_deref(),
            Some("http://list/previous")
        );
    }

    #[test]
    fn track_metadata_names_the_class_and_only_mimes_what_the_address_proves() {
        let fake = Arc::new(Fake::default());
        let dmr = start(Arc::clone(&fake));
        let port = dmr.info().port;

        let didl_for = |uri: &str| {
            soap_request(
                port,
                AVT,
                "SetAVTransportURI",
                &format!(
                    "<InstanceID>0</InstanceID><CurrentURI>{uri}</CurrentURI><CurrentURIMetaData></CurrentURIMetaData>"
                ),
            );
            let info = soap_request(port, AVT, "GetPositionInfo", "<InstanceID>0</InstanceID>");
            assert!(
                info.contains("&lt;DIDL-Lite"),
                "a document inside an element is escaped: {info}"
            );
            unescape(
                &xml_tag(&info, "TrackMetaData").expect("track metadata"),
            )
        };

        let photo = didl_for("http://127.0.0.1:7878/media/照片.jpg");
        assert!(photo.contains("xmlns:dc=\"http://purl.org/dc/elements/1.1/\""), "{photo}");
        assert!(photo.contains("<upnp:class>object.item.imageItem.photo</upnp:class>"), "{photo}");
        assert!(photo.contains("<dc:title>照片.jpg</dc:title>"), "{photo}");
        assert!(photo.contains("protocolInfo=\"http-get:*:image/jpeg:*\""), "{photo}");
        assert!(
            photo.contains(">http://127.0.0.1:7878/media/照片.jpg</res>"),
            "{photo}"
        );

        let song = didl_for("http://127.0.0.1:7878/media/背景音乐.mp3");
        assert!(
            song.contains("<upnp:class>object.item.audioItem.musicTrack</upnp:class>"),
            "{song}"
        );
        assert!(song.contains("protocolInfo=\"http-get:*:audio/mpeg:*\""), "{song}");

        // A library address spells out no extension, so the mime stays a
        // wildcard: the hub answers with the real Content-Type, the renderer
        // will not invent one.
        let stored = didl_for("http://127.0.0.1:7878/media/20e2c4680433");
        assert!(stored.contains("protocolInfo=\"http-get:*:*:*\""), "{stored}");
        assert!(
            stored.contains("<upnp:class>object.item.videoItem</upnp:class>"),
            "{stored}"
        );
        assert_eq!(stored.matches("<item ").count(), 1, "{stored}");
        assert!(stored.contains("</DIDL-Lite>"), "{stored}");
    }

    #[test]
    fn the_description_lists_all_three_services() {
        let fake = Arc::new(Fake::default());
        let dmr = start(fake);
        let port = dmr.info().port;
        let response = http(
            port,
            "GET /dd.xml HTTP/1.1\r\nHost: test\r\nConnection: close\r\n\r\n",
        );
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        assert!(response.contains("<friendlyName>Test Renderer</friendlyName>"), "{response}");
        assert!(response.contains("<UDN>uuid:"), "{response}");
        for service in &SERVICES {
            assert!(response.contains(service.control), "missing {}", service.name);
            assert!(response.contains(service.scpd), "missing {}", service.name);
            let scpd = http(
                port,
                &format!("GET {} HTTP/1.1\r\nHost: test\r\nConnection: close\r\n\r\n", service.scpd),
            );
            assert!(scpd.starts_with("HTTP/1.1 200"), "{scpd}");
            assert!(scpd.contains("<actionList>"), "{scpd}");
        }

        let page = http(port, "GET / HTTP/1.1\r\nHost: test\r\nConnection: close\r\n\r\n");
        assert!(page.contains("Test Renderer"), "{page}");
        assert!(page.contains("NO_MEDIA_PRESENT"), "{page}");
    }

    #[test]
    fn subscribing_delivers_an_initial_last_change() {
        let fake = Arc::new(Fake::default());
        fake.set_duration(30.0);
        let dmr = start(fake);
        let port = dmr.info().port;

        let callback = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        callback.set_nonblocking(true).unwrap();
        let callback_port = callback.local_addr().unwrap().port();

        let response = http(
            port,
            &format!(
                "SUBSCRIBE /evt/AVTransport HTTP/1.1\r\nHost: 127.0.0.1\r\n\
                 CALLBACK: <http://127.0.0.1:{callback_port}/events>\r\n\
                 NT: upnp:event\r\nTIMEOUT: Second-300\r\nCONNECTION: close\r\n\r\n"
            ),
        );
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        assert!(response.contains("TIMEOUT: Second-300"), "{response}");
        let sid_line = response
            .lines()
            .find(|line| line.to_ascii_lowercase().starts_with("sid:"))
            .expect("SID");
        let sid = sid_line.split_once(':').unwrap().1.trim().to_string();

        let deadline = Instant::now() + Duration::from_secs(3);
        let mut stream = loop {
            match callback.accept() {
                Ok((stream, _)) => break stream,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < deadline, "no initial event arrived");
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(e) => panic!("{e}"),
            }
        };
        stream.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let mut event = String::new();
        let _ = stream.read_to_string(&mut event);
        assert!(event.starts_with("NOTIFY /events HTTP/1.1"), "{event}");
        assert!(event.contains("NT: upnp:event"), "{event}");
        assert!(event.contains("NTS: upnp:propchange"), "{event}");
        assert!(event.contains(&format!("SID: {sid}")), "{event}");
        assert!(event.contains("SEQ: 0"), "{event}");
        assert!(event.contains("LastChange"), "{event}");
        assert!(event.contains("TransportState"), "{event}");

        let renew = http(
            port,
            &format!(
                "SUBSCRIBE /evt/AVTransport HTTP/1.1\r\nHost: 127.0.0.1\r\n\
                 SID: {sid}\r\nTIMEOUT: Second-600\r\nCONNECTION: close\r\n\r\n"
            ),
        );
        assert!(renew.starts_with("HTTP/1.1 200"), "{renew}");
        assert!(renew.contains("TIMEOUT: Second-600"), "{renew}");

        let gone = http(
            port,
            &format!(
                "UNSUBSCRIBE /evt/AVTransport HTTP/1.1\r\nHost: 127.0.0.1\r\n\
                 SID: {sid}\r\nCONNECTION: close\r\n\r\n"
            ),
        );
        assert!(gone.starts_with("HTTP/1.1 200"), "{gone}");
        let unknown = http(
            port,
            &format!(
                "UNSUBSCRIBE /evt/AVTransport HTTP/1.1\r\nHost: 127.0.0.1\r\n\
                 SID: {sid}\r\nCONNECTION: close\r\n\r\n"
            ),
        );
        assert!(unknown.starts_with("HTTP/1.1 412"), "{unknown}");
    }

    #[test]
    fn ssdp_search_targets_collapse_to_the_right_usns() {
        let fake = Arc::new(Fake::default());
        let dmr = start(fake);
        let info = dmr.info();
        let udn = info.udn.clone();
        let server = Server::new(
            TcpListener::bind(("127.0.0.1", 0)).unwrap(),
            None,
            info.clone(),
            Arc::new(Fake::default()),
        );
        let adverts = server.adverts();
        assert_eq!(adverts.len(), 6);
        let root: Vec<_> = adverts
            .iter()
            .filter(|(nt, _)| search_matches("upnp:rootdevice", nt))
            .collect();
        assert_eq!(root.len(), 1);
        assert_eq!(root[0].1, format!("{udn}::upnp:rootdevice"));
        let all: Vec<_> = adverts
            .iter()
            .filter(|(nt, _)| search_matches("ssdp:all", nt))
            .collect();
        assert_eq!(all.len(), 6);
        assert!(adverts.iter().any(|(nt, _)| nt == DEVICE_TYPE));
        assert!(adverts.iter().any(|(nt, _)| nt == AVT_TYPE));
        assert!(!adverts.iter().any(|(nt, _)| search_matches(
            "urn:schemas-upnp-org:device:MediaServer:1",
            nt
        )));
    }

    #[test]
    fn m_search_and_http_dates_parse() {
        let message = "M-SEARCH * HTTP/1.1\r\nHOST: 239.255.255.250:1900\r\n\
                       MAN: \"ssdp:discover\"\r\nMX: 1\r\nST: ssdp:all\r\n\r\n";
        let headers = parse_m_search(message).expect("m-search");
        assert_eq!(headers.get("mx").map(String::as_str), Some("1"));
        assert_eq!(headers.get("st").map(String::as_str), Some("ssdp:all"));
        assert!(parse_m_search("NOTIFY * HTTP/1.1\r\n\r\n").is_none());

        assert_eq!(
            http_date(UNIX_EPOCH),
            "Thu, 01 Jan 1970 00:00:00 GMT"
        );
        assert_eq!(
            http_date(UNIX_EPOCH + Duration::from_secs(1_700_000_000)),
            "Tue, 14 Nov 2023 22:13:20 GMT"
        );
    }

    #[test]
    fn time_and_xml_helpers_are_exact() {
        assert_eq!(hms(0.0), "0:00:00");
        assert_eq!(hms(3_723.9), "1:02:03");
        assert_eq!(hms(f64::NAN), "0:00:00");
        assert_eq!(parse_hms("1:02:03"), Some(3_723.0));
        assert_eq!(parse_hms("0:04"), Some(4.0));
        assert_eq!(parse_hms("0:99:00"), None);
        assert_eq!(parse_hms("banana"), None);

        let body = "<u:Play xmlns:u=\"x\"><InstanceID>0</InstanceID><Speed>1</Speed></u:Play>";
        assert_eq!(xml_tag(body, "InstanceID").as_deref(), Some("0"));
        assert_eq!(xml_tag(body, "Play").as_deref(), Some("<InstanceID>0</InstanceID><Speed>1</Speed>"));
        assert_eq!(xml_tag(body, "Missing"), None);
        assert_eq!(xml_tag("<A><B x=\"1>2\">v</B></A>", "B").as_deref(), Some("v"));
        assert_eq!(xml_tag("<A/>", "A").as_deref(), Some(""));
        assert_eq!(xml_tag("<A>&lt;DIDL&gt;</A>", "A").as_deref(), Some("<DIDL>"));
        assert_eq!(xml_tag("<A><![CDATA[x<y]]></A>", "A").as_deref(), Some("x<y"));
    }

    #[test]
    fn callbacks_and_timeouts_are_bounded() {
        let callback = parse_callback("<http://192.168.0.9:7777/event>").expect("callback");
        assert_eq!(callback.host, "192.168.0.9");
        assert_eq!(callback.port, 7777);
        assert_eq!(callback.path, "/event");
        assert!(parse_callback("192.168.0.9:7777/event").is_none());
        assert_eq!(parse_timeout("Second-300"), 300);
        assert_eq!(parse_timeout("Second-infinite"), EVENT_TIMEOUT);
        assert_eq!(parse_timeout("garbage"), EVENT_TIMEOUT);
        assert_eq!(parse_timeout("Second-5"), 60);
    }
}
