//! `hearth models`: every model on this machine's disks, from the stores
//! `fit` reads (the Hugging Face cache, LM Studio, Ollama), with its
//! weights and the largest context the device holds it at. Only files are
//! read; nothing is downloaded or loaded.

use std::fmt::Write as _;

use hearth_sense::kv::CacheType;
use serde::Serialize;

use crate::units::bytes;

#[derive(clap::Args)]
pub struct Args {
    /// Only models whose name contains this, case ignored.
    filter: Option<String>,
    /// KV cache type to size contexts with (for keys and values). Without
    /// it, a model's own dtype when its config says (MLX, safetensors);
    /// a GGUF's context is then not sized.
    #[arg(long, value_parser = crate::fit::parse_cache)]
    cache_type: Option<CacheType>,
}

#[derive(Serialize)]
struct Model {
    name: String,
    store: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    arch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    weights_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    trained_context: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cache_type: Option<&'static str>,
    /// The largest context whose weights and KV cache fit under the
    /// device ceiling, capped at the trained context. `None` when the
    /// weights alone do not fit, or the cache type is unknown.
    #[serde(skip_serializing_if = "Option::is_none")]
    max_ctx_device: Option<u64>,
    fits_device: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Serialize)]
struct Report {
    ceiling_bytes: Option<u64>,
    ceiling_source: Option<String>,
    models: Vec<Model>,
    /// Stores that could not be listed.
    errors: Vec<String>,
}

/// Where a listed model lives, so it is read from that store and no other.
#[derive(Clone, Copy)]
enum Source {
    Download(hearth_runtime::model_stores::Store),
    Ollama,
}

impl Source {
    fn name(self) -> &'static str {
        match self {
            Self::Download(s) => s.name(),
            Self::Ollama => "ollama",
        }
    }

    /// The model's weights, looked up in this store only.
    fn locate(self, name: &str) -> Result<std::path::PathBuf, String> {
        match self {
            Self::Download(store) => hearth_runtime::model_stores::find_in(name, store)
                .unwrap_or_else(|| Err(format!("{name}: gone from the {}", store.name()))),
            Self::Ollama => hearth_runtime::ollama_store::weights(name),
        }
    }
}

/// Every name `fit` resolves, with the store it is in.
fn names(errors: &mut Vec<String>) -> Vec<(Source, String)> {
    let mut out = Vec::new();
    let (repos, problems) = hearth_runtime::model_stores::list();
    errors.extend(problems);
    for (store, repo) in repos {
        match hearth_runtime::model_stores::weights_of(&repo, store) {
            Ok(files) => out.extend(files.into_iter().map(|f| (Source::Download(store), f))),
            Err(e) => errors.push(e),
        }
    }
    let (models, problems) = hearth_runtime::ollama_store::list();
    errors.extend(problems);
    out.extend(models.into_iter().map(|m| (Source::Ollama, m)));
    out
}

fn survey(source: Source, name: String, cache: Option<CacheType>, ceiling: Option<u64>) -> Model {
    let mut model = Model {
        name,
        store: source.name(),
        arch: None,
        weights_bytes: None,
        trained_context: None,
        cache_type: None,
        max_ctx_device: None,
        fits_device: None,
        error: None,
    };
    let found = source
        .locate(&model.name)
        .and_then(|path| crate::fit::weights_and_geometry(&path).map(|(_, w, g)| (path, w, g)));
    let (path, weights, geometry) = match found {
        Ok(f) => f,
        Err(e) => {
            model.error = Some(e);
            return model;
        }
    };
    let cache = cache.or_else(|| crate::fit::dtype_cache_type(&path));
    model.arch = Some(geometry.arch.clone());
    model.weights_bytes = Some(weights);
    model.trained_context = geometry.trained_context;
    model.cache_type = cache.map(CacheType::name);
    model.fits_device = ceiling.map(|c| weights <= c);
    model.max_ctx_device = ceiling.zip(cache).and_then(|(c, t)| {
        c.checked_sub(weights)
            .and_then(|left| geometry.max_tokens(left, t, t, None))
            .map(|n| geometry.trained_context.map_or(n, |t| n.min(t)))
    });
    model
}

pub fn run(args: &Args, json: bool) -> Result<String, String> {
    let ceiling = crate::fit::device_ceiling()?;
    let mut errors = Vec::new();
    let wanted = args.filter.as_deref().map(str::to_lowercase);
    let models: Vec<Model> = names(&mut errors)
        .into_iter()
        .filter(|(_, n)| wanted.as_ref().is_none_or(|w| n.to_lowercase().contains(w)))
        .map(|(store, name)| {
            survey(
                store,
                name,
                args.cache_type,
                ceiling.as_ref().map(|c| c.bytes),
            )
        })
        .collect();
    let report = Report {
        ceiling_bytes: ceiling.as_ref().map(|c| c.bytes),
        ceiling_source: ceiling.map(|c| c.source),
        models,
        errors,
    };
    if json {
        return crate::to_json(&report);
    }
    let mut out = String::new();
    match (report.ceiling_bytes, &report.ceiling_source) {
        (Some(b), Some(s)) => {
            let _ = writeln!(out, "device ceiling {} ({s})", bytes(b));
        }
        _ => {
            let _ = writeln!(out, "no accelerator limit known");
        }
    }
    if report.models.is_empty() {
        let _ = writeln!(
            out,
            "no models found in the Hugging Face cache, LM Studio or Ollama"
        );
    }
    for m in &report.models {
        let line = match (&m.error, m.weights_bytes) {
            (Some(e), _) => format!("unreadable: {e}"),
            (None, Some(w)) => {
                let ctx = match (m.fits_device, m.max_ctx_device, m.cache_type) {
                    (None, _, _) => "no accelerator limit known to size it against".to_string(),
                    (Some(false), _, _) => "weights alone exceed the device".to_string(),
                    (_, Some(n), Some(t)) => format!("max context {n} at {t}"),
                    (_, None, Some(t)) => format!("no context fits at {t}"),
                    (_, _, None) => "give --cache-type to size its context".to_string(),
                };
                format!(
                    "{}  {}  {ctx}{}",
                    m.arch.as_deref().unwrap_or("?"),
                    bytes(w),
                    m.trained_context
                        .map_or_else(String::new, |t| format!(" (trained {t})"))
                )
            }
            (None, None) => String::new(),
        };
        let _ = writeln!(out, "{:<60} [{}]  {line}", m.name, m.store);
    }
    for e in &report.errors {
        let _ = writeln!(out, "store unreadable: {e}");
    }
    Ok(out)
}
