//! What a runtime's own log lines say about its health.
//!
//! These are facts of the runtimes' source code, matched case-insensitively
//! as substrings. A line can mean more than one thing; the most serious
//! reading wins.

use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Signal {
    /// The accelerator ran out of memory for one command buffer. On Apple
    /// Silicon llama.cpp latches after this; the next decode reports it.
    OutOfMemory,
    /// The backend is in a permanent error state and fails every request,
    /// quickly, while `/health` still answers ok. Only a restart cures it.
    Poisoned,
}

/// `(substring, signal)`, lowercase.
const SIGNATURES: &[(&str, Signal)] = &[
    // ggml-metal (ggml-metal-context.m, `ggml_metal_synchronize`): logged when
    // a command buffer did not complete, right where the context sets
    // `has_error`. The backend is latched from this line on; the request that
    // logged it is the first of all the ones that will fail.
    // "%s: error: command buffer %d failed with status %d". The capture path
    // logs a similar line at INFO level without latching, under another
    // function name, so the function name is part of the signature.
    (
        "ggml_metal_synchronize: error: command buffer",
        Signal::Poisoned,
    ),
    // ggml-metal (ggml-metal-context.m), logged on every graph compute after
    // one command buffer failed: "%s: backend is in error state from a
    // previous command buffer failure - recreate the backend to recover".
    ("recreate the backend to recover", Signal::Poisoned),
    (
        "error state from a previous command buffer failure",
        Signal::Poisoned,
    ),
    // Metal's description of a command buffer that could not get memory,
    // which ggml-metal logs as "error: %s" after "command buffer %d failed".
    (
        "kiogpucommandbuffercallbackerroroutofmemory",
        Signal::OutOfMemory,
    ),
    // CUDA and Vulkan allocation failures inside ggml.
    ("cuda error: out of memory", Signal::OutOfMemory),
    ("erroroutofdevicememory", Signal::OutOfMemory),
];

/// The most serious signal in `line`, if any.
#[must_use]
pub fn classify(line: &str) -> Option<Signal> {
    let line = line.to_ascii_lowercase();
    SIGNATURES
        .iter()
        .filter(|(needle, _)| line.contains(needle))
        .map(|(_, signal)| *signal)
        .max()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognises_the_latched_metal_backend() {
        // The format string from ggml-metal-context.m, filled in.
        let line = "ggml_metal_graph_compute: backend is in error state from a previous command buffer failure - recreate the backend to recover";
        assert_eq!(classify(line), Some(Signal::Poisoned));
        assert_eq!(
            classify(
                "error: Insufficient Memory (00000008:kIOGPUCommandBufferCallbackErrorOutOfMemory)"
            ),
            Some(Signal::OutOfMemory)
        );
    }

    #[test]
    fn the_failing_command_buffer_is_the_moment_metal_latches() {
        assert_eq!(
            classify("ggml_metal_synchronize: error: command buffer 0 failed with status 5"),
            Some(Signal::Poisoned)
        );
        // The capture path's line does not latch the backend.
        assert_eq!(
            classify("ggml_metal_graph_compute: command buffer 0 failed with status 5"),
            None
        );
    }

    #[test]
    fn ordinary_lines_mean_nothing() {
        for line in [
            "srv  log_server_r: request: POST /v1/chat/completions 127.0.0.1 200",
            "llama_kv_cache: size = 4096.00 MiB",
            "main: server is listening on http://127.0.0.1:8080",
        ] {
            assert_eq!(classify(line), None, "{line}");
        }
    }
}
