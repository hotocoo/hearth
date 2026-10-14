//! `hearth fit`: does a GGUF model at a given context fit on this machine?
//!
//! Inputs are the model file and the runtime settings the caller intends to
//! use; outputs are measured (memory, device limits) or computed from the
//! model's own metadata. Runtime compute buffers are not known until a model
//! has been loaded once, and the report says so instead of guessing them.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use hearth_sense::kv::{CacheType, Confidence, Geometry, Swa};
use hearth_sense::{PressureLevel, gguf};
use serde::Serialize;

use crate::units::bytes;

#[derive(clap::Args)]
pub struct Args {
    /// GGUF file (any shard of a split model); a model directory with
    /// config.json and .safetensors (MLX, Transformers); a Hugging Face or LM Studio
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
    /// With --port and no --cache-type, the server's own command line is
    /// read for it (llama.cpp -ctk/-ctv; f16, its default, when absent).
    #[arg(long, value_parser = parse_cache, required_unless_present = "port")]
    cache_type: Option<CacheType>,
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

pub(crate) fn parse_cache(s: &str) -> Result<CacheType, String> {
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
    /// When the requested context does not fit the device: the KV cache
    /// types (same for keys and values) with which it would, each with its
    /// total. Empty when it fits, or when no cache type makes it fit.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    fits_device_with: Vec<(String, u64)>,
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

/// The KV cache types the llama.cpp server listening on `port` was started
/// with, from its command line: `-ctk`/`--cache-type-k` and
/// `-ctv`/`--cache-type-v`, each f16 (llama.cpp's default) when absent.
/// `None` when no readable llama-server listens there.
fn server_cache_types(port: u16) -> Option<(CacheType, CacheType)> {
    cache_types_from_args(&listener_args(port)?)
}

/// The command line of the process listening on `port`, when readable.
fn listener_args(port: u16) -> Option<Vec<String>> {
    let pid = hearth_sense::list_processes()
        .ok()?
        .into_iter()
        .find_map(|p| {
            hearth_sense::tcp_sockets(p.pid)
                .ok()?
                .iter()
                .any(|s| s.state == hearth_sense::TcpState::Listen && s.local.port() == port)
                .then_some(p.pid)
        })?;
    hearth_sense::process_args(pid).ok()
}

/// The value of `--model` (or `--model=VALUE`) on a command line, as
/// `mlx_lm.server` and vLLM take the model they serve.
fn model_arg(args: &[String]) -> Option<String> {
    args.iter().enumerate().find_map(|(i, a)| {
        a.strip_prefix("--model=")
            .map(str::to_string)
            .or_else(|| (a == "--model").then(|| args.get(i + 1).cloned()).flatten())
    })
}

/// See [`server_cache_types`]: only a llama-server's arguments say this.
pub(crate) fn cache_types_from_args(args: &[String]) -> Option<(CacheType, CacheType)> {
    let program = args.first()?;
    if !std::path::Path::new(program)
        .file_name()
        .is_some_and(|n| n.to_string_lossy().contains("llama-server"))
    {
        return None;
    }
    // The last occurrence wins, as in llama.cpp; long flags may be spelt
    // with `_`. An absent flag is not assumed to mean the f16 default: the
    // server may take it from LLAMA_ARG_CACHE_TYPE_* in its environment,
    // which is not read, so the type is then unknown.
    let flag = |names: [&str; 3]| -> Option<CacheType> {
        let value = args.iter().enumerate().rev().find_map(|(i, a)| {
            names.iter().find_map(|n| {
                if a == n {
                    args.get(i + 1).cloned()
                } else {
                    a.strip_prefix(&format!("{n}=")).map(str::to_string)
                }
            })
        })?;
        CacheType::parse(&value)
    };
    Some((
        flag(["-ctk", "--cache-type-k", "--cache_type_k"])?,
        flag(["-ctv", "--cache-type-v", "--cache_type_v"])?,
    ))
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
        // `/props` reports n_ctx per sequence (llama_n_ctx_seq); with
        // several slots and a non-unified cache, the cache holds that many
        // times more. Multiplying is exact then and an overestimate for a
        // unified cache, which is the safe side for a fit.
        Identity::LlamaCpp {
            model: Some(path),
            n_ctx,
            slots,
            ..
        } if model.is_none() => Ok((
            PathBuf::from(path),
            n_ctx.map(|n| n.saturating_mul(slots.unwrap_or(1).max(1))),
        )),
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
        // mlx_lm.server and other OpenAI-compatible servers name their
        // model by repo id or path, which fit resolves like any name. What
        // the server was started with wins over what it lists: mlx_lm.server
        // lists the models in the cache, not the one it serves.
        Identity::OpenAiCompatible { models } => {
            let started = model
                .is_none()
                .then(|| listener_args(port).as_deref().and_then(model_arg))
                .flatten();
            let name = match (model, started, models.as_slice()) {
                (Some(m), _, _) => m.to_string_lossy().into_owned(),
                (None, Some(m), _) => m,
                (None, None, [only]) => only.clone(),
                (None, None, []) => {
                    return Err(format!("{addr} lists no model; name one with --model"));
                }
                (None, None, several) => {
                    return Err(format!(
                        "{addr} lists several models ({}); pick one with --model",
                        several.join(", ")
                    ));
                }
            };
            from_name(std::path::Path::new(&name)).map(|p| (p, None))
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

/// The cache types that would bring the total under the device ceiling,
/// when the requested one does not.
fn alternatives(
    ceiling: Option<&Ceiling>,
    total: u64,
    weights: u64,
    kv: impl Fn(CacheType) -> u64,
) -> Vec<(String, u64)> {
    let Some(c) = ceiling.filter(|c| total > c.bytes) else {
        return Vec::new();
    };
    CacheType::ALL
        .iter()
        .map(|t| (t.name().to_string(), weights + kv(*t)))
        .filter(|(_, total)| *total <= c.bytes)
        .collect()
}

/// The key and value cache types: as given, else the server's own.
fn cache_types(args: &Args) -> Result<(CacheType, CacheType), String> {
    if let Some(k) = args.cache_type {
        return Ok((k, args.cache_type_v.unwrap_or(k)));
    }
    let (k, v) = args
        .port
        .and_then(server_cache_types)
        .ok_or("give --cache-type: the server's command line does not say it")?;
    Ok((k, args.cache_type_v.unwrap_or(v)))
}

/// The model's files, their total size, and its cache geometry: from the
/// GGUF header, or for a directory with `config.json` (MLX, safetensors)
/// from that config and the `.safetensors` files beside it.
fn weights_and_geometry(model: &std::path::Path) -> Result<(Vec<PathBuf>, u64, Geometry), String> {
    let size = |files: &[PathBuf]| {
        files
            .iter()
            .map(|f| {
                std::fs::metadata(f)
                    .map(|m| m.len())
                    .map_err(|e| format!("{}: {e}", f.display()))
            })
            .sum::<Result<u64, String>>()
    };
    if model.is_dir() {
        let config_path = model.join("config.json");
        let text = std::fs::read_to_string(&config_path).map_err(|e| {
            format!(
                "{}: {e}; a model directory needs config.json",
                config_path.display()
            )
        })?;
        let config: serde_json::Value =
            serde_json::from_str(&text).map_err(|e| format!("{}: {e}", config_path.display()))?;
        let mut files = safetensors_weights(model)?;
        files.sort();
        files.dedup();
        if files.is_empty() {
            return Err(format!("{}: no .safetensors weights", model.display()));
        }
        let geometry = Geometry::from_hf_config(&config).map_err(|e| e.to_string())?;
        return Ok((files.clone(), size(&files)?, geometry));
    }
    let files = gguf::model_files(model);
    let weights = size(&files)?;
    let header = gguf::read_header(&files[0]).map_err(|e| e.to_string())?;
    let geometry = Geometry::from_header(&header).map_err(|e| e.to_string())?;
    Ok((files, weights, geometry))
}

/// The weight files of a safetensors model directory: those its
/// `model.safetensors.index.json` maps tensors to when it has one (so a
/// `consolidated.safetensors` beside the shards is not counted twice),
/// else every `.safetensors` file but a `LoRA` adapter's `adapters.safetensors`.
fn safetensors_weights(dir: &Path) -> Result<Vec<PathBuf>, String> {
    let index = dir.join("model.safetensors.index.json");
    if index.is_file() {
        let text =
            std::fs::read_to_string(&index).map_err(|e| format!("{}: {e}", index.display()))?;
        let json: serde_json::Value =
            serde_json::from_str(&text).map_err(|e| format!("{}: {e}", index.display()))?;
        let map = json["weight_map"]
            .as_object()
            .ok_or_else(|| format!("{}: no weight_map", index.display()))?;
        return map
            .values()
            .map(|v| {
                // Shards sit beside the index; a name is never a path.
                v.as_str()
                    .filter(|n| Path::new(n).file_name().is_some_and(|f| f == *n))
                    .map(|n| dir.join(n))
                    .ok_or_else(|| format!("{}: bad shard name {v}", index.display()))
            })
            .collect();
    }
    Ok(std::fs::read_dir(dir)
        .and_then(Iterator::collect::<Result<Vec<_>, _>>)
        .map_err(|e| format!("{}: {e}", dir.display()))?
        .into_iter()
        .map(|e| e.path())
        .filter(|p| {
            p.extension()
                .is_some_and(|x| x.eq_ignore_ascii_case("safetensors"))
                && p.file_name().is_some_and(|n| n != "adapters.safetensors")
        })
        .collect())
}

pub fn run(args: &Args, json: bool) -> Result<String, String> {
    let (model, server_ctx) = locate(args)?;
    let ctx = args
        .ctx
        .or(server_ctx)
        .ok_or("the server does not say its context; give --ctx (tokens of KV cache to fit)")?;
    let (files, weights_bytes, geometry) = weights_and_geometry(&model)?;
    let (k, v) = cache_types(args)?;
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
        fits_device_with: alternatives(ceiling.as_ref(), total_bytes, weights_bytes, |t| {
            geometry.kv_bytes(ctx, t, t, swa)
        }),
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
    if !r.fits_device_with.is_empty() {
        let options: Vec<String> = r
            .fits_device_with
            .iter()
            .map(|(t, total)| format!("{t} ({})", bytes(*total)))
            .collect();
        let _ = writeln!(
            out,
            "would fit with KV cache type: {} (llama.cpp -ctk/-ctv, Ollama OLLAMA_KV_CACHE_TYPE)",
            options.join(", ")
        );
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
pub(crate) fn fits_device(model: &str, ctx: u64, cache: &str) -> Verdict {
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
    let report: serde_json::Value = match crate::dispatch(&line)
        .and_then(|t| serde_json::from_str(&t).map_err(|e| e.to_string()))
    {
        Ok(r) => r,
        Err(e) => return Verdict::Unknown(e),
    };
    if report["fits_device"] != false {
        return Verdict::Fits;
    }
    let mut advice = report["max_ctx_device"].as_u64().map_or_else(
        || "the weights alone do not fit: use a smaller model or quantization".to_string(),
        |n| format!("the largest context that fits is {n}: lower ctx"),
    );
    let kinds: Vec<&str> = report["fits_device_with"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|o| o[0].as_str())
        .collect();
    if !kinds.is_empty() {
        let _ = write!(advice, ", or keep it with cache_type {}", kinds.join(" / "));
    }
    Verdict::TooBig(format!(
        "{model} at {ctx} tokens needs {} but the device holds {} ({}); {advice}",
        crate::units::bytes(report["total_bytes"].as_u64().unwrap_or_default()),
        crate::units::bytes(report["ceiling"]["bytes"].as_u64().unwrap_or_default()),
        report["ceiling"]["source"].as_str().unwrap_or("device"),
    ))
}

/// What the fit check concluded.
pub(crate) enum Verdict {
    Fits,
    /// Never fits the device: starting it would only run out of memory.
    TooBig(String),
    /// The check itself could not run (a driver query failed, the model is
    /// not downloaded yet). Not a verdict on the model.
    Unknown(String),
}

/// The fit check a supervisor runs before each start: refuses only what
/// cannot fit; a check that cannot run is reported and the start goes
/// ahead, since the server's own failure is then the honest answer. A
/// "fits" verdict is kept for later starts.
pub(crate) fn start_check(
    label: String,
    model: String,
    ctx: u64,
    cache: String,
) -> impl Fn() -> Result<(), String> + Send {
    let known: std::sync::Mutex<Option<Result<(), String>>> = std::sync::Mutex::new(None);
    move || {
        let mut known = known
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(verdict) = known.as_ref() {
            return verdict.clone();
        }
        match fits_device(&model, ctx, &cache) {
            Verdict::Fits => {
                *known = Some(Ok(()));
                Ok(())
            }
            // Not kept: the ceiling can be raised (iogpu.wired_limit_mb),
            // and the next start should see it.
            Verdict::TooBig(why) => Err(why),
            Verdict::Unknown(why) => {
                eprintln!("{label} fit check could not run ({why}); starting anyway");
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod args_tests {
    use super::*;

    #[test]
    fn cache_types_come_from_a_llama_server_command_line() {
        let args = |a: &[&str]| a.iter().map(|s| (*s).to_string()).collect::<Vec<_>>();
        assert_eq!(
            cache_types_from_args(&args(&[
                "/x/llama-server",
                "-c",
                "8",
                "--cache-type-k",
                "q8_0",
                "-ctv=q4_0"
            ])),
            Some((CacheType::Q8_0, CacheType::Q4_0))
        );
        assert_eq!(cache_types_from_args(&args(&["llama-server"])), None);
        assert_eq!(
            cache_types_from_args(&args(&[
                "llama-server",
                "-ctk",
                "f16",
                "-ctv",
                "f16",
                "-ctk",
                "q8_0",
                "--cache_type_v",
                "q8_0"
            ])),
            Some((CacheType::Q8_0, CacheType::Q8_0)),
            "the last flag wins"
        );
        assert_eq!(cache_types_from_args(&args(&["ollama", "serve"])), None);
        assert_eq!(
            cache_types_from_args(&args(&["llama-server", "-ctk", "nonsense"])),
            None
        );
    }
}

#[cfg(test)]
mod alternative_tests {
    use super::*;

    #[test]
    fn only_cache_types_under_the_ceiling_are_offered() {
        let ceiling = Ceiling {
            bytes: 100,
            source: "test".into(),
            free_bytes: None,
        };
        // KV in bytes per element times 10; weights 50.
        let kv = |t: CacheType| match t {
            CacheType::F32 => 80,
            CacheType::F16 | CacheType::Bf16 => 60,
            _ => 30,
        };
        let offered = alternatives(Some(&ceiling), 130, 50, kv);
        assert!(offered.iter().all(|(_, total)| *total <= 100));
        assert!(!offered.iter().any(|(t, _)| t == "f32" || t == "f16"));
        assert!(
            alternatives(Some(&ceiling), 90, 50, kv).is_empty(),
            "already fits"
        );
    }
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

    #[test]
    fn the_model_a_server_was_started_with() {
        let line = |s: &str| s.split(' ').map(String::from).collect::<Vec<_>>();
        assert_eq!(
            model_arg(&line("python -m mlx_lm.server --model org/m --port 8081")).as_deref(),
            Some("org/m")
        );
        assert_eq!(
            model_arg(&line("vllm serve --model=/m")).as_deref(),
            Some("/m")
        );
        assert_eq!(model_arg(&line("python -m mlx_lm.server --model")), None);
        assert_eq!(model_arg(&line("python -m mlx_lm.server")), None);
    }

    #[test]
    fn safetensors_weights_follow_the_index() {
        let dir = std::env::temp_dir().join(format!("hearth-st-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for f in [
            "a.safetensors",
            "b.safetensors",
            "consolidated.safetensors",
            "adapters.safetensors",
        ] {
            std::fs::write(dir.join(f), "").unwrap();
        }
        let names = |v: Vec<PathBuf>| {
            let mut n: Vec<String> = v
                .iter()
                .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
                .collect();
            n.sort();
            n.dedup();
            n
        };
        assert_eq!(
            names(safetensors_weights(&dir).unwrap()),
            ["a.safetensors", "b.safetensors", "consolidated.safetensors"]
        );
        let index = dir.join("model.safetensors.index.json");
        std::fs::write(
            &index,
            r#"{"weight_map":{"x":"a.safetensors","y":"b.safetensors","z":"a.safetensors"}}"#,
        )
        .unwrap();
        assert_eq!(
            names(safetensors_weights(&dir).unwrap()),
            ["a.safetensors", "b.safetensors"]
        );
        std::fs::write(&index, r#"{"weight_map":{"x":"../a.safetensors"}}"#).unwrap();
        assert!(safetensors_weights(&dir).is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
