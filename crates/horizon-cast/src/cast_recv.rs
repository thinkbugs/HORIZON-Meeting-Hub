//! Google Cast v2 receiver: makes this machine appear as a Chromecast device.
//!
//! A Chromecast receiver is the mirror image of the sender in [`crate::cast`]:
//! instead of connecting to a device, this module listens for incoming CASTV2
//! connections from Chrome browsers and Android devices. The protocol is the
//! same — protobuf messages over TLS on port 8009 — but the roles are reversed.
//!
//! ## Layers
//!
//! 1. **Discovery**: an mDNS responder for `_googlecast._tcp.local` shaped like a
//!    real Chromecast: instance id, `id`/`fn`/`md`/`ve` TXT records, SRV to a
//!    unique host and an A record for the primary interface.
//! 2. **Control plane**: an SChannel TLS server on 8009. Each sender gets a
//!    session that answers CONNECT, heartbeats, receiver status, app
//!    availability, launch, volume and the media transport namespace.
//! 3. **Media plane**: LOAD hands the sender's URL to [`MediaEngineTransport`],
//!    which fetches and renders it with the same Media Foundation engine as the
//!    DLNA renderer. MEDIA_STATUS reports the engine's real clock, so a phone or
//!    browser can pause, seek and see progress.
//!
//! ## What this receiver supports
//!
//! - **Default Media Receiver** (`CC1AD845`): URL flinging, which is what
//!   Chrome's "Cast file", the Cast SDK and most enterprise apps use.
//! - **Receiver audio/video transport**: PLAY, PAUSE, SEEK, STOP, SET_VOLUME.
//! - **Discovery for every other app**, reported as `APP_UNAVAILABLE` so the
//!   sender never offers a session this receiver cannot serve.
//!
//! Mirrored tabs use Chrome's proprietary WebRTC push protocol and are not
//! implemented; the browser UI therefore shows this device for media casts, not
//! for tab mirroring. DRM-protected media is likewise out of scope — Media
//! Foundation has no licence exchange with the sender here.

use std::ffi::c_void;
use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use windows::core::{w, BOOL, PCWSTR};
use windows::Win32::Security::Cryptography::{
    CertCloseStore, CertEnumCertificatesInStore, CertFreeCertificateContext,
    CryptAcquireCertificatePrivateKey, NCryptFreeObject, NCryptSignHash, PFXImportCertStore,
    BCRYPT_PKCS1_PADDING_INFO, CERT_CONTEXT, CERT_KEY_SPEC, CERT_NCRYPT_KEY_SPEC,
    CRYPT_ACQUIRE_ONLY_NCRYPT_KEY_FLAG, CRYPT_INTEGER_BLOB, CRYPT_KEY_FLAGS,
    HCRYPTPROV_OR_NCRYPT_KEY_HANDLE, HCERTSTORE, NCRYPT_KEY_HANDLE,
    NCRYPT_PAD_PKCS1_FLAG, PKCS12_IMPORT_SILENT,
};

use crate::cast::tls::ServerCertificate;
use crate::cast::{
    CastMessage, FramePoll, FrameReader, NS_CONNECTION, NS_HEARTBEAT, NS_MEDIA, NS_RECEIVER,
    RECEIVER_ID,
};
use crate::discovery::{multicast_socket, MDNS_GROUP, MDNS_PORT};
use crate::error::{CastError, CastResult};
use crate::player::{MediaEngineTransport, Phase, Transport};

/// The Default Media Receiver app ID. This is a Google-provided app that plays
/// any media URL, so no developer registration is needed.
pub const DEFAULT_MEDIA_RECEIVER: &str = "CC1AD845";

/// CASTV2 protocol port. Chrome expects this on port 8009.
pub const CASTV2_PORT: u16 = 8009;

/// The virtual transport a launched app is addressed as.
const CAST_TRANSPORT_ID: &str = "web-0";

/// PAUSE | SEEK | STREAM_VOLUME | STREAM_MUTE, the commands the player honours.
const MEDIA_COMMANDS: u64 = 1 | 2 | 4 | 8;

/// How often an otherwise idle session looks for playback changes to push.
const STATUS_TICK: Duration = Duration::from_millis(250);

/// A position jump this large is a seek even when the phase did not change.
const STATUS_SEEK_MS: i64 = 1_500;

/// Device authentication, the first exchange on every Chromium Cast socket.
const NS_DEVICE_AUTH: &str = "urn:x-cast:com.google.cast.tp.deviceauth";

/// The internal root Chromium must be told to trust (`--cast-developer-
/// certificate-path`) before it will accept this receiver.
const DEV_ROOT_PEM: &[u8] = include_bytes!("../assets/cast/dev-root.pem");
const DEVICE_PFX: &[u8] = include_bytes!("../assets/cast/device.pfx");
const DEVICE_PFX_PASSWORD: &str = "horizon-cast";

static TRUST_ANCHOR: OnceLock<Option<PathBuf>> = OnceLock::new();

/// Path to the PEM Chromium needs as `--cast-developer-certificate-path`.
///
/// The file is copied out of the binary so a browser launch configuration has
/// something stable to point at. `None` only if the user profile directory is
/// unwritable, in which case the receiver still serves senders that do not
/// enforce Google device authentication.
pub fn trust_anchor_path() -> Option<&'static Path> {
    TRUST_ANCHOR
        .get_or_init(|| {
            let base = std::env::var_os("LOCALAPPDATA")?;
            let dir = PathBuf::from(base).join("HORIZON Meeting").join("cast");
            std::fs::create_dir_all(&dir).ok()?;
            let path = dir.join("cast-dev-root.pem");
            if std::fs::read(&path).ok().as_deref() != Some(DEV_ROOT_PEM) {
                std::fs::write(&path, DEV_ROOT_PEM).ok()?;
            }
            Some(path)
        })
        .as_deref()
}

/// The receiver's Cast device identity: a leaf certificate issued by the
/// internal root, plus the private key that answers Chromium's challenge.
struct DeviceIdentity {
    store: HCERTSTORE,
    cert: *const CERT_CONTEXT,
    key: NCRYPT_KEY_HANDLE,
    free_key: bool,
    client_der: Vec<u8>,
}

// The store, certificate and CNG key are process-wide handles that CryptoAPI
// documents as shareable; the certificate is only ever read.
unsafe impl Send for DeviceIdentity {}
unsafe impl Sync for DeviceIdentity {}

impl DeviceIdentity {
    fn load() -> CastResult<Self> {
        unsafe {
            let mut pfx = DEVICE_PFX.to_vec();
            let blob = CRYPT_INTEGER_BLOB {
                cbData: pfx.len() as u32,
                pbData: pfx.as_mut_ptr(),
            };
            let password: Vec<u16> = DEVICE_PFX_PASSWORD
                .encode_utf16()
                .chain(std::iter::once(0))
                .collect();
            let store = PFXImportCertStore(
                &blob,
                PCWSTR(password.as_ptr()),
                CRYPT_KEY_FLAGS(PKCS12_IMPORT_SILENT),
            )?;

            let mut cert = CertEnumCertificatesInStore(store, None);
            while !cert.is_null() {
                let mut key = HCRYPTPROV_OR_NCRYPT_KEY_HANDLE::default();
                let mut spec = CERT_KEY_SPEC::default();
                let mut free = BOOL(0);
                let acquired = CryptAcquireCertificatePrivateKey(
                    cert,
                    CRYPT_ACQUIRE_ONLY_NCRYPT_KEY_FLAG,
                    None,
                    &mut key,
                    Some(&mut spec),
                    Some(&mut free),
                );
                if acquired.is_ok() && spec == CERT_NCRYPT_KEY_SPEC {
                    let client_der = std::slice::from_raw_parts(
                        (*cert).pbCertEncoded,
                        (*cert).cbCertEncoded as usize,
                    )
                    .to_vec();
                    return Ok(Self {
                        store,
                        cert,
                        key: NCRYPT_KEY_HANDLE(key.0),
                        free_key: free.as_bool(),
                        client_der,
                    });
                }
                cert = CertEnumCertificatesInStore(store, Some(cert));
            }
            let _ = CertCloseStore(Some(store), 0);
            Err(CastError::InvalidState(
                "cast device certificate has no private key",
            ))
        }
    }

    fn sign(&self, input: &[u8], hash: HashAlgorithm) -> CastResult<Vec<u8>> {
        let digest = match hash {
            HashAlgorithm::Sha1 => sha1(input).to_vec(),
            HashAlgorithm::Sha256 => Sha256::digest(input).to_vec(),
        };
        let algorithm = match hash {
            HashAlgorithm::Sha1 => w!("SHA1"),
            HashAlgorithm::Sha256 => w!("SHA256"),
        };
        let padding = BCRYPT_PKCS1_PADDING_INFO { pszAlgId: algorithm };
        let padding = &padding as *const BCRYPT_PKCS1_PADDING_INFO as *const c_void;
        unsafe {
            let mut needed = 0u32;
            NCryptSignHash(
                self.key,
                Some(padding),
                &digest,
                None,
                &mut needed,
                NCRYPT_PAD_PKCS1_FLAG,
            )?;
            let mut signature = vec![0u8; needed as usize];
            NCryptSignHash(
                self.key,
                Some(padding),
                &digest,
                Some(&mut signature),
                &mut needed,
                NCRYPT_PAD_PKCS1_FLAG,
            )?;
            signature.truncate(needed as usize);
            Ok(signature)
        }
    }
}

/// The hash algorithm named by a device-auth challenge. Chromium's challenge
/// omits the field, and proto2's default is SHA-1 even on current builds, so
/// the signature has to follow the challenge rather than assume SHA-256.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HashAlgorithm {
    Sha1,
    Sha256,
}

impl HashAlgorithm {
    fn from_field(value: Option<u64>) -> Self {
        match value {
            Some(1) => Self::Sha256,
            _ => Self::Sha1,
        }
    }

    fn field(self) -> u64 {
        match self {
            Self::Sha1 => 0,
            Self::Sha256 => 1,
        }
    }
}

/// SHA-1, needed because Chromium's Cast device-auth challenge defaults to it.
pub(crate) fn sha1(input: &[u8]) -> [u8; 20] {
    let mut state = [
        0x6745_2301u32,
        0xEFCD_AB89,
        0x98BA_DCFE,
        0x1032_5476,
        0xC3D2_E1F0,
    ];
    let bit_length = (input.len() as u64).wrapping_mul(8);
    let mut data = Vec::with_capacity(input.len() + 72);
    data.extend_from_slice(input);
    data.push(0x80);
    while data.len() % 64 != 56 {
        data.push(0);
    }
    data.extend_from_slice(&bit_length.to_be_bytes());

    for chunk in data.chunks_exact(64) {
        let mut words = [0u32; 80];
        for (index, word) in words.iter_mut().take(16).enumerate() {
            *word = u32::from_be_bytes([
                chunk[index * 4],
                chunk[index * 4 + 1],
                chunk[index * 4 + 2],
                chunk[index * 4 + 3],
            ]);
        }
        for index in 16..80 {
            words[index] = (words[index - 3]
                ^ words[index - 8]
                ^ words[index - 14]
                ^ words[index - 16])
                .rotate_left(1);
        }

        let [mut a, mut b, mut c, mut d, mut e] = state;
        for (index, word) in words.iter().enumerate() {
            let (f, k) = match index {
                0..=19 => ((b & c) | (!b & d), 0x5A82_7999),
                20..=39 => (b ^ c ^ d, 0x6ED9_EBA1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1B_BCDC),
                _ => (b ^ c ^ d, 0xCA62_C1D6),
            };
            let temp = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(*word);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = temp;
        }
        state[0] = state[0].wrapping_add(a);
        state[1] = state[1].wrapping_add(b);
        state[2] = state[2].wrapping_add(c);
        state[3] = state[3].wrapping_add(d);
        state[4] = state[4].wrapping_add(e);
    }

    let mut digest = [0u8; 20];
    for (index, word) in state.iter().enumerate() {
        digest[index * 4..index * 4 + 4].copy_from_slice(&word.to_be_bytes());
    }
    digest
}

impl Drop for DeviceIdentity {
    fn drop(&mut self) {
        unsafe {
            if self.free_key {
                let _ = NCryptFreeObject(windows::Win32::Security::Cryptography::NCRYPT_HANDLE(
                    self.key.0,
                ));
            }
            let _ = CertFreeCertificateContext(Some(self.cert));
            let _ = CertCloseStore(Some(self.store), 0);
        }
    }
}

/// Answers the sender's [`DeviceAuthMessage`] challenge.
///
/// Chromium verifies the returned leaf against the Cast roots in its trust
/// store (or the developer root passed on its command line), then checks the
/// signature over `sender_nonce || tls_peer_certificate`. The signature is what
/// proves the responder owns the device key; the certificate chain is what
/// proves that key belongs to a Cast device.
fn device_auth_reply(
    challenge: &[u8],
    identity: &DeviceIdentity,
    tls_certificate: &[u8],
) -> CastResult<Vec<u8>> {
    let outer = pb_fields(challenge)?;
    let challenge = outer
        .iter()
        .find_map(|(number, value)| match (number, value) {
            (1, PbValue::Bytes(bytes)) => Some(*bytes),
            _ => None,
        })
        .ok_or(CastError::Malformed("device auth challenge missing"))?;
    let inner = pb_fields(challenge)?;
    let nonce = inner
        .iter()
        .find_map(|(number, value)| match (number, value) {
            (2, PbValue::Bytes(bytes)) => Some(*bytes),
            _ => None,
        })
        .unwrap_or_default();
    let signature_algorithm = inner
        .iter()
        .find_map(|(number, value)| match (number, value) {
            (1, PbValue::Varint(value)) => Some(*value),
            _ => None,
        })
        .unwrap_or(1);
    let hash = HashAlgorithm::from_field(inner.iter().find_map(|(number, value)| match (number, value) {
        (3, PbValue::Varint(value)) => Some(*value),
        _ => None,
    }));

    let mut signed = Vec::with_capacity(nonce.len() + tls_certificate.len());
    signed.extend_from_slice(nonce);
    signed.extend_from_slice(tls_certificate);
    let signature = identity.sign(&signed, hash)?;

    let mut response = Vec::new();
    pb_bytes(&mut response, 1, &signature);
    pb_bytes(&mut response, 2, &identity.client_der);
    pb_uint(&mut response, 4, signature_algorithm);
    pb_bytes(&mut response, 5, nonce);
    pb_uint(&mut response, 6, hash.field());

    let mut message = Vec::new();
    pb_bytes(&mut message, 2, &response);
    Ok(message)
}

enum PbValue<'a> {
    Varint(u64),
    Bytes(&'a [u8]),
    Other,
}

fn pb_fields(bytes: &[u8]) -> CastResult<Vec<(u32, PbValue<'_>)>> {
    let mut at = 0usize;
    let mut fields = Vec::new();
    while at < bytes.len() {
        let key = pb_varint(bytes, &mut at)?;
        let number = (key >> 3) as u32;
        match key & 7 {
            0 => fields.push((number, PbValue::Varint(pb_varint(bytes, &mut at)?))),
            2 => {
                let len = pb_varint(bytes, &mut at)? as usize;
                let end = at
                    .checked_add(len)
                    .filter(|end| *end <= bytes.len())
                    .ok_or(CastError::Malformed("protobuf field truncated"))?;
                fields.push((number, PbValue::Bytes(&bytes[at..end])));
                at = end;
            }
            1 => {
                at = at.saturating_add(8);
                fields.push((number, PbValue::Other));
            }
            5 => {
                at = at.saturating_add(4);
                fields.push((number, PbValue::Other));
            }
            _ => return Err(CastError::Malformed("unsupported protobuf wire type")),
        }
        if at > bytes.len() {
            return Err(CastError::Malformed("protobuf field truncated"));
        }
    }
    Ok(fields)
}

fn pb_varint(bytes: &[u8], at: &mut usize) -> CastResult<u64> {
    let mut value = 0u64;
    let mut shift = 0u32;
    loop {
        let byte = *bytes
            .get(*at)
            .ok_or(CastError::Malformed("protobuf varint truncated"))?;
        *at += 1;
        value |= ((byte & 0x7f) as u64) << shift;
        if byte & 0x80 == 0 {
            return Ok(value);
        }
        shift += 7;
        if shift >= 64 {
            return Err(CastError::Malformed("protobuf varint too long"));
        }
    }
}

fn pb_uint(out: &mut Vec<u8>, field: u32, value: u64) {
    put_varint(out, ((field as u64) << 3) | 0);
    put_varint(out, value);
}

fn pb_bytes(out: &mut Vec<u8>, field: u32, bytes: &[u8]) {
    put_varint(out, ((field as u64) << 3) | 2);
    put_varint(out, bytes.len() as u64);
    out.extend_from_slice(bytes);
}

fn put_varint(out: &mut Vec<u8>, mut value: u64) {
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        if value == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

/// Configuration for the Cast receiver.
#[derive(Debug, Clone)]
pub struct CastReceiverConfig {
    /// Device name shown in Chrome's "Cast" menu.
    pub name: String,
    /// Model name (e.g., "HORIZON Meeting").
    pub model: String,
    /// IPv4 address to bind to.
    pub address: Ipv4Addr,
    /// Port for CASTV2 TLS server (default 8009).
    pub port: u16,
}

impl Default for CastReceiverConfig {
    fn default() -> Self {
        Self {
            name: "HORIZON Meeting".to_string(),
            model: "HORIZON".to_string(),
            address: Ipv4Addr::UNSPECIFIED,
            port: CASTV2_PORT,
        }
    }
}

/// Media and app state shared by every CASTV2 connection to this receiver.
///
/// Chrome may reconnect (sleep/wake, network change) while an app keeps
/// running, so the launched app and the player cannot live inside one socket's
/// session. One receiver owns one [`MediaEngineTransport`]; sessions only borrow
/// it.
struct ReceiverState {
    player: Option<MediaEngineTransport>,
    app_id: Option<String>,
    session_id: String,
    media_session_id: i64,
    content_id: String,
    content_type: String,
    /// Duration announced by the sender, used until the engine knows better.
    declared_duration: f64,
    volume: f64,
    muted: bool,
}

impl Default for ReceiverState {
    fn default() -> Self {
        Self {
            player: None,
            app_id: None,
            session_id: String::new(),
            media_session_id: 0,
            content_id: String::new(),
            content_type: String::new(),
            declared_duration: 0.0,
            volume: 1.0,
            muted: false,
        }
    }
}

fn ensure_player(state: &mut ReceiverState, show_video: bool) -> CastResult<&MediaEngineTransport> {
    if state.player.is_none() {
        let player = MediaEngineTransport::start(show_video)?;
        let _ = player.set_volume(state.volume);
        let _ = player.set_mute(state.muted);
        state.player = Some(player);
    }
    Ok(state
        .player
        .as_ref()
        .expect("player was installed above"))
}

/// A running Cast receiver.
pub struct CastReceiver {
    stop: Arc<AtomicBool>,
    mdns_thread: Option<JoinHandle<()>>,
    server_thread: Option<JoinHandle<()>>,
    config: CastReceiverConfig,
    state: Arc<Mutex<ReceiverState>>,
}

impl CastReceiver {
    /// Starts the Cast receiver: mDNS advertisement + TLS server.
    pub fn start(config: CastReceiverConfig) -> CastResult<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let state = Arc::new(Mutex::new(ReceiverState::default()));

        // Chromium refuses a Cast socket whose TLS certificate is older than
        // four days, so the receiver mints a fresh one per run instead of
        // shipping a fixed key. Device authentication is what actually proves
        // identity to a Chromium sender; see `device_auth_reply`.
        let certificate = Arc::new(ServerCertificate::generate(&config.name)?);
        let identity = match DeviceIdentity::load() {
            Ok(identity) => Some(Arc::new(identity)),
            Err(error) => {
                if debug_enabled() {
                    eprintln!("cast: device authentication unavailable: {error}");
                }
                None
            }
        };

        let mdns_stop = Arc::clone(&stop);
        let mdns_config = config.clone();
        let mdns_thread = thread::Builder::new()
            .name("cast-recv-mdns".to_string())
            .spawn(move || mdns_responder(mdns_config, mdns_stop))
            .map_err(|_| CastError::Io)?;

        let server_stop = Arc::clone(&stop);
        let server_config = config.clone();
        let server_state = Arc::clone(&state);
        let server_certificate = Arc::clone(&certificate);
        let server_identity = identity.clone();
        let server_thread = thread::Builder::new()
            .name("cast-recv-server".to_string())
            .spawn(move || {
                tls_server(
                    server_config,
                    server_stop,
                    server_state,
                    server_certificate,
                    server_identity,
                )
            })
            .map_err(|_| CastError::Io)?;

        Ok(Self {
            stop,
            mdns_thread: Some(mdns_thread),
            server_thread: Some(server_thread),
            config,
            state,
        })
    }

    pub fn config(&self) -> &CastReceiverConfig {
        &self.config
    }

    fn shutdown(&self) {
        let mut state = lock(&self.state);
        if let Some(player) = state.player.take() {
            let _ = player.stop();
        }
        state.app_id = None;
        state.media_session_id = 0;
    }

    pub fn stop(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.mdns_thread.take() {
            let _ = thread.join();
        }
        if let Some(thread) = self.server_thread.take() {
            let _ = thread.join();
        }
        self.shutdown();
    }
}

impl Drop for CastReceiver {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.mdns_thread.take() {
            let _ = thread.join();
        }
        if let Some(thread) = self.server_thread.take() {
            let _ = thread.join();
        }
        self.shutdown();
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Interop tracing for the console probe. Off in normal builds, where the
/// launcher has no terminal to print to.
fn debug_enabled() -> bool {
    static DEBUG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *DEBUG.get_or_init(|| std::env::var_os("HORIZON_CAST_DEBUG").is_some())
}

// ------------------------------------------------------------------ mDNS

/// Advertises this device as a Chromecast via mDNS.
///
/// The instance name is the device id rather than the friendly name: Chrome
/// treats the instance as a DNS-SD identity, keys its sink cache on it, and
/// shows `fn` from TXT as the label. Two receivers that share a friendly name
/// would otherwise fight over the same instance name on a LAN.
fn mdns_responder(config: CastReceiverConfig, stop: Arc<AtomicBool>) {
    let socket = match multicast_socket(MDNS_PORT, MDNS_GROUP, 255) {
        Ok(s) => s,
        Err(_) => return,
    };

    let device_id = generate_device_id(&config.name);
    let instance = instance_name(&device_id);
    let host = host_name(&device_id);

    let mut announcements = 0;
    let mut next_announce = std::time::Instant::now();

    while !stop.load(Ordering::Relaxed) {
        let mut buffer = [0u8; 4096];
        socket
            .set_read_timeout(Some(Duration::from_millis(100)))
            .ok();

        while let Ok((length, from)) = socket.recv_from(&mut buffer) {
            if let Ok(questions) = parse_mdns_questions(&buffer[..length]) {
                if questions.is_empty() {
                    continue;
                }
                let matches = questions.iter().any(|q| {
                    let name = q.name.trim_end_matches('.').to_ascii_lowercase();
                    name == "_googlecast._tcp.local"
                        || name == instance.to_ascii_lowercase()
                        || name == host.to_ascii_lowercase()
                });
                if !matches {
                    continue;
                }
                let legacy = from.port() != MDNS_PORT;
                let response = build_mdns_response(&config, &device_id, &instance, &host, 120, legacy);
                let target = if legacy {
                    from
                } else {
                    SocketAddr::from((MDNS_GROUP, MDNS_PORT))
                };
                let _ = socket.send_to(&response, target);
            }
        }

        if announcements < 3 && std::time::Instant::now() >= next_announce {
            let response = build_mdns_response(&config, &device_id, &instance, &host, 120, false);
            let _ = socket.send_to(&response, SocketAddr::from((MDNS_GROUP, MDNS_PORT)));
            announcements += 1;
            next_announce = std::time::Instant::now() + Duration::from_secs(1);
        }
    }

    // Goodbye records (TTL 0) so the sink disappears from senders immediately.
    for _ in 0..3 {
        let response = build_mdns_response(&config, &device_id, &instance, &host, 0, false);
        let _ = socket.send_to(&response, SocketAddr::from((MDNS_GROUP, MDNS_PORT)));
        thread::sleep(Duration::from_millis(20));
    }
}

fn generate_device_id(name: &str) -> String {
    let mut id = 0xcbf2_9ce4_8422_2325u64;
    for byte in name.bytes() {
        id ^= byte as u64;
        id = id.wrapping_mul(0x0000_0100_0000_01b3);
    }
    // Mix in the machine name so two receivers with the default name on one LAN
    // still get distinct instance names.
    let host = std::env::var("COMPUTERNAME").unwrap_or_default();
    for byte in host.bytes() {
        id ^= byte as u64;
        id = id.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{:016x}", id)
}

fn instance_name(device_id: &str) -> String {
    format!("{device_id}._googlecast._tcp.local")
}

fn host_name(device_id: &str) -> String {
    format!("horizon-{device_id}.local")
}

fn build_mdns_response(
    config: &CastReceiverConfig,
    device_id: &str,
    instance: &str,
    host: &str,
    ttl: u32,
    legacy: bool,
) -> Vec<u8> {
    let ttl = if legacy { ttl.min(10) } else { ttl };
    let flush = if legacy { 0 } else { 0x8000 };

    let address = if config.address.is_unspecified() {
        crate::discovery::primary_interface()
    } else {
        config.address
    };

    let mut out = Vec::new();
    out.extend_from_slice(&0u16.to_be_bytes()); // id
    out.extend_from_slice(&0x8400u16.to_be_bytes()); // QR + AA
    out.extend_from_slice(&0u16.to_be_bytes()); // qdcount
    out.extend_from_slice(&4u16.to_be_bytes()); // ancount (PTR, SRV, TXT, A)
    out.extend_from_slice(&0u16.to_be_bytes()); // nscount
    out.extend_from_slice(&0u16.to_be_bytes()); // arcount

    // PTR: _googlecast._tcp.local -> service instance
    let mut rdata = Vec::new();
    write_mdns_name(&mut rdata, instance);
    push_mdns_record(&mut out, "_googlecast._tcp.local", 12, 1, ttl, &rdata);

    // SRV: service instance -> host:port
    let mut rdata = Vec::new();
    rdata.extend_from_slice(&0u16.to_be_bytes()); // priority
    rdata.extend_from_slice(&0u16.to_be_bytes()); // weight
    rdata.extend_from_slice(&config.port.to_be_bytes());
    write_mdns_name(&mut rdata, host);
    push_mdns_record(&mut out, instance, 33, 1 | flush, ttl, &rdata);

    // TXT: what Chrome reads to name and identify the sink. Field set and order
    // mirror a first-generation Chromecast closely enough that every sender we
    // care about parses it.
    let mut rdata = Vec::new();
    let setup_id = &device_id[device_id.len().saturating_sub(12)..];
    let txt_records = [
        format!("id={device_id}"),
        format!("cd={device_id}"),
        "rm=".to_string(),
        "ve=05".to_string(),
        format!("md={}", config.model),
        "ic=/setup/icon.png".to_string(),
        format!("fn={}", config.name),
        "ca=4101".to_string(),
        "st=0".to_string(),
        format!("bs=FA8FCA{setup_id}"),
        "nf=1".to_string(),
        "rs=".to_string(),
    ];
    for entry in txt_records {
        let bytes = entry.as_bytes();
        let length = bytes.len().min(255);
        rdata.push(length as u8);
        rdata.extend_from_slice(&bytes[..length]);
    }
    push_mdns_record(&mut out, instance, 16, 1 | flush, ttl, &rdata);

    // A: host -> address
    push_mdns_record(&mut out, host, 1, 1 | flush, ttl, &address.octets());

    out
}

fn push_mdns_record(out: &mut Vec<u8>, name: &str, kind: u16, class: u16, ttl: u32, rdata: &[u8]) {
    write_mdns_name(out, name);
    out.extend_from_slice(&kind.to_be_bytes());
    out.extend_from_slice(&class.to_be_bytes());
    out.extend_from_slice(&ttl.to_be_bytes());
    out.extend_from_slice(&(rdata.len() as u16).to_be_bytes());
    out.extend_from_slice(rdata);
}

fn write_mdns_name(out: &mut Vec<u8>, name: &str) {
    for label in name.trim_end_matches('.').split('.') {
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

#[derive(Debug, Clone)]
struct MdnsQuestion {
    name: String,
    #[allow(dead_code)]
    kind: u16,
    #[allow(dead_code)]
    class: u16,
}

fn parse_mdns_questions(packet: &[u8]) -> CastResult<Vec<MdnsQuestion>> {
    if packet.len() < 12 {
        return Err(CastError::Malformed("dns packet too short"));
    }
    let flags = u16::from_be_bytes([packet[2], packet[3]]);
    if flags & 0x8000 != 0 {
        return Ok(Vec::new()); // Response, not a query
    }
    let count = u16::from_be_bytes([packet[4], packet[5]]) as usize;
    let mut at = 12usize;
    let mut questions = Vec::with_capacity(count);
    for _ in 0..count {
        let name = read_mdns_name(packet, &mut at)?;
        if at + 4 > packet.len() {
            return Err(CastError::Malformed("dns question truncated"));
        }
        let kind = u16::from_be_bytes([packet[at], packet[at + 1]]);
        let class = u16::from_be_bytes([packet[at + 2], packet[at + 3]]);
        at += 4;
        questions.push(MdnsQuestion { name, kind, class });
    }
    Ok(questions)
}

fn read_mdns_name(packet: &[u8], at: &mut usize) -> CastResult<String> {
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

// ------------------------------------------------------------------ TLS server

/// Accepts CASTV2 connections over TLS.
fn tls_server(
    config: CastReceiverConfig,
    stop: Arc<AtomicBool>,
    state: Arc<Mutex<ReceiverState>>,
    certificate: Arc<ServerCertificate>,
    identity: Option<Arc<DeviceIdentity>>,
) {
    let addr = SocketAddr::from((config.address, config.port));
    let listener = match TcpListener::bind(addr) {
        Ok(listener) => listener,
        Err(_) => return,
    };
    if listener.set_nonblocking(true).is_err() {
        return;
    }

    while !stop.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((stream, peer)) => {
                // Winsock inherits the listener's non-blocking mode; SChannel
                // would then see WouldBlock before the ClientHello even lands.
                let _ = stream.set_nonblocking(false);
                let _ = stream.set_nodelay(true);
                let session_state = Arc::clone(&state);
                let session_certificate = Arc::clone(&certificate);
                let session_identity = identity.as_ref().map(Arc::clone);
                thread::Builder::new()
                    .name("cast-recv-session".to_string())
                    .spawn(move || {
                        match crate::cast::tls::TlsStream::accept(
                            stream,
                            Duration::from_secs(10),
                            &session_certificate,
                        ) {
                            Ok(tls) => {
                                if debug_enabled() {
                                    eprintln!("cast: TLS session from {peer}");
                                }
                                // Send a media status within a quarter second of
                                // a playback change instead of waiting for the
                                // next inbound message. The handshake keeps the
                                // long timeout; only the steady state shortens.
                                let _ = tls.set_read_timeout(STATUS_TICK);
                                let tls_der = session_certificate.der().to_vec();
                                match handle_castv2_connection(
                                    tls,
                                    peer,
                                    session_state,
                                    session_identity,
                                    tls_der,
                                ) {
                                    Ok(()) => {
                                        if debug_enabled() {
                                            eprintln!("cast: session with {peer} closed by peer");
                                        }
                                    }
                                    Err(error) => {
                                        if debug_enabled() {
                                            eprintln!("cast: session with {peer} failed: {error}");
                                        }
                                    }
                                }
                            }
                            Err(error) => {
                                if debug_enabled() {
                                    eprintln!("cast: TLS handshake from {peer} failed: {error}");
                                }
                            }
                        }
                    })
                    .ok();
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(50));
            }
            Err(_) => thread::sleep(Duration::from_millis(100)),
        }
    }
}

/// Handles one CASTV2 connection from a sender.
fn handle_castv2_connection<S: Read + Write>(
    stream: S,
    peer: SocketAddr,
    state: Arc<Mutex<ReceiverState>>,
    identity: Option<Arc<DeviceIdentity>>,
    tls_certificate: Vec<u8>,
) -> CastResult<()> {
    CastSession {
        stream,
        reader: FrameReader::new(),
        peer,
        state,
        identity,
        tls_certificate,
        media_sender: None,
        last_status: None,
        closed: false,
    }
    .run()
}

/// One CASTV2 session with a sender.
struct CastSession<S: Read + Write> {
    stream: S,
    reader: FrameReader,
    #[allow(dead_code)]
    peer: SocketAddr,
    state: Arc<Mutex<ReceiverState>>,
    identity: Option<Arc<DeviceIdentity>>,
    /// DER of the TLS certificate presented to this peer, which the device-auth
    /// signature must cover.
    tls_certificate: Vec<u8>,
    /// The sender that last addressed the media transport, and so receives
    /// unsolicited `MEDIA_STATUS`.
    media_sender: Option<String>,
    /// `(media session, phase, position in ms)` of the last status this session
    /// sent on its own. Used to decide whether the player moved.
    last_status: Option<(i64, Phase, i64)>,
    closed: bool,
}

impl<S: Read + Write> CastSession<S> {
    fn run(&mut self) -> CastResult<()> {
        loop {
            match self.reader.poll(&mut self.stream)? {
                FramePoll::Ready(message) => {
                    self.handle_message(&message)?;
                    if self.closed {
                        return Ok(());
                    }
                }
                // Nothing arrived within the tick: a chance to report playback
                // that started, paused, moved or finished since the last push.
                FramePoll::Waiting => self.push_media_status_if_changed()?,
                FramePoll::Closed => return Ok(()),
            }
        }
    }

    /// Pushes a `MEDIA_STATUS` when the player moved since the last push.
    ///
    /// Once media is loaded a sender stops asking: progress, a seek and the end
    /// of the stream are all announced by the receiver. Comparing the last
    /// `(session, phase, position)` keeps a paused stream from producing four
    /// identical messages a second while still reporting a sub-second seek.
    fn push_media_status_if_changed(&mut self) -> CastResult<()> {
        let Some(sender) = self.media_sender.clone() else {
            return Ok(());
        };
        let status = {
            let state = lock(&self.state);
            if state.media_session_id == 0 || state.player.is_none() {
                return Ok(());
            }
            let snapshot = state.player.as_ref().expect("player exists").state();
            let key = (
                state.media_session_id,
                snapshot.phase,
                (snapshot.position * 1000.0).round() as i64,
            );
            let changed = match self.last_status {
                None => true,
                Some(last) => {
                    last.0 != key.0
                        || last.1 != key.1
                        || (last.2 - key.2).abs() >= STATUS_SEEK_MS
                }
            };
            if !changed {
                return Ok(());
            }
            self.last_status = Some(key);
            media_status(&state, 0, None)
        };
        self.respond(CAST_TRANSPORT_ID, &sender, NS_MEDIA, status)
    }

    fn handle_message(&mut self, message: &CastMessage) -> CastResult<()> {
        if message.payload_type != 0 {
            if message.namespace == NS_DEVICE_AUTH {
                self.handle_device_auth(message)?;
            }
            return Ok(());
        }
        let Some(text) = message.payload_utf8.as_deref() else {
            return Ok(());
        };
        if debug_enabled() {
            eprintln!(
                "cast <- [{}] {} -> {}: {text}",
                message.namespace, message.source_id, message.destination_id
            );
        }
        let payload: Value =
            serde_json::from_str(text).map_err(|_| CastError::Malformed("invalid JSON"))?;

        let msg_type = payload
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("");

        match message.namespace.as_str() {
            NS_CONNECTION => self.handle_connection(msg_type),
            NS_HEARTBEAT => self.handle_heartbeat(msg_type, message),
            NS_RECEIVER => self.handle_receiver(msg_type, &payload, message),
            NS_MEDIA => self.handle_media(msg_type, &payload, message),
            _ => Ok(()),
        }
    }

    /// Answers the sender's device-auth challenge, the mandatory first exchange
    /// on a Chromium Cast socket.
    fn handle_device_auth(&mut self, message: &CastMessage) -> CastResult<()> {
        let Some(challenge) = message.payload_binary.as_deref() else {
            return Ok(());
        };
        let Some(identity) = self.identity.as_deref() else {
            return Ok(());
        };
        if debug_enabled() {
            let hex: String = challenge.iter().map(|byte| format!("{byte:02x}")).collect();
            eprintln!(
                "cast <- [deviceauth] {} -> {}: {} bytes {hex}",
                message.source_id,
                message.destination_id,
                challenge.len()
            );
        }
        let reply = device_auth_reply(challenge, identity, &self.tls_certificate)?;
        let response = CastMessage::binary(
            RECEIVER_ID,
            &message.source_id,
            NS_DEVICE_AUTH,
            reply,
        );
        crate::cast::write_frame(&mut self.stream, &response)?;
        if debug_enabled() {
            eprintln!("cast -> [deviceauth] signed response sent");
        }
        Ok(())
    }

    fn respond(
        &mut self,
        source: &str,
        destination: &str,
        namespace: &str,
        payload: Value,
    ) -> CastResult<()> {
        if debug_enabled() {
            eprintln!("cast -> [{namespace}] {source} -> {destination}: {payload}");
        }
        let message = CastMessage::text(source, destination, namespace, &payload.to_string());
        crate::cast::write_frame(&mut self.stream, &message)
    }

    fn handle_connection(&mut self, msg_type: &str) -> CastResult<()> {
        match msg_type {
            "CLOSE" => self.closed = true,
            _ => {}
        }
        Ok(())
    }

    fn handle_heartbeat(&mut self, msg_type: &str, message: &CastMessage) -> CastResult<()> {
        if msg_type == "PING" {
            self.respond(
                RECEIVER_ID,
                &message.source_id,
                NS_HEARTBEAT,
                json!({"type": "PONG"}),
            )?;
        }
        Ok(())
    }

    fn handle_receiver(
        &mut self,
        msg_type: &str,
        payload: &Value,
        message: &CastMessage,
    ) -> CastResult<()> {
        let request_id = payload
            .get("requestId")
            .and_then(Value::as_u64)
            .unwrap_or(0);

        match msg_type {
            "GET_STATUS" => {
                let status = {
                    let state = lock(&self.state);
                    receiver_status(&state, request_id)
                };
                self.respond(
                    RECEIVER_ID,
                    &message.source_id,
                    NS_RECEIVER,
                    status,
                )
            }
            "GET_APP_AVAILABILITY" => {
                let requested: Vec<String> = payload
                    .get("appId")
                    .and_then(Value::as_array)
                    .map(|apps| {
                        apps.iter()
                            .filter_map(Value::as_str)
                            .map(str::to_string)
                            .collect()
                    })
                    .unwrap_or_default();
                let mut availability = serde_json::Map::new();
                for app in requested {
                    let state = if app == DEFAULT_MEDIA_RECEIVER {
                        "APP_AVAILABLE"
                    } else {
                        // Tab mirroring and third-party receivers need protocol
                        // work this device does not do; refusing here keeps the
                        // sender from offering a route that would hang.
                        "APP_UNAVAILABLE"
                    };
                    availability.insert(app, Value::String(state.to_string()));
                }
                self.respond(
                    RECEIVER_ID,
                    &message.source_id,
                    NS_RECEIVER,
                    json!({
                        "type": "GET_APP_AVAILABILITY",
                        "requestId": request_id,
                        "availability": availability,
                    }),
                )
            }
            "LAUNCH" => {
                let app_id = payload
                    .get("appId")
                    .and_then(Value::as_str)
                    .unwrap_or(DEFAULT_MEDIA_RECEIVER);
                if app_id != DEFAULT_MEDIA_RECEIVER {
                    self.respond(
                        RECEIVER_ID,
                        &message.source_id,
                        NS_RECEIVER,
                        json!({
                            "type": "LAUNCH_ERROR",
                            "requestId": request_id,
                            "reason": "NOT_SUPPORTED",
                        }),
                    )?;
                    return Ok(());
                }
                let status = {
                    let mut state = lock(&self.state);
                    if state.app_id.is_none() {
                        state.session_id = format!("session-{request_id}");
                    }
                    state.app_id = Some(app_id.to_string());
                    receiver_status(&state, request_id)
                };
                self.respond(RECEIVER_ID, &message.source_id, NS_RECEIVER, status)
            }
            "STOP" | "STANDBY" => {
                let status = {
                    let mut state = lock(&self.state);
                    if let Some(player) = state.player.as_ref() {
                        let _ = player.stop();
                    }
                    state.app_id = None;
                    state.media_session_id = 0;
                    state.content_id.clear();
                    state.content_type.clear();
                    state.declared_duration = 0.0;
                    receiver_status(&state, request_id)
                };
                self.respond(RECEIVER_ID, &message.source_id, NS_RECEIVER, status)
            }
            "SET_VOLUME" => {
                let status = {
                    let mut state = lock(&self.state);
                    if let Some(volume) = payload.get("volume") {
                        if let Some(level) = volume.get("level").and_then(Value::as_f64) {
                            state.volume = level.clamp(0.0, 1.0);
                        }
                        if let Some(muted) = volume.get("muted").and_then(Value::as_bool) {
                            state.muted = muted;
                        }
                    }
                    if let Some(player) = state.player.as_ref() {
                        let _ = player.set_volume(state.volume);
                        let _ = player.set_mute(state.muted);
                    }
                    receiver_status(&state, request_id)
                };
                self.respond(RECEIVER_ID, &message.source_id, NS_RECEIVER, status)
            }
            _ => Ok(()),
        }
    }

    fn handle_media(
        &mut self,
        msg_type: &str,
        payload: &Value,
        message: &CastMessage,
    ) -> CastResult<()> {
        // Whoever last spoke to the media transport gets the unsolicited
        // statuses; a receiver does not broadcast to senders it never heard.
        self.media_sender = Some(message.source_id.clone());
        let request_id = payload
            .get("requestId")
            .and_then(Value::as_u64)
            .unwrap_or(0);

        match msg_type {
            "LOAD" => {
                let media = payload.get("media").cloned().unwrap_or(json!({}));
                let content_id = media
                    .get("contentId")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let content_type = media
                    .get("contentType")
                    .and_then(Value::as_str)
                    .unwrap_or("video/mp4")
                    .to_string();
                if content_id.is_empty() {
                    self.respond(
                        CAST_TRANSPORT_ID,
                        &message.source_id,
                        NS_MEDIA,
                        json!({
                            "type": "LOAD_FAILED",
                            "requestId": request_id,
                            "detailedErrorCode": 103,
                        }),
                    )?;
                    return Ok(());
                }

                let show_video = content_type.starts_with("video/")
                    || content_type.contains("mpegurl");
                let autoplay = payload
                    .get("autoplay")
                    .and_then(Value::as_bool)
                    .or_else(|| media.get("autoplay").and_then(Value::as_bool))
                    .unwrap_or(true);
                let start_time = payload
                    .get("currentTime")
                    .and_then(Value::as_f64)
                    .or_else(|| media.get("currentTime").and_then(Value::as_f64))
                    .unwrap_or(0.0);

                let (mut status, session_id) = {
                    let mut state = lock(&self.state);
                    state.content_id = content_id.clone();
                    state.content_type = content_type.clone();
                    state.declared_duration = media
                        .get("duration")
                        .and_then(Value::as_f64)
                        .unwrap_or(0.0);
                    state.media_session_id += 1;
                    if state.app_id.is_none() {
                        state.app_id = Some(DEFAULT_MEDIA_RECEIVER.to_string());
                    }
                    if state.session_id.is_empty() {
                        state.session_id = format!("session-{request_id}");
                    }

                    let loaded = ensure_player(&mut state, show_video)
                        .and_then(|player| player.load(&content_id));
                    match loaded {
                        Ok(()) => {
                            let player = state.player.as_ref().expect("player exists");
                            if start_time > 0.0 {
                                let _ = player.seek(start_time);
                            }
                            if autoplay {
                                let _ = player.play();
                            } else {
                                // Without this the engine sits in `Stopped`,
                                // which a sender reads as a cancelled stream.
                                let _ = player.pause();
                            }
                        }
                        Err(_) => {
                            state.content_id.clear();
                            let status = media_status(&state, request_id, Some("ERROR"));
                            drop(state);
                            self.respond(
                                CAST_TRANSPORT_ID,
                                &message.source_id,
                                NS_MEDIA,
                                status,
                            )?;
                            return Ok(());
                        }
                    }
                    (
                        media_status(&state, request_id, None),
                        state.media_session_id,
                    )
                };
                // The engine has not opened the source yet, so its snapshot is
                // still the previous stream. Answer with what a sender must see
                // right after LOAD — buffering, or paused for autoplay=false —
                // and never with the stale IDLE of the media being replaced.
                if let Some(entry) = status["status"].get_mut(0) {
                    entry["playerState"] = json!(if autoplay { "BUFFERING" } else { "PAUSED" });
                    entry["currentTime"] = json!(start_time);
                    if let Some(fields) = entry.as_object_mut() {
                        fields.remove("idleReason");
                    }
                }
                self.last_status = Some((
                    session_id,
                    if autoplay { Phase::Loading } else { Phase::Paused },
                    (start_time * 1000.0).round() as i64,
                ));
                self.respond(CAST_TRANSPORT_ID, &message.source_id, NS_MEDIA, status)
            }
            "PLAY" | "PAUSE" | "SEEK" | "STOP" => {
                let (status, session_id, phase, position) = {
                    let state = lock(&self.state);
                    let target = payload.get("currentTime").and_then(Value::as_f64);
                    if let Some(player) = state.player.as_ref() {
                        match msg_type {
                            "PLAY" => {
                                let _ = player.play();
                            }
                            "PAUSE" => {
                                let _ = player.pause();
                            }
                            "SEEK" => {
                                let _ = player.seek(target.unwrap_or(0.0));
                            }
                            _ => {
                                let _ = player.stop();
                            }
                        }
                    }
                    let snapshot = state
                        .player
                        .as_ref()
                        .map(|player| player.state())
                        .unwrap_or_default();
                    // The command is queued, so the snapshot predates it. The
                    // answer carries the state the sender asked for; if the
                    // engine disagrees the next pushed status corrects it.
                    let phase = match msg_type {
                        "PLAY" => Phase::Playing,
                        "PAUSE" => Phase::Paused,
                        "STOP" => Phase::Stopped,
                        _ => snapshot.phase,
                    };
                    let position = if msg_type == "SEEK" {
                        target.unwrap_or(snapshot.position)
                    } else {
                        snapshot.position
                    };
                    let reason = if msg_type == "STOP" {
                        Some("CANCELLED")
                    } else {
                        None
                    };
                    let mut status = media_status(&state, request_id, reason);
                    if let Some(entry) = status["status"].get_mut(0) {
                        entry["playerState"] = json!(cast_player_state(phase));
                        entry["currentTime"] = json!(position);
                    }
                    (status, state.media_session_id, phase, position)
                };
                self.last_status = Some((
                    session_id,
                    phase,
                    (position * 1000.0).round() as i64,
                ));
                self.respond(CAST_TRANSPORT_ID, &message.source_id, NS_MEDIA, status)
            }
            "GET_STATUS" => {
                let status = {
                    let state = lock(&self.state);
                    if state.player.is_none() || state.media_session_id == 0 {
                        json!({
                            "type": "MEDIA_STATUS",
                            "requestId": request_id,
                            "status": [],
                        })
                    } else {
                        media_status(&state, request_id, None)
                    }
                };
                self.respond(CAST_TRANSPORT_ID, &message.source_id, NS_MEDIA, status)
            }
            _ => Ok(()),
        }
    }
}

fn receiver_status(state: &ReceiverState, request_id: u64) -> Value {
    let applications: Vec<Value> = match &state.app_id {
        Some(app_id) => vec![json!({
            "appId": app_id,
            "sessionId": state.session_id,
            "transportId": CAST_TRANSPORT_ID,
            "displayName": "HORIZON Cast",
            "statusText": "",
            "isIdleScreen": false,
            "namespaces": [{"name": NS_MEDIA}],
        })],
        None => Vec::new(),
    };
    json!({
        "type": "RECEIVER_STATUS",
        "requestId": request_id,
        "status": {
            "volume": {
                "level": state.volume,
                "muted": state.muted,
            },
            "applications": applications,
        },
    })
}

fn media_status(state: &ReceiverState, request_id: u64, idle_reason: Option<&str>) -> Value {
    let snapshot = state
        .player
        .as_ref()
        .map(|player| player.state())
        .unwrap_or_default();
    let duration = if snapshot.duration > 0.0 {
        snapshot.duration
    } else {
        state.declared_duration
    };
    let mut entry = json!({
        "mediaSessionId": state.media_session_id,
        "playbackRate": 1,
        "playerState": cast_player_state(snapshot.phase),
        "currentTime": snapshot.position,
        "supportedMediaCommands": MEDIA_COMMANDS,
        "volume": {
            "level": state.volume,
            "muted": state.muted,
        },
        "media": {
            "contentId": state.content_id,
            "contentType": state.content_type,
            "streamType": "BUFFERED",
            "duration": duration,
        },
    });
    if let Some(reason) = idle_reason.or_else(|| idle_reason_for(snapshot.phase)) {
        entry["idleReason"] = json!(reason);
    }
    json!({
        "type": "MEDIA_STATUS",
        "requestId": request_id,
        "status": [entry],
    })
}

fn cast_player_state(phase: Phase) -> &'static str {
    match phase {
        Phase::Playing => "PLAYING",
        Phase::Paused => "PAUSED",
        Phase::Loading => "BUFFERING",
        Phase::Idle | Phase::Stopped | Phase::Ended | Phase::Error => "IDLE",
    }
}

fn idle_reason_for(phase: Phase) -> Option<&'static str> {
    match phase {
        Phase::Ended => Some("FINISHED"),
        Phase::Error => Some("ERROR"),
        Phase::Stopped => Some("CANCELLED"),
        _ => None,
    }
}

// ------------------------------------------------------------------ tests

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_id_is_stable_for_same_name() {
        let id1 = generate_device_id("HORIZON Meeting");
        let id2 = generate_device_id("HORIZON Meeting");
        assert_eq!(id1, id2);
        assert_eq!(id1.len(), 16);
    }

    #[test]
    fn device_id_differs_for_different_names() {
        let id1 = generate_device_id("HORIZON Meeting");
        let id2 = generate_device_id("Other Device");
        assert_ne!(id1, id2);
    }

    #[test]
    fn mdns_response_has_four_records() {
        let config = CastReceiverConfig::default();
        let device_id = generate_device_id(&config.name);
        let instance = instance_name(&device_id);
        let host = host_name(&device_id);
        let response = build_mdns_response(&config, &device_id, &instance, &host, 120, false);

        let ancount = u16::from_be_bytes([response[6], response[7]]);
        assert_eq!(ancount, 4);
    }

    #[test]
    fn mdns_txt_records_contain_required_fields() {
        let config = CastReceiverConfig {
            name: "Test Device".to_string(),
            model: "TestModel".to_string(),
            address: Ipv4Addr::new(192, 168, 1, 100),
            port: 8009,
        };
        let device_id = generate_device_id(&config.name);
        let instance = instance_name(&device_id);
        let host = host_name(&device_id);
        let response = build_mdns_response(&config, &device_id, &instance, &host, 120, false);

        let response_str = String::from_utf8_lossy(&response);
        assert!(response_str.contains("fn=Test Device"));
        assert!(response_str.contains("md=TestModel"));
        assert!(response_str.contains("ve=05"));
        assert!(response_str.contains(&format!("id={device_id}")));
        assert!(response_str.contains("ca=4101"));
        assert!(response_str.contains("googlecast"));
        assert!(response_str.contains("_tcp"));
    }

    #[test]
    fn mdns_query_parsing_handles_compression() {
        let mut packet = vec![0u8; 12];
        packet[5] = 1; // qdcount = 1
        write_mdns_name(&mut packet, "_googlecast._tcp.local");
        packet.extend_from_slice(&12u16.to_be_bytes()); // TYPE_PTR
        packet.extend_from_slice(&1u16.to_be_bytes()); // CLASS_IN

        let questions = parse_mdns_questions(&packet).unwrap();
        assert_eq!(questions.len(), 1);
        assert_eq!(questions[0].name, "_googlecast._tcp.local");
    }

    #[test]
    fn sha1_matches_the_standard_vectors() {
        assert_eq!(
            sha1(b""),
            [
                0xda, 0x39, 0xa3, 0xee, 0x5e, 0x6b, 0x4b, 0x0d, 0x32, 0x55, 0xbf, 0xef, 0x95,
                0x60, 0x18, 0x90, 0xaf, 0xd8, 0x07, 0x09
            ]
        );
        assert_eq!(
            sha1(b"abc"),
            [
                0xa9, 0x99, 0x3e, 0x36, 0x47, 0x06, 0x81, 0x6a, 0xba, 0x3e, 0x25, 0x71, 0x78,
                0x50, 0xc2, 0x6c, 0x9c, 0xd0, 0xd8, 0x9d
            ]
        );
    }

    #[test]
    fn device_auth_hashes_follow_the_challenge() {
        assert_eq!(HashAlgorithm::from_field(None), HashAlgorithm::Sha1);
        assert_eq!(HashAlgorithm::from_field(Some(0)), HashAlgorithm::Sha1);
        assert_eq!(HashAlgorithm::from_field(Some(1)), HashAlgorithm::Sha256);
    }

    #[test]
    fn idle_reason_is_only_for_idle_phases() {
        assert_eq!(idle_reason_for(Phase::Playing), None);
        assert_eq!(idle_reason_for(Phase::Paused), None);
        assert_eq!(idle_reason_for(Phase::Ended), Some("FINISHED"));
        assert_eq!(idle_reason_for(Phase::Error), Some("ERROR"));
        assert_eq!(cast_player_state(Phase::Loading), "BUFFERING");
        assert_eq!(cast_player_state(Phase::Playing), "PLAYING");
        assert_eq!(cast_player_state(Phase::Stopped), "IDLE");
    }
}
