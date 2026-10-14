//! `hearth-probe`: records host and per-process memory readings as JSON lines,
//! so the admission model can be validated against what the kernel actually
//! did before any of it is trusted.
//!
//! ```text
//! hearth-probe sample [--interval-ms N] [--duration-s S] [--match SUBSTR]... [--pid PID]... [--pids-from FILE] [--out FILE]
//! hearth-probe mark --out FILE LABEL [KEY=VALUE]...
//! hearth-probe report FILE
//! ```

use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

mod report;

const USAGE: &str = "usage:
  hearth-probe sample [--interval-ms N] [--duration-s S] [--match SUBSTR]... [--pid PID]... [--pids-from FILE] [--out FILE]
      Sample host memory and the footprint of every process whose name or executable
      path contains SUBSTR (re-resolved each tick, so restarts are followed), whose pid
      is PID, or whose pid is listed one per line in FILE (re-read each tick).
      --interval-ms defaults to 100. Runs until --duration-s or Ctrl-C.
  hearth-probe mark --out FILE LABEL [KEY=VALUE]...
      Append a timestamped marker line to FILE.
  hearth-probe report FILE
      Evaluate a scripts/p0-gate.sh recording against the P0 criteria.";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("sample") => parse_sample(&args[1..]).and_then(|o| sample(&o)),
        Some("mark") => mark(&args[1..]),
        Some("report") => match args.get(1) {
            Some(file) => report::run(file).map(|text| print!("{text}")),
            None => Err(USAGE.to_string()),
        },
        _ => Err(USAGE.to_string()),
    };
    if let Err(message) = result {
        eprintln!("{message}");
        std::process::exit(2);
    }
}

struct SampleOpts {
    interval: Duration,
    duration: Option<Duration>,
    matches: Vec<String>,
    pids: Vec<i32>,
    pids_from: Option<String>,
    out: Option<String>,
}

fn parse_sample(args: &[String]) -> Result<SampleOpts, String> {
    let mut opts = SampleOpts {
        interval: Duration::from_millis(100),
        duration: None,
        matches: Vec::new(),
        pids: Vec::new(),
        pids_from: None,
        out: None,
    };
    let mut it = args.iter();
    while let Some(flag) = it.next() {
        let mut value = || {
            it.next()
                .cloned()
                .ok_or_else(|| format!("{flag} needs a value\n{USAGE}"))
        };
        let number = |v: String| v.parse::<u64>().map_err(|e| format!("{flag} {v}: {e}"));
        match flag.as_str() {
            "--interval-ms" => opts.interval = Duration::from_millis(number(value()?)?),
            "--duration-s" => opts.duration = Some(Duration::from_secs(number(value()?)?)),
            "--match" => opts.matches.push(value()?),
            "--pid" => opts
                .pids
                .push(value()?.parse().map_err(|e| format!("--pid: {e}"))?),
            "--pids-from" => opts.pids_from = Some(value()?),
            "--out" => opts.out = Some(value()?),
            other => return Err(format!("unknown flag {other}\n{USAGE}")),
        }
    }
    if opts.interval.is_zero() {
        return Err("--interval-ms must be > 0".into());
    }
    Ok(opts)
}

fn open_out(path: Option<&str>) -> Result<Box<dyn Write>, String> {
    match path {
        None => Ok(Box::new(std::io::stdout().lock())),
        Some(p) => OpenOptions::new()
            .create(true)
            .append(true)
            .open(p)
            .map(|f| Box::new(f) as Box<dyn Write>)
            .map_err(|e| format!("open {p}: {e}")),
    }
}

/// One line per write: appends of a single line are atomic, so `mark` can
/// write into the same file while `sample` runs.
fn emit(out: &mut dyn Write, value: &Value) -> Result<(), String> {
    let mut line = value.to_string();
    line.push('\n');
    out.write_all(line.as_bytes())
        .and_then(|()| out.flush())
        .map_err(|e| format!("write: {e}"))
}

fn sample(opts: &SampleOpts) -> Result<(), String> {
    let mut out = open_out(opts.out.as_deref())?;
    let host = hearth_sense::host_memory().map_err(|e| e.to_string())?;
    let accelerators = hearth_sense::accelerators().map_err(|e| e.to_string())?;
    emit(
        out.as_mut(),
        &json!({
            "kind": "meta",
            "t_ms": host.taken_at_ms,
            "total_bytes": host.total_bytes,
            "accelerators": accelerators,
            "interval_ms": opts.interval.as_millis(),
            "matches": opts.matches,
            "pids": opts.pids,
        }),
    )?;
    let started = Instant::now();
    let mut next = started;
    loop {
        if opts.duration.is_some_and(|d| started.elapsed() >= d) {
            return Ok(());
        }
        let line = take_sample(opts);
        emit(out.as_mut(), &line)?;
        next += opts.interval;
        let now = Instant::now();
        if next > now {
            std::thread::sleep(next - now);
        } else {
            // Fell behind; do not burst to catch up.
            next = now;
        }
    }
}

fn take_sample(opts: &SampleOpts) -> Value {
    let t0 = Instant::now();
    let host = hearth_sense::host_memory();
    let host_us = t0.elapsed().as_micros();

    let t1 = Instant::now();
    let mut targets: BTreeMap<i32, Option<hearth_sense::ProcessEntry>> =
        opts.pids.iter().map(|&pid| (pid, None)).collect();
    if let Some(file) = &opts.pids_from {
        // A missing file just means nothing has been started yet.
        let listed = std::fs::read_to_string(file).unwrap_or_default();
        targets.extend(
            listed
                .lines()
                .filter_map(|l| l.trim().parse::<i32>().ok())
                .map(|pid| (pid, None)),
        );
    }
    let mut errors = Vec::new();
    if !opts.matches.is_empty() {
        match hearth_sense::list_processes() {
            Ok(list) => targets.extend(
                list.into_iter()
                    .filter(|p| opts.matches.iter().any(|m| p.matches(m)))
                    .map(|p| (p.pid, Some(p))),
            ),
            Err(e) => errors.push(e.to_string()),
        }
    }
    let procs: Vec<Value> = targets
        .into_iter()
        .map(|(pid, entry)| match hearth_sense::process_footprint(pid) {
            Ok(fp) => json!({ "pid": pid, "proc": entry, "fp": fp }),
            Err(e) => json!({ "pid": pid, "proc": entry, "error": e.to_string() }),
        })
        .collect();
    let procs_us = t1.elapsed().as_micros();

    match host {
        Ok(h) => json!({
            "kind": "sample",
            "t_ms": h.taken_at_ms,
            "host": h,
            "procs": procs,
            "host_us": host_us,
            "procs_us": procs_us,
            "errors": errors,
        }),
        Err(e) => json!({
            "kind": "sample",
            "host_error": e.to_string(),
            "procs": procs,
            "errors": errors,
        }),
    }
}

fn mark(args: &[String]) -> Result<(), String> {
    let mut it = args.iter();
    let mut out = None;
    let mut label = None;
    let mut fields = serde_json::Map::new();
    while let Some(arg) = it.next() {
        if arg == "--out" {
            out = it.next().cloned();
        } else if label.is_none() {
            label = Some(arg.clone());
        } else if let Some((k, v)) = arg.split_once('=') {
            let value = v
                .parse::<u64>()
                .map_or_else(|_| Value::String(v.to_string()), Value::from);
            fields.insert(k.to_string(), value);
        } else {
            return Err(format!("expected KEY=VALUE, got {arg}\n{USAGE}"));
        }
    }
    let (Some(out), Some(label)) = (out, label) else {
        return Err(USAGE.to_string());
    };
    let t_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| e.to_string())?
        .as_millis();
    let mut line = json!({ "kind": "mark", "t_ms": t_ms, "label": label });
    if let Value::Object(map) = &mut line {
        map.extend(fields);
    }
    emit(open_out(Some(&out))?.as_mut(), &line)
}
