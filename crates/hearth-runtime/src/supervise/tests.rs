use super::*;

fn policy() -> RestartPolicy {
    RestartPolicy {
        budget: 3,
        window: Duration::from_secs(10),
        backoff_start: Duration::from_millis(100),
        backoff_max: Duration::from_millis(400),
        backoff_factor: 2,
    }
}

fn at(ms: u64) -> Duration {
    Duration::from_millis(ms)
}

#[test]
fn backoff_grows_to_its_ceiling_and_a_crash_loop_gives_up() {
    let mut r = Restarts::new(policy());
    assert_eq!(r.on_exit(at(0), None), Decision::Restart { after_ms: 100 });
    assert_eq!(
        r.on_exit(at(1000), None),
        Decision::Restart { after_ms: 200 }
    );
    assert_eq!(
        r.on_exit(at(2000), None),
        Decision::Restart { after_ms: 400 }
    );
    assert_eq!(
        r.on_exit(at(3000), None),
        Decision::GiveUp {
            exits: 4,
            window_ms: 10_000
        }
    );
    assert_eq!(r.count(), 3);
}

#[test]
fn the_growth_factor_is_the_policys() {
    let mut r = Restarts::new(RestartPolicy {
        backoff_factor: 3,
        backoff_max: Duration::from_secs(60),
        ..policy()
    });
    r.on_exit(at(0), None);
    assert_eq!(
        r.on_exit(at(1000), None),
        Decision::Restart { after_ms: 300 }
    );
}

#[test]
fn exits_spread_wider_than_the_window_never_give_up() {
    let mut r = Restarts::new(policy());
    for i in 0..20u64 {
        let d = r.on_exit(at(i * 4000), None);
        assert!(matches!(d, Decision::Restart { .. }), "exit {i}: {d:?}");
    }
}

#[test]
fn a_server_ready_a_whole_window_restarts_at_the_shortest_backoff() {
    let mut r = Restarts::new(policy());
    r.on_exit(at(0), None);
    r.on_exit(at(100), None);
    assert_eq!(
        r.on_exit(at(200), None),
        Decision::Restart { after_ms: 400 }
    );
    assert_eq!(
        r.on_exit(at(30_000), Some(at(1000))),
        Decision::Restart { after_ms: 100 }
    );
}

#[test]
fn serving_means_a_runtime_with_the_expected_model() {
    let llama = |health: &str| Identity::LlamaCpp {
        model: Some("/models/qwen3-8b-q4.gguf".into()),
        n_ctx: None,
        slots: None,
        health: health.into(),
        models: vec![],
    };
    assert!(serves(&llama("ok"), None));
    assert!(serves(&llama("ok"), Some("qwen3-8b")));
    assert!(!serves(&llama("ok"), Some("gemma")));
    assert!(!serves(&Identity::OtherHttp { status: 404 }, None));
    assert_eq!(llama("http 503").not_ready(), Some("http 503"));
}

#[cfg(unix)]
#[test]
fn signal_deaths_are_reported_as_signals() {
    use std::os::unix::process::ExitStatusExt as _;
    // Raw wait statuses: exit code in the high byte, signal in the low.
    assert_eq!(ended(ExitStatus::from_raw(3 << 8)), (Some(3), None));
    assert_eq!(ended(ExitStatus::from_raw(137 << 8)), (None, Some(9)));
    assert_eq!(ended(ExitStatus::from_raw(9)), (None, Some(9)));
}
