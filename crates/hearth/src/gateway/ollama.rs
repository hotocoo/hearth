//! Ollama's API in front of a runtime that only speaks OpenAI's: a client
//! written for Ollama (`/api/chat`, `/api/generate`, `/api/tags`,
//! `/api/version`) can use a llama.cpp server through the gateway. A server
//! that answers `/api/version` itself speaks Ollama already and is passed
//! through untouched.
//!
//! `/api/chat` and `/api/generate` become `/v1/chat/completions`; streamed
//! answers come back as Ollama's newline-delimited JSON, ending with a
//! `done` line carrying the token counts. Options map to their OpenAI
//! names; images become data URLs; tool calls carry their arguments as
//! objects, as Ollama does.

use std::io::{BufRead, BufReader, Read, Write as _};
use std::net::TcpStream;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use hearth_core::config::Gateway;
use serde_json::{Map, Value, json};

use super::{Forward, Refusal, refuse, reply};

/// The Ollama endpoint a request line asks for, when it is one.
pub(super) fn endpoint(forward: &Forward) -> Option<(&str, &str)> {
    let mut words = forward.head.split(' ');
    let (method, path) = (words.next()?, words.next()?);
    let path = path.split('?').next()?;
    matches!(
        (method, path),
        ("POST", "/api/chat" | "/api/generate") | ("GET", "/api/tags" | "/api/version")
    )
    .then_some((method, path))
}

/// One HTTP exchange with the server, `Connection: close`; the answer's
/// status, whether its body is chunked, and a reader at the body's start.
fn exchange(
    forward: &Forward,
    method: &str,
    path: &str,
    body: &[u8],
    config: &Gateway,
    idle: Duration,
) -> std::io::Result<(u16, bool, BufReader<TcpStream>)> {
    let mut s = TcpStream::connect_timeout(
        &forward.addr,
        Duration::from_millis(config.connect_timeout_ms),
    )?;
    s.set_read_timeout(Some(idle))?;
    write!(
        s,
        "{method} {path} HTTP/1.1\r\nHost: {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        forward.addr,
        body.len()
    )?;
    s.write_all(body)?;
    let mut r = BufReader::new(s);
    let mut line = String::new();
    r.read_line(&mut line)?;
    let status = line
        .split(' ')
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);
    let mut chunked = false;
    loop {
        line.clear();
        if r.read_line(&mut line)? == 0 || line.trim().is_empty() {
            break;
        }
        let lower = line.to_ascii_lowercase();
        chunked |= lower.starts_with("transfer-encoding:") && lower.contains("chunked");
    }
    Ok((status, chunked, r))
}

/// A body's bytes, de-chunked as they arrive.
struct Body<R> {
    inner: R,
    chunked: bool,
    left: u64,
    done: bool,
}

impl<R: BufRead> Read for Body<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if !self.chunked {
            return self.inner.read(buf);
        }
        while self.left == 0 {
            if self.done {
                return Ok(0);
            }
            let mut line = String::new();
            if self.inner.read_line(&mut line)? == 0 {
                return Ok(0);
            }
            let size = line.trim().split(';').next().unwrap_or_default();
            if size.is_empty() {
                continue; // the CRLF after a chunk's data
            }
            self.left = u64::from_str_radix(size, 16)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
            self.done = self.left == 0;
        }
        let want = usize::try_from(self.left)
            .unwrap_or(usize::MAX)
            .min(buf.len());
        let n = self.inner.read(&mut buf[..want])?;
        self.left -= n as u64;
        Ok(n)
    }
}

/// Ollama's options under their OpenAI names; unknown ones are dropped.
fn options(into: &mut Map<String, Value>, request: &Value) {
    let o = &request["options"];
    for (from, to) in [
        ("temperature", "temperature"),
        ("top_p", "top_p"),
        ("top_k", "top_k"),
        ("min_p", "min_p"),
        ("seed", "seed"),
        ("stop", "stop"),
        ("num_predict", "max_tokens"),
        ("repeat_penalty", "repeat_penalty"),
        ("presence_penalty", "presence_penalty"),
        ("frequency_penalty", "frequency_penalty"),
    ] {
        if !o[from].is_null() {
            into.insert(to.into(), o[from].clone());
        }
    }
    match &request["format"] {
        Value::String(f) if f == "json" => {
            into.insert("response_format".into(), json!({ "type": "json_object" }));
        }
        schema @ Value::Object(_) => {
            into.insert(
                "response_format".into(),
                json!({ "type": "json_schema", "json_schema": { "name": "format", "schema": schema } }),
            );
        }
        _ => {}
    }
}

/// An Ollama message as OpenAI's: images become data-URL content parts,
/// tool-call arguments become strings.
fn message(m: &Value) -> Value {
    let mut out = m.clone();
    if let Some(images) = m["images"].as_array().filter(|i| !i.is_empty()) {
        let mut parts =
            vec![json!({ "type": "text", "text": m["content"].as_str().unwrap_or("") })];
        parts.extend(images.iter().filter_map(Value::as_str).map(|b64| {
            json!({ "type": "image_url", "image_url": { "url": format!("data:image/jpeg;base64,{b64}") } })
        }));
        out["content"] = Value::Array(parts);
    }
    if let Some(obj) = out.as_object_mut() {
        obj.remove("images");
    }
    if let Some(calls) = out["tool_calls"].as_array_mut() {
        for c in calls {
            let args = &mut c["function"]["arguments"];
            if !args.is_string() {
                *args = Value::String(args.to_string());
            }
            c["type"] = json!("function");
        }
    }
    out
}

/// The OpenAI chat request for an Ollama `/api/chat` or `/api/generate`.
fn to_openai(path: &str, request: &Value) -> Value {
    let messages: Vec<Value> = if path == "/api/chat" {
        request["messages"]
            .as_array()
            .into_iter()
            .flatten()
            .map(message)
            .collect()
    } else {
        let mut m = Vec::new();
        if let Some(system) = request["system"].as_str() {
            m.push(json!({ "role": "system", "content": system }));
        }
        let mut user =
            json!({ "role": "user", "content": request["prompt"].as_str().unwrap_or("") });
        user["images"] = request["images"].clone();
        m.push(message(&user));
        m
    };
    let stream = request["stream"].as_bool().unwrap_or(true);
    let mut out = Map::new();
    out.insert("model".into(), request["model"].clone());
    out.insert("messages".into(), Value::Array(messages));
    out.insert("stream".into(), json!(stream));
    if stream {
        out.insert("stream_options".into(), json!({ "include_usage": true }));
    }
    if request["tools"].is_array() {
        out.insert("tools".into(), request["tools"].clone());
    }
    options(&mut out, request);
    Value::Object(out)
}

/// Seconds since the epoch as RFC 3339 in UTC.
fn rfc3339(secs: u64) -> String {
    let (days, rest) = (secs / 86_400, secs % 86_400);
    // Howard Hinnant's civil_from_days.
    let z = i64::try_from(days).unwrap_or(0) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rest / 3600,
        rest % 3600 / 60,
        rest % 60
    )
}

fn now() -> String {
    rfc3339(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs()),
    )
}

/// Tool calls as Ollama gives them: arguments as objects.
fn tool_calls(calls: &Value) -> Option<Value> {
    let calls = calls.as_array().filter(|c| !c.is_empty())?;
    Some(Value::Array(
        calls
            .iter()
            .map(|c| {
                let args = c["function"]["arguments"].as_str().unwrap_or("{}");
                json!({ "function": {
                    "name": c["function"]["name"],
                    "arguments": serde_json::from_str::<Value>(args).unwrap_or(json!({})),
                }})
            })
            .collect(),
    ))
}

/// One Ollama answer line: `text` (and tool calls) as a chat message or a
/// generate response.
fn line(path: &str, model: &Value, text: &str, calls: Option<Value>) -> Value {
    let mut v = json!({ "model": model, "created_at": now(), "done": false });
    if path == "/api/chat" {
        v["message"] = json!({ "role": "assistant", "content": text });
        if let Some(c) = calls {
            v["message"]["tool_calls"] = c;
        }
    } else {
        v["response"] = json!(text);
    }
    v
}

/// The closing fields Ollama puts on its last line.
fn finish(v: &mut Value, reason: &Value, usage: &Value, started: Instant) {
    v["done"] = json!(true);
    v["done_reason"] = if reason.is_null() {
        json!("stop")
    } else {
        reason.clone()
    };
    v["total_duration"] = json!(u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX));
    v["prompt_eval_count"] = usage["prompt_tokens"].clone();
    v["eval_count"] = usage["completion_tokens"].clone();
}

/// The server's error, passed on: a 4xx (context exceeded, a bad
/// argument) is the client's to fix, as Ollama says with a 400; anything
/// else is the server's.
fn upstream_error(forward: &Forward, status: u16, body: &str) -> Refusal {
    let message = serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| v["error"]["message"].as_str().map(String::from))
        .unwrap_or_else(|| body.chars().take(300).collect());
    let (status_line, kind) = if (400..500).contains(&status) {
        ("400 Bad Request", "bad_request")
    } else {
        ("502 Bad Gateway", "upstream")
    };
    refuse(
        status_line,
        kind,
        format!("server {} answered {status}: {message}", forward.name),
        format!("hearth logs {}", forward.name),
    )
}

/// Whether the server speaks Ollama itself; it must say so within the
/// connect timeout, so a wedged server does not stall the request here.
fn native(forward: &Forward, config: &Gateway) -> bool {
    let quick = Duration::from_millis(config.connect_timeout_ms);
    hearth_runtime::http::get(forward.addr, "/api/version", quick, config.max_head_bytes)
        .is_ok_and(|r| r.status == 200)
}

/// An Ollama request that only loads or unloads the model (no prompt, no
/// messages), answered at once as Ollama does instead of generating.
fn load_only(path: &str, ask: &Value) -> Option<Value> {
    let empty = match path {
        "/api/chat" => ask["messages"].as_array().is_none_or(Vec::is_empty),
        _ => ask["prompt"].as_str().is_none_or(str::is_empty) && ask["images"].is_null(),
    };
    empty.then(|| {
        let mut v = line(path, &ask["model"], "", None);
        v["done"] = json!(true);
        // 0, "0", "0s", "0m": Ollama's ways of saying unload now. hearth
        // keeps the server as configured; `hearth stop` frees its memory.
        let zero = ask["keep_alive"].as_f64() == Some(0.0)
            || ask["keep_alive"]
                .as_str()
                .is_some_and(|k| k.trim_end_matches(char::is_alphabetic).parse() == Ok(0.0));
        v["done_reason"] = json!(if zero { "unload" } else { "load" });
        v
    })
}

/// Ollama's model list, from the server's OpenAI one.
fn tags(client: &mut TcpStream, forward: &Forward, config: &Gateway) -> std::io::Result<()> {
    let idle = Duration::from_millis(config.idle_timeout_ms);
    let answer = match hearth_runtime::http::get(
        forward.addr,
        "/v1/models",
        idle,
        config.translate_body_bytes,
    ) {
        Ok(a) => a,
        Err(e) => {
            return reply(client, &super::retry::exhausted(forward, 1, &e.to_string()));
        }
    };
    let text = String::from_utf8_lossy(&answer.body);
    if answer.status != 200 {
        return reply(client, &upstream_error(forward, answer.status, &text));
    }
    let listed: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    let models: Vec<Value> = listed["data"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|m| m["id"].as_str())
        .map(|id| json!({ "name": id, "model": id, "modified_at": now(), "size": 0, "digest": "", "details": {} }))
        .collect();
    let body = json!({ "models": models }).to_string();
    crate::listen::respond(&mut *client, "200 OK", "application/json", "", &body)
}

/// Answers an Ollama request by translating it; `Ok(false)` when the
/// server speaks Ollama and the request should be forwarded as is.
pub(super) fn serve(
    client: &mut TcpStream,
    forward: &Forward,
    request: &[u8],
    config: &Gateway,
) -> std::io::Result<bool> {
    if native(forward, config) {
        return Ok(false);
    }
    let Some((_, path)) = endpoint(forward) else {
        return Ok(false);
    };
    let fail = |client: &mut TcpStream, r: Refusal| reply(client, &r).map(|()| true);
    match path {
        "/api/version" => {
            let body = json!({ "version": env!("CARGO_PKG_VERSION") }).to_string();
            return crate::listen::respond(&mut *client, "200 OK", "application/json", "", &body)
                .map(|()| true);
        }
        "/api/tags" => return tags(client, forward, config).map(|()| true),
        _ => {}
    }
    let Ok(ask) = serde_json::from_slice::<Value>(request) else {
        return fail(
            client,
            refuse(
                "400 Bad Request",
                "bad_request",
                "the body is not JSON".into(),
                "send Ollama's JSON request".into(),
            ),
        );
    };
    if let Some(done) = load_only(path, &ask) {
        return crate::listen::respond(
            &mut *client,
            "200 OK",
            "application/json",
            "",
            &done.to_string(),
        )
        .map(|()| true);
    }
    let started = Instant::now();
    let openai = to_openai(path, &ask).to_string();
    let (status, chunked, r) = match exchange(
        forward,
        "POST",
        "/v1/chat/completions",
        openai.as_bytes(),
        config,
        Duration::from_millis(config.idle_timeout_ms),
    ) {
        Ok(x) => x,
        Err(e) => return fail(client, super::retry::exhausted(forward, 1, &e.to_string())),
    };
    let mut body = Body {
        inner: r,
        chunked,
        left: 0,
        done: false,
    };
    if status != 200 {
        let mut text = String::new();
        body.read_to_string(&mut text)?;
        return fail(client, upstream_error(forward, status, &text));
    }
    let model = ask["model"].clone();
    if !ask["stream"].as_bool().unwrap_or(true) {
        let mut text = String::new();
        body.read_to_string(&mut text)?;
        let Some(answer) = serde_json::from_str::<Value>(&text)
            .ok()
            .filter(|a| a["choices"][0].is_object())
        else {
            return fail(client, upstream_error(forward, 502, &text));
        };
        let choice = &answer["choices"][0];
        let mut v = line(
            path,
            &model,
            choice["message"]["content"].as_str().unwrap_or(""),
            tool_calls(&choice["message"]["tool_calls"]),
        );
        finish(&mut v, &choice["finish_reason"], &answer["usage"], started);
        return crate::listen::respond(
            &mut *client,
            "200 OK",
            "application/json",
            "",
            &v.to_string(),
        )
        .map(|()| true);
    }
    stream(client, path, &model, body, started).map(|()| true)
}

/// Relays a server-sent-events answer as Ollama's NDJSON, a line per
/// piece, until the server ends it or the client goes.
fn stream<R: BufRead>(
    client: &mut TcpStream,
    path: &str,
    model: &Value,
    body: Body<R>,
    started: Instant,
) -> std::io::Result<()> {
    write!(
        client,
        "HTTP/1.1 200 OK\r\nContent-Type: application/x-ndjson\r\nConnection: close\r\n\r\n"
    )?;
    let (mut reason, mut usage) = (Value::Null, Value::Null);
    // Tool calls stream as fragments of their arguments: gathered by index
    // and sent whole on the last line.
    let mut calls: Vec<(Value, String)> = Vec::new();
    for event in BufReader::new(body).lines() {
        let event = event?;
        let Some(data) = event.strip_prefix("data:").map(str::trim) else {
            continue;
        };
        if data == "[DONE]" {
            break;
        }
        let Ok(chunk) = serde_json::from_str::<Value>(data) else {
            continue;
        };
        // An error mid-stream ends the answer as Ollama ends one: a line
        // with `error`, not a clean finish with the text cut short.
        if !chunk["error"].is_null() {
            let message = chunk["error"]["message"]
                .as_str()
                .map_or_else(|| chunk["error"].to_string(), String::from);
            return writeln!(client, "{}", json!({ "error": message }));
        }
        if chunk["usage"].is_object() {
            usage = chunk["usage"].clone();
        }
        let Some(choice) = chunk["choices"].get(0) else {
            continue;
        };
        if !choice["finish_reason"].is_null() {
            reason = choice["finish_reason"].clone();
        }
        let text = choice["delta"]["content"].as_str().unwrap_or("");
        for c in choice["delta"]["tool_calls"]
            .as_array()
            .into_iter()
            .flatten()
        {
            let i = c["index"]
                .as_u64()
                .and_then(|i| usize::try_from(i).ok())
                .unwrap_or(calls.len());
            if calls.len() <= i {
                calls.resize(i + 1, (Value::Null, String::new()));
            }
            if !c["function"]["name"].is_null() {
                calls[i].0 = c["function"]["name"].clone();
            }
            calls[i]
                .1
                .push_str(c["function"]["arguments"].as_str().unwrap_or(""));
        }
        if !text.is_empty() {
            writeln!(client, "{}", line(path, model, text, None))?;
        } else if super::gone(client) {
            // Thinking or tool-call pieces send the client nothing, so its
            // leaving shows only here.
            return Ok(());
        }
    }
    let gathered = json!(
        calls
            .into_iter()
            .map(|(name, args)| json!({ "function": { "name": name, "arguments": args } }))
            .collect::<Vec<_>>()
    );
    let mut last = line(path, model, "", tool_calls(&gathered));
    finish(&mut last, &reason, &usage, started);
    writeln!(client, "{last}")
}

#[cfg(test)]
mod tests;
