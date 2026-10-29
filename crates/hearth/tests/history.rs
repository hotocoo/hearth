//! History commands bound what they print and say when they left some out.

mod common;

use common::*;

#[test]
fn log_and_follow_show_the_latest_by_default_and_say_there_is_more() {
    let dir = home("history-limit");
    std::fs::write(dir.join("hearth.toml"), "[state]\nhistory_limit = 2\n").unwrap();
    for n in 0..4 {
        hearth(&dir, &["note", "--by", "test", "--", &format!("note {n}")]).unwrap();
    }
    let log = hearth(&dir, &["log", "--kind", "note"]).unwrap();
    assert_eq!(log["snapshots"].as_array().unwrap().len(), 2, "{log}");
    assert_eq!(log["more"], true, "{log}");
    let all = hearth(&dir, &["log", "--kind", "note", "--limit", "10"]).unwrap();
    assert_eq!(all["snapshots"].as_array().unwrap().len(), 4);
    assert!(
        all.get("more").is_none(),
        "an explicit limit is what was asked"
    );
    let follow = hearth(&dir, &["follow", "note", "--kind", "note", "--all"]).unwrap();
    assert_eq!(follow["steps"].as_array().unwrap().len(), 2, "{follow}");
    assert_eq!(follow["more"], true, "{follow}");
}
