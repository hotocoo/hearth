//! What a llama-server holds on the accelerator beside its model: a
//! multimodal projector and a speculative-decoding draft.

use std::path::{Path, PathBuf};

use super::{Args, listener_args};
use crate::server_args::{hf_projector_of, projector_arg};
use crate::units::bytes;

/// Adds the projector to the files and weights, since it is on the
/// accelerator beside the model: `--mmproj` as given (which must exist),
/// else the one the server on `--port` loads. A server's that cannot be
/// found from here (a path relative to its own directory) is not counted,
/// and the note says so.
pub(super) fn add_projector(
    args: &Args,
    model: &Path,
    files: &mut Vec<PathBuf>,
    weights_bytes: &mut u64,
) -> Result<Option<String>, String> {
    let given = args.mmproj.is_some();
    let from_server = || {
        args.port
            .and_then(listener_args)
            .as_deref()
            .and_then(projector_arg)
    };
    let Some(projector) = args.mmproj.clone().or_else(from_server) else {
        return Ok(hf_projector(args, model, files, weights_bytes));
    };
    let size = match std::fs::metadata(&projector) {
        Ok(m) => m.len(),
        Err(e) if !given => {
            return Ok(Some(format!(
                "the server loads the projector {} but it is not readable from here ({e}): not counted",
                projector.display()
            )));
        }
        Err(e) => return Err(format!("{}: {e}", projector.display())),
    };
    *weights_bytes = weights_bytes.saturating_add(size);
    files.push(projector);
    Ok(None)
}

/// [`hf_projector_of`] for `fit --port`, counted, with a note either way.
pub(super) fn hf_projector(
    args: &Args,
    model: &Path,
    files: &mut Vec<PathBuf>,
    weights_bytes: &mut u64,
) -> Option<String> {
    let words = args.port.and_then(listener_args)?;
    let found = match hf_projector_of(&words, model) {
        Ok(found) => found?,
        Err(e) => return Some(format!("{e}; the -hf projector is not counted")),
    };
    match std::fs::metadata(&found) {
        Ok(m) => {
            *weights_bytes = weights_bytes.saturating_add(m.len());
            let note = format!(
                "counted {} ({}), the projector -hf loads beside the model",
                found.display(),
                bytes(m.len())
            );
            files.push(found);
            Some(note)
        }
        Err(e) => Some(format!(
            "{}: {e}; the -hf projector is not counted",
            found.display()
        )),
    }
}

/// The draft model's weights and KV, and a note: `--draft` (which must be
/// readable), else the `--port` server's own `-md`. A server's draft that
/// is kept on the CPU, comes from `-hfd`, or is not readable from here is
/// noted, not counted.
/// The slots a llama-server on `port` runs, from its `/props`.
fn server_slots(port: u16) -> Option<u64> {
    use hearth_runtime::fingerprint::{Identity, fingerprint};
    let config = hearth_core::config::load().ok()?;
    let addr = std::net::SocketAddr::from((std::net::Ipv4Addr::LOCALHOST, port));
    match fingerprint(
        addr,
        std::time::Duration::from_millis(config.discovery.first_contact_timeout_ms),
        config.discovery.max_probe_response_bytes,
    )
    .identity
    {
        Identity::LlamaCpp { slots, .. } => slots,
        _ => None,
    }
}

pub(super) fn draft_of(args: &Args, ctx: u64) -> Result<(Option<u64>, Option<String>), String> {
    use crate::draft::{Draft, Found};
    let given = args.draft.as_ref().map(|model| Draft {
        model: model.clone(),
        ctx: args.draft_ctx,
        cache: {
            let k = args
                .draft_cache_type
                .unwrap_or(hearth_sense::kv::CacheType::F16);
            (k, args.draft_cache_type_v.unwrap_or(k))
        },
        // --draft-ctx is already the total over the slots.
        slots: Some(1),
    });
    let draft = match given {
        Some(d) => d,
        None => match args.port.and_then(listener_args) {
            None => return Ok((None, None)),
            Some(words) => match crate::draft::from_args(&words)? {
                Found::None => return Ok((None, None)),
                Found::NotCounted(why) => return Ok((None, Some(why))),
                Found::Draft(d) if !d.model.is_file() => {
                    return Ok((
                        None,
                        Some(format!(
                            "the server's draft {} is not readable from here (relative to its own directory?): not counted",
                            d.model.display()
                        )),
                    ));
                }
                Found::Draft(d) if d.slots.is_none() && d.ctx.is_some() => Draft {
                    slots: args.port.and_then(server_slots),
                    ..d
                },
                Found::Draft(d) => d,
            },
        },
    };
    let (weights, kv) = crate::draft::bytes(&draft, ctx)?;
    let note = draft.ctx.is_none().then(|| {
        format!(
            "the draft's cache follows the context; the largest context is computed with it held at {ctx} tokens"
        )
    });
    Ok((Some(weights.saturating_add(kv)), note))
}
