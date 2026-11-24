//! Isolate the lifecycle checks from the planner's deliberately redundant checks.
use super::*;
use std::path::PathBuf;

const PATH: &str = "page.md";

fn fixture(content: &str) -> (PathBuf, Project) {
    let root = crate::test_support::scratch("rivet-transaction");
    let project = Project::init(&root).unwrap();
    std::fs::write(root.join(PATH), content).unwrap();
    (root, project)
}

fn options(kind: Kind<'_>) -> Options<'_> {
    Options {
        kind,
        preview: true,
        timeout: None,
        by: Attribution {
            message: None,
            session: None,
        },
        require_commit: false,
        pending: Pending::Settle,
    }
}

#[test]
fn transaction_loader_reads_and_parses_the_named_file() {
    let (root, project) = fixture("# Preserve\r\n\r\nActual bytes.\r\n");
    let tx = Context {
        project: &project,
        path: PATH,
        kind: Kind::Edit,
    };
    let loaded = tx.load().unwrap();
    assert_eq!(
        loaded.doc.source,
        std::fs::read_to_string(root.join(PATH)).unwrap()
    );
    assert_eq!(loaded.doc.sections[0].path, ["Preserve"]);
    std::fs::write(root.join(PATH), "# Invalid\rsource\n").unwrap();
    assert!(tx.load().err().unwrap().message.contains("lone CR"));
    std::fs::remove_file(root.join(PATH)).unwrap();
    assert_eq!(tx.load().err().unwrap().code, ErrorCode::NotFound);
}

#[test]
fn transaction_loader_refuses_blockers_without_a_planner() {
    let source = "# A\n\ntext\n{/* stay:mdx */}\n";
    let (root, project) = fixture(source);
    let tx = Context {
        project: &project,
        path: PATH,
        kind: Kind::Edit,
    };
    let expected = Document::parse(source)
        .unwrap()
        .write_blocker
        .unwrap()
        .with_path(PATH);
    assert_eq!(tx.load().err().expect("loader must refuse"), expected);
    assert_eq!(std::fs::read_to_string(root.join(PATH)).unwrap(), source);
}

#[test]
fn transaction_loader_resolves_matching_bindings_but_archive_uses_its_source_binding() {
    let (_, project) = fixture("# A\n\nKeep.\n");
    let config = json!({"collections":{"unavailable":{
        "schema":"missing@1", "files":["page.md"], "section":"A"
    }}});
    std::fs::write(project.root.join(".rivet/project.json"), config.to_string()).unwrap();
    let tx = Context {
        project: &project,
        path: PATH,
        kind: Kind::Edit,
    };
    assert_eq!(tx.load().err().unwrap().code, ErrorCode::InvalidSchema);
    let tx = Context {
        kind: Kind::ArchiveCopy,
        ..tx
    };
    assert!(tx.load().unwrap().bindings.is_empty());
}

#[test]
fn transaction_create_validates_supplied_content_and_absence_even_in_preview() {
    let (root, project) = fixture("# Existing\n");
    let tx = Context {
        project: &project,
        path: PATH,
        kind: Kind::Create("# A\n<!-- stay:owned -->\n"),
    };
    assert_eq!(tx.load().err().unwrap().code, ErrorCode::InvalidContent);
    let out = transact(&project, PATH, options(Kind::Create("# New\n")), |tx| {
        Ok(Draft::new(tx.load()?, "# New\n".into(), json!({})).ready())
    });
    assert_eq!(out.exit, 2, "{}", out.body);
    assert_eq!(out.body["error"]["code"], "already_exists");
    assert_eq!(
        std::fs::read_to_string(root.join(PATH)).unwrap(),
        "# Existing\n"
    );
}

#[test]
fn transaction_caps_candidates_before_preview_without_relying_on_planning() {
    let (root, project) = fixture("# A\n");
    let out = transact(&project, PATH, options(Kind::Edit), |tx| {
        Ok(Draft::new(
            tx.load()?,
            "x".repeat(MAX_FILE_BYTES as usize + 1),
            json!({}),
        )
        .ready())
    });
    assert_eq!(out.exit, 2, "{}", out.body);
    assert_eq!(out.body["error"]["code"], "limit_exceeded");
    assert_eq!(std::fs::read_to_string(root.join(PATH)).unwrap(), "# A\n");
}

#[test]
fn transaction_restore_pending_policy_refuses_even_a_preview_noop_before_planning() {
    let (root, project) = fixture("# A\n");
    std::fs::create_dir(root.join(".rivet/pending")).unwrap();
    let pending = root.join(".rivet/pending/broken.json");
    std::fs::write(&pending, "{\"op\":").unwrap();
    let mut opts = options(Kind::Edit);
    opts.pending = Pending::Refuse;
    let out = transact(&project, PATH, opts, |_| {
        Ok(Plan::Unchanged(json!({"status":"unchanged"})))
    });
    assert_eq!(
        out.body["error"]["code"], "pending_unresolved",
        "{}",
        out.body
    );
    assert_eq!(std::fs::read_to_string(pending).unwrap(), "{\"op\":");
    assert_eq!(std::fs::read_to_string(root.join(PATH)).unwrap(), "# A\n");
}

#[test]
fn transaction_rechecks_archive_source_before_entering_commit_preflight() {
    let (root, project) = fixture("# A\n");
    std::fs::write(
        root.join(".rivet/project.json"),
        r#"{"git":{"commit":"per-write","on_dirty":"refuse"}}"#,
    )
    .unwrap();
    let mut opts = options(Kind::ArchiveCopy);
    opts.preview = false;
    opts.require_commit = true;
    let out = transact(&project, PATH, opts, |tx| {
        let mut draft = Draft::new(tx.load()?, "# A\n\nCandidate.\n".into(), json!({}));
        draft.archive_copy = Some(crate::archive::CopyIntent {
            source: "missing.md".into(),
            id: "copied01".into(),
            guard: "unused".into(),
            path: PATH.into(),
            item_sha256: "unused".into(),
            ids: vec![],
        });
        Ok(draft.ready())
    });
    // No repository exists, so reaching commit preflight would instead report
    // commit_refused. Isolate this early check from landing's redundant rechecks.
    assert_eq!(out.body["error"]["code"], "not_found", "{}", out.body);
    assert_eq!(std::fs::read_to_string(root.join(PATH)).unwrap(), "# A\n");
    assert!(!root.join(".rivet/pending").exists());
}
