use super::*;

fn journal(test: &str, keep: usize) -> (Journal, PathBuf) {
    let dir = std::env::temp_dir().join(format!("hearth-state-{test}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    (Journal::at(dir.clone(), keep).unwrap(), dir)
}

#[test]
fn records_resolves_and_prunes() {
    let (j, dir) = journal("prunes", 2);
    for n in 0..3 {
        j.record("ports", &serde_json::json!({ "n": n })).unwrap();
    }
    j.record("doctor", &serde_json::json!({})).unwrap();
    assert_eq!(j.ids("ports").unwrap().len(), 2, "oldest pruned");
    assert_eq!(j.ids("doctor").unwrap().len(), 1, "kinds pruned separately");
    assert_eq!(j.resolve("ports", "~0").unwrap().body["n"], 2);
    // Records made back to back keep distinct ids.
    assert_eq!(j.resolve("ports", "~1").unwrap().body["n"], 1);
    for beyond in ["~2", &format!("~{}", usize::MAX)] {
        assert!(matches!(
            j.resolve("ports", beyond),
            Err(StateError::NotFound { available: 2, .. })
        ));
    }
    let id = j.ids("ports").unwrap()[0].clone();
    assert_eq!(j.resolve("ports", &id).unwrap().id, id);
    assert!(matches!(
        j.resolve("doctor", &id),
        Err(StateError::WrongKind { .. })
    ));
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn a_tag_can_name_one_earlier_snapshot() {
    let dir = std::env::temp_dir().join(format!("hearth-tag-at-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let j = Journal::at(dir.clone(), 2).unwrap();
    let first = j.record("ports", &1).unwrap().snapshot.id;
    j.record("ports", &2).unwrap();
    j.tag_snapshot("first", &first).unwrap();
    // Kept past the cap, and the tag holds only that snapshot.
    j.record("ports", &3).unwrap();
    assert_eq!(j.resolve("ports", "first").unwrap().id, first);
    assert_eq!(j.tag("first").unwrap().len(), 1);
    assert!(j.tag_snapshot("bad", "not-an-id").is_err());
    let gone = first.replace("-ports", "-doctor");
    assert!(j.tag_snapshot("gone", &gone).is_err());
    assert!(j.tag("gone").is_err());
    // A failed retag leaves the tag where it was.
    assert!(j.tag_snapshot("first", &gone).is_err());
    assert_eq!(j.resolve("ports", "first").unwrap().id, first);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn tags_name_moments_and_survive_pruning() {
    let (j, dir) = journal("tags", 2);
    j.record("ports", &serde_json::json!({ "n": 0 })).unwrap();
    j.record("doctor", &serde_json::json!({ "d": 0 })).unwrap();
    let tagged = j.set_tag("healthy", &["ports", "doctor"]).unwrap();
    assert_eq!(tagged.len(), 2);
    for n in 1..5 {
        j.record("ports", &serde_json::json!({ "n": n })).unwrap();
    }
    assert_eq!(
        j.resolve("ports", "healthy").unwrap().body["n"],
        0,
        "kept past retention"
    );
    assert_eq!(
        j.ids("ports").unwrap().len(),
        3,
        "two kept plus the tagged one"
    );
    assert_eq!(j.resolve("doctor", "healthy").unwrap().body["d"], 0);
    assert!(j.resolve("run", "healthy").is_err());
    assert!(j.tags().unwrap().contains_key("healthy"));
    for bad in ["1abc", "~1", "../x", "A", "a/b", "a..b"] {
        assert!(j.set_tag(bad, &["ports"]).is_err(), "{bad}");
    }
    // A corrupt tag protects nothing and does not stop retention.
    std::fs::write(dir.join("tags").join("broken.json"), "{").unwrap();
    for n in 5..8 {
        let r = j.record("ports", &serde_json::json!({ "n": n })).unwrap();
        assert_eq!(r.warnings.len(), 1, "the broken tag is reported");
    }
    assert_eq!(j.ids("ports").unwrap().len(), 3, "still pruned");
    std::fs::remove_file(dir.join("tags").join("broken.json")).unwrap();
    j.delete_tag("healthy").unwrap();
    j.record("ports", &serde_json::json!({ "n": 5 })).unwrap();
    assert_eq!(
        j.ids("ports").unwrap().len(),
        2,
        "pruned again once untagged"
    );
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn tiers_thin_old_snapshots_and_drop_the_oldest() {
    use crate::config::Tier;
    let minute = 60_000_000_000_u64;
    let now = 1_000 * minute;
    let id = |ago: u64| format!("{:020}-1-doctor", now - ago * minute);
    // Every minute for the last 200 minutes, oldest first.
    let ids: Vec<String> = (0..200).rev().map(id).collect();
    let tiers = [
        Tier {
            within_ms: 10 * 60_000,
            every_ms: 0,
        },
        Tier {
            within_ms: 100 * 60_000,
            every_ms: 30 * 60_000,
        },
    ];
    let (keep, drop) = thin(&ids, &tiers, now);
    assert_eq!(keep.len() + drop.len(), 200);
    // All of the last ten minutes, then about one per half hour.
    assert!(keep.contains(&id(0)) && keep.contains(&id(10)));
    let thinned = keep
        .iter()
        .filter(|k| !(0..=10).any(|a| **k == id(a)))
        .count();
    assert!((3..=4).contains(&thinned), "{thinned}");
    assert!(drop.contains(&id(150)), "past the last window");
    assert_eq!(thin(&ids, &[], now).0.len(), 200);
}

#[test]
fn time_references_find_how_things_were() {
    let (j, dir) = journal("asof", 10);
    j.record("ports", &serde_json::json!({ "n": 0 })).unwrap();
    assert_eq!(j.resolve("ports", "@0s").unwrap().body["n"], 0);
    assert!(matches!(
        j.resolve("ports", "@1h"),
        Err(StateError::NotFound { .. })
    ));
    assert_eq!(
        j.ids_since("ports", Duration::from_secs(3600))
            .unwrap()
            .len(),
        1
    );
    for bad in ["@", "@5", "@5w", "@-1h", "@1.5h"] {
        assert!(
            matches!(j.resolve("ports", bad), Err(StateError::BadRef(_))),
            "{bad}"
        );
    }
    assert_eq!(parse_age("90s"), Some(Duration::from_secs(90)));
    assert_eq!(parse_age("2d"), Some(Duration::from_secs(172_800)));
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn rejects_references_that_escape_the_directory() {
    let (j, dir) = journal("escape", 1);
    for bad in [
        "../x",
        "1-2-../../etc/passwd",
        "1-2-Ports",
        "~x",
        "",
        "1--ports",
    ] {
        assert!(j.resolve("ports", bad).is_err(), "{bad}");
    }
    assert!(j.record("../x", &()).is_err());
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn ignores_temporaries_and_foreign_files() {
    let (j, dir) = journal("foreign", 5);
    j.record("ports", &()).unwrap();
    std::fs::write(dir.join(".1-1-ports.tmp"), "{").unwrap();
    std::fs::write(dir.join("notes.json"), "{}").unwrap();
    assert_eq!(j.ids("ports").unwrap().len(), 1);
    std::fs::remove_dir_all(dir).unwrap();
}
