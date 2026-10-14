//! `hearth-probe report`: evaluates a `scripts/p0-gate.sh` recording against
//! the P0 acceptance criteria and prints the evidence for each verdict.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use serde_json::{Value, json};

const GIB: f64 = 1024.0 * 1024.0 * 1024.0;
const MIB: f64 = 1024.0 * 1024.0;

/// Acceptance thresholds, read from the file ADR-000 points to. None of them
/// live in code.
#[derive(serde::Deserialize)]
struct Criteria {
    footprint_vs_allocated_max_rel: f64,
    host_vs_footprint_rel: f64,
    host_vs_footprint_abs_bytes: f64,
    load_visibility_min_rel: f64,
    reclaim_min_rel: f64,
    noise_sigma_multiplier: f64,
    naive_overstatement_min_rel: f64,
}

struct Sample {
    t: u64,
    total: f64,
    used: f64,
    naive_used: Option<f64>,
    procs: BTreeMap<i64, f64>,
    host_us: f64,
    procs_us: f64,
}

struct Mark {
    t: u64,
    label: String,
    fields: Value,
}

struct Recording {
    samples: Vec<Sample>,
    marks: Vec<Mark>,
    meta: Value,
}

fn parse(text: &str) -> Result<Recording, String> {
    let mut rec = Recording {
        samples: Vec::new(),
        marks: Vec::new(),
        meta: Value::Null,
    };
    for (n, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let v: Value = serde_json::from_str(line).map_err(|e| format!("line {}: {e}", n + 1))?;
        let t = v["t_ms"].as_u64().unwrap_or(0);
        match v["kind"].as_str() {
            Some("meta") => rec.meta = v,
            Some("mark") => rec.marks.push(Mark {
                t,
                label: v["label"].as_str().unwrap_or_default().to_string(),
                fields: v,
            }),
            Some("sample") if v.get("host").is_some() => rec.samples.push(sample(&v, t)),
            _ => {}
        }
    }
    if rec.samples.is_empty() {
        return Err("no samples in recording".into());
    }
    Ok(rec)
}

#[allow(clippy::cast_precision_loss)]
fn sample(v: &Value, t: u64) -> Sample {
    let host = &v["host"];
    let num = |x: &Value| x.as_u64().map_or(0.0, |n| n as f64);
    let total = num(&host["total_bytes"]);
    let raw = &host["raw"];
    let naive_used = raw["free"]
        .as_u64()
        .zip(raw["page_size"].as_u64())
        .map(|(free, page)| total - (free * page) as f64);
    Sample {
        t,
        total,
        used: total - num(&host["available_bytes"]),
        naive_used,
        procs: v["procs"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|p| Some((p["pid"].as_i64()?, num(&p["fp"]["footprint_bytes"]))))
            .collect(),
        host_us: num(&v["host_us"]),
        procs_us: num(&v["procs_us"]),
    }
}

fn median(mut xs: Vec<f64>) -> Option<f64> {
    if xs.is_empty() {
        return None;
    }
    xs.sort_by(f64::total_cmp);
    Some(xs[xs.len() / 2])
}

#[allow(clippy::cast_precision_loss)]
fn stddev(xs: &[f64]) -> Option<f64> {
    if xs.len() < 2 {
        return None;
    }
    let n = xs.len() as f64;
    let mean = xs.iter().sum::<f64>() / n;
    Some((xs.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / (n - 1.0)).sqrt())
}

impl Recording {
    fn mark(&self, label: &str) -> Option<&Mark> {
        self.marks.iter().find(|m| m.label == label)
    }

    fn between(&self, from: u64, to: u64) -> impl Iterator<Item = &Sample> {
        self.samples
            .iter()
            .filter(move |s| s.t >= from && s.t <= to)
    }

    /// Start of the phase that ends at `t`: the previous mark. Medians are
    /// taken over a whole phase, so the window comes from the experiment's own
    /// structure rather than a fixed duration.
    fn phase_start(&self, t: u64) -> u64 {
        self.marks
            .iter()
            .map(|m| m.t)
            .filter(|&m| m < t)
            .max()
            .unwrap_or(0)
    }

    fn used_before(&self, t: u64) -> Option<f64> {
        median(
            self.between(self.phase_start(t), t)
                .map(|s| s.used)
                .collect(),
        )
    }

    fn footprint_before(&self, pid: i64, t: u64) -> Option<f64> {
        median(
            self.between(self.phase_start(t), t)
                .filter_map(|s| s.procs.get(&pid).copied())
                .collect(),
        )
    }
}

/// Sums llama.cpp's "... buffer size = N MiB" log lines by kind.
fn log_buffers(path: &str) -> BTreeMap<&'static str, f64> {
    let mut sums = BTreeMap::new();
    let Ok(text) = std::fs::read_to_string(path) else {
        return sums;
    };
    for line in text.lines() {
        let lower = line.to_ascii_lowercase();
        let Some((_, rest)) = lower.split_once("buffer size =") else {
            continue;
        };
        let Some(mib) = rest
            .split_whitespace()
            .next()
            .and_then(|n| n.parse::<f64>().ok())
        else {
            continue;
        };
        let kind = if lower.contains("kv") {
            "kv"
        } else if lower.contains("compute") {
            "compute"
        } else if lower.contains("model") {
            "model"
        } else {
            "other"
        };
        *sums.entry(kind).or_insert(0.0) += mib * MIB;
    }
    sums
}

struct Verdict {
    id: &'static str,
    pass: Option<bool>,
    evidence: String,
}

#[allow(clippy::cast_precision_loss, clippy::too_many_lines)]
pub fn run(path: &str, criteria_path: &str) -> Result<String, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("read {path}: {e}"))?;
    let rec = parse(&text)?;
    let c: Criteria = std::fs::read_to_string(criteria_path)
        .map_err(|e| format!("read {criteria_path}: {e}"))
        .and_then(|t| serde_json::from_str(&t).map_err(|e| format!("{criteria_path}: {e}")))?;
    let mut verdicts = Vec::new();
    let mut servers = Vec::new();

    for label in ["a", "b"] {
        let (Some(start), Some(settled)) = (
            rec.mark(&format!("{label}_start")),
            rec.mark(&format!("{label}_settled")),
        ) else {
            continue;
        };
        let pid = settled.fields["pid"].as_i64().unwrap_or(-1);
        let file = start.fields["file_bytes"].as_f64();
        let buffers = log_buffers(start.fields["log"].as_str().unwrap_or_default());
        let expected: f64 = buffers.values().sum();
        let footprint = rec.footprint_before(pid, settled.t);
        let host_delta = rec
            .used_before(settled.t)
            .zip(rec.used_before(start.t))
            .map(|(after, before)| after - before);
        let reclaim = rec.mark(&format!("{label}_unload")).and_then(|unload| {
            let before = rec.used_before(unload.t)?;
            let fp = footprint?;
            let target = before - c.reclaim_min_rel * fp;
            let end = rec
                .mark(&format!("{label}_reclaimed"))
                .map_or(u64::MAX, |m| m.t);
            let hit = rec.between(unload.t, end).find(|s| s.used <= target);
            Some((hit.map(|s| s.t - unload.t), before))
        });
        servers.push(json!({
            "label": label,
            "pid": pid,
            "file_bytes": file,
            "log_buffers_bytes": buffers,
            "footprint_bytes": footprint,
            "host_used_delta_bytes": host_delta,
            "reclaim_90pct_ms": reclaim.and_then(|r| r.0),
        }));
        if let (Some(fp), true) = (footprint, expected > 0.0) {
            let err = (fp - expected).abs() / expected;
            verdicts.push(Verdict {
                id: "a",
                pass: Some(err <= c.footprint_vs_allocated_max_rel),
                evidence: format!(
                    "{label}: footprint {:.2} GiB vs log buffers {:.2} GiB (file {}): {:.1}% off",
                    fp / GIB,
                    expected / GIB,
                    file.map_or("unknown".into(), |f| format!("{:.2} GiB", f / GIB)),
                    err * 100.0
                ),
            });
        }
        if let (Some(fp), Some(delta)) = (footprint, host_delta) {
            let tol = (c.host_vs_footprint_rel * fp).max(c.host_vs_footprint_abs_bytes);
            verdicts.push(Verdict {
                id: "b",
                pass: Some((delta - fp).abs() <= tol),
                evidence: format!(
                    "{label}: host used +{:.2} GiB vs footprint {:.2} GiB (tolerance {:.2} GiB)",
                    delta / GIB,
                    fp / GIB,
                    tol / GIB
                ),
            });
            verdicts.push(Verdict {
                id: "c-load",
                pass: Some(delta >= c.load_visibility_min_rel * fp),
                evidence: format!(
                    "{label}: available fell {:.0}% of footprint",
                    delta / fp * 100.0
                ),
            });
        }
        if let Some((hit, _)) = reclaim {
            let window = rec
                .mark(&format!("{label}_unload"))
                .zip(rec.mark(&format!("{label}_reclaimed")))
                .map_or(0, |(u, r)| r.t - u.t);
            verdicts.push(Verdict {
                id: "c-reclaim",
                pass: Some(hit.is_some()),
                evidence: hit.map_or_else(
                    || {
                        format!(
                            "{label}: {:.0}% not reclaimed within {window} ms",
                            c.reclaim_min_rel * 100.0
                        )
                    },
                    |ms| {
                        format!(
                            "{label}: {:.0}% reclaimed after {ms} ms",
                            c.reclaim_min_rel * 100.0
                        )
                    },
                ),
            });
        }
    }

    if let Some(fill) = rec.mark("fill_start")
        && let Some(done) = rec.mark("fill_settled")
    {
        let pid = fill.fields["pid"].as_i64().unwrap_or(-1);
        if let (Some(before), Some(after)) = (
            rec.footprint_before(pid, fill.t),
            rec.footprint_before(pid, done.t),
        ) {
            servers.push(json!({ "fill_growth_bytes": after - before }));
            verdicts.push(Verdict {
                id: "kv-upfront",
                pass: None,
                evidence: format!(
                    "a: footprint grew {:.0} MiB while filling the context (informs per-request KV reservation)",
                    (after - before) / MIB
                ),
            });
        }
    }

    let reserve_floor = rec.meta["accelerators"][0]["recommended_working_set_bytes"]
        .as_f64()
        .map(|ceiling| rec.samples[0].total - ceiling);
    if let (Some(start), Some(end)) = (rec.mark("baseline_start"), rec.mark("baseline_end")) {
        let used: Vec<f64> = rec.between(start.t, end.t).map(|s| s.used).collect();
        if let Some(sd) = stddev(&used) {
            let drift = used.iter().copied().fold(f64::MIN, f64::max)
                - used.iter().copied().fold(f64::MAX, f64::min);
            verdicts.push(Verdict {
                id: "d",
                pass: reserve_floor.map(|floor| c.noise_sigma_multiplier * sd < floor),
                evidence: format!(
                    "idle σ {:.0} MiB, drift {:.0} MiB over {} samples; reserve floor (memsize − Metal working set) {}",
                    sd / MIB,
                    drift / MIB,
                    used.len(),
                    reserve_floor.map_or("unknown".into(), |f| format!("{:.2} GiB", f / GIB))
                ),
            });
        }
        let naive: Vec<f64> = rec
            .between(start.t, end.t)
            .filter_map(|s| Some(s.naive_used? / s.total))
            .collect();
        let real: Vec<f64> = rec
            .between(start.t, end.t)
            .map(|s| s.used / s.total)
            .collect();
        if let (Some(n), Some(r)) = (median(naive), median(real)) {
            verdicts.push(Verdict {
                id: "f",
                pass: Some(n - r > c.naive_overstatement_min_rel),
                evidence: format!(
                    "free-pages-only reads {:.1}% used; Activity Monitor formula reads {:.1}%",
                    n * 100.0,
                    r * 100.0
                ),
            });
        }
    }
    for (id, what) in [("e", "induced-pressure phase"), ("g", "overload phase")] {
        verdicts.push(Verdict {
            id,
            pass: None,
            evidence: format!("{what} not in this recording"),
        });
    }

    let host_us = median(rec.samples.iter().map(|s| s.host_us).collect()).unwrap_or(0.0);
    let procs_us = median(rec.samples.iter().map(|s| s.procs_us).collect()).unwrap_or(0.0);

    let mut out = String::new();
    let _ = writeln!(out, "| criterion | verdict | evidence |\n|---|---|---|");
    for v in &verdicts {
        let verdict = match v.pass {
            Some(true) => "pass",
            Some(false) => "FAIL",
            None => "n/a",
        };
        let _ = writeln!(out, "| {} | {verdict} | {} |", v.id, v.evidence);
    }
    let _ = writeln!(
        out,
        "\nsensing cost (median): host {host_us:.0} µs, processes {procs_us:.0} µs; {} samples",
        rec.samples.len()
    );
    let _ = writeln!(
        out,
        "\n```json\n{}\n```",
        serde_json::to_string_pretty(&json!({ "servers": servers })).unwrap_or_default()
    );
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn median_and_stddev() {
        assert_eq!(median(vec![3.0, 1.0, 2.0]), Some(2.0));
        assert_eq!(median(vec![]), None);
        let sd = stddev(&[2.0, 4.0, 4.0, 4.0, 5.0, 5.0, 7.0, 9.0]).unwrap();
        assert!((sd - 2.138).abs() < 0.001);
    }

    #[test]
    fn llama_log_buffers_are_summed_by_kind() {
        let dir = std::env::temp_dir().join(format!("hearth-probe-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("a.log");
        std::fs::write(
            &log,
            "load_tensors:   Metal_Mapped model buffer size =   604.50 MiB\n\
             load_tensors:    CPU_Mapped model buffer size =    10.00 MiB\n\
             llama_kv_cache:      Metal KV buffer size =   448.00 MiB\n\
             llama_context:      Metal compute buffer size =   300.75 MiB\n",
        )
        .unwrap();
        let sums = log_buffers(log.to_str().unwrap());
        assert!((sums["model"] - 614.5 * MIB).abs() < 1.0);
        assert!((sums["kv"] - 448.0 * MIB).abs() < 1.0);
        assert!((sums["compute"] - 300.75 * MIB).abs() < 1.0);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
