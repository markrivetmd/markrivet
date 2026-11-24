#[path = "../../test-support/mod.rs"]
mod test_support;
use markrivet_core::doctor::run_with;
use markrivet_core::fs::{Project, WalkFaults};
use serde_json::json;
use std::path::PathBuf;

fn scratch(name: &str) -> PathBuf {
    let dir = test_support::scratch(&format!("doctor-walk-{name}"));
    std::fs::create_dir_all(dir.join("records/.drafts")).unwrap();
    std::fs::create_dir_all(dir.join(".hidden")).unwrap();
    std::fs::write(dir.join("records/one.md"), "## Tasks\n").unwrap();
    dir
}

#[test]
fn root_glob_never_reports_rivet_config_as_skipped() {
    let dir = scratch("root-glob");
    let project = Project::init(&dir).unwrap();
    std::fs::write(
        dir.join(".rivet/project.json"),
        json!({"collections":{"tasks":{"schema":"task@1","files":["**/*.md"],"section":"Tasks","level":2}}}).to_string(),
    )
    .unwrap();
    std::fs::write(
        dir.join(".rivet/schemas/task.json"),
        r#"{"id":"task","version":1,"item":"checkbox","fields":{"done":{"type":"boolean","from":"checkbox"}}}"#,
    )
    .unwrap();
    // A **/*.md binding reaches the root .rivet, but Rivet's own configuration is
    // never a skipped document; other hidden entries it reaches are.
    let r = run_with(&project, None, None, false, &WalkFaults::default());
    assert_eq!(r.exit, 0, "{}", r.body);
    let skipped: Vec<_> = r.body["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|d| d["code"] == "excluded_entry")
        .map(|d| d["path"].as_str().unwrap())
        .collect();
    assert_eq!(skipped, [".hidden", "records/.drafts"], "{}", r.body);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn walker_limit_is_incomplete_and_keeps_requested_evidence() {
    let dir = scratch("limit");
    let project = Project::init(&dir).unwrap();
    std::fs::write(
        dir.join(".rivet/project.json"),
        json!({"collections":{"tasks":{"schema":"task@1","files":["records/*.md"],"section":"Tasks","level":2}}}).to_string(),
    )
    .unwrap();
    std::fs::write(
        dir.join(".rivet/schemas/task.json"),
        r#"{"id":"task","version":1,"item":"checkbox","fields":{"done":{"type":"boolean","from":"checkbox"}}}"#,
    )
    .unwrap();
    // One more matching file than the walker's 10,000-file bound.
    for n in 0..10_000 {
        std::fs::write(dir.join(format!("records/f{n}.md")), "## Tasks\n").unwrap();
    }
    let requested = "records/one.md".to_string();
    let r = run_with(
        &project,
        None,
        Some(vec![requested.clone()]),
        false,
        &WalkFaults::default(),
    );
    assert_eq!(r.exit, 2, "{}", r.body["summary"]);
    assert_eq!(r.body["complete"], false);
    assert_eq!(r.body["health"], "unknown");
    assert_eq!(r.body["counts"], "lower_bound");
    let limit: Vec<_> = r.body["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|d| d["code"] == "enumeration")
        .collect();
    assert_eq!(limit.len(), 1, "{}", r.body["diagnostics"]);
    assert!(
        limit[0]["next_action"]
            .as_str()
            .unwrap()
            .starts_with("narrow the binding globs")
    );
    assert!(
        r.body["traversal_events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| { e["reason"] == "limit" && e["error"]["code"] == "limit_exceeded" })
    );
    // The requested file is still read and diagnosed in full.
    assert_eq!(r.body["summary"]["files_parsed"], 1);
    assert_eq!(r.body["files"][0]["path"], requested);
    assert_eq!(r.body["files"][0]["status"], "parsed");
    std::fs::remove_dir_all(&dir).unwrap();
}

/// The citation check walks every collection, not only the selected one, and
/// a directory it cannot list there is named rather than read as an absence.
#[test]
fn a_walk_gap_outside_the_selection_is_named_by_the_citation_check() {
    let dir = test_support::scratch("doctor-walk-citation-gap");
    std::fs::create_dir_all(dir.join("a")).unwrap();
    std::fs::create_dir_all(dir.join("b")).unwrap();
    let project = Project::init(&dir).unwrap();
    std::fs::write(
        dir.join(".rivet/project.json"),
        json!({"collections": {
            "a": {"schema": "task@1", "files": ["a/*.md"], "section": "Tasks", "level": 2},
            "b": {"schema": "task@1", "files": ["b/*.md"], "section": "Tasks", "level": 2}}})
        .to_string(),
    )
    .unwrap();
    std::fs::write(
        dir.join(".rivet/schemas/task.json"),
        r#"{"id":"task","version":1,"item":"checkbox","fields":{"done":{"type":"boolean","from":"checkbox"}}}"#,
    )
    .unwrap();
    std::fs::write(
        dir.join("a/one.md"),
        "## Tasks\n\n- [ ] after item:BeeItem1\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("b/one.md"),
        "## Tasks\n\n- [ ] bee <!-- stay:BeeItem1 subhash=sha256:111111111111 -->\n",
    )
    .unwrap();
    let codes = |faults: &WalkFaults| -> Vec<(String, String)> {
        let r = run_with(&project, Some("a".into()), None, false, faults);
        r.body["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|d| d["code"].as_str().unwrap().starts_with("citation_"))
            .map(|d| {
                (
                    d["code"].as_str().unwrap().to_string(),
                    d["path"].as_str().unwrap().to_string(),
                )
            })
            .collect()
    };
    assert_eq!(codes(&WalkFaults::default()), vec![]);
    let faults = WalkFaults {
        directories: ["b".to_string()].into(),
        ..Default::default()
    };
    let mut found = codes(&faults);
    found.sort();
    assert_eq!(
        found,
        [
            ("citation_scope".to_string(), "b".to_string()),
            ("citation_unresolved".to_string(), "a/one.md".to_string())
        ]
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

/// A non-UTF-8 name the selected glob cannot match is no gap for the main
/// walk, but another collection's glob could match it, so the citation check
/// still names it.
#[test]
fn a_name_only_another_collection_could_match_is_named_by_the_citation_check() {
    use std::os::unix::ffi::OsStrExt;
    let dir = test_support::scratch("doctor-walk-citation-name");
    std::fs::create_dir_all(dir.join("records")).unwrap();
    let project = Project::init(&dir).unwrap();
    std::fs::write(
        dir.join(".rivet/project.json"),
        json!({"collections": {
            "one": {"schema": "task@1", "files": ["records/one.md"], "section": "Tasks", "level": 2},
            "all": {"schema": "task@1", "files": ["records/*.md"], "section": "Tasks", "level": 2}}})
        .to_string(),
    )
    .unwrap();
    std::fs::write(
        dir.join(".rivet/schemas/task.json"),
        r#"{"id":"task","version":1,"item":"checkbox","fields":{"done":{"type":"boolean","from":"checkbox"}}}"#,
    )
    .unwrap();
    std::fs::write(
        dir.join("records/one.md"),
        "## Tasks\n\n- [ ] after item:HiddenId\n",
    )
    .unwrap();
    let name = std::ffi::OsStr::from_bytes(b"\xff.md");
    std::fs::write(dir.join("records").join(name), "## Tasks\n").unwrap();
    // No glob could match this one, so it is no gap for either walk.
    let other = std::ffi::OsStr::from_bytes(b"\xfe.txt");
    std::fs::write(dir.join("records").join(other), "text\n").unwrap();
    let r = run_with(
        &project,
        Some("one".into()),
        None,
        false,
        &WalkFaults::default(),
    );
    let codes: Vec<&str> = r.body["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["code"].as_str().unwrap())
        .collect();
    assert!(!codes.contains(&"malformed_name"), "{}", r.body);
    assert_eq!(
        codes.iter().filter(|c| **c == "citation_scope").count(),
        1,
        "{}",
        r.body
    );
    assert!(codes.contains(&"citation_unresolved"), "{}", r.body);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// A gap the main walk already reported for the selected collection is not
/// reported a second time by the citation check.
#[test]
fn a_gap_the_main_walk_reported_is_not_named_again() {
    let dir = test_support::scratch("doctor-walk-citation-dedup");
    std::fs::create_dir_all(dir.join("a")).unwrap();
    std::fs::create_dir_all(dir.join("x")).unwrap();
    let project = Project::init(&dir).unwrap();
    std::fs::write(
        dir.join(".rivet/project.json"),
        json!({"collections": {"a": {"schema": "task@1", "files": ["a/*.md", "x/*.md"],
                                     "section": "Tasks", "level": 2}}})
        .to_string(),
    )
    .unwrap();
    std::fs::write(
        dir.join(".rivet/schemas/task.json"),
        r#"{"id":"task","version":1,"item":"checkbox","fields":{"done":{"type":"boolean","from":"checkbox"}}}"#,
    )
    .unwrap();
    std::fs::write(
        dir.join("a/one.md"),
        "## Tasks\n\n- [ ] after item:HiddenId\n",
    )
    .unwrap();
    let faults = WalkFaults {
        directories: ["x".to_string()].into(),
        ..Default::default()
    };
    let r = run_with(&project, None, None, false, &faults);
    let codes: Vec<(&str, &str)> = r.body["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| {
            (
                d["code"].as_str().unwrap(),
                d["path"].as_str().unwrap_or(""),
            )
        })
        .collect();
    assert!(codes.contains(&("enumeration", "x")), "{}", r.body);
    assert!(
        codes.contains(&("citation_unresolved", "a/one.md")),
        "{}",
        r.body
    );
    assert!(
        !codes.iter().any(|(c, _)| *c == "citation_scope"),
        "{}",
        r.body
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn union_walk_faults_are_partitioned_and_archive_limits_are_conservative() {
    let dir = scratch("union");
    let project = Project::init(&dir).unwrap();
    std::fs::create_dir_all(dir.join("other")).unwrap();
    std::fs::create_dir_all(dir.join("archive")).unwrap();
    std::fs::write(dir.join("other/a.md"), "## Tasks\n").unwrap();
    std::fs::write(dir.join(".rivet/project.json"),
        json!({"collections":{
            "tasks":{"schema":"task@1","files":["records/*.md"],"archive":["archive/*.md"],"section":"Tasks","level":2},
            "other":{"schema":"task@1","files":["other/*.md"],"section":"Tasks","level":2}}}).to_string()).unwrap();
    std::fs::write(dir.join(".rivet/schemas/task.json"),
        r#"{"id":"task","version":1,"item":"checkbox","fields":{"done":{"type":"boolean","from":"checkbox"}}}"#).unwrap();
    for faults in [
        WalkFaults {
            directories: ["other".into()].into(),
            ..Default::default()
        },
        WalkFaults {
            entries: ["other".into()].into(),
            ..Default::default()
        },
        WalkFaults {
            types: ["other/a.md".into()].into(),
            ..Default::default()
        },
    ] {
        let r = run_with(&project, Some("tasks".into()), None, false, &faults);
        assert_eq!(r.body["complete"], true, "{}", r.body);
        // Raw events retain the whole union even when coverage selects tasks.
        assert!(
            r.body["traversal_events"]
                .as_array()
                .unwrap()
                .iter()
                .any(|e| {
                    e["path"]
                        .as_str()
                        .is_some_and(|p| p == "other" || p == "other/a.md")
                        && e["error"]["code"] == "io"
                }),
            "{}",
            r.body
        );
        assert_eq!(r.body["summary"]["files_discovered"], 1);
        assert_eq!(r.body["collections"][0]["coverage"]["files_discovered"], 1);
    }
    for n in 0..10_001 {
        std::fs::write(dir.join(format!("archive/{n}.md")), "archive\n").unwrap();
    }
    // Snapshot discovery keeps its uncapped whole-project namespace.
    let (snapshot_files, errors) = project.list_strict();
    assert!(errors.is_empty(), "{errors:?}");
    assert_eq!(snapshot_files.len(), 10_003);
    assert!(snapshot_files.windows(2).all(|p| p[0] < p[1]));
    for citation in [false, true] {
        std::fs::write(
            dir.join("records/one.md"),
            if citation {
                "## Tasks\n\nitem:Missing1\n"
            } else {
                "## Tasks\n"
            },
        )
        .unwrap();
        let r = run_with(
            &project,
            Some("tasks".into()),
            Some(vec!["records/one.md".into()]),
            false,
            &WalkFaults::default(),
        );
        assert_eq!(r.body["complete"], false, "{}", r.body);
        assert_eq!(r.body["summary"]["files_parsed"], 1);
        assert_eq!(r.body["counts"], "lower_bound");
        assert_eq!(
            r.body["diagnostics"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|d| d["code"] == "enumeration"
                    && d["message"].as_str().unwrap().contains("cap"))
                .count(),
            1
        );
    }
    std::fs::remove_dir_all(dir).unwrap();
}
