//! E1 writer-lock, publication and recovery boundaries with real processes.
use super::*;
use markrivet_core::{Document, document::Target};

const ARCHIVE: &str = "archive/month/closed.md";
const HISTORY: &str = "history/month/earlier.md";

fn sole_store() -> Store {
    let s = archive_store("list-race");
    s.configure(json!({"commit":"off"}));
    std::fs::write(s.root.join(WIDGETS), "# Synthetic\n\n## Action\n\n- [x] **Closed.** Retained nested prose.\n\n## Log\n\nSuffix.\n").unwrap();
    stamp(&s.root, WIDGETS);
    s.configure(json!({"commit":"per-write","on_dirty":"refuse"}));
    let path = s.root.join(".rivet/project.json");
    let mut config: Value = serde_json::from_str(&read(&s.root, ".rivet/project.json")).unwrap();
    config["section_archives"] =
        json!({"history":{"files":["other/*.md"],"archive":["history/**/*.md"]}});
    std::fs::write(path, config.to_string()).unwrap();
    std::fs::create_dir_all(s.root.join("history/month")).unwrap();
    std::fs::write(s.root.join(HISTORY), "# Earlier\n\nUncited.\n").unwrap();
    s.git(&["add", "-A"]);
    s.git(&["commit", "-qm", "synthetic sole list"]);
    s
}

fn request(s: &Store) -> (Value, String, String) {
    let doc = Document::parse(&read(&s.root, WIDGETS)).unwrap();
    let target = Target::Item { list: 0, item: 0 };
    let item = doc.target_ids(target)[0].clone();
    let block = Target::Block(doc.lists[0].block);
    let list = doc.target_ids(block)[0].clone();
    (
        json!({"path":WIDGETS,"select":{"kind":"id","id":item},"guard":doc.target_revision(target),"list":{"guard":doc.target_revision(block)}}),
        item,
        list,
    )
}

#[test]
fn list_delete_citations_added_at_locked_planned_and_pending_written_refuse() {
    for point in ["locked", "planned", "pending_written"] {
        let s = sole_store();
        let (request, _, list) = request(&s);
        let before = read(&s.root, WIDGETS);
        let head = s.head();
        let index = std::fs::read(s.root.join(".git/index")).unwrap();
        let pause = Pause::new(&s.root, point);
        let child = pause.wait_for(s.spawn("delete", &request, &[pause.env()]));
        std::fs::write(
            s.root.join(HISTORY),
            format!("# Earlier\n\nSee list:{list}.\n"),
        )
        .unwrap();
        pause.release();
        let (code, out) = finish(child);
        assert_eq!(code, 2, "{point}: {out}");
        assert_eq!(
            out["error"]["code"], "list_citation_refused",
            "{point}: {out}"
        );
        assert_eq!(read(&s.root, WIDGETS), before);
        assert_eq!(s.head(), head);
        assert_eq!(std::fs::read(s.root.join(".git/index")).unwrap(), index);
    }
}

#[test]
fn list_delete_second_item_inserted_at_locked_is_rejected() {
    let s = sole_store();
    let (request, _, _) = request(&s);
    let before = read(&s.root, WIDGETS);
    let doc = Document::parse(&before).unwrap();
    let at = doc.lists[0].items[0].span.end;
    let mut changed = before.clone();
    changed.insert_str(at, "\n- [ ] A new sibling.\n");
    let pause = Pause::new(&s.root, "locked");
    let child = pause.wait_for(s.spawn("delete", &request, &[pause.env()]));
    std::fs::write(s.root.join(WIDGETS), &changed).unwrap();
    // Keep the fixture eligible so the structural check, rather than dirty
    // policy, must detect the inserted item after the locked reread.
    s.git(&["add", WIDGETS]);
    s.git(&["commit", "-qm", "synthetic concurrent sibling"]);
    let head = s.head();
    let index = std::fs::read(s.root.join(".git/index")).unwrap();
    pause.release();
    let (code, out) = finish(child);
    assert_eq!(code, 2, "{out}");
    assert_eq!(out["error"]["code"], "invalid_request", "{out}");
    assert_eq!(read(&s.root, WIDGETS), changed);
    assert_eq!(s.head(), head);
    assert_eq!(std::fs::read(s.root.join(".git/index")).unwrap(), index);
}

#[test]
fn list_delete_recovery_checks_list_ids_and_keeps_the_receipt_on_refusal() {
    for cite_item in [false, true] {
        let s = sole_store();
        let (request, item, list) = request(&s);
        let before = read(&s.root, WIDGETS);
        let head = s.head();
        let index = std::fs::read(s.root.join(".git/index")).unwrap();
        s.kill_at("after_rename", "delete", &request, &[]);
        let candidate = read(&s.root, WIDGETS);
        let record = s.record();
        assert_eq!(record["delete_receipt"]["version"], 2, "{record}");
        let receipt_dropped = record["delete_receipt"]["operation"]["dropped"]
            .as_array()
            .unwrap();
        assert_eq!(receipt_dropped.len(), 2);
        assert!(receipt_dropped.contains(&json!(item)));
        assert!(receipt_dropped.contains(&json!(list)));
        std::fs::write(
            s.root.join(HISTORY),
            format!(
                "# Earlier\n\nSee {}.\n",
                if cite_item { item } else { list }
            ),
        )
        .unwrap();
        let out = s.recover(json!({}), &[]);
        if cite_item {
            assert_eq!(one_result(&out)["result"], "committed", "{out}");
            assert!(s.records().is_empty());
            let receipt = &s.receipts().last().unwrap()["delete_receipt"].clone();
            assert_eq!(receipt["version"], 2);
        } else {
            assert_eq!(one_result(&out)["result"], "repair_required", "{out}");
            assert_eq!(s.head(), head);
            assert_eq!(std::fs::read(s.root.join(".git/index")).unwrap(), index);
            assert_eq!(s.record()["delete_receipt"], record["delete_receipt"]);
            assert!(
                s.record()["repair"]["detail"]
                    .as_str()
                    .unwrap()
                    .contains(HISTORY),
                "{}",
                s.record()
            );
            assert_eq!(s.git(&["show", &format!("HEAD:{WIDGETS}")]), before);
        }
        assert_eq!(read(&s.root, WIDGETS), candidate);
    }
}

#[test]
fn list_delete_publication_ignores_citations_of_item_ids() {
    let s = sole_store();
    let (request, item, _) = request(&s);
    let pause = Pause::new(&s.root, "pending_written");
    let child = pause.wait_for(s.spawn("delete", &request, &[pause.env()]));
    std::fs::write(
        s.root.join(HISTORY),
        format!("# Earlier\n\nSee item:{item}.\n"),
    )
    .unwrap();
    pause.release();
    let (code, out) = finish(child);
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["commit"]["status"], "committed", "{out}");
}

#[test]
fn list_archive_copy_publishes_recovers_and_retries_even_when_list_is_cited() {
    for recovery in [false, true] {
        let s = sole_store();
        let (_, item, list) = request(&s);
        let before = read(&s.root, WIDGETS);
        let request = archive_request(&s, &item);
        let pause = Pause::new(&s.root, if recovery { "after_rename" } else { "planned" });
        let mut child = pause.wait_for(s.spawn("archive-copy", &request, &[pause.env()]));
        std::fs::write(
            s.root.join(HISTORY),
            format!("# Earlier\n\nSee list:{list}.\n"),
        )
        .unwrap();
        if recovery {
            child.kill().unwrap();
            child.wait().unwrap();
            let out = s.recover(json!({}), &[]);
            assert_eq!(one_result(&out)["result"], "committed", "{out}");
        } else {
            pause.release();
            let (code, out) = finish(child);
            assert_eq!(code, 0, "{out}");
            assert_eq!(out["commit"]["status"], "committed", "{out}");
        }
        let archive = read(&s.root, ARCHIVE);
        let (code, out) = s.run("archive-copy", request, &[]);
        assert_eq!(code, 0, "{out}");
        assert_eq!(out["status"], "unchanged", "{out}");
        assert_eq!(read(&s.root, ARCHIVE), archive);
        assert_eq!(read(&s.root, WIDGETS), before);
    }
}

#[test]
fn list_delete_crash_and_fault_boundaries_keep_one_receipted_outcome() {
    for point in [
        "pending_written",
        "before_rename",
        "after_rename",
        "committed",
        "finalise",
    ] {
        let s = sole_store();
        let (request, _, _) = request(&s);
        let before = read(&s.root, WIDGETS);
        let count = s.receipts().len();
        s.kill_at(point, "delete", &request, &[]);
        let out = s.recover(json!({}), &[]);
        assert_ne!(
            one_result(&out)["result"],
            "repair_required",
            "{point}: {out}"
        );
        assert!(s.records().is_empty());
        let events = s.receipts();
        assert_eq!(events.len(), count + 1, "{point}: {events:?}");
        if ["pending_written", "before_rename"].contains(&point) {
            assert_eq!(read(&s.root, WIDGETS), before);
        } else {
            assert_eq!(events.last().unwrap()["delete_receipt"]["version"], 2);
            let (code,out) = s.run("verify-delete",json!({"parent":before,"candidate":read(&s.root,WIDGETS),"receipt":events.last().unwrap()["delete_receipt"]}),&[]);
            assert_eq!(code, 0, "{point}: {out}");
        }
        let _ = s.recover(json!({}), &[]);
        assert_eq!(s.receipts().len(), events.len());
    }
    for point in [
        "rename",
        "dir_sync",
        "receipt_append",
        "receipt_sync",
        "record_retire",
    ] {
        let s = sole_store();
        let (request, _, _) = request(&s);
        let before = read(&s.root, WIDGETS);
        let head = s.head();
        let (code, out) = s.run("delete", request, &[("RIVET_TEST_FAULT", point.into())]);
        if point == "rename" {
            assert_eq!(code, 2, "{out}");
            assert_eq!(read(&s.root, WIDGETS), before);
            assert_eq!(s.head(), head);
        } else {
            let _ = s.recover(json!({}), &[]);
            assert!(
                !read(&s.root, WIDGETS).contains("Retained nested prose"),
                "{point}: {out}"
            );
            assert_ne!(s.head(), head, "{point}: {out}");
            assert!(s.records().is_empty(), "{point}: {out}");
        }
    }
}

#[test]
fn list_archive_copy_crash_and_fault_matrix_preserves_source_and_one_copy() {
    for point in [
        "before_rename",
        "after_rename",
        "pending_written",
        "committed",
        "finalise",
        "rename",
        "dir_sync",
        "receipt_append",
        "receipt_sync",
        "record_retire",
    ] {
        let s = sole_store();
        let (_, item, _) = request(&s);
        let copy = archive_request(&s, &item);
        let before = read(&s.root, WIDGETS);
        if [
            "rename",
            "dir_sync",
            "receipt_append",
            "receipt_sync",
            "record_retire",
        ]
        .contains(&point)
        {
            let _ = s.run(
                "archive-copy",
                copy.clone(),
                &[("RIVET_TEST_FAULT", point.into())],
            );
        } else {
            s.kill_at(point, "archive-copy", &copy, &[]);
        }
        let _ = s.recover(json!({}), &[]);
        let (code, out) = s.run("archive-copy", copy.clone(), &[]);
        assert_eq!(code, 0, "{point}: {out}");
        assert_eq!(read(&s.root, WIDGETS), before);
        let archive = read(&s.root, ARCHIVE);
        let doc = Document::parse(&archive).unwrap();
        assert_eq!(doc.ids[&item].len(), 1, "{point}: {archive}");
        let head = s.head();
        let (code, out) = s.run("archive-copy", copy, &[]);
        assert_eq!(code, 0, "{point}: {out}");
        assert_eq!(out["status"], "unchanged", "{out}");
        assert_eq!(read(&s.root, ARCHIVE), archive);
        assert_eq!(s.head(), head);
    }
}

#[test]
fn list_delete_recovery_repins_receipt_and_failed_message_publication_keeps_candidate() {
    for fail in [false, true] {
        let s = sole_store();
        let (request, _, _) = request(&s);
        let before = read(&s.root, WIDGETS);
        s.kill_at("after_rename", "delete", &request, &[]);
        let candidate = read(&s.root, WIDGETS);
        std::fs::write(s.root.join("notes.txt"), "unrelated commit\n").unwrap();
        s.git(&["add", "notes.txt"]);
        s.git(&["commit", "-qm", "synthetic unrelated head"]);
        let parent = s.head();
        let faults = if fail {
            vec![("RIVET_TEST_FAULT", "record_publish".into())]
        } else {
            vec![]
        };
        let out = s.recover(json!({}), &faults);
        if fail {
            assert_eq!(one_result(&out)["result"], "repair_required", "{out}");
            assert_eq!(s.head(), parent);
            assert_eq!(s.records().len(), 1);
            assert_eq!(read(&s.root, WIDGETS), candidate, "{out}");
            let out = s.recover(json!({}), &[]);
            assert_eq!(one_result(&out)["result"], "committed", "{out}");
        }
        let events = s.receipts();
        let receipt = &events.last().unwrap()["delete_receipt"];
        assert_eq!(receipt["version"], 2, "{out}");
        assert_eq!(
            receipt["parent"],
            parent.trim(),
            "actual recovery parent: {out}"
        );
        let (code, verified) = s.run(
            "verify-delete",
            json!({"parent":before,"candidate":candidate,"receipt":receipt}),
            &[],
        );
        assert_eq!(code, 0, "{verified}");
    }
}

#[test]
fn list_delete_late_parse_failure_names_the_unreadable_scope_path() {
    for point in ["planned", "pending_written"] {
        let s = sole_store();
        let (request, _, _) = request(&s);
        let before = read(&s.root, WIDGETS);
        let head = s.head();
        let index = std::fs::read(s.root.join(".git/index")).unwrap();
        let pause = Pause::new(&s.root, point);
        let child = pause.wait_for(s.spawn("delete", &request, &[pause.env()]));
        std::fs::write(s.root.join(HISTORY), "# Earlier\rUnsupported\n").unwrap();
        pause.release();
        let (code, out) = finish(child);
        assert_eq!(code, 2, "{out}");
        assert_eq!(out["error"]["code"], "list_citation_refused", "{out}");
        assert_eq!(out["error"]["path"], HISTORY, "{out}");
        assert!(
            out["error"]["message"].as_str().unwrap().contains(HISTORY),
            "{out}"
        );
        assert!(
            out["error"]["message"]
                .as_str()
                .unwrap()
                .contains("unsupported_syntax"),
            "{out}"
        );
        assert_eq!(read(&s.root, WIDGETS), before);
        assert_eq!(s.head(), head);
        assert_eq!(std::fs::read(s.root.join(".git/index")).unwrap(), index);
    }
}

#[test]
fn list_delete_shared_item_and_list_alias_still_checks_the_list_citation() {
    for phase in ["initial", "publication", "recovery"] {
        let s = sole_store();
        let source = "# Synthetic\n\n## Action\n\n- [x] Sole item. <!-- stay:Selected01 subhash=sha256:000000000000 --> <!-- stay:Shared01 subhash=sha256:000000000000 -->\n<!-- stay:Shared01 -->\n\n## Log\n\nSuffix.\n";
        std::fs::write(s.root.join(WIDGETS), source).unwrap();
        s.git(&["add", WIDGETS]);
        s.git(&["commit", "-qm", "synthetic shared item and list alias"]);
        let (request, item, list) = request(&s);
        assert_eq!(item, "Selected01");
        assert_eq!(list, "Shared01");
        let head = s.head();
        let index = std::fs::read(s.root.join(".git/index")).unwrap();
        let cite =
            || std::fs::write(s.root.join(HISTORY), "# Earlier\n\nSee list:Shared01.\n").unwrap();
        if phase == "recovery" {
            s.kill_at("after_rename", "delete", &request, &[]);
            let candidate = read(&s.root, WIDGETS);
            let receipt = s.record()["delete_receipt"].clone();
            cite();
            let out = s.recover(json!({}), &[]);
            assert_eq!(one_result(&out)["result"], "repair_required", "{out}");
            assert_eq!(s.record()["delete_receipt"], receipt);
            assert_eq!(read(&s.root, WIDGETS), candidate);
        } else {
            let (code, out) = if phase == "publication" {
                let pause = Pause::new(&s.root, "pending_written");
                let child = pause.wait_for(s.spawn("delete", &request, &[pause.env()]));
                cite();
                pause.release();
                finish(child)
            } else {
                cite();
                s.run("delete", request, &[])
            };
            assert_eq!(code, 2, "{out}");
            assert_eq!(out["error"]["code"], "list_citation_refused", "{out}");
            assert_eq!(out["error"]["path"], HISTORY, "{out}");
            assert_eq!(read(&s.root, WIDGETS), source);
        }
        assert_eq!(s.head(), head);
        assert_eq!(std::fs::read(s.root.join(".git/index")).unwrap(), index);
    }
}
