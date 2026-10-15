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
    let partial =
        serde_json::json!({"num_hidden_layers": 1, "num_attention_heads": 1, "kv_lora_rank": 512});
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
    let g2 =
        serde_json::json!({"model_type": "gemma2", "num_hidden_layers": 4, "hidden_size": 2304});
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
