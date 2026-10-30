//! Run the Google Cast receiver outside the launcher.
//!
//! This is the tool for checking interop with a real sender: it prints every
//! CASTV2 message it receives (with `--debug`), so Chrome's discovery, launch,
//! load and transport sequence is visible in a terminal.
//!
//! Usage:
//! ```text
//! cast-recv-probe [--name="HORIZON Meeting"] [--seconds=120] [--debug]
//! ```

use std::time::Duration;

use horizon_cast::cast_recv::{CastReceiver, CastReceiverConfig, CASTV2_PORT};

fn main() {
    let mut name = "HORIZON Meeting".to_string();
    let mut seconds = 120.0;
    let mut debug = false;
    for arg in std::env::args().skip(1) {
        if let Some(value) = arg.strip_prefix("--name=") {
            name = value.to_string();
        } else if let Some(value) = arg.strip_prefix("--seconds=") {
            seconds = value.parse().unwrap_or(seconds);
        } else if arg == "--debug" {
            debug = true;
        } else if arg == "--help" || arg == "-h" {
            println!("usage: cast-recv-probe [--name=NAME] [--seconds=N] [--debug]");
            return;
        }
    }
    if debug {
        std::env::set_var("HORIZON_CAST_DEBUG", "1");
    }

    let receiver = match CastReceiver::start(CastReceiverConfig {
        name: name.clone(),
        ..Default::default()
    }) {
        Ok(receiver) => receiver,
        Err(error) => {
            eprintln!("cast-recv-probe: cannot start receiver: {error}");
            std::process::exit(1);
        }
    };

    println!("cast receiver '{name}' listening on 0.0.0.0:{CASTV2_PORT}");
    println!("advertising _googlecast._tcp.local for {seconds:.0}s");
    match horizon_cast::cast_recv::trust_anchor_path() {
        Some(path) => println!(
            "Chromium trust anchor: {}\nlaunch browsers with: --cast-developer-certificate-path=\"{}\"",
            path.display(),
            path.display()
        ),
        None => println!("warning: could not write the Chromium trust anchor"),
    }
    if debug {
        println!("debug: incoming CASTV2 messages are printed below");
    }

    std::thread::sleep(Duration::from_secs_f64(seconds));
    receiver.stop();
    println!("cast receiver stopped");
}
