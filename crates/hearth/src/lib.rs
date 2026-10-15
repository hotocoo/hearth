//! `hearth`: inspect a machine that runs local models, and keep its model
//! servers alive. The `hearth` CLI and the `hearthd` daemon are both built
//! from this library.
//!
//! Every command prints a human summary, or with `--json` a machine-readable
//! document meant for scripts and AI agents. Nothing here guesses: values are
//! measured, read from files, or given on the command line.

mod bundle;
mod config_cmd;
#[cfg(unix)]
mod control;
#[cfg(unix)]
pub mod daemon;
mod doctor;
mod draft;
mod fit;
#[cfg(unix)]
mod gateway;
mod history;
mod incidents;
mod mcp;
mod measured;
#[cfg(unix)]
mod metrics;
mod model_files;
mod models;
mod note;
mod notify;
mod ports;
mod pressure;
mod run;
mod server_args;
#[cfg(unix)]
mod service;
#[cfg(unix)]
mod top;
mod units;
mod watch;
mod workflows;

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
    /// Whether a model (GGUF, MLX or safetensors) at a given context fits on
    /// this machine, now and at all.
    Fit(fit::Args),
    /// Every model on this machine's disks (Hugging Face cache, LM Studio,
    /// Ollama), with its weights and the largest context the device holds
    /// it at. Names are what `fit` takes.
    Models(models::Args),
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
    /// Name this moment (the latest snapshot of each kind), like `git tag`:
    /// kept past retention, and usable wherever a snapshot is asked for.
    Tag(history::TagArgs),
    /// Record what was done or seen in the journal, beside what hearth
    /// observed, like a commit message: "restarted llama-server with -c
    /// 131072". `hearth log --kind note` lists notes.
    Note(note::Args),
    /// Start a model server and keep it alive: restart it when it exits or
    /// stops answering, give up on a crash loop, and stop it when hearth
    /// stops. Refuses a port another server already holds.
    Run(run::Args),
    /// Keep running doctor and ports, record every round, and report (and
    /// notify) when memory pressure rises, a resource would reach its limit
    /// soon, or a model runtime vanishes, restarts or stops answering.
    Watch(watch::Args),
    /// One screen that keeps showing hearthd's servers, the machine's memory
    /// and findings, and the journal; keys restart, stop and start servers and
    /// show their output. `--once` prints one frame, `--json` one document.
    #[cfg(unix)]
    Top(top::Args),
    /// What hearthd keeps alive: each server's state, pid, restarts and last
    /// event.
    #[cfg(unix)]
    Status(control::StatusArgs),
    /// Ask hearthd to restart one of its servers now (the restart budget is
    /// not charged).
    #[cfg(unix)]
    Restart(control::RestartArgs),
    /// Ask hearthd to stop one of its servers and keep it down, to free its
    /// memory, until `hearth start`.
    #[cfg(unix)]
    Stop(control::RestartArgs),
    /// Ask hearthd to start a server it stopped or gave up on.
    #[cfg(unix)]
    Start(control::RestartArgs),
    /// The last lines one of hearthd's servers printed.
    #[cfg(unix)]
    Logs(control::LogsArgs),
    /// Apply the configuration's [servers] to hearthd now: new servers
    /// start, removed ones stop, changed ones restart, the rest keep running.
    #[cfg(unix)]
    Reload(control::ReloadArgs),
    /// Run hearthd at login under launchd (macOS) or a systemd user unit
    /// (Linux): install, uninstall, or show the unit.
    #[cfg(unix)]
    Service(service::Args),
    /// Everything hearth can say about this machine in one JSON document:
    /// doctor, ports, models on disk, incidents, hearthd, recent run events and notes, tags and
    /// settings, with secrets redacted. For bug reports and agents.
    Bundle(bundle::Args),
    /// Workflows: which tools to call, in which order, for a recurring
    /// question. Lists them, or prints one filled in. Add your own in
    /// workflows.json beside your config; MCP serves them as prompts.
    Workflows(workflows::Args),
    /// Every setting in effect: its value, whether it is built in or comes
    /// from your file, and what it means.
    Config(config_cmd::Args),
    /// Print the shell completion script, e.g. `hearth completions zsh >
    /// ~/.zfunc/_hearth` (bash, elvish, fish, powershell, zsh).
    Completions { shell: clap_complete::Shell },
    /// Serve these commands as tools to AI agents over the Model Context
    /// Protocol on stdin/stdout.
    Mcp,
}

fn execute(cli: &Cli) -> Result<String, String> {
    match &cli.command {
        Command::Doctor(args) => doctor::run(args, cli.json),
        Command::Incidents(args) => incidents::run(args, cli.json),
        Command::Fit(args) => fit::run(args, cli.json),
        Command::Models(args) => models::run(args, cli.json),
        Command::Ports(args) => ports::run(args, cli.json),
        Command::Log(args) => history::log(args, cli.json),
        Command::Diff(args) => history::diff(args, cli.json),
        Command::Show(args) => history::show(args, cli.json),
        Command::Follow(args) => history::follow(args, cli.json),
        Command::Tag(args) => history::tag(args, cli.json),
        Command::Note(args) => note::run(args, cli.json),
        Command::Run(args) => run::run(args, cli.json),
        Command::Watch(args) => watch::run(args, cli.json),
        Command::Config(args) => config_cmd::run(args, cli.json),
        Command::Bundle(args) => bundle::run(args, cli.json),
        Command::Workflows(args) => workflows::run(args, cli.json),
        Command::Completions { shell } => {
            let mut script = Vec::new();
            clap_complete::generate(
                *shell,
                &mut <Cli as clap::CommandFactory>::command(),
                "hearth",
                &mut script,
            );
            String::from_utf8(script).map_err(|e| e.to_string())
        }
        #[cfg(unix)]
        Command::Service(args) => service::run(args, cli.json),
        #[cfg(unix)]
        Command::Status(args) => control::status(args, cli.json),
        #[cfg(unix)]
        Command::Restart(args) => control::order("restart", args, cli.json),
        #[cfg(unix)]
        Command::Stop(args) => control::order("stop", args, cli.json),
        #[cfg(unix)]
        Command::Start(args) => control::order("start", args, cli.json),
        #[cfg(unix)]
        Command::Top(args) => top::run(args, cli.json),
        #[cfg(unix)]
        Command::Logs(args) => control::logs(args, cli.json),
        #[cfg(unix)]
        Command::Reload(args) => control::reload(args, cli.json),
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
            match out.write_all(text.as_bytes()).and_then(|()| out.flush()) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => std::process::exit(0),
                Err(e) => {
                    eprintln!("hearth: writing the output failed: {e}");
                    std::process::exit(1);
                }
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
