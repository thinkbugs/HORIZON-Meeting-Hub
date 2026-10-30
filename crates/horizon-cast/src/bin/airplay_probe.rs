//! Advertise an AirPlay speaker and play what a sender streams to it, or run a
//! self test that drives the receiver with a scripted sender.
//!
//! Usage:
//!   airplay-probe [--name=NAME] [--port=5000] [--out=FILE] [--silent]
//!   airplay-probe --selftest [--out=FILE]
//!   airplay-probe --selftest-mirror
//!
//! Without `--selftest` this is the receiver: it publishes `_raop._tcp` on the
//! LAN and serves one RTSP session after another, playing to the sound card
//! unless `--out` names a WAV file.
//!
//! `--selftest` binds a loopback RTSP listener and speaks to it as a sender:
//! challenge, encrypted ANNOUNCE, SETUP, RECORD, twenty RTP packets with one
//! deliberately lost and retransmitted after a `0xd5` request, volume parameter,
//! TEARDOWN. It checks the replies, the packet counters and the PCM in the WAV
//! file. No Apple device is involved; this proves the bytes this receiver
//! decides, not interop.
//!
//! `--selftest-mirror` drives the screen path the same way: `/info`, pair-setup,
//! pair-verify, both FairPlay messages, the three binary-plist SETUPs, RECORD,
//! an AES-CTR H.264 key frame over the mirror connection, TEARDOWN. It checks
//! that the frame the sink receives is the frame the sender encrypted.

use std::io::{BufReader, Write};
use std::net::{Ipv4Addr, TcpListener, TcpStream, UdpSocket};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use horizon_cast::airplay::crypto::{base64_encode, AesCtr, AesStream, RsaIdentity};
use horizon_cast::airplay::fairplay;
use horizon_cast::airplay::mirror::{stream_keys, VideoFrame};
use horizon_cast::airplay::pairing::Identity;
use horizon_cast::airplay::plist::Plist;
use horizon_cast::airplay::rtsp::{parse_transport, Request, Response};
use horizon_cast::airplay::session::{Config, Receiver, Session, Stats};

fn main() {
    if let Err(error) = run() {
        eprintln!("airplay-probe: {error}");
        std::process::exit(1);
    }
}

struct Args {
    name: String,
    port: u16,
    out: Option<PathBuf>,
    silent: bool,
    selftest: bool,
    selftest_mirror: bool,
}

impl Args {
    fn parse() -> Self {
        let mut args = Self {
            name: "HORIZON Meeting Hub".to_string(),
            port: 5000,
            out: None,
            silent: false,
            selftest: false,
            selftest_mirror: false,
        };
        for arg in std::env::args().skip(1) {
            if let Some(value) = arg.strip_prefix("--name=") {
                args.name = value.to_string();
            } else if let Some(value) = arg.strip_prefix("--port=") {
                args.port = value.parse().unwrap_or(args.port);
            } else if let Some(value) = arg.strip_prefix("--out=") {
                args.out = Some(PathBuf::from(value));
            } else if arg == "--silent" {
                args.silent = true;
            } else if arg == "--selftest" {
                args.selftest = true;
            } else if arg == "--selftest-mirror" {
                args.selftest_mirror = true;
            } else if arg == "--help" || arg == "-h" {
                println!(
                    "usage: airplay-probe [--name=NAME] [--port=5000] [--out=FILE] [--silent]\n\
                     \x20      airplay-probe --selftest [--out=FILE]\n\
                     \x20      airplay-probe --selftest-mirror"
                );
                std::process::exit(0);
            }
        }
        args
    }
}

fn run() -> Result<(), String> {
    let args = Args::parse();
    if args.selftest_mirror {
        mirror_selftest(&args)
    } else if args.selftest {
        selftest(&args)
    } else {
        serve(&args)
    }
}

fn serve(args: &Args) -> Result<(), String> {
    let config = Config {
        name: args.name.clone(),
        port: args.port,
        output: args.out.clone(),
        silent: args.silent,
        ..Config::default()
    };
    let receiver = Receiver::bind(config).map_err(|e| e.to_string())?;
    let advert = receiver.advert();
    println!(
        "advertising \"{}\" on {}:{} (A record {}, TXT codec L16)",
        advert.service(),
        advert.host(),
        advert.port,
        advert.address
    );
    println!("waiting for a sender; the firewall must allow inbound TCP {} and UDP", advert.port);
    let stats = Arc::new(Stats::default());
    loop {
        match receiver.accept(Arc::clone(&stats)) {
            Ok(report) => print_report(&report, &stats),
            Err(error) => eprintln!("session ended: {error}"),
        }
    }
}

fn print_report(report: &horizon_cast::airplay::session::SessionReport, stats: &Arc<Stats>) {
    let snapshot = stats.snapshot();
    println!(
        "\n{} from {:?}: {} packets, {} frames ({:.2}s of audio)",
        report.codec,
        report.peer,
        report.packets,
        report.frames,
        report.frames as f64 / 44_100.0
    );
    println!(
        "  encrypted: {}  missing: {}  resend requests: {}  resends heard: {}  sync: {}",
        report.encrypted,
        report.missing,
        report.resend_requests,
        snapshot.resent_packets,
        report.sync_packets
    );
    println!("  volume: {:.1} dB", report.volume_db);
    if report.mirror_frames > 0 {
        println!(
            "  mirror: {} frames ({} key) {} KB, {}x{}",
            report.mirror_frames,
            report.mirror_keyframes,
            report.mirror_bytes / 1024,
            report.mirror_width,
            report.mirror_height
        );
    }
    if let Some(path) = &report.output {
        println!("  wrote {}", path.display());
    }
}

// ----------------------------------------------------------------- selftest

fn selftest(args: &Args) -> Result<(), String> {
    let started = Instant::now();
    let wav_path = args.out.clone().unwrap_or_else(|| {
        std::env::temp_dir().join(format!("horizon-airplay-selftest-{}.wav", std::process::id()))
    });
    if wav_path.exists() {
        let _ = std::fs::remove_file(&wav_path);
    }

    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).map_err(|e| e.to_string())?;
    let port = listener.local_addr().map_err(|e| e.to_string())?.port();
    println!("selftest: receiver listening on 127.0.0.1:{port}");

    let stats = Arc::new(Stats::default());
    let thread_stats = Arc::clone(&stats);
    let config = Config {
        name: "HORIZON Selftest".to_string(),
        port,
        output: Some(wav_path.clone()),
        silent: false,
        ..Config::default()
    };
    let receiver = thread::spawn(move || {
        let (stream, peer) = listener.accept().map_err(|e| e.to_string())?;
        let mut session = Session::new(
            stream,
            peer,
            Ipv4Addr::LOCALHOST,
            [0x02, 0x00, 0x00, 0x00, 0x00, 0x01],
            config,
            Identity::from_seed([0x33; 32]),
        )
        .map_err(|e| e.to_string())?;
        session.run(thread_stats).map_err(|e| e.to_string())
    });

    let mut sender = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).map_err(|e| e.to_string())?;
    sender
        .set_read_timeout(Some(Duration::from_secs(5)))
        .map_err(|e| e.to_string())?;
    let mut reader = BufReader::new(sender.try_clone().map_err(|e| e.to_string())?);

    // 1. OPTIONS with a challenge, the sender's first question.
    let mut options = Request::new("OPTIONS", "*");
    options.set_header("CSeq", "1");
    options.set_header("Apple-Challenge", "AAECAwQFBgcICQoLDA0ODw==");
    options.write_to(&mut sender).map_err(|e| e.to_string())?;
    let response = read_response(&mut reader)?;
    if response.status != 200 {
        return Err(format!("OPTIONS answered {}", response.status));
    }
    let answer = response
        .header("Apple-Response")
        .ok_or("OPTIONS carried no Apple-Response")?;
    println!(
        "{:>6.0}ms  OPTIONS 200, Apple-Response {} chars, Public={}",
        started.elapsed().as_secs_f64() * 1000.0,
        answer.len(),
        response.header("Public").unwrap_or("-")
    );

    // 2. ANNOUNCE with the same key material a real sender uses.
    let key: Vec<u8> = (0u8..16).collect();
    let iv: Vec<u8> = (16u8..32).collect();
    let identity = RsaIdentity::raop().map_err(|e| e.to_string())?;
    let wrapped = identity.wrap_aes_key(&key).map_err(|e| e.to_string())?;
    let sdp = format!(
        "v=0\r\no=iTunes 3413821434 0 IN IP4 127.0.0.1\r\ns=iTunes\r\n\
         c=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 0 RTP/AVP 96\r\n\
         a=rtpmap:96 L16/44100/2\r\na=rsaaeskey:{}\r\na=aesiv:{}\r\n",
        base64_encode(&wrapped),
        base64_encode(&iv)
    );
    let mut announce = Request::new("ANNOUNCE", "rtsp://127.0.0.1/1");
    announce.set_header("CSeq", "2");
    announce.set_header("Content-Type", "application/sdp");
    announce.body = sdp.into_bytes();
    announce.write_to(&mut sender).map_err(|e| e.to_string())?;
    let response = read_response(&mut reader)?;
    println!(
        "{:>6.0}ms  ANNOUNCE 200 (rsaaeskey {} bytes wrapped, L16)",
        started.elapsed().as_secs_f64() * 1000.0,
        wrapped.len()
    );
    if response.status != 200 {
        return Err(format!("ANNOUNCE answered {}", response.status));
    }

    // 3. SETUP names the sender's control and timing ports.
    let sender_control = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).map_err(|e| e.to_string())?;
    let sender_timing = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).map_err(|e| e.to_string())?;
    sender_control
        .set_read_timeout(Some(Duration::from_millis(100)))
        .map_err(|e| e.to_string())?;
    let mut setup = Request::new("SETUP", "rtsp://127.0.0.1/1");
    setup.set_header("CSeq", "3");
    setup.set_header("DACP-ID", "0123456789ABCDEF");
    setup.set_header(
        "Transport",
        format!(
            "RTP/AVP/UDP;unicast;interleaved=0-1;mode=record;\
             control_port={};timing_port={}",
            sender_control.local_addr().map_err(|e| e.to_string())?.port(),
            sender_timing.local_addr().map_err(|e| e.to_string())?.port()
        ),
    );
    setup.write_to(&mut sender).map_err(|e| e.to_string())?;
    let response = read_response(&mut reader)?;
    let transport = parse_transport(response.header("Transport").ok_or("no Transport")?);
    let audio_port = response
        .header("Transport")
        .and_then(|value| {
            value
                .split(';')
                .find_map(|part| part.trim().strip_prefix("server_port="))
        })
        .and_then(|value| value.parse::<u16>().ok())
        .ok_or("no server_port")?;
    println!(
        "{:>6.0}ms  SETUP 200: server audio {}, control {:?}, timing {:?}, interleaved {:?}",
        started.elapsed().as_secs_f64() * 1000.0,
        audio_port,
        transport
            .control_port
            .map(|port| port.to_string())
            .unwrap_or_else(|| "-".to_string()),
        transport
            .timing_port
            .map(|port| port.to_string())
            .unwrap_or_else(|| "-".to_string()),
        transport.interleaved
    );

    // 4. RECORD starts the stream. A lost packet near the end is resent after
    //    the receiver asks with 0xd5.
    let total = 20u16;
    let base_seq = 1000u16;
    let lost_seq = base_seq + total - 2;
    let audio = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).map_err(|e| e.to_string())?;
    let resent = spawn_resend_responder(
        sender_control.try_clone().map_err(|e| e.to_string())?,
        audio.try_clone().map_err(|e| e.to_string())?,
        (Ipv4Addr::LOCALHOST, audio_port),
        key.clone(),
        iv.clone(),
        base_seq,
        total,
        lost_seq,
    );

    let mut record = Request::new("RECORD", "rtsp://127.0.0.1/1");
    record.set_header("CSeq", "4");
    record.set_header("RTP-Info", format!("seq={base_seq};rtptime=100000"));
    record.write_to(&mut sender).map_err(|e| e.to_string())?;
    let response = read_response(&mut reader)?;
    println!(
        "{:>6.0}ms  RECORD 200, Audio-Latency {}",
        started.elapsed().as_secs_f64() * 1000.0,
        response.header("Audio-Latency").unwrap_or("-")
    );

    let aes = AesStream::new(&key, &iv).map_err(|e| e.to_string())?;
    let mut sent = 0;
    for offset in 0..total {
        let seq = base_seq + offset;
        if seq == lost_seq {
            continue;
        }
        let packet = build_audio_packet(&aes, seq, offset, false)
            .map_err(|e| format!("encrypting packet {seq}: {e}"))?;
        audio
            .send_to(&packet, (Ipv4Addr::LOCALHOST, audio_port))
            .map_err(|e| e.to_string())?;
        sent += 1;
        thread::sleep(Duration::from_millis(3));
    }
    println!(
        "{:>6.0}ms  sent {sent} of {total} packets, seq {lost_seq} withheld",
        started.elapsed().as_secs_f64() * 1000.0
    );

    // The receiver notices the hole on its next loop and requests it after its
    // 250 ms resend timer; wait long enough for request and answer.
    thread::sleep(Duration::from_millis(600));
    resent
        .store(true, Ordering::Relaxed);

    // 5. Volume readback on the live stream.
    let mut parameter = Request::new("SET_PARAMETER", "rtsp://127.0.0.1/1");
    parameter.set_header("CSeq", "5");
    parameter.set_header("Content-Type", "text/parameters");
    parameter.body = b"volume: 0.0\r\n".to_vec();
    parameter.write_to(&mut sender).map_err(|e| e.to_string())?;
    read_response(&mut reader)?;
    let mut get = Request::new("GET_PARAMETER", "rtsp://127.0.0.1/1");
    get.set_header("CSeq", "6");
    get.set_header("Content-Type", "text/parameters");
    get.body = b"volume\r\n".to_vec();
    get.write_to(&mut sender).map_err(|e| e.to_string())?;
    let response = read_response(&mut reader)?;
    println!(
        "{:>6.0}ms  SET/GET_PARAMETER volume: {:?}",
        started.elapsed().as_secs_f64() * 1000.0,
        String::from_utf8_lossy(&response.body).trim()
    );

    // 6. TEARDOWN.
    let mut teardown = Request::new("TEARDOWN", "rtsp://127.0.0.1/1");
    teardown.set_header("CSeq", "7");
    teardown.write_to(&mut sender).map_err(|e| e.to_string())?;
    let response = read_response(&mut reader)?;
    println!(
        "{:>6.0}ms  TEARDOWN {} Connection: {:?}",
        started.elapsed().as_secs_f64() * 1000.0,
        response.status,
        response.header("Connection")
    );
    drop(reader);
    drop(sender);

    let report = receiver.join().map_err(|_| "receiver thread panicked")??;
    let snapshot = stats.snapshot();
    let audio_seconds = report.frames as f64 / 44_100.0;
    println!(
        "\nreport: codec {} encrypted {} packets {} frames {} ({:.3}s audio)",
        report.codec, report.encrypted, report.packets, report.frames, audio_seconds
    );
    println!(
        "        missing {} resend requests {} resent packets heard {} sync {}",
        report.missing, snapshot.resend_requests, snapshot.resent_packets, snapshot.sync_packets
    );

    if report.codec != "L16" || !report.encrypted {
        return Err(format!(
            "unexpected report: codec {:?} encrypted {}",
            report.codec, report.encrypted
        ));
    }
    if report.packets != total as u64 {
        return Err(format!("played {} packets, wanted {total}", report.packets));
    }
    if report.missing != 0 {
        return Err(format!("{} packets never arrived", report.missing));
    }
    if snapshot.resent_packets == 0 {
        return Err("the retransmission path was never exercised".to_string());
    }

    let bytes = std::fs::read(&wav_path).map_err(|e| e.to_string())?;
    let expected = 44 + total as usize * 1408;
    println!(
        "        WAV {}: {} bytes (expected {expected})",
        wav_path.display(),
        bytes.len()
    );
    if bytes.len() != expected {
        return Err("WAV length does not match the played audio".to_string());
    }
    let first_left = i16::from_le_bytes([bytes[44], bytes[45]]);
    if first_left != 1 {
        return Err(format!("first sample is {first_left}, wanted the little-endian 1"));
    }
    let _ = std::fs::remove_file(&wav_path);

    println!(
        "\nselftest passed in {:.0} ms",
        started.elapsed().as_secs_f64() * 1000.0
    );
    Ok(())
}

// ----------------------------------------------------------- mirror selftest

fn mirror_selftest(_args: &Args) -> Result<(), String> {
    const X_BASE: [u8; 32] = [
        9, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0,
    ];
    let started = Instant::now();
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).map_err(|e| e.to_string())?;
    let port = listener
        .local_addr()
        .map_err(|e| e.to_string())?
        .port();
    println!("selftest: mirror receiver listening on 127.0.0.1:{port}");

    let (frame_tx, frame_rx) = std::sync::mpsc::channel::<VideoFrame>();
    let config = Config {
        name: "HORIZON Mirror Selftest".to_string(),
        port: 0,
        rtsp_port: port,
        silent: true,
        frame_sink: Some(Arc::new(move |frame: VideoFrame| {
            let _ = frame_tx.send(frame);
        })),
        ..Config::default()
    };
    let identity = Identity::from_seed([0x42; 32]);
    let public_key = *identity.public_key();
    let stats = Arc::new(Stats::default());
    let thread_stats = Arc::clone(&stats);
    let receiver = thread::spawn(move || {
        let (stream, peer) = listener.accept().map_err(|e| e.to_string())?;
        let mut session = Session::new(
            stream,
            peer,
            Ipv4Addr::LOCALHOST,
            [0x02, 0x00, 0x00, 0x00, 0x00, 0x42],
            config,
            identity,
        )
        .map_err(|e| e.to_string())?;
        session.run(thread_stats).map_err(|e| e.to_string())
    });

    let mut sender = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).map_err(|e| e.to_string())?;
    sender
        .set_read_timeout(Some(Duration::from_secs(5)))
        .map_err(|e| e.to_string())?;
    let mut reader = BufReader::new(sender.try_clone().map_err(|e| e.to_string())?);
    let http = |method: &str, uri: &str, cseq: u32| {
        let mut request = Request::new(method, uri);
        request.version = "HTTP/1.1".to_string();
        request.set_header("CSeq", cseq.to_string());
        request
    };

    // 1. /info: the feature bitmap must match the mDNS advert byte for byte.
    let response = exchange(&mut sender, &mut reader, &http("GET", "/info", 1))?;
    let info = Plist::parse(&response.body).map_err(|e| e.to_string())?;
    let features = info
        .get("features")
        .and_then(Plist::as_i64)
        .ok_or("info has no features")?;
    if features != horizon_cast::airplay::mdns::FEATURES as i64 {
        return Err(format!(
            "info features {features}, advert {}",
            horizon_cast::airplay::mdns::FEATURES
        ));
    }
    let pk = info
        .get("pk")
        .and_then(Plist::as_data)
        .ok_or("info has no binary pk")?;
    if pk.len() != 32 {
        return Err(format!("/info pk is {} bytes, want 32", pk.len()));
    }
    let display = info
        .get("displays")
        .and_then(Plist::as_array)
        .and_then(|displays| displays.first())
        .ok_or("/info advertises no display")?;
    for key in ["width", "height", "widthPixels", "heightPixels", "uuid"] {
        if display.get(key).is_none() {
            return Err(format!("/info display is missing {key}"));
        }
    }
    let pk_hex: String = pk.iter().map(|byte| format!("{byte:02x}")).collect();
    println!(
        "{:>6.0}ms  /info 200: features 0x{features:X}, pk {}…, display {}x{}",
        started.elapsed().as_secs_f64() * 1000.0,
        &pk_hex[..8],
        display.get("width").and_then(Plist::as_i64).unwrap_or(0),
        display.get("height").and_then(Plist::as_i64).unwrap_or(0),
    );

    // 2. pair-setup returns the identity the advert published.
    let response = exchange(&mut sender, &mut reader, &http("POST", "/pair-setup", 2))?;
    if response.status != 200 || response.body != public_key.to_vec() {
        return Err(format!("pair-setup answered {} with {} bytes", response.status, response.body.len()));
    }

    // 3. pair-verify M1/M2. The sender derives the same CTR keystream the
    //    receiver used and checks the signature is over both ephemeral keys.
    let x_secret = [0x21u8; 32];
    let x_public = horizon_cast::airplay::nacl::x25519(&x_secret, &X_BASE);
    let ed_seed = [0x55u8; 32];
    let ed_public = horizon_cast::airplay::nacl::ed25519_public_key(&ed_seed);
    let mut request = http("POST", "/pair-verify", 3);
    request.body = [vec![1, 0, 0, 0], x_public.to_vec(), ed_public.to_vec()].concat();
    let response = exchange(&mut sender, &mut reader, &request)?;
    if response.status != 200 || response.body.len() != 96 {
        return Err(format!(
            "pair-verify M1 answered {} with {} bytes",
            response.status,
            response.body.len()
        ));
    }
    let mut ecdh_ours = [0u8; 32];
    ecdh_ours.copy_from_slice(&response.body[..32]);
    let shared = horizon_cast::airplay::nacl::x25519(&x_secret, &ecdh_ours);
    let key = fairplay::sha512_pair(b"Pair-Verify-AES-Key", &shared);
    let iv = fairplay::sha512_pair(b"Pair-Verify-AES-IV", &shared);
    let mut cipher = AesCtr::new(&key[..16], &iv[..16]).map_err(|e| e.to_string())?;
    let signature = cipher
        .process(&response.body[32..96])
        .map_err(|e| e.to_string())?;
    let mut signed = [0u8; 64];
    signed[..32].copy_from_slice(&ecdh_ours);
    signed[32..].copy_from_slice(&x_public);
    if !horizon_cast::airplay::nacl::ed25519_verify(
        &public_key,
        &signed,
        signature.as_slice().try_into().map_err(|_| "signature length")?,
    ) {
        return Err("pair-verify M1 signature did not verify".to_string());
    }
    let mut answer = [0u8; 64];
    answer[..32].copy_from_slice(&x_public);
    answer[32..].copy_from_slice(&ecdh_ours);
    let own = horizon_cast::airplay::nacl::ed25519_sign(&ed_seed, &answer);
    let mut request = http("POST", "/pair-verify", 4);
    request.body = [
        vec![0, 0, 0, 0],
        cipher.process(&own).map_err(|e| e.to_string())?,
    ]
    .concat();
    let response = exchange(&mut sender, &mut reader, &request)?;
    if response.status != 200 {
        return Err(format!("pair-verify M2 answered {}", response.status));
    }
    println!(
        "{:>6.0}ms  pair-setup + pair-verify 200, shared secret established",
        started.elapsed().as_secs_f64() * 1000.0
    );

    // 4. FairPlay: one fixed reply, then the key message SETUP unlocks.
    let mut request = http("POST", "/fp-setup", 5);
    let mut fp_request = vec![0u8; 16];
    fp_request[4] = 0x03;
    fp_request[14] = 0x01;
    request.body = fp_request;
    let response = exchange(&mut sender, &mut reader, &request)?;
    if response.status != 200 || response.body.len() != 142 {
        return Err(format!(
            "fp-setup 16 answered {} with {} bytes",
            response.status,
            response.body.len()
        ));
    }
    let mut key_message: Vec<u8> = (0..164u8).map(|index| index.wrapping_mul(3)).collect();
    key_message[..12].copy_from_slice(&fairplay::FP_HEADER);
    let mut request = http("POST", "/fp-setup", 6);
    request.body = key_message.clone();
    let response = exchange(&mut sender, &mut reader, &request)?;
    if response.status != 200 || response.body.len() != 32 {
        return Err(format!(
            "fp-setup 164 answered {} with {} bytes",
            response.status,
            response.body.len()
        ));
    }
    println!(
        "{:>6.0}ms  fp-setup 142 + 32 bytes, key message stored",
        started.elapsed().as_secs_f64() * 1000.0
    );

    // 5. The three SETUPs: keys, mirror port, mirrored-audio ports.
    let ekey: Vec<u8> = (0..72u8)
        .map(|index| index.wrapping_mul(5).wrapping_add(1))
        .collect();
    let initial = Plist::dict()
        .set("ekey", ekey.clone())
        .set("eiv", vec![0u8; 16])
        .set("isScreenMirroringSession", true)
        .set("timingPort", 6000i64)
        .to_bytes();
    let mut request = http("POST", "/setup", 7);
    request.body = initial;
    let response = exchange(&mut sender, &mut reader, &request)?;
    if response.status != 200 {
        return Err(format!("initial SETUP answered {}", response.status));
    }
    let reply = Plist::parse(&response.body).map_err(|e| e.to_string())?;
    let timing_port = reply
        .get("timingPort")
        .and_then(Plist::as_i64)
        .ok_or("initial SETUP reply has no timingPort")?
        as u16;
    if timing_port == 0 || timing_port == port {
        return Err(format!(
            "initial SETUP timingPort {timing_port} is not a UDP timing endpoint"
        ));
    }
    let event_port = reply
        .get("eventPort")
        .and_then(Plist::as_i64)
        .ok_or("initial SETUP reply has no eventPort")? as u16;
    if event_port == 0 || event_port == port {
        return Err(format!("initial SETUP eventPort {event_port} is not a TCP channel"));
    }
    TcpStream::connect((Ipv4Addr::LOCALHOST, event_port))
        .map_err(|e| format!("event channel refused the connection: {e}"))?;
    println!(
        "{:>6.0}ms  initial SETUP 200, timingPort udp {timing_port}, eventPort tcp {event_port}",
        started.elapsed().as_secs_f64() * 1000.0
    );

    // The timing endpoint has to answer NTP, or the sender aborts at RECORD.
    let clock = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).map_err(|e| e.to_string())?;
    clock
        .set_read_timeout(Some(Duration::from_secs(2)))
        .map_err(|e| e.to_string())?;
    let mut request_ntp = [0u8; 48];
    request_ntp[0] = 0x23; // version 4, mode 3 (client)
    request_ntp[40..48].copy_from_slice(&[9, 8, 7, 6, 5, 4, 3, 2]);
    clock
        .send_to(&request_ntp, (Ipv4Addr::LOCALHOST, timing_port))
        .map_err(|e| e.to_string())?;
    let mut answer = [0u8; 64];
    let (len, _) = clock
        .recv_from(&mut answer)
        .map_err(|e| format!("timing endpoint did not answer: {e}"))?;
    if len != 48 || answer[0] & 0x07 != 4 || &answer[24..32] != &[9, 8, 7, 6, 5, 4, 3, 2] {
        return Err("timing endpoint answered a malformed NTP reply".to_string());
    }
    println!(
        "{:>6.0}ms  timing NTP exchange answered with our transmit timestamp",
        started.elapsed().as_secs_f64() * 1000.0
    );

    let connection_id = 123_456_789_012i64;
    let mirror_request = Plist::dict()
        .set(
            "streams",
            Plist::Array(vec![Plist::dict()
                .set("type", 110i64)
                .set("streamConnectionID", connection_id)]),
        )
        .to_bytes();
    let mut request = http("POST", "/setup", 8);
    request.body = mirror_request;
    let response = exchange(&mut sender, &mut reader, &request)?;
    if response.status != 200 {
        return Err(format!("stream 110 SETUP answered {}", response.status));
    }
    let reply = Plist::parse(&response.body).map_err(|e| e.to_string())?;
    let mirror_port = reply
        .get("streams")
        .and_then(Plist::as_array)
        .and_then(|streams| streams.first())
        .and_then(|stream| stream.get("dataPort"))
        .and_then(Plist::as_i64)
        .ok_or("stream 110 reply has no dataPort")? as u16;

    let audio_request = Plist::dict()
        .set(
            "streams",
            Plist::Array(vec![Plist::dict().set("type", 96i64)]),
        )
        .to_bytes();
    let mut request = http("POST", "/setup", 9);
    request.body = audio_request;
    let response = exchange(&mut sender, &mut reader, &request)?;
    if response.status != 200 {
        return Err(format!("stream 96 SETUP answered {}", response.status));
    }
    println!(
        "{:>6.0}ms  SETUP 200: mirror TCP {mirror_port}, timing {port}",
        started.elapsed().as_secs_f64() * 1000.0
    );

    let response = exchange(&mut sender, &mut reader, &http("RECORD", "/record", 10))?;
    if response.status != 200 || response.header("Audio-Latency") != Some("11025") {
        return Err(format!(
            "RECORD answered {} with Audio-Latency {:?}",
            response.status,
            response.header("Audio-Latency")
        ));
    }

    // 6. The mirror connection: plaintext codec config, encrypted AVCC IDR.
    let aes_key = fairplay::decrypt_aes_key(&key_message, &ekey)
        .ok_or("the FairPlay key unwrap failed")?;
    let (stream_key, stream_iv) =
        stream_keys(&aes_key, &shared, &connection_id.to_string()).map_err(|e| e.to_string())?;
    let mut mirror = TcpStream::connect((Ipv4Addr::LOCALHOST, mirror_port))
        .map_err(|e| format!("mirror connect: {e}"))?;
    let codec: Vec<u8> = vec![
        1, 0x42, 0x00, 0x1e, 0x00, 0x01, 0x00, 0x03, 0x67, 0x42, 0x00, 0x01, 0x00, 0x02, 0x68, 0xce,
    ];
    let mut header = [0u8; 128];
    header[0..4].copy_from_slice(&(codec.len() as u32).to_le_bytes());
    header[4..6].copy_from_slice(&1u16.to_le_bytes());
    header[40..44].copy_from_slice(&1920.0f32.to_le_bytes());
    header[44..48].copy_from_slice(&1080.0f32.to_le_bytes());
    header[56..60].copy_from_slice(&1280.0f32.to_le_bytes());
    header[60..64].copy_from_slice(&720.0f32.to_le_bytes());
    mirror.write_all(&header).map_err(|e| e.to_string())?;
    mirror.write_all(&codec).map_err(|e| e.to_string())?;

    let nalu = [0x65u8, 1, 2, 3, 4];
    let mut avcc = Vec::with_capacity(4 + nalu.len());
    avcc.extend_from_slice(&(nalu.len() as u32).to_be_bytes());
    avcc.extend_from_slice(&nalu);
    let mut cipher = AesCtr::new(&stream_key, &stream_iv).map_err(|e| e.to_string())?;
    let encrypted = cipher.process(&avcc).map_err(|e| e.to_string())?;
    let ntp = (77u64 << 32) | 0x4000_0000;
    let mut header = [0u8; 128];
    header[0..4].copy_from_slice(&(encrypted.len() as u32).to_le_bytes());
    header[4..6].copy_from_slice(&0u16.to_le_bytes());
    header[8..16].copy_from_slice(&ntp.to_le_bytes());
    mirror.write_all(&header).map_err(|e| e.to_string())?;
    mirror.write_all(&encrypted).map_err(|e| e.to_string())?;

    let frame = frame_rx
        .recv_timeout(Duration::from_secs(5))
        .map_err(|_| "the decrypted frame never reached the sink".to_string())?;
    let expected: Vec<u8> = vec![
        0, 0, 0, 1, 0x67, 0x42, 0x00, 0, 0, 0, 1, 0x68, 0xce, 0, 0, 0, 1, 0x65, 1, 2, 3, 4,
    ];
    if frame.data != expected {
        return Err(format!("frame payload was {:02x?}", frame.data));
    }
    if !frame.keyframe || (frame.width, frame.height) != (1920, 1080) {
        return Err(format!(
            "frame flags: keyframe {} size {}x{}",
            frame.keyframe, frame.width, frame.height
        ));
    }
    if frame.pts_us != 77_250_000 {
        return Err(format!("frame PTS {} µs, wanted 77250000", frame.pts_us));
    }
    println!(
        "{:>6.0}ms  mirror frame: {} bytes, keyframe, 1920×1080, pts {} µs",
        started.elapsed().as_secs_f64() * 1000.0,
        frame.data.len(),
        frame.pts_us
    );
    drop(mirror);

    // 7. TEARDOWN ends the session; the report carries the same counters.
    let response = exchange(&mut sender, &mut reader, &http("TEARDOWN", "/teardown", 11))?;
    if response.status != 200 {
        return Err(format!("TEARDOWN answered {}", response.status));
    }
    drop(reader);
    drop(sender);

    let report = receiver.join().map_err(|_| "receiver thread panicked")??;
    let snapshot = stats.snapshot();
    println!(
        "\nreport: codec {} paired {} fairplay {} frames {} keyframes {} {}x{}",
        report.codec,
        report.paired,
        report.fairplay,
        report.mirror_frames,
        report.mirror_keyframes,
        report.mirror_width,
        report.mirror_height
    );
    println!(
        "        packets {} missing {} mirroring {}",
        snapshot.packets, snapshot.missing, snapshot.mirroring
    );
    if report.codec != "H.264" || !report.paired || !report.fairplay {
        return Err(format!(
            "unexpected report: codec {:?} paired {} fairplay {}",
            report.codec, report.paired, report.fairplay
        ));
    }
    if report.mirror_frames != 1 || report.mirror_keyframes != 1 {
        return Err(format!(
            "played {} frames ({} keyframes), wanted 1",
            report.mirror_frames, report.mirror_keyframes
        ));
    }
    if (report.mirror_width, report.mirror_height) != (1920, 1080) {
        return Err(format!(
            "reported geometry {}x{}",
            report.mirror_width, report.mirror_height
        ));
    }

    println!(
        "\nmirror selftest passed in {:.0} ms",
        started.elapsed().as_secs_f64() * 1000.0
    );
    Ok(())
}

fn exchange(
    sender: &mut TcpStream,
    reader: &mut BufReader<TcpStream>,
    request: &Request,
) -> Result<Response, String> {
    request.write_to(sender).map_err(|e| e.to_string())?;
    read_response(reader)
}

fn read_response(reader: &mut BufReader<TcpStream>) -> Result<Response, String> {
    Response::read(reader)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "the receiver closed the connection".to_string())
}

/// Builds one encrypted L16 RTP packet: 352 stereo frames, both channels known
/// values so the receiver's byte order and gain can be checked from the WAV.
fn build_audio_packet(
    aes: &AesStream,
    seq: u16,
    index: u16,
    resent: bool,
) -> Result<Vec<u8>, horizon_cast::CastError> {
    let mut pcm = vec![0u8; 352 * 4];
    for frame in 0..352usize {
        let value = (index as i16) * 100 + frame as i16 + 1;
        pcm[frame * 4..frame * 4 + 2].copy_from_slice(&value.to_be_bytes());
        pcm[frame * 4 + 2..frame * 4 + 4].copy_from_slice(&(-value).to_be_bytes());
    }
    let encrypted = aes.encrypt(&pcm)?;
    let header = if resent { 16 } else { 12 };
    let mut packet = vec![0u8; header + encrypted.len()];
    packet[0] = 0x80;
    packet[1] = if resent { 0xd6 } else { 0x60 };
    packet[2..4].copy_from_slice(&seq.to_be_bytes());
    packet[4..8].copy_from_slice(&(100_000u32 + (seq as u32) * 352).to_be_bytes());
    packet[8..12].copy_from_slice(&0x1234_5678u32.to_be_bytes());
    if resent {
        // The sequence number and count of the request being answered.
        packet[12..14].copy_from_slice(&1u16.to_be_bytes());
        packet[14..16].copy_from_slice(&1u16.to_be_bytes());
    }
    packet[header..].copy_from_slice(&encrypted);
    Ok(packet)
}

#[allow(clippy::too_many_arguments)]
fn spawn_resend_responder(
    control: UdpSocket,
    audio: UdpSocket,
    to: (Ipv4Addr, u16),
    key: Vec<u8>,
    iv: Vec<u8>,
    base_seq: u16,
    total: u16,
    lost_seq: u16,
) -> Arc<AtomicBool> {
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = Arc::clone(&stop);
    thread::spawn(move || {
        let Ok(aes) = AesStream::new(&key, &iv) else {
            return;
        };
        let mut buffer = [0u8; 64];
        while !thread_stop.load(Ordering::Relaxed) {
            let Ok((length, _)) = control.recv_from(&mut buffer) else {
                continue;
            };
            if length < 8 || buffer[1] != 0xd5 {
                continue;
            }
            let first = u16::from_be_bytes([buffer[4], buffer[5]]);
            let count = u16::from_be_bytes([buffer[6], buffer[7]]).max(1);
            for seq in first..first.saturating_add(count) {
                if seq != lost_seq {
                    continue;
                }
                let index = seq.wrapping_sub(base_seq).min(total - 1);
                if let Ok(packet) = build_audio_packet(&aes, seq, index, true) {
                    let _ = audio.send_to(&packet, to);
                }
            }
        }
    });
    stop
}
