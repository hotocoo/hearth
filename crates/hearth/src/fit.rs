//! `hearth fit`: does a GGUF model at a given context fit on this machine?
//!
//! Inputs are the model file and the runtime settings the caller intends to
//! use; outputs are measured (memory, device limits) or computed from the
//! model's own metadata. Runtime compute buffers are not known until a model
//! has been loaded once, and the report says so instead of guessing them.

use std::fmt::Write as _;
use std::path::PathBuf;

use hearth_sense::kv::{CacheType, Confidence, Geometry, Swa};
use hearth_sense::{PressureLevel, gguf};
use serde::Serialize;

use crate::units::bytes;

#[derive(clap::Args)]
pub struct Args {
    /// GGUF file (any shard of a split model); a Hugging Face or LM Studio
    /// name such as `org/repo:Q6_K` or `org/repo/file.gguf`, found in their
    /// download stores; or an Ollama model name such as `qwen3:8b`, found
    /// in Ollama's store. Nothing is downloaded or loaded.
    #[arg(required_unless_present = "port")]
    model: Option<PathBuf>,
    /// Instead of a model: the port of a running server on this machine,
    /// asked which model it serves (llama.cpp reports its file; Ollama the
    /// model loaded, given with --model when it has several).
    #[arg(long)]
    port: Option<u16>,
    /// Total tokens of KV cache the runtime will allocate (llama.cpp `-c`;
    /// for Ollama, `num_ctx` × `OLLAMA_NUM_PARALLEL`).
    #[arg(long, required_unless_present = "port")]
    ctx: Option<u64>,
    /// KV cache type for keys, and for values unless --cache-type-v is given
    /// (llama.cpp --cache-type-k, Ollama `OLLAMA_KV_CACHE_TYPE`).
    #[arg(long, value_parser = parse_cache)]
    cache_type: CacheType,
    #[arg(long, value_parser = parse_cache)]
    cache_type_v: Option<CacheType>,
    /// Parallel sequences; with --ubatch, bounds sliding-window caches.
    #[arg(long, requires = "ubatch")]
    parallel: Option<u64>,
    /// Physical batch size (llama.cpp -ub); with --parallel, bounds
    /// sliding-window caches.
    #[arg(long, requires = "parallel")]
    ubatch: Option<u64>,
}

fn parse_cache(s: &str) -> Result<CacheType, String> {
    CacheType::parse(s).ok_or_else(|| {
        format!("unknown cache type {s}; one of f32 f16 bf16 q8_0 q5_1 q5_0 q4_1 q4_0 iq4_nl")
    })
}

#[derive(Serialize)]
struct Report {
    model: PathBuf,
    files: Vec<PathBuf>,
    weights_bytes: u64,
    ctx: u64,
    kv_bytes: u64,
    total_bytes: u64,
    excludes: &'static str,
    geometry: Geometry,
    ceiling: Option<Ceiling>,
    available_bytes: u64,
    pressure: Option<PressureLevel>,
    fits_device: Option<bool>,
    fits_now: bool,
    /// True until ADR-000's gate confirms that available memory falls by a
    /// loaded model's footprint (weights are memory-mapped and may count as
    /// reclaimable file cache before they are wired).
    fits_now_provisional: bool,
    max_ctx_device: Option<u64>,
    max_ctx_now: Option<u64>,
    notes: Vec<String>,
}

#[derive(Serialize)]
struct Ceiling {
    bytes: u64,
    source: String,
    /// Free on that device now, when it is a discrete GPU whose driver
    /// reports usage: what "fits right now" is checked against there.
    #[serde(skip_serializing_if = "Option::is_none")]
    free_bytes: Option<u64>,
}

/// The model's file: from the server on `port` when given, else the path
/// given, else the Ollama model of that name.
/// The model's file, and the context a running server holds it with when
/// it was asked (`--port`) and says.
fn locate(args: &Args) -> Result<(PathBuf, Option<u64>), String> {
    match (args.port, &args.model) {
        (Some(port), model) => from_server(port, model.as_deref()),
        (None, Some(model)) => from_name(model).map(|p| (p, None)),
        (None, None) => Err("give a model or --port".into()),
    }
}

/// Asks the server on `port` which model it serves, read-only.
fn from_server(
    port: u16,
    model: Option<&std::path::Path>,
) -> Result<(PathBuf, Option<u64>), String> {
    use hearth_runtime::fingerprint::{Identity, fingerprint};
    let config = hearth_core::config::load().map_err(|e| e.to_string())?;
    let addr = std::net::SocketAddr::from((std::net::Ipv4Addr::LOCALHOST, port));
    let identity = fingerprint(
        addr,
        std::time::Duration::from_millis(config.discovery.first_contact_timeout_ms),
        config.discovery.max_probe_response_bytes,
    )
    .identity;
    match identity {
        Identity::LlamaCpp {
            model: Some(path),
            n_ctx,
            ..
        } if model.is_none() => Ok((PathBuf::from(path), n_ctx)),
        Identity::Ollama { loaded, .. } => {
            let names: Vec<String> = loaded.iter().map(|l| l.name.clone()).collect();
            let name = match (model, names.as_slice()) {
                (Some(m), _) => m.to_string_lossy().into_owned(),
                (None, [only]) => only.clone(),
                (None, []) => {
                    return Err(format!(
                        "Ollama on {addr} has no model loaded; name one with --model"
                    ));
                }
                (None, several) => {
                    return Err(format!(
                        "Ollama on {addr} has several models loaded ({}); pick one with --model",
                        several.join(", ")
                    ));
                }
            };
            let ctx = loaded
                .iter()
                .find(|l| l.name == name)
                .and_then(|l| l.context_length);
            hearth_runtime::ollama_store::weights(&name).map(|p| (p, ctx))
        }
        Identity::LmStudio { models } => {
            let loaded: Vec<&str> = models
                .iter()
                .filter(|(_, state)| state == "loaded")
                .map(|(m, _)| m.as_str())
                .collect();
            let name = match (model, loaded.as_slice()) {
                (Some(m), _) => m.to_string_lossy().into_owned(),
                (None, [only]) => (*only).to_string(),
                (None, []) => {
                    return Err(format!(
                        "LM Studio on {addr} has no model loaded; name one with --model"
                    ));
                }
                (None, several) => {
                    return Err(format!(
                        "LM Studio on {addr} has several models loaded ({}); pick one with --model",
                        several.join(", ")
                    ));
                }
            };
            hearth_runtime::model_stores::find_in(&name, hearth_runtime::model_stores::Store::LmStudio)
                .unwrap_or_else(|| {
                    Err(format!(
                        "{name}: not found in LM Studio's models directory; the id LM Studio reports may differ from its folder: pass the file with --model"
                    ))
                })
                .map(|p| (p, None))
        }
        Identity::LlamaCpp { .. } if model.is_some() => Err(
            "--model and --port name the model twice: a llama.cpp server reports its own".into(),
        ),
        other => Err(format!(
            "{addr} does not report a model file: {}",
            serde_json::to_string(&other).unwrap_or_default()
        )),
    }
}

/// The path given, or the Ollama model of that name.
fn from_name(model: &std::path::Path) -> Result<PathBuf, String> {
    if model.exists() {
        return Ok(model.to_path_buf());
    }
    let name = model.to_string_lossy();
    // org/repo names live in the Hugging Face cache or LM Studio first:
    // that is where `llama-server -hf` and LM Studio put them.
    if let Some(found) = hearth_runtime::model_stores::find(&name) {
        return found;
    }
    if hearth_runtime::ollama_store::parse_name(&name).is_none() {
        return Err(format!("{}: no such file", model.display()));
    }
    hearth_runtime::ollama_store::weights(&name).map_err(|e| {
        format!(
            "{} is not a file, and as an Ollama model: {e}",
            model.display()
        )
    })
}

pub fn run(args: &Args, json: bool) -> Result<String, String> {
    let (model, server_ctx) = locate(args)?;
    let ctx = args
        .ctx
        .or(server_ctx)
        .ok_or("the server does not say its context; give --ctx (tokens of KV cache to fit)")?;
    let files = gguf::model_files(&model);
    let weights_bytes = files
        .iter()
        .map(|f| {
            std::fs::metadata(f)
                .map(|m| m.len())
                .map_err(|e| format!("{}: {e}", f.display()))
        })
        .sum::<Result<u64, String>>()?;
    let header = gguf::read_header(&files[0]).map_err(|e| e.to_string())?;
    let geometry = Geometry::from_header(&header).map_err(|e| e.to_string())?;
    let (k, v) = (
        args.cache_type,
        args.cache_type_v.unwrap_or(args.cache_type),
    );
    let swa = args
        .parallel
        .zip(args.ubatch)
        .map(|(sequences, ubatch)| Swa { sequences, ubatch });
    let kv_bytes = geometry.kv_bytes(ctx, k, v, swa);
    let total_bytes = weights_bytes + kv_bytes;

    let host = crate::pressure::host_memory()?;
    let ceiling = device_ceiling()?;
    let under_pressure = matches!(
        host.pressure,
        Some(PressureLevel::Warn | PressureLevel::Critical)
    );
    // On a discrete GPU the model goes into the device's own memory, so
    // what is free there decides; otherwise it is the host's memory.
    let available_now = ceiling
        .as_ref()
        .and_then(|c| c.free_bytes)
        .unwrap_or(host.available_bytes);
    let fits_now = !under_pressure && total_bytes <= available_now;
    // Beyond the trained context a model is not expected to work, so the
    // largest useful context is capped there even when memory allows more.
    let max_ctx = |budget: u64| {
        budget
            .checked_sub(weights_bytes)
            .and_then(|left| geometry.max_tokens(left, k, v, swa))
            .map(|n| geometry.trained_context.map_or(n, |t| n.min(t)))
    };
    let mut notes = geometry.notes.clone();
    if let Some(trained) = geometry.trained_context
        && ctx > trained
    {
        notes.push(format!(
            "requested context {ctx} exceeds the trained context {trained}"
        ));
    }
    notes.push(
        "fits right now is provisional: whether available memory drops by the full weights of a memory-mapped model is what the P0 gate (ADR-000) measures"
            .into(),
    );
    if under_pressure {
        notes.push("memory is under pressure; nothing should be loaded now".into());
    }
    if swa.is_none()
        && geometry
            .layers
            .iter()
            .any(|l| matches!(l, hearth_sense::kv::Layer::Sliding { .. }))
    {
        notes.push("sliding-window layers costed as full; pass --parallel and --ubatch for the bounded size".into());
    }
    let report = Report {
        model: model.clone(),
        files,
        weights_bytes,
        ctx,
        kv_bytes,
        total_bytes,
        excludes: "runtime compute buffers (known after the model's first load)",
        fits_device: ceiling.as_ref().map(|c| total_bytes <= c.bytes),
        max_ctx_device: ceiling.as_ref().and_then(|c| max_ctx(c.bytes)),
        max_ctx_now: if under_pressure {
            None
        } else {
            max_ctx(available_now)
        },
        ceiling,
        available_bytes: available_now,
        pressure: host.pressure,
        fits_now,
        fits_now_provisional: true,
        geometry,
        notes,
    };
    if json {
        crate::to_json(&report)
    } else {
        Ok(render(&report))
    }
}

/// The most one process may place on the accelerator: Metal's recommended
/// working set, or the iogpu wired limit when an override is set and lower.
fn device_ceiling() -> Result<Option<Ceiling>, String> {
    let accelerators = crate::pressure::accelerators()?;
    Ok(accelerators
        .iter()
        .flat_map(|a| {
            [
                a.recommended_working_set_bytes.map(|b| Ceiling {
                    bytes: b,
                    source: format!("{} recommended working set", a.name),
                    free_bytes: None,
                }),
                a.wired_limit_bytes.map(|b| Ceiling {
                    bytes: b,
                    source: "iogpu.wired_limit_mb".into(),
                    free_bytes: None,
                }),
                // A discrete GPU holds the model in its own memory. With
                // several, the smallest is the one a model must fit to be
                // placed on any single device.
                a.total_bytes
                    .filter(|_| !a.unified_memory)
                    .map(|b| Ceiling {
                        bytes: b,
                        source: format!("{} memory", a.name),
                        free_bytes: a.used_bytes.map(|u| b.saturating_sub(u)),
                    }),
            ]
        })
        .flatten()
        .min_by_key(|c| c.bytes))
}

fn render(r: &Report) -> String {
    let mut out = String::new();
    let verdict = |b: bool| if b { "yes" } else { "NO" };
    let _ = writeln!(
        out,
        "{} · {} · {} layers",
        r.model.display(),
        r.geometry.arch,
        r.geometry.layers.len()
    );
    let _ = writeln!(
        out,
        "weights {} + KV {} at {} tokens = {} (excludes {})",
        bytes(r.weights_bytes),
        bytes(r.kv_bytes),
        r.ctx,
        bytes(r.total_bytes),
        r.excludes
    );
    match (&r.ceiling, r.fits_device) {
        (Some(c), Some(fits)) => {
            let _ = writeln!(
                out,
                "fits the device:  {} (ceiling {} from {}; max context {})",
                verdict(fits),
                bytes(c.bytes),
                c.source,
                r.max_ctx_device.map_or("none".into(), |n| n.to_string())
            );
        }
        _ => {
            let _ = writeln!(out, "fits the device:  no accelerator limit known");
        }
    }
    let _ = writeln!(
        out,
        "fits right now:   {} ({} available; max context {}; provisional, see notes)",
        verdict(r.fits_now),
        bytes(r.available_bytes),
        r.max_ctx_now.map_or("none".into(), |n| n.to_string())
    );
    let confidence = match r.geometry.confidence {
        Confidence::High => "high",
        Confidence::Medium => "medium",
        Confidence::Low => "low",
    };
    let _ = writeln!(out, "geometry confidence: {confidence}");
    for n in &r.notes {
        let _ = writeln!(out, "  note: {n}");
    }
    out
}

/// Whether `model` at `ctx` tokens of `cache` KV cache fits the device at
/// all, as `hearth fit` computes it. Only the device ceiling is checked: it
/// does not move, while what is free now is provisional (ADR-000) and is
/// the supervisor's to find out.
pub(crate) fn fits_device(model: &str, ctx: u64, cache: &str) -> Result<(), String> {
    let line: Vec<String> = [
        "hearth",
        "--json",
        "fit",
        "--ctx",
        &ctx.to_string(),
        "--cache-type",
        cache,
        "--",
        model,
    ]
    .iter()
    .map(|s| (*s).to_string())
    .collect();
    let report: serde_json::Value = crate::dispatch(&line)
        .and_then(|t| serde_json::from_str(&t).map_err(|e| e.to_string()))
        .map_err(|e| format!("fit check failed: {e}"))?;
    if report["fits_device"] == false {
        return Err(format!(
            "{model} at {ctx} tokens needs {} but the device holds {} ({}); the largest context that fits is {}: lower ctx in the configuration",
            crate::units::bytes(report["total_bytes"].as_u64().unwrap_or_default()),
            crate::units::bytes(report["ceiling"]["bytes"].as_u64().unwrap_or_default()),
            report["ceiling"]["source"].as_str().unwrap_or("device"),
            report["max_ctx_device"]
        ));
    }
    Ok(())
}

#[cfg(test)]
mod port_tests {
    use super::*;
    use std::io::{Read as _, Write as _};

    /// Answers fixed JSON per path, as the runtime would.
    fn fake(routes: Vec<(&'static str, &'static str)>) -> u16 {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut s) = stream else { return };
                let mut buf = [0u8; 2048];
                let n = s.read(&mut buf).unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]);
                let path = req.split_whitespace().nth(1).unwrap_or("/").to_string();
                let reply = routes.iter().find(|(p, _)| *p == path).map_or_else(
                    || "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n".to_string(),
                    |(_, body)| {
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{body}",
                            body.len()
                        )
                    },
                );
                let _ = s.write_all(reply.as_bytes());
            }
        });
        port
    }

    #[test]
    fn a_llama_server_names_its_own_file() {
        let port = fake(vec![
            (
                "/props",
                r#"{"default_generation_settings":{"n_ctx":4096},"total_slots":1,"model_path":"/m/q.gguf"}"#,
            ),
            ("/health", r#"{"status":"ok"}"#),
        ]);
        // The server's own context comes with its file.
        assert_eq!(
            from_server(port, None).unwrap(),
            (PathBuf::from("/m/q.gguf"), Some(4096))
        );
        assert!(
            from_server(port, Some(std::path::Path::new("x")))
                .unwrap_err()
                .contains("twice")
        );
    }

    #[test]
    fn ollama_with_several_models_needs_one_named() {
        let port = fake(vec![
            ("/api/version", r#"{"version":"0.30"}"#),
            (
                "/api/ps",
                r#"{"models":[{"name":"a:1","size":1},{"name":"b:2","size":2}]}"#,
            ),
        ]);
        let e = from_server(port, None).unwrap_err();
        assert!(e.contains("a:1, b:2"), "{e}");
    }
}
