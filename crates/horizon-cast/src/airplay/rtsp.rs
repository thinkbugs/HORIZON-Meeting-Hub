//! RTSP and SDP for RAOP: the text protocol that opens and closes a stream.
//!
//! RTSP looks like HTTP/1.1 with a different method set, and RAOP uses the
//! resemblance literally: requests are `METHOD uri RTSP/1.0`, headers one per
//! line, a blank line, then an optional body sized by `Content-Length`. There is
//! no chunked encoding and no pipelining requirement, so a line reader is enough.
//!
//! The bodies that matter are SDP from `ANNOUNCE` and the key material hidden in
//! its `a=` lines. [`Sdp::parse`] turns that text into the codec, the frame size
//! and the wrapped AES key; everything else here is framing.
//!
//! Interleaved binary frames (`$` + channel + length) belong to later AirPlay
//! revisions. They are skipped rather than misread as a method line, so a sender
//! that tries one gets no answer instead of a garbled one.

use std::io::{BufRead, Write};
use std::net::Ipv4Addr;

use crate::airplay::crypto::base64_decode;
use crate::error::{CastError, CastResult};

/// One RTSP request. Header names are compared case-insensitively, which is what
/// senders expect even though they all spell `CSeq` the same way.
#[derive(Debug, Clone, Default)]
pub struct Request {
    pub method: String,
    pub uri: String,
    /// `RTSP/1.0`, or `HTTP/1.0`/`HTTP/1.1` for senders that use HTTP framing.
    pub version: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Request {
    pub fn new(method: impl Into<String>, uri: impl Into<String>) -> Self {
        Self {
            method: method.into(),
            uri: uri.into(),
            version: "RTSP/1.0".to_string(),
            headers: Vec::new(),
            body: Vec::new(),
        }
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    pub fn cseq(&self) -> Option<u32> {
        self.header("CSeq")?.trim().parse().ok()
    }

    pub fn header_count(&self, name: &str) -> usize {
        self.headers
            .iter()
            .filter(|(key, _)| key.eq_ignore_ascii_case(name))
            .count()
    }

    pub fn set_header(&mut self, name: &str, value: impl Into<String>) {
        self.headers.push((name.to_string(), value.into()));
    }

    pub fn body_text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    /// Parses exactly one request from an in-memory buffer.
    pub fn parse(input: &[u8]) -> CastResult<Self> {
        let mut slice = input;
        Self::read(&mut slice)?.ok_or(CastError::Malformed("empty rtsp request"))
    }

    /// Reads one request. `None` means the peer closed the connection cleanly
    /// before a method line.
    pub fn read<R: BufRead>(reader: &mut R) -> CastResult<Option<Self>> {
        let request_line = loop {
            skip_interleaved(reader)?;
            let line = read_line(reader)?;
            let Some(line) = line else {
                return Ok(None);
            };
            if line.is_empty() {
                continue;
            }
            break line;
        };

        let mut parts = request_line.split_whitespace();
        let method = parts
            .next()
            .ok_or(CastError::Malformed("rtsp method missing"))?;
        let uri = parts.next().ok_or(CastError::Malformed("rtsp uri missing"))?;
        let version = parts
            .next()
            .ok_or(CastError::Malformed("rtsp version missing"))?;
        // AirPlay senders occasionally use HTTP framing; the methods and headers
        // are identical, so accept both and mirror the version in the reply.
        if !version.starts_with("RTSP/") && !version.starts_with("HTTP/") {
            return Err(CastError::Malformed("not an rtsp request line"));
        }

        let mut request = Request::new(method, uri);
        request.version = version.to_string();
        request.headers = read_headers(reader)?;
        request.body = read_body(reader, &request.headers)?;
        Ok(Some(request))
    }

    pub fn write_to<W: Write>(&self, out: &mut W) -> std::io::Result<()> {
        let version = if self.version.is_empty() {
            "RTSP/1.0"
        } else {
            &self.version
        };
        write!(out, "{} {} {}\r\n", self.method, self.uri, version)?;
        for (name, value) in &self.headers {
            write!(out, "{name}: {value}\r\n")?;
        }
        write!(out, "Content-Length: {}\r\n\r\n", self.body.len())?;
        out.write_all(&self.body)?;
        out.flush()
    }
}

/// One RTSP response. RAOP senders key off `CSeq`; without it they wait forever,
/// so [`Response::new`] takes the sequence number rather than trusting a caller
/// to remember.
#[derive(Debug, Clone)]
pub struct Response {
    pub status: u16,
    pub reason: String,
    /// Mirrors the request's protocol version; `RTSP/1.0` unless told otherwise.
    pub version: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Response {
    pub fn new(status: u16, cseq: Option<u32>) -> Self {
        let reason = match status {
            200 => "OK",
            400 => "Bad Request",
            401 => "Unauthorized",
            403 => "Forbidden",
            404 => "Not Found",
            405 => "Method Not Allowed",
            453 => "Not Enough Bandwidth",
            461 => "Unsupported Transport",
            500 => "Internal Server Error",
            501 => "Not Implemented",
            _ => "RTSP Response",
        };
        let mut response = Self {
            status,
            reason: reason.to_string(),
            version: "RTSP/1.0".to_string(),
            headers: Vec::new(),
            body: Vec::new(),
        };
        if let Some(cseq) = cseq {
            response.set_header("CSeq", cseq.to_string());
        }
        response
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    pub fn set_header(&mut self, name: &str, value: impl Into<String>) {
        self.headers.push((name.to_string(), value.into()));
    }

    /// Replies in the same protocol dialect the request used. Empty keeps the
    /// default `RTSP/1.0`.
    pub fn set_version(&mut self, version: impl Into<String>) {
        let version = version.into();
        if !version.is_empty() {
            self.version = version;
        }
    }

    pub fn set_body(&mut self, content_type: &str, body: impl Into<Vec<u8>>) {
        self.set_header("Content-Type", content_type);
        self.body = body.into();
    }

    pub fn write_to<W: Write>(&self, out: &mut W) -> std::io::Result<()> {
        write!(out, "{} {} {}\r\n", self.version, self.status, self.reason)?;
        for (name, value) in &self.headers {
            write!(out, "{name}: {value}\r\n")?;
        }
        // Always present, even at zero: a sender using HTTP framing keeps the
        // connection open, and without a length it cannot tell where a bodiless
        // reply (pair-verify M4) ends — it waits for bytes until it times out.
        write!(out, "Content-Length: {}\r\n", self.body.len())?;
        write!(out, "\r\n")?;
        out.write_all(&self.body)?;
        out.flush()
    }

    /// Reads one response. Only the emulated sender in `airplay-probe` needs
    /// this, which is why it lives next to the writer.
    pub fn read<R: BufRead>(reader: &mut R) -> CastResult<Option<Self>> {
        let line = loop {
            skip_interleaved(reader)?;
            let line = read_line(reader)?;
            let Some(line) = line else {
                return Ok(None);
            };
            if line.is_empty() {
                continue;
            }
            break line;
        };
        let mut parts = line.split_whitespace();
        let version = parts.next().ok_or(CastError::Malformed("rtsp status line"))?;
        if !version.starts_with("RTSP/") && !version.starts_with("HTTP/") {
            return Err(CastError::Malformed("not an rtsp response line"));
        }
        let status: u16 = parts
            .next()
            .ok_or(CastError::Malformed("rtsp status missing"))?
            .parse()
            .map_err(|_| CastError::Malformed("rtsp status"))?;
        let reason = parts.collect::<Vec<_>>().join(" ");
        let headers = read_headers(reader)?;
        let body = read_body(reader, &headers)?;
        Ok(Some(Self {
            status,
            reason,
            version: version.to_string(),
            headers,
            body,
        }))
    }
}

fn read_line<R: BufRead>(reader: &mut R) -> CastResult<Option<String>> {
    let mut line = String::new();
    let read = match reader.read_line(&mut line) {
        Ok(read) => read,
        Err(error) => {
            crate::airplay::session::tracef(format_args!(
                "read failed: {error} ({:?})",
                error.kind()
            ));
            return Err(error.into());
        }
    };
    if read == 0 {
        return Ok(None);
    }
    while line.ends_with('\n') || line.ends_with('\r') {
        line.pop();
    }
    Ok(Some(line))
}

fn read_headers<R: BufRead>(reader: &mut R) -> CastResult<Vec<(String, String)>> {
    let mut headers = Vec::new();
    loop {
        let Some(line) = read_line(reader)? else {
            return Err(CastError::Malformed("rtsp ended inside the headers"));
        };
        if line.is_empty() {
            return Ok(headers);
        }
        let Some((name, value)) = line.split_once(':') else {
            return Err(CastError::Malformed("rtsp header without a colon"));
        };
        headers.push((name.trim().to_string(), value.trim().to_string()));
    }
}

fn read_body<R: BufRead>(reader: &mut R, headers: &[(String, String)]) -> CastResult<Vec<u8>> {
    let length = headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("Content-Length"))
        .and_then(|(_, value)| value.trim().parse::<usize>().ok())
        .unwrap_or(0);
    if length > 1 << 20 {
        return Err(CastError::Malformed("rtsp body is implausibly large"));
    }
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body)?;
    Ok(body)
}

/// An interleaved frame is `$`, one channel byte, a big-endian length, payload.
/// Peeking rather than line-reading matters: the payload is arbitrary audio and
/// may not contain a newline for a long time.
fn skip_interleaved<R: BufRead>(reader: &mut R) -> CastResult<()> {
    loop {
        let available = reader.fill_buf()?;
        if available.first() != Some(&b'$') {
            return Ok(());
        }
        let mut header = [0u8; 4];
        reader
            .read_exact(&mut header)
            .map_err(|_| CastError::Malformed("truncated interleaved header"))?;
        let length = u16::from_be_bytes([header[2], header[3]]) as usize;
        let mut discard = vec![0u8; length];
        reader
            .read_exact(&mut discard)
            .map_err(|_| CastError::Malformed("truncated interleaved payload"))?;
    }
}

// ------------------------------------------------------------------ sdp

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    /// Raw 16-bit signed PCM, big-endian on the wire.
    L16,
    /// Apple Lossless. Recognised so it can be refused with a reason.
    AppleLossless,
    Other,
}

/// The parts of an `ANNOUNCE` body the receiver acts on.
#[derive(Debug, Clone, Default)]
pub struct Sdp {
    pub address: Option<Ipv4Addr>,
    pub port: u16,
    pub payload_type: u8,
    /// The `rtpmap` encoding name, verbatim (`L16`, `AppleLossless`, …).
    pub encoding: String,
    pub sample_rate: u32,
    pub channels: u8,
    pub frames_per_packet: usize,
    pub rsa_aes_key: Option<Vec<u8>>,
    pub aes_iv: Option<Vec<u8>>,
}

impl Sdp {
    pub fn codec(&self) -> Codec {
        if self.encoding.eq_ignore_ascii_case("L16") {
            Codec::L16
        } else if self.encoding.eq_ignore_ascii_case("AppleLossless") {
            Codec::AppleLossless
        } else {
            Codec::Other
        }
    }

    pub fn parse(text: &str) -> CastResult<Self> {
        let mut sdp = Sdp {
            sample_rate: 44_100,
            channels: 2,
            frames_per_packet: 352,
            ..Sdp::default()
        };
        let mut saw_media = false;
        let mut fmtp: Option<String> = None;

        for line in text.lines() {
            let line = line.trim();
            let Some((kind, value)) = line.split_once('=') else {
                continue;
            };
            match kind {
                "m" => {
                    let mut parts = value.split_whitespace();
                    let _media = parts.next();
                    sdp.port = parts
                        .next()
                        .and_then(|port| port.parse().ok())
                        .unwrap_or(0);
                    let _protocol = parts.next();
                    sdp.payload_type = parts
                        .next()
                        .and_then(|pt| pt.parse().ok())
                        .unwrap_or(96);
                    saw_media = true;
                }
                "c" => {
                    let mut parts = value.split_whitespace();
                    if parts.next() == Some("IN") && parts.next() == Some("IP4") {
                        sdp.address = parts.next().and_then(|addr| addr.parse().ok());
                    }
                }
                "a" => {
                    if let Some(rest) = value.strip_prefix("rtpmap:") {
                        sdp.apply_rtpmap(rest);
                    } else if let Some(rest) = value.strip_prefix("fmtp:") {
                        // The token before the first space is the payload type,
                        // not part of the format parameters.
                        if let Some((_, parameters)) = rest.split_once(char::is_whitespace) {
                            fmtp = Some(parameters.to_string());
                        }
                    } else if let Some(rest) = value.strip_prefix("rsaaeskey:") {
                        sdp.rsa_aes_key = Some(
                            base64_decode(rest.trim())
                                .ok_or(CastError::Malformed("rsaaeskey is not base64"))?,
                        );
                    } else if let Some(rest) = value.strip_prefix("aesiv:") {
                        let iv = base64_decode(rest.trim())
                            .ok_or(CastError::Malformed("aesiv is not base64"))?;
                        if iv.len() != 16 {
                            return Err(CastError::Malformed("aesiv is not 16 bytes"));
                        }
                        sdp.aes_iv = Some(iv);
                    }
                }
                _ => {}
            }
        }

        if !saw_media {
            return Err(CastError::Malformed("sdp has no media line"));
        }
        if sdp.encoding.is_empty() {
            return Err(CastError::Malformed("sdp has no rtpmap"));
        }
        if let Some(fmtp) = fmtp {
            sdp.apply_fmtp(&fmtp);
        }
        if sdp.frames_per_packet == 0 {
            return Err(CastError::Malformed("sdp frame size is zero"));
        }
        Ok(sdp)
    }

    fn apply_rtpmap(&mut self, rest: &str) {
        let Some((pt, encoding)) = rest.split_once(char::is_whitespace) else {
            return;
        };
        let Ok(pt) = pt.trim().parse::<u8>() else {
            return;
        };
        if self.port != 0 && pt != self.payload_type {
            return;
        }
        self.payload_type = pt;
        let mut parts = encoding.trim().split('/');
        self.encoding = parts.next().unwrap_or_default().to_string();
        if let Some(rate) = parts.next().and_then(|r| r.trim().parse::<u32>().ok()) {
            if rate > 0 {
                self.sample_rate = rate;
            }
        }
        if let Some(channels) = parts.next().and_then(|c| c.trim().parse::<u8>().ok()) {
            if channels > 0 {
                self.channels = channels;
            }
        }
    }

    /// ALAC's `fmtp` is positional: frame length first, sample rate last. L16
    /// rarely carries one; when it does the first number is still the frame size.
    fn apply_fmtp(&mut self, fmtp: &str) {
        let numbers: Vec<u32> = fmtp
            .split_whitespace()
            .filter_map(|token| token.parse::<u32>().ok())
            .collect();
        if let Some(&frames) = numbers.first() {
            if frames > 0 {
                self.frames_per_packet = frames as usize;
            }
        }
        if let Some(&rate) = numbers.last() {
            if rate >= 8_000 {
                self.sample_rate = rate;
            }
        }
    }
}

// -------------------------------------------------------------- transport

/// The `Transport:` header a sender puts on `SETUP`. The receiver cares about
/// the sender's control and timing ports; `interleaved` is parsed so its absence
/// can be stated rather than assumed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Transport {
    pub control_port: Option<u16>,
    pub timing_port: Option<u16>,
    pub interleaved: Option<(u8, u8)>,
    pub mode: Option<String>,
}

pub fn parse_transport(value: &str) -> Transport {
    let mut transport = Transport::default();
    for part in value.split(';') {
        let part = part.trim();
        if let Some(port) = part.strip_prefix("control_port=") {
            transport.control_port = port.trim().parse().ok();
        } else if let Some(port) = part.strip_prefix("timing_port=") {
            transport.timing_port = port.trim().parse().ok();
        } else if let Some(pair) = part.strip_prefix("interleaved=") {
            let mut halves = pair.split('-');
            if let (Some(a), Some(b)) = (
                halves.next().and_then(|v| v.parse().ok()),
                halves.next().and_then(|v| v.parse().ok()),
            ) {
                transport.interleaved = Some((a, b));
            }
        } else if let Some(mode) = part.strip_prefix("mode=") {
            transport.mode = Some(mode.trim().to_string());
        }
    }
    transport
}

// ----------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;

    const ANNOUNCE_L16: &str = "v=0\r\no=iTunes 3413821434 0 IN IP4 192.168.0.52\r\n\
        s=iTunes\r\nc=IN IP4 192.168.0.52\r\nt=0 0\r\nm=audio 0 RTP/AVP 96\r\n\
        a=rtpmap:96 L16/44100/2\r\n\
        a=rsaaeskey:uiWSo+WEKHGnnyklbJkT+MFvoY3aHDtk071mtQ0iH25ZV3UAXa/0Cc6J38IGvxUxgcnMwA/e60pgFRiec+lnEKLADOK8XN8qYNh1Gu7sY9xGbpmUkZcbGfKrDnRn3KB3uVrvWto7zp8ahh+X5RXt8oKKqUAZ3iN4vF+67c7RirQucaoeyVxGlAqvpb9TNZ37v6VVJ7LaMJ9rq9xlydBgQxebqvz3MZc8CqcuVM1XDVPa1JI7jV4MnAVSIgyKCmBzMBl4izA2pWyIozc8mE2Cb5W2HvHMFIi7gjGMJvD6DLu+ItqwmqONUeFpO8rhMZSaZ+IabWVYmUOgCrm5YXUjog==\r\n\
        a=aesiv:AAECAwQFBgcICQoLDA0ODw==\r\n";

    #[test]
    fn an_announce_parses_into_codec_and_keys() {
        let sdp = Sdp::parse(ANNOUNCE_L16).expect("sdp");
        assert_eq!(sdp.address, Some(Ipv4Addr::new(192, 168, 0, 52)));
        assert_eq!(sdp.payload_type, 96);
        assert_eq!(sdp.codec(), Codec::L16);
        assert_eq!(sdp.sample_rate, 44_100);
        assert_eq!(sdp.channels, 2);
        assert_eq!(sdp.frames_per_packet, 352);
        assert_eq!(sdp.aes_iv, Some((0u8..16).collect::<Vec<_>>()));
        assert!(sdp.rsa_aes_key.as_ref().unwrap().len() == 256);
    }

    #[test]
    fn alac_is_recognised_from_its_positional_fmtp() {
        let text = "v=0\r\nm=audio 0 RTP/AVP 96\r\na=rtpmap:96 AppleLossless\r\n\
                    a=fmtp:96 352 0 16 40 10 14 2 255 0 0 44100\r\n";
        let sdp = Sdp::parse(text).expect("sdp");
        assert_eq!(sdp.codec(), Codec::AppleLossless);
        assert_eq!(sdp.frames_per_packet, 352);
        assert_eq!(sdp.sample_rate, 44_100);
    }

    #[test]
    fn a_body_without_a_media_line_is_refused() {
        assert!(Sdp::parse("v=0\r\na=rtpmap:96 L16/44100/2\r\n").is_err());
        assert!(Sdp::parse("v=0\r\nm=audio 0 RTP/AVP 96\r\n").is_err());
    }

    #[test]
    fn requests_round_trip_with_a_body() {
        let mut request = Request::new("ANNOUNCE", "rtsp://192.168.0.52/1234");
        request.set_header("CSeq", "2");
        request.set_header("Content-Type", "application/sdp");
        request.body = ANNOUNCE_L16.as_bytes().to_vec();
        let mut wire = Vec::new();
        request.write_to(&mut wire).unwrap();
        let parsed = Request::parse(&wire).unwrap();
        assert_eq!(parsed.method, "ANNOUNCE");
        assert_eq!(parsed.cseq(), Some(2));
        assert_eq!(parsed.body_text(), ANNOUNCE_L16);
    }

    #[test]
    fn two_pipelined_requests_are_read_one_at_a_time() {
        let text = "OPTIONS * RTSP/1.0\r\nCSeq: 1\r\n\r\n\
                    SET_PARAMETER rtsp://x/1 RTSP/1.0\r\nCSeq: 2\r\nContent-Type: text/parameters\r\n\
                    Content-Length: 13\r\n\r\nvolume: -12.0";
        let mut reader = std::io::BufReader::new(text.as_bytes());
        let first = Request::read(&mut reader).unwrap().unwrap();
        let second = Request::read(&mut reader).unwrap().unwrap();
        assert_eq!(first.method, "OPTIONS");
        assert_eq!(second.method, "SET_PARAMETER");
        assert_eq!(second.body_text(), "volume: -12.0");
        assert!(Request::read(&mut reader).unwrap().is_none());
    }

    #[test]
    fn a_response_always_carries_its_cseq() {
        let mut response = Response::new(200, Some(7));
        response.set_header("Audio-Jack-Status", "connected; type=analog");
        let mut wire = Vec::new();
        response.write_to(&mut wire).unwrap();
        let text = String::from_utf8(wire).unwrap();
        assert!(text.starts_with("RTSP/1.0 200 OK\r\n"));
        assert!(text.contains("CSeq: 7\r\n"));
        assert!(text.ends_with("\r\n\r\n"));
    }

    #[test]
    fn a_transport_header_yields_the_sender_ports() {
        let transport = parse_transport(
            "RTP/AVP/UDP;unicast;interleaved=0-1;mode=record;control_port=6001;timing_port=6002",
        );
        assert_eq!(transport.control_port, Some(6001));
        assert_eq!(transport.timing_port, Some(6002));
        assert_eq!(transport.interleaved, Some((0, 1)));
        assert_eq!(transport.mode.as_deref(), Some("record"));
    }
}
