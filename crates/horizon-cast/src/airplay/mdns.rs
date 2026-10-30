//! The mDNS advertisement that makes this machine appear in an AirPlay sender's
//! list, as both a speaker and a screen.
//!
//! Two services share one RTSP port, because the sender resolves the port from
//! whichever advert it read and then runs its whole handshake there:
//!
//! - `_raop._tcp` with the instance `<MAC>@<name>` — RAOP audio. The MAC is also
//!   the device id used in `Apple-Challenge`.
//! - `_airplay._tcp` with the instance `<name>` — the AirPlay service proper.
//!   Screen mirroring is only offered when this record carries `features` and
//!   the receiver's Ed25519 `pk`; a sender will not even try `/pair-verify`
//!   against a speaker-only advert.
//!
//! Every PTR target, and the SRV/TXT owner that follows it, is the instance
//! name **fully qualified** (`<name>._airplay._tcp.local`). Apple's resolver
//! follows the PTR target verbatim and looks up SRV/TXT under exactly that
//! name — it never appends the service type — so a bare instance resolves to
//! nothing and the receiver stays invisible on iPhone, iPad and Mac.
//!
//! There is no registration step and no server: the whole protocol is a
//! multicast responder on 5353. Every response carries the records for both
//! services plus one A record for the host, which is what an unsolicited
//! announcement needs; a query for either service type is answered with the
//! whole set.
//!
//! The DNS encoder/decoder here is deliberately narrow — names, four record
//! types, no EDNS. It is not a general mDNS stack; it is the smallest thing that
//! answers a real sender without lying about what this receiver can play.
//!
//! `cn` on the RAOP record advertises **codec 0 (L16 PCM) only**. shairport-sync
//! also lists ALAC; this receiver does not decode it, and a TXT record that
//! claims otherwise would produce a successful handshake followed by silence.
//! `et=0,3,5` is the AirPlay 1 standard set and is honest here: plain and
//! RSA-wrapped AES for RAOP, FairPlay for the mirroring stream. `ft` and
//! `features` are the same bitmap in the two syntaxes, `0x1E5A7FFFF7`.
//!
//! ## Verified
//!
//! Tests cover name compression on read, the exact bytes of a SRV target, both
//! TXT records, and matching a query against either service type, either
//! instance and the host name. The live responder test is `--ignored` because it
//! binds 5353; it sends a real query to the real multicast group from an
//! ephemeral port and checks the unicast reply RFC 6762 §6.7 requires, and it
//! passes on the development machine when run explicitly
//! (`cargo test ... -- --ignored`).

use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::discovery::{multicast_socket, MDNS_GROUP, MDNS_PORT};
use crate::error::{CastError, CastResult};

const TYPE_A: u16 = 1;
const TYPE_PTR: u16 = 12;
const TYPE_TXT: u16 = 16;
const TYPE_SRV: u16 = 33;
const TYPE_ANY: u16 = 255;
const CLASS_IN: u16 = 1;
const CACHE_FLUSH: u16 = 0x8000;

/// AirPlay feature bits, `(0x1E << 32) | 0x5A7FFFF7`. The same value is what
/// `/info` returns as an integer; a mismatch makes iOS fall back to PIN pairing.
pub const FEATURES: u64 = 0x1E_5A7F_FFF7;

/// `features` / `ft` syntax: the low word first, then the high word.
pub fn features_txt() -> String {
    format!("0x{:X},0x{:X}", FEATURES as u32, FEATURES >> 32)
}

/// A stable product UUID for the `pi` record. Version 4 in its third group and
/// RFC 4122 variant in its fourth, so parsers that validate the shape accept it.
const PRODUCT_ID: &str = "686f7269-7a6f-4e00-8a4d-484f52495a4f";

/// What the responder publishes.
#[derive(Debug, Clone)]
pub struct Advert {
    pub name: String,
    pub port: u16,
    pub mac: [u8; 6],
    pub address: Ipv4Addr,
    /// The receiver's Ed25519 public key, hex, as advertised in `pk`. `None`
    /// means the handshake is not running and senders should see a speaker.
    pub public_key: Option<String>,
}

impl Advert {
    pub fn new(name: impl Into<String>, port: u16, mac: [u8; 6], address: Ipv4Addr) -> Self {
        Self {
            name: name.into(),
            port,
            mac,
            address,
            public_key: None,
        }
    }

    pub fn with_public_key(mut self, public_key: impl Into<String>) -> Self {
        self.public_key = Some(public_key.into());
        self
    }

    /// RAOP's service instance name: the device id, an `@`, then the display name.
    pub fn service(&self) -> String {
        let mac: String = self.mac.iter().map(|byte| format!("{byte:02X}")).collect();
        format!("{mac}@{}", self.name)
    }

    pub fn service_type(&self) -> String {
        "_raop._tcp.local".to_string()
    }

    /// The AirPlay service instance is just the display name.
    pub fn airplay_service(&self) -> String {
        self.name.clone()
    }

    pub fn airplay_service_type(&self) -> String {
        "_airplay._tcp.local".to_string()
    }

    pub fn host(&self) -> String {
        let label: String = self
            .name
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect();
        format!("{label}.local")
    }

    pub fn mac_colons(&self) -> String {
        self.mac
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<Vec<_>>()
            .join(":")
    }

    fn txt_raop(&self) -> Vec<String> {
        let mut txt = vec![
            "txtvers=1".to_string(),
            // L16 stereo PCM only; see the module note.
            "ch=2".to_string(),
            "cn=0".to_string(),
            // Plain, RSA-wrapped AES and FairPlay.
            "et=0,3,5".to_string(),
            "sv=false".to_string(),
            "da=true".to_string(),
            "sr=44100".to_string(),
            "ss=16".to_string(),
            "pw=false".to_string(),
            "vn=65537".to_string(),
            "tp=UDP".to_string(),
            "md=0,1,2".to_string(),
            "am=AppleTV3,2".to_string(),
            format!("ft={}", features_txt()),
            "vs=220.68".to_string(),
            "sf=0x4".to_string(),
            "vv=2".to_string(),
        ];
        if let Some(public_key) = &self.public_key {
            txt.push(format!("pk={public_key}"));
        }
        txt.sort();
        txt
    }

    fn txt_airplay(&self) -> Vec<String> {
        let mut txt = vec![
            // Access control level 0: open, no password or PIN. `pw` is the RAOP
            // spelling of the same fact; `_airplay` senders read `acl`.
            "acl=0".to_string(),
            format!("deviceid={}", self.mac_colons()),
            format!("features={}", features_txt()),
            "pw=false".to_string(),
            "flags=0x4".to_string(),
            // An AirPlay 1 receiver: no `protovers`, so a sender must not expect
            // the AirPlay 2 setup path (`/stream.xml`, event channel). AppleTV3,2
            // is the last Apple device that speaks exactly this dialect.
            "model=AppleTV3,2".to_string(),
            format!("pi={PRODUCT_ID}"),
            "srcvers=220.68".to_string(),
            "vv=2".to_string(),
        ];
        if let Some(public_key) = &self.public_key {
            txt.push(format!("pk={public_key}"));
        }
        txt.sort();
        txt
    }

    fn matches(&self, question: &Question) -> bool {
        if question.class & !CACHE_FLUSH != CLASS_IN {
            return false;
        }
        let name = question.name.trim_end_matches('.').to_ascii_lowercase();
        let wants = |kind: u16| question.kind == kind || question.kind == TYPE_ANY;
        if name == self.service_type() && wants(TYPE_PTR) {
            return true;
        }
        if name == self.airplay_service_type() && wants(TYPE_PTR) {
            return true;
        }
        if name == self.host().to_ascii_lowercase() && wants(TYPE_A) {
            return true;
        }
        let instance = wants(TYPE_SRV) || wants(TYPE_TXT) || wants(TYPE_PTR);
        if !instance {
            return false;
        }
        // The qualified name is what a sender resolves; the bare instance is kept
        // for resolvers that cached the name from an older advertiser.
        name == self.instance_name()
            || name == self.airplay_instance_name()
            || name == self.service().to_ascii_lowercase()
            || name == self.airplay_service().to_ascii_lowercase()
    }

    /// The fully qualified RAOP instance name a sender queries for. PTR rdata,
    /// SRV and TXT must all use this name: Apple's resolver follows the PTR
    /// target verbatim and never appends the service type itself.
    fn instance_name(&self) -> String {
        format!("{}.{}", self.service().to_ascii_lowercase(), self.service_type())
    }

    fn airplay_instance_name(&self) -> String {
        format!(
            "{}.{}",
            self.airplay_service().to_ascii_lowercase(),
            self.airplay_service_type()
        )
    }
}

pub struct Responder {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    advert: Advert,
}

impl Responder {
    pub fn start(advert: Advert) -> CastResult<Self> {
        let socket = multicast_socket(MDNS_PORT, MDNS_GROUP, 255)?;
        let stop = Arc::new(AtomicBool::new(false));
        let thread_advert = advert.clone();
        let thread_stop = Arc::clone(&stop);
        let thread = thread::Builder::new()
            .name("airplay-mdns".to_string())
            .spawn(move || respond(thread_advert, socket, thread_stop))
            .map_err(|_| CastError::Io)?;
        Ok(Self {
            stop,
            thread: Some(thread),
            advert,
        })
    }

    pub fn advert(&self) -> &Advert {
        &self.advert
    }

    pub fn stop(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for Responder {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn respond(advert: Advert, socket: UdpSocket, stop: Arc<AtomicBool>) {
    let group = SocketAddr::from((MDNS_GROUP, MDNS_PORT));
    let mut buffer = [0u8; 4096];
    let mut announcements = 0;
    let mut next_announce = Instant::now();

    while !stop.load(Ordering::Relaxed) {
        while let Ok((length, from)) = socket.recv_from(&mut buffer) {
            let Ok(questions) = parse_questions(&buffer[..length]) else {
                continue;
            };
            // A response, not a query: every mDNS peer hears it and no reply is
            // owed. Ignoring it also keeps two receivers on one LAN from
            // answering each other forever.
            if questions.is_empty() {
                continue;
            }
            if questions.iter().any(|question| advert.matches(question)) {
                // RFC 6762 §6.7: a query that did not come from 5353 belongs to
                // a legacy resolver, which never joined the group and must be
                // answered directly; the cached entry carries a short TTL.
                let legacy = from.port() != MDNS_PORT;
                let response = advert.response_for(120, legacy);
                let target = if legacy { from } else { group };
                let _ = socket.send_to(&response, target);
            }
        }

        // RFC 6762 wants a few unsolicited announcements so a sender that is
        // already browsing notices a receiver that just started.
        if announcements < 3 && Instant::now() >= next_announce {
            let _ = socket.send_to(&advert.response(120), group);
            announcements += 1;
            next_announce = Instant::now() + Duration::from_secs(1);
        }

        thread::sleep(Duration::from_millis(50));
    }

    // Goodbye records: TTL zero, so senders drop the entry instead of waiting for
    // it to expire.
    for _ in 0..3 {
        let _ = socket.send_to(&advert.response(0), group);
        thread::sleep(Duration::from_millis(20));
    }
}

impl Advert {
    fn response(&self, ttl: u32) -> Vec<u8> {
        self.response_for(ttl, false)
    }

    /// `legacy` is a unicast reply to a resolver that is not on 5353. RFC 6762
    /// §6.7 caps its TTL at ten seconds and forbids the cache-flush bit.
    fn response_for(&self, ttl: u32, legacy: bool) -> Vec<u8> {
        let ttl = if legacy { ttl.min(10) } else { ttl };
        let flush = if legacy { 0 } else { CACHE_FLUSH };
        let mut out = Vec::new();
        out.extend_from_slice(&0u16.to_be_bytes()); // id
        out.extend_from_slice(&0x8400u16.to_be_bytes()); // QR + AA
        out.extend_from_slice(&0u16.to_be_bytes()); // qdcount
        out.extend_from_slice(&7u16.to_be_bytes()); // ancount
        out.extend_from_slice(&0u16.to_be_bytes()); // nscount
        out.extend_from_slice(&0u16.to_be_bytes()); // arcount

        let service_type = self.service_type();
        let raop_instance = self.service();
        let airplay_type = self.airplay_service_type();
        let airplay_instance = self.airplay_service();

        // Each instance is one label plus the three service labels. Apple's
        // resolver follows the PTR target verbatim and looks up SRV/TXT under
        // that exact name, so a bare instance would leave the sender with a
        // service it can never resolve.
        let raop_labels = [raop_instance.as_str(), "_raop", "_tcp", "local"];
        let airplay_labels = [airplay_instance.as_str(), "_airplay", "_tcp", "local"];
        let host = self.host();

        for (record_type, labels, txt) in [
            (service_type.as_str(), raop_labels, self.txt_raop()),
            (airplay_type.as_str(), airplay_labels, self.txt_airplay()),
        ] {
            // PTR: service type -> the fully qualified instance.
            let mut rdata = Vec::new();
            write_labels(&mut rdata, &labels);
            push_record(&mut out, record_type, TYPE_PTR, CLASS_IN, ttl, &rdata);

            // SRV: instance -> host and port. 0 priority, 0 weight.
            let mut rdata = Vec::new();
            rdata.extend_from_slice(&0u16.to_be_bytes());
            rdata.extend_from_slice(&0u16.to_be_bytes());
            rdata.extend_from_slice(&self.port.to_be_bytes());
            write_name(&mut rdata, &host);
            push_record_labels(&mut out, &labels, TYPE_SRV, CLASS_IN | flush, ttl, &rdata);

            // TXT: one length-prefixed string per key.
            let mut rdata = Vec::with_capacity(txt.iter().map(|s| s.len() + 1).sum());
            for entry in &txt {
                let bytes = entry.as_bytes();
                let length = bytes.len().min(255);
                rdata.push(length as u8);
                rdata.extend_from_slice(&bytes[..length]);
            }
            push_record_labels(&mut out, &labels, TYPE_TXT, CLASS_IN | flush, ttl, &rdata);
        }

        // A: host -> address.
        let octets = self.address.octets();
        push_record(
            &mut out,
            &host,
            TYPE_A,
            CLASS_IN | flush,
            ttl,
            &octets,
        );

        out
    }
}

fn push_record(
    out: &mut Vec<u8>,
    name: &str,
    kind: u16,
    class: u16,
    ttl: u32,
    rdata: &[u8],
) {
    let labels: Vec<&str> = name.trim_end_matches('.').split('.').collect();
    push_record_labels(out, &labels, kind, class, ttl, rdata);
}

fn push_record_labels(
    out: &mut Vec<u8>,
    labels: &[&str],
    kind: u16,
    class: u16,
    ttl: u32,
    rdata: &[u8],
) {
    write_labels(out, labels);
    out.extend_from_slice(&kind.to_be_bytes());
    out.extend_from_slice(&class.to_be_bytes());
    out.extend_from_slice(&ttl.to_be_bytes());
    out.extend_from_slice(&(rdata.len() as u16).to_be_bytes());
    out.extend_from_slice(rdata);
}

fn write_name(out: &mut Vec<u8>, name: &str) {
    let labels: Vec<&str> = name.trim_end_matches('.').split('.').collect();
    write_labels(out, &labels);
}

fn write_labels(out: &mut Vec<u8>, labels: &[&str]) {
    for label in labels {
        if label.is_empty() {
            continue;
        }
        let bytes = label.as_bytes();
        let length = bytes.len().min(63);
        out.push(length as u8);
        out.extend_from_slice(&bytes[..length]);
    }
    out.push(0);
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Question {
    pub name: String,
    pub kind: u16,
    pub class: u16,
}

/// A response (QR set) parses to an empty list; that is how the responder tells
/// queries from answers.
fn parse_questions(packet: &[u8]) -> CastResult<Vec<Question>> {
    if packet.len() < 12 {
        return Err(CastError::Malformed("dns packet too short"));
    }
    let flags = u16::from_be_bytes([packet[2], packet[3]]);
    if flags & 0x8000 != 0 {
        return Ok(Vec::new());
    }
    let count = u16::from_be_bytes([packet[4], packet[5]]) as usize;
    let mut at = 12usize;
    let mut questions = Vec::with_capacity(count);
    for _ in 0..count {
        let name = read_name(packet, &mut at)?;
        if at + 4 > packet.len() {
            return Err(CastError::Malformed("dns question truncated"));
        }
        let kind = u16::from_be_bytes([packet[at], packet[at + 1]]);
        let class = u16::from_be_bytes([packet[at + 2], packet[at + 3]]);
        at += 4;
        questions.push(Question { name, kind, class });
    }
    Ok(questions)
}

fn read_name(packet: &[u8], at: &mut usize) -> CastResult<String> {
    let mut labels = Vec::new();
    let mut cursor = *at;
    let mut jumped = false;
    let mut hops = 0;
    loop {
        let length = *packet
            .get(cursor)
            .ok_or(CastError::Malformed("dns name truncated"))?;
        if length == 0 {
            cursor += 1;
            if !jumped {
                *at = cursor;
            }
            break;
        }
        if length & 0xc0 == 0xc0 {
            let low = *packet
                .get(cursor + 1)
                .ok_or(CastError::Malformed("dns pointer truncated"))?;
            let target = (((length & 0x3f) as usize) << 8) | low as usize;
            if !jumped {
                *at = cursor + 2;
                jumped = true;
            }
            hops += 1;
            if hops > 8 || target >= packet.len() {
                return Err(CastError::Malformed("dns pointer loop"));
            }
            cursor = target;
            continue;
        }
        let end = cursor + 1 + length as usize;
        let label = packet
            .get(cursor + 1..end)
            .ok_or(CastError::Malformed("dns label truncated"))?;
        labels.push(String::from_utf8_lossy(label).into_owned());
        cursor = end;
    }
    Ok(labels.join("."))
}

// ----------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;

    fn advert() -> Advert {
        Advert::new(
            "HORIZON Meeting Hub",
            5000,
            [0x02, 0x11, 0x22, 0x33, 0x44, 0x55],
            Ipv4Addr::new(192, 168, 0, 52),
        )
    }

    #[test]
    fn the_instance_name_is_mac_at_display_name() {
        assert_eq!(
            advert().service(),
            "021122334455@HORIZON Meeting Hub"
        );
    }

    /// Walk every answer and return `(owner, type, rdata)`.
    fn records(response: &[u8]) -> Vec<(String, u16, Vec<u8>)> {
        let count = u16::from_be_bytes([response[6], response[7]]) as usize;
        let mut at = 12usize;
        let mut out = Vec::with_capacity(count);
        for _ in 0..count {
            let owner = read_name(response, &mut at).unwrap();
            let kind = u16::from_be_bytes([response[at], response[at + 1]]);
            let rdlength = u16::from_be_bytes([response[at + 8], response[at + 9]]) as usize;
            out.push((owner, kind, response[at + 10..at + 10 + rdlength].to_vec()));
            at += 10 + rdlength;
        }
        assert_eq!(at, response.len(), "record walk did not consume the packet");
        out
    }

    fn txt_entries(rdata: &[u8]) -> Vec<String> {
        let mut entries = Vec::new();
        let mut at = 0usize;
        while at < rdata.len() {
            let length = rdata[at] as usize;
            let entry = &rdata[at + 1..at + 1 + length];
            entries.push(String::from_utf8_lossy(entry).into_owned());
            at += 1 + length;
        }
        entries
    }

    #[test]
    fn a_response_carries_both_services_and_one_address() {
        let response = advert().with_public_key("ab".repeat(32)).response(120);
        assert_eq!(u16::from_be_bytes([response[2], response[3]]), 0x8400);
        assert_eq!(u16::from_be_bytes([response[6], response[7]]), 7);

        let records = records(&response);
        let kinds: Vec<u16> = records.iter().map(|(_, kind, _)| *kind).collect();
        assert_eq!(
            kinds,
            vec![
                TYPE_PTR, TYPE_SRV, TYPE_TXT, // _raop
                TYPE_PTR, TYPE_SRV, TYPE_TXT, // _airplay
                TYPE_A,
            ]
        );
        assert_eq!(records[0].0, "_raop._tcp.local");
        assert_eq!(records[3].0, "_airplay._tcp.local");
    }

    /// Apple's resolver follows the PTR target verbatim and joins SRV/TXT by that
    /// exact name, so the instance must carry the service type or the sender sees
    /// the service and nothing underneath it.
    #[test]
    fn instance_names_on_the_wire_are_fully_qualified() {
        let response = advert().response(120);
        let records = records(&response);
        assert_eq!(
            records[1].0,
            "021122334455@HORIZON Meeting Hub._raop._tcp.local"
        );
        assert_eq!(records[4].0, "HORIZON Meeting Hub._airplay._tcp.local");

        let mut at = 0usize;
        assert_eq!(read_name(&records[0].2, &mut at).unwrap(), records[1].0);
        let mut at = 0usize;
        assert_eq!(read_name(&records[3].2, &mut at).unwrap(), records[4].0);
    }

    #[test]
    fn both_txt_records_publish_features_and_the_pairing_key() {
        let key = "ab".repeat(32);
        let response = advert().with_public_key(key.clone()).response(120);
        let records = records(&response);
        let raop = txt_entries(&records[2].2);
        let airplay = txt_entries(&records[5].2);
        let has = |entries: &[String], wanted: &str| entries.iter().any(|e| e == wanted);

        assert!(has(&raop, "cn=0"));
        assert!(has(&raop, "et=0,3,5"));
        assert!(has(&raop, "ft=0x5A7FFFF7,0x1E"));
        assert!(has(&raop, &format!("pk={key}")));
        assert!(has(&raop, "vv=2"));

        assert!(has(&airplay, "deviceid=02:11:22:33:44:55"));
        assert!(has(&airplay, "acl=0"));
        assert!(has(&airplay, "features=0x5A7FFFF7,0x1E"));
        assert!(has(&airplay, "model=AppleTV3,2"));
        assert!(has(&airplay, "flags=0x4"));
        assert!(has(&airplay, &format!("pk={key}")));
    }

    #[test]
    fn a_speaker_only_advert_has_no_pairing_key() {
        let response = advert().response(120);
        let records = records(&response);
        for index in [2usize, 5] {
            let txt = txt_entries(&records[index].2);
            assert!(!txt.iter().any(|entry| entry.starts_with("pk=")));
        }
    }

    #[test]
    fn the_srv_record_names_the_host_and_port() {
        let response = advert().response(120);
        let mut at = 12usize;
        let _ = read_name(&response, &mut at).unwrap(); // PTR owner
        let rdlength = u16::from_be_bytes([response[at + 8], response[at + 9]]) as usize;
        at += 10 + rdlength; // skip PTR rdata
        let _ = read_name(&response, &mut at).unwrap(); // SRV owner
        let rdlength = u16::from_be_bytes([response[at + 8], response[at + 9]]) as usize;
        at += 10;
        assert_eq!(u16::from_be_bytes([response[at + 4], response[at + 5]]), 5000);
        let mut target = at + 6;
        assert_eq!(read_name(&response, &mut target).unwrap(), "HORIZON-Meeting-Hub.local");
        assert_eq!(target, at + rdlength);
    }

    #[test]
    fn queries_match_type_instance_and_host() {
        let advert = advert();
        let query = |name: &str, kind: u16| Question {
            name: name.to_string(),
            kind,
            class: CLASS_IN,
        };
        assert_eq!(
            advert.service().to_ascii_lowercase(),
            "021122334455@horizon meeting hub"
        );
        assert!(advert.matches(&query("_raop._tcp.local", TYPE_PTR)));
        assert!(advert.matches(&query("_raop._tcp.local.", TYPE_ANY)));
        assert!(advert.matches(&query(
            "021122334455@horizon meeting hub._raop._tcp.local",
            TYPE_SRV
        )));
        assert!(advert.matches(&query("_airplay._tcp.local", TYPE_PTR)));
        assert!(advert.matches(&query(
            "horizon meeting hub._airplay._tcp.local",
            TYPE_TXT
        )));
        assert!(advert.matches(&query("horizon-meeting-hub.local", TYPE_A)));
        assert!(!advert.matches(&query("_raop._tcp.local", TYPE_A)));
        assert!(!advert.matches(&query("_airplay._tcp.local", TYPE_A)));
    }

    #[test]
    fn a_response_packet_is_not_mistaken_for_a_query() {
        let response = advert().response(120);
        assert!(parse_questions(&response).unwrap().is_empty());
    }

    #[test]
    fn a_compressed_question_name_is_followed() {
        // Header with one question; the name uses a pointer back into itself is
        // illegal, so build a valid packet with the label once and a pointer to
        // it from a fake second question.
        let mut packet = vec![0u8; 12];
        packet[5] = 2; // qdcount
        let label_at = packet.len();
        write_name(&mut packet, "_raop._tcp.local");
        packet.extend_from_slice(&TYPE_PTR.to_be_bytes());
        packet.extend_from_slice(&CLASS_IN.to_be_bytes());
        // Second question: pointer to the name, then type ANY.
        packet.extend_from_slice(&[0xc0, label_at as u8, 0x00, 0xff, 0x00, 0x01]);
        let questions = parse_questions(&packet).unwrap();
        assert_eq!(questions.len(), 2);
        assert_eq!(questions[0].name, "_raop._tcp.local");
        assert_eq!(questions[1].name, "_raop._tcp.local");
        assert_eq!(questions[1].kind, TYPE_ANY);
    }

    #[test]
    fn a_legacy_query_gets_a_short_lived_response_without_cache_flush() {
        let response = advert().response_for(120, true);
        let mut at = 12usize;
        for _ in 0..7 {
            let _ = read_name(&response, &mut at).unwrap();
            let class = u16::from_be_bytes([response[at + 2], response[at + 3]]);
            let ttl = u32::from_be_bytes([
                response[at + 4],
                response[at + 5],
                response[at + 6],
                response[at + 7],
            ]);
            assert_eq!(class & CACHE_FLUSH, 0);
            assert_eq!(ttl, 10);
            let rdlength = u16::from_be_bytes([response[at + 8], response[at + 9]]) as usize;
            at += 10 + rdlength;
        }
    }

    #[test]
    #[ignore = "needs 5353 and multicast; run airplay-probe --selftest instead"]
    fn a_real_query_gets_a_real_response() {
        let responder = Responder::start(advert()).unwrap();
        // Port 0 makes this a legacy resolver, which RFC 6762 says is answered
        // by unicast — no group membership needed to hear the reply.
        let socket = UdpSocket::bind(("0.0.0.0", 0)).unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let mut query = vec![0u8; 12];
        query[5] = 1;
        write_name(&mut query, "_raop._tcp.local");
        query.extend_from_slice(&TYPE_PTR.to_be_bytes());
        query.extend_from_slice(&CLASS_IN.to_be_bytes());
        socket
            .send_to(&query, SocketAddr::from((MDNS_GROUP, MDNS_PORT)))
            .unwrap();
        let mut buffer = [0u8; 4096];
        let (length, from) = socket.recv_from(&mut buffer).unwrap();
        assert!(length > 12, "no response from the responder");
        assert_eq!(buffer[2] & 0x80, 0x80, "not a response packet");
        assert_eq!(from.port(), MDNS_PORT);
        responder.stop();
    }
}
