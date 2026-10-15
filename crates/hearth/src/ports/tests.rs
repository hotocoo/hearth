use super::*;
use serde_json::json;

fn port(pid: i32, port: u16, identity: &Value, footprint: Option<u64>) -> Value {
    let mut p = json!({
        "pid": pid, "process": "llama-server", "path": "/opt/bin/llama-server",
        "addr": format!("127.0.0.1:{port}"),
        "memory": footprint.map(|f| json!({ "footprint_bytes": f })),
    });
    for (k, v) in identity.as_object().unwrap() {
        p[k] = v.clone();
    }
    p
}

#[test]
fn diff_reports_restarts_changes_and_footprint() {
    let ready = json!({"kind":"llama_cpp","model":"/m/a.gguf","n_ctx":4096,"slots":1,"health":"ok","models":[]});
    let silent = json!({"kind":"unresponsive","waited_ms":2500});
    let silent_longer = json!({"kind":"unresponsive","waited_ms":2501});
    let before = json!({"ports": [
        port(10, 8080, &ready, Some(100)),
        port(11, 8081, &ready, None),
        port(12, 8082, &silent, None),
        port(13, 9000, &ready, None),
    ]});
    let after = json!({"ports": [
        port(10, 8080, &ready, Some(300)),
        port(21, 8081, &ready, None),
        port(12, 8082, &silent_longer, None),
        port(14, 9001, &silent, None),
    ]});
    let (changes, deltas) = diff(&before, &after);
    let kinds: Vec<(&str, &str)> = changes
        .iter()
        .map(|c| (c.kind, c.subject.as_str()))
        .collect();
    assert_eq!(
        kinds,
        [
            ("vanished", "llama-server :9000"),
            ("restarted", "llama-server :8081"),
            ("appeared", "llama-server :9001"),
        ],
        "a longer wait on a port that was already silent is not a change"
    );
    assert_eq!(deltas.len(), 1);
    assert_eq!(deltas[0].by, 200);
    let (none, _) = diff(&after, &after);
    assert!(none.is_empty());
}

#[test]
fn workers_sharing_a_port_are_compared_as_a_set() {
    let ready = json!({"kind":"open_ai_compatible","models":["m"]});
    let two = |a: i32, b: i32| json!({"ports": [port(a, 8000, &ready, Some(10)), port(b, 8000, &ready, Some(10))]});
    let (none, _) = diff(&two(1, 2), &two(1, 2));
    assert!(none.is_empty());
    assert_eq!(summary(&two(1, 2)).split(' ').next(), Some("2"));
    let (changes, deltas) = diff(&two(1, 2), &two(1, 3));
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0].kind, "restarted");
    assert_eq!(
        changes[0].after.as_deref(),
        Some("pid 1,3 · OpenAI-compatible · m")
    );
    assert_eq!(
        deltas.len(),
        1,
        "only the surviving worker's footprint compares"
    );
}

#[test]
fn alarms_are_runtimes_that_vanished_restarted_or_went_silent() {
    let ready = json!({"kind":"ollama","version":"0.30","models":[],"loaded":[]});
    let silent = json!({"kind":"unresponsive","waited_ms":2500});
    let other = json!({"kind":"other_http","status":404});
    let mut dev = port(30, 3000, &other, None);
    dev["path"] = json!("/usr/bin/node");
    let before = json!({"ports": [port(5, 11434, &ready, None), port(6, 8080, &ready, None), dev]});
    let after = json!({"ports": [port(5, 11434, &silent, None)]});
    let alarms = alarms(&before, &after, &std::collections::BTreeSet::default());
    let kinds: Vec<&str> = alarms.iter().map(|c| c.kind).collect();
    assert_eq!(
        kinds,
        ["vanished", "changed"],
        "a dev server that went away is not an alarm: {:?}",
        alarms.iter().map(|c| &c.subject).collect::<Vec<_>>()
    );
}

#[test]
fn a_known_runtime_that_went_silent_then_vanished_is_still_reported() {
    let ready = json!({"kind":"ollama","version":"0.30","models":[],"loaded":[]});
    let silent = json!({"kind":"unresponsive","waited_ms":2500});
    let first = json!({"ports": [port(5, 11434, &ready, None)]});
    let second = json!({"ports": [port(5, 11434, &silent, None)]});
    let third = json!({"ports": []});
    let known = runtime_keys(&first);
    assert_eq!(alarms(&second, &third, &known)[0].kind, "vanished");
    assert!(alarms(&second, &third, &std::collections::BTreeSet::default()).is_empty());
    // Answering again is reported too.
    assert_eq!(alarms(&second, &first, &known)[0].kind, "changed");
}

#[test]
fn a_model_only_partly_on_the_accelerator_is_a_finding() {
    let mk = |vram: u64| Port {
        pid: 1,
        process: "ollama".into(),
        path: None,
        bound: vec![],
        fingerprint: Fingerprint {
            addr: "127.0.0.1:11434".parse().unwrap(),
            identity: Identity::Ollama {
                version: "0".into(),
                models: vec![],
                loaded: vec![hearth_runtime::fingerprint::Loaded {
                    name: "big:70b".into(),
                    size_bytes: Some(100),
                    size_vram_bytes: Some(vram),
                    context_length: None,
                }],
            },
            latency_us: None,
        },
        memory: None,
        accelerator_bytes: None,
        deadline: Deadline::FirstContact { us: 1, samples: 0 },
        slow: false,
        owned_by: None,
    };
    assert_eq!(partially_offloaded(&[mk(60)], || true).len(), 1);
    assert!(partially_offloaded(&[mk(100)], || true).is_empty());
    assert_eq!(partially_offloaded(&[mk(0)], || true).len(), 1);
    assert!(partially_offloaded(&[mk(0)], || false).is_empty());
}

#[test]
fn diff_reports_a_runtime_that_stopped_answering() {
    let ready = json!({"kind":"ollama","version":"0.30","models":[],"loaded":[]});
    let silent = json!({"kind":"unresponsive","waited_ms":2500});
    let (changes, _) = diff(
        &json!({"ports": [port(5, 11434, &ready, None)]}),
        &json!({"ports": [port(5, 11434, &silent, None)]}),
    );
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0].kind, "changed");
    assert_eq!(changes[0].after.as_deref(), Some("NO ANSWER"));
}

fn discovery(min_samples: usize) -> hearth_core::config::Discovery {
    let mut d = hearth_core::config::from_layers(None).unwrap().discovery;
    d.learned_min_samples = min_samples;
    d.learned_deadline_floor_ms = 0;
    d
}

#[test]
fn deadline_is_learned_only_from_enough_answers() {
    let d = discovery(3);
    assert!(matches!(
        deadline(Some(&vec![100, 300]), &d),
        Deadline::FirstContact { samples: 2, .. }
    ));
    let Deadline::Learned { us, slowest_us, .. } = deadline(Some(&vec![100, 300, 200]), &d) else {
        panic!("three answers are enough")
    };
    assert_eq!(slowest_us, 300);
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    let expected = (300.0 * d.learned_deadline_multiplier).ceil() as u64;
    assert_eq!(us, expected);
}

#[test]
fn learned_deadline_never_drops_below_the_floor() {
    let mut d = discovery(1);
    d.learned_deadline_floor_ms = 7;
    let Deadline::Learned { us, .. } = deadline(Some(&vec![1]), &d) else {
        panic!("a learned deadline")
    };
    assert_eq!(Duration::from_micros(us), Duration::from_millis(7));
}

#[test]
fn learned_deadline_never_exceeds_first_contact() {
    let d = discovery(1);
    let first = deadline(None, &d);
    let Deadline::FirstContact { us: first_us, .. } = first else {
        panic!()
    };
    assert!(matches!(
        deadline(Some(&vec![first_us]), &d),
        Deadline::FirstContact { .. }
    ));
}

#[test]
fn missed_learned_deadline_gets_a_second_chance() {
    use std::io::{Read, Write};
    // Answers like Ollama, but only after a pause longer than the
    // learned deadline and shorter than the first-contact one.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let pause = Duration::from_millis(50);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut s) = stream else { return };
            let mut buf = [0u8; 1024];
            let _ = s.read(&mut buf);
            std::thread::sleep(pause);
            let body = r#"{"version":"1"}"#;
            let _ = write!(
                s,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            );
        }
    });
    let learned = Deadline::Learned {
        us: 1000,
        samples: 3,
        slowest_us: 100,
    };
    let first = Deadline::FirstContact {
        us: u64::try_from((pause * 40).as_micros()).unwrap(),
        samples: 0,
    };
    let (fp, slow) = probe(addr, learned, first, 1 << 16);
    assert!(slow);
    assert!(
        matches!(fp.identity, Identity::Ollama { .. }),
        "{:?}",
        fp.identity
    );
}

#[test]
fn headroom_says_how_close_the_slots_are() {
    assert_eq!(
        super::findings::headroom(Some(100), Some(250)),
        "; its slots' last requests used about 100 tokens, 150 short of the 250 that fit"
    );
    assert!(super::findings::headroom(Some(300), Some(250)).contains("about 300 tokens, past"));
    assert_eq!(super::findings::headroom(None, Some(250)), "");
}
