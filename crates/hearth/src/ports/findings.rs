//! What `hearth ports` warns about: runtimes that stopped answering or are
//! unhealthy, servers configured past the device, and models only partly
//! on the accelerator.

use serde_json::Value;

use super::{Finding, Port, explain, is_runtime};
use crate::units::bytes;
use hearth_runtime::fingerprint::Identity;

/// Ollama models loaded only partly onto the accelerator: Ollama reports a
/// loaded model's `size` and how much of it sits in GPU memory
/// (`size_vram`). Less than all of it means the rest runs on the CPU, many
/// times slower, because the accelerator had no room for it.
pub(super) fn partially_offloaded(
    ports: &[Port],
    has_accelerator: impl FnOnce() -> bool,
) -> Vec<Finding> {
    let mut out = Vec::new();
    // Asked only when a model sits wholly on the CPU.
    let has_accelerator = std::cell::LazyCell::new(has_accelerator);
    for p in ports {
        let Identity::Ollama { loaded, .. } = &p.fingerprint.identity else {
            continue;
        };
        for m in loaded {
            let (Some(size), Some(vram)) = (m.size_bytes, m.size_vram_bytes) else {
                continue;
            };
            // Wholly on the CPU is no room at all, unless the host has no
            // accelerator. (A deliberate num_gpu 0 looks the same.)
            if vram >= size || (vram == 0 && !*has_accelerator) {
                continue;
            }
            out.push(Finding {
                severity: "warn",
                code: "partial_offload",
                pid: p.pid,
                model: Some(m.name.clone()),
                message: format!(
                    "{} on {}: only {} of {} is on the accelerator; the rest runs on the CPU",
                    m.name,
                    p.fingerprint.addr,
                    bytes(vram),
                    bytes(size)
                ),
                remediation: format!(
                    "there was no room for all of {}: unload other models, or lower its num_ctx (hearth fit {} --ctx N shows what fits)",
                    m.name, m.name
                ),
            });
        }
    }
    out
}

/// llama.cpp servers configured past what the device holds: their model
/// at their context and cache types, as `hearth fit --port` computes it
/// from the server's own `/props` and command line. Such a server runs
/// until its context fills, then runs out of accelerator memory.
pub(super) fn oversized(ports: &[Port]) -> Vec<Finding> {
    let mut seen = std::collections::BTreeSet::new();
    ports
        .iter()
        .filter(|p| seen.insert(p.pid))
        .filter_map(|p| {
            // What this exact process runs: its model and context from the
            // answer already probed, its cache types from its own command
            // line. Nothing is probed again and no other process is read.
            let Identity::LlamaCpp {
                model: Some(model),
                n_ctx: Some(n_ctx),
                slots,
                ..
            } = &p.fingerprint.identity
            else {
                return None;
            };
            let ctx = n_ctx.saturating_mul(slots.unwrap_or(1).max(1));
            let argv = hearth_sense::process_args(p.pid).ok()?;
            let (k, v) = crate::server_args::cache_types_from_args(&argv)?;
            let mut line: Vec<String> = [
                "hearth",
                "--json",
                "fit",
                "--ctx",
                &ctx.to_string(),
                "--cache-type",
                k.name(),
                "--cache-type-v",
                v.name(),
            ]
            .iter()
            .map(|s| (*s).to_string())
            .collect();
            // Its projector is on the accelerator beside the model.
            // One not readable from here (relative to the server's own
            // directory) cannot be counted.
            if let Some(projector) = crate::server_args::served_projector(&argv, std::path::Path::new(model)) {
                line.push(format!("--mmproj={}", projector.display()));
            }
            // So is a draft model, with its own context and cache.
            if let Some(d) = crate::draft::from_args(&argv).filter(|d| d.model.is_file()) {
                line.push(format!("--draft={}", d.model.display()));
                line.extend(d.ctx.map(|c| format!("--draft-ctx={c}")));
                line.extend(d.cache.map(|(k, _)| format!("--draft-cache-type={}", k.name())));
            }
            line.extend(["--".to_string(), model.clone()]);
            let r: Value = serde_json::from_str(&crate::dispatch(&line).ok()?).ok()?;
            if r["fits_device"] != false {
                return None;
            }
            let types = |key: &str| -> Vec<String> {
                r[key]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|o| o[0].as_str().map(str::to_string))
                    .collect()
            };
            let (kinds, values) = (types("fits_device_with"), types("fits_device_with_values"));
            Some(Finding {
                code: "oversized",
                pid: p.pid,
                model: None,
                severity: "warn",
                message: format!(
                    "{} (pid {}) on {} is configured past the device: its context needs {} of {} ({})",
                    p.process,
                    p.pid,
                    p.fingerprint.addr,
                    bytes(r["total_bytes"].as_u64().unwrap_or_default()),
                    bytes(r["ceiling"]["bytes"].as_u64().unwrap_or_default()),
                    r["ceiling"]["source"].as_str().unwrap_or("device")
                ),
                remediation: format!(
                    "it will run out of accelerator memory as its context fills; {}{}{}",
                    r["max_ctx_device"].as_u64().map_or_else(
                        || "the weights alone do not fit: use a smaller model or quantization".to_string(),
                        |n| format!("restart it with -c at most {n}"),
                    ),
                    if kinds.is_empty() {
                        String::new()
                    } else {
                        format!(", or keep the context with -ctk/-ctv {}", kinds.join(" / "))
                    },
                    // All of them: the largest has the least headroom, which
                    // compute buffers may take when they were not measured.
                    if values.is_empty() {
                        String::new()
                    } else {
                        format!(
                            ", or keep -ctk {} and set only -ctv {} (with -fa on)",
                            r["cache_type_k"].as_str().unwrap_or("as is"),
                            values.join(" / ")
                        )
                    }
                ),
            })
        })
        .collect()
}

pub(super) fn findings(ports: &[Port]) -> Vec<Finding> {
    // Silence from a process that serves a model on another port is a likely
    // wedge; silence from anything else may just be a non-HTTP protocol.
    let runtime_pids: std::collections::BTreeSet<i32> = ports
        .iter()
        .filter(|p| is_runtime(&p.fingerprint.identity))
        .map(|p| p.pid)
        .collect();
    let mut out = Vec::new();
    for p in ports {
        if let Identity::Unresponsive { waited_ms } = &p.fingerprint.identity {
            out.push(Finding {
                code: "unresponsive",
                pid: p.pid,
                model: None,
                severity: if runtime_pids.contains(&p.pid) {
                    "warn"
                } else {
                    "info"
                },
                message: format!(
                    "{} (pid {}) accepts connections on {} but did not answer within {waited_ms} ms",
                    p.process, p.pid, p.fingerprint.addr
                ),
                remediation: format!(
                    "if {} is a model server, it may be wedged: check its log; restart it if it stays silent",
                    p.process
                ),
            });
        }
        if p.slow {
            out.push(Finding {
                code: "slow",
                pid: p.pid,
                model: None,
                severity: "warn",
                message: format!(
                    "{} (pid {}) on {} answered only after its learned deadline ({})",
                    p.process,
                    p.pid,
                    p.fingerprint.addr,
                    explain(p.deadline)
                ),
                remediation: format!(
                    "{} is much slower than it has been: check memory pressure (hearth doctor) and its log; a server that keeps slowing may be about to wedge",
                    p.process
                ),
            });
        }
        match &p.fingerprint.identity {
            Identity::LlamaCpp { health, .. } if health != "ok" => out.push(Finding {
                code: "not_ready",
                pid: p.pid,
                model: None,
                severity: "info",
                message: format!(
                    "llama-server on {} is not ready: {health}",
                    p.fingerprint.addr
                ),
                remediation: "a 503 while loading is normal; if it persists, read the server log"
                    .into(),
            }),
            _ => {}
        }
    }
    out
}
