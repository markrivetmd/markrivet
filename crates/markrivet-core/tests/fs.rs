#[path = "../../test-support/mod.rs"]
mod test_support;
use markrivet_core::fs::{Project, WalkFaults};
use std::collections::BTreeSet;
use std::path::PathBuf;

fn scratch(name: &str) -> PathBuf {
    let dir = test_support::scratch(&format!("fs-{name}"));
    std::fs::create_dir_all(dir.join("docs")).unwrap();
    std::fs::write(dir.join("docs/a.md"), "# A\n").unwrap();
    dir
}

#[test]
fn a_held_root_survives_its_path_being_replaced() {
    let dir = scratch("held");
    let project = Project::init(&dir).unwrap();
    let moved = dir.with_extension("moved");
    let _ = std::fs::remove_dir_all(&moved);
    std::fs::rename(&dir, &moved).unwrap();
    std::fs::create_dir_all(dir.join(".rivet")).unwrap();
    std::fs::create_dir_all(dir.join("docs")).unwrap();
    std::fs::write(dir.join("docs/decoy.md"), "# Decoy\n").unwrap();
    // Listing, reading and configuration all follow the held descriptor.
    let (files, errors) = project.list();
    assert!(errors.is_empty(), "{errors:?}");
    assert_eq!(files, ["docs/a.md"]);
    assert_eq!(project.read("docs/a.md").unwrap(), "# A\n");
    assert!(project.read("docs/decoy.md").is_err());
    assert_eq!(
        project.read_config(&[".gitignore"]).unwrap().unwrap(),
        b"/lock\n/seq\n/pending/\n/commits.jsonl\n/push.jsonl\n"
    );
}

#[test]
fn oversized_configuration_is_refused_not_truncated() {
    let dir = scratch("config");
    let project = Project::init(&dir).unwrap();
    let mut schema = br#"{"id": "x"}"#.to_vec();
    schema.resize(8 * 1024 * 1024 + 10, b' ');
    std::fs::write(dir.join(".rivet/schemas/x.json"), schema).unwrap();
    let e = project.read_config(&["schemas", "x.json"]).unwrap_err();
    assert_eq!(e.code, markrivet_core::ErrorCode::LimitExceeded);
}

#[test]
fn configuration_members_refuse_fifos_without_blocking() {
    use std::ffi::CString;
    let root = test_support::scratch("rivet-config-fifo");
    let project = markrivet_core::fs::Project::init(&root).unwrap();
    let fifo = CString::new(root.join(".rivet/fifo.json").to_str().unwrap()).unwrap();
    // SAFETY: a terminated path in this test's own fresh directory.
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
    let error = project.read_config(&["fifo.json"]).unwrap_err();
    assert_eq!(error.code, markrivet_core::ErrorCode::PathRejected);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn entry_and_file_type_failures_are_listing_diagnostics_not_absent_files() {
    let dir = scratch("listing-faults");
    std::fs::create_dir_all(dir.join("queue")).unwrap();
    std::fs::write(dir.join("queue/record.md"), "# R\n").unwrap();
    std::fs::write(dir.join("queue/other.md"), "# O\n").unwrap();
    let project = Project::init(&dir).unwrap();
    let (files, errors) = project.list();
    assert!(errors.is_empty(), "{errors:?}");
    assert_eq!(files, ["docs/a.md", "queue/other.md", "queue/record.md"]);

    let faults = WalkFaults {
        entries: BTreeSet::from(["queue".to_string()]),
        types: BTreeSet::from(["queue/record.md".to_string()]),
        ..WalkFaults::default()
    };
    let (files, errors) = project.list_with(&faults);
    assert_eq!(files, ["docs/a.md", "queue/other.md"]);
    let paths: BTreeSet<_> = errors.iter().map(|e| e.path.as_deref().unwrap()).collect();
    assert_eq!(paths, BTreeSet::from(["queue", "queue/record.md"]));
    assert!(
        errors
            .iter()
            .all(|e| e.code == markrivet_core::contract::ErrorCode::Io)
    );
}

#[test]
fn documents_scope_gaps_and_strict_snapshot_policy_share_discovery() {
    use markrivet_core::ErrorCode;
    use std::os::unix::ffi::OsStrExt;
    let dir = scratch("policies");
    let project = Project::init(&dir).unwrap();
    std::fs::write(dir.join("docs/notes.txt"), "ordinary bytes\n").unwrap();
    std::fs::create_dir_all(dir.join("unrelated")).unwrap();
    std::fs::create_dir_all(dir.join("target")).unwrap();
    std::fs::write(dir.join("target/build.md"), "ignored").unwrap();
    let faults = WalkFaults {
        directories: ["unrelated".into()].into(),
        ..Default::default()
    };
    let (files, errors) = project.documents(&["docs/*".into()], None, &faults);
    assert_eq!(files, ["docs/a.md"]);
    assert!(errors.is_empty(), "{errors:?}");
    std::fs::write(dir.join("docs/.hidden.md"), "ignored").unwrap();
    let (files, errors) = project.list_strict();
    std::fs::remove_file(dir.join("docs/.hidden.md")).unwrap();
    assert_eq!(files, ["docs/a.md", "docs/notes.txt"]);
    assert!(errors.is_empty(), "{errors:?}");

    // Even a visible symlink outside the document binding refuses a snapshot.
    std::os::unix::fs::symlink(dir.join("docs/a.md"), dir.join("unrelated/link")).unwrap();
    assert!(
        project
            .documents(&["docs/*".into()], None, &faults)
            .1
            .is_empty()
    );
    let (_, errors) = project.list_strict();
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert_eq!(errors[0].path.as_deref(), Some("unrelated/link"));
    assert_eq!(errors[0].code, ErrorCode::PathRejected);
    std::fs::remove_file(dir.join("unrelated/link")).unwrap();

    // Strict discovery rejects native names before hidden-entry filtering.
    std::fs::write(
        dir.join(std::ffi::OsStr::from_bytes(b".hidden\xff")),
        "native",
    )
    .unwrap();
    assert!(
        project
            .documents(&["docs/*".into()], None, &faults)
            .1
            .is_empty()
    );
    assert_eq!(project.list_strict().1.len(), 1);
    assert_eq!(
        std::fs::read(dir.join("docs/notes.txt")).unwrap(),
        b"ordinary bytes\n"
    );
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn scoped_strict_walk_keeps_relevant_gaps_and_proves_unrelated_entries_outside_scope() {
    use std::os::unix::{ffi::OsStrExt, fs::symlink};
    let dir = scratch("scoped-snapshot");
    let project = Project::init(&dir).unwrap();
    std::fs::create_dir_all(dir.join("unrelated")).unwrap();
    symlink("missing", dir.join("save.sh")).unwrap();
    symlink("missing", dir.join("docs/helper.sh")).unwrap();
    std::fs::write(
        dir.join(std::ffi::OsStr::from_bytes(b"other\xff.md")),
        "outside",
    )
    .unwrap();
    std::fs::write(
        dir.join("docs")
            .join(std::ffi::OsStr::from_bytes(b"other\xff.txt")),
        "outside",
    )
    .unwrap();
    let globs = vec!["docs/*.md".into()];
    let (files, errors) = project.list_strict_scoped(&globs);
    assert_eq!(files, ["docs/a.md"]);
    assert!(errors.is_empty(), "{errors:?}");
    for operation in ["directories", "entries", "types"] {
        let faults = match operation {
            "directories" => WalkFaults {
                directories: ["docs".into()].into(),
                ..Default::default()
            },
            "entries" => WalkFaults {
                entries: ["docs".into()].into(),
                ..Default::default()
            },
            _ => WalkFaults {
                types: ["docs/a.md".into()].into(),
                ..Default::default()
            },
        };
        assert_eq!(
            project.list_strict_scoped_with(&globs, &faults).1.len(),
            1,
            "{operation}"
        );
    }
    let faults = WalkFaults {
        directories: ["unrelated".into()].into(),
        types: ["save.sh".into()].into(),
        ..Default::default()
    };
    assert!(
        project
            .list_strict_scoped_with(&globs, &faults)
            .1
            .is_empty()
    );
    symlink("missing", dir.join("docs/link.md")).unwrap();
    assert_eq!(
        project.list_strict_scoped(&globs).1[0].path.as_deref(),
        Some("docs/link.md")
    );
    std::fs::remove_file(dir.join("docs/link.md")).unwrap();
    // A symlink can conceal an entire relevant subtree, even without a Markdown suffix.
    symlink("missing", dir.join("docs/subtree")).unwrap();
    assert!(project.list_strict_scoped(&globs).1.is_empty());
    assert_eq!(
        project.list_strict_scoped(&["docs/**/*.md".into()]).1.len(),
        2 // Both subtree and helper.sh could conceal matching descendants.
    );
    std::fs::remove_file(dir.join("docs/subtree")).unwrap();
    let fifo = std::ffi::CString::new(dir.join("docs/pipe.md").as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
    let (_, errors) = project.list_strict_scoped(&globs);
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].code, markrivet_core::ErrorCode::PathRejected);
    assert_eq!(errors[0].path.as_deref(), Some("docs/pipe.md"));
    assert!(
        project
            .list_strict()
            .1
            .iter()
            .any(|e| e.path.as_deref() == Some("docs/pipe.md"))
    );
    std::fs::remove_file(dir.join("docs/pipe.md")).unwrap();
    std::fs::write(
        dir.join("docs")
            .join(std::ffi::OsStr::from_bytes(b"other\xff.md")),
        "unknown",
    )
    .unwrap();
    assert_eq!(project.list_strict_scoped(&globs).1.len(), 1);
    assert_eq!(std::fs::read(dir.join("docs/a.md")).unwrap(), b"# A\n");
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn native_gap_classification_uses_bytes_even_when_type_check_failed() {
    use markrivet_core::fs::WalkEvent;
    use markrivet_core::{Diagnostic, ErrorCode};
    let globs = vec!["docs/*.md".into()];
    for (hex, matches) in [("ff2e6d64", true), ("ff2e747874", false)] {
        for failed in [false, true] {
            let event = WalkEvent {
                path: Some("docs".into()),
                operation: if failed { "file_type" } else { "exclude" }.into(),
                reason: "non_utf8".into(),
                scope: "unknown".into(),
                descendants_unknown: failed,
                native_name_hex: Some(hex.into()),
                error: failed.then(|| Diagnostic::new(ErrorCode::Io, "injected")),
            };
            assert_eq!(event.is_gap(&globs), matches, "{event:?}");
            assert!(!event.is_gap(&["other/*.md".into()]), "{event:?}");
        }
    }
}
