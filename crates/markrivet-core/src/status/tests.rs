use super::*;
use crate::ErrorCode;
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;

fn project() -> Project {
    let root = crate::test_support::scratch("rivet-status-enumeration");
    let project = Project::init(&root).unwrap();
    std::fs::create_dir_all(root.join("records/deep")).unwrap();
    std::fs::write(
        root.join(".rivet/project.json"),
        json!({"collections":{"tasks":{"schema":"task@1","files":["records/**/*.md"],"section":"Tasks","level":2}}}).to_string(),
    ).unwrap();
    std::fs::write(
        root.join(".rivet/schemas/task.json"),
        r#"{"id":"task","version":1,"item":"checkbox","fields":{"done":{"type":"boolean","from":"checkbox"}}}"#,
    ).unwrap();
    for file in ["records/one.md", "records/deep/two.md"] {
        std::fs::write(root.join(file), "## Tasks\n").unwrap();
    }
    let init = std::process::Command::new("git")
        .args(["init", "-q", "-b", "main"])
        .current_dir(&root)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap())
        .env("HOME", root.join(".rivet"))
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    assert!(init.status.success(), "{init:?}");
    project
}

fn report(project: &Project, faults: &WalkFaults) -> Value {
    let out = run_with(project, None, faults).unwrap();
    assert_eq!(out["status"], "read");
    out
}

#[test]
fn enumeration_failures_use_the_doctor_gap_rule() {
    let p = project();
    for faults in [
        WalkFaults {
            directories: BTreeSet::from(["records/deep".into()]),
            ..Default::default()
        },
        WalkFaults {
            entries: BTreeSet::from(["records".into()]),
            ..Default::default()
        },
        WalkFaults {
            types: BTreeSet::from(["records/one.md".into()]),
            ..Default::default()
        },
    ] {
        let out = report(&p, &faults);
        let doctor = crate::doctor::run_with(&p, None, None, false, &faults);
        assert_eq!(out["complete"], false, "{out}");
        assert_eq!(out["complete"], doctor.body["complete"]);
        assert!(!out["paths"].as_array().unwrap().is_empty());
        assert!(
            out["diagnostics"]
                .as_array()
                .unwrap()
                .iter()
                .any(|d| d["code"] == "io"),
            "{out}"
        );
    }
    assert_eq!(report(&p, &WalkFaults::default())["complete"], true);
    assert_eq!(
        std::fs::read_to_string(p.root.join("records/one.md")).unwrap(),
        "## Tasks\n"
    );
    std::fs::remove_dir_all(&p.root).unwrap();
}

#[test]
fn policy_exclusions_and_unrelated_errors_leave_enumeration_complete() {
    let p = project();
    std::fs::create_dir(p.root.join("unrelated")).unwrap();
    for dir in [".drafts", "target", "node_modules"] {
        std::fs::create_dir_all(p.root.join("records").join(dir)).unwrap();
        std::fs::write(
            p.root.join("records").join(dir).join("hidden.md"),
            "## Tasks\n",
        )
        .unwrap();
    }
    std::os::unix::fs::symlink("one.md", p.root.join("records/link.md")).unwrap();
    let out = report(
        &p,
        &WalkFaults {
            types: BTreeSet::from(["unrelated".into()]),
            ..Default::default()
        },
    );
    assert_eq!(out["complete"], true, "{out}");
    assert_eq!(out["paths"].as_array().unwrap().len(), 2);
    let diagnostics = out["diagnostics"].as_array().unwrap();
    assert_eq!(diagnostics.len(), 4, "{out}");
    assert!(diagnostics.iter().all(|d| d["code"] == "path_rejected"));
    std::fs::remove_dir_all(&p.root).unwrap();
}

#[test]
fn non_utf8_names_only_bound_matching_names_make_enumeration_incomplete() {
    let p = project();
    let path = p.root.join("records").join(OsStr::from_bytes(b"\xfe.txt"));
    std::fs::write(&path, "ignored").unwrap();
    assert_eq!(report(&p, &WalkFaults::default())["complete"], true);
    std::fs::write(
        p.root.join("records").join(OsStr::from_bytes(b"\xfe.md")),
        "## Tasks\n",
    )
    .unwrap();
    let out = report(&p, &WalkFaults::default());
    assert_eq!(out["complete"], false, "{out}");
    assert_eq!(out["paths"].as_array().unwrap().len(), 2);
    assert!(
        out["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|d| d["code"] == "path_rejected"
                && d["message"].as_str().unwrap().contains("hex fe2e6d64")),
        "{out}"
    );
    std::fs::remove_dir_all(&p.root).unwrap();
}

#[test]
fn traversal_cap_marks_the_partial_list_incomplete() {
    let p = project();
    for i in 0..10_001 {
        std::fs::write(p.root.join(format!("records/{i}.md")), "## Tasks\n").unwrap();
    }
    let out = report(&p, &WalkFaults::default());
    assert_eq!(out["complete"], false);
    assert_eq!(out["paths"].as_array().unwrap().len(), 10_000);
    assert!(
        out["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|d| d["code"] == serde_json::to_value(ErrorCode::LimitExceeded).unwrap()),
        "missing traversal cutoff"
    );
    std::fs::remove_dir_all(&p.root).unwrap();
}

#[test]
fn explicit_paths_define_a_complete_set_without_traversal() {
    let p = project();
    let out = run_with(
        &p,
        Some(vec!["records/one.md".into(), "records/missing.md".into()]),
        &WalkFaults {
            directories: BTreeSet::from([String::new()]),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(out["complete"], true, "{out}");
    assert_eq!(out["paths"].as_array().unwrap().len(), 2);
    std::fs::remove_dir_all(&p.root).unwrap();
}
