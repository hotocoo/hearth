use super::*;

#[test]
fn chat_requests_become_openai_with_options_images_and_tools() {
    let ask = json!({
        "model": "m", "stream": false, "format": "json",
        "messages": [
            {"role": "user", "content": "what is this", "images": ["QUJD"]},
            {"role": "assistant", "content": "", "tool_calls": [{"function": {"name": "f", "arguments": {"x": 1}}}]},
        ],
        "options": {"temperature": 0.2, "num_predict": 7, "stop": ["\n"], "mirostat": 2},
        "tools": [{"type": "function", "function": {"name": "f"}}],
    });
    let o = to_openai("/api/chat", &ask);
    assert_eq!(o["stream"], false);
    assert!(o.get("stream_options").is_none());
    assert_eq!(o["max_tokens"], 7);
    assert_eq!(o["temperature"], 0.2);
    assert!(o.get("mirostat").is_none(), "unknown options are dropped");
    assert_eq!(o["response_format"]["type"], "json_object");
    assert_eq!(
        o["messages"][0]["content"][1]["image_url"]["url"],
        "data:image/jpeg;base64,QUJD"
    );
    assert!(o["messages"][0].get("images").is_none());
    assert_eq!(
        o["messages"][1]["tool_calls"][0]["function"]["arguments"],
        r#"{"x":1}"#
    );
    assert_eq!(o["tools"][0]["function"]["name"], "f");
}

#[test]
fn generate_requests_become_a_system_and_user_message_streamed_by_default() {
    let o = to_openai(
        "/api/generate",
        &json!({"model": "m", "prompt": "hi", "system": "be brief"}),
    );
    assert_eq!(
        o["messages"][0],
        json!({"role": "system", "content": "be brief"})
    );
    assert_eq!(o["messages"][1]["content"], "hi");
    assert_eq!(o["stream"], true);
    assert_eq!(o["stream_options"]["include_usage"], true);
}

#[test]
fn answers_take_ollamas_shape() {
    let mut v = line(
        "/api/chat",
        &json!("m"),
        "hey",
        tool_calls(&json!([
            {"function": {"name": "f", "arguments": "{\"x\":1}"}}
        ])),
    );
    finish(
        &mut v,
        &json!("length"),
        &json!({"prompt_tokens": 3, "completion_tokens": 4}),
        Instant::now(),
    );
    assert_eq!(v["message"]["content"], "hey");
    assert_eq!(
        v["message"]["tool_calls"][0]["function"]["arguments"]["x"],
        1
    );
    assert_eq!(
        (v["done"].clone(), v["done_reason"].clone()),
        (json!(true), json!("length"))
    );
    assert_eq!(
        (v["prompt_eval_count"].clone(), v["eval_count"].clone()),
        (json!(3), json!(4))
    );
    let g = line("/api/generate", &json!("m"), "x", None);
    assert_eq!(g["response"], "x");
}

#[test]
fn chunked_bodies_are_read_through() {
    let raw = b"4\r\nWiki\r\n6;ext\r\npedia \r\n0\r\n\r\n".as_slice();
    let mut text = String::new();
    Body {
        inner: raw,
        chunked: true,
        left: 0,
        done: false,
    }
    .read_to_string(&mut text)
    .unwrap();
    assert_eq!(text, "Wikipedia ");
}

#[test]
fn times_are_rfc3339_utc() {
    assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
    assert_eq!(rfc3339(951_782_400), "2000-02-29T00:00:00Z");
    assert_eq!(rfc3339(1_792_051_473), "2026-10-15T08:04:33Z");
}

#[test]
fn load_and_unload_requests_are_answered_without_generating() {
    let load = load_only("/api/generate", &json!({ "model": "m" })).unwrap();
    assert_eq!(
        (load["done"].clone(), load["done_reason"].clone()),
        (json!(true), json!("load"))
    );
    let unload = load_only(
        "/api/chat",
        &json!({ "model": "m", "messages": [], "keep_alive": 0 }),
    );
    assert_eq!(unload.unwrap()["done_reason"], "unload");
    let unload = load_only(
        "/api/generate",
        &json!({ "model": "m", "keep_alive": "0s" }),
    );
    assert_eq!(unload.unwrap()["done_reason"], "unload");
    assert!(load_only("/api/generate", &json!({ "model": "m", "prompt": "hi" })).is_none());
    assert!(
        load_only(
            "/api/generate",
            &json!({ "model": "m", "images": ["QUJD"] })
        )
        .is_none()
    );
    let chat = json!({ "model": "m", "messages": [{ "role": "user", "content": "hi" }] });
    assert!(load_only("/api/chat", &chat).is_none());
}

#[test]
fn a_server_error_mid_stream_reaches_the_client_as_an_error_line() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let reader = std::thread::spawn(move || {
        let mut text = String::new();
        let (mut s, _) = listener.accept().unwrap();
        s.read_to_string(&mut text).unwrap();
        text
    });
    let mut client = TcpStream::connect(addr).unwrap();
    let sse = "data: {\"choices\":[{\"delta\":{\"content\":\"Hi\"}}]}\n\ndata: {\"error\":{\"message\":\"context exceeded\"}}\n\n";
    let body = Body {
        inner: sse.as_bytes(),
        chunked: false,
        left: 0,
        done: false,
    };
    stream(&mut client, "/api/chat", &json!("m"), body, Instant::now()).unwrap();
    drop(client);
    let got = reader.join().unwrap();
    let last = got.lines().last().unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(last).unwrap(),
        json!({ "error": "context exceeded" })
    );
    assert!(!got.contains("\"done\":true"), "{got}");
}

#[test]
fn a_4xx_from_the_server_is_the_clients_to_fix() {
    let forward = Forward {
        name: "a".into(),
        addr: "127.0.0.1:1".parse().unwrap(),
        head: String::new(),
    };
    let r = upstream_error(&forward, 400, r#"{"error":{"message":"too long"}}"#);
    assert_eq!((r.status, r.kind), ("400 Bad Request", "bad_request"));
    assert!(r.message.contains("too long"));
    assert_eq!(
        upstream_error(&forward, 500, "boom").status,
        "502 Bad Gateway"
    );
}
