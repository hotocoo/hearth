//! `hearthd`: keeps the model servers in `[servers]` alive.

use clap::Parser;

/// Keep every server in your config's [servers] alive: restart it when it
/// exits, wedges or poisons its backend, record and notify what happened,
/// and answer `hearth status` and `hearth restart`. Proxies no inference.
#[derive(Parser)]
#[command(name = "hearthd", version)]
struct Args {}

fn main() {
    let _ = Args::parse();
    if let Err(e) = hearth::daemon::run() {
        eprintln!("hearthd: {e}");
        std::process::exit(1);
    }
}
