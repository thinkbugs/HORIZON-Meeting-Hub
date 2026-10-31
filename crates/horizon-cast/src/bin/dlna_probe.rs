//! DLNA renderer probe: run it, drive it, or prove it works.
//!
//! Modes:
//! - default: start the DMR and print discovery details plus live transport state;
//! - `--url=...`: also point the renderer at that URL with SOAP and play it;
//! - `--selftest`: generate a WAV, serve it from loopback, and drive the renderer
//!   entirely through its own SOAP endpoints — SetAVTransportURI, Play, position,
//!   Pause, Seek, volume, Stop — then report PASS or the first step that failed.
//!
//! The self-test talks HTTP to the renderer rather than calling internals, which
//! is the point: it exercises the same path BubbleUPnP or Windows Media Player
//! would use. Its HTTP client is written here on purpose, so a bug that happens
//! to be symmetric in `dlna.rs` cannot hide.

use std::fs;
use std::io::{BufWriter, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use horizon_cast::dlna::{Dmr, DmrConfig};

struct Args {
    seconds: u64,
    port: u16,
    advertise: bool,
    show_video: bool,
    selftest: bool,
    url: Option<String>,
}

impl Default for Args {
    fn default() -> Self {
        Self {
            seconds: 15,
            port: 0,
            advertise: true,
            show_video: true,
            selftest: false,
            url: None,
        }
    }
}

fn main() {
    let args = match parse_args() {
        Ok(args) => args,
        Err(message) => {
            eprintln!("{message}");
            std::process::exit(2);
        }
    };
    let result = if args.selftest {
        selftest()
    } else {
        run(&args)
    };
    if let Err(message) = result {
        eprintln!("FAIL: {message}");
        std::process::exit(1);
    }
}

fn parse_args() -> Result<Args, String> {
    let mut args = Args::default();
    for argument in std::env::args().skip(1) {
        if argument == "--help" || argument == "-h" {
            println!(
                "dlna-probe [--seconds=N] [--port=N] [--no-advertise] [--headless]\n\
                 \x20           [--url=http://...] [--selftest]\n\n\
                 --seconds=N      how long the renderer runs when not self-testing (default 15)\n\
                 --port=N         fixed HTTP port (default: OS chooses)\n\
                 --no-advertise   do not join SSDP; the HTTP endpoints still work\n\
                 --headless       never show the video window\n\
                 --url=URL        after starting, SetAVTransportURI+Play through SOAP\n\
                 --selftest       serve a generated WAV and drive the renderer end to end"
            );
            std::process::exit(0);
        } else if let Some(value) = argument.strip_prefix("--seconds=") {
            args.seconds = value.parse().map_err(|_| "bad --seconds".to_string())?;
        } else if let Some(value) = argument.strip_prefix("--port=") {
            args.port = value.parse().map_err(|_| "bad --port".to_string())?;
        } else if argument == "--no-advertise" {
            args.advertise = false;
        } else if argument == "--headless" {
            args.show_video = false;
        } else if argument == "--selftest" {
            args.selftest = true;
        } else if let Some(value) = argument.strip_prefix("--url=") {
            args.url = Some(value.to_string());
        } else {
            return Err(format!("unknown argument {argument}; try --help"));
        }
    }
    Ok(args)
}

fn run(args: &Args) -> Result<(), String> {
    let dmr = Dmr::start(DmrConfig {
        friendly_name: "HORIZON Cast Receiver".into(),
        bind: "0.0.0.0".parse().unwrap(),
        port: args.port,
        show_video: args.show_video,
        video_parent: None,
        advertise: args.advertise,
    })
    .map_err(|e| format!("could not start the renderer: {e}"))?;
    print_details(&dmr);

    if let Some(url) = &args.url {
        let port = dmr.info().port;
        soap(port, "AVTransport", "SetAVTransportURI", &format!(
            "<InstanceID>0</InstanceID><CurrentURI>{}</CurrentURI><CurrentURIMetaData></CurrentURIMetaData>",
            escape(url)
        ))?;
        soap(port, "AVTransport", "Play", "<InstanceID>0</InstanceID><Speed>1</Speed>")?;
        println!("  playing    {url}");
    }

    let start = Instant::now();
    let deadline = start + Duration::from_secs(args.seconds);
    let mut last_phase = None;
    let mut last_uri: Option<String> = None;
    let mut last_print = Instant::now() - Duration::from_secs(2);
    while Instant::now() < deadline {
        let state = dmr.state();
        let phase_changed = last_phase != Some(state.phase);
        let uri_changed = last_uri != state.uri;
        if phase_changed || uri_changed || last_print.elapsed() >= Duration::from_secs(1) {
            last_phase = Some(state.phase);
            last_uri = state.uri.clone();
            last_print = Instant::now();
            println!(
                "  [{:>5.1}s] {:<16} {} / {}{}",
                start.elapsed().as_secs_f64(),
                format!("{:?}", state.phase),
                hms(state.position),
                hms(state.duration),
                state
                    .uri
                    .as_deref()
                    .map(|uri| format!("  {uri}"))
                    .unwrap_or_default()
            );
            if let Some(error) = &state.error {
                println!("            error: {error}");
            }
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    Ok(())
}

fn print_details(dmr: &Dmr) {
    let info = dmr.info();
    println!("HORIZON DLNA renderer");
    println!("  name       {}", info.friendly_name);
    println!("  udn        {}", info.udn);
    println!("  location   {}", info.location);
    match dmr.ssdp_error() {
        None => println!("  ssdp       announcing on 239.255.255.250:1900"),
        Some(error) => println!("  ssdp       unavailable: {error}"),
    }
    println!("  state      {:?}", dmr.state().phase);
    println!("  browser    http://127.0.0.1:{}/", info.port);
    println!();
}

fn selftest() -> Result<(), String> {
    let started = Instant::now();
    let stamp = |what: &str, detail: &str| {
        println!("[{:>6}ms] {:<10} {detail}", started.elapsed().as_millis(), what);
    };

    let wav = std::env::temp_dir().join("horizon-dmr-selftest.wav");
    write_tone(&wav).map_err(|e| format!("could not write the test tone: {e}"))?;
    let bytes = fs::metadata(&wav).map_err(|e| e.to_string())?.len();
    let media_port = serve_file(wav.clone())?;
    stamp(
        "host",
        &format!("http://127.0.0.1:{media_port}/tone.wav ({bytes} bytes)"),
    );

    let dmr = Dmr::start(DmrConfig {
        friendly_name: "HORIZON Selftest Renderer".into(),
        bind: "127.0.0.1".parse().unwrap(),
        port: 0,
        show_video: false,
        video_parent: None,
        advertise: false,
    })
    .map_err(|e| format!("could not start the renderer: {e}"))?;
    let port = dmr.info().port;
    stamp("dmr", &dmr.info().location);

    let uri = format!("http://127.0.0.1:{media_port}/tone.wav");
    soap(
        port,
        "AVTransport",
        "SetAVTransportURI",
        &format!(
            "<InstanceID>0</InstanceID><CurrentURI>{}</CurrentURI><CurrentURIMetaData></CurrentURIMetaData>",
            escape(&uri)
        ),
    )
    .map_err(|e| format!("SetAVTransportURI: {e}"))?;
    stamp("soap", "SetAVTransportURI -> 200");

    soap(
        port,
        "AVTransport",
        "Play",
        "<InstanceID>0</InstanceID><Speed>1</Speed>",
    )
    .map_err(|e| format!("Play: {e}"))?;
    stamp("soap", "Play -> 200");

    // The engine is asynchronous: TRANSITIONING until the WAV is decoded and the
    // audio clock is running, then PLAYING. Position is the proof that it is not
    // merely reporting a state it never reached.
    let play_deadline = Instant::now() + Duration::from_secs(10);
    let (seconds, duration) = loop {
        let info = soap(
            port,
            "AVTransport",
            "GetTransportInfo",
            "<InstanceID>0</InstanceID>",
        )?;
        let state = tag(&info, "CurrentTransportState").unwrap_or_default();
        let position = soap(
            port,
            "AVTransport",
            "GetPositionInfo",
            "<InstanceID>0</InstanceID>",
        )?;
        let rel = tag(&position, "RelTime").unwrap_or_default();
        let seconds = parse_hms(&rel).unwrap_or(-1.0);
        if state == "PLAYING" && seconds >= 0.5 {
            break (
                seconds,
                tag(&position, "TrackDuration").unwrap_or_default(),
            );
        }
        if Instant::now() >= play_deadline {
            let state_now = dmr.state();
            return Err(format!(
                "never reached PLAYING+0.5s (state {state}, rel {rel}, player {:?} {:?})",
                state_now.phase, state_now.error
            ));
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    stamp(
        "playing",
        &format!("RelTime {seconds:.2}s, duration {duration}"),
    );

    soap(
        port,
        "AVTransport",
        "Pause",
        "<InstanceID>0</InstanceID>",
    )?;
    std::thread::sleep(Duration::from_millis(300));
    let info = soap(
        port,
        "AVTransport",
        "GetTransportInfo",
        "<InstanceID>0</InstanceID>",
    )?;
    let paused = tag(&info, "CurrentTransportState").unwrap_or_default();
    if paused != "PAUSED_PLAYBACK" {
        return Err(format!("Pause reported {paused}"));
    }
    stamp("paused", "PAUSED_PLAYBACK");

    soap(
        port,
        "AVTransport",
        "Seek",
        "<InstanceID>0</InstanceID><Unit>REL_TIME</Unit><Target>0:00:03</Target>",
    )?;
    soap(
        port,
        "AVTransport",
        "Play",
        "<InstanceID>0</InstanceID><Speed>1</Speed>",
    )?;
    let seek_deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let position = soap(
            port,
            "AVTransport",
            "GetPositionInfo",
            "<InstanceID>0</InstanceID>",
        )?;
        let rel = tag(&position, "RelTime").unwrap_or_default();
        if parse_hms(&rel).unwrap_or(0.0) >= 3.0 {
            stamp("seek", &format!("resumed at {rel}"));
            break;
        }
        if Instant::now() >= seek_deadline {
            return Err(format!("seek to 3s did not take (rel {rel})"));
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    soap(
        port,
        "RenderingControl",
        "SetVolume",
        "<InstanceID>0</InstanceID><Channel>Master</Channel><DesiredVolume>30</DesiredVolume>",
    )?;
    std::thread::sleep(Duration::from_millis(300));
    let volume = soap(
        port,
        "RenderingControl",
        "GetVolume",
        "<InstanceID>0</InstanceID><Channel>Master</Channel>",
    )?;
    let current = tag(&volume, "CurrentVolume").unwrap_or_default();
    if current != "30" {
        return Err(format!("SetVolume 30 read back as {current}"));
    }
    stamp("volume", "SetVolume 30 -> GetVolume 30");

    soap(port, "AVTransport", "Stop", "<InstanceID>0</InstanceID>")?;
    std::thread::sleep(Duration::from_millis(200));
    let info = soap(
        port,
        "AVTransport",
        "GetTransportInfo",
        "<InstanceID>0</InstanceID>",
    )?;
    let stopped = tag(&info, "CurrentTransportState").unwrap_or_default();
    if stopped != "STOPPED" {
        return Err(format!("Stop reported {stopped}"));
    }
    stamp("stopped", "STOPPED");

    let protocols = soap(
        port,
        "ConnectionManager",
        "GetProtocolInfo",
        "",
    )?;
    let sink = tag(&protocols, "Sink").unwrap_or_default();
    if !sink.contains("audio/wav") {
        return Err(format!("GetProtocolInfo did not offer WAV: {sink}"));
    }
    stamp("sink", &format!("{} protocol entries", sink.split(',').count()));

    drop(dmr);
    let _ = fs::remove_file(&wav);
    println!("\nPASS: renderer played, paused, sought, changed volume and stopped through SOAP");
    Ok(())
}

// --------------------------------------------------------------------- host

/// A 6 second 440 Hz mono WAV, small enough to stream instantly over loopback.
fn write_tone(path: &PathBuf) -> std::io::Result<()> {
    const RATE: u32 = 44_100;
    const SECONDS: u32 = 6;
    let samples = RATE * SECONDS;
    let mut data = Vec::with_capacity(samples as usize * 2);
    for index in 0..samples {
        let t = index as f64 / RATE as f64;
        let sample = (t * 440.0 * std::f64::consts::TAU).sin() * 0.2;
        data.extend_from_slice(&((sample * i16::MAX as f64) as i16).to_le_bytes());
    }
    let mut file = BufWriter::new(fs::File::create(path)?);
    file.write_all(b"RIFF")?;
    file.write_all(&(36 + data.len() as u32).to_le_bytes())?;
    file.write_all(b"WAVEfmt ")?;
    file.write_all(&16u32.to_le_bytes())?;
    file.write_all(&1u16.to_le_bytes())?;
    file.write_all(&1u16.to_le_bytes())?;
    file.write_all(&RATE.to_le_bytes())?;
    file.write_all(&(RATE * 2).to_le_bytes())?;
    file.write_all(&2u16.to_le_bytes())?;
    file.write_all(&16u16.to_le_bytes())?;
    file.write_all(b"data")?;
    file.write_all(&(data.len() as u32).to_le_bytes())?;
    file.write_all(&data)?;
    file.flush()
}

/// Minimal HTTP/1.1 file server with byte ranges: Media Foundation's source
/// resolver issues them, and a server that ignores `Range` makes a seekable
/// file look unseekable.
fn serve_file(path: PathBuf) -> Result<u16, String> {
    let listener = TcpListener::bind(("127.0.0.1", 0)).map_err(|e| e.to_string())?;
    let port = listener.local_addr().map_err(|e| e.to_string())?.port();
    std::thread::spawn(move || {
        let data = fs::read(&path).unwrap_or_default();
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else {
                continue;
            };
            let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
            let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
            let mut request = Vec::new();
            let mut buffer = [0u8; 4096];
            while request.len() < 16 * 1024 {
                match stream.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(n) => {
                        request.extend_from_slice(&buffer[..n]);
                        if request.windows(4).any(|window| window == b"\r\n\r\n") {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
            let head = String::from_utf8_lossy(&request);
            let mut lines = head.split("\r\n");
            let request_line = lines.next().unwrap_or_default();
            let mut parts = request_line.split_whitespace();
            let method = parts.next().unwrap_or("GET");
            let mut range = None;
            for line in lines {
                let lower = line.to_ascii_lowercase();
                if let Some(value) = lower.strip_prefix("range:") {
                    range = parse_range(value.trim(), data.len());
                }
            }
            let response = match range {
                Some((start, end)) => {
                    let body = &data[start..=end];
                    let mut out = format!(
                        "HTTP/1.1 206 Partial Content\r\nContent-Type: audio/wav\r\n\
                         Content-Length: {}\r\nContent-Range: bytes {start}-{end}/{}\r\n\
                         Accept-Ranges: bytes\r\nConnection: close\r\n\r\n",
                        body.len(),
                        data.len()
                    )
                    .into_bytes();
                    out.extend_from_slice(body);
                    out
                }
                None => {
                    let mut out = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: audio/wav\r\nContent-Length: {}\r\n\
                         Accept-Ranges: bytes\r\nConnection: close\r\n\r\n",
                        data.len()
                    )
                    .into_bytes();
                    out.extend_from_slice(&data);
                    out
                }
            };
            if method != "HEAD" {
                let _ = stream.write_all(&response);
            } else if let Some((start, end)) = range {
                let _ = stream.write_all(
                    format!(
                        "HTTP/1.1 206 Partial Content\r\nContent-Type: audio/wav\r\n\
                         Content-Length: {}\r\nContent-Range: bytes {start}-{end}/{}\r\n\
                         Accept-Ranges: bytes\r\nConnection: close\r\n\r\n",
                        end - start + 1,
                        data.len()
                    )
                    .as_bytes(),
                );
            }
            let _ = stream.flush();
        }
    });
    Ok(port)
}

fn parse_range(value: &str, len: usize) -> Option<(usize, usize)> {
    let spec = value.strip_prefix("bytes=")?;
    let (start, end) = spec.split_once('-')?;
    let start: usize = if start.is_empty() {
        len.checked_sub(end.parse().ok()?)?
    } else {
        start.parse().ok()?
    };
    let end: usize = if end.is_empty() {
        len.checked_sub(1)?
    } else {
        end.parse::<usize>().ok()?.min(len.saturating_sub(1))
    };
    (start <= end && start < len).then_some((start, end))
}

// --------------------------------------------------------------------- soap

fn soap(port: u16, service: &str, action: &str, inner: &str) -> Result<String, String> {
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
    let mut stream =
        TcpStream::connect(("127.0.0.1", port)).map_err(|e| format!("connect: {e}"))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .map_err(|e| e.to_string())?;
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .map_err(|e| e.to_string())?;
    stream
        .write_all(request.as_bytes())
        .map_err(|e| format!("write: {e}"))?;
    let mut response = String::new();
    stream
        .read_to_string(&mut response)
        .map_err(|e| format!("read: {e}"))?;
    if !response.starts_with("HTTP/1.1 200") {
        let code = tag(&response, "errorCode").unwrap_or_default();
        let text = tag(&response, "errorDescription").unwrap_or_default();
        return Err(format!("{action}: {code} {text}"));
    }
    Ok(response)
}

fn tag(body: &str, name: &str) -> Option<String> {
    let needle = format!("<{name}>");
    let start = body.find(&needle)? + needle.len();
    let end = body[start..].find(&format!("</{name}>"))? + start;
    Some(unescape(body[start..end].trim()))
}

fn escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn unescape(value: &str) -> String {
    value
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&amp;", "&")
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
    let parts: Vec<&str> = value.trim().split(':').collect();
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
    Some(hours * 3600.0 + minutes * 60.0 + seconds)
}
