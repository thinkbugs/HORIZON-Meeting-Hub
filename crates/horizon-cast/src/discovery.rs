//! Who on this LAN can receive a cast.
//!
//! Two protocols answer that question, and they have nothing in common beyond
//! both being multicast:
//!
//! - **mDNS** (RFC 6762) is what Cast, AirPlay and this engine's own channel use.
//!   A browse query for a service type gets a DNS response carrying PTR, SRV, TXT
//!   and A records that together mean "a device of this kind, at this address".
//! - **SSDP** (UPnP) is plain text over UDP and is how DLNA renderers — and the
//!   DIAL service some Cast apps use — announce themselves.
//!
//! ## Why the DNS code is here and not in a crate
//!
//! The only registry this machine can reach is the offline one, and it does not
//! carry an mDNS crate. The subset a browser needs is small and, more importantly,
//! testable: a name codec, a record parser, and six record types. Writing it here
//! means the tests below can hand-build a packet with a compression pointer and
//! prove the parser follows it — the failure mode that matters, since every real
//! responder uses compression.
//!
//! ## What is deliberately not here
//!
//! Only the three service types this engine can actually cast to are interpreted
//! ([`MDNS_SERVICES`], and the SSDP targets in [`SSDP_TARGETS`]). A list of every
//! printer and NAS on the network is not a device picker; it is noise.
//!
//! There is also no advertisement yet: this module finds peers, it does not make
//! this machine findable. That belongs with the receiver that has something to
//! advertise (P2-B for DLNA, P3 for AirPlay), not with the browse foundation.
//!
//! ## Limits a caller must know
//!
//! - mDNS is joined on **one interface**: whichever one the default route uses.
//!   That is deliberate — a Hyper-V or VPN adapter also has an IPv4 address, and
//!   the OS's default multicast interface is not always the one the devices are
//!   on. Enumerating every adapter and browsing each is a P2-B concern along with
//!   binding the sockets to each one.
//! - A peer found by SSDP has a `LOCATION` URL, not a friendly name. The name is
//!   in the description XML behind that URL; [`describe`] fetches it, but that is a
//!   TCP round trip and the discovery loop never makes it for you.
//! - Windows Firewall may drop inbound multicast until the app is allowed. The
//!   probe binary prints what answered, which makes that failure visible rather
//!   than mysterious.
//!
//! ## Verified
//!
//! The codec tests hand-build packets, including a compression pointer inside SRV
//! rdata. Two tests need real sockets and are `--ignored` by default: one sends a
//! synthetic Cast response to 224.0.0.251:5353 and requires the browser to see it;
//! the other sends an M-SEARCH, catches it on a responder bound to 1900, and
//! requires the unicast reply to become a DLNA peer. Both pass on the development
//! machine.
//!
//! A ten-second browse of its Wi-Fi found nothing. That is not a parser failure —
//! the receive path is proven above — but it is not proof that the LAN is empty
//! either: Windows Firewall has no inbound rule for the probe binary, and access
//! points often drop multicast between wireless clients. The probe prints the
//! interface it uses and counts malformed packets, so a silent run can be
//! diagnosed instead of guessed at.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{
    IpAddr, Ipv4Addr, SocketAddr, TcpStream, ToSocketAddrs, UdpSocket,
};
use std::time::{Duration, Instant};

use crate::error::{CastError, CastResult};

pub const MDNS_GROUP: Ipv4Addr = Ipv4Addr::new(224, 0, 0, 251);
pub const MDNS_PORT: u16 = 5353;
pub const SSDP_GROUP: Ipv4Addr = Ipv4Addr::new(239, 255, 255, 250);
pub const SSDP_PORT: u16 = 1900;

/// The service types this engine understands. `_raop` is deliberately absent:
/// it advertises audio streaming, and this engine's product question is video.
const MDNS_SERVICES: [&str; 3] = [
    "_horizon-cast._udp.local",
    "_googlecast._tcp.local",
    "_airplay._tcp.local",
];

/// A DLNA renderer is castable video. DIAL is the launch service Cast apps use
/// for YouTube and friends; both are worth listing, and the protocol field keeps
/// them distinguishable.
const SSDP_TARGETS: [&str; 2] = [
    "urn:schemas-upnp-org:device:MediaRenderer:1",
    "urn:dial-multiscreen-org:service:dial:1",
];

const DNS_HEADER: usize = 12;
const TYPE_A: u16 = 1;
const TYPE_PTR: u16 = 12;
const TYPE_TXT: u16 = 16;
const TYPE_SRV: u16 = 33;
const CLASS_IN: u16 = 1;

// ------------------------------------------------------------------ types

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Protocol {
    Horizon,
    GoogleCast,
    AirPlay,
    Dlna,
}

impl Protocol {
    pub fn label(self) -> &'static str {
        match self {
            Protocol::Horizon => "HORIZON",
            Protocol::GoogleCast => "Google Cast",
            Protocol::AirPlay => "AirPlay",
            Protocol::Dlna => "DLNA",
        }
    }

    fn from_mdns_type(name: &str) -> Option<Self> {
        match name.trim_end_matches('.').to_ascii_lowercase().as_str() {
            "_horizon-cast._udp.local" => Some(Protocol::Horizon),
            "_googlecast._tcp.local" => Some(Protocol::GoogleCast),
            "_airplay._tcp.local" => Some(Protocol::AirPlay),
            _ => None,
        }
    }

    fn from_ssdp_st(st: &str) -> Option<Self> {
        let st = st.to_ascii_lowercase();
        if st.contains("dial-multiscreen-org") {
            Some(Protocol::GoogleCast)
        } else if st.contains("mediarenderer") {
            Some(Protocol::Dlna)
        } else {
            None
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Mdns,
    Ssdp,
}

#[derive(Debug, Clone)]
pub struct Peer {
    /// Stable across refreshes: the full mDNS instance name, or the SSDP USN's
    /// UUID part. This is the key a UI should use, not the display name.
    pub id: String,
    pub name: String,
    pub protocol: Protocol,
    pub source: Source,
    /// Set for mDNS peers once an A record for the SRV target has arrived.
    pub addr: Option<SocketAddr>,
    /// Set for SSDP peers: the device description URL.
    pub location: Option<String>,
    pub txt: Vec<(String, String)>,
    pub seen: Instant,
    pub expires: Instant,
}

impl Peer {
    pub fn is_expired(&self, now: Instant) -> bool {
        now >= self.expires
    }

    /// The TXT value Cast devices use for their display name, when present.
    pub fn txt_value(&self, key: &str) -> Option<&str> {
        self.txt
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(key))
            .map(|(_, v)| v.as_str())
    }
}

#[derive(Debug, Clone)]
pub enum DiscoveryEvent {
    Found(Peer),
    Updated(Peer),
    Lost(String),
}

// ------------------------------------------------------------- dns names

fn be16(msg: &[u8], at: usize) -> CastResult<u16> {
    let bytes = msg
        .get(at..at + 2)
        .ok_or(CastError::Malformed("truncated dns field"))?;
    Ok(u16::from_be_bytes([bytes[0], bytes[1]]))
}

fn be32(msg: &[u8], at: usize) -> CastResult<u32> {
    let bytes = msg
        .get(at..at + 4)
        .ok_or(CastError::Malformed("truncated dns field"))?;
    Ok(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

fn push_u16(out: &mut Vec<u8>, v: u16) {
    out.extend_from_slice(&v.to_be_bytes());
}

fn encode_name(out: &mut Vec<u8>, name: &str) -> CastResult<()> {
    let trimmed = name.trim_end_matches('.');
    if trimmed.is_empty() {
        out.push(0);
        return Ok(());
    }
    for label in trimmed.split('.') {
        let bytes = label.as_bytes();
        if bytes.is_empty() || bytes.len() > 63 {
            return Err(CastError::Malformed("dns label length"));
        }
        out.push(bytes.len() as u8);
        out.extend_from_slice(bytes);
    }
    out.push(0);
    Ok(())
}

/// Decodes a possibly-compressed name. Returns the name and the offset just past
/// the name *in the record that contained it*, which is what the caller needs to
/// keep walking even when the name itself was a pointer.
fn decode_name(msg: &[u8], mut at: usize) -> CastResult<(String, usize)> {
    let mut out = String::new();
    let mut next = None;
    let mut jumps = 0;
    loop {
        let len = *msg
            .get(at)
            .ok_or(CastError::Malformed("dns name overruns"))?;
        if len == 0 {
            at += 1;
            break;
        }
        if len & 0xC0 == 0xC0 {
            let second = *msg
                .get(at + 1)
                .ok_or(CastError::Malformed("truncated dns pointer"))?;
            let target = (((len & 0x3F) as usize) << 8) | second as usize;
            next.get_or_insert(at + 2);
            jumps += 1;
            if jumps > 64 {
                return Err(CastError::Malformed("dns compression loop"));
            }
            at = target;
            continue;
        }
        let len = len as usize;
        if len > 63 {
            return Err(CastError::Malformed("dns label length"));
        }
        let label = msg
            .get(at + 1..at + 1 + len)
            .ok_or(CastError::Malformed("dns label overruns"))?;
        let label = std::str::from_utf8(label).map_err(|_| CastError::Malformed("dns name utf8"))?;
        if !out.is_empty() {
            out.push('.');
        }
        out.push_str(label);
        at += 1 + len;
    }
    Ok((out, next.unwrap_or(at)))
}

/// Case-insensitive suffix strip. DNS comparison is case-insensitive, but the
/// instance name must be reported with the case the device chose.
fn strip_suffix_ci<'a>(value: &'a str, suffix: &str) -> Option<&'a str> {
    if value.len() >= suffix.len() && value[value.len() - suffix.len()..].eq_ignore_ascii_case(suffix)
    {
        Some(&value[..value.len() - suffix.len()])
    } else {
        None
    }
}

// ---------------------------------------------------------- mdns records

#[derive(Debug)]
struct Record {
    name: String,
    rtype: u16,
    ttl: u32,
    rdata_at: usize,
    rdlength: usize,
}

fn skip_question(msg: &[u8], at: usize) -> CastResult<usize> {
    let (_, next) = decode_name(msg, at)?;
    if next + 4 > msg.len() {
        return Err(CastError::Malformed("truncated dns question"));
    }
    Ok(next + 4)
}

/// Every record in the answer, authority and additional sections. Questions are
/// skipped; a query (QR=0) yields no records and is not an error.
fn parse_response(msg: &[u8]) -> CastResult<Vec<Record>> {
    if msg.len() < DNS_HEADER {
        return Err(CastError::Malformed("short dns header"));
    }
    let flags = be16(msg, 2)?;
    if flags & 0x8000 == 0 {
        return Ok(Vec::new());
    }
    let questions = be16(msg, 4)? as usize;
    let total = be16(msg, 6)? as usize + be16(msg, 8)? as usize + be16(msg, 10)? as usize;
    if questions > 64 || total > 256 {
        return Err(CastError::Malformed("implausible dns record count"));
    }
    let mut at = DNS_HEADER;
    for _ in 0..questions {
        at = skip_question(msg, at)?;
    }
    let mut out = Vec::with_capacity(total);
    for _ in 0..total {
        let (name, next) = decode_name(msg, at)?;
        at = next;
        if at + 10 > msg.len() {
            return Err(CastError::Malformed("truncated dns record"));
        }
        let rtype = be16(msg, at)?;
        let ttl = be32(msg, at + 4)?;
        let rdlength = be16(msg, at + 8)? as usize;
        at += 10;
        if at + rdlength > msg.len() {
            return Err(CastError::Malformed("dns rdata overruns"));
        }
        out.push(Record {
            name,
            rtype,
            ttl,
            rdata_at: at,
            rdlength,
        });
        at += rdlength;
    }
    Ok(out)
}

fn read_txt(msg: &[u8], at: usize, len: usize) -> CastResult<Vec<(String, String)>> {
    let end = at + len;
    let mut out = Vec::new();
    let mut p = at;
    while p < end {
        let n = msg[p] as usize;
        p += 1;
        if p + n > end {
            return Err(CastError::Malformed("txt overruns"));
        }
        let text =
            std::str::from_utf8(&msg[p..p + n]).map_err(|_| CastError::Malformed("txt utf8"))?;
        match text.split_once('=') {
            Some((key, value)) => out.push((key.to_string(), value.to_string())),
            None => out.push((text.to_string(), String::new())),
        }
        p += n;
    }
    Ok(out)
}

fn mdns_query(services: &[&str]) -> Vec<u8> {
    let mut questions = Vec::with_capacity(services.len() * 32);
    let mut count = 0u16;
    for service in services {
        // A name too long for DNS is a programmer error; leave it out rather
        // than advertise a question count the body does not match.
        if encode_name(&mut questions, service).is_err() {
            continue;
        }
        push_u16(&mut questions, TYPE_PTR);
        push_u16(&mut questions, CLASS_IN);
        count += 1;
    }
    let mut out = Vec::with_capacity(DNS_HEADER + questions.len());
    push_u16(&mut out, 0); // id: mDNS ignores it
    push_u16(&mut out, 0); // standard query
    push_u16(&mut out, count);
    push_u16(&mut out, 0); // answers
    push_u16(&mut out, 0); // authority
    push_u16(&mut out, 0); // additional
    out.extend_from_slice(&questions);
    out
}

fn ttl_secs(ttl: u32) -> u64 {
    // A zero TTL is a "do not cache" hint; treating it as one second keeps the
    // peer visible long enough to be acted on without lying about its life.
    (ttl as u64).clamp(1, 3600)
}

struct Ptr {
    protocol: Protocol,
    display: String,
    seen: Instant,
    expires: Instant,
}

struct Srv {
    host: String,
    port: u16,
    expires: Instant,
}

#[derive(Default)]
struct MdnsBrowser {
    /// Keyed by lowercased instance name.
    ptr: HashMap<String, Ptr>,
    srv: HashMap<String, Srv>,
    txt: HashMap<String, (Vec<(String, String)>, Instant)>,
    addr: HashMap<String, (IpAddr, Instant)>,
}

impl MdnsBrowser {
    fn apply(&mut self, msg: &[u8], now: Instant) -> CastResult<()> {
        for record in parse_response(msg)? {
            let expires = now + Duration::from_secs(ttl_secs(record.ttl));
            match record.rtype {
                TYPE_PTR => {
                    let Some(protocol) = Protocol::from_mdns_type(&record.name) else {
                        continue;
                    };
                    let (instance, _) = decode_name(msg, record.rdata_at)?;
                    let key = instance.to_ascii_lowercase();
                    let suffix = format!(".{}", record.name);
                    let display = strip_suffix_ci(&instance, &suffix).unwrap_or(&instance);
                    self.ptr.insert(
                        key,
                        Ptr {
                            protocol,
                            display: display.to_string(),
                            seen: now,
                            expires,
                        },
                    );
                }
                TYPE_SRV => {
                    if record.rdlength < 7 {
                        continue;
                    }
                    let port = be16(msg, record.rdata_at + 4)?;
                    let (host, _) = decode_name(msg, record.rdata_at + 6)?;
                    self.srv.insert(
                        record.name.to_ascii_lowercase(),
                        Srv {
                            host,
                            port,
                            expires,
                        },
                    );
                }
                TYPE_TXT => {
                    let txt = read_txt(msg, record.rdata_at, record.rdlength)?;
                    self.txt
                        .insert(record.name.to_ascii_lowercase(), (txt, expires));
                }
                TYPE_A => {
                    if record.rdlength != 4 {
                        continue;
                    }
                    let ip = Ipv4Addr::new(
                        msg[record.rdata_at],
                        msg[record.rdata_at + 1],
                        msg[record.rdata_at + 2],
                        msg[record.rdata_at + 3],
                    );
                    self.addr
                        .insert(record.name.to_ascii_lowercase(), (IpAddr::V4(ip), expires));
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn snapshot(&self, now: Instant) -> Vec<Peer> {
        let mut out = Vec::new();
        for (key, ptr) in &self.ptr {
            if ptr.expires <= now {
                continue;
            }
            let srv = self
                .srv
                .get(key)
                .filter(|srv| srv.expires > now);
            let addr = srv.and_then(|srv| {
                self.addr
                    .get(&srv.host.to_ascii_lowercase())
                    .filter(|(_, expires)| *expires > now)
                    .map(|(ip, _)| SocketAddr::new(*ip, srv.port))
            });
            let txt = self
                .txt
                .get(key)
                .filter(|(_, expires)| *expires > now)
                .map(|(txt, _)| txt.clone())
                .unwrap_or_default();
            out.push(Peer {
                id: format!("mdns:{key}"),
                name: ptr.display.clone(),
                protocol: ptr.protocol,
                source: Source::Mdns,
                addr,
                location: None,
                txt,
                seen: ptr.seen,
                expires: ptr.expires,
            });
        }
        out
    }

    fn has_live(&self, now: Instant) -> bool {
        self.ptr.values().any(|ptr| ptr.expires > now)
    }

    /// Drops expired cache entries so a long-running browse does not accumulate
    /// records from devices that have left.
    fn expire(&mut self, now: Instant) {
        self.ptr.retain(|_, ptr| ptr.expires > now);
        self.srv.retain(|_, srv| srv.expires > now);
        self.txt.retain(|_, (_, expires)| *expires > now);
        self.addr.retain(|_, (_, expires)| *expires > now);
    }
}

// ------------------------------------------------------------------ ssdp

#[derive(Debug)]
enum SsdpMessage {
    Response(HashMap<String, String>),
    Notify(HashMap<String, String>),
}

fn parse_ssdp(buf: &[u8]) -> CastResult<SsdpMessage> {
    let text = std::str::from_utf8(buf).map_err(|_| CastError::Malformed("ssdp utf8"))?;
    let mut lines = text.split("\r\n");
    let start = lines
        .next()
        .ok_or(CastError::Malformed("empty ssdp message"))?;
    let kind = if start.starts_with("HTTP/1.1 200") {
        0
    } else if start.starts_with("NOTIFY ") {
        1
    } else {
        return Err(CastError::Malformed("unknown ssdp start line"));
    };
    let mut headers = HashMap::new();
    for line in lines {
        if line.is_empty() {
            break;
        }
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
    }
    Ok(if kind == 0 {
        SsdpMessage::Response(headers)
    } else {
        SsdpMessage::Notify(headers)
    })
}

fn max_age(headers: &HashMap<String, String>) -> u64 {
    headers
        .get("cache-control")
        .and_then(|value| {
            let lower = value.to_ascii_lowercase();
            let age = lower.find("max-age")?;
            let rest = lower[age..].split_once('=')?.1.trim_start();
            let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
            digits.parse().ok()
        })
        .unwrap_or(1800)
        .clamp(30, 86_400)
}

fn search_packet(target: &str) -> Vec<u8> {
    format!(
        "M-SEARCH * HTTP/1.1\r\n\
         HOST: {SSDP_GROUP}:{SSDP_PORT}\r\n\
         MAN: \"ssdp:discover\"\r\n\
         MX: 1\r\n\
         ST: {target}\r\n\
         \r\n"
    )
    .into_bytes()
}

struct SsdpEntry {
    protocol: Protocol,
    location: String,
    seen: Instant,
    expires: Instant,
}

/// Folds one SSDP message into the table. Kept free of sockets so the protocol
/// rules — which targets count, how USN identifies a device, what byebye means —
/// are testable without a network.
fn absorb(entries: &mut HashMap<String, SsdpEntry>, message: SsdpMessage, now: Instant) {
    let (headers, alive) = match message {
        SsdpMessage::Response(headers) => (headers, true),
        SsdpMessage::Notify(headers) => {
            let alive = headers
                .get("nts")
                .map(|nts| !nts.eq_ignore_ascii_case("ssdp:byebye"))
                .unwrap_or(true);
            (headers, alive)
        }
    };
    let Some(usn) = headers.get("usn") else {
        return;
    };
    // The UUID before `::` names the device; the rest names the service. Keying
    // on the UUID collapses the several search targets one device answers with.
    let id = usn.split("::").next().unwrap_or(usn).trim().to_string();
    if !alive {
        entries.remove(&id);
        return;
    }
    let target = headers
        .get("st")
        .or_else(|| headers.get("nt"))
        .cloned()
        .unwrap_or_default();
    let Some(protocol) = Protocol::from_ssdp_st(&target) else {
        return;
    };
    let Some(location) = headers.get("location").cloned() else {
        return;
    };
    entries.insert(
        id,
        SsdpEntry {
            protocol,
            location,
            seen: now,
            expires: now + Duration::from_secs(max_age(&headers)),
        },
    );
}

fn ssdp_peers(entries: &HashMap<String, SsdpEntry>, now: Instant) -> Vec<Peer> {
    entries
        .iter()
        .filter(|(_, entry)| entry.expires > now)
        .map(|(id, entry)| Peer {
            id: format!("ssdp:{id}"),
            // SSDP carries no friendly name; the description behind LOCATION
            // does. Until a caller fetches it, the USN is the honest label.
            name: id.clone(),
            protocol: entry.protocol,
            source: Source::Ssdp,
            addr: None,
            location: Some(entry.location.clone()),
            txt: Vec::new(),
            seen: entry.seen,
            expires: entry.expires,
        })
        .collect()
}

struct Ssdp {
    /// Bound to 1900: this is where NOTIFY arrives and where a responder that
    /// insists on answering port 1900 reaches us.
    sock: UdpSocket,
    /// Ephemeral, and the source of every M-SEARCH. A unicast reply goes to the
    /// port the search came from, so this is what makes discovery work on a
    /// machine where another process (our own renderer, a media player) also
    /// holds 1900: Windows delivers a unicast datagram addressed to :1900 to one
    /// of those sockets, not all of them.
    search: UdpSocket,
    entries: HashMap<String, SsdpEntry>,
    next_query: Instant,
}

impl Ssdp {
    fn bind() -> CastResult<Self> {
        // Port 1900 is needed to hear NOTIFY; if it is taken, an ephemeral port
        // still gets unicast M-SEARCH responses, so discovery keeps working and
        // only unsolicited announcements are lost.
        Self::bind_to(SSDP_PORT).or_else(|_| Self::bind_to(0))
    }

    fn bind_to(port: u16) -> CastResult<Self> {
        Ok(Self {
            sock: multicast_socket(port, SSDP_GROUP, 4)?,
            search: multicast_socket(0, SSDP_GROUP, 4)?,
            entries: HashMap::new(),
            next_query: Instant::now(),
        })
    }

    fn drain(&mut self, now: Instant) -> CastResult<()> {
        drain_ssdp_socket(&self.sock, &mut self.entries, now)?;
        drain_ssdp_socket(&self.search, &mut self.entries, now)
    }

    fn maybe_query(&mut self, now: Instant) -> CastResult<()> {
        if now < self.next_query {
            return Ok(());
        }
        for target in SSDP_TARGETS {
            self.search
                .send_to(&search_packet(target), SocketAddr::from((SSDP_GROUP, SSDP_PORT)))
                .map_err(|_| CastError::Io)?;
        }
        self.next_query = now
            + if self.entries.is_empty() {
                Duration::from_secs(3)
            } else {
                Duration::from_secs(60)
            };
        Ok(())
    }

    fn snapshot(&self, now: Instant) -> Vec<Peer> {
        ssdp_peers(&self.entries, now)
    }

    fn expire(&mut self, now: Instant) {
        self.entries.retain(|_, entry| entry.expires > now);
    }
}

fn drain_ssdp_socket(
    socket: &UdpSocket,
    entries: &mut HashMap<String, SsdpEntry>,
    now: Instant,
) -> CastResult<()> {
    let mut buf = [0u8; 4096];
    loop {
        match socket.recv_from(&mut buf) {
            Ok((n, _)) => {
                if let Ok(message) = parse_ssdp(&buf[..n]) {
                    absorb(entries, message, now);
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return Ok(()),
            // ICMP port-unreachable from an earlier send, as everywhere else.
            Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => continue,
            Err(_) => return Err(CastError::Io),
        }
    }
}

// --------------------------------------------------------------- sockets

/// The IPv4 address discovery sends and joins on.
///
/// Found by asking the routing table rather than enumerating adapters: a UDP
/// `connect` performs no I/O and its `local_addr` is the source address the
/// default route would use. On a machine with a virtual switch this is the
/// difference between asking the LAN and asking a Hyper-V network nobody is on.
pub fn primary_interface() -> Ipv4Addr {
    UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))
        .and_then(|socket| {
            socket.connect((Ipv4Addr::new(8, 8, 8, 8), 80))?;
            socket.local_addr()
        })
        .ok()
        .and_then(|addr| match addr.ip() {
            IpAddr::V4(ip) if !ip.is_loopback() => Some(ip),
            _ => None,
        })
        .unwrap_or(Ipv4Addr::UNSPECIFIED)
}

pub(crate) fn multicast_socket(port: u16, group: Ipv4Addr, ttl: u32) -> CastResult<UdpSocket> {
    let sock = socket2::Socket::new(
        socket2::Domain::IPV4,
        socket2::Type::DGRAM,
        Some(socket2::Protocol::UDP),
    )
    .map_err(|_| CastError::Io)?;
    // Must precede bind on Windows, where this is what lets two processes (this
    // engine and Bonjour) hold 5353 at once.
    sock.set_reuse_address(true).map_err(|_| CastError::Io)?;
    sock.bind(&SocketAddr::from((Ipv4Addr::UNSPECIFIED, port)).into())
        .map_err(|_| CastError::Io)?;
    let interface = primary_interface();
    sock.join_multicast_v4(&group, &interface)
        .map_err(|_| CastError::Io)?;
    // Joining on an interface decides what arrives; this decides what leaves.
    if !interface.is_unspecified() {
        sock.set_multicast_if_v4(&interface)
            .map_err(|_| CastError::Io)?;
    }
    sock.set_multicast_ttl_v4(ttl).map_err(|_| CastError::Io)?;
    // Loopback keeps two engines on one machine able to see each other in tests.
    sock.set_multicast_loop_v4(true).map_err(|_| CastError::Io)?;
    sock.set_nonblocking(true).map_err(|_| CastError::Io)?;
    Ok(sock.into())
}

struct MdnsSocket {
    sock: UdpSocket,
    browser: MdnsBrowser,
    next_query: Instant,
    /// The socket is shared with every mDNS responder on the machine, so bad
    /// packets from unrelated software are expected and skipped, not fatal.
    pub ignored: u64,
}

impl MdnsSocket {
    fn bind() -> CastResult<Self> {
        Ok(Self {
            // RFC 6762 wants the hop limit at 255 so responders can tell a local
            // query from a routed one.
            sock: multicast_socket(MDNS_PORT, MDNS_GROUP, 255)?,
            browser: MdnsBrowser::default(),
            next_query: Instant::now(),
            ignored: 0,
        })
    }

    fn drain(&mut self, now: Instant) -> CastResult<()> {
        let mut buf = [0u8; 9000];
        loop {
            match self.sock.recv_from(&mut buf) {
                Ok((n, _)) => {
                    if self.browser.apply(&buf[..n], now).is_err() {
                        self.ignored += 1;
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => continue,
                Err(_) => return Err(CastError::Io),
            }
        }
    }

    fn maybe_query(&mut self, now: Instant) -> CastResult<()> {
        if now < self.next_query {
            return Ok(());
        }
        self.sock
            .send_to(&mdns_query(&MDNS_SERVICES), SocketAddr::from((MDNS_GROUP, MDNS_PORT)))
            .map_err(|_| CastError::Io)?;
        // While the list is empty, ask often enough that a device which just
        // appeared shows up while the user is looking at the picker. Once
        // something answered, back off: records refresh on their own TTL.
        self.next_query = now
            + if self.browser.has_live(now) {
                Duration::from_secs(60)
            } else {
                Duration::from_secs(3)
            };
        Ok(())
    }
}

// -------------------------------------------------------------- facade

pub struct Discovery {
    mdns: Option<MdnsSocket>,
    mdns_error: Option<String>,
    ssdp: Ssdp,
    peers: HashMap<String, Peer>,
    events: Vec<DiscoveryEvent>,
}

impl Discovery {
    pub fn new() -> CastResult<Self> {
        let (mdns, mdns_error) = match MdnsSocket::bind() {
            Ok(socket) => (Some(socket), None),
            // Losing mDNS is not losing discovery: SSDP still answers for DLNA.
            Err(e) => (None, Some(e.to_string())),
        };
        Ok(Self {
            mdns,
            mdns_error,
            ssdp: Ssdp::bind()?,
            peers: HashMap::new(),
            events: Vec::new(),
        })
    }

    /// Why mDNS is unavailable, if it is. SSDP can still be working.
    pub fn mdns_error(&self) -> Option<&str> {
        self.mdns_error.as_deref()
    }

    /// Packets on 5353 that were not valid mDNS. Useful when "nothing found" needs
    /// to be distinguished from "someone else is using the port".
    pub fn ignored_packets(&self) -> u64 {
        self.mdns.as_ref().map(|mdns| mdns.ignored).unwrap_or(0)
    }

    /// Every live peer, by display name.
    pub fn peers(&self) -> Vec<Peer> {
        let mut out: Vec<Peer> = self.peers.values().cloned().collect();
        out.sort_by(|a, b| a.name.cmp(&b.name).then(a.id.cmp(&b.id)));
        out
    }

    /// Reads the sockets, sends any due queries and expires stale peers, waiting
    /// up to `timeout` for something to happen. Events are the changes since the
    /// previous call.
    pub fn poll(&mut self, timeout: Duration) -> CastResult<Vec<DiscoveryEvent>> {
        let deadline = Instant::now() + timeout;
        self.pump(Instant::now())?;
        loop {
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            let nap = (deadline - now).min(Duration::from_millis(10));
            std::thread::sleep(nap);
            self.pump(Instant::now())?;
        }
        self.pump(Instant::now())?;
        Ok(std::mem::take(&mut self.events))
    }

    fn pump(&mut self, now: Instant) -> CastResult<()> {
        if let Some(mdns) = &mut self.mdns {
            mdns.drain(now)?;
            mdns.maybe_query(now)?;
            mdns.browser.expire(now);
        }
        self.ssdp.drain(now)?;
        self.ssdp.maybe_query(now)?;
        self.ssdp.expire(now);
        self.sync(now);
        self.expire(now);
        Ok(())
    }

    fn sync(&mut self, now: Instant) {
        let mut incoming = Vec::new();
        if let Some(mdns) = &self.mdns {
            incoming.extend(mdns.browser.snapshot(now));
        }
        incoming.extend(self.ssdp.snapshot(now));
        for peer in incoming {
            match self.peers.get(&peer.id) {
                None => {
                    self.events.push(DiscoveryEvent::Found(peer.clone()));
                    self.peers.insert(peer.id.clone(), peer);
                }
                Some(old) => {
                    // Address, name and TXT are what a picker shows; a bare TTL
                    // refresh is not an update worth redrawing for.
                    let changed = old.name != peer.name
                        || old.addr != peer.addr
                        || old.location != peer.location
                        || old.txt != peer.txt;
                    if changed {
                        self.events.push(DiscoveryEvent::Updated(peer.clone()));
                    }
                    self.peers.insert(peer.id.clone(), peer);
                }
            }
        }
    }

    fn expire(&mut self, now: Instant) {
        let mut gone = Vec::new();
        self.peers.retain(|id, peer| {
            let live = !peer.is_expired(now);
            if !live {
                gone.push(id.clone());
            }
            live
        });
        for id in gone {
            self.events.push(DiscoveryEvent::Lost(id));
        }
    }
}

// ---------------------------------------------------------- description

#[derive(Debug, Clone, Default)]
pub struct Description {
    pub friendly_name: Option<String>,
    pub manufacturer: Option<String>,
    pub model_name: Option<String>,
}

/// Fetch and read the UPnP description behind an SSDP `LOCATION`.
///
/// This is a blocking TCP request with an explicit timeout; callers run it when
/// they want to show a name, not from the discovery loop, which must keep reading
/// multicast while a slow device takes its time.
pub fn describe(location: &str, timeout: Duration) -> CastResult<Description> {
    let (authority, path) = split_location(location)?;
    let (host, port) = split_authority(authority)?;
    let addr = (host, port)
        .to_socket_addrs()
        .map_err(|_| CastError::Io)?
        .next()
        .ok_or(CastError::Io)?;
    let mut stream = TcpStream::connect_timeout(&addr, timeout).map_err(|_| CastError::Io)?;
    stream
        .set_read_timeout(Some(timeout))
        .map_err(|_| CastError::Io)?;
    stream
        .set_write_timeout(Some(timeout))
        .map_err(|_| CastError::Io)?;
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: {authority}\r\nConnection: close\r\n\
         User-Agent: HORIZON/0.1 UPnP/1.1\r\n\r\n"
    )
    .map_err(|_| CastError::Io)?;
    let mut body = Vec::new();
    stream
        .take(512 * 1024)
        .read_to_end(&mut body)
        .map_err(|_| CastError::Io)?;
    let text = String::from_utf8_lossy(&body);
    Ok(Description {
        friendly_name: xml_tag(&text, "friendlyName"),
        manufacturer: xml_tag(&text, "manufacturer"),
        model_name: xml_tag(&text, "modelName"),
    })
}

fn split_location(location: &str) -> CastResult<(&str, &str)> {
    let rest = location
        .strip_prefix("http://")
        .ok_or(CastError::Malformed("location is not http"))?;
    let (authority, path) = match rest.find('/') {
        Some(at) => (&rest[..at], &rest[at..]),
        None => (rest, "/"),
    };
    if authority.is_empty() {
        return Err(CastError::Malformed("location has no host"));
    }
    Ok((authority, path))
}

/// `host:port` or `host`, with `[v6]:port` accepted because LOCATION may be one.
fn split_authority(authority: &str) -> CastResult<(&str, u16)> {
    if let Some(rest) = authority.strip_prefix('[') {
        let (host, rest) = rest
            .split_once(']')
            .ok_or(CastError::Malformed("unclosed ipv6 host"))?;
        let port = rest
            .strip_prefix(':')
            .map(|p| p.parse().map_err(|_| CastError::Malformed("location port")))
            .transpose()?
            .unwrap_or(80);
        Ok((host, port))
    } else if let Some((host, port)) = authority.rsplit_once(':') {
        Ok((
            host,
            port.parse()
                .map_err(|_| CastError::Malformed("location port"))?,
        ))
    } else {
        Ok((authority, 80))
    }
}

fn xml_tag(text: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = text.find(&open)? + open.len();
    let end = text[start..].find(&close)? + start;
    Some(unescape(text[start..end].trim()))
}

fn unescape(value: &str) -> String {
    value
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

// ----------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;

    fn push_name(out: &mut Vec<u8>, name: &str) {
        encode_name(out, name).unwrap();
    }

    fn record(out: &mut Vec<u8>, name: &str, rtype: u16, ttl: u32, rdata: &[u8]) {
        push_name(out, name);
        push_u16(out, rtype);
        push_u16(out, CLASS_IN);
        out.extend_from_slice(&ttl.to_be_bytes());
        push_u16(out, rdata.len() as u16);
        out.extend_from_slice(rdata);
    }

    fn response(build: impl FnOnce(&mut Vec<u8>), an: u16) -> Vec<u8> {
        let mut out = Vec::new();
        push_u16(&mut out, 0);
        push_u16(&mut out, 0x8400); // response, authoritative
        push_u16(&mut out, 0);
        push_u16(&mut out, an);
        push_u16(&mut out, 0);
        push_u16(&mut out, 0);
        build(&mut out);
        out
    }

    fn cast_response(ttl: u32) -> Vec<u8> {
        response(
            |out| {
                let mut target = Vec::new();
                push_name(&mut target, "Living Room TV._googlecast._tcp.local");
                record(out, "_googlecast._tcp.local", TYPE_PTR, ttl, &target);

                let mut srv = Vec::new();
                push_u16(&mut srv, 0); // priority
                push_u16(&mut srv, 0); // weight
                push_u16(&mut srv, 8009);
                push_name(&mut srv, "tv-1.local");
                record(out, "Living Room TV._googlecast._tcp.local", TYPE_SRV, ttl, &srv);

                let mut txt = Vec::new();
                for part in ["fn=Living Room TV", "md=Chromecast"] {
                    txt.push(part.len() as u8);
                    txt.extend_from_slice(part.as_bytes());
                }
                record(out, "Living Room TV._googlecast._tcp.local", TYPE_TXT, ttl, &txt);

                record(out, "tv-1.local", TYPE_A, ttl, &[192, 168, 1, 9]);
            },
            4,
        )
    }

    #[test]
    fn names_round_trip_and_labels_are_bounded() {
        let mut out = Vec::new();
        encode_name(&mut out, "_googlecast._tcp.local").unwrap();
        assert_eq!(
            out,
            b"\x0b_googlecast\x04_tcp\x05local\x00".to_vec()
        );
        let (name, next) = decode_name(&out, 0).unwrap();
        assert_eq!(name, "_googlecast._tcp.local");
        assert_eq!(next, out.len());

        let mut bad = Vec::new();
        assert!(encode_name(&mut bad, &"x".repeat(64)).is_err());
        assert!(encode_name(&mut bad, "a..b").is_err());
    }

    #[test]
    fn a_compression_pointer_in_rdata_is_followed() {
        let mut msg = Vec::new();
        push_u16(&mut msg, 0);
        push_u16(&mut msg, 0x8400);
        push_u16(&mut msg, 0);
        push_u16(&mut msg, 2);
        push_u16(&mut msg, 0);
        push_u16(&mut msg, 0);

        // The host name is the first record's owner and lives at offset 12, so
        // the SRV target below can be a pointer to it.
        let host_at = msg.len();
        push_name(&mut msg, "tv-1.local");
        push_u16(&mut msg, TYPE_A);
        push_u16(&mut msg, CLASS_IN);
        msg.extend_from_slice(&120u32.to_be_bytes());
        push_u16(&mut msg, 4);
        msg.extend_from_slice(&[192, 168, 1, 9]);

        // SRV whose target is a pointer rather than the literal name.
        push_name(&mut msg, "TV._airplay._tcp.local");
        push_u16(&mut msg, TYPE_SRV);
        push_u16(&mut msg, CLASS_IN);
        msg.extend_from_slice(&120u32.to_be_bytes());
        push_u16(&mut msg, 8);
        push_u16(&mut msg, 0);
        push_u16(&mut msg, 0);
        push_u16(&mut msg, 7000);
        push_u16(&mut msg, 0xC000 | host_at as u16);

        let records = parse_response(&msg).unwrap();
        assert_eq!(records.len(), 2);
        let mut browser = MdnsBrowser::default();
        browser.apply(&msg, Instant::now()).unwrap();
        let peers = browser.snapshot(Instant::now());
        let srv = browser.srv.values().next().unwrap();
        assert_eq!(srv.host, "tv-1.local");
        assert_eq!(srv.port, 7000);
        assert_eq!(
            browser
                .addr
                .get("tv-1.local")
                .map(|(ip, _)| *ip),
            Some(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 9)))
        );
        assert!(peers.is_empty(), "no PTR yet: nothing claims this service");
    }

    #[test]
    fn a_cast_announcement_becomes_one_peer() {
        let mut browser = MdnsBrowser::default();
        let now = Instant::now();
        browser.apply(&cast_response(120), now).unwrap();
        let peers = browser.snapshot(now);
        assert_eq!(peers.len(), 1);
        let peer = &peers[0];
        assert_eq!(peer.name, "Living Room TV");
        assert_eq!(peer.protocol, Protocol::GoogleCast);
        assert_eq!(
            peer.addr,
            Some(SocketAddr::from(([192, 168, 1, 9], 8009)))
        );
        assert_eq!(peer.txt_value("fn"), Some("Living Room TV"));
        assert_eq!(peer.txt_value("FN"), Some("Living Room TV"));
        assert!(peer.id.starts_with("mdns:"));
    }

    #[test]
    fn a_peer_expires_with_its_ptr_ttl() {
        let mut browser = MdnsBrowser::default();
        let now = Instant::now();
        browser.apply(&cast_response(5), now).unwrap();
        assert_eq!(browser.snapshot(now + Duration::from_secs(4)).len(), 1);
        assert!(browser.snapshot(now + Duration::from_secs(6)).is_empty());
        browser.expire(now + Duration::from_secs(6));
        assert!(!browser.has_live(now + Duration::from_secs(6)));
    }

    #[test]
    fn a_query_is_not_a_response() {
        let query = mdns_query(&["_horizon-cast._udp.local"]);
        assert!(parse_response(&query).unwrap().is_empty());

        let mut browser = MdnsBrowser::default();
        browser.apply(&query, Instant::now()).unwrap();
        assert!(browser.snapshot(Instant::now()).is_empty());
    }

    #[test]
    fn txt_without_a_value_is_kept_as_a_flag() {
        let mut rdata = Vec::new();
        rdata.push(9);
        rdata.extend_from_slice(b"txtvers=1");
        rdata.push(5);
        rdata.extend_from_slice(b"flags");
        let txt = read_txt(&rdata, 0, rdata.len()).unwrap();
        assert_eq!(
            txt,
            vec![
                ("txtvers".to_string(), "1".to_string()),
                ("flags".to_string(), String::new())
            ]
        );
    }

    fn ssdp_response(st: &str, max_age: &str) -> Vec<u8> {
        format!(
            "HTTP/1.1 200 OK\r\n\
             CACHE-CONTROL: {max_age}\r\n\
             EXT:\r\n\
             LOCATION: http://192.168.1.20:9197/dmr\r\n\
             SERVER: Windows/10.0 UPnP/1.0 Test/1.0\r\n\
             ST: {st}\r\n\
             USN: uuid:4d696e69-444c-164e-9d41-000000000001::{st}\r\n\
             \r\n"
        )
        .into_bytes()
    }

    #[test]
    fn an_ssdp_response_becomes_a_dlna_peer() {
        let text = ssdp_response("urn:schemas-upnp-org:device:MediaRenderer:1", "max-age = 1800");
        let SsdpMessage::Response(headers) = parse_ssdp(&text).unwrap() else {
            panic!("a 200 OK is a response");
        };
        assert_eq!(headers["st"], "urn:schemas-upnp-org:device:MediaRenderer:1");
        assert_eq!(max_age(&headers), 1800);

        let mut entries = HashMap::new();
        absorb(&mut entries, SsdpMessage::Response(headers), Instant::now());
        let peers = ssdp_peers(&entries, Instant::now());
        assert_eq!(peers.len(), 1);
        assert_eq!(peers[0].protocol, Protocol::Dlna);
        assert_eq!(
            peers[0].location.as_deref(),
            Some("http://192.168.1.20:9197/dmr")
        );
        assert!(peers[0].id.starts_with("ssdp:uuid:"));
    }

    #[test]
    fn an_ssdp_byebye_removes_the_device() {
        let text = ssdp_response("urn:schemas-upnp-org:device:MediaRenderer:1", "max-age=1800");
        let SsdpMessage::Response(headers) = parse_ssdp(&text).unwrap() else {
            panic!();
        };
        let mut entries = HashMap::new();
        let now = Instant::now();
        absorb(&mut entries, SsdpMessage::Response(headers.clone()), now);
        assert_eq!(ssdp_peers(&entries, now).len(), 1);

        let byebye = format!(
            "NOTIFY * HTTP/1.1\r\nHOST: 239.255.255.250:1900\r\n\
             NTS: ssdp:byebye\r\nNT: {}\r\nUSN: {}\r\n\r\n",
            headers["st"], headers["usn"]
        );
        let SsdpMessage::Notify(headers) = parse_ssdp(byebye.as_bytes()).unwrap() else {
            panic!("a NOTIFY is not a response");
        };
        absorb(&mut entries, SsdpMessage::Notify(headers), now);
        assert!(ssdp_peers(&entries, now).is_empty());
    }

    #[test]
    fn an_ssdp_peer_expires_with_max_age() {
        let text = ssdp_response("urn:schemas-upnp-org:device:MediaRenderer:1", "max-age=30");
        let SsdpMessage::Response(headers) = parse_ssdp(&text).unwrap() else {
            panic!();
        };
        let mut entries = HashMap::new();
        let now = Instant::now();
        absorb(&mut entries, SsdpMessage::Response(headers), now);
        assert!(ssdp_peers(&entries, now + Duration::from_secs(31)).is_empty());
    }

    #[test]
    fn only_castable_ssdp_targets_are_kept() {
        let text = ssdp_response("urn:schemas-upnp-org:device:Basic:1", "max-age=1800");
        let SsdpMessage::Response(headers) = parse_ssdp(&text).unwrap() else {
            panic!();
        };
        let mut entries = HashMap::new();
        absorb(&mut entries, SsdpMessage::Response(headers), Instant::now());
        assert!(ssdp_peers(&entries, Instant::now()).is_empty());
    }

    #[test]
    fn locations_split_into_authority_and_path() {
        assert_eq!(
            split_location("http://192.168.1.20:9197/dmr?x=1").unwrap(),
            ("192.168.1.20:9197", "/dmr?x=1")
        );
        assert_eq!(
            split_location("http://tv.local").unwrap(),
            ("tv.local", "/")
        );
        assert!(split_location("https://tv.local/desc").is_err());
        assert_eq!(split_authority("192.168.1.20:9197").unwrap(), ("192.168.1.20", 9197));
        assert_eq!(split_authority("tv.local").unwrap(), ("tv.local", 80));
        assert_eq!(split_authority("[fe80::1]:9197").unwrap(), ("fe80::1", 9197));
    }

    #[test]
    fn the_description_reader_takes_the_tags_it_needs() {
        let xml = "<root><device><friendlyName>客厅电视 &amp; 投影</friendlyName>\
                   <manufacturer>ACME</manufacturer><modelName>TV-9</modelName></device></root>";
        assert_eq!(
            xml_tag(xml, "friendlyName").as_deref(),
            Some("客厅电视 & 投影")
        );
        assert_eq!(xml_tag(xml, "manufacturer").as_deref(), Some("ACME"));
        assert_eq!(xml_tag(xml, "modelName").as_deref(), Some("TV-9"));
        assert_eq!(xml_tag(xml, "missing"), None);
    }

    /// The codec is tested above without a network; this proves the socket is
    /// actually joined to the group and that a datagram addressed to it is
    /// received. Ignored by default because it needs multicast loopback to be
    /// allowed on the host — run it with `cargo test -- --ignored` when a silent
    /// discovery result needs to be explained.
    #[test]
    #[ignore = "needs multicast loopback on the host"]
    fn a_response_sent_to_the_group_is_received() {
        let Ok(mut mdns) = MdnsSocket::bind() else {
            panic!("could not bind 5353; is another mDNS responder holding it without reuse?");
        };
        // A plain sender would use the OS default multicast interface, which is
        // the very ambiguity `primary_interface` exists to avoid.
        let sender = multicast_socket(0, MDNS_GROUP, 255).unwrap();
        sender
            .send_to(
                &cast_response(120),
                SocketAddr::from((MDNS_GROUP, MDNS_PORT)),
            )
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let now = Instant::now();
            mdns.drain(now).unwrap();
            if !mdns.browser.snapshot(now).is_empty() {
                break;
            }
            assert!(now < deadline, "the group datagram never arrived");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Search out, reply back: proves the M-SEARCH bytes, the multicast join and
    /// the unicast return path. Run with `cargo test -- --ignored`.
    #[test]
    #[ignore = "needs multicast loopback on the host"]
    fn an_ssdp_search_gets_a_unicast_reply() {
        // The searcher takes an ephemeral port so the unicast reply cannot be
        // delivered to the responder, which must own 1900 to hear the search:
        // multicast is delivered by port, not just by group.
        let mut ssdp = Ssdp::bind_to(0).unwrap();
        let responder = multicast_socket(SSDP_PORT, SSDP_GROUP, 4).unwrap();
        ssdp.maybe_query(Instant::now()).unwrap();

        let mut buf = [0u8; 1500];
        let deadline = Instant::now() + Duration::from_secs(2);
        let (n, from) = loop {
            match responder.recv_from(&mut buf) {
                Ok(got) => break got,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < deadline, "no M-SEARCH was sent");
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(e) => panic!("{e}"),
            }
        };
        let text = std::str::from_utf8(&buf[..n]).unwrap();
        assert!(text.starts_with("M-SEARCH"), "{text}");
        assert!(text.contains("MAN: \"ssdp:discover\""), "{text}");

        let reply = ssdp_response("urn:schemas-upnp-org:device:MediaRenderer:1", "max-age=1800");
        responder.send_to(&reply, from).unwrap();

        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let now = Instant::now();
            ssdp.drain(now).unwrap();
            if ssdp
                .snapshot(now)
                .iter()
                .any(|peer| peer.protocol == Protocol::Dlna)
            {
                break;
            }
            assert!(
                now < deadline,
                "the unicast SSDP reply never arrived"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}
