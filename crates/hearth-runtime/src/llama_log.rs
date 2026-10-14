//! What llama.cpp's own log says it allocated for compute: the
//! `llama_context: Metal compute buffer size = N MiB` lines it prints
//! while loading, one per backend. Compute buffers are the part of a
//! loaded model no file states in advance, so a fit learns them from a
//! start that logged them.

/// Bytes per mebibyte, the unit llama.cpp prints buffer sizes in.
const MIB: f64 = 1_048_576.0;

/// The compute buffer bytes one log line reports, if it is such a line.
#[must_use]
pub fn compute_bytes(line: &str) -> Option<u64> {
    // llama.cpp prints these in lowercase; most lines are not, and are
    // passed over without copying.
    let (head, rest) = line.split_once("buffer size =")?;
    if !head.to_ascii_lowercase().contains("compute") {
        return None;
    }
    let mut words = rest.split_whitespace();
    let mib = words.next()?.parse::<f64>().ok()?;
    if words.next()? != "MiB" || !mib.is_finite() || mib < 0.0 {
        return None;
    }
    // Saturating float-to-int conversion: a nonsense size cannot wrap.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    Some((mib * MIB).round() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_llama_cpp_compute_buffer_lines_only() {
        let total: u64 = [
            "load_tensors:   Metal_Mapped model buffer size =   604.50 MiB",
            "llama_kv_cache:      Metal KV buffer size =   448.00 MiB",
            "llama_context:      Metal compute buffer size =   300.75 MiB",
            "llama_context:        CPU compute buffer size =    16.01 MiB",
            "llama_context:  CPU  output buffer size =     0.58 MiB",
            "srv  load_model: loading model",
            "x compute buffer size = nan MiB",
        ]
        .iter()
        .filter_map(|l| compute_bytes(l))
        .sum();
        // 300.75 MiB + 16.01 MiB, each rounded to whole bytes.
        assert_eq!(total, 315_359_232 + 16_787_702);
    }
}
