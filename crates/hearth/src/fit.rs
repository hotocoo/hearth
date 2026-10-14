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
    /// GGUF file (any shard of a split model).
    model: PathBuf,
    /// Total tokens of KV cache the runtime will allocate (llama.cpp `-c`;
    /// for Ollama, `num_ctx` × `OLLAMA_NUM_PARALLEL`).
    #[arg(long)]
    ctx: u64,
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
    max_ctx_device: Option<u64>,
    max_ctx_now: Option<u64>,
    notes: Vec<String>,
}

#[derive(Serialize)]
struct Ceiling {
    bytes: u64,
    source: String,
}

pub fn run(args: &Args, json: bool) -> Result<String, String> {
    let files = gguf::model_files(&args.model);
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
    let kv_bytes = geometry.kv_bytes(args.ctx, k, v, swa);
    let total_bytes = weights_bytes + kv_bytes;

    let host = hearth_sense::host_memory().map_err(|e| e.to_string())?;
    let ceiling = device_ceiling()?;
    let under_pressure = matches!(
        host.pressure,
        Some(PressureLevel::Warn | PressureLevel::Critical)
    );
    let fits_now = !under_pressure && total_bytes <= host.available_bytes;
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
        && args.ctx > trained
    {
        notes.push(format!(
            "requested context {} exceeds the trained context {trained}",
            args.ctx
        ));
    }
    if under_pressure {
        notes.push("the kernel reports memory pressure; nothing should be loaded now".into());
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
        model: args.model.clone(),
        files,
        weights_bytes,
        ctx: args.ctx,
        kv_bytes,
        total_bytes,
        excludes: "runtime compute buffers (known after the model's first load)",
        fits_device: ceiling.as_ref().map(|c| total_bytes <= c.bytes),
        max_ctx_device: ceiling.as_ref().and_then(|c| max_ctx(c.bytes)),
        max_ctx_now: if under_pressure {
            None
        } else {
            max_ctx(host.available_bytes)
        },
        ceiling,
        available_bytes: host.available_bytes,
        pressure: host.pressure,
        fits_now,
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
    let accelerators = hearth_sense::accelerators().map_err(|e| e.to_string())?;
    Ok(accelerators
        .iter()
        .flat_map(|a| {
            [
                a.recommended_working_set_bytes.map(|b| Ceiling {
                    bytes: b,
                    source: format!("{} recommended working set", a.name),
                }),
                a.wired_limit_bytes.map(|b| Ceiling {
                    bytes: b,
                    source: "iogpu.wired_limit_mb".into(),
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
        "fits right now:   {} ({} available; max context {})",
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
