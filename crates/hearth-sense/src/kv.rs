//! KV-cache geometry from GGUF metadata, and the bytes a context costs.
//!
//! Everything comes from the model file or the caller. The only constants are
//! ggml's storage formats (bytes per block for each cache type), which are
//! facts of the file format rather than choices.

use serde::Serialize;

use crate::gguf::{Header, Value};
use crate::{Result, SenseError};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Layer {
    /// Attention over the whole context.
    Full {
        kv_heads: u64,
        k_len: u64,
        v_len: u64,
    },
    /// Attention over a fixed window of recent tokens.
    Sliding {
        kv_heads: u64,
        k_len: u64,
        v_len: u64,
        window: u64,
    },
    /// Multi-head latent attention: one compressed vector per token.
    Latent { k_len: u64, v_len: u64 },
    /// Recurrent or SSM layer: no per-token cache.
    Recurrent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Confidence {
    /// Geometry had to be assumed; the estimate errs high on purpose.
    Low,
    /// Geometry is known but some per-sequence state is not counted.
    Medium,
    /// Every layer's geometry came from explicit metadata.
    High,
}

#[derive(Debug, Clone, Serialize)]
pub struct Geometry {
    pub arch: String,
    pub trained_context: Option<u64>,
    pub layers: Vec<Layer>,
    pub confidence: Confidence,
    /// Why confidence is not `High`, one entry per assumption.
    pub notes: Vec<String>,
}

fn missing(key: &str) -> SenseError {
    SenseError::Parse {
        what: "gguf geometry",
        detail: format!("missing {key}"),
    }
}

/// A layer count read from a file is not trusted to be allocatable: one
/// that is not fails here instead of aborting the process later.
fn room_for_layers(n: usize) -> Result<()> {
    Vec::<Layer>::new()
        .try_reserve_exact(n)
        .map_err(|e| SenseError::Parse {
            what: "model geometry",
            detail: format!("{n} layers: {e}"),
        })
}

/// Expands a scalar or a per-layer array to one value per layer.
fn per_layer(value: Option<&Value>, layers: usize) -> Option<Vec<u64>> {
    let list = value?.as_u64_list()?;
    match list.len() {
        1 => Some(vec![list[0]; layers]),
        n if n == layers => Some(list),
        _ => None,
    }
}

impl Geometry {
    #[allow(clippy::too_many_lines)]
    pub fn from_header(h: &Header) -> Result<Self> {
        let arch = h
            .architecture()
            .ok_or_else(|| missing("general.architecture"))?
            .to_string();
        let key = |suffix: &str| h.arch_key(suffix);
        let num = |suffix: &str| key(suffix).and_then(Value::as_u64);
        let block_count = num("block_count").ok_or_else(|| missing("block_count"))?;
        let n = usize::try_from(block_count).map_err(|_| missing("block_count"))?;
        room_for_layers(n)?;
        let mut notes = Vec::new();
        let mut confidence = Confidence::High;

        let heads = per_layer(key("attention.head_count"), n);
        // The GGUF spec defines head_count_kv as defaulting to head_count.
        let kv_heads = per_layer(key("attention.head_count_kv"), n).or_else(|| heads.clone());
        let head_dim = || {
            let embd = num("embedding_length")?;
            let h0 = *heads.as_ref()?.iter().find(|&&h| h > 0)?;
            Some(embd / h0)
        };
        // The spec defines key/value length as defaulting to embd / heads.
        let k_len = num("attention.key_length").or_else(head_dim);
        let v_len = num("attention.value_length").or_else(head_dim);

        let latent = num("attention.kv_lora_rank");
        let window = num("attention.sliding_window");
        let pattern = key("attention.sliding_window_pattern");

        let layers = if let Some(rank) = latent {
            let rope = num("rope.dimension_count").unwrap_or(0);
            confidence = Confidence::Medium;
            notes.push(format!(
                "latent attention: cache sized as kv_lora_rank {rank} + rope dims {rope} per token; runtimes differ in how they store it"
            ));
            vec![
                Layer::Latent {
                    k_len: rank + rope,
                    v_len: rank,
                };
                n
            ]
        } else {
            let kv_heads = kv_heads.ok_or_else(|| missing("attention.head_count_kv"))?;
            let (k_len, v_len) = k_len
                .zip(v_len)
                .ok_or_else(|| missing("attention.key_length"))?;
            let sliding = sliding_layers(window, pattern, n, &mut notes, &mut confidence);
            kv_heads
                .iter()
                .zip(sliding)
                .map(|(&kv_heads, slide)| match (kv_heads, slide) {
                    (0, _) => Layer::Recurrent,
                    (_, Some(window)) => Layer::Sliding {
                        kv_heads,
                        k_len,
                        v_len,
                        window,
                    },
                    (_, None) => Layer::Full {
                        kv_heads,
                        k_len,
                        v_len,
                    },
                })
                .collect()
        };
        if layers.contains(&Layer::Recurrent) {
            confidence = confidence.min(Confidence::Medium);
            notes.push(
                "recurrent layers keep a per-sequence state that is not counted here".to_string(),
            );
        }
        Ok(Self {
            arch,
            trained_context: num("context_length"),
            layers,
            confidence,
            notes,
        })
    }

    /// Geometry from a Transformers `config.json`, the convention MLX and
    /// safetensors models follow. Multimodal configs keep the language
    /// model's under `text_config`; a key missing there is looked up at the
    /// top level, then in the model type's Transformers defaults
    /// ([`hf_default`]). Keys: `num_hidden_layers`, `num_key_value_heads`
    /// (defaults to `num_attention_heads`), `head_dim` (defaults to
    /// `hidden_size / num_attention_heads`), `sliding_window` with
    /// `layer_types` naming each layer's attention (or a
    /// `sliding_window_pattern` period), `kv_lora_rank` for latent attention,
    /// and `max_position_embeddings` as the trained context.
    #[allow(clippy::too_many_lines)]
    pub fn from_hf_config(config: &serde_json::Value) -> Result<Self> {
        let c = if config["text_config"].is_object() {
            &config["text_config"]
        } else {
            config
        };
        let arch = c["model_type"]
            .as_str()
            .or_else(|| config["model_type"].as_str())
            .unwrap_or("unknown");
        let num = |k: &str| {
            c[k].as_u64()
                .or_else(|| config[k].as_u64())
                .or_else(|| hf_default(arch, k))
        };
        let need = |k: &'static str| {
            num(k).ok_or_else(|| SenseError::Parse {
                what: "config.json geometry",
                detail: format!(
                    "missing {k} (model type {arch}; Transformers may fill it from defaults hearth does not know)"
                ),
            })
        };
        let n = usize::try_from(need("num_hidden_layers")?).map_err(|_| SenseError::Parse {
            what: "config.json geometry",
            detail: "num_hidden_layers too large".into(),
        })?;
        room_for_layers(n)?;
        let mut notes = Vec::new();
        let mut confidence = Confidence::High;
        let heads = need("num_attention_heads")?;
        if let Some(rank) = num("kv_lora_rank").filter(|&r| r > 0) {
            let layers = latent_layers(rank, heads, &num, &mut notes)?;
            return Ok(Self {
                arch: arch.to_string(),
                trained_context: num("max_position_embeddings"),
                layers: vec![layers; n],
                confidence: Confidence::Medium,
                notes,
            });
        }
        let kv_heads = num("num_key_value_heads").unwrap_or(heads);
        let head_dim = if let Some(d) = num("head_dim") {
            d
        } else {
            confidence = Confidence::Medium;
            notes.push("no head_dim: taken as hidden_size / num_attention_heads".into());
            need("hidden_size")? / heads.max(1)
        };
        let window = num("sliding_window").filter(|&w| w > 0);
        let kinds: Option<Vec<&str>> = c["layer_types"]
            .as_array()
            .map(|a| a.iter().filter_map(serde_json::Value::as_str).collect());
        let period = num("sliding_window_pattern").filter(|&p| p > 0);
        let mut unknown: Vec<&str> = Vec::new();
        let layers = (0..n)
            .map(|i| {
                let kind = kinds.as_ref().and_then(|k| k.get(i).copied());
                let full = Layer::Full {
                    kv_heads,
                    k_len: head_dim,
                    v_len: head_dim,
                };
                let sliding = |window| Layer::Sliding {
                    kv_heads,
                    k_len: head_dim,
                    v_len: head_dim,
                    window,
                };
                match (kind, window) {
                    (None, _) if kv_heads == 0 => Layer::Recurrent,
                    (Some("sliding_attention"), Some(w)) => sliding(w),
                    // A sliding layer without its window is noted below.
                    (Some("sliding_attention" | "full_attention" | "attention"), _) => full,
                    (Some(k), _) if k.contains("linear") || k.contains("mamba") => Layer::Recurrent,
                    (Some(k), _) => {
                        if !unknown.contains(&k) {
                            unknown.push(k);
                        }
                        full
                    }
                    (None, Some(w)) if kinds.is_none() => match period {
                        Some(p) if !(i as u64 + 1).is_multiple_of(p) => sliding(w),
                        _ => full,
                    },
                    (None, _) => full,
                }
            })
            .collect::<Vec<_>>();
        if !unknown.is_empty() {
            confidence = confidence.min(Confidence::Medium);
            notes.push(format!(
                "layer types {} costed as full attention, which errs high",
                unknown.join(", ")
            ));
        }
        if kinds
            .as_ref()
            .is_some_and(|k| k.contains(&"sliding_attention"))
            && window.is_none()
        {
            confidence = confidence.min(Confidence::Medium);
            notes.push(
                "sliding_attention layers without a sliding_window: costed as full attention, which errs high".into(),
            );
        }
        if window.is_none() && kinds.is_none() && period.is_some() {
            confidence = confidence.min(Confidence::Medium);
            notes.push(
                "sliding_window_pattern is set but sliding_window is not: every layer is costed as full attention, which errs high".into(),
            );
        }
        if window.is_some() && kinds.is_none() && period.is_none() {
            confidence = confidence.min(Confidence::Medium);
            notes.push(
                "sliding_window is set but layer_types is not: every layer is costed as full attention, which errs high"
                    .into(),
            );
        }
        if layers.contains(&Layer::Recurrent) {
            confidence = confidence.min(Confidence::Medium);
            notes.push(
                "recurrent layers keep a per-sequence state that is not counted here".to_string(),
            );
        }
        Ok(Self {
            arch: arch.to_string(),
            trained_context: num("max_position_embeddings"),
            layers,
            confidence,
            notes,
        })
    }

    /// Bytes of KV cache for `cache_tokens` tokens of total context.
    ///
    /// `sequences` and `ubatch` only matter for sliding-window layers, whose
    /// cache holds `window × sequences + ubatch` tokens at most. When they are
    /// not given, sliding layers are costed as full layers, which errs high.
    #[must_use]
    pub fn kv_bytes(&self, cache_tokens: u64, k: CacheType, v: CacheType, swa: Option<Swa>) -> u64 {
        self.layers
            .iter()
            .map(|layer| {
                let (tokens, k_elems, v_elems) = match *layer {
                    Layer::Full {
                        kv_heads,
                        k_len,
                        v_len,
                    } => (
                        cache_tokens,
                        kv_heads.saturating_mul(k_len),
                        kv_heads.saturating_mul(v_len),
                    ),
                    Layer::Sliding {
                        kv_heads,
                        k_len,
                        v_len,
                        window,
                    } => {
                        let held = swa.map_or(cache_tokens, |s| {
                            cache_tokens
                                .min(window.saturating_mul(s.sequences).saturating_add(s.ubatch))
                        });
                        (
                            held,
                            kv_heads.saturating_mul(k_len),
                            kv_heads.saturating_mul(v_len),
                        )
                    }
                    Layer::Latent { k_len, v_len } => (cache_tokens, k_len, v_len),
                    Layer::Recurrent => (0, 0, 0),
                };
                tokens.saturating_mul(k.bytes(k_elems).saturating_add(v.bytes(v_elems)))
            })
            .fold(0, u64::saturating_add)
    }

    /// Largest total context whose KV cache fits in `budget` bytes, or `None`
    /// when not even one token fits.
    #[must_use]
    pub fn max_tokens(
        &self,
        budget: u64,
        k: CacheType,
        v: CacheType,
        swa: Option<Swa>,
    ) -> Option<u64> {
        // kv_bytes is monotonic in tokens, so binary search it. The upper end
        // is where a single layer's cost alone exhausts the budget.
        let per_token = self.kv_bytes(1, k, v, swa).max(1);
        let (mut lo, mut hi) = (0u64, budget / per_token + 1);
        while lo < hi {
            let mid = lo + (hi - lo).div_ceil(2);
            if self.kv_bytes(mid, k, v, swa) <= budget {
                lo = mid;
            } else {
                hi = mid - 1;
            }
        }
        (lo > 0).then_some(lo)
    }
}

/// Defaults Transformers' configuration classes fill in when a
/// `config.json` leaves a key out, for the model types whose configs are
/// known to rely on them (Gemma's `head_dim` is not hidden / heads). Format
/// facts, as GGUF's key defaults are.
fn hf_default(model_type: &str, key: &str) -> Option<u64> {
    match (model_type, key) {
        ("gemma" | "gemma2" | "gemma3" | "gemma3_text", "head_dim") => Some(256),
        ("gemma2" | "gemma3" | "gemma3_text", "num_attention_heads") => Some(8),
        ("gemma2" | "gemma3" | "gemma3_text", "num_key_value_heads") => Some(4),
        ("gemma2" | "gemma3" | "gemma3_text", "sliding_window") => Some(4096),
        // Gemma 2 alternates sliding and full layers.
        ("gemma2", "sliding_window_pattern") => Some(2),
        ("gemma3" | "gemma3_text", "sliding_window_pattern") => Some(6),
        _ => None,
    }
}

/// One latent-attention (MLA) layer from a Transformers config. Runtimes
/// differ: llama.cpp caches the compressed latent (`kv_lora_rank +
/// qk_rope_head_dim` per token), MLX and Transformers the expanded keys and
/// values (`heads × (qk_nope_head_dim + qk_rope_head_dim)` and `heads ×
/// v_head_dim`). The larger is costed, so the fit never says yes wrongly.
fn latent_layers(
    rank: u64,
    heads: u64,
    num: &dyn Fn(&str) -> Option<u64>,
    notes: &mut Vec<String>,
) -> Result<Layer> {
    let need = |k: &'static str| {
        num(k).ok_or_else(|| SenseError::Parse {
            what: "config.json geometry",
            detail: format!("latent attention without {k}"),
        })
    };
    let rope = need("qk_rope_head_dim")?;
    let nope = need("qk_nope_head_dim")?;
    let v = need("v_head_dim")?;
    let latent = Layer::Latent {
        k_len: rank.saturating_add(rope),
        v_len: rank,
    };
    let expanded = Layer::Full {
        kv_heads: heads,
        k_len: nope.saturating_add(rope),
        v_len: v,
    };
    let per_token = |l: Layer| match l {
        Layer::Latent { k_len, v_len } => k_len.saturating_add(v_len),
        Layer::Full {
            kv_heads,
            k_len,
            v_len,
        } => kv_heads.saturating_mul(k_len.saturating_add(v_len)),
        _ => 0,
    };
    notes.push(format!(
        "latent attention: a runtime caching the latent needs {} values per token per layer, one caching expanded keys and values (MLX) {}; the larger is costed",
        per_token(latent),
        per_token(expanded)
    ));
    Ok(if per_token(expanded) >= per_token(latent) {
        expanded
    } else {
        latent
    })
}

/// Runtime parameters that bound sliding-window caches.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct Swa {
    pub sequences: u64,
    pub ubatch: u64,
}

fn sliding_layers(
    window: Option<u64>,
    pattern: Option<&Value>,
    layers: usize,
    notes: &mut Vec<String>,
    confidence: &mut Confidence,
) -> Vec<Option<u64>> {
    let Some(window) = window.filter(|&w| w > 0) else {
        return vec![None; layers];
    };
    match pattern.and_then(Value::as_u64_list) {
        // Per-layer flags: non-zero marks a sliding layer.
        Some(flags) if flags.len() == layers => flags
            .into_iter()
            .map(|f| (f != 0).then_some(window))
            .collect(),
        // A period p: every p-th layer is full, the rest slide.
        Some(period) if period.len() == 1 && period[0] > 0 => (0..layers)
            .map(|i| (!(i as u64 + 1).is_multiple_of(period[0])).then_some(window))
            .collect(),
        _ => {
            *confidence = Confidence::Low;
            notes.push(format!(
                "sliding window {window} declared without a layer pattern; all layers costed as full attention (errs high)"
            ));
            vec![None; layers]
        }
    }
}

/// KV cache storage types a runtime can be told to use (llama.cpp
/// `--cache-type-k/v`, Ollama `OLLAMA_KV_CACHE_TYPE`). Sizes are ggml's block
/// layouts: bytes per block and elements per block.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheType {
    F32,
    F16,
    Bf16,
    Q8_0,
    Q5_1,
    Q5_0,
    Q4_1,
    Q4_0,
    Iq4Nl,
}

impl CacheType {
    const fn block(self) -> (u64, u64) {
        match self {
            CacheType::F32 => (4, 1),
            CacheType::F16 | CacheType::Bf16 => (2, 1),
            CacheType::Q8_0 => (34, 32),
            CacheType::Q5_1 => (24, 32),
            CacheType::Q5_0 => (22, 32),
            CacheType::Q4_1 => (20, 32),
            CacheType::Q4_0 | CacheType::Iq4Nl => (18, 32),
        }
    }

    /// Bytes to store `elements` values, rounded up to whole blocks.
    #[must_use]
    pub const fn bytes(self, elements: u64) -> u64 {
        let (bytes, per) = self.block();
        elements.div_ceil(per).saturating_mul(bytes)
    }

    /// Every cache type, largest per element first.
    pub const ALL: [CacheType; 9] = [
        CacheType::F32,
        CacheType::F16,
        CacheType::Bf16,
        CacheType::Q8_0,
        CacheType::Q5_1,
        CacheType::Q5_0,
        CacheType::Q4_1,
        CacheType::Q4_0,
        CacheType::Iq4Nl,
    ];

    /// The name runtimes accept for it on their command lines.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            CacheType::F32 => "f32",
            CacheType::F16 => "f16",
            CacheType::Bf16 => "bf16",
            CacheType::Q8_0 => "q8_0",
            CacheType::Q5_1 => "q5_1",
            CacheType::Q5_0 => "q5_0",
            CacheType::Q4_1 => "q4_1",
            CacheType::Q4_0 => "q4_0",
            CacheType::Iq4Nl => "iq4_nl",
        }
    }

    /// Parses the names runtimes accept on their command lines.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        Some(match name.to_ascii_lowercase().as_str() {
            "f32" => CacheType::F32,
            "f16" => CacheType::F16,
            "bf16" => CacheType::Bf16,
            "q8_0" => CacheType::Q8_0,
            "q5_1" => CacheType::Q5_1,
            "q5_0" => CacheType::Q5_0,
            "q4_1" => CacheType::Q4_1,
            "q4_0" => CacheType::Q4_0,
            "iq4_nl" => CacheType::Iq4Nl,
            _ => return None,
        })
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn transformers_config_gives_the_same_geometry_rules() {
        let cfg = serde_json::json!({
            "model_type": "gemma3",
            "text_config": {
                "num_hidden_layers": 4, "num_attention_heads": 8, "num_key_value_heads": 4,
                "head_dim": 256, "sliding_window": 1024, "max_position_embeddings": 131_072,
                "layer_types": ["sliding_attention", "sliding_attention", "sliding_attention", "full_attention"]
            }
        });
        let g = Geometry::from_hf_config(&cfg).unwrap();
        assert_eq!(g.layers.len(), 4);
        assert_eq!(g.trained_context, Some(131_072));
        assert!(matches!(g.layers[0], Layer::Sliding { window: 1024, .. }));
        assert!(matches!(
            g.layers[3],
            Layer::Full {
                kv_heads: 4,
                k_len: 256,
                ..
            }
        ));
        let plain = serde_json::json!({"model_type":"llama","num_hidden_layers":2,"num_attention_heads":32,"hidden_size":4096});
        let g = Geometry::from_hf_config(&plain).unwrap();
        assert!(matches!(
            g.layers[0],
            Layer::Full {
                kv_heads: 32,
                k_len: 128,
                ..
            }
        ));
        assert!(Geometry::from_hf_config(&serde_json::json!({})).is_err());
    }

    #[test]
    fn transformers_latent_attention_costs_the_larger_layout() {
        // DeepSeek-V3's attention: MLX caches expanded keys and values.
        let cfg = serde_json::json!({
            "model_type": "deepseek_v3", "num_hidden_layers": 61, "num_attention_heads": 128,
            "num_key_value_heads": 128, "hidden_size": 7168, "kv_lora_rank": 512,
            "qk_rope_head_dim": 64, "qk_nope_head_dim": 128, "v_head_dim": 128
        });
        let g = Geometry::from_hf_config(&cfg).unwrap();
        assert_eq!(g.confidence, Confidence::Medium);
        assert_eq!(
            g.layers[0],
            Layer::Full {
                kv_heads: 128,
                k_len: 192,
                v_len: 128
            }
        );
        let partial = serde_json::json!({"num_hidden_layers": 1, "num_attention_heads": 1, "kv_lora_rank": 512});
        assert!(Geometry::from_hf_config(&partial).is_err());
    }

    #[test]
    fn transformers_defaults_fill_gemma_configs() {
        // gemma-3-4b-it leaves the attention keys to Transformers' defaults.
        let cfg = serde_json::json!({
            "model_type": "gemma3",
            "text_config": {"model_type": "gemma3_text", "num_hidden_layers": 12,
                            "hidden_size": 2560, "sliding_window": 1024}
        });
        let g = Geometry::from_hf_config(&cfg).unwrap();
        assert_eq!(g.confidence, Confidence::High);
        assert!(matches!(
            g.layers[0],
            Layer::Sliding {
                kv_heads: 4,
                k_len: 256,
                window: 1024,
                ..
            }
        ));
        assert!(matches!(g.layers[5], Layer::Full { .. }));
        assert!(matches!(g.layers[11], Layer::Full { .. }));
        let odd = serde_json::json!({
            "num_hidden_layers": 2, "num_attention_heads": 4, "num_key_value_heads": 0, "head_dim": 64
        });
        assert!(
            Geometry::from_hf_config(&odd)
                .unwrap()
                .layers
                .iter()
                .all(|l| *l == Layer::Recurrent)
        );
        let conv = serde_json::json!({
            "num_hidden_layers": 2, "num_attention_heads": 4, "head_dim": 64,
            "layer_types": ["conv", "full_attention"]
        });
        let g = Geometry::from_hf_config(&conv).unwrap();
        assert!(g.notes.iter().any(|n| n.contains("conv")));
        // Gemma 2 alternates sliding and full layers, by default 4096 wide.
        let g2 = serde_json::json!({"model_type": "gemma2", "num_hidden_layers": 4, "hidden_size": 2304});
        let g = Geometry::from_hf_config(&g2).unwrap();
        assert!(matches!(g.layers[0], Layer::Sliding { window: 4096, .. }));
        assert!(matches!(g.layers[1], Layer::Full { .. }));
        // A zero rank is no latent attention.
        let dense = serde_json::json!({"num_hidden_layers": 1, "num_attention_heads": 2, "head_dim": 8, "kv_lora_rank": 0});
        assert!(matches!(
            Geometry::from_hf_config(&dense).unwrap().layers[0],
            Layer::Full { .. }
        ));
        let huge = serde_json::json!({"num_hidden_layers": u64::MAX >> 4, "num_attention_heads": 2, "head_dim": 8});
        assert!(Geometry::from_hf_config(&huge).is_err());
    }

    use super::*;
    use crate::gguf::tests::{build, s, u32_array, u32v};
    use std::io::Cursor;

    fn header(entries: &[(&str, u32, Vec<u8>)]) -> Header {
        let bytes = build(entries);
        crate::gguf::parse(&mut Cursor::new(&bytes), bytes.len() as u64).unwrap()
    }

    fn qwen3_06b() -> Header {
        // The metadata of Qwen3-0.6B as published in its GGUF.
        header(&[
            ("general.architecture", 8, s("qwen3")),
            ("qwen3.block_count", 4, u32v(28)),
            ("qwen3.context_length", 4, u32v(40960)),
            ("qwen3.embedding_length", 4, u32v(1024)),
            ("qwen3.attention.head_count", 4, u32v(16)),
            ("qwen3.attention.head_count_kv", 4, u32v(8)),
            ("qwen3.attention.key_length", 4, u32v(128)),
            ("qwen3.attention.value_length", 4, u32v(128)),
        ])
    }

    #[test]
    fn dense_model_kv_matches_hand_arithmetic() {
        let g = Geometry::from_header(&qwen3_06b()).unwrap();
        assert_eq!(g.confidence, Confidence::High);
        // 28 layers × 8 heads × (128 + 128) × 2 bytes = 114688 bytes per token.
        assert_eq!(g.kv_bytes(1, CacheType::F16, CacheType::F16, None), 114_688);
        assert_eq!(
            g.kv_bytes(40960, CacheType::F16, CacheType::F16, None),
            114_688 * 40960
        );
        // q8_0: 1024 values per layer-head-set → 32 blocks × 34 bytes.
        assert_eq!(
            g.kv_bytes(1, CacheType::Q8_0, CacheType::Q8_0, None),
            28 * 2 * (1024 / 32) * 34
        );
    }

    #[test]
    fn max_tokens_inverts_kv_bytes() {
        let g = Geometry::from_header(&qwen3_06b()).unwrap();
        let (k, v) = (CacheType::F16, CacheType::F16);
        let budget = g.kv_bytes(12_345, k, v, None);
        assert_eq!(g.max_tokens(budget, k, v, None), Some(12_345));
        assert_eq!(g.max_tokens(budget - 1, k, v, None), Some(12_344));
        assert_eq!(g.max_tokens(0, k, v, None), None);
    }

    #[test]
    fn defaults_follow_the_spec() {
        // No head_count_kv and no key/value length: both default per spec.
        let g = Geometry::from_header(&header(&[
            ("general.architecture", 8, s("llama")),
            ("llama.block_count", 4, u32v(2)),
            ("llama.embedding_length", 4, u32v(4096)),
            ("llama.attention.head_count", 4, u32v(32)),
        ]))
        .unwrap();
        assert_eq!(
            g.layers[0],
            Layer::Full {
                kv_heads: 32,
                k_len: 128,
                v_len: 128
            }
        );
    }

    #[test]
    fn sliding_window_with_period_and_without_pattern() {
        let base = |extra: Vec<(&'static str, u32, Vec<u8>)>| {
            let mut e = vec![
                ("general.architecture", 8, s("gemma3")),
                ("gemma3.block_count", 4, u32v(6)),
                ("gemma3.attention.head_count", 4, u32v(8)),
                ("gemma3.attention.head_count_kv", 4, u32v(4)),
                ("gemma3.attention.key_length", 4, u32v(256)),
                ("gemma3.attention.value_length", 4, u32v(256)),
                ("gemma3.attention.sliding_window", 4, u32v(1024)),
            ];
            e.extend(extra);
            Geometry::from_header(&header(&e)).unwrap()
        };
        let with = base(vec![(
            "gemma3.attention.sliding_window_pattern",
            4,
            u32v(3),
        )]);
        let sliding = with
            .layers
            .iter()
            .filter(|l| matches!(l, Layer::Sliding { .. }))
            .count();
        assert_eq!(sliding, 4); // layers 3 and 6 are full
        let swa = Some(Swa {
            sequences: 1,
            ubatch: 512,
        });
        let long = with.kv_bytes(100_000, CacheType::F16, CacheType::F16, swa);
        let short = with.kv_bytes(1_536, CacheType::F16, CacheType::F16, swa);
        // Beyond window + ubatch only the full layers keep growing.
        let per_full_token = 2 * (4 * 256 * 2 * 2);
        assert_eq!(long - short, (100_000 - 1_536) * per_full_token);

        let without = base(vec![]);
        assert_eq!(without.confidence, Confidence::Low);
        assert!(
            without
                .layers
                .iter()
                .all(|l| matches!(l, Layer::Full { .. }))
        );
    }

    #[test]
    fn hybrid_recurrent_layers_cost_nothing_per_token() {
        let g = Geometry::from_header(&header(&[
            ("general.architecture", 8, s("hybrid")),
            ("hybrid.block_count", 4, u32v(4)),
            ("hybrid.attention.head_count", 4, u32v(8)),
            (
                "hybrid.attention.head_count_kv",
                9,
                u32_array(&[0, 0, 0, 2]),
            ),
            ("hybrid.attention.key_length", 4, u32v(128)),
            ("hybrid.attention.value_length", 4, u32v(128)),
        ]))
        .unwrap();
        assert_eq!(g.confidence, Confidence::Medium);
        assert_eq!(
            g.kv_bytes(1, CacheType::F16, CacheType::F16, None),
            2 * (128 + 128) * 2
        );
    }

    #[test]
    fn cache_type_names_and_block_sizes() {
        assert_eq!(CacheType::parse("Q8_0"), Some(CacheType::Q8_0));
        assert_eq!(CacheType::parse("q3_k"), None);
        assert_eq!(CacheType::Q4_0.bytes(64), 36);
        assert_eq!(CacheType::Q4_0.bytes(33), 36);
        assert_eq!(CacheType::F32.bytes(3), 12);
    }
}
