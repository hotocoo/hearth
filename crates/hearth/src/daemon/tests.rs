use super::reload::plan_reload;
use super::*;

fn server(port: u16, program: &str) -> Server {
    Server {
        port,
        host: None,
        expect: None,
        command: vec![program.into()],
        env: BTreeMap::new(),
        dir: None,
        model: None,
        ctx: None,
        cache_type: None,
    }
}

#[test]
fn reload_adds_removes_and_replaces_only_what_changed() {
    let kept = BTreeMap::from([
        ("same".to_string(), server(1, "a")),
        ("gone".to_string(), server(2, "b")),
        ("moved".to_string(), server(3, "c")),
    ]);
    let wanted = BTreeMap::from([
        ("same".to_string(), server(1, "a")),
        ("moved".to_string(), server(4, "c")),
        ("new".to_string(), server(5, "d")),
    ]);
    let (added, removed, changed) = plan_reload(&kept, &wanted);
    assert_eq!(
        (added, removed, changed),
        (
            vec!["new".to_string()],
            vec!["gone".to_string()],
            vec!["moved".to_string()]
        )
    );
}
