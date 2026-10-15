//! hearthd's gateway: one address in front of the servers it keeps. A
//! request for `/s/NAME/REST` goes to server NAME as `/REST`, unchanged
//! otherwise, so any OpenAI-, Anthropic- or Ollama-style client works by
//! pointing its base URL at `http://LISTEN/s/NAME/v1`.
//!
//! What it adds over calling the server directly: a server that is stopped,
//! starting or given up on, or not configured, gets a JSON error with a
//! `remediation` at once instead of a refused connection or a hang; and
//! cancellation is structural: when the client goes, the connection to the
//! server is closed, so the server stops generating for it.
//!
//! Not here yet (ADR-010): API translation, retry, admission, scheduling.

use std::fmt::Write as _;
use std::io::{ErrorKind, Read as _, Write as _};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::time::{Duration, Instant};

use hearth_core::config::Gateway;
use serde_json::{Value, json};

/// The servers hearthd keeps, as `hearth status` reports them.
pub type Servers = Arc<dyn Fn() -> Vec<Value> + Send + Sync>;

/// Headers that describe one connection, not the request; the gateway
/// opens its own connection to the server, so they are not passed on.
const HOP_BY_HOP: &[&str] = &["connection", "keep-alive", "proxy-connection"];

/// A request the gateway answers itself, as a JSON error.
#[derive(Debug, PartialEq, Eq)]
struct Refusal {
    status: &'static str,
    kind: &'static str,
    message: String,
    remediation: String,
}

/// Where a request goes: the server's name and address, and the request
/// head to send it.
#[derive(Debug, PartialEq, Eq)]
struct Forward {
    name: String,
    addr: SocketAddr,
    head: String,
}

fn refuse(
    status: &'static str,
    kind: &'static str,
    message: String,
    remediation: String,
) -> Refusal {
    Refusal {
        status,
        kind,
        message,
        remediation,
    }
}

/// Splits `/s/NAME/REST` into NAME and `/REST`.
fn split_target(target: &str) -> Option<(&str, String)> {
    let rest = target.strip_prefix("/s/")?;
    let (name, tail) = rest.split_at(rest.find(['/', '?']).unwrap_or(rest.len()));
    if name.is_empty() {
        return None;
    }
    Some((
        name,
        if tail.starts_with('/') {
            tail.to_string()
        } else {
            format!("/{tail}")
        },
    ))
}

/// What to do about a server that is not ready, by its state.
fn not_ready_remedy(name: &str, state: &str) -> String {
    match state {
        "stopped" => format!("hearth start {name}"),
        "gave_up" => {
            format!(
                "hearth logs {name} says why it kept failing; fix that, then hearth start {name}"
            )
        }
        "unsupervised" => format!("hearthd lost track of it; hearth restart {name}"),
        _ => format!("retry shortly; hearth status shows when {name} is ready"),
    }
}

/// Decides where the request with this head goes, from the servers'
/// states now.
fn route(head: &str, servers: &[Value]) -> Result<Forward, Refusal> {
    let mut lines = head.lines();
    let mut parts = lines.next().unwrap_or_default().split(' ');
    let (Some(method), Some(target), Some(version), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(refuse(
            "400 Bad Request",
            "bad_request",
            "not an HTTP request line".into(),
            "send an HTTP/1.1 request".into(),
        ));
    };
    let names = servers
        .iter()
        .filter_map(|s| s["name"].as_str())
        .collect::<Vec<_>>()
        .join(", ");
    let names = if names.is_empty() {
        "none configured yet".to_string()
    } else {
        names
    };
    let Some((name, tail)) = split_target(target) else {
        return Err(refuse(
            "404 Not Found",
            "not_found",
            format!("{target} is not under /s/NAME/"),
            format!("use http://THIS-ADDRESS/s/NAME/v1 as the base URL; servers: {names}"),
        ));
    };
    let Some(server) = servers.iter().find(|s| s["name"] == name) else {
        return Err(refuse(
            "404 Not Found",
            "not_found",
            format!("hearthd keeps no server named {name}"),
            format!(
                "servers: {names}; or add [servers.{name}] to the config and run hearth reload"
            ),
        ));
    };
    let state = server["state"].as_str().unwrap_or("unknown");
    if state != "ready" {
        return Err(refuse(
            "503 Service Unavailable",
            "not_ready",
            format!("server {name} is {state}"),
            not_ready_remedy(name, state),
        ));
    }
    let Some(addr) = server["addr"].as_str().and_then(|a| a.parse().ok()) else {
        return Err(refuse(
            "502 Bad Gateway",
            "unreachable",
            format!("server {name} has no address"),
            format!("hearth status {name}"),
        ));
    };
    let mut headers = String::new();
    for l in lines.filter(|l| !l.is_empty()).filter(|l| {
        let field = l.split(':').next().unwrap_or_default().trim();
        !HOP_BY_HOP.iter().any(|h| field.eq_ignore_ascii_case(h))
    }) {
        let _ = write!(headers, "{l}\r\n");
    }
    // shortcut: one request per connection, upgrade when keep-alive matters.
    Ok(Forward {
        name: name.to_string(),
        addr,
        head: format!("{method} {tail} {version}\r\n{headers}Connection: close\r\n\r\n"),
    })
}

fn reply(stream: &mut TcpStream, r: &Refusal) -> std::io::Result<()> {
    let body = json!({ "error": {
        "type": r.kind, "message": r.message, "remediation": r.remediation,
    }})
    .to_string();
    write!(
        stream,
        "HTTP/1.1 {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        r.status,
        body.len()
    )
}

/// Copies the request body up and the answer down until either side is
/// done. When the client closes, the server's connection is closed with
/// it; when the server finishes or goes silent past `idle`, so is the
/// client's.
fn pipe(
    mut client: TcpStream,
    mut upstream: TcpStream,
    forward: &Forward,
    idle: Duration,
) -> std::io::Result<()> {
    client.set_read_timeout(None)?;
    upstream.set_read_timeout(Some(idle))?;
    upstream.write_all(forward.head.as_bytes())?;
    let (mut from_client, mut to_upstream) = (client.try_clone()?, upstream.try_clone()?);
    // shortcut: a client that half-closes after its body counts as gone,
    // which no HTTP client waiting for its answer does.
    std::thread::Builder::new()
        .name("gateway request".into())
        .spawn(move || {
            let _ = std::io::copy(&mut from_client, &mut to_upstream);
            let _ = to_upstream.shutdown(Shutdown::Both);
        })?;
    let mut buf = [0u8; 16 * 1024];
    let mut sent = 0usize;
    let outcome = loop {
        match upstream.read(&mut buf) {
            Ok(0) => break Ok(()),
            Ok(n) => {
                if let Err(e) = client.write_all(&buf[..n]) {
                    break Err(e);
                }
                sent += n;
            }
            Err(e) => break Err(e),
        }
    };
    if let Err(e) = &outcome
        && sent == 0
        && matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut)
    {
        let _ = reply(
            &mut client,
            &refuse(
                "504 Gateway Timeout",
                "timeout",
                format!(
                    "server {} sent nothing for {} ms",
                    forward.name,
                    idle.as_millis()
                ),
                format!(
                    "hearth status {0}; a wedged server is restarted by hearthd, or raise gateway.idle_timeout_ms for very long prompts",
                    forward.name
                ),
            ),
        );
    }
    let _ = client.shutdown(Shutdown::Both);
    let _ = upstream.shutdown(Shutdown::Both);
    outcome
}

fn handle(mut client: TcpStream, config: &Gateway, servers: &Servers) -> std::io::Result<()> {
    let head_time = Duration::from_millis(config.head_timeout_ms);
    let head =
        crate::metrics::read_head(&client, config.max_head_bytes, Instant::now() + head_time)?;
    client.set_write_timeout(Some(head_time))?;
    if !(head.ends_with("\r\n\r\n") || head.ends_with("\n\n")) {
        return reply(
            &mut client,
            &refuse(
                "431 Request Header Fields Too Large",
                "bad_request",
                format!(
                    "request head longer than {} bytes or cut off",
                    config.max_head_bytes
                ),
                "send a complete request with smaller headers".into(),
            ),
        );
    }
    let forward = match route(&head, &servers()) {
        Ok(f) => f,
        Err(r) => return reply(&mut client, &r),
    };
    match TcpStream::connect_timeout(
        &forward.addr,
        Duration::from_millis(config.connect_timeout_ms),
    ) {
        Ok(upstream) => {
            client.set_write_timeout(None)?;
            pipe(
                client,
                upstream,
                &forward,
                Duration::from_millis(config.idle_timeout_ms),
            )
        }
        Err(e) => reply(
            &mut client,
            &refuse(
                "502 Bad Gateway",
                "unreachable",
                format!("server {} at {}: {e}", forward.name, forward.addr),
                format!("hearth status {0}; hearth logs {0}", forward.name),
            ),
        ),
    }
}

/// Forwards on `listener` until hearthd ends: a thread per request, up to
/// `max_clients`; one beyond that is told the gateway is busy.
fn serve(listener: &TcpListener, config: &Gateway, servers: &Servers) {
    let (handler_config, servers) = (config.clone(), servers.clone());
    let busy_config = config.clone();
    crate::listen::serve(
        "gateway",
        listener,
        config.max_clients,
        Duration::from_millis(config.connect_timeout_ms),
        &(Arc::new(move |stream| handle(stream, &handler_config, &servers)) as _),
        move |mut stream| {
            let _ =
                stream.set_write_timeout(Some(Duration::from_millis(busy_config.head_timeout_ms)));
            let _ = reply(
                &mut stream,
                &refuse(
                    "503 Service Unavailable",
                    "busy",
                    format!(
                        "{} requests are already being forwarded",
                        busy_config.max_clients
                    ),
                    "retry shortly, or raise gateway.max_clients".into(),
                ),
            );
        },
    );
}

/// Starts the gateway when `gateway.listen` is set; the address it serves.
pub fn start(config: &Gateway, servers: &Servers) -> Result<Option<SocketAddr>, String> {
    let Some(addr) = config.listen else {
        return Ok(None);
    };
    let listener = TcpListener::bind(addr).map_err(|e| format!("gateway.listen {addr}: {e}"))?;
    let (config, servers) = (config.clone(), servers.clone());
    std::thread::Builder::new()
        .name("gateway".into())
        .spawn(move || serve(&listener, &config, &servers))
        .map_err(|e| format!("cannot start the gateway: {e}"))?;
    Ok(Some(addr))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn servers() -> Vec<Value> {
        vec![
            json!({ "name": "chat", "state": "ready", "addr": "127.0.0.1:8081" }),
            json!({ "name": "vision", "state": "stopped", "addr": "127.0.0.1:8082" }),
        ]
    }

    #[test]
    fn targets_split_into_server_and_path() {
        assert_eq!(
            split_target("/s/chat/v1/models"),
            Some(("chat", "/v1/models".into()))
        );
        assert_eq!(split_target("/s/chat"), Some(("chat", "/".into())));
        assert_eq!(split_target("/s/chat?x=1"), Some(("chat", "/?x=1".into())));
        assert_eq!(split_target("/s//v1"), None);
        assert_eq!(split_target("/v1/models"), None);
    }

    #[test]
    fn a_ready_server_gets_the_request_with_its_own_path_and_one_connection() {
        let head = "POST /s/chat/v1/chat/completions HTTP/1.1\r\nHost: x\r\nConnection: keep-alive\r\nkeep-alive: 5\r\nContent-Length: 2\r\n\r\n";
        let f = route(head, &servers()).unwrap();
        assert_eq!(f.addr, "127.0.0.1:8081".parse().unwrap());
        assert_eq!(
            f.head,
            "POST /v1/chat/completions HTTP/1.1\r\nHost: x\r\nContent-Length: 2\r\nConnection: close\r\n\r\n"
        );
    }

    #[test]
    fn refusals_say_what_to_do() {
        let stopped = route("GET /s/vision/v1/models HTTP/1.1\r\n\r\n", &servers()).unwrap_err();
        assert_eq!(stopped.status, "503 Service Unavailable");
        assert_eq!(stopped.remediation, "hearth start vision");
        let unknown = route("GET /s/nope/v1/models HTTP/1.1\r\n\r\n", &servers()).unwrap_err();
        assert_eq!(unknown.status, "404 Not Found");
        assert!(unknown.remediation.contains("chat, vision"), "{unknown:?}");
        let outside = route("GET /v1/models HTTP/1.1\r\n\r\n", &servers()).unwrap_err();
        assert!(outside.remediation.contains("/s/NAME/v1"), "{outside:?}");
        assert_eq!(
            route("hello\r\n\r\n", &servers()).unwrap_err().kind,
            "bad_request"
        );
    }
}
