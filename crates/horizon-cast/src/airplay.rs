//! AirPlay 1 receiver: RAOP audio and H.264 screen mirroring.
//!
//! Apple's AirPlay has two generations with little in common on the wire. This
//! module implements the first: [RAOP][raop] audio, plus the mirroring session an
//! iOS device starts when you pick a screen instead of a speaker. Control is
//! RTSP; audio is RTP with an optional RSA-wrapped AES layer; video is a separate
//! TCP connection with its own FairPlay-derived AES-CTR keystream.
//!
//! [raop]: https://openairplay.github.io/airplay-spec/
//!
//! ## The conversation
//!
//! | step | what happens |
//! |---|---|
//! | mDNS | `_raop._tcp` advertises `<MAC>@<name>` and `_airplay._tcp` advertises `<name>`, both on the RTSP port |
//! | `OPTIONS` | the sender may send `Apple-Challenge`; the reply is `Apple-Response` |
//! | `GET /info` | binary plist: codecs, `features`, `model`, the Ed25519 `pk` |
//! | `/pair-setup`, `/pair-verify` | Ed25519/X25519 handshake that authorises mirroring |
//! | `/fp-setup` | FairPlay exchange; the key message unlocks the video keystream |
//! | `ANNOUNCE` | SDP names the codec (`L16` or Apple Lossless) and carries `a=aesiv` and `a=rsaaeskey` |
//! | `SETUP` | audio: transport ports and `RTP-Info`; mirroring: binary plist with `streamConnectionID`, reply names the data port |
//! | `RECORD` | audio starts on UDP, H.264 frames start on the mirror TCP connection |
//! | `SET_PARAMETER` | volume and progress |
//! | `FLUSH` / `TEARDOWN` | pause and end |
//!
//! ## Crypto: RAOP audio
//!
//! RAOP senders do not use the receiver's published key. Every AirPlay 1 client
//! encrypts the per-session AES-128 key with **one well-known RSA key pair** — the
//! one from the AirPort Express — and receivers are expected to hold that private
//! key. It is not a secret, it cannot be rotated, and an AirPlay 1 receiver that
//! does not embed it cannot decrypt anything. [`crypto::RsaIdentity::raop`] holds
//! that key; it is 2048 bits because the senders are.
//!
//! The AES layer is CBC with a **fixed** IV for the whole stream: the sender picks
//! 16 bytes in `a=aesiv`, and every RTP payload is decrypted with that same IV.
//! Reusing an IV across messages would be a real weakness in a protocol that let
//! an attacker submit chosen plaintext; RAOP does not, and there is no other
//! choice — the sender will not accept a change. Each payload is padded to a
//! 16-byte boundary and the true length is implied by the codec.
//!
//! ## Crypto: mirroring
//!
//! Mirroring is gated by a four-step chain, and each step has its own key schedule:
//!
//! 1. [`pairing`] proves the receiver's identity. The sender sends its X25519 and
//!    Ed25519 public keys; the receiver answers with an ephemeral X25519 key and a
//!    signature over both keys, encrypted with AES-128-CTR under
//!    `SHA-512("Pair-Verify-AES-Key"‖shared)`.
//! 2. `/fp-setup` runs Apple's FairPlay exchange. The 164-byte key message is kept;
//!    [`fairplay`] turns it plus the 16-byte AES key from SETUP into the 16-byte
//!    stream key.
//! 3. SETUP names a `streamConnectionID`, which mixes into the keystream labels
//!    `AirPlayStreamKey` and `AirPlayStreamIV`.
//! 4. Each 128-byte frame header on the mirror connection says where its H.264
//!    payload begins and, in codec-config frames, the source geometry. The payload
//!    is AVCC with 4-byte length prefixes and is only decryptable after step 2.
//!
//! The details live in [`mirror`]; the point here is that no step can be skipped.
//!
//! ## Discovery and output
//!
//! The responder in [`mdns`] answers for both services and announces one A record
//! for the host. `_airplay._tcp` carries the feature bitmap and the pairing key,
//! which is what makes a sender offer this machine as a screen; `_raop._tcp`
//! advertises `cn=0` (L16 only), so senders offer the audio codec this module can
//! actually play.
//!
//! Received PCM goes to one of two sinks, chosen by `Config`: [`session::WavSink`]
//! writes a canonical 44-byte-header WAV for capture and tests, and
//! [`session::WaveOutSink`] feeds the Windows waveOut API through four rotating
//! 8 KiB buffers for live playback. Mirror frames leave through a
//! [`mirror::FrameSink`], which the desktop shell turns into WebCodecs input.
//!
//! ## What this receiver plays, and what it refuses
//!
//! - **L16 (`rtpmap:96 L16/44100/2`)** — raw 16-bit stereo PCM, 352 frames per
//!   packet. Decrypted, unwrapped into a WAV file or the sound card, and that is
//!   the whole path. This is the only codec here that needs no decoder.
//! - **Apple Lossless** — refused with RTSP 403 and a line in the log. ALAC needs
//!   a decoder that this project does not have; pretending to accept it would
//!   produce silence after a successful handshake.
//! - **H.264 mirroring** — accepted, decrypted and timestamped; the sender's
//!   SPS/PPS are prepended to every IDR so a decoder can start at any key frame.
//! - **AirPlay 2's encrypted channel** (ChaCha20-Poly1305, buffered audio) — not
//!   implemented. The mirroring handshake here is the AirPlay 1-style one: no SRP,
//!   no HomeKit pairing, no PIN. A sender that insists on those is refused.
//! - **Mirror audio (AAC-ELD)** — the stream is negotiated and its packets are
//!   counted, but not decoded. Silent mirroring is the documented limitation;
//!   claiming otherwise would produce a successful handshake with no sound.
//!
//! ## What is not implemented yet
//!
//! Clock recovery is the remaining gap: timing packets (`0xd2`/`0xd3`) and sync
//! packets (`0xd4`) are received and counted, but playback is driven by the sound
//! card's own clock rather than by the sender's timestamp. On a LAN that is
//! inaudible; across a congested link it would drift. Mirror frames carry the
//! sender's PTS and reach the webview with it, so a player can schedule them, but
//! this receiver does not itself queue to a sender clock.
//!
//! ## Verified
//!
//! The crypto layer is tested against vectors produced independently with Node's
//! OpenSSL bindings: the RSA-OAEP-SHA1 unwrap of a 16-byte AES key, the raw
//! PKCS#1 private-key operation behind `Apple-Response`, and AES-128-CBC
//! decryption of a PCM block. The RTSP, RTP and mDNS layers are tested with a
//! scripted sender (see `airplay-probe --selftest`) — an emulator that speaks the
//! wire format, not an Apple device. Its measured transcript on this machine:
//!
//! ```text
//! OPTIONS 200, Apple-Response 342 chars
//! ANNOUNCE 200 (rsaaeskey 256 bytes wrapped, L16)
//! SETUP 200: server audio 49401, control 49402, timing 49403
//! RECORD 200, Audio-Latency 11025
//! 19 of 20 packets sent, seq 1018 withheld
//! report: L16 encrypted, 20 packets, 7040 frames (0.160s), missing 0,
//!         1 resend request, 1 resend heard, sync 0
//! WAV 28204 bytes (expected 28204) — selftest passed in 726 ms
//! ```
//!
//! The withheld packet was requested with `0xd5` and recovered from the emulated
//! sender's `0xd6`, which is the one path a fire-and-forget receiver gets wrong.
//!
//! The mirroring path has its own scripted sender (`airplay-probe
//! --selftest-mirror`): `/info`, pair-setup, a real pair-verify signature check,
//! both FairPlay messages, the three plist SETUPs, RECORD, one AES-CTR key frame
//! over the mirror TCP connection, TEARDOWN.
//!
//! ```text
//! /info 200: features 0x1E5A7FFFF7, pk 2152f8d1…
//! pair-setup + pair-verify 200, shared secret established
//! fp-setup 142 + 32 bytes, key message stored
//! SETUP 200: mirror TCP 63978, timing 63974
//! mirror frame: 22 bytes, keyframe, 1920×1080, pts 77250000 µs
//! report: H.264 paired true fairplay true frames 1 keyframes 1 1920x1080
//! mirror selftest passed in 850 ms
//! ```
//!
//! No Apple hardware was available, so real-sender interop is unverified; what is
//! verified is every byte this module decides.

#[cfg(windows)]
pub mod crypto;
#[cfg(windows)]
pub mod fairplay;
#[cfg(windows)]
pub(crate) mod fairplay_tables;
#[cfg(windows)]
pub(crate) mod fp_replies;
#[cfg(windows)]
pub mod mdns;
#[cfg(windows)]
pub mod mirror;
#[cfg(windows)]
pub mod nacl;
#[cfg(windows)]
pub mod pairing;
#[cfg(windows)]
pub mod plist;
#[cfg(windows)]
pub mod rtsp;
#[cfg(windows)]
pub mod session;
