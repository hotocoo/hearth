//! `hearth`: inspect a machine that runs local models, and keep its model
//! servers alive. The `hearth` CLI and the `hearthd` daemon are both built
//! from this library.
//!
//! Every command prints a human summary, or with `--json` a machine-readable
//! document meant for scripts and AI agents. Nothing here guesses: values are
//! measured, read from files, or given on the command line.

mod config_cmd;
#[cfg(unix)]
mod control;
#[cfg(unix)]
pub mod daemon;
mod doctor;
mod fit;
mod history;
mod incidents;
mod mcp;
mod notify;
mod ports;
mod run;
#[cfg(unix)]
mod service;
mod units;
mod watch;

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
    /// Every listening port, the process behind it, and which model runtime
    /// answers there (Ollama, llama.cpp, ComfyUI, LM Studio, OpenAI-compatible).
    Ports(ports::Args),
    /// Snapshots hearth recorded of this machine, newest first (`ports` records
    /// one on every unfiltered run).
    Log(history::LogArgs),
    /// What changed between two recorded snapshots: ports that appeared or
    /// vanished, runtimes that restarted, stopped answering or loaded other
    /// models, and how their memory moved. Defaults to the last two.
    Diff(history::DiffArgs),
    /// One recorded snapshot in full, as JSON (`hearth log` lists them).
    Show(history::ShowArgs),
    /// One process, port or server across every recorded snapshot: when it
    /// appeared, changed, restarted, grew or went away.
    Follow(history::FollowArgs),
    /// Start a model server and keep it alive: restart it when it exits or
    /// stops answering, give up on a crash loop, and stop it when hearth
    /// stops. Refuses a port another server already holds.
    Run(run::Args),
    /// Keep running doctor and ports, record every round, and report (and
    /// notify) when memory pressure rises, a resource would reach its limit
    /// soon, or a model runtime vanishes, restarts or stops answering.
    Watch(watch::Args),
    /// What hearthd keeps alive: each server's state, pid, restarts and last
    /// event.
    #[cfg(unix)]
    Status(control::StatusArgs),
    /// Ask hearthd to restart one of its servers now (the restart budget is
    /// not charged).
    #[cfg(unix)]
    Restart(control::RestartArgs),
    /// Run hearthd at login under launchd (macOS) or a systemd user unit
    /// (Linux): install, uninstall, or show the unit.
    #[cfg(unix)]
    Service(service::Args),
    /// Every setting in effect: its value, whether it is built in or comes
    /// from your file, and what it means.
    Config(config_cmd::Args),
    /// Serve these commands as tools to AI agents over the Model Context
    /// Protocol on stdin/stdout.
    Mcp,
}

fn execute(cli: &Cli) -> Result<String, String> {
    match &cli.command {
        Command::Doctor(args) => doctor::run(args, cli.json),
        Command::Incidents(args) => incidents::run(args, cli.json),
        Command::Fit(args) => fit::run(args, cli.json),
        Command::Ports(args) => ports::run(args, cli.json),
        Command::Log(args) => history::log(args, cli.json),
        Command::Diff(args) => history::diff(args, cli.json),
        Command::Show(args) => history::show(args, cli.json),
        Command::Follow(args) => history::follow(args, cli.json),
        Command::Run(args) => run::run(args, cli.json),
        Command::Watch(args) => watch::run(args, cli.json),
        Command::Config(args) => config_cmd::run(args, cli.json),
        #[cfg(unix)]
        Command::Service(args) => service::run(args, cli.json),
        #[cfg(unix)]
        Command::Status(args) => control::status(args, cli.json),
        #[cfg(unix)]
        Command::Restart(args) => control::restart(args, cli.json),
        Command::Mcp => mcp::serve(),
    }
}

/// Runs a command line in-process; how MCP tools reach the CLI commands.
pub(crate) fn dispatch(argv: &[String]) -> Result<String, String> {
    let cli = Cli::try_parse_from(argv).map_err(|e| e.to_string())?;
    if matches!(
        cli.command,
        Command::Mcp | Command::Run(_) | Command::Watch(_)
    ) {
        return Err("long-running commands cannot be called as tools".into());
    }
    #[cfg(unix)]
    if matches!(cli.command, Command::Service(_)) {
        return Err("installing a login service is for a person to decide, not a tool".into());
    }
    execute(&cli)
}

/// The `hearth` command line.
pub fn main() {
    let cli = Cli::parse();
    let result = execute(&cli);
    match result {
        Ok(text) => {
            use std::io::Write as _;
            // A reader that went away (`| head`) has all it wanted.
            let mut out = std::io::stdout().lock();
            if out
                .write_all(text.as_bytes())
                .and_then(|()| out.flush())
                .is_err()
            {
                std::process::exit(0);
            }
        }
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
