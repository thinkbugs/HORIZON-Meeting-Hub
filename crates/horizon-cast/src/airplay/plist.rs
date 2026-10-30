//! Apple's binary property lists, the body format of AirPlay's `/info` and
//! `SETUP`.
//!
//! AirPlay speaks binary plists, not XML: `bplist00`, an object table indexed by
//! an offset table, and a 32-byte trailer. This module implements the subset a
//! receiver needs — booleans, integers up to 64 bits, reals, strings, data,
//! arrays and dictionaries — and refuses anything it cannot represent rather
//! than guessing.
//!
//! ## Shape of the file
//!
//! ```text
//! bplist00
//! <objects, back to back>
//! <offset table: one offset per object, each offset_size bytes, big-endian>
//! <trailer: 6 zero bytes, offset_size, ref_size, object count, top object,
//!           offset-table offset — the last three as u64 big-endian>
//! ```
//!
//! Container objects store *references* (indices into the offset table), not
//! inline values, which is what lets a sender describe a whole `streams` array
//! without knowing the final layout. Lengths below 15 are in the marker nibble;
//! at 15 a separate integer object follows inline.
//!
//! ## Verified
//!
//! The two fixtures in `resources/airplay/` were written by the original .NET
//! receiver's `BinaryPlistWriter` and are parsed here byte for byte. Responses
//! are written by this module and read back by its own parser in the tests; the
//! reference reader accepted the same shapes when the fixtures were produced.

use std::collections::BTreeMap;

use crate::error::{CastError, CastResult};

const HEADER: &[u8; 8] = b"bplist00";
const TRAILER: usize = 32;
const MAX_DEPTH: usize = 32;

/// One plist value. Dictionaries keep their keys sorted, which makes both the
/// tests and the encoder deterministic.
#[derive(Debug, Clone, PartialEq)]
pub enum Plist {
    Bool(bool),
    Integer(i64),
    Real(f64),
    String(String),
    Data(Vec<u8>),
    Array(Vec<Plist>),
    Dict(BTreeMap<String, Plist>),
    Uid(u64),
    Null,
}

impl Plist {
    pub fn dict() -> Self {
        Plist::Dict(BTreeMap::new())
    }

    pub fn set(mut self, key: impl Into<String>, value: impl Into<Plist>) -> Self {
        if let Plist::Dict(ref mut map) = self {
            map.insert(key.into(), value.into());
        }
        self
    }

    pub fn get(&self, key: &str) -> Option<&Plist> {
        match self {
            Plist::Dict(map) => map.get(key),
            _ => None,
        }
    }

    pub fn as_dict(&self) -> Option<&BTreeMap<String, Plist>> {
        match self {
            Plist::Dict(map) => Some(map),
            _ => None,
        }
    }

    pub fn as_array(&self) -> Option<&[Plist]> {
        match self {
            Plist::Array(values) => Some(values),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Plist::String(value) => Some(value),
            _ => None,
        }
    }

    pub fn as_data(&self) -> Option<&[u8]> {
        match self {
            Plist::Data(value) => Some(value),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Plist::Bool(value) => Some(*value),
            _ => None,
        }
    }

    /// Integers are the only numeric type senders use for ports and stream
    /// types. A real that is exactly integral is accepted too, because some
    /// writers represent small numbers as doubles.
    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Plist::Integer(value) => Some(*value),
            Plist::Real(value) if value.fract() == 0.0 => Some(*value as i64),
            _ => None,
        }
    }

    pub fn parse(bytes: &[u8]) -> CastResult<Self> {
        let reader = Reader::new(bytes)?;
        reader.parse(reader.top(), 0)
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        Writer::new(self).finish()
    }
}

impl From<bool> for Plist {
    fn from(value: bool) -> Self {
        Plist::Bool(value)
    }
}

impl From<i32> for Plist {
    fn from(value: i32) -> Self {
        Plist::Integer(value as i64)
    }
}

impl From<u16> for Plist {
    fn from(value: u16) -> Self {
        Plist::Integer(value as i64)
    }
}

impl From<i64> for Plist {
    fn from(value: i64) -> Self {
        Plist::Integer(value)
    }
}

impl From<u64> for Plist {
    fn from(value: u64) -> Self {
        Plist::Integer(value as i64)
    }
}

impl From<f64> for Plist {
    fn from(value: f64) -> Self {
        Plist::Real(value)
    }
}

impl From<&str> for Plist {
    fn from(value: &str) -> Self {
        Plist::String(value.to_string())
    }
}

impl From<String> for Plist {
    fn from(value: String) -> Self {
        Plist::String(value)
    }
}

impl From<Vec<u8>> for Plist {
    fn from(value: Vec<u8>) -> Self {
        Plist::Data(value)
    }
}

impl From<&[u8]> for Plist {
    fn from(value: &[u8]) -> Self {
        Plist::Data(value.to_vec())
    }
}

impl From<Vec<Plist>> for Plist {
    fn from(value: Vec<Plist>) -> Self {
        Plist::Array(value)
    }
}

// ------------------------------------------------------------------ reader

struct Reader<'a> {
    bytes: &'a [u8],
    offsets: Vec<usize>,
    ref_size: usize,
    top: usize,
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> CastResult<Self> {
        if bytes.len() < HEADER.len() + TRAILER || &bytes[..8] != HEADER {
            return Err(CastError::Malformed("not a binary plist"));
        }
        let trailer = &bytes[bytes.len() - TRAILER..];
        let offset_size = trailer[6] as usize;
        let ref_size = trailer[7] as usize;
        let count = read_u64(&trailer[8..16]) as usize;
        let top = read_u64(&trailer[16..24]) as usize;
        let table_at = read_u64(&trailer[24..32]) as usize;

        if !(1..=8).contains(&offset_size)
            || !(1..=8).contains(&ref_size)
            || count == 0
            || count > 1 << 24
            || top >= count
        {
            return Err(CastError::Malformed("binary plist trailer"));
        }
        let table_end = table_at
            .checked_add(count * offset_size)
            .ok_or(CastError::Malformed("binary plist offset table"))?;
        if table_end > bytes.len() - TRAILER {
            return Err(CastError::Malformed("binary plist offset table"));
        }

        let mut offsets = Vec::with_capacity(count);
        for index in 0..count {
            let at = table_at + index * offset_size;
            let offset = read_uint(&bytes[at..at + offset_size]) as usize;
            if offset >= bytes.len() - TRAILER {
                return Err(CastError::Malformed("binary plist object offset"));
            }
            offsets.push(offset);
        }
        Ok(Self {
            bytes,
            offsets,
            ref_size,
            top,
        })
    }

    fn parse(&self, index: usize, depth: usize) -> CastResult<Plist> {
        if index >= self.offsets.len() {
            return Err(CastError::Malformed("binary plist reference"));
        }
        if depth > MAX_DEPTH {
            return Err(CastError::Malformed("binary plist nesting"));
        }
        let mut at = self.offsets[index];
        let marker = *self
            .bytes
            .get(at)
            .ok_or(CastError::Malformed("binary plist truncated"))?;
        at += 1;
        match marker >> 4 {
            0x0 => match marker & 0x0f {
                0x0 => Ok(Plist::Null),
                0x8 => Ok(Plist::Bool(false)),
                0x9 => Ok(Plist::Bool(true)),
                _ => Err(CastError::Malformed("binary plist marker")),
            },
            0x1 => {
                let size = integer_size(marker)?;
                let raw = self.slice(at, size)?;
                Ok(Plist::Integer(sign_extend(raw)))
            }
            0x2 => {
                let size = integer_size(marker)?;
                let raw = self.slice(at, size)?;
                let value = match size {
                    4 => f32::from_be_bytes(raw.try_into().unwrap()) as f64,
                    8 => f64::from_be_bytes(raw.try_into().unwrap()),
                    _ => return Err(CastError::Malformed("binary plist real size")),
                };
                Ok(Plist::Real(value))
            }
            // A date is a double counting seconds since 2001-01-01; nothing in
            // the AirPlay path reads one, so it stays a real.
            0x3 => {
                let raw = self.slice(at, 8)?;
                Ok(Plist::Real(f64::from_be_bytes(raw.try_into().unwrap())))
            }
            0x4 => {
                let (length, used) = self.length(at, marker & 0x0f)?;
                at += used;
                Ok(Plist::Data(self.slice(at, length)?.to_vec()))
            }
            0x5 => {
                let (length, used) = self.length(at, marker & 0x0f)?;
                at += used;
                let raw = self.slice(at, length)?;
                Ok(Plist::String(
                    String::from_utf8(raw.to_vec())
                        .map_err(|_| CastError::Malformed("binary plist ascii"))?,
                ))
            }
            0x6 => {
                let (length, used) = self.length(at, marker & 0x0f)?;
                at += used;
                let raw = self.slice(at, length * 2)?;
                let units: Vec<u16> = raw
                    .chunks_exact(2)
                    .map(|pair| u16::from_be_bytes([pair[0], pair[1]]))
                    .collect();
                Ok(Plist::String(String::from_utf16_lossy(&units)))
            }
            0x8 => {
                let size = (marker & 0x0f) as usize + 1;
                Ok(Plist::Uid(read_uint(self.slice(at, size)?)))
            }
            0xa | 0xc => {
                let (count, used) = self.length(at, marker & 0x0f)?;
                at += used;
                let mut values = Vec::with_capacity(count.min(1024));
                for item in 0..count {
                    let reference = self.reference(at + item * self.ref_size)?;
                    values.push(self.parse(reference, depth + 1)?);
                }
                Ok(Plist::Array(values))
            }
            0xd => {
                let (count, used) = self.length(at, marker & 0x0f)?;
                at += used;
                let keys_at = at;
                let values_at = at + count * self.ref_size;
                let mut map = BTreeMap::new();
                for item in 0..count {
                    let key_ref = self.reference(keys_at + item * self.ref_size)?;
                    let value_ref = self.reference(values_at + item * self.ref_size)?;
                    let key = self.parse(key_ref, depth + 1)?;
                    let key = match key {
                        Plist::String(key) => key,
                        _ => return Err(CastError::Malformed("binary plist dictionary key")),
                    };
                    map.insert(key, self.parse(value_ref, depth + 1)?);
                }
                Ok(Plist::Dict(map))
            }
            _ => Err(CastError::Malformed("binary plist marker")),
        }
    }

    /// Length nibbles: values below 15 are inline, 15 means "an integer object
    /// follows right here". The returned count excludes that integer object.
    fn length(&self, at: usize, nibble: u8) -> CastResult<(usize, usize)> {
        if nibble != 0x0f {
            return Ok((nibble as usize, 0));
        }
        let marker = *self
            .bytes
            .get(at)
            .ok_or(CastError::Malformed("binary plist length"))?;
        if marker >> 4 != 0x1 {
            return Err(CastError::Malformed("binary plist length marker"));
        }
        let size = integer_size(marker)?;
        let raw = self.slice(at + 1, size)?;
        let length = sign_extend(raw);
        if length < 0 {
            return Err(CastError::Malformed("binary plist negative length"));
        }
        Ok((length as usize, 1 + size))
    }

    fn reference(&self, at: usize) -> CastResult<usize> {
        Ok(read_uint(self.slice(at, self.ref_size)?) as usize)
    }

    fn slice(&self, at: usize, length: usize) -> CastResult<&'a [u8]> {
        self.bytes
            .get(at..at + length)
            .ok_or(CastError::Malformed("binary plist truncated"))
    }

    fn top(&self) -> usize {
        self.top
    }
}

fn integer_size(marker: u8) -> CastResult<usize> {
    let exponent = marker & 0x0f;
    if exponent > 3 {
        return Err(CastError::Malformed("binary plist integer size"));
    }
    Ok(1usize << exponent)
}

fn sign_extend(bytes: &[u8]) -> i64 {
    let mut value = 0u64;
    for byte in bytes {
        value = (value << 8) | *byte as u64;
    }
    // Shift the sign bit up and back down again: this is what makes a 1-byte
    // `0xff` the integer -1 rather than 255.
    let shift = 64 - bytes.len() * 8;
    ((value << shift) as i64) >> shift
}

fn read_uint(bytes: &[u8]) -> u64 {
    let mut value = 0u64;
    for byte in bytes.iter().take(8) {
        value = (value << 8) | *byte as u64;
    }
    value
}

fn read_u64(bytes: &[u8]) -> u64 {
    u64::from_be_bytes(bytes.try_into().expect("eight bytes"))
}

// ------------------------------------------------------------------ writer

enum Flat {
    Bool(bool),
    Integer(i64),
    Real(f64),
    String(String),
    Data(Vec<u8>),
    Array(Vec<usize>),
    Dict(Vec<(usize, usize)>),
    Uid(u64),
    Null,
}

struct Writer {
    objects: Vec<Flat>,
}

impl Writer {
    fn new(value: &Plist) -> Self {
        let mut writer = Self {
            objects: Vec::new(),
        };
        writer.flatten(value);
        writer
    }

    fn flatten(&mut self, value: &Plist) -> usize {
        let index = self.objects.len();
        // Reserve the slot before descending, so every child gets a higher
        // index than its parent; references are always forward or equal.
        self.objects.push(Flat::Null);
        let flat = match value {
            Plist::Bool(value) => Flat::Bool(*value),
            Plist::Integer(value) => Flat::Integer(*value),
            Plist::Real(value) => Flat::Real(*value),
            Plist::String(value) => Flat::String(value.clone()),
            Plist::Data(value) => Flat::Data(value.clone()),
            Plist::Uid(value) => Flat::Uid(*value),
            Plist::Null => Flat::Null,
            Plist::Array(values) => Flat::Array(values.iter().map(|item| self.flatten(item)).collect()),
            Plist::Dict(map) => {
                let mut entries = Vec::with_capacity(map.len());
                for (key, value) in map {
                    let key = self.flatten(&Plist::String(key.clone()));
                    let value = self.flatten(value);
                    entries.push((key, value));
                }
                Flat::Dict(entries)
            }
        };
        self.objects[index] = flat;
        index
    }

    fn ref_size(&self) -> usize {
        match self.objects.len() {
            0..=0xff => 1,
            0x100..=0xffff => 2,
            0x1_0000..=0xffff_ffff => 4,
            _ => 8,
        }
    }

    fn finish(self) -> Vec<u8> {
        let ref_size = self.ref_size();
        let mut bodies = Vec::with_capacity(self.objects.len() * 8);
        let mut offsets = Vec::with_capacity(self.objects.len());
        for object in &self.objects {
            // Offsets in the trailer table are absolute, header included.
            offsets.push(HEADER.len() + bodies.len());
            write_object(&mut bodies, object, ref_size);
        }
        let offset_size = choose_offset_size(bodies.len(), self.objects.len());
        let table_at = HEADER.len() + bodies.len();
        let mut out = Vec::with_capacity(table_at + self.objects.len() * offset_size + TRAILER);
        out.extend_from_slice(HEADER);
        out.extend_from_slice(&bodies);
        for offset in &offsets {
            push_uint(&mut out, *offset as u64, offset_size);
        }
        out.extend_from_slice(&[0u8; 6]);
        out.push(offset_size as u8);
        out.push(ref_size as u8);
        out.extend_from_slice(&(self.objects.len() as u64).to_be_bytes());
        out.extend_from_slice(&0u64.to_be_bytes());
        out.extend_from_slice(&(table_at as u64).to_be_bytes());
        out
    }
}

fn choose_offset_size(body_length: usize, count: usize) -> usize {
    for size in [1usize, 2, 4] {
        let total = body_length + 8 + count * size + TRAILER;
        if total < 1usize << (size * 8) {
            return size;
        }
    }
    8
}

fn write_object(out: &mut Vec<u8>, object: &Flat, ref_size: usize) {
    match object {
        Flat::Bool(true) => out.push(0x09),
        Flat::Bool(false) => out.push(0x08),
        Flat::Null => out.push(0x00),
        Flat::Integer(value) => write_integer(out, *value),
        Flat::Real(value) => {
            out.push(0x23);
            out.extend_from_slice(&value.to_be_bytes());
        }
        Flat::String(value) => {
            if value.is_ascii() {
                out.push(0x50);
                write_length(out, value.len());
                out.extend_from_slice(value.as_bytes());
            } else {
                let units: Vec<u16> = value.encode_utf16().collect();
                out.push(0x60);
                write_length(out, units.len());
                for unit in units {
                    out.extend_from_slice(&unit.to_be_bytes());
                }
            }
        }
        Flat::Data(value) => {
            out.push(0x40);
            write_length(out, value.len());
            out.extend_from_slice(value);
        }
        Flat::Uid(value) => {
            let bytes = value.to_be_bytes();
            let start = bytes.iter().position(|byte| *byte != 0).unwrap_or(7);
            let used = 8 - start;
            out.push(0x80 | (used as u8 - 1));
            out.extend_from_slice(&bytes[start..]);
        }
        Flat::Array(values) => {
            out.push(0xa0);
            write_length(out, values.len());
            for value in values {
                push_uint(out, *value as u64, ref_size);
            }
        }
        Flat::Dict(entries) => {
            out.push(0xd0);
            write_length(out, entries.len());
            for (key, _) in entries {
                push_uint(out, *key as u64, ref_size);
            }
            for (_, value) in entries {
                push_uint(out, *value as u64, ref_size);
            }
        }
    }
}

fn write_length(out: &mut Vec<u8>, length: usize) {
    let last = out.len() - 1;
    if length < 15 {
        out[last] |= length as u8;
    } else {
        // A nibble of 15 announces a separate integer object; without it a
        // reader takes the marker's zero nibble as an empty container and the
        // integer behind it as the next object.
        out[last] |= 0x0f;
        write_integer(out, length as i64);
    }
}

fn write_integer(out: &mut Vec<u8>, value: i64) {
    // Integers are signed two's complement, so a positive value needs a size
    // whose sign bit stays clear: 0xff in one byte would read back as -1.
    let size: usize = if value >= 0 {
        if value <= i8::MAX as i64 {
            1
        } else if value <= i16::MAX as i64 {
            2
        } else if value <= i32::MAX as i64 {
            4
        } else {
            8
        }
    } else if value >= i8::MIN as i64 {
        1
    } else if value >= i16::MIN as i64 {
        2
    } else if value >= i32::MIN as i64 {
        4
    } else {
        8
    };
    let bytes = value.to_be_bytes();
    out.push(0x10 | (size.trailing_zeros() as u8));
    out.extend_from_slice(&bytes[8 - size..]);
}

fn push_uint(out: &mut Vec<u8>, value: u64, size: usize) {
    let bytes = value.to_be_bytes();
    out.extend_from_slice(&bytes[8 - size..]);
}

#[cfg(test)]
mod tests {
    use super::*;

    const INITIAL: &[u8] = include_bytes!("../../resources/airplay/setup-initial.bplist");
    const STREAMS: &[u8] = include_bytes!("../../resources/airplay/setup-streams.bplist");

    #[test]
    fn the_reference_initial_setup_parses() {
        let plist = Plist::parse(INITIAL).expect("initial setup");
        assert_eq!(plist.get("et").and_then(Plist::as_i64), Some(4));
        assert_eq!(plist.get("timingPort").and_then(Plist::as_i64), Some(6000));
        assert_eq!(
            plist.get("isScreenMirroringSession").and_then(Plist::as_bool),
            Some(true)
        );
        let ekey = plist.get("ekey").and_then(Plist::as_data).expect("ekey");
        assert_eq!(ekey.len(), 72);
        assert_eq!(ekey[0], 0x40);
        assert_eq!(ekey[71], ((71 * 7 + 0x40) & 0xff) as u8);
        let eiv = plist.get("eiv").and_then(Plist::as_data).expect("eiv");
        assert_eq!(eiv.len(), 16);
        assert_eq!(eiv[0], 0x50);
    }

    #[test]
    fn the_reference_stream_setup_parses() {
        let plist = Plist::parse(STREAMS).expect("stream setup");
        let streams = plist.get("streams").and_then(Plist::as_array).expect("streams");
        assert_eq!(streams.len(), 2);

        let mirror = &streams[0];
        assert_eq!(mirror.get("type").and_then(Plist::as_i64), Some(110));
        assert_eq!(
            mirror.get("streamConnectionID").and_then(Plist::as_i64),
            Some(1_234_567_890_123)
        );

        let audio = &streams[1];
        assert_eq!(audio.get("type").and_then(Plist::as_i64), Some(96));
        assert_eq!(audio.get("audioFormat").and_then(Plist::as_i64), Some(0x0100_0000));
        assert_eq!(audio.get("spf").and_then(Plist::as_i64), Some(480));
    }

    #[test]
    fn a_stream_response_round_trips() {
        let response = Plist::Dict(BTreeMap::from([(
            "streams".to_string(),
            Plist::Array(vec![Plist::Dict(BTreeMap::from([
                ("type".to_string(), Plist::Integer(110)),
                ("dataPort".to_string(), Plist::Integer(50123)),
            ]))]),
        )]));
        let encoded = response.to_bytes();
        assert_eq!(&encoded[..8], HEADER);
        let parsed = Plist::parse(&encoded).expect("round trip");
        assert_eq!(parsed, response);
        let stream = &parsed.get("streams").unwrap().as_array().unwrap()[0];
        assert_eq!(stream.get("dataPort").and_then(Plist::as_i64), Some(50123));
    }

    #[test]
    fn large_and_negative_integers_survive() {
        for value in [i64::MIN, -70_000, -32_769, -129, -128, 0, 1, 255, 256, 65_535, 65_536, i64::MAX] {
            let plist = Plist::Dict(BTreeMap::from([(
                "v".to_string(),
                Plist::Integer(value),
            )]));
            let parsed = Plist::parse(&plist.to_bytes()).expect("round trip");
            assert_eq!(parsed.get("v").and_then(Plist::as_i64), Some(value));
        }
    }

    #[test]
    fn non_ascii_strings_round_trip_as_utf16() {
        let plist = Plist::Dict(BTreeMap::from([(
            "name".to_string(),
            Plist::String("HORIZON 会议中枢".to_string()),
        )]));
        let parsed = Plist::parse(&plist.to_bytes()).expect("round trip");
        assert_eq!(
            parsed.get("name").and_then(Plist::as_str),
            Some("HORIZON 会议中枢")
        );
    }

    #[test]
    fn containers_and_strings_past_fourteen_round_trip() {
        // Lengths of 15 and above live in a separate integer object after a
        // marker nibble of 15; a writer that forgets the nibble produces a
        // zero-length container that still parses.
        let entries: BTreeMap<String, Plist> = (0..20)
            .map(|index| (format!("key-{index}"), Plist::Integer(index)))
            .collect();
        let plist = Plist::dict()
            .set("dict", Plist::Dict(entries.clone()))
            .set(
                "array",
                Plist::Array((0..20).map(Plist::Integer).collect()),
            )
            .set("string", "a string longer than fourteen bytes")
            .set("data", vec![0xabu8; 40]);
        let parsed = Plist::parse(&plist.to_bytes()).expect("round trip");
        assert_eq!(parsed, plist);
    }

    #[test]
    fn a_malformed_file_is_refused() {
        assert!(Plist::parse(b"bplist00").is_err());
        assert!(Plist::parse(b"not a plist at all, definitely not").is_err());
        let mut broken = INITIAL.to_vec();
        broken[12] = 0xff; // root marker
        assert!(Plist::parse(&broken).is_err());
    }
}
