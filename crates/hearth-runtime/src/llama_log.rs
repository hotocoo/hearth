//! What llama.cpp's own log says it allocated: the "... buffer size = N
//! MiB" lines it prints while loading (`load_tensors`, `llama_kv_cache`,
//! `llama_context`). The compute buffers are the part of a loaded model no
//! file says in advance, so a fit learns them from a start that logged them.

use serde::{Deserialize, Serialize};

/// Bytes per mebibyte, the unit llama.cpp prints buffer sizes in.
const MIB: f64 = 1_048_576.0;

/// Bytes llama.cpp reported allocating, by kind, summed over its devices.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Buffers {
    pub model_bytes: u64,
    pub kv_bytes: u64,
    pub compute_bytes: u64,
}

impl Buffers {
    /// Adds what one log line reports, if it is a buffer line.
    pub fn add_line(&mut self, line: &str) {
        let lower = line.to_ascii_lowercase();
        let Some((head, rest)) = lower.split_once("buffer size =") else {
            return;
        };
        let mut words = rest.split_whitespace();
        let (Some(number), Some("mib")) = (words.next(), words.next()) else {
            return;
        };
        let Ok(mib) = number.parse::<f64>() else {
            return;
        };
        if !mib.is_finite() || mib < 0.0 {
            return;
        }
        // Saturating float-to-int conversion: a nonsense size cannot wrap.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let bytes = (mib * MIB).round() as u64;
        let slot = if head.contains("compute") {
            &mut self.compute_bytes
        } else if head.contains("kv") {
            &mut self.kv_bytes
        } else if head.contains("model") {
            &mut self.model_bytes
        } else {
            return;
        };
        *slot = slot.saturating_add(bytes);
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sums_llama_cpp_buffer_lines_by_kind() {
        let mut b = Buffers::default();
        for line in [
            "load_tensors:   Metal_Mapped model buffer size =   604.50 MiB",
            "load_tensors:    CPU_Mapped model buffer size =    10.00 MiB",
            "llama_kv_cache:      Metal KV buffer size =   448.00 MiB",
            "llama_context:      Metal compute buffer size =   300.75 MiB",
            "llama_context:        CPU compute buffer size =    16.01 MiB",
            "llama_context:  CPU  output buffer size =     0.58 MiB",
            "srv  load_model: loading model",
            "x buffer size = nan MiB",
        ] {
            b.add_line(line);
        }
        assert_eq!(b.model_bytes, 644_349_952);
        assert_eq!(b.kv_bytes, 469_762_048);
        // 300.75 MiB + 16.01 MiB, each rounded to whole bytes.
        assert_eq!(b.compute_bytes, 315_359_232 + 16_787_702);
        assert!(Buffers::default().is_empty());
    }
}
