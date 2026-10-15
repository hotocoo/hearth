use super::*;

fn load(pid: i32, files: u64) -> ProcessLoad {
    ProcessLoad {
        pid,
        name: format!("p{pid}"),
        open_files: Some(files),
        threads: None,
        close_wait: None,
        listening: Vec::new(),
    }
}

#[test]
fn majority_is_the_fewest_processes_holding_half() {
    let procs = [load(1, 10), load(2, 60), load(3, 20), load(4, 10)];
    let m = majority(&procs, |p| p.open_files);
    assert_eq!(m.iter().map(|h| h.pid).collect::<Vec<_>>(), vec![2]);
    let even = [load(1, 25), load(2, 25), load(3, 25), load(4, 25)];
    assert_eq!(majority(&even, |p| p.open_files).len(), 2);
    assert!(majority(&[], |p| p.open_files).is_empty());
}

#[test]
fn durations_read_naturally() {
    assert_eq!(duration(42.0), "42s");
    assert_eq!(duration(600.0), "10m");
    assert_eq!(duration(7200.0), "2.0h");
    assert_eq!(duration(f64::INFINITY), "never");
}
