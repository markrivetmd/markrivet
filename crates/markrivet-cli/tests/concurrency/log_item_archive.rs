//! Log-bullet item sources (a section binding's `items` key) under real
//! processes: a racing writer under the lock, publication faults, and
//! recovery between copy and delete. Fixtures are synthetic.
use super::*;
use markrivet_core::{Document, Selector, document::Target};

const LOG: &str = "tracking/TRACKING_LOG.md";
const HISTORY: &str = "archive/2026-10/history/LOG_HISTORY_2026-10.md";

/// The tracker example plus an `items`-enabled history binding and one stamped
/// bullet Log, committed in per-write/refuse mode.
fn log_store(name: &str, log: &str) -> Store {
    let s = Store::new(name, "refuse");
    let path = s.root.join(".rivet/project.json");
    let mut config: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    config["section_archives"] = json!({"history":{"files":["tracking/*.md"],
        "archive":["archive/*/history/*_HISTORY_*.md"],"items":{"headings":[["Log"]]}}});
    std::fs::write(&path, config.to_string()).unwrap();
    std::fs::create_dir_all(s.root.join("archive/2026-10/history")).unwrap();
    std::fs::write(s.root.join(LOG), log).unwrap();
    std::fs::write(s.root.join(HISTORY), "# Log history\n\n## Log\n").unwrap();
    s.configure(json!({"commit":"off"}));
    for p in [LOG, HISTORY] {
        let (_, map) = s.run("map", json!({"path":p}), &[]);
        let (code, out) = s.run(
            "stamp",
            json!({"path":p,"guard":map["revision"],"select":{"kind":"document"}}),
            &[],
        );
        assert_eq!(code, 0, "{out}");
    }
    s.configure(json!({"commit":"per-write","on_dirty":"refuse"}));
    s.git(&["add", "-A"]);
    s.git(&["commit", "-qm", "synthetic bullet log"]);
    s
}

const TWO: &str = "# Synthetic\n\n## Log\n\n- **2026-10-02: A** One.\n\n- **2026-10-01: B** Two.\n";
const THREE: &str = "# Synthetic\n\n## Log\n\n- **2026-10-03: A** One.\n\n- **2026-10-02: B** Two.\n\n- **2026-10-01: C** Three.\n";

fn item(s: &Store, needle: &str) -> String {
    let doc = Document::parse(&read(&s.root, LOG)).unwrap();
    doc.lists
        .iter()
        .flat_map(|l| &l.items)
        .find(|it| it.span.slice(&doc.source).unwrap().contains(needle))
        .map(|it| doc.stay_ids(&it.markers)[0].clone())
        .unwrap()
}

fn log_heading(s: &Store) -> String {
    let doc = Document::parse(&read(&s.root, LOG)).unwrap();
    let i = doc
        .sections
        .iter()
        .position(|sec| sec.path == ["Synthetic", "Log"])
        .unwrap();
    doc.section_ids(i)[0].clone()
}

fn copy_request(s: &Store, id: &str) -> Value {
    let select = json!({"kind":"heading_path","path":["Log"]});
    let (_, dest) = s.run("get", json!({"path":HISTORY,"select":select}), &[]);
    json!({"source":LOG,"id":id,"guard":item_guard(&s.root,LOG,id),"parent":log_heading(s),
        "path":HISTORY,"select":select,"destination_guard":dest["revision"]})
}

fn delete_request(s: &Store, id: &str, copy: &Value) -> Value {
    json!({"path":LOG,"select":{"kind":"id","id":id},"guard":item_guard(&s.root,LOG,id),
        "require_commit":true,"archive":{"path":HISTORY,"copy_op":copy["commit"]["copy_op"],
        "commit":copy["commit"]["sha"],"parent":log_heading(s)}})
}

fn plain_delete(s: &Store, id: &str) -> Value {
    json!({"path":LOG,"select":{"kind":"id","id":id},"guard":item_guard(&s.root,LOG,id)})
}

fn surfaces(s: &Store) -> (String, String, String, Vec<u8>, usize) {
    (
        read(&s.root, LOG),
        read(&s.root, HISTORY),
        s.head(),
        std::fs::read(s.root.join(".git/index")).unwrap(),
        s.records().len(),
    )
}

/// A writer paused after taking the lock changes the Log while the archive
/// command waits. Deleting a sibling leaves the item's guard valid and the
/// command proceeds; leaving the item sole refuses it with nothing written.
#[test]
fn a_waiting_copy_or_delete_rechecks_sole_item_under_the_writer_lock() {
    for command in ["archive-copy", "delete"] {
        for (log, sole) in [(THREE, false), (TWO, true)] {
            let s = log_store("log-item-lock-race", log);
            let target = item(&s, "2026-10-01");
            let racer = plain_delete(
                &s,
                &item(&s, if sole { "2026-10-02" } else { "2026-10-03" }),
            );
            let request = if command == "delete" {
                let (code, copy) = s.run("archive-copy", copy_request(&s, &target), &[]);
                assert_eq!(code, 0, "{copy}");
                delete_request(&s, &target, &copy)
            } else {
                copy_request(&s, &target)
            };
            let pause = Pause::new(&s.root, "before_rename");
            let published = Pause {
                dir: pause.dir.clone(),
                point: "after_rename",
            };
            let both = (
                "RIVET_TEST_PAUSE",
                format!("before_rename,after_rename:{}", pause.dir.display()),
            );
            let writer = s.spawn("delete", &racer, &[both]);
            pause.wait();
            let waiting = s.spawn(command, &request, &[]);
            wait_for_writer_lock(&s.root, &waiting);
            pause.release();
            published.wait();
            published.release();
            let (code, out) = finish(writer);
            assert_eq!(code, 0, "{out}");
            let raced = surfaces(&s);
            let (code, out) = finish(waiting);
            if sole {
                assert_eq!(code, 2, "{command}: {out}");
                let message = out.to_string();
                assert!(
                    message.contains("sole item") || message.contains("list"),
                    "{command}: {out}"
                );
                assert_eq!(surfaces(&s), raced, "{command}");
            } else {
                assert_eq!(code, 0, "{command}: {out}");
                assert_eq!(out["commit"]["status"], "committed", "{command}: {out}");
                let history = read(&s.root, HISTORY);
                assert!(history.contains(&format!("stay:{target} ")), "{command}");
                let live = read(&s.root, LOG).contains(&format!("stay:{target} "));
                assert_eq!(live, command == "archive-copy", "{command}");
            }
            assert!(s.records().is_empty(), "{command}");
        }
    }
}

/// A copy or delete killed after publication recovers into its commit, and a
/// rename fault on either changes nothing. Copy then delete still pair up.
#[test]
fn copy_and_delete_recover_after_publication_faults() {
    let s = log_store("log-item-faults", THREE);
    let target = item(&s, "2026-10-02");
    let original = read(&s.root, LOG);

    let before = surfaces(&s);
    let (code, out) = s.run(
        "archive-copy",
        copy_request(&s, &target),
        &[("RIVET_TEST_FAULT", "rename".into())],
    );
    assert_eq!(code, 2, "{out}");
    assert_eq!(surfaces(&s), before);

    let request = copy_request(&s, &target);
    s.kill_at("after_rename", "archive-copy", &request, &[]);
    assert_eq!(s.record()["archive_parent"], request["parent"]);
    let recovered = s.recover(json!({}), &[]);
    assert_eq!(one_result(&recovered)["result"], "committed", "{recovered}");
    assert!(s.records().is_empty());
    assert_eq!(read(&s.root, LOG), original);
    // The lost response is answered by a retry, with no second copy.
    let (code, retry) = s.run("archive-copy", request.clone(), &[]);
    assert_eq!(code, 0, "{retry}");
    assert_eq!(retry["status"], "unchanged", "{retry}");
    assert_eq!(
        read(&s.root, HISTORY)
            .matches(&format!("stay:{target} "))
            .count(),
        1
    );

    let delete = delete_request(&s, &target, &retry);
    let before = surfaces(&s);
    let (code, out) = s.run(
        "delete",
        delete.clone(),
        &[("RIVET_TEST_FAULT", "rename".into())],
    );
    assert_eq!(code, 2, "{out}");
    assert_eq!(surfaces(&s), before);

    s.kill_at("after_rename", "delete", &delete, &[]);
    let candidate = read(&s.root, LOG);
    assert!(!candidate.contains(&format!("stay:{target} ")));
    let recovered = s.recover(json!({}), &[]);
    assert_eq!(one_result(&recovered)["result"], "committed", "{recovered}");
    assert_eq!(read(&s.root, LOG), candidate);
    let op = s.git(&["log", "-1", "--format=%B"]);
    assert!(op.contains("Rivet-Delete"), "{op}");
    let receipt = s
        .receipts()
        .into_iter()
        .rev()
        .find(|e| e["delete_receipt"].is_object())
        .unwrap()["delete_receipt"]
        .clone();
    assert_eq!(receipt["version"], 1, "{receipt}");
    let (code, verified) = s.run(
        "verify-delete",
        json!({"parent":original,"candidate":candidate,"receipt":receipt}),
        &[],
    );
    assert_eq!(code, 0, "{verified}");
}

/// Recovery repeats the shared validation: a pending copy whose item became
/// sole, and a pending delete whose record lost or changed its parent, both
/// require repair and leave every byte, commit and record as they were.
#[test]
fn recovery_refuses_a_sole_copy_and_a_delete_without_its_parent() {
    let s = log_store("log-item-recover-sole", TWO);
    let target = item(&s, "2026-10-01");
    let other = item(&s, "2026-10-02");
    s.kill_at(
        "after_rename",
        "archive-copy",
        &copy_request(&s, &target),
        &[],
    );
    // The sibling leaves by hand while the copy is pending.
    let doc = Document::parse(&read(&s.root, LOG)).unwrap();
    let Target::Item { list, item: i } = doc.resolve(&Selector::Id { id: other }).unwrap() else {
        panic!()
    };
    let span = doc.lists[list].items[i].span;
    let edited = format!(
        "{}{}",
        &doc.source[..span.start],
        doc.source[span.end..].trim_start_matches('\n')
    );
    std::fs::write(s.root.join(LOG), &edited).unwrap();
    s.git(&["commit", "-qm", "synthetic: target is now sole", "--", LOG]);
    let before = surfaces(&s);
    let recovered = s.recover(json!({}), &[]);
    assert_eq!(
        one_result(&recovered)["result"],
        "repair_required",
        "{recovered}"
    );
    assert!(
        s.record()["repair"]["detail"]
            .as_str()
            .unwrap()
            .contains("sole item"),
        "{}",
        s.record()
    );
    assert_eq!(surfaces(&s), before);

    for mutation in ["missing-parent", "wrong-parent"] {
        let s = log_store("log-item-recover-parent", THREE);
        let target = item(&s, "2026-10-02");
        let (code, copy) = s.run("archive-copy", copy_request(&s, &target), &[]);
        assert_eq!(code, 0, "{copy}");
        s.kill_at(
            "after_rename",
            "delete",
            &delete_request(&s, &target, &copy),
            &[],
        );
        let mut record = s.record();
        let path = s.root.join(format!(
            ".rivet/pending/{}.json",
            record["op"].as_str().unwrap()
        ));
        let archive = &mut record["archive"];
        assert!(archive.is_object());
        if mutation == "missing-parent" {
            archive.as_object_mut().unwrap().remove("parent");
        } else {
            archive["parent"] = json!(item(&s, "2026-10-03"));
        }
        std::fs::write(path, record.to_string()).unwrap();
        let before = surfaces(&s);
        let recovered = s.recover(json!({}), &[]);
        assert_eq!(
            one_result(&recovered)["result"],
            "repair_required",
            "{mutation}: {recovered}"
        );
        let detail = s.record()["repair"]["detail"].as_str().unwrap().to_string();
        assert!(
            detail.contains("parent precondition"),
            "{mutation}: {detail}"
        );
        assert_eq!(surfaces(&s), before, "{mutation}");
    }
}

/// A pending heading-item copy whose recorded parent is lost or changed
/// requires repair on recovery, with nothing published further.
#[test]
fn copy_recovery_rechecks_the_recorded_parent() {
    for mutation in ["missing-parent", "wrong-parent"] {
        let s = log_store("log-item-recover-copy-parent", THREE);
        let target = item(&s, "2026-10-02");
        let request = copy_request(&s, &target);
        s.kill_at("after_rename", "archive-copy", &request, &[]);
        let mut record = s.record();
        assert_eq!(record["archive_parent"], request["parent"]);
        let path = s.root.join(format!(
            ".rivet/pending/{}.json",
            record["op"].as_str().unwrap()
        ));
        if mutation == "missing-parent" {
            record.as_object_mut().unwrap().remove("archive_parent");
        } else {
            record["archive_parent"] = json!(item(&s, "2026-10-03"));
        }
        std::fs::write(path, record.to_string()).unwrap();
        let before = surfaces(&s);
        let recovered = s.recover(json!({}), &[]);
        assert_eq!(
            one_result(&recovered)["result"],
            "repair_required",
            "{mutation}: {recovered}"
        );
        let detail = s.record()["repair"]["detail"].as_str().unwrap().to_string();
        assert!(
            detail.contains("parent precondition"),
            "{mutation}: {detail}"
        );
        assert_eq!(surfaces(&s), before, "{mutation}");
    }
}
