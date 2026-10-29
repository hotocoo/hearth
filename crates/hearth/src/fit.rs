//! `hearth fit`: does a GGUF model at a given context fit on this machine?
//!
//! Inputs are the model file and the runtime settings the caller intends to
//! use; outputs are measured (memory, device limits) or computed from the
//! model's own metadata. Runtime compute buffers are not known until a model
//! has been loaded once, and the report says so instead of guessing them.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use hearth_sense::PressureLevel;
use hearth_sense::kv::{CacheType, Confidence, Geometry, Swa};
use serde::Serialize;

use crate::measured::measured_compute;
use crate::model_files::{dtype_cache_type, from_name, weights_and_geometry};
use crate::server_args::{cache_types_from_args, model_arg};
use crate::units::bytes;

mod beside;

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
    /// asked which model it serves (llama.cpp reports its file; Ollama and
    /// LM Studio the model loaded, and an OpenAI-compatible server such as
    /// `mlx_lm.server` the --model it was started with, else the one it
    /// lists; --model picks one when there are several).
    #[arg(long)]
    port: Option<u16>,
    /// Total tokens of KV cache the runtime will allocate (llama.cpp `-c`;
    /// for Ollama, `num_ctx` × `OLLAMA_NUM_PARALLEL`).
    #[arg(long, required_unless_present = "port")]
    ctx: Option<u64>,
    /// KV cache type for keys, and for values unless --cache-type-v is given
    /// (llama.cpp --cache-type-k, Ollama `OLLAMA_KV_CACHE_TYPE`).
    /// Without it: with --port, the llama-server's own -ctk/-ctv; for an
    /// MLX or safetensors directory, the model's dtype from config.json,
    /// which is what MLX and Transformers cache in.
    #[arg(long, value_parser = parse_cache)]
    cache_type: Option<CacheType>,
    #[arg(long, value_parser = parse_cache)]
    cache_type_v: Option<CacheType>,
    /// Parallel sequences; with --ubatch, bounds sliding-window caches.
    #[arg(long, requires = "ubatch", value_parser = clap::value_parser!(u64).range(1..))]
    parallel: Option<u64>,
    /// Physical batch size (llama.cpp -ub); with --parallel, bounds
    /// sliding-window caches.
    #[arg(long, requires = "parallel", value_parser = clap::value_parser!(u64).range(1..))]
    ubatch: Option<u64>,
    /// A multimodal projector loaded beside the model (llama.cpp --mmproj),
    /// counted with the weights. With --port, the server's own is read.
    #[arg(long)]
    mmproj: Option<PathBuf>,
    /// A draft model for speculative decoding (llama.cpp -md), counted with
    /// its own KV cache at --ctx (f16, llama.cpp's default). With --port,
    /// the server's own -md, -cd and -ctkd/-ctvd are read.
    #[arg(long)]
    draft: Option<PathBuf>,
    /// The draft's total context over all slots (llama.cpp -cd × -np),
    /// when it does not follow --ctx.
    #[arg(long, requires = "draft", value_parser = clap::value_parser!(u64).range(1..))]
    draft_ctx: Option<u64>,
    /// The draft's KV cache type for keys, and values unless
    /// --draft-cache-type-v (llama.cpp -ctkd); f16 when not given.
    #[arg(long, requires = "draft", value_parser = parse_cache)]
    draft_cache_type: Option<CacheType>,
    /// The draft's KV cache type for values (llama.cpp -ctvd).
    #[arg(long, requires = "draft", value_parser = parse_cache)]
    draft_cache_type_v: Option<CacheType>,
}

impl Args {
    /// The sliding-window bound, when both of its parts are given.
    fn swa(&self) -> Option<Swa> {
        self.parallel
            .zip(self.ubatch)
            .map(|(sequences, ubatch)| Swa { sequences, ubatch })
    }
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
    /// The KV cache types costed, as runtimes name them.
    cache_type_k: &'static str,
    cache_type_v: &'static str,
    kv_bytes: u64,
    /// The compute buffers llama.cpp logged when hearth last started this
    /// model at this context (`hearth run`, hearthd); counted in the total.
    #[serde(skip_serializing_if = "Option::is_none")]
    compute_bytes: Option<u64>,
    /// A speculative-decoding draft model's weights and KV cache; counted.
    #[serde(skip_serializing_if = "Option::is_none")]
    draft_bytes: Option<u64>,
    total_bytes: u64,
    /// What the total leaves out, if anything.
    #[serde(skip_serializing_if = "Option::is_none")]
    excludes: Option<&'static str>,
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
    /// Like `fits_device_with`, but keys kept at their requested type and
    /// only the values smaller (llama.cpp -ctv alone): keys carry more of
    /// the attention's precision. Each value type with its total.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    fits_device_with_values: Vec<(String, u64)>,
    max_ctx_now: Option<u64>,
    notes: Vec<String>,
}

#[derive(Serialize)]
pub(crate) struct Ceiling {
    pub(crate) bytes: u64,
    pub(crate) source: String,
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
    crate::server_args::server_words(listener_pid(port)?)
}

/// The pid listening on `port`, when one is found.
fn listener_pid(port: u16) -> Option<i32> {
    hearth_sense::list_processes()
        .ok()?
        .into_iter()
        .find_map(|p| {
            hearth_sense::tcp_sockets(p.pid)
                .ok()?
                .iter()
                .any(|s| s.state == hearth_sense::TcpState::Listen && s.local.port() == port)
                .then_some(p.pid)
        })
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

/// Says so when what loads before any context is already past the ceiling,
/// so `max_ctx_device: null` reads as nothing fits, not as not computed.
fn overweight_note(fixed: u64, ceiling: Option<&Ceiling>) -> Option<String> {
    ceiling.filter(|c| fixed > c.bytes).map(|c| {
        format!(
            "what loads before any context ({}) exceeds the device ceiling ({}): no context fits; use a smaller model or quantization",
            crate::units::bytes(fixed),
            crate::units::bytes(c.bytes)
        )
    })
}

/// The key and value cache types: as given, else the server's own.
fn cache_types(args: &Args, model: &Path) -> Result<(CacheType, CacheType), String> {
    if let Some(k) = args.cache_type {
        return Ok((k, args.cache_type_v.unwrap_or(k)));
    }
    let (k, v) = args
        .port
        .and_then(server_cache_types)
        .or_else(|| dtype_cache_type(model).map(|t| (t, t)))
        .ok_or_else(|| match args.port {
            Some(port) => format!(
                "give --cache-type: the command line of what listens on {port} ({}) does not say it",
                listener_args(port).map_or_else(
                    || "unreadable".to_string(),
                    |a| a.iter().take(4).cloned().collect::<Vec<_>>().join(" ")
                )
            ),
            None => "give --cache-type (llama.cpp's default is f16)".to_string(),
        })?;
    Ok((k, args.cache_type_v.unwrap_or(v)))
}

/// The machine's memory, the device ceiling, whether memory is under
/// pressure, and what is available now: on a discrete GPU the model goes
/// into the device's own memory, so what is free there decides; otherwise
/// it is the host's memory.
fn room() -> Result<(hearth_sense::HostMemory, Option<Ceiling>, bool, u64), String> {
    let host = crate::pressure::host_memory()?;
    let ceiling = device_ceiling()?.or_else(|| host_ceiling(&host));
    let under_pressure = matches!(
        host.pressure,
        Some(PressureLevel::Warn | PressureLevel::Critical)
    );
    let available_now = ceiling
        .as_ref()
        .and_then(|c| c.free_bytes)
        .unwrap_or(host.available_bytes);
    Ok((host, ceiling, under_pressure, available_now))
}

/// With no accelerator limit (Linux without a GPU, an AMD APU) the model
/// lives in host RAM, so total memory is the ceiling; none when the total is
/// unknown.
fn host_ceiling(host: &hearth_sense::HostMemory) -> Option<Ceiling> {
    (host.total_bytes > 0).then(|| Ceiling {
        bytes: host.total_bytes,
        source: "host memory".into(),
        free_bytes: None,
    })
}

/// Sliding-window layers are costed as full unless --parallel and --ubatch
/// bound them.
fn sliding_note(swa: Option<&Swa>, geometry: &Geometry) -> Option<String> {
    (swa.is_none()
        && geometry
            .layers
            .iter()
            .any(|l| matches!(l, hearth_sense::kv::Layer::Sliding { .. })))
    .then(|| {
        "sliding-window layers costed as full; pass --parallel and --ubatch for the bounded size"
            .into()
    })
}

/// What the server asked about with --port holds now: a restart at another
/// size frees that first. Unified memory only; a discrete GPU's free memory
/// is not where the host footprint lives.
fn resident_bytes(pid: Option<i32>, ceiling: Option<&Ceiling>) -> Option<u64> {
    pid.filter(|_| ceiling.is_none_or(|c| c.free_bytes.is_none()))
        .and_then(|p| hearth_sense::process_footprint(p).ok())
        .map(|f| f.footprint_bytes)
}

pub fn run(args: &Args, json: bool) -> Result<String, String> {
    let (model, server_ctx) = locate(args)?;
    let ctx = args
        .ctx
        .or(server_ctx)
        .ok_or("the server does not say its context; give --ctx (tokens of KV cache to fit)")?;
    let (mut files, mut weights_bytes, geometry) = weights_and_geometry(&model)?;
    let projector_note = beside::add_projector(args, &model, &mut files, &mut weights_bytes)?;
    let (k, v) = cache_types(args, &model)?;
    let swa = args.swa();
    let kv_bytes = geometry.kv_bytes(ctx, k, v, swa);
    let (compute_bytes, compute_note) = measured_compute(&model, ctx);
    let (draft_bytes, draft_note) = beside::draft_of(args, ctx)?;
    // What the context search holds fixed: the weights, the compute buffers
    // when a start of this model at this context logged them, and a draft.
    let fixed = weights_bytes
        .saturating_add(compute_bytes.unwrap_or(0))
        .saturating_add(draft_bytes.unwrap_or(0));
    let total_bytes = fixed.saturating_add(kv_bytes);

    let (host, ceiling, under_pressure, available_now) = room()?;
    let pid = args.port.and_then(listener_pid);
    // Only a llama.cpp server says its context, and only there does the
    // listener hold the model; an Ollama daemon's runner is a child.
    let resident = resident_bytes(pid.filter(|_| server_ctx.is_some()), ceiling.as_ref());
    let available_now = available_now.saturating_add(resident.unwrap_or(0));
    let fits_now = !under_pressure && total_bytes <= available_now;
    // Beyond the trained context a model is not expected to work, so the
    // largest useful context is capped there even when memory allows more.
    let max_ctx = |budget: u64| {
        budget
            .checked_sub(fixed)
            .and_then(|left| geometry.max_tokens(left, k, v, swa))
            .map(|n| geometry.trained_context.map_or(n, |t| n.min(t)))
    };
    let mut notes = geometry.notes.clone();
    notes.extend(compute_note);
    notes.extend(projector_note);
    notes.extend(draft_note);
    notes.extend(pid.and_then(crate::server_args::env_problem));
    notes.extend(resident.map(|r| {
        format!(
            "available counts the {} the server holds now: restarting it frees that first",
            crate::units::bytes(r)
        )
    }));
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
    notes.extend(overweight_note(fixed, ceiling.as_ref()));
    notes.extend(sliding_note(swa.as_ref(), &geometry));
    let report = Report {
        model: model.clone(),
        files,
        weights_bytes,
        ctx,
        cache_type_k: k.name(),
        cache_type_v: v.name(),
        kv_bytes,
        total_bytes,
        compute_bytes,
        draft_bytes,
        excludes: if compute_bytes.is_some() {
            None
        } else {
            Some("runtime compute buffers (known after the model's first load)")
        },
        fits_device: ceiling.as_ref().map(|c| total_bytes <= c.bytes),
        max_ctx_device: ceiling.as_ref().and_then(|c| max_ctx(c.bytes)),
        fits_device_with: alternatives(ceiling.as_ref(), total_bytes, fixed, |t| {
            geometry.kv_bytes(ctx, t, t, swa)
        }),
        fits_device_with_values: alternatives(ceiling.as_ref(), total_bytes, fixed, |t| {
            geometry.kv_bytes(ctx, k, t, swa)
        })
        .into_iter()
        // Values at the keys' own type are fits_device_with's case.
        .filter(|(t, _)| t != k.name())
        .collect(),
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
pub(crate) fn device_ceiling() -> Result<Option<Ceiling>, String> {
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
        "weights {} + KV {} at {} tokens ({}/{}){} = {}{}",
        bytes(r.weights_bytes),
        bytes(r.kv_bytes),
        r.ctx,
        r.cache_type_k,
        r.cache_type_v,
        r.compute_bytes
            .map_or_else(String::new, |c| format!(" + compute {}", bytes(c)))
            + &r.draft_bytes
                .map_or_else(String::new, |d| format!(" + draft {}", bytes(d))),
        bytes(r.total_bytes),
        r.excludes
            .map_or_else(String::new, |e| format!(" (excludes {e})"))
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
    if !r.fits_device_with_values.is_empty() {
        let options: Vec<String> = r
            .fits_device_with_values
            .iter()
            .map(|(t, total)| format!("{t} ({})", bytes(*total)))
            .collect();
        let _ = writeln!(
            out,
            "or keep keys at {}, values at: {} (llama.cpp -ctv; a quantized V cache needs flash attention, -fa on)",
            r.cache_type_k,
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
pub(crate) fn fits_device(
    model: &str,
    ctx: u64,
    cache: &str,
    projector: Option<&std::path::Path>,
) -> Verdict {
    let mut line: Vec<String> = [
        "hearth",
        "--json",
        "fit",
        "--ctx",
        &ctx.to_string(),
        "--cache-type",
        cache,
    ]
    .iter()
    .map(|s| (*s).to_string())
    .collect();
    if let Some(p) = projector {
        line.push(format!("--mmproj={}", p.display()));
    }
    line.extend(["--".to_string(), model.to_string()]);
    let report: serde_json::Value = match crate::dispatch(&line)
        .and_then(|t| serde_json::from_str(&t).map_err(|e| e.to_string()))
    {
        Ok(r) => r,
        Err(e) => return Verdict::Unknown(e),
    };
    // No ceiling known at all is not a fit: never admit blind.
    match report["fits_device"].as_bool() {
        Some(true) => return Verdict::Fits,
        None => return Verdict::Unknown("no memory ceiling to check against".into()),
        Some(false) => {}
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
    (model, ctx, cache): (String, u64, String),
    projector: Option<PathBuf>,
) -> impl Fn() -> Result<(), String> + Send {
    let known: std::sync::Mutex<Option<Result<(), String>>> = std::sync::Mutex::new(None);
    move || {
        let mut known = known
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(verdict) = known.as_ref() {
            return verdict.clone();
        }
        match fits_device(&model, ctx, &cache, projector.as_deref()) {
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
mod zero_tests {
    use clap::Parser;

    #[derive(Parser)]
    struct Cli {
        #[command(flatten)]
        args: super::Args,
    }

    #[test]
    fn zero_parallel_or_ubatch_is_refused() {
        for flag in ["--parallel", "--ubatch"] {
            let other = if flag == "--parallel" {
                "--ubatch"
            } else {
                "--parallel"
            };
            let line = ["fit", "--ctx", "8", flag, "0", other, "1", "m.gguf"];
            assert!(Cli::try_parse_from(line).is_err(), "{flag} 0");
        }
    }
}

#[cfg(test)]
mod port_tests;
