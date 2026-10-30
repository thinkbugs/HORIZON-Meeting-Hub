//! Ask the LAN who can receive a cast, and print what answered.
//!
//! This is the human view of [`horizon_cast::discovery`]: it browses mDNS for
//! HORIZON / Cast / AirPlay services and searches SSDP for DLNA renderers and DIAL,
//! then optionally fetches the UPnP description behind each SSDP `LOCATION` so the
//! output shows a name instead of a UUID.
//!
//! A silent result is a valid result. Multicast is easy to lose: Windows Firewall
//! blocks inbound 5353/1900 until the executable is allowed, a VPN can swallow the
//! group, and many access points drop multicast between wireless clients. The probe
//! prints the count of malformed packets it ignored, which distinguishes "the port
//! is busy but nothing valid arrived" from "nothing is out there".
//!
//! Usage: `discover-probe [--seconds=10] [--no-resolve]`

use std::collections::HashMap;
use std::time::{Duration, Instant};

use horizon_cast::discovery::{describe, primary_interface, Discovery, DiscoveryEvent, Peer};

fn main() {
    let args = Args::parse();
    if let Err(e) = run(args) {
        eprintln!("discover-probe: {e}");
        std::process::exit(1);
    }
}

struct Args {
    seconds: f64,
    resolve: bool,
}

impl Args {
    fn parse() -> Self {
        let mut args = Self {
            seconds: 10.0,
            resolve: true,
        };
        for arg in std::env::args().skip(1) {
            if let Some(value) = arg.strip_prefix("--seconds=") {
                args.seconds = value.parse().unwrap_or(args.seconds);
            } else if arg == "--no-resolve" {
                args.resolve = false;
            } else if arg == "--help" || arg == "-h" {
                println!("usage: discover-probe [--seconds=N] [--no-resolve]");
                std::process::exit(0);
            }
        }
        args
    }
}

fn run(args: Args) -> Result<(), String> {
    let mut discovery = Discovery::new().map_err(|e| e.to_string())?;
    if let Some(reason) = discovery.mdns_error() {
        println!("mDNS unavailable ({reason}); continuing with SSDP only");
    }
    println!(
        "browsing mDNS + SSDP for {:.0}s from {} ...",
        args.seconds.max(0.5),
        primary_interface()
    );

    let deadline = Instant::now() + Duration::from_secs_f64(args.seconds.max(0.5));
    while Instant::now() < deadline {
        let events = discovery
            .poll(Duration::from_millis(250))
            .map_err(|e| e.to_string())?;
        for event in events {
            match event {
                DiscoveryEvent::Found(peer) => print_event("found  ", &peer),
                DiscoveryEvent::Updated(peer) => print_event("updated", &peer),
                DiscoveryEvent::Lost(id) => println!("lost     {id}"),
            }
        }
    }

    let peers = discovery.peers();
    println!("\n{} peer(s) after {:.0}s", peers.len(), args.seconds);
    if peers.is_empty() {
        println!(
            "  nothing answered. Check that the firewall allows inbound UDP 5353/1900,\n  \
             that this machine is on the same subnet as the devices, and that the\n  \
             access point forwards multicast."
        );
    } else {
        for peer in &peers {
            println!(
                "  {:<12} {:<28} {:<28} {}",
                peer.protocol.label(),
                peer.name,
                address(peer),
                txt_summary(peer)
            );
        }
    }
    if discovery.ignored_packets() > 0 {
        println!(
            "  ({} malformed packet(s) on 5353 were ignored)",
            discovery.ignored_packets()
        );
    }

    if args.resolve {
        resolve_names(&peers);
    }
    Ok(())
}

fn print_event(prefix: &str, peer: &Peer) {
    println!(
        "{prefix} {:<12} {:<28} {:<28} {}",
        peer.protocol.label(),
        peer.name,
        address(peer),
        txt_summary(peer)
    );
}

fn address(peer: &Peer) -> String {
    match (&peer.addr, &peer.location) {
        (Some(addr), _) => addr.to_string(),
        (None, Some(location)) => location.clone(),
        _ => "-".to_string(),
    }
}

fn txt_summary(peer: &Peer) -> String {
    let mut parts = Vec::new();
    for key in ["fn", "md", "model"] {
        if let Some(value) = peer.txt_value(key) {
            parts.push(format!("{key}={value}"));
        }
    }
    parts.join(" ")
}

/// SSDP peers are named by UUID until someone reads the description behind
/// `LOCATION`. Do it once, at the end, so the discovery loop is never blocked on a
/// device that answers slowly.
fn resolve_names(peers: &[Peer]) {
    let mut names: HashMap<&str, (String, String)> = HashMap::new();
    for peer in peers {
        let Some(location) = &peer.location else {
            continue;
        };
        let rendered = match describe(location, Duration::from_secs(1)) {
            Ok(description) => {
                let name = description
                    .friendly_name
                    .unwrap_or_else(|| "<unnamed>".to_string());
                let maker = match (description.manufacturer, description.model_name) {
                    (Some(maker), Some(model)) => format!("{maker} / {model}"),
                    (Some(maker), None) => maker,
                    (None, Some(model)) => model,
                    (None, None) => String::new(),
                };
                (name, maker)
            }
            Err(error) => (format!("<unreachable: {error}>"), String::new()),
        };
        names.insert(peer.id.as_str(), rendered);
    }
    if names.is_empty() {
        return;
    }
    println!("\nfrom the UPnP descriptions:");
    for (id, (name, maker)) in names {
        if maker.is_empty() {
            println!("  {id}\n    {name}");
        } else {
            println!("  {id}\n    {name} ({maker})");
        }
    }
}
