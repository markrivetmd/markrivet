use super::*;

fn setup() -> std::path::PathBuf {
    let root = project("bound-race");
    let baseline: Value =
        serde_json::from_str(include_str!("../fixtures/guards_before_binding.json")).unwrap();
    std::fs::write(
        root.join(".rivet/project.json"),
        baseline["config"].to_string(),
    )
    .unwrap();
    std::fs::write(
        root.join(".rivet/schemas/task.json"),
        baseline["schema_bytes"].as_str().unwrap(),
    )
    .unwrap();
    std::fs::write(root.join("T.md"), baseline["source"].as_str().unwrap()).unwrap();
    // A separate file lets the lock holder publish without changing either
    // target precondition. Both processes still acquire the same project lock.
    std::fs::write(root.join("Lock.md"), "## Holder\n\nOriginal.\n").unwrap();
    root
}
fn request(root: &Path, command: &str) -> Value {
    let (_, map) = rivet(root, "map", json!({"path":"T.md"}));
    if command == "stamp" {
        json!({"path":"T.md","guard":map["revision"],"collection":"tasks","checkbox_only":true,"collection_guard":map["collections"][0]["revision"]})
    } else {
        let (_, got) = rivet(
            root,
            "get",
            json!({"path":"T.md","select":{"kind":"id","id":"Item0001"}}),
        );
        json!({"path":"T.md","select":{"kind":"id","id":"Item0001"},"guard":got["revision"],"collection":"tasks","typed_guard":got["typed"]["tasks"]["revision"],"content":"[x] **One.** Replaced."})
    }
}
fn holder(root: &Path, pause: &Pause) -> std::process::Child {
    let select = json!({"kind":"heading_path","path":["Holder"]});
    let (_, got) = rivet(root, "get", json!({"path":"Lock.md","select":select}));
    spawn(
        root,
        "append",
        &json!({"path":"Lock.md","select":select,"guard":got["revision"],"content":"Published.\n"}),
        &[pause.env()],
    )
}
#[test]
fn paired_writers_recheck_binding_and_schema_after_waiting_for_the_lock() {
    for command in ["set", "stamp"] {
        for mutation in [
            "level",
            "files",
            "subsections",
            "section",
            "schema",
            "membership",
        ] {
            let root = setup();
            let mut original = request(&root, command);
            original["lock_timeout_ms"] = json!(60000);
            let pause = Pause::new(&root, "planned");
            let writer = holder(&root, &pause);
            pause.wait();
            let stale = spawn(&root, command, &original, &[]);
            wait_for_writer_lock(&root, &stale);
            let mut config: Value =
                serde_json::from_str(&read(&root, ".rivet/project.json")).unwrap();
            match mutation {
                "level" => config["collections"]["tasks"]["level"] = Value::Null,
                "files" => config["collections"]["tasks"]["files"] = json!(["*.md"]),
                "subsections" => config["collections"]["tasks"]["subsections"] = json!(true),
                "section" => {
                    config["collections"]["tasks"]["section"] = json!("Renamed");
                    std::fs::write(
                        root.join("T.md"),
                        read(&root, "T.md").replace("## Work", "## Renamed"),
                    )
                    .unwrap();
                }
                "schema" => std::fs::write(
                    root.join(".rivet/schemas/task.json"),
                    format!("{}\n", read(&root, ".rivet/schemas/task.json")),
                )
                .unwrap(),
                "membership" => std::fs::write(
                    root.join("T.md"),
                    read(&root, "T.md").replace("## Work", "## Elsewhere"),
                )
                .unwrap(),
                _ => unreachable!(),
            }
            std::fs::write(root.join(".rivet/project.json"), config.to_string()).unwrap();
            let before = read(&root, "T.md");
            pause.release();
            let (code, out) = finish(writer);
            assert_eq!(code, 0, "{out}");
            let (code, out) = finish(stale);
            assert_eq!(code, 2, "{command}/{mutation}: {out}");
            if mutation == "membership" {
                assert_eq!(out["error"]["code"], "collection_conflict", "{out}");
            } else {
                assert_eq!(out["error"]["code"], "guard_conflict", "{out}");
            }
            assert_eq!(read(&root, "T.md"), before, "{command}/{mutation}");
            assert!(read(&root, "Lock.md").contains("Published."));
        }
    }
}
#[test]
fn paired_writers_rename_failure_preserves_complete_preimage_and_stays() {
    for command in ["set", "stamp"] {
        let root = setup();
        if command == "stamp" {
            let before = read(&root, "T.md");
            std::fs::write(
                root.join("T.md"),
                before.replace(
                    "- [x] **Two.** Keep.",
                    "- [ ] **New.**\n- [x] **Two.** Keep.",
                ),
            )
            .unwrap();
        }
        let before = read(&root, "T.md");
        let (code, out) = finish(spawn(
            &root,
            command,
            &request(&root, command),
            &[("RIVET_TEST_FAULT", "rename".into())],
        ));
        assert_eq!(code, 2, "{out}");
        assert_eq!(out["error"]["code"], "io", "{out}");
        assert_eq!(read(&root, "T.md"), before);
    }
}
#[test]
fn bound_set_peer_edit_keeps_native_guard_but_invalidates_original_typed_guard() {
    let root = setup();
    let original = request(&root, "set");
    let pause = Pause::new(&root, "locked");
    let writer = spawn(&root, "set", &original, &[pause.env()]);
    pause.wait();
    // The lock is held but its preimage/binding read has not happened.
    let mut config: Value = serde_json::from_str(&read(&root, ".rivet/project.json")).unwrap();
    config["collections"]["tasks"]["archive"] = json!(["archive/*.md"]);
    std::fs::write(root.join(".rivet/project.json"), config.to_string()).unwrap();
    let before = read(&root, "T.md");
    pause.release();
    let (code, out) = finish(writer);
    assert_eq!(code, 2, "{out}");
    assert_eq!(out["error"]["code"], "guard_conflict", "{out}");
    assert_eq!(read(&root, "T.md"), before);
}
