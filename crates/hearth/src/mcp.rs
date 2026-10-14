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
                    "capabilities": { "tools": {}, "prompts": {} },
                    "serverInfo": { "name": "hearth", "version": env!("CARGO_PKG_VERSION") },
                    "instructions": "hearth inspects a machine that runs local models. \
                        Use doctor before loading a model or when the machine is slow, \
                        incidents after a crash or reboot, fit before starting a model \
                        server, ports to find which runtimes run where and whether one is wedged, \
                        process_memory to see what a server really uses, \
                        log and diff to see what changed on the machine since an earlier ports or doctor run."
                }),
            )
        }
        Some("ping") => ok(id, json!({})),
        Some("tools/list") => ok(id, json!({ "tools": tools() })),
        Some("prompts/list") => ok(id, json!({ "prompts": prompt_list() })),
        Some("prompts/get") => match prompt(
            params["name"].as_str().unwrap_or_default(),
            &params["arguments"],
        ) {
            Ok(result) => ok(id, result),
            Err(e) => error(id, -32602, &e),
        },
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

/// Diagnostic workflows: which tools to call in which order for a question
/// agents keep being asked. Kept as data next to this file.
fn prompts() -> Vec<Value> {
    serde_json::from_str::<Vec<Value>>(include_str!("mcp-prompts.json")).unwrap_or_default()
}

fn prompt_list() -> Vec<Value> {
    prompts()
        .into_iter()
        .map(|mut p| {
            // Only what the protocol defines: the template and the
            // argument defaults stay on this side.
            if let Some(o) = p.as_object_mut() {
                o.remove("template");
                for arg in o
                    .get_mut("arguments")
                    .and_then(Value::as_array_mut)
                    .into_iter()
                    .flatten()
                {
                    if let Some(a) = arg.as_object_mut() {
                        a.remove("default");
                    }
                }
            }
            p
        })
        .collect()
}

/// The prompt `name` with `arguments` filled in. A missing argument takes
/// its documented default; a missing required one is an error.
fn prompt(name: &str, arguments: &Value) -> Result<Value, String> {
    let p = prompts()
        .into_iter()
        .find(|p| p["name"] == name)
        .ok_or_else(|| format!("no prompt named {name:?}; prompts/list has them"))?;
    let mut text = p["template"].as_str().unwrap_or_default().to_string();
    for arg in p["arguments"].as_array().into_iter().flatten() {
        let key = arg["name"].as_str().unwrap_or_default();
        let given = arguments[key].as_str().map(str::to_string).or_else(|| {
            arguments[key]
                .is_number()
                .then(|| arguments[key].to_string())
        });
        let value = match (given, arg["default"].as_str()) {
            (Some(v), _) => v,
            (None, Some(default)) => default.to_string(),
            (None, None) if arg["required"] == true => {
                return Err(format!("{name} needs the argument {key}"));
            }
            (None, None) => "not given".into(),
        };
        text = text.replace(&format!("{{{key}}}"), &value);
    }
    Ok(json!({
        "description": p["description"],
        "messages": [{ "role": "user", "content": { "type": "text", "text": text } }],
    }))
}

/// The tool list, kept as data next to this file. Its validity is checked by
/// the tests below.
fn tools() -> Value {
    serde_json::from_str(include_str!("mcp-tools.json")).unwrap_or_else(
        |e| json!([{ "name": "broken", "description": format!("mcp-tools.json: {e}") }]),
    )
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
            match (args["model"].as_str(), args["port"].as_u64()) {
                (None, None) => return Err("model or port is required".into()),
                (model, port) => {
                    command_line.extend(model.map(str::to_string));
                    if let Some(p) = port {
                        command_line.extend(["--port".into(), p.to_string()]);
                    }
                }
            }
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
        "ports" => {
            argv_push_ports(&mut command_line, args);
        }
        "log" | "diff" => {
            command_line.push(name.into());
            if let Some(kind) = args["kind"].as_str() {
                command_line.extend(["--kind".into(), kind.into()]);
            }
            if let Some(n) = args["limit"].as_u64() {
                command_line.extend(["--limit".into(), n.to_string()]);
            }
            for key in ["from", "to"] {
                if let Some(r) = args[key].as_str() {
                    command_line.extend([format!("--{key}"), r.into()]);
                }
            }
        }
        "status" => command_line.push("status".into()),
        "restart" => {
            command_line.push("restart".into());
            command_line.push(args["server"].as_str().ok_or("server is required")?.into());
        }
        "config" => {
            command_line.push("config".into());
            if let Some(f) = args["filter"].as_str() {
                command_line.push(f.into());
            }
        }
        "process_memory" => return process_memory(args),
        other => return Err(format!("unknown tool {other}")),
    }
    let text = crate::dispatch(&command_line)?;
    serde_json::from_str(&text).map_err(|e| e.to_string())
}

fn argv_push_ports(command_line: &mut Vec<String>, args: &Value) {
    command_line.push("ports".into());
    if args["runtimes_only"].as_bool() == Some(true) {
        command_line.push("--runtimes".into());
    }
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
    fn prompts_are_listed_and_filled_in() {
        let reply = handle(&json!({"jsonrpc":"2.0","id":7,"method":"prompts/list"})).unwrap();
        let listed = reply["result"]["prompts"].as_array().unwrap();
        assert!(listed.len() >= 4);
        assert!(listed.iter().all(|p| p.get("template").is_none()));
        let allowed = ["name", "description", "required"];
        for arg in listed
            .iter()
            .flat_map(|p| p["arguments"].as_array().unwrap())
        {
            let keys: Vec<&String> = arg.as_object().unwrap().keys().collect();
            assert!(
                keys.iter().all(|k| allowed.contains(&k.as_str())),
                "{keys:?}"
            );
        }
        let tools: Vec<String> = tools()
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_string())
            .collect();
        // Every tool a workflow names must exist.
        for p in prompts() {
            let text = p["template"].as_str().unwrap();
            for named in text.split('`').skip(1).step_by(2) {
                assert!(
                    tools.contains(&named.to_string()),
                    "{}: `{named}` is not a tool",
                    p["name"]
                );
            }
        }
        let reply = handle(&json!({"jsonrpc":"2.0","id":8,"method":"prompts/get",
            "params":{"name":"can-i-load","arguments":{"model":"qwen3:8b","ctx":"32768"}}}))
        .unwrap();
        let text = reply["result"]["messages"][0]["content"]["text"]
            .as_str()
            .unwrap();
        assert!(text.contains("qwen3:8b") && text.contains("32768") && !text.contains("{model}"));
        let missing = handle(&json!({"jsonrpc":"2.0","id":9,"method":"prompts/get",
            "params":{"name":"can-i-load","arguments":{}}}))
        .unwrap();
        assert_eq!(missing["error"]["code"], -32602);
        let reply = handle(&json!({"jsonrpc":"2.0","id":10,"method":"prompts/get",
            "params":{"name":"what-changed"}}))
        .unwrap();
        let text = reply["result"]["messages"][0]["content"]["text"]
            .as_str()
            .unwrap();
        assert!(text.contains("\"~1\""), "defaults fill in");
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
