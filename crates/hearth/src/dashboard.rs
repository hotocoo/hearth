//! hearthd's dashboard: a web page on loopback showing what `hearth top`
//! shows (the servers hearthd keeps, the machine, the journal) with buttons
//! for restart, stop, start and a server's output (ADR-011).
//!
//! The page acts on servers and has no login, so every request is checked:
//! the address must be loopback (config validation), `Host` must name this
//! listener (no DNS rebinding), and an action must carry `X-Hearth: 1` and,
//! when the browser sends one, an `Origin` of this listener (no cross-site
//! request can set the header without a preflight, which is never
//! allowed). The page, its style and its script are files built into the
//! binary and served under a CSP that allows nothing else.

use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::time::{Duration, Instant};

use hearth_core::config::Dashboard;
use serde_json::{Value, json};

const INDEX: &str = include_str!("dashboard/index.html");
const STYLE: &str = include_str!("dashboard/app.css");
const SCRIPT: &str = include_str!("dashboard/app.js");

const SECURITY_HEADERS: &str = "Content-Security-Policy: default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self'; img-src 'self' data:; base-uri 'none'; form-action 'none'; frame-ancestors 'none'\r\nX-Content-Type-Options: nosniff\r\nReferrer-Policy: no-referrer\r\nCache-Control: no-store\r\n";

/// One answer: status line, content type, body.
#[derive(Debug, PartialEq, Eq)]
struct Answer {
    status: &'static str,
    kind: &'static str,
    body: String,
}

fn ok(kind: &'static str, body: impl Into<String>) -> Answer {
    Answer {
        status: "200 OK",
        kind,
        body: body.into(),
    }
}

fn error(status: &'static str, kind: &str, message: &str, remediation: &str) -> Answer {
    Answer {
        status,
        kind: "application/json",
        body: json!({ "error": {
            "type": kind, "message": message, "remediation": remediation,
        }})
        .to_string(),
    }
}

/// What the page needs from one request head.
#[derive(Debug, Default, PartialEq, Eq)]
struct Request {
    method: String,
    path: String,
    host: Option<String>,
    origin: Option<String>,
    marked: bool,
}

fn parse(head: &str) -> Option<Request> {
    let mut lines = head.lines();
    let mut first = lines.next()?.split(' ');
    let mut r = Request {
        method: first.next()?.to_string(),
        path: first.next()?.to_string(),
        ..Request::default()
    };
    for l in lines {
        let Some((field, value)) = l.split_once(':') else {
            continue;
        };
        let value = value.trim().to_string();
        match field.trim().to_ascii_lowercase().as_str() {
            "host" => r.host = Some(value),
            "origin" => r.origin = Some(value),
            "x-hearth" => r.marked = value == "1",
            _ => {}
        }
    }
    Some(r)
}

/// The `Host` values that name this listener: its own address, and
/// `localhost` at its port.
fn allowed_hosts(addr: SocketAddr) -> Vec<String> {
    let port = addr.port();
    vec![
        addr.to_string(),
        format!("localhost:{port}"),
        format!("127.0.0.1:{port}"),
        format!("[::1]:{port}"),
    ]
}

/// Refuses a request that could come from another site or another name.
fn guard(r: &Request, addr: SocketAddr) -> Result<(), Answer> {
    let hosts = allowed_hosts(addr);
    if !r.host.as_ref().is_some_and(|h| hosts.contains(h)) {
        return Err(error(
            "403 Forbidden",
            "forbidden",
            "the Host header does not name this dashboard",
            &format!("open http://{addr}/ directly"),
        ));
    }
    if r.method != "GET" {
        let same_origin = r
            .origin
            .as_ref()
            .is_none_or(|o| hosts.iter().any(|h| o == &format!("http://{h}")));
        if !r.marked || !same_origin {
            return Err(error(
                "403 Forbidden",
                "forbidden",
                "actions come only from the dashboard page itself",
                &format!("use the buttons on http://{addr}/, or `hearth restart NAME`"),
            ));
        }
    }
    Ok(())
}

/// A server name as configured: nothing that needs escaping.
fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// hearthd's answer to an order, as the page reads it.
fn control(request: &Value) -> Answer {
    match crate::control::call(request) {
        Ok(v) => ok("application/json", v.to_string()),
        Err(e) if e.contains("no server named") => error(
            "404 Not Found",
            "not_found",
            &e,
            "reload the page: the configuration changed",
        ),
        Err(e) => error("502 Bad Gateway", "control", &e, "hearth status"),
    }
}

fn route(r: &Request, config: &Dashboard) -> Answer {
    let path = r.path.split('?').next().unwrap_or_default();
    let parts: Vec<&str> = path.trim_start_matches('/').split('/').collect();
    match (r.method.as_str(), parts.as_slice()) {
        ("GET", [""]) => ok("text/html; charset=utf-8", INDEX),
        ("GET", ["app.css"]) => ok("text/css; charset=utf-8", STYLE),
        ("GET", ["app.js"]) => ok("text/javascript; charset=utf-8", SCRIPT),
        ("GET", ["api", "frame"]) => {
            let mut frame = crate::top::frame_of(config.journal);
            frame["refresh_ms"] = json!(config.refresh_ms);
            ok("application/json", frame.to_string())
        }
        ("GET", ["api", "logs", name]) if valid_name(name) => {
            control(&json!({ "op": "logs", "server": name }))
        }
        ("POST", ["api", "servers", name, op @ ("restart" | "stop" | "start")])
            if valid_name(name) =>
        {
            control(&json!({ "op": op, "server": name }))
        }
        _ => error(
            "404 Not Found",
            "not_found",
            &format!("{} {path} is not part of the dashboard", r.method),
            "open / for the page",
        ),
    }
}

fn answer(stream: TcpStream, config: &Dashboard, addr: SocketAddr) -> std::io::Result<()> {
    let deadline = Instant::now() + Duration::from_millis(config.timeout_ms);
    let head = crate::listen::read_head(&stream, config.max_head_bytes, deadline)?;
    let a = match parse(&head) {
        None => error("400 Bad Request", "bad_request", "not an HTTP request", ""),
        Some(r) => guard(&r, addr).map_or_else(|refused| refused, |()| route(&r, config)),
    };
    stream.set_write_timeout(Some(
        deadline
            .checked_duration_since(Instant::now())
            .unwrap_or(Duration::from_millis(config.timeout_ms)),
    ))?;
    crate::listen::respond(stream, a.status, a.kind, SECURITY_HEADERS, &a.body)
}

/// Starts the dashboard when `dashboard.listen` is set; the address it
/// serves.
pub fn start(config: &Dashboard) -> Result<Option<SocketAddr>, String> {
    let Some(addr) = config.listen else {
        return Ok(None);
    };
    let listener = TcpListener::bind(addr).map_err(|e| format!("dashboard.listen {addr}: {e}"))?;
    let config = config.clone();
    std::thread::Builder::new()
        .name("dashboard".into())
        .spawn(move || {
            let (max, pause) = (config.max_clients, Duration::from_millis(config.timeout_ms));
            crate::listen::serve(
                "dashboard",
                &listener,
                max,
                pause,
                &(Arc::new(move |stream| answer(stream, &config, addr)) as _),
                drop,
            );
        })
        .map_err(|e| format!("cannot start the dashboard: {e}"))?;
    Ok(Some(addr))
}

#[cfg(test)]
mod tests;
