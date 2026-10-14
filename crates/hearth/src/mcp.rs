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
                    "instructions": instructions("hearth inspects a machine that runs local models and keeps the servers hearthd owns alive. \
                        Start with a workflow from prompts/list when one fits (diagnose-slow-machine, server-died, can-i-load, what-changed, find-a-leak). \
                        Otherwise: doctor for memory, files and threads; ports for what runs where (owned_by says what hearthd owns); \
                        incidents for crashes and kills; fit before loading a model; \
                        log, diff, show, follow and tag for history (references: ~N, @2h, ids, tag names); \
                        status, logs, restart, stop, start and reload for hearthd's servers (only those: never act on a process hearth did not start); \
                        config for why hearth did something; process_memory for what one process really uses; bundle for everything at once."),
                }),
            )
        }
        Some("ping") => ok(id, json!({})),
        Some("tools/list") => ok(id, json!({ "tools": listed_tools() })),
        Some("prompts/list") => ok(id, json!({ "prompts": prompt_list(prompts()) })),
        Some("prompts/get") => match prompt(
            &prompts(),
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

/// The workflows, built-in and the user's (see [`crate::workflows`]).
/// What was wrong with the user's file goes to stderr, where MCP clients
/// show a server's log, and into `initialize`'s instructions.
fn prompts() -> Vec<Value> {
    let (all, problems) = crate::workflows::all(&tool_names());
    for p in &problems {
        eprintln!("hearth: workflow skipped: {p}");
    }
    all
}

/// The `initialize` instructions, with any problem in the user's workflows.
fn instructions(base: &str) -> String {
    let (_, problems) = crate::workflows::all(&tool_names());
    if problems.is_empty() {
        base.to_string()
    } else {
        format!(
            "{base} Some of the user's workflows were skipped: {}",
            problems.join("; ")
        )
    }
}

/// Names of every tool served.
pub(crate) fn tool_names() -> Vec<String> {
    tools()
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|t| t["name"].as_str().map(str::to_string))
        .collect()
}

fn prompt_list(prompts: Vec<Value>) -> Vec<Value> {
    prompts
        .into_iter()
        .map(|mut p| {
            // Only what the protocol defines: the template and the
            // argument defaults stay on this side.
            if let Some(o) = p.as_object_mut() {
                o.remove("template");
                o.remove("source");
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
fn prompt(prompts: &[Value], name: &str, arguments: &Value) -> Result<Value, String> {
    let p = prompts
        .iter()
        .find(|p| p["name"] == name)
        .ok_or_else(|| format!("no prompt named {name:?}; prompts/list has them"))?;
    let text = crate::workflows::render(p, arguments)?;
    Ok(json!({
        "description": p["description"],
        "messages": [{ "role": "user", "content": { "type": "text", "text": text } }],
    }))
}

/// The tool list, kept as data next to this file. Its validity is checked by
/// the tests below.
/// The tool list as MCP clients see it: without the CLI mapping.
fn listed_tools() -> Value {
    let mut tools = tools();
    for t in tools.as_array_mut().into_iter().flatten() {
        if let Some(o) = t.as_object_mut() {
            o.remove("cli");
        }
    }
    tools
}

fn tools() -> Value {
    serde_json::from_str(include_str!("mcp-tools.json")).unwrap_or_else(
        |e| json!([{ "name": "broken", "description": format!("mcp-tools.json: {e}") }]),
    )
}

/// Turns a tool call into the command line the CLI would get, from the
/// tool's own definition: its `cli` entry names the arguments that are
/// positional (in that order) and the ones whose flag is spelled
/// differently; every other argument `x_y` is the flag `--x-y`, and `true`
/// booleans are bare flags. Then the CLI parses it, so agents and people
/// get the same validation.
fn command_line(tool: &Value, args: &Value) -> Result<Vec<String>, String> {
    let name = tool["name"].as_str().unwrap_or_default();
    let cli = &tool["cli"];
    let schema = &tool["inputSchema"];
    let empty = serde_json::Map::new();
    let given = match args {
        Value::Null => &empty,
        Value::Object(o) => o,
        _ => return Err(format!("{name}: arguments must be an object")),
    };
    for key in schema["required"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
    {
        if given.get(key).is_none_or(Value::is_null) {
            return Err(format!("{key} is required"));
        }
    }
    let positional: Vec<&str> = cli["positional"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    let mut flags: Vec<String> = Vec::new();
    let mut tail: Vec<(usize, String)> = Vec::new();
    for (key, value) in given {
        if value.is_null() {
            continue;
        }
        let spec = schema["properties"]
            .get(key)
            .ok_or_else(|| format!("{name} takes no argument {key}"))?;
        let text = typed(key, value, spec)?;
        if let Some(at) = positional.iter().position(|p| p == key) {
            tail.push((at, text));
        } else if spec["type"] == "boolean" {
            if value == &Value::Bool(true) {
                flags.push(flag(cli, key));
            }
        } else {
            flags.push(format!("{}={text}", flag(cli, key)));
        }
    }
    tail.sort_by_key(|(at, _)| *at);
    let mut line: Vec<String> = vec!["hearth".into(), "--json".into(), name.into()];
    line.extend(flags);
    // Everything after `--` is a value, whatever it starts with.
    if !tail.is_empty() {
        line.push("--".into());
        line.extend(tail.into_iter().map(|(_, t)| t));
    }
    Ok(line)
}

/// The CLI flag for argument `key`: renamed in the tool's `cli.flags`, else
/// `--key` with `_` as `-`.
fn flag(cli: &Value, key: &str) -> String {
    cli["flags"][key]
        .as_str()
        .map_or_else(|| format!("--{}", key.replace('_', "-")), str::to_string)
}

/// `value` as command-line text, if it has the type (and is one of the
/// values) the tool's schema declares for `key`.
fn typed(key: &str, value: &Value, spec: &Value) -> Result<String, String> {
    let ok = match spec["type"].as_str() {
        Some("string") => value.is_string(),
        Some("integer") => value.is_u64() || value.is_i64(),
        Some("boolean") => value.is_boolean(),
        _ => false,
    };
    if !ok {
        return Err(format!("{key} must be a {}", spec["type"]));
    }
    if let Some(allowed) = spec["enum"].as_array()
        && !allowed.contains(value)
    {
        return Err(format!(
            "{key} must be one of {}",
            Value::Array(allowed.clone())
        ));
    }
    Ok(value
        .as_str()
        .map_or_else(|| value.to_string(), str::to_string))
}

fn call(name: &str, args: &Value) -> Result<Value, String> {
    if name == "process_memory" {
        return process_memory(args);
    }
    let tools = tools();
    let tool = tools
        .as_array()
        .into_iter()
        .flatten()
        .find(|t| t["name"] == name && t.get("cli").is_some())
        .ok_or_else(|| format!("unknown tool {name}"))?;
    let text = crate::dispatch(&command_line(tool, args)?)?;
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
    fn prompts_are_listed_and_filled_in() {
        // The built-in workflows only: the user's own file must not decide
        // whether these tests pass.
        let builtin = crate::workflows::builtin();
        let listed = &prompt_list(builtin.clone());
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
        for p in crate::workflows::builtin() {
            let text = p["template"].as_str().unwrap();
            for named in text.split('`').skip(1).step_by(2) {
                assert!(
                    tools.contains(&named.to_string()),
                    "{}: `{named}` is not a tool",
                    p["name"]
                );
            }
        }
        let reply = prompt(
            &builtin,
            "can-i-load",
            &json!({"model":"qwen3:8b","ctx":"32768"}),
        )
        .unwrap();
        let text = reply["messages"][0]["content"]["text"].as_str().unwrap();
        assert!(text.contains("qwen3:8b") && text.contains("32768") && !text.contains("{model}"));
        assert!(prompt(&builtin, "can-i-load", &json!({})).is_err());
        let reply = prompt(&builtin, "what-changed", &Value::Null).unwrap();
        let text = reply["messages"][0]["content"]["text"].as_str().unwrap();
        assert!(text.contains("\"~1\""), "defaults fill in");
    }

    #[test]
    fn tool_arguments_become_the_cli_command_line() {
        let find = |n: &str| {
            tools()
                .as_array()
                .unwrap()
                .iter()
                .find(|t| t["name"] == n)
                .unwrap()
                .clone()
        };
        let line = command_line(
            &find("fit"),
            &json!({"model":"qwen3:8b","ctx":4096,"cache_type":"q8_0","parallel":2,"ubatch":512}),
        )
        .unwrap();
        assert_eq!(&line[..3], ["hearth", "--json", "fit"]);
        assert_eq!(&line[line.len() - 2..], ["--", "qwen3:8b"]);
        assert!(line.contains(&"--cache-type=q8_0".to_string()));
        assert!(line.contains(&"--ctx=4096".to_string()));
        // A value that looks like a flag stays a value.
        let follow = command_line(&find("follow"), &json!({"pattern":"--kind=run"})).unwrap();
        assert_eq!(&follow[3..], ["--", "--kind=run"]);
        // Types and enums are the schema's.
        assert!(command_line(&find("tag"), &json!({"delete":"healthy"})).is_err());
        assert!(command_line(&find("log"), &json!({"kind":"secrets"})).is_err());
        assert!(command_line(&find("log"), &json!(["x"])).is_err());
        let ports = command_line(&find("ports"), &json!({"runtimes_only": true})).unwrap();
        assert_eq!(ports.last().unwrap(), "--runtimes");
        assert!(command_line(&find("follow"), &json!({})).is_err());
        assert!(command_line(&find("log"), &json!({"evil":"--x"})).is_err());
        // Every tool the CLI serves parses with an empty or minimal call.
        for t in tools().as_array().unwrap() {
            assert!(
                t.get("cli").is_some() || t["name"] == "process_memory",
                "{}",
                t["name"]
            );
        }
        let listed = listed_tools();
        assert!(
            listed
                .as_array()
                .unwrap()
                .iter()
                .all(|t| t.get("cli").is_none())
        );
    }

    #[test]
    fn instructions_name_every_tool() {
        let reply =
            handle(&json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}})).unwrap();
        let text = reply["result"]["instructions"].as_str().unwrap();
        for t in tools().as_array().unwrap() {
            let name = t["name"].as_str().unwrap();
            assert!(
                text.contains(name),
                "the instructions do not mention {name}"
            );
        }
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
