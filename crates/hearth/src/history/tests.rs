#[test]
fn diff_lists_what_happened_between_its_snapshots() {
    let dir = std::env::temp_dir().join(format!("hearth-meanwhile-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let j = Journal::at(dir.clone(), 10).unwrap();
    j.record("note", &serde_json::json!({"message": "before"}))
        .unwrap();
    let from = j.record("ports", &1).unwrap().snapshot.id;
    j.record(
        "note",
        &serde_json::json!({"message": "during", "by": "me"}),
    )
    .unwrap();
    let to = j.record("ports", &2).unwrap().snapshot.id;
    j.record("note", &serde_json::json!({"message": "after"}))
        .unwrap();
    let (entries, unreadable) = meanwhile(&j, &from, &to).unwrap();
    let said: Vec<&str> = entries.iter().map(|e| e.summary.as_str()).collect();
    assert_eq!(said, ["me: during"]);
    assert_eq!(entries[0].back, 1);
    assert_eq!(unreadable, Vec::<String>::new());
    // Either order of the two ends gives the same span.
    assert_eq!(meanwhile(&j, &to, &from).unwrap().0.len(), 1);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn a_wrong_pointer_says_where_it_went_wrong() {
    let v = serde_json::json!({"body": {"ports": [1, 2], "health": {"memory": 1}}});
    assert_eq!(nearest(&v, "/body/nope/x"), "/body has: health, ports");
    assert_eq!(
        nearest(&v, "/body/ports/5"),
        "/body/ports has: 2 items, /0 to /1"
    );
    assert_eq!(nearest(&v, "nope"), "the top has: body");
    let empty = serde_json::json!({"a": []});
    assert_eq!(nearest(&empty, "/a/0"), "/a has: an empty list");
    assert_eq!(
        nearest(&v, "/body/health/memory/x"),
        "/body/health/memory has: the value 1"
    );
}

#[test]
fn grep_reads_values_not_escaped_json() {
    let body = serde_json::json!({
        "pid": 1234, "error": "no \"model\" at C:\\models", "findings": [{"code": "oversized"}]
    });
    assert!(mentions(&body, "1234"));
    assert!(!mentions(&body, "123"));
    assert!(mentions(&body, "NO \"MODEL\""));
    assert!(mentions(&body, "c:\\models"));
    assert!(mentions(&body, "OVERSIZED"));
    assert!(mentions(&body, "findings"));
    assert!(!mentions(&body, "partial_offload"));
}

use super::*;
