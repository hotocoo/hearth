//! `hearth mcp`: the same commands as tools for AI agents, over the Model
//! Context Protocol (JSON-RPC 2.0, one message per line on stdin/stdout).
//!
//! Tool arguments are turned back into a command line and parsed by the same
//! `clap` definitions the CLI uses, so an agent and a person get identical
//! validation and identical answers.

use std::io::{BufRead, Write};

use serde_json::{Value, json};

/// Protocol revisions this server speaks. The server answers with the
/// client's requested revision when it is one of these, else the newest.
const PROTOCOL_VERSIONS: &[&str] = &["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

pub fn serve() -> Result<String, String> {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout().lock();
    for line in stdin.lock().lines() {
        let line = line.map_err(|e| e.to_string())?;
        if line.trim().is_empty() {
            continue;
        }
        let reply = match serde_json::from_str::<Value>(&line) {
            Ok(msg) => handle(&msg),
            Err(e) => Some(error(&Value::Null, -32700, &format!("parse error: {e}"))),
        };
        if let Some(reply) = reply {
            writeln!(stdout, "{reply}")
                .and_then(|()| stdout.flush())
                .map_err(|e| e.to_string())?;
        }
    }
    Ok(String::new())
}

fn error(id: &Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

#[allow(clippy::needless_pass_by_value)] // every caller builds the value inline
fn ok(id: &Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

/// `None` for notifications, which get no reply.
fn handle(msg: &Value) -> Option<Value> {
    let id = msg.get("id")?;
    let params = &msg["params"];
    Some(match msg["method"].as_str() {
        Some("initialize") => {
            let asked = params["protocolVersion"].as_str();
            let version = asked
                .filter(|v| PROTOCOL_VERSIONS.contains(v))
                .unwrap_or(PROTOCOL_VERSIONS[0]);
            ok(
                id,
                json!({
                    "protocolVersion": version,
                    "capabilities": { "tools": {} },
                    "serverInfo": { "name": "hearth", "version": env!("CARGO_PKG_VERSION") },
                    "instructions": "hearth inspects a machine that runs local models. \
                        Use doctor before loading a model or when the machine is slow, \
                        incidents after a crash or reboot, fit before starting a model \
                        server, process_memory to see what a server really uses."
                }),
            )
        }
        Some("ping") => ok(id, json!({})),
        Some("tools/list") => ok(id, json!({ "tools": tools() })),
        Some("tools/call") => {
            let name = params["name"].as_str().unwrap_or_default();
            match call(name, &params["arguments"]) {
                Ok(value) => ok(id, tool_result(&value, false)),
                Err(e) => ok(id, tool_result(&json!({ "error": e }), true)),
            }
        }
        Some(other) => error(id, -32601, &format!("method not found: {other}")),
        None => error(id, -32600, "missing method"),
    })
}

fn tool_result(value: &Value, is_error: bool) -> Value {
    // Structured content must be an object; lists are wrapped.
    let structured = if value.is_object() {
        value.clone()
    } else {
        json!({ "items": value })
    };
    json!({
        "content": [{ "type": "text", "text": serde_json::to_string_pretty(value).unwrap_or_default() }],
        "structuredContent": structured,
        "isError": is_error,
    })
}

fn tools() -> Value {
    json!([
        {
            "name": "doctor",
            "description": "Memory (available, kernel pressure, swap), open files and threads against the kernel's limits; the processes holding half of each; findings with remediation. With watch_seconds, samples twice and projects time to each limit from measured growth (use it to catch descriptor or thread leaks).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "watch_seconds": { "type": "integer", "minimum": 1, "description": "Seconds between the two samples." }
                }
            }
        },
        {
            "name": "incidents",
            "description": "Kernel panics (classified watchdog / GPU memory driver / other), jetsam kills with the largest process, shutdown stalls, and optionally resource-limit reports, read from the OS's own crash reports.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "since": { "type": "string", "description": "Time prefix, e.g. 2026-10 or 2026-10-12." },
                    "all": { "type": "boolean", "description": "Include resource-limit reports (disk writes, CPU, wakeups)." }
                }
            }
        },
        {
            "name": "fit",
            "description": "Whether a GGUF model at a total KV context fits this machine's accelerator and fits right now, with weights and KV bytes from the model's own metadata and the largest context that would fit. Excludes runtime compute buffers.",
            "inputSchema": {
                "type": "object",
                "required": ["model", "ctx", "cache_type"],
                "properties": {
                    "model": { "type": "string", "description": "Path to a .gguf file (any shard of a split model)." },
                    "ctx": { "type": "integer", "minimum": 1, "description": "Total tokens of KV cache the runtime will allocate (llama.cpp -c; Ollama num_ctx x OLLAMA_NUM_PARALLEL)." },
                    "cache_type": { "type": "string", "description": "KV cache type: f32 f16 bf16 q8_0 q5_1 q5_0 q4_1 q4_0 iq4_nl." },
                    "cache_type_v": { "type": "string", "description": "Value cache type when it differs from cache_type." },
                    "parallel": { "type": "integer", "minimum": 1, "description": "Parallel sequences (with ubatch, bounds sliding-window caches)." },
                    "ubatch": { "type": "integer", "minimum": 1, "description": "Physical batch size (with parallel)." }
                }
            }
        },
        {
            "name": "process_memory",
            "description": "Memory the kernel charges to processes whose name or path contains a pattern, or to one pid: footprint (includes GPU allocations on Apple Silicon), resident, lifetime peak.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "pattern": { "type": "string" },
                    "pid": { "type": "integer" }
                }
            }
        }
    ])
}

fn call(name: &str, args: &Value) -> Result<Value, String> {
    let mut command_line: Vec<String> = vec!["hearth".into(), "--json".into()];
    match name {
        "doctor" => {
            command_line.push("doctor".into());
            if let Some(s) = args["watch_seconds"].as_u64() {
                command_line.extend(["--watch".into(), s.to_string()]);
            }
        }
        "incidents" => {
            command_line.push("incidents".into());
            if let Some(s) = args["since"].as_str() {
                command_line.extend(["--since".into(), s.into()]);
            }
            if args["all"].as_bool() == Some(true) {
                command_line.push("--all".into());
            }
        }
        "fit" => {
            command_line.push("fit".into());
            command_line.push(args["model"].as_str().ok_or("model is required")?.into());
            command_line.extend([
                "--ctx".into(),
                args["ctx"].as_u64().ok_or("ctx is required")?.to_string(),
                "--cache-type".into(),
                args["cache_type"]
                    .as_str()
                    .ok_or("cache_type is required")?
                    .into(),
            ]);
            if let Some(v) = args["cache_type_v"].as_str() {
                command_line.extend(["--cache-type-v".into(), v.into()]);
            }
            for key in ["parallel", "ubatch"] {
                if let Some(n) = args[key].as_u64() {
                    command_line.extend([format!("--{key}"), n.to_string()]);
                }
            }
        }
        "process_memory" => return process_memory(args),
        other => return Err(format!("unknown tool {other}")),
    }
    let text = crate::dispatch(&command_line)?;
    serde_json::from_str(&text).map_err(|e| e.to_string())
}

fn process_memory(args: &Value) -> Result<Value, String> {
    let pid = args["pid"].as_i64().and_then(|p| i32::try_from(p).ok());
    let pattern = args["pattern"].as_str();
    if pid.is_none() && pattern.is_none() {
        return Err("pass pattern or pid".into());
    }
    let processes = hearth_sense::list_processes().map_err(|e| e.to_string())?;
    let matches: Vec<Value> = processes
        .into_iter()
        .filter(|p| Some(p.pid) == pid || pattern.is_some_and(|m| p.matches(m)))
        .map(|p| match hearth_sense::process_footprint(p.pid) {
            Ok(fp) => json!({ "process": p, "memory": fp }),
            Err(e) => json!({ "process": p, "error": e.to_string() }),
        })
        .collect();
    Ok(json!({ "processes": matches }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initialize_negotiates_version() {
        let reply = handle(&json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}})).unwrap();
        assert_eq!(reply["result"]["protocolVersion"], "2025-06-18");
        let reply = handle(&json!({"jsonrpc":"2.0","id":2,"method":"initialize","params":{"protocolVersion":"1999-01-01"}})).unwrap();
        assert_eq!(reply["result"]["protocolVersion"], PROTOCOL_VERSIONS[0]);
    }

    #[test]
    fn notifications_get_no_reply_and_unknown_methods_error() {
        assert!(handle(&json!({"jsonrpc":"2.0","method":"notifications/initialized"})).is_none());
        let reply = handle(&json!({"jsonrpc":"2.0","id":3,"method":"nope"})).unwrap();
        assert_eq!(reply["error"]["code"], -32601);
    }

    #[test]
    fn tools_are_listed_with_schemas() {
        let reply = handle(&json!({"jsonrpc":"2.0","id":4,"method":"tools/list"})).unwrap();
        let tools = reply["result"]["tools"].as_array().unwrap();
        assert!(tools.iter().all(|t| t["inputSchema"]["type"] == "object"));
    }

    #[test]
    fn tool_errors_are_reported_in_band() {
        let reply = handle(&json!({"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"fit","arguments":{}}})).unwrap();
        assert_eq!(reply["result"]["isError"], true);
        let reply = handle(&json!({"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"process_memory","arguments":{"pid": std::process::id()}}})).unwrap();
        assert_eq!(reply["result"]["isError"], false);
        assert_eq!(
            reply["result"]["structuredContent"]["processes"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
    }
}
