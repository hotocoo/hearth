//! `hearth`: inspect a machine that runs local models.
//!
//! Every command prints a human summary, or with `--json` a machine-readable
//! document meant for scripts and AI agents. Nothing here guesses: values are
//! measured, read from files, or given on the command line.

mod doctor;
mod fit;
mod incidents;
mod units;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "hearth", version, about)]
struct Cli {
    /// Print JSON instead of a human summary.
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Memory, open files and threads: the resources whose exhaustion takes
    /// the whole machine down.
    Doctor(doctor::Args),
    /// Kernel panics, jetsam kills and resource-limit reports the OS recorded.
    Incidents(incidents::Args),
    /// Whether a GGUF model at a given context fits on this machine, now and
    /// at all.
    Fit(fit::Args),
}

fn main() {
    let cli = Cli::parse();
    let result = match cli.command {
        Command::Doctor(args) => doctor::run(&args, cli.json),
        Command::Incidents(args) => incidents::run(&args, cli.json),
        Command::Fit(args) => fit::run(&args, cli.json),
    };
    match result {
        Ok(text) => print!("{text}"),
        Err(e) => {
            if cli.json {
                println!("{}", serde_json::json!({ "error": e }));
            } else {
                eprintln!("hearth: {e}");
            }
            std::process::exit(1);
        }
    }
}

pub(crate) fn to_json<T: serde::Serialize>(value: &T) -> Result<String, String> {
    serde_json::to_string_pretty(value)
        .map(|s| s + "\n")
        .map_err(|e| e.to_string())
}
