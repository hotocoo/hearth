use super::*;

fn addr() -> SocketAddr {
    "127.0.0.1:9471".parse().unwrap()
}

fn request(method: &str, host: &str, origin: Option<&str>, marked: bool) -> Request {
    Request {
        method: method.into(),
        path: "/api/servers/chat/stop".into(),
        host: Some(host.into()),
        origin: origin.map(Into::into),
        marked,
    }
}

#[test]
fn heads_are_read_case_insensitively() {
    let r = parse("POST /api/servers/a/stop HTTP/1.1\r\nhost: localhost:9471\r\nORIGIN: http://localhost:9471\r\nX-Hearth: 1\r\n\r\n").unwrap();
    assert_eq!(r.method, "POST");
    assert_eq!(r.host.as_deref(), Some("localhost:9471"));
    assert_eq!(r.origin.as_deref(), Some("http://localhost:9471"));
    assert!(r.marked);
}

#[test]
fn only_this_listener_by_name_is_served() {
    assert!(guard(&request("GET", "127.0.0.1:9471", None, false), addr()).is_ok());
    assert!(guard(&request("GET", "localhost:9471", None, false), addr()).is_ok());
    let rebound = guard(&request("GET", "evil.example:9471", None, false), addr()).unwrap_err();
    assert_eq!(rebound.status, "403 Forbidden");
    assert!(guard(&request("GET", "localhost:9999", None, false), addr()).is_err());
}

#[test]
fn actions_need_the_mark_and_this_origin() {
    let host = "127.0.0.1:9471";
    assert!(guard(&request("POST", host, None, true), addr()).is_ok());
    assert!(
        guard(
            &request("POST", host, Some("http://127.0.0.1:9471"), true),
            addr()
        )
        .is_ok()
    );
    assert!(
        guard(
            &request("POST", host, Some("http://localhost:9471"), true),
            addr()
        )
        .is_ok()
    );
    assert!(guard(&request("POST", host, None, false), addr()).is_err());
    assert!(
        guard(
            &request("POST", host, Some("https://evil.example"), true),
            addr()
        )
        .is_err()
    );
}

#[test]
fn only_known_paths_and_plain_names_are_routed() {
    let config = hearth_core::config::from_layers(None).unwrap();
    let get = |path: &str| {
        route(
            &Request {
                method: "GET".into(),
                path: path.into(),
                ..Request::default()
            },
            &config.dashboard,
        )
    };
    assert_eq!(get("/").kind, "text/html; charset=utf-8");
    assert_eq!(get("/app.js").kind, "text/javascript; charset=utf-8");
    assert_eq!(get("/etc/passwd").status, "404 Not Found");
    assert_eq!(get("/api/logs/..%2f").status, "404 Not Found");
    let post = route(
        &Request {
            method: "POST".into(),
            path: "/api/servers/chat/delete".into(),
            ..Request::default()
        },
        &config.dashboard,
    );
    assert_eq!(post.status, "404 Not Found");
    assert!(valid_name("qwen-3.8_b"));
    assert!(!valid_name("a b"));
    assert!(!valid_name(""));
}

#[test]
fn the_page_loads_only_its_own_files() {
    assert!(INDEX.contains("src=\"/app.js\""));
    assert!(INDEX.contains("href=\"/app.css\""));
    for text in [INDEX, STYLE, SCRIPT] {
        assert!(!text.contains("http://") && !text.contains("https://"));
        assert!(!text.contains('\u{2014}'), "no em dashes");
    }
    assert!(
        !SCRIPT.contains("innerHTML"),
        "untrusted text goes in as text"
    );
}
