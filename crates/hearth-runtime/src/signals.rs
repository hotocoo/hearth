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

/// Why a server that exited did, as its own log said it. Read from the
/// lines it printed since it started; the first that says wins, because
/// later lines are usually the consequence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Cause {
    /// The model file is missing or could not be opened: a restart cannot
    /// help until the path is fixed.
    ModelMissing,
    /// The file opened but did not load (corrupt, unsupported, wrong arch).
    LoadFailed,
    /// An allocation on the accelerator or host failed: the model, its
    /// context or its cache does not fit beside what else is loaded.
    OutOfMemory,
    /// Its port was taken.
    PortInUse,
    /// It refused its command line.
    BadArgument,
}

impl Cause {
    /// Whether starting the same command again can only fail the same way.
    /// A taken port may free up, and memory may be released by others.
    #[must_use]
    pub const fn is_permanent(self) -> bool {
        matches!(
            self,
            Self::ModelMissing | Self::LoadFailed | Self::BadArgument
        )
    }
}

/// `(substring, cause)`, lowercase, from llama.cpp's own messages.
const CAUSES: &[(&str, Cause)] = &[
    // gguf.cpp: "%s: failed to open GGUF file '%s'"
    ("failed to open gguf file", Cause::ModelMissing),
    // ggml backends: "failed to allocate buffer of size", "%s: failed to
    // allocate %s buffer", CUDA's "cudaMalloc failed: out of memory".
    // Before the load failures: a load that failed to allocate is memory.
    ("failed to allocate", Cause::OutOfMemory),
    ("out of memory", Cause::OutOfMemory),
    (
        "kiogpucommandbuffercallbackerroroutofmemory",
        Cause::OutOfMemory,
    ),
    // llama.cpp: "llama_model_load: error loading model: %s", then
    // server.cpp: "failed to load model, '%s'"
    ("error loading model", Cause::LoadFailed),
    ("failed to load model", Cause::LoadFailed),
    // server.cpp: "couldn't bind HTTP server socket, hostname: %s, port: %d"
    ("couldn't bind http server socket", Cause::PortInUse),
    ("address already in use", Cause::PortInUse),
    // common/arg.cpp: "error: invalid argument: %s", "error: unknown argument: %s"
    ("error: invalid argument", Cause::BadArgument),
    ("error: unknown argument", Cause::BadArgument),
];

/// What `line` says about why the server would stop, if anything.
#[must_use]
pub fn cause(line: &str) -> Option<Cause> {
    let line = line.to_ascii_lowercase();
    CAUSES
        .iter()
        .find(|(needle, _)| line.contains(needle))
        .map(|(_, c)| *c)
}

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
    fn exit_causes_from_llama_cpp_messages() {
        for (line, want) in [
            (
                "gguf_init_from_file_impl: failed to open GGUF file '/m/x.gguf' (No such file or directory)",
                Cause::ModelMissing,
            ),
            (
                "llama_model_load: error loading model: unknown model architecture: 'foo'",
                Cause::LoadFailed,
            ),
            (
                "ggml_backend_metal_buffer_type_alloc_buffer: error: failed to allocate buffer, size = 35000.00 MiB",
                Cause::OutOfMemory,
            ),
            (
                "srv    init: couldn't bind HTTP server socket, hostname: 127.0.0.1, port: 8080",
                Cause::PortInUse,
            ),
            ("error: invalid argument: --ctx-sizee", Cause::BadArgument),
        ] {
            assert_eq!(cause(line), Some(want), "{line}");
        }
        assert_eq!(
            cause("main: server is listening on http://127.0.0.1:8080"),
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
