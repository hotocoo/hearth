use super::*;
use std::io::{Read as _, Write as _};

#[test]
fn a_resident_server_counts_its_own_memory_on_unified_memory_only() {
    let me = Some(std::process::id().cast_signed());
    assert!(resident_bytes(me, None).is_some_and(|b| b > 0));
    let discrete = Ceiling {
        bytes: 1,
        source: "test".into(),
        free_bytes: Some(1),
    };
    assert_eq!(resident_bytes(me, Some(&discrete)), None);
    assert_eq!(resident_bytes(None, None), None);
}

/// Answers fixed JSON per path, as the runtime would.
fn fake(routes: Vec<(&'static str, &'static str)>) -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut s) = stream else { return };
            let mut buf = [0u8; 2048];
            let n = s.read(&mut buf).unwrap_or(0);
            let req = String::from_utf8_lossy(&buf[..n]);
            let path = req.split_whitespace().nth(1).unwrap_or("/").to_string();
            let reply = routes.iter().find(|(p, _)| *p == path).map_or_else(
                || "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n".to_string(),
                |(_, body)| {
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{body}",
                        body.len()
                    )
                },
            );
            let _ = s.write_all(reply.as_bytes());
        }
    });
    port
}

#[test]
fn a_llama_server_names_its_own_file() {
    let port = fake(vec![
        (
            "/props",
            r#"{"default_generation_settings":{"n_ctx":4096},"total_slots":1,"model_path":"/m/q.gguf"}"#,
        ),
        ("/health", r#"{"status":"ok"}"#),
    ]);
    // The server's own context comes with its file.
    assert_eq!(
        from_server(port, None).unwrap(),
        (PathBuf::from("/m/q.gguf"), Some(4096))
    );
    assert!(
        from_server(port, Some(std::path::Path::new("x")))
            .unwrap_err()
            .contains("twice")
    );
}

#[test]
fn ollama_with_several_models_needs_one_named() {
    let port = fake(vec![
        ("/api/version", r#"{"version":"0.30"}"#),
        (
            "/api/ps",
            r#"{"models":[{"name":"a:1","size":1},{"name":"b:2","size":2}]}"#,
        ),
    ]);
    let e = from_server(port, None).unwrap_err();
    assert!(e.contains("a:1, b:2"), "{e}");
}
