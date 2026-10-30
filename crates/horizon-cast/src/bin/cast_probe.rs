//! Drive a Google Cast receiver from the command line.
//!
//! Two modes:
//! - no `--host`: browse mDNS for `_googlecast._tcp` and print what answered;
//! - with `--host`: open the TLS control channel, launch the Default Media
//!   Receiver, optionally load a URL, then print media status until the poll
//!   window ends.
//!
//! Usage:
//! ```text
//! cast-probe [--seconds=6]
//! cast-probe --host=192.168.0.23 [--url=http://... --type=video/mp4]
//! ```
//!
//! A device that answers discovery but refuses the channel will say so once:
//! the reason is printed and the process exits non-zero. There is no retry loop,
//! because a failed handshake on the same address will fail the same way.

use std::net::{SocketAddr, ToSocketAddrs};
use std::time::{Duration, Instant};

use horizon_cast::cast::{self, Client, Event, DEFAULT_MEDIA_RECEIVER};
use horizon_cast::discovery::{primary_interface, Discovery, DiscoveryEvent, Protocol};
use horizon_cast::CastError;

fn main() {
    let args = Args::parse();
    if let Err(e) = run(args) {
        eprintln!("cast-probe: {e}");
        std::process::exit(1);
    }
}

struct Args {
    seconds: f64,
    host: Option<String>,
    name: Option<String>,
    url: Option<String>,
    content_type: String,
    volume: Option<f64>,
    seek: Option<f64>,
    stop: bool,
    poll_seconds: f64,
}

impl Args {
    fn parse() -> Self {
        let mut args = Self {
            seconds: 6.0,
            host: None,
            name: None,
            url: None,
            content_type: "video/mp4".to_string(),
            volume: None,
            seek: None,
            stop: false,
            poll_seconds: 20.0,
        };
        for arg in std::env::args().skip(1) {
            if let Some(value) = arg.strip_prefix("--seconds=") {
                args.seconds = value.parse().unwrap_or(args.seconds);
            } else if let Some(value) = arg.strip_prefix("--host=") {
                args.host = Some(value.to_string());
            } else if let Some(value) = arg.strip_prefix("--name=") {
                args.name = Some(value.to_string());
            } else if let Some(value) = arg.strip_prefix("--url=") {
                args.url = Some(value.to_string());
            } else if let Some(value) = arg.strip_prefix("--type=") {
                args.content_type = value.to_string();
            } else if let Some(value) = arg.strip_prefix("--volume=") {
                args.volume = value.parse().ok();
            } else if let Some(value) = arg.strip_prefix("--seek=") {
                args.seek = value.parse().ok();
            } else if let Some(value) = arg.strip_prefix("--poll=") {
                args.poll_seconds = value.parse().unwrap_or(args.poll_seconds);
            } else if arg == "--stop" {
                args.stop = true;
            } else if arg == "--help" || arg == "-h" {
                println!(
                    "usage: cast-probe [--seconds=N]\n       \
                     cast-probe --host=ADDR[:PORT] [--name=HOSTNAME] [--url=URL] \
                     [--type=MIME] [--volume=0..1] [--seek=SECONDS] [--poll=SECONDS] [--stop]"
                );
                std::process::exit(0);
            }
        }
        args
    }
}

fn run(args: Args) -> Result<(), String> {
    let Some(host) = args.host.clone() else {
        return browse(args.seconds);
    };

    let (address, default_name) = resolve_host(&host)?;
    let server_name = args.name.clone().unwrap_or(default_name);
    println!("cast        {address} (tls name {server_name:?})");
    let stream = cast::tls::TlsStream::connect(address, &server_name, Duration::from_secs(8))
        .map_err(|e| format!("tls: {e}"))?;
    println!("tls         established");
    let mut client = Client::new(stream, "sender-0");
    client.connect().map_err(|e| format!("connect: {e}"))?;
    println!("channel     CONNECT sent");

    let session = client
        .launch(DEFAULT_MEDIA_RECEIVER)
        .map_err(|e| format!("launch {DEFAULT_MEDIA_RECEIVER}: {e}"))?;
    println!(
        "launched    app {} session {} transport {}",
        session.app_id, session.session_id, session.transport_id
    );

    if let Some(level) = args.volume {
        client
            .set_volume(level)
            .map_err(|e| format!("volume: {e}"))?;
        println!("volume      set to {level:.2}");
    }

    let Some(url) = args.url.clone() else {
        client.stop().map_err(|e| format!("stop: {e}"))?;
        client.close();
        println!("stopped     no URL was given, so the app was closed again");
        return Ok(());
    };

    let started = Instant::now();
    let status = client
        .load(&url, &args.content_type)
        .map_err(|e| format!("load {url}: {e}"))?;
    println!(
        "loaded      {} at {:.2}s ({})",
        status.player_state, status.current_time, args.content_type
    );

    if let Some(seconds) = args.seek {
        let status = client.seek(seconds).map_err(|e| format!("seek: {e}"))?;
        println!("sought      {:.2}s, state {}", status.current_time, status.player_state);
    }

    let deadline = Instant::now() + Duration::from_secs_f64(args.poll_seconds.max(0.5));
    let mut last = String::new();
    while Instant::now() < deadline {
        match client.poll() {
            Ok(Some(Event::Media(status))) => {
                let idle = status
                    .idle_reason
                    .as_deref()
                    .map(|reason| format!(" ({reason})"))
                    .unwrap_or_default();
                let line = format!(
                    "{} at {:.2}s / {:.2}s{}",
                    status.player_state, status.current_time, status.duration, idle
                );
                if line != last {
                    println!("media       {line}  [+{:.1}s]", started.elapsed().as_secs_f64());
                    last = line;
                }
            }
            Ok(Some(Event::Receiver {
                volume,
                muted,
                session,
            })) => {
                println!(
                    "receiver    volume={:?} muted={:?} app={:?}",
                    volume,
                    muted,
                    session.map(|s| s.app_id)
                );
            }
            Ok(Some(Event::Closed)) => {
                println!("channel     closed by the device");
                return Ok(());
            }
            // A read timeout is how the loop regains control; the device sends
            // a heartbeat every few seconds, so a long silence is still normal.
            Ok(None) | Err(CastError::Timeout) => {}
            Err(e) => return Err(format!("poll: {e}")),
        }
    }

    if args.stop {
        client.stop().map_err(|e| format!("stop: {e}"))?;
        println!("stopped     receiver app closed");
    } else {
        println!("leaving     receiver running; pass --stop to close it");
    }
    client.close();
    Ok(())
}

/// `ip`, `ip:port`, or `host:port`; the Cast control port is always 8009.
fn resolve_host(host: &str) -> Result<(SocketAddr, String), String> {
    let with_port = if host.rsplit_once(':').is_some_and(|(_, port)| port.parse::<u16>().is_ok()) {
        host.to_string()
    } else {
        format!("{host}:8009")
    };
    let name = with_port
        .rsplit_once(':')
        .map(|(name, _)| name.trim_matches(['[', ']']).to_string())
        .unwrap_or_else(|| host.to_string());
    let address = with_port
        .to_socket_addrs()
        .map_err(|e| format!("cannot resolve {host}: {e}"))?
        .next()
        .ok_or_else(|| format!("no address for {host}"))?;
    Ok((address, name))
}

fn browse(seconds: f64) -> Result<(), String> {
    let mut discovery = Discovery::new().map_err(|e| e.to_string())?;
    if let Some(reason) = discovery.mdns_error() {
        println!("mDNS unavailable ({reason}); cannot find Cast devices");
        return Ok(());
    }
    println!(
        "browsing mDNS for _googlecast._tcp on {} for {:.0}s ...",
        primary_interface(),
        seconds.max(0.5)
    );
    let deadline = Instant::now() + Duration::from_secs_f64(seconds.max(0.5));
    while Instant::now() < deadline {
        let events = discovery
            .poll(Duration::from_millis(250))
            .map_err(|e| e.to_string())?;
        for event in events {
            match event {
                DiscoveryEvent::Found(peer) | DiscoveryEvent::Updated(peer)
                    if peer.protocol == Protocol::GoogleCast =>
                {
                    let name = peer
                        .txt_value("fn")
                        .map(str::to_string)
                        .unwrap_or_else(|| peer.name.clone());
                    let model = peer
                        .txt_value("md")
                        .map(str::to_string)
                        .unwrap_or_default();
                    let addr = peer
                        .addr
                        .map(|a| a.ip().to_string())
                        .unwrap_or_else(|| "-".to_string());
                    println!(
                        "device      {name:<28} {addr:<16} 8009  {model}"
                    );
                }
                _ => {}
            }
        }
    }
    let cast = discovery
        .peers()
        .into_iter()
        .filter(|peer| peer.protocol == Protocol::GoogleCast)
        .count();
    if cast == 0 {
        println!(
            "no Cast device answered. A Chromecast answers mDNS even in standby;\n  \
             a TV with Cast built in may only do so while it is awake."
        );
    }
    Ok(())
}
