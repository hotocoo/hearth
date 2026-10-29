use super::*;

#[test]
fn base_layer_parses() {
    from_layers(None).unwrap();
}

#[test]
fn overlay_replaces_only_its_keys() {
    let base = from_layers(None).unwrap();
    let wanted = base.discovery.first_contact_timeout_ms + 1;
    let cfg = from_layers(Some((
        "test",
        &format!("[discovery]\nfirst_contact_timeout_ms = {wanted}\n"),
    )))
    .unwrap();
    assert_eq!(cfg.discovery.first_contact_timeout_ms, wanted);
}

#[test]
fn meaningless_values_are_rejected() {
    for bad in [
        "[discovery]\nlearned_deadline_multiplier = 0.5\n",
        "[discovery]\nlearned_deadline_multiplier = nan\n",
        "[discovery]\nlearned_min_samples = 0\n",
        "[state]\nmax_snapshots_per_kind = 0\n",
        "[supervise]\nunresponsive_probes = 0\n",
        "[supervise]\nbackoff_start_ms = 9\nbackoff_max_ms = 8\n",
        "[supervise]\nbackoff_max_ms = 60001\n",
        "[supervise]\nbackoff_factor = 0\n",
        "[notify]\nwebhook_url = \"file:///etc/passwd\"\n",
        "[[state.tiers]]\nwithin_ms = 2\nevery_ms = 0\n[[state.tiers]]\nwithin_ms = 1\nevery_ms = 0\n",
        "[state]\nthinned_kinds = [\"doctr\"]\n",
        "[control]\nmax_clients = 0\n",
        "[accelerators]\nquery_timeout_ms = 0\n",
        "[supervise]\noutput_line_bytes = 0\n",
        "[gateway]\nallowed_origins = [\"localhost:3000\"]\n",
        "[gateway]\nallowed_origins = [\"http://localhost:3000/\"]\n",
        "[discovery]\nfirst_contact_timeout_ms = 10\nlearned_deadline_floor_ms = 11\n",
    ] {
        assert!(
            matches!(
                from_layers(Some(("test", bad))),
                Err(ConfigError::Invalid { .. })
            ),
            "{bad}"
        );
    }
}

/// What the clock can represent differs by platform, so the check is
/// that a value either fails validation or can be used, never panics.
#[test]
fn huge_durations_are_rejected_or_usable() {
    let text = format!("[supervise]\nrestart_window_ms = {}\n", i64::MAX);
    if let Ok(cfg) = from_layers(Some(("test", &text))) {
        let _ = std::time::Instant::now()
            + std::time::Duration::from_millis(cfg.supervise.restart_window_ms);
    }
}

#[test]
fn servers_are_parsed_and_checked() {
    let one = "[servers.qwen]\nport = 8081\ncommand = [\"llama-server\", \"-m\", \"q.gguf\"]\n";
    let cfg = from_layers(Some(("test", one))).unwrap();
    assert_eq!(cfg.servers["qwen"].command[0], "llama-server");
    for bad in [
        "[servers.qwen]\nport = 8081\ncommand = []\n",
        "[servers.\"a b\"]\nport = 8081\ncommand = [\"x\"]\n",
        "[servers.a]\nport = 1\ncommand = [\"x\"]\n[servers.b]\nport = 1\ncommand = [\"y\"]\n",
        "[servers.a]\nport = 1\ncommand = [\"x\"]\nmodel = \"m.gguf\"\n",
    ] {
        assert!(
            matches!(
                from_layers(Some(("test", bad))),
                Err(ConfigError::Invalid { .. })
            ),
            "{bad}"
        );
    }
}

#[test]
fn typos_are_rejected() {
    let err =
        from_layers(Some(("test", "[discovery]\nfirst_contact_timout_ms = 1\n"))).unwrap_err();
    assert!(err.to_string().contains("test"));
}

#[test]
fn a_server_definition_changes_with_what_it_runs_only() {
    let text =
        "[servers.a]\nport = 1\ncommand = [\"x\"]\nmodel = \"m\"\nctx = 8\ncache_type = \"f16\"\n";
    let a = from_layers(Some(("test", text))).unwrap().servers["a"].clone();
    let moved = Server {
        port: 2,
        expect: Some("m".into()),
        ..a.clone()
    };
    assert_eq!(
        a.definition(),
        moved.definition(),
        "where it listens is not what it runs"
    );
    assert_ne!(
        a.definition(),
        Server {
            ctx: Some(16),
            ..a.clone()
        }
        .definition()
    );
    assert_ne!(
        a.definition(),
        Server {
            command: vec!["y".into()],
            ..a
        }
        .definition()
    );
}

#[test]
fn secrets_are_redacted_and_the_program_kept() {
    let setting = |key: &str, value: toml::Value| {
        Setting {
            key: key.into(),
            value: Some(value),
            source: "test".into(),
            doc: String::new(),
        }
        .redacted()
        .value
        .unwrap()
    };
    let s = |v: &str| toml::Value::String(v.into());
    assert_eq!(
        setting("notify.webhook_url", s("https://ntfy.sh/tok")),
        s("(redacted)")
    );
    assert_eq!(
        setting("servers.chat.env.HF_TOKEN", s("hf_x")),
        s("(redacted)")
    );
    let argv = toml::Value::Array(vec![s("llama-server"), s("--api-key"), s("sk-1")]);
    assert_eq!(
        setting("servers.chat.command", argv),
        toml::Value::Array(vec![s("llama-server"), s("(arguments redacted)")])
    );
    assert_eq!(
        setting("servers.chat.port", toml::Value::Integer(8080)),
        toml::Value::Integer(8080)
    );
}

#[test]
fn app_origins_can_be_allowed() {
    let text =
        "[gateway]\nallowed_origins = [\"app://obsidian.md\", \"null\", \"https://x.dev:8443\"]\n";
    assert!(from_layers(Some(("test", text))).is_ok());
}
