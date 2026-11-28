//! Real processes, deterministic ordering through the debug-build pause
//! hooks. Each case states which writer holds the project lock when.
#[path = "concurrency/bound_body.rs"]
mod bound_body;
mod common;
#[path = "common/move_fixture.rs"]
mod move_fixture;
#[path = "concurrency/move_item.rs"]
mod move_item;
#[path = "concurrency/move_section.rs"]
mod move_section;
#[path = "concurrency/pair.rs"]
mod pair;
#[path = "concurrency/position.rs"]
mod position;
use common::*;
use serde_json::{Value, json};
use std::path::Path;

#[path = "concurrency/list_delete.rs"]
mod list_delete;

#[path = "concurrency/guarded_adopt.rs"]
mod guarded_adopt;

#[path = "concurrency/refresh.rs"]
mod refresh;

fn set_item(id: &str, guard: &str, content: &str) -> Value {
    json!({"path": WIDGETS, "select": {"kind": "id", "id": id}, "guard": guard, "content": content})
}

#[test]
fn citation_walk_releases_the_lock_and_keeps_the_landed_candidate() {
    for mode in ["off", "per-write", "not-durable"] {
        let s = Store::new(&format!("citation-lock-{mode}"), "refuse");
        if mode != "per-write" {
            s.configure(json!({"commit": "off"}));
        }
        let ids = s.items();
        let (code, out) = s.set(
            &ids[1],
            &format!("[ ] **Refresh.** See item:{}.", ids[0]),
            &[],
        );
        assert_eq!(code, 0, "{out}");
        let before = read(&s.root, WIDGETS);
        let keep_guard = item_guard(&s.root, WIDGETS, &ids[1]);
        let (_, keep) = rivet(
            &s.root,
            "get",
            json!({"path": WIDGETS, "select": {"kind": "id", "id": ids[1]}}),
        );
        let (_, got) = rivet(
            &s.root,
            "get",
            json!({"path": WIDGETS, "select": {"kind": "id", "id": ids[0]}}),
        );
        let request = json!({"path": WIDGETS, "select": {"kind": "id", "id": ids[0]},
                             "guard": got["revision"]});
        let (code, refused) = s.run(
            "delete",
            request.clone(),
            &[("RIVET_TEST_FAULT", "rename".into())],
        );
        assert_eq!(code, 2, "{refused}");
        assert!(refused.get("dangling_citations").is_none());
        assert_eq!(read(&s.root, WIDGETS), before);
        assert_eq!(item_guard(&s.root, WIDGETS, &ids[1]), keep_guard);

        let pause = Pause::new(&s.root, "dangling_citations");
        let mut vars = vec![pause.env()];
        if mode == "not-durable" {
            vars.push(("RIVET_TEST_FAULT", "dir_sync".into()));
        }
        let first = s.spawn("delete", &request, &vars);
        pause.wait();
        let candidate = read(&s.root, WIDGETS);
        // Deletion also refreshes the list's aggregate evidence. Bytes
        // outside that list and the surviving item's own bytes stay exact.
        assert_eq!(
            candidate.split_once("## Action").unwrap().0,
            before.split_once("## Action").unwrap().0
        );
        assert_eq!(
            candidate.split_once("## Notes").unwrap().1,
            before.split_once("## Notes").unwrap().1
        );
        assert!(candidate.contains(keep["text"].as_str().unwrap()));
        assert_eq!(item_guard(&s.root, WIDGETS, &ids[1]), keep_guard);
        let line = candidate
            .lines()
            .position(|l| l.contains("See item:"))
            .unwrap()
            + 1;

        // A second process must publish while the first walks citations. It
        // removes the mention, so rereading the target would lose the warning.
        let mut second = s.set_request(&ids[1], "[x] **Refresh.** Mention removed.");
        second["lock_timeout_ms"] = json!(500);
        let (code, out) = s.run("set", second, &[]);
        pause.release();
        let (first_code, first_out) = finish(first);
        assert_eq!(code, 0, "{mode}: {out}");
        assert_eq!(
            first_code,
            if mode == "not-durable" { 3 } else { 0 },
            "{first_out}"
        );
        assert_eq!(first_out["status"], "applied", "{first_out}");
        assert_eq!(
            first_out["dangling_citations"],
            json!([{"id": ids[0], "path": WIDGETS, "line": line}]),
            "{mode}: the response describes the first writer's candidate"
        );
        if mode == "per-write" {
            assert_eq!(first_out["commit"]["status"], "committed", "{first_out}");
            assert_eq!(out["commit"]["status"], "committed", "{out}");
        }
        assert!(read(&s.root, WIDGETS).contains("Mention removed."));
        assert!(!read(&s.root, WIDGETS).contains("See item:"));
        assert_eq!(s.items(), ids[1..]);
    }
}

#[test]
fn publication_reread_refuses_a_swapped_special_file_without_touching_saved_bytes() {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    for kind in ["symlink", "fifo"] {
        let (root, open) = stamped(&format!("reread-special-{kind}"));
        let before = read(&root, WIDGETS);
        let guards: Vec<_> = open
            .iter()
            .map(|a| item_guard(&root, WIDGETS, &a.1))
            .collect();
        let pause = Pause::new(&root, "before_rename");
        let mut writer = spawn(
            &root,
            "set",
            &set_item(&open[0].1, &guards[0], "[x] Must not publish."),
            &[pause.env()],
        );
        pause.wait();
        let path = root.join(WIDGETS);
        let saved = path.with_extension("saved");
        std::fs::rename(&path, &saved).unwrap();
        if kind == "symlink" {
            std::os::unix::fs::symlink(&saved, &path).unwrap();
        } else {
            let name = CString::new(path.as_os_str().as_bytes()).unwrap();
            // SAFETY: a terminated path in this test fixture.
            assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        }
        pause.release();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while writer.try_wait().unwrap().is_none() {
            if std::time::Instant::now() > deadline {
                writer.kill().unwrap();
                writer.wait().unwrap();
                panic!("publication reread blocked on {kind}");
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let (code, out) = finish(writer);
        assert_eq!(code, 2, "{kind}: {out}");
        assert_eq!(
            out["error"]["code"],
            if kind == "symlink" {
                "io"
            } else {
                "path_rejected"
            },
            "{out}"
        );
        assert_eq!(std::fs::read(&saved).unwrap(), before.as_bytes());
        assert!(temp_files(&root).is_empty());
        std::fs::remove_file(&path).unwrap();
        std::fs::rename(saved, path).unwrap();
        for (a, guard) in open.iter().zip(guards) {
            assert_eq!(item_guard(&root, WIDGETS, &a.1), guard);
        }
    }
}

fn stamped(name: &str) -> (std::path::PathBuf, Vec<(String, String, String)>) {
    let root = project(name);
    stamp(&root, WIDGETS);
    let open: Vec<_> = open_actions(&root)
        .into_iter()
        .filter(|a| !a.1.is_empty())
        .collect();
    assert_eq!(open.len(), 2);
    (root, open)
}

fn temp_files(root: &Path) -> Vec<String> {
    std::fs::read_dir(root.join("tracking"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
        .filter(|n| n.contains(".rivet-"))
        .collect()
}

#[test]
fn disjoint_writers_both_survive_a_stale_snapshot() {
    let (root, open) = stamped("disjoint");
    let (a, b) = (&open[0], &open[1]);
    // Both guards come from the same snapshot, before either write.
    let ga = item_guard(&root, WIDGETS, &a.1);
    let gb = item_guard(&root, WIDGETS, &b.1);
    let pause = Pause::new(&root, "planned");
    let first = spawn(
        &root,
        "set",
        &set_item(
            &a.1,
            &ga,
            "[x] **Ship the widget. (2026-09-01)** Shipped by writer A.",
        ),
        &[pause.env()],
    );
    pause.wait();
    // A holds the lock with its candidate built; B queues behind it.
    let second = spawn(
        &root,
        "set",
        &set_item(
            &b.1,
            &gb,
            "[x] `agent` **Refresh the fixtures. (2026-09-03)** Done by writer B.",
        ),
        &[],
    );
    std::thread::sleep(std::time::Duration::from_millis(100));
    pause.release();
    let (ca, ra) = finish(first);
    let (cb, rb) = finish(second);
    assert_eq!((ca, cb), (0, 0), "{ra}\n{rb}");
    let text = read(&root, WIDGETS);
    assert!(text.contains("Shipped by writer A."));
    assert!(
        text.contains("Done by writer B."),
        "B re-read under the lock and kept A's change"
    );
    assert!(temp_files(&root).is_empty());
}

#[test]
fn same_item_stale_writer_is_rejected() {
    let (root, open) = stamped("same");
    let a = &open[0];
    let guard = item_guard(&root, WIDGETS, &a.1);
    let pause = Pause::new(&root, "planned");
    let first = spawn(
        &root,
        "set",
        &set_item(&a.1, &guard, "[x] **Ship the widget.** A wins."),
        &[pause.env()],
    );
    pause.wait();
    let second = spawn(
        &root,
        "set",
        &set_item(&a.1, &guard, "[x] **Ship the widget.** B loses."),
        &[],
    );
    pause.release();
    let (ca, _) = finish(first);
    let (cb, rb) = finish(second);
    assert_eq!(ca, 0);
    assert_eq!(cb, 2);
    assert_eq!(rb["error"]["code"], "guard_conflict");
    let text = read(&root, WIDGETS);
    assert!(text.contains("A wins.") && !text.contains("B loses."));
}

#[test]
fn ancestor_guard_sees_a_descendant_edit_but_siblings_do_not_conflict() {
    let (root, open) = stamped("ancestor");
    let (_, map) = rivet(&root, "map", json!({"path": WIDGETS}));
    let section = |title: &str| {
        map["outline"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["title"] == title)
            .unwrap()
            .clone()
    };
    let action = section("Action");
    let log = section("Log");
    let guard = item_guard(&root, WIDGETS, &open[0].1);
    let (code, _) = rivet(
        &root,
        "set",
        set_item(&open[0].1, &guard, "[x] **Ship the widget.** Done."),
    );
    assert_eq!(code, 0);
    let (code, out) = rivet(
        &root,
        "set",
        json!({"path": WIDGETS, "select": {"kind": "id", "id": action["id"]}, "part": "title",
               "guard": action["revision"], "content": "Actions"}),
    );
    assert_eq!(code, 2);
    assert_eq!(out["error"]["code"], "guard_conflict");
    let (code, out) = rivet(
        &root,
        "set",
        json!({"path": WIDGETS, "select": {"kind": "id", "id": log["id"]}, "part": "title",
               "guard": log["revision"], "content": "History"}),
    );
    assert_eq!(code, 0, "{out}");
}

#[test]
fn lock_timeout_refuses_without_writing() {
    let (root, open) = stamped("timeout");
    let guard = item_guard(&root, WIDGETS, &open[0].1);
    let pause = Pause::new(&root, "locked");
    let holder = spawn(
        &root,
        "set",
        &set_item(&open[0].1, &guard, "[x] held"),
        &[pause.env()],
    );
    pause.wait();
    let before = read(&root, WIDGETS);
    let mut request = set_item(
        &open[1].1,
        &item_guard(&root, WIDGETS, &open[1].1),
        "[x] waited",
    );
    request["lock_timeout_ms"] = json!(150);
    let (code, out) = rivet(&root, "set", request);
    assert_eq!(code, 2);
    assert_eq!(out["error"]["code"], "lock_timeout");
    assert_eq!(read(&root, WIDGETS), before);
    pause.release();
    assert_eq!(finish(holder).0, 0);
}

#[test]
fn a_crashed_lock_holder_releases_the_lock() {
    let (root, open) = stamped("crash-lock");
    let before = read(&root, WIDGETS);
    let guard = item_guard(&root, WIDGETS, &open[0].1);
    let pause = Pause::new(&root, "locked");
    let mut holder = spawn(
        &root,
        "set",
        &set_item(&open[0].1, &guard, "[x] never"),
        &[pause.env()],
    );
    pause.wait();
    holder.kill().unwrap();
    holder.wait().unwrap();
    assert_eq!(read(&root, WIDGETS), before);
    let (code, out) = rivet(
        &root,
        "set",
        set_item(
            &open[0].1,
            &guard,
            "[x] **Ship the widget.** After the crash.",
        ),
    );
    assert_eq!(code, 0, "{out}");
}

#[test]
fn a_crash_before_rename_leaves_content_and_a_crash_after_applies() {
    let (root, open) = stamped("crash-rename");
    let before = read(&root, WIDGETS);
    let guard = item_guard(&root, WIDGETS, &open[0].1);
    let pause = Pause::new(&root, "before_rename");
    let mut writer = spawn(
        &root,
        "set",
        &set_item(&open[0].1, &guard, "[x] **Ship.** Lost."),
        &[pause.env()],
    );
    pause.wait();
    writer.kill().unwrap();
    writer.wait().unwrap();
    assert_eq!(
        read(&root, WIDGETS),
        before,
        "an unrenamed temp never becomes the file"
    );
    assert_eq!(
        temp_files(&root).len(),
        1,
        "a killed writer can leave its temp file"
    );
    let pause = Pause::new(&root, "after_rename");
    let mut writer = spawn(
        &root,
        "set",
        &set_item(&open[0].1, &guard, "[x] **Ship.** Kept."),
        &[pause.env()],
    );
    pause.wait();
    writer.kill().unwrap();
    writer.wait().unwrap();
    assert!(
        read(&root, WIDGETS).contains("Kept."),
        "the rename is the commit point"
    );
}

#[test]
fn injected_rename_failure_changes_nothing() {
    let (root, open) = stamped("fault-rename");
    let before = read(&root, WIDGETS);
    let guard = item_guard(&root, WIDGETS, &open[0].1);
    let (code, out) = finish(spawn(
        &root,
        "set",
        &set_item(&open[0].1, &guard, "[x] no"),
        &[("RIVET_TEST_FAULT", "rename".into())],
    ));
    assert_eq!(code, 2);
    assert_eq!(out["error"]["code"], "io");
    assert_eq!(read(&root, WIDGETS), before);
    assert!(temp_files(&root).is_empty());
}

#[test]
fn injected_directory_sync_failure_reports_applied_but_not_durable() {
    let (root, open) = stamped("fault-sync");
    let guard = item_guard(&root, WIDGETS, &open[0].1);
    let (code, out) = finish(spawn(
        &root,
        "set",
        &set_item(&open[0].1, &guard, "[x] **Ship.** Visible."),
        &[("RIVET_TEST_FAULT", "dir_sync".into())],
    ));
    assert_eq!(code, 3, "possibly-not-durable is never an ordinary success");
    assert_eq!(out["status"], "applied");
    assert_eq!(out["durable"], false);
    assert!(read(&root, WIDGETS).contains("Visible."));
}

#[test]
fn an_outside_edit_during_planning_is_detected_best_effort() {
    let (root, open) = stamped("external");
    let guard = item_guard(&root, WIDGETS, &open[0].1);
    let pause = Pause::new(&root, "planned");
    let writer = spawn(
        &root,
        "set",
        &set_item(&open[0].1, &guard, "[x] **Ship.** Mine."),
        &[pause.env()],
    );
    pause.wait();
    // An editor that ignores the lock saves while Rivet holds it.
    let edited =
        read(&root, WIDGETS).replace("Newest first, one entry per event.", "Edited in an editor.");
    std::fs::write(root.join(WIDGETS), &edited).unwrap();
    pause.release();
    let (code, out) = finish(writer);
    assert_eq!(code, 2);
    assert_eq!(out["error"]["code"], "external_change");
    assert_eq!(
        read(&root, WIDGETS),
        edited,
        "the outside edit is preserved"
    );
    // A guarded write after the outside edit keeps it too.
    let (code, _) = rivet(
        &root,
        "set",
        set_item(&open[0].1, &guard, "[x] **Ship.** Mine."),
    );
    assert_eq!(code, 0);
    assert!(read(&root, WIDGETS).contains("Edited in an editor."));
}

#[test]
fn many_disjoint_writers_all_land() {
    let (root, open) = stamped("many");
    // Items 1 and 2 plus the two log headings: four disjoint targets.
    let (_, map) = rivet(&root, "map", json!({"path": WIDGETS}));
    let logs: Vec<Value> = map["outline"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["level"] == 3)
        .cloned()
        .collect();
    let mut children = vec![];
    for (i, item) in open.iter().enumerate() {
        let guard = item_guard(&root, WIDGETS, &item.1);
        children.push(spawn(
            &root,
            "set",
            &set_item(&item.1, &guard, &format!("[x] **Item {i}.** Parallel.")),
            &[],
        ));
    }
    for (i, log) in logs.iter().enumerate() {
        children.push(spawn(
            &root,
            "set",
            &json!({"path": WIDGETS, "select": {"kind": "id", "id": log["id"]}, "part": "title",
                    "guard": log["revision"], "content": format!("Entry {i}")}),
            &[],
        ));
    }
    for child in children {
        let (code, out) = finish(child);
        assert_eq!(code, 0, "{out}");
    }
    let text = read(&root, WIDGETS);
    for needle in [
        "**Item 0.** Parallel.",
        "**Item 1.** Parallel.",
        "### Entry 0",
        "### Entry 1",
    ] {
        assert!(text.contains(needle), "{needle} missing");
    }
}

#[test]
fn a_swapped_project_root_cannot_redirect_the_write() {
    let (root, open) = stamped("root-swap");
    let guard = item_guard(&root, WIDGETS, &open[0].1);
    let original = read(&root, WIDGETS);
    let pause = Pause::new(&root, "planned");
    let writer = spawn(
        &root,
        "set",
        &set_item(&open[0].1, &guard, "[x] **Ship the widget.** Written."),
        &[pause.env()],
    );
    pause.wait();
    // Move the whole project away and point its old path at a decoy with an
    // identical target file.
    let moved = root.with_extension("moved-root");
    let decoy = root.with_extension("decoy-root");
    std::fs::rename(&root, &moved).unwrap();
    std::fs::create_dir_all(decoy.join("tracking")).unwrap();
    std::fs::create_dir_all(decoy.join(".rivet")).unwrap();
    std::fs::write(decoy.join(WIDGETS), &original).unwrap();
    std::os::unix::fs::symlink(&decoy, &root).unwrap();
    pause.release();
    let (code, out) = finish(writer);
    assert_eq!(code, 0, "{out}");
    assert_eq!(
        std::fs::read_to_string(decoy.join(WIDGETS)).unwrap(),
        original
    );
    assert!(
        std::fs::read_to_string(moved.join(WIDGETS))
            .unwrap()
            .contains("Written.")
    );
}

#[test]
fn analytics_competing_exports_preserve_published_capture() {
    let root = project("analytics-competing-exports");
    let before: Vec<_> = std::fs::read_dir(root.join("tracking"))
        .unwrap()
        .map(|entry| {
            let path = entry.unwrap().path();
            let bytes = std::fs::read(&path).unwrap();
            (path, bytes)
        })
        .collect();
    let request = json!({"bundle":"shared","query":{"collection":"actions"}});
    let pause = Pause::new(&root, "analytics_read");
    let first = spawn(&root, "export", &request, &[pause.env()]);
    pause.wait();
    // The second process publishes while the first holds its observed source bytes.
    let (code, published) = rivet(&root, "export", request);
    assert_eq!(code, 0, "{published}");
    let manifest = read(&root, ".rivet/exports/shared/manifest.json");
    let records = read(&root, ".rivet/exports/shared/records.json");
    pause.release();
    let (code, refused) = finish(first);
    assert_eq!(code, 2, "{refused}");
    assert_eq!(read(&root, ".rivet/exports/shared/manifest.json"), manifest);
    assert_eq!(read(&root, ".rivet/exports/shared/records.json"), records);
    let (code, replay) = rivet(&root, "replay", json!({"bundle":"shared"}));
    assert_eq!(code, 0, "{replay}");
    assert_eq!(replay, published["report"]);
    for (path, bytes) in before {
        assert_eq!(std::fs::read(path).unwrap(), bytes);
    }
}

#[test]
fn init_waits_for_the_project_lock() {
    let (root, open) = stamped("init-lock");
    let a = &open[0];
    let guard = item_guard(&root, WIDGETS, &a.1);
    std::fs::write(root.join(".rivet/.gitignore"), "/lock\n").unwrap();
    let pause = Pause::new(&root, "planned");
    let writer = spawn(
        &root,
        "set",
        &set_item(
            &a.1,
            &guard,
            "[x] **Ship the widget. (2026-09-01)** Shipped.",
        ),
        &[pause.env()],
    );
    pause.wait();
    let mut init = raw_command()
        .args(["--root", root.to_str().unwrap(), "init"])
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(300));
    assert!(
        init.try_wait().unwrap().is_none(),
        "init ran while a writer held the lock"
    );
    assert_eq!(read(&root, ".rivet/.gitignore"), "/lock\n");
    pause.release();
    assert_eq!(finish(writer).0, 0);
    assert!(init.wait().unwrap().success());
    assert!(read(&root, ".rivet/.gitignore").contains("/pending/"));
}

// ---------------------------------------------------------------------------
// Per-write commits, in real repositories. Every Rivet process gets HOME and
// XDG_CONFIG_HOME pointing at an empty directory, so no user git
// configuration reaches it; faults in git itself come from a wrapper script
// named by `RIVET_TEST_GIT`.

const DOCS: &str = "tracking/TRACKING_DOCS.md";

fn section_replace(root: &Path, heading: &str, old: &str, new: &str) -> Value {
    let select = json!({"kind": "heading_path", "path": [heading]});
    let (code, out) = rivet(root, "get", json!({"path": WIDGETS, "select": select}));
    assert_eq!(code, 0, "{out}");
    json!({"path": WIDGETS, "select": select, "guard": out["revision"], "old": old, "new": new,
        "message": "replace-in-section: state", "session": "test:replace"})
}

fn committed_writer(child: std::process::Child) {
    let (code, out) = finish(child);
    assert_eq!(code, 0, "{out}");
    assert_eq!(
        out["commit"]["status"],
        "committed",
        "the first writer must finish each Git child within {:?}; \
         a failed/deadline commit may be recovered by the queued writer: {out}",
        markrivet_core::git::DEFAULT_DEADLINE
    );
}

fn stale_writer(child: std::process::Child) -> Value {
    let (code, out) = finish(child);
    assert_eq!(code, 2, "{out}");
    assert_eq!(
        out["error"]["code"],
        "guard_conflict",
        "the stale writer must acquire the lock within {:?} to check its guard; \
         lock_timeout is the engine's bounded refusal while the first writer still holds it: {out}",
        markrivet_core::fs::DEFAULT_LOCK_TIMEOUT
    );
    out
}

const REPLACE_SOURCE: &str = "# Example\n\n## State\n<!-- stay:state -->\n\n| Key | Value |\n| --- | --- |\n| First | waiting |\n| Second | ready |\n<!-- stay:table hash=sha256:000000000000 -->\n\n### Child\n\nchild text\n\n## Plans\n\nsibling text\n";

#[test]
fn replace_in_section_rereads_under_lock_and_checks_parent_before_search() {
    for heading in ["State", "Child"] {
        let s = Store::new(&format!("replace-locked-{heading}"), "skip");
        std::fs::write(s.root.join(WIDGETS), REPLACE_SOURCE).unwrap();
        s.git(&["commit", "-qam", "table fixture"]);
        let stale = section_replace(&s.root, "State", "waiting", "waiting");
        let sibling = section_replace(&s.root, "Plans", "sibling text", "sibling survived");
        let (old, new) = if heading == "State" {
            ("waiting", "landed")
        } else {
            ("child text", "child landed")
        };
        let first = section_replace(&s.root, heading, old, new);
        let pause = Pause::new(&s.root, "planned");
        let writer = s.spawn("replace-in-section", &first, &[pause.env()]);
        pause.wait();
        let queued = s.spawn("replace-in-section", &stale, &[]);
        pause.release();
        committed_writer(writer);
        let before = read(&s.root, WIDGETS);
        let head = s.head();
        let out = stale_writer(queued);
        let current = section_replace(&s.root, "State", "unused", "unused");
        assert_eq!(out["error"]["current"], current["guard"]);
        assert_eq!(read(&s.root, WIDGETS), before);
        assert_eq!(s.head(), head);
        let (code, out) = s.run("replace-in-section", sibling, &[]);
        assert_eq!(code, 0, "{out}");
        assert_eq!(out["commit"]["status"], "committed", "{out}");
        assert!(read(&s.root, WIDGETS).contains(new));
        assert!(read(&s.root, WIDGETS).contains("sibling survived"));
    }
}

#[test]
fn replace_in_section_top_level_refuses_nested_text_and_lands_prose_over_the_cli() {
    let s = Store::new("replace-top-level", "skip");
    for (body, want) in [
        ("- [ ] first waiting", Some("unsupported_syntax")),
        ("> quoted waiting", Some("unsupported_syntax")),
        (
            "- [ ] first\n  | h | v |\n  |---|---|\nwaiting",
            Some("unsupported_syntax"),
        ),
        ("Prose waiting.", None),
    ] {
        let source = format!("# Example\n\n## State\n\n{body}\n");
        std::fs::write(s.root.join(WIDGETS), &source).unwrap();
        s.git(&["add", WIDGETS]);
        s.git(&["commit", "--allow-empty", "-qm", "top-level fixture"]);
        let mut request = section_replace(&s.root, "State", "waiting", "landed");
        request["top_level"] = json!(true);
        let head = s.head();
        let (code, out) = s.run("replace-in-section", request, &[]);
        match want {
            Some(want) => {
                assert_eq!(code, 2, "{body:?}: {out}");
                assert_eq!(out["error"]["code"], want, "{body:?}: {out}");
                assert_eq!(read(&s.root, WIDGETS), source);
                assert_eq!(s.head(), head);
            }
            None => {
                assert_eq!(code, 0, "{body:?}: {out}");
                assert_eq!(read(&s.root, WIDGETS), source.replace("waiting", "landed"));
            }
        }
    }
}

#[test]
fn replace_in_section_refusals_and_noop_leave_bytes_and_commits_identical() {
    let s = Store::new("replace-refused", "skip");
    for (body, old, new, want) in [
        ("text", "missing", "new", "match_not_found"),
        ("aaa", "aa", "new", "ambiguous_match"),
        ("text text", "text", "new", "ambiguous_match"),
        ("text", "", "new", "invalid_request"),
        ("text", "text\n", "new", "invalid_request"),
        ("text", "text", "new\nline", "invalid_request"),
        ("- item", "- ", "new", "unsupported_syntax"),
        ("```\ncode\n```", "code", "new", "unsupported_syntax"),
        (
            "text\n<!-- stay:owned -->",
            "owned",
            "new",
            "invalid_content",
        ),
        (
            "text <!-- stay:owned -->",
            "text <!-- stay:",
            "new",
            "invalid_content",
        ),
        (
            "text <!-- stay:row subhash=sha256:1 -->",
            "text",
            "new",
            "unsupported_syntax",
        ),
        ("text", "text", "# Heading", "invalid_content"),
        ("text", "text", "- Item", "invalid_content"),
        ("first\nmiddle\nlast", "middle", "", "invalid_content"),
        ("text", "text", "<!-- stay:injected -->", "invalid_content"),
        (
            "text `quote` <!-- stay:owned --> `<!-- stay:owned -->`",
            "`quote`",
            "`quote",
            "invalid_content",
        ),
        (
            "[ref]: /url\nprose\n\n[foo][ref](<!-- stay:owned -->) [bar][other](<!-- stay:owned -->)",
            "[ref]:",
            "[other]:",
            "invalid_content",
        ),
        (
            "[ref]: /url\nprose\n\n[foo][ref](<!-- stay:row subhash=sha256:1 -->) [bar][other](<!-- stay:row subhash=sha256:1 -->)",
            "[ref]:",
            "[other]:",
            "invalid_content",
        ),
        (
            "text\n\n### Child\n\nchild only",
            "child only",
            "new",
            "match_not_found",
        ),
    ] {
        let source = format!("# Example\n\n## State\n\n{body}\n");
        std::fs::write(s.root.join(WIDGETS), &source).unwrap();
        s.git(&["add", WIDGETS]);
        s.git(&["commit", "--allow-empty", "-qm", "refusal fixture"]);
        let request = section_replace(&s.root, "State", old, new);
        let head = s.head();
        let (code, out) = s.run("replace-in-section", request, &[]);
        assert_eq!(code, 2, "{body:?}: {out}");
        assert_eq!(out["error"]["code"], want, "{body:?}: {out}");
        assert_eq!(read(&s.root, WIDGETS), source);
        assert_eq!(s.head(), head);
        assert!(s.records().is_empty());
    }
    let request = section_replace(&s.root, "State", "text", "text");
    let before = read(&s.root, WIDGETS);
    let head = s.head();
    let (code, out) = s.run("replace-in-section", request.clone(), &[]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["status"], "unchanged");
    assert!(out.get("commit").is_none());
    assert_eq!(read(&s.root, WIDGETS), before);
    assert_eq!(s.head(), head);
    let (_, map) = s.run("map", json!({"path": WIDGETS}), &[]);
    let mut wrong = request;
    wrong["select"] = json!({"kind": "document"});
    wrong["guard"] = map["revision"].clone();
    let (code, out) = s.run("replace-in-section", wrong, &[]);
    assert_eq!(code, 2, "{out}");
    assert_eq!(out["error"]["code"], "invalid_request");
    assert_eq!(read(&s.root, WIDGETS), before);
    assert_eq!(s.head(), head);
}

#[test]
fn replace_in_section_faults_preserve_preimage_and_dirty_skip_is_reported() {
    let s = Store::new("replace-fault", "skip");
    std::fs::write(s.root.join(WIDGETS), REPLACE_SOURCE).unwrap();
    s.git(&["commit", "-qam", "table fixture"]);
    let request = section_replace(&s.root, "State", "waiting", "landed");
    let head = s.head();
    let (code, out) = s.run(
        "replace-in-section",
        request,
        &[("RIVET_TEST_FAULT", "rename".into())],
    );
    assert_eq!(code, 2, "{out}");
    assert_eq!(read(&s.root, WIDGETS), REPLACE_SOURCE);
    assert_eq!(s.head(), head);
    assert!(temp_files(&s.root).is_empty());
    // Exercise the JSON batch form too: it follows the section and uses the
    // same commit policy while preserving the unrelated dirty bytes.
    let source = REPLACE_SOURCE.to_string() + "\nhand edit\n";
    std::fs::write(s.root.join(WIDGETS), &source).unwrap();
    let mut op = section_replace(&s.root, "State", "waiting", "landed");
    op.as_object_mut().unwrap().remove("path");
    op.as_object_mut().unwrap().remove("message");
    op.as_object_mut().unwrap().remove("session");
    op["op"] = json!("replace_in_section");
    let (code, out) = s.run("write", json!({"path": WIDGETS, "ops": [op]}), &[]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["commit"]["status"], "skipped");
    assert_eq!(out["commit"]["reason"], "dirty");
    assert!(
        out["ops"][0]["revision"]
            .as_str()
            .unwrap()
            .starts_with("rivet:v1:section:")
    );
    assert!(read(&s.root, WIDGETS).ends_with("\nhand edit\n"));
    assert_eq!(s.head(), head);
}

#[test]
fn nested_replace_and_paragraph_removal_reread_under_lock() {
    for remove in [false, true] {
        let s = Store::new(&format!("paragraph-lock-{remove}"), "skip");
        let source = "# Example\n\n## State\n<!-- stay:state -->\n\n- waiting\n- other\n\nLast scanned: before\ncontinued residue\n\nkept\n<!-- stay:kept -->\n\n## Plans\n\nsibling text\n";
        std::fs::write(s.root.join(WIDGETS), source).unwrap();
        s.git(&["commit", "-qam", "paragraph fixture"]);
        let sibling = section_replace(&s.root, "Plans", "sibling text", "sibling survived");
        let mut removal = section_replace(
            &s.root,
            "State",
            "Last scanned: before\ncontinued residue",
            "",
        );
        removal.as_object_mut().unwrap().remove("new");
        let replacement = section_replace(&s.root, "State", "waiting", "landed");
        let (command, first, queued_command, queued) = if remove {
            (
                "remove-paragraph-in-section",
                removal,
                "replace-in-section",
                replacement,
            )
        } else {
            (
                "replace-in-section",
                replacement,
                "remove-paragraph-in-section",
                removal,
            )
        };
        let pause = Pause::new(&s.root, "planned");
        let writer = s.spawn(command, &first, &[pause.env()]);
        pause.wait();
        let second = s.spawn(queued_command, &queued, &[]);
        pause.release();
        committed_writer(writer);
        let after = read(&s.root, WIDGETS);
        let expected = if remove {
            source.replace("Last scanned: before\ncontinued residue", "")
        } else {
            source.replace("waiting", "landed")
        };
        assert_eq!(after, expected);
        let head = s.head();
        stale_writer(second);
        assert_eq!(read(&s.root, WIDGETS), after);
        assert_eq!(s.head(), head);
        let (code, out) = s.run("replace-in-section", sibling, &[]);
        assert_eq!(code, 0, "{out}");
        assert_eq!(
            read(&s.root, WIDGETS),
            expected.replace("sibling text", "sibling survived")
        );
    }
}

#[test]
fn paragraph_operations_refusals_and_faults_leave_file_and_head_identical() {
    let s = Store::new("paragraph-refusals", "skip");
    for (body, old, new, remove, want) in [
        (
            "- waiting",
            "waiting",
            "# Heading",
            false,
            "invalid_content",
        ),
        (
            "- waiting\n- waiting",
            "waiting",
            "new",
            false,
            "ambiguous_match",
        ),
        (
            "> waiting <!-- stay:row subhash=sha256:1 -->",
            "waiting",
            "new",
            false,
            "unsupported_syntax",
        ),
        (
            "> first\n> continuation",
            "> continuation",
            "continuation",
            false,
            "unsupported_syntax",
        ),
        (
            "- first\n  continuation",
            "  continuation",
            "continuation",
            false,
            "unsupported_syntax",
        ),
        (
            "> first\n> continuation",
            "> continuation",
            "> continuation",
            false,
            "unsupported_syntax",
        ),
        ("whole text", "text", "", true, "invalid_content"),
        (
            "text\n<!-- stay:owned -->",
            "text",
            "",
            true,
            "invalid_content",
        ),
        ("text\n\ntext", "text", "", true, "ambiguous_match"),
        ("- text", "text", "", true, "unsupported_syntax"),
        ("text", "text", "", false, "invalid_content"),
        ("text", "missing", "", true, "match_not_found"),
    ] {
        let source = format!("# Example\n\n## State\n<!-- stay:state -->\n\n{body}\n");
        std::fs::write(s.root.join(WIDGETS), &source).unwrap();
        s.git(&["commit", "--allow-empty", "-qam", "refusal fixture"]);
        let mut request = section_replace(&s.root, "State", old, new);
        let command = if remove {
            request.as_object_mut().unwrap().remove("new");
            "remove-paragraph-in-section"
        } else {
            "replace-in-section"
        };
        let head = s.head();
        let (code, out) = s.run(command, request, &[]);
        assert_eq!(code, 2, "{out}");
        assert_eq!(out["error"]["code"], want, "{out}");
        assert_eq!(read(&s.root, WIDGETS), source);
        assert_eq!(s.head(), head);
    }
    for remove in [false, true] {
        let source = "# Example\n\n## State\n\n- waiting\n\nremove this\n";
        std::fs::write(s.root.join(WIDGETS), source).unwrap();
        s.git(&["commit", "-qam", "fault fixture"]);
        let mut request = section_replace(
            &s.root,
            "State",
            if remove { "remove this" } else { "waiting" },
            "landed",
        );
        let command = if remove {
            request.as_object_mut().unwrap().remove("new");
            "remove-paragraph-in-section"
        } else {
            "replace-in-section"
        };
        let head = s.head();
        let pause = Pause::new(&s.root, "planned");
        let writer = s.spawn(
            command,
            &request,
            &[pause.env(), ("RIVET_TEST_FAULT", "rename".into())],
        );
        pause.wait();
        let retry_pause = Pause::new(&s.root, "locked");
        let stale = s.spawn(command, &request, &[retry_pause.env()]);
        pause.release();
        let (code, out) = finish(writer);
        retry_pause.wait();
        assert_eq!(code, 2, "{out}");
        assert_eq!(read(&s.root, WIDGETS), source);
        assert_eq!(s.head(), head);
        assert!(temp_files(&s.root).is_empty());
        // The second process rereads the unchanged preimage after the failed publish.
        retry_pause.release();
        let (code, out) = finish(stale);
        assert_eq!(code, 0, "{out}");
        let expected = if remove {
            source.replace("remove this", "")
        } else {
            source.replace("waiting", "landed")
        };
        assert_eq!(read(&s.root, WIDGETS), expected);
    }
}

#[test]
fn paragraph_removal_batch_preview_and_dirty_skip_use_the_section_revision() {
    let s = Store::new("paragraph-batch", "skip");
    let source = "# Example\n\n## State\n<!-- stay:state -->\n\nNothing queued.\n\nkept\n<!-- stay:kept -->\n\n## Plans\n\nsibling\n";
    std::fs::write(s.root.join(WIDGETS), source).unwrap();
    s.git(&["commit", "-qam", "paragraph fixture"]);
    let mut op = section_replace(&s.root, "State", "Nothing queued.", "");
    for field in ["path", "message", "session", "new"] {
        op.as_object_mut().unwrap().remove(field);
    }
    op["op"] = json!("remove_paragraph_in_section");
    let head = s.head();
    let request = json!({"path": WIDGETS, "ops": [op], "preview": true});
    let (code, preview) = s.run("write", request.clone(), &[]);
    assert_eq!(code, 0, "{preview}");
    assert_eq!(read(&s.root, WIDGETS), source);
    assert_eq!(s.head(), head);
    let mut bad = section_replace(&s.root, "State", "Nothing queued.", "");
    bad["allow_drop"] = json!(true);
    bad.as_object_mut().unwrap().remove("new");
    let (code, out) = s.run("remove-paragraph-in-section", bad, &[]);
    assert_eq!(code, 1, "{out}");
    assert_eq!(read(&s.root, WIDGETS), source);
    assert_eq!(s.head(), head);
    let dirty = source.to_string() + "\nunrelated hand edit\n";
    std::fs::write(s.root.join(WIDGETS), &dirty).unwrap();
    let mut request = request;
    request["preview"] = json!(false);
    let (code, out) = s.run("write", request, &[]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["commit"]["status"], "skipped", "{out}");
    assert_eq!(out["commit"]["reason"], "dirty");
    assert_eq!(s.head(), head);
    assert_eq!(read(&s.root, WIDGETS), dirty.replace("Nothing queued.", ""));
    let (_, got) = s.run(
        "get",
        json!({"path": WIDGETS, "select": {"kind": "heading_path", "path": ["State"]}}),
        &[],
    );
    assert_eq!(out["ops"][0]["revision"], got["revision"]);
}

type Var = (&'static str, String);

struct Store {
    root: std::path::PathBuf,
    home: std::path::PathBuf,
}

impl Store {
    /// The tracker example, stamped, with commit mode on and committed on
    /// `main`.
    fn new(name: &str, on_dirty: &str) -> Store {
        let root = project(name);
        let home = root.with_extension("home");
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        let s = Store { root, home };
        s.git(&["init", "-q", "-b", "main"]);
        s.git(&["config", "user.name", "Tester"]);
        s.git(&["config", "user.email", "tester@example.invalid"]);
        stamp(&s.root, WIDGETS);
        stamp(&s.root, DOCS);
        std::fs::write(s.root.join("notes.txt"), "notes\n").unwrap();
        s.configure(json!({"commit": "per-write", "on_dirty": on_dirty}));
        s.git(&["add", "-A"]);
        s.git(&["commit", "-qm", "init"]);
        s
    }

    fn configure(&self, git: Value) {
        let path = self.root.join(".rivet/project.json");
        let mut config: Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        config["git"] = git;
        std::fs::write(&path, serde_json::to_string_pretty(&config).unwrap()).unwrap();
    }

    fn git_try(&self, args: &[&str]) -> (bool, String) {
        let out = git_command(&self.root, &self.home, args).output().unwrap();
        let text = String::from_utf8_lossy(&out.stdout).into_owned()
            + &String::from_utf8_lossy(&out.stderr);
        (out.status.success(), text)
    }

    fn git(&self, args: &[&str]) -> String {
        let (ok, text) = self.git_try(args);
        assert!(ok, "git {args:?}: {text}");
        text
    }

    /// `git` with extra environment, such as a scratch `GIT_INDEX_FILE`.
    fn git_env(&self, args: &[&str], env: &[Var]) -> String {
        let out = git_command(&self.root, &self.home, args)
            .envs(env.iter().map(|(k, v)| (k, v)))
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {out:?}");
        String::from_utf8(out.stdout).unwrap()
    }

    /// `git` reading `input` on stdin.
    fn git_stdin(&self, args: &[&str], input: &str) -> String {
        use std::io::Write;
        let mut child = git_command(&self.root, &self.home, args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success(), "git {args:?}: {out:?}");
        String::from_utf8(out.stdout).unwrap()
    }

    fn vars(&self, extra: &[Var]) -> Vec<Var> {
        let mut vars = vec![
            ("HOME", self.home.display().to_string()),
            ("XDG_CONFIG_HOME", self.home.display().to_string()),
        ];
        vars.extend_from_slice(extra);
        vars
    }

    fn spawn(&self, cmd: &str, request: &Value, extra: &[Var]) -> std::process::Child {
        spawn(&self.root, cmd, request, &self.vars(extra))
    }

    fn run(&self, cmd: &str, request: Value, extra: &[Var]) -> (i32, Value) {
        finish(self.spawn(cmd, &request, extra))
    }

    /// The three action items' ids, in document order.
    fn items(&self) -> Vec<String> {
        let (code, out) = rivet(
            &self.root,
            "query",
            json!({"collection": "actions", "paths": [WIDGETS]}),
        );
        assert_eq!(code, 0, "{out}");
        out["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["id"].as_str().unwrap().to_string())
            .collect()
    }

    fn set_request(&self, id: &str, content: &str) -> Value {
        let guard = item_guard(&self.root, WIDGETS, id);
        set_item(id, &guard, content)
    }

    fn set(&self, id: &str, content: &str, extra: &[Var]) -> (i32, Value) {
        let request = self.set_request(id, content);
        self.run("set", request, extra)
    }

    /// A write to the docs tracker's first item.
    fn set_docs(&self, content: &str) -> (i32, Value) {
        let (_, out) = rivet(
            &self.root,
            "query",
            json!({"collection": "actions", "paths": [DOCS]}),
        );
        let id = out["items"][0]["id"].as_str().unwrap().to_string();
        let guard = item_guard(&self.root, DOCS, &id);
        self.run(
            "set",
            json!({"path": DOCS, "select": {"kind": "id", "id": id}, "guard": guard, "content": content}),
            &[],
        )
    }

    /// Whole receipt lines, in file order.
    fn receipts(&self) -> Vec<Value> {
        let text =
            std::fs::read_to_string(self.root.join(".rivet/commits.jsonl")).unwrap_or_default();
        let whole = &text[..text.rfind('\n').map_or(0, |i| i + 1)];
        whole
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    fn events(&self, op: &str) -> Vec<Value> {
        self.receipts()
            .into_iter()
            .filter(|e| e["op"] == op)
            .collect()
    }

    fn records(&self) -> Vec<Value> {
        let Ok(dir) = std::fs::read_dir(self.root.join(".rivet/pending")) else {
            return vec![];
        };
        let mut found: Vec<Value> = dir
            .map(|e| e.unwrap().path())
            .filter(|p| p.extension().is_some_and(|x| x == "json"))
            .map(|p| serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap())
            .collect();
        found.sort_by_key(|r| r["seq"].as_u64());
        found
    }

    fn record(&self) -> Value {
        let records = self.records();
        assert_eq!(records.len(), 1, "{records:?}");
        records[0].clone()
    }

    fn recover(&self, request: Value, extra: &[Var]) -> Value {
        let (code, out) = self.run("commit-pending", request, extra);
        assert_eq!(code, 0, "{out}");
        out
    }

    fn head(&self) -> String {
        self.git(&["rev-parse", "HEAD"]).trim().to_string()
    }

    /// Commits after `base` on first-parent history, oldest first.
    fn since(&self, base: &str) -> Vec<String> {
        let range = format!("{base}..HEAD");
        self.git(&["rev-list", "--first-parent", "--reverse", &range])
            .lines()
            .map(str::to_string)
            .collect()
    }

    fn message(&self, sha: &str) -> String {
        self.git(&["log", "-1", "--format=%B", sha])
    }

    fn changed(&self, sha: &str) -> Vec<String> {
        self.git(&["diff-tree", "-r", "--no-commit-id", "--name-only", sha])
            .lines()
            .map(str::to_string)
            .collect()
    }

    /// Commits whose message carries `Rivet-Op: <op>`.
    fn carrying(&self, op: &str) -> Vec<String> {
        let grep = format!("--grep=Rivet-Op: {op}");
        self.git(&["log", "--format=%H", "--fixed-strings", &grep])
            .lines()
            .map(str::to_string)
            .collect()
    }

    /// Tracked trackers match HEAD and the index. A killed writer's
    /// temporary file is untracked and does not count.
    fn clean(&self) -> bool {
        self.git(&[
            "status",
            "--porcelain",
            "--untracked-files=no",
            "--",
            "tracking",
        ])
        .is_empty()
    }

    /// A git wrapper script under the home directory, for `RIVET_TEST_GIT`.
    /// `$HOME_DIR` in the body names that directory.
    fn script(&self, name: &str, body: &str) -> Var {
        let path = self.home.join(name);
        let body = body.replace("$HOME_DIR", &self.home.display().to_string());
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .unwrap();
        ("RIVET_TEST_GIT", path.display().to_string())
    }

    /// A wrapper that logs every call to `<home>/<name>.log` and runs git.
    fn logging(&self, name: &str) -> Var {
        self.script(
            name,
            &format!("echo \"$*\" >> $HOME_DIR/{name}.log\nexec git \"$@\""),
        )
    }

    fn logged(&self, name: &str) -> String {
        std::fs::read_to_string(self.home.join(format!("{name}.log"))).unwrap_or_default()
    }

    /// Simulate a legacy child taking the named locks before publication.
    fn holding(&self, locks: &str) -> Var {
        self.script("holding", &format!(
            "case \"$*\" in *\" read-tree \"*)\n  touch {locks}\n  echo $$ > $HOME_DIR/git.pid\n  while :; do sleep 0.05; done;;\nesac\nexec git \"$@\""))
    }

    /// Publish the ref but fail before the caller can install its index.
    fn half_commit(&self) -> Var {
        self.script(
            "half",
            r#"case "$*" in *" update-ref --stdin "*)
  git "$@"
  exit 1;;
esac
exec git "$@""#,
        )
    }

    /// Corrupt the object returned by commit-tree, or stage a third blob
    /// after ref publication, so verification must refuse the transaction.
    fn forged(&self, variant: &str) -> Var {
        self.script(&format!("forged-{variant}"), &format!(
            r#"variant={variant}
case "$*" in
*" commit-tree "*)
  prev=""; for a in "$@"; do [ "$prev" = "-F" ] && msg="$a"; prev="$a"; done
  path=tracking/TRACKING_WIDGETS.md
  other=$(printf 'other\n' | git hash-object -w --stdin)
  GIT_INDEX_FILE=.git/forged-index git read-tree HEAD
  blob=$(git hash-object -w -- "$path")
  [ "$variant" = blob ] && blob=$other
  GIT_INDEX_FILE=.git/forged-index git update-index --cacheinfo "100644,$blob,$path"
  [ "$variant" = extra ] && GIT_INDEX_FILE=.git/forged-index git update-index --cacheinfo "100644,$other,notes.txt"
  tree=$(GIT_INDEX_FILE=.git/forged-index git write-tree)
  rm -f .git/forged-index
  if [ "$variant" = twice ]; then cat "$msg" "$msg" > .git/forged-msg; msg=.git/forged-msg; fi
  if [ "$variant" = orphan ]; then
    git commit-tree "$tree" -F "$msg"
  else
    git commit-tree "$tree" -p HEAD -F "$msg"
  fi
  exit $?;;
*" update-ref --stdin "*)
  git "$@"; rc=$?
  if [ "$variant" = index ]; then
    other=$(printf 'other\n' | git hash-object -w --stdin)
    GIT_INDEX_FILE=.git/foreign-index git read-tree HEAD
    GIT_INDEX_FILE=.git/foreign-index git update-index --cacheinfo "100644,$other,tracking/TRACKING_WIDGETS.md"
    cp .git/foreign-index .git/index
  fi
  exit $rc;;
esac
exec git "$@""#))
    }

    /// Kill a writer at a pause point and wait for its recorded git helper to exit.
    fn kill_at(&self, point: &'static str, cmd: &str, request: &Value, extra: &[Var]) {
        let pause = Pause::new(&self.root, point);
        let mut vars = extra.to_vec();
        vars.push(pause.env());
        let mut child = pause.wait_for(self.spawn(cmd, request, &vars));
        child.kill().unwrap();
        child.wait().unwrap();
        // Reaping the writer does not establish that its helper has handled
        // the parent-death signal yet. Recovery must still refuse a live helper.
        for record in self.records() {
            if let Some(pid) = record["pid"].as_u64() {
                let helper = markrivet_core::git::GitChild {
                    pid: Some(pid.try_into().unwrap()),
                    spawned: std::time::UNIX_EPOCH,
                    identity: serde_json::from_value(record["child_identity"].clone()).unwrap(),
                };
                assert!(gone(helper), "{point}: git outlived Rivet: {record:#?}");
            }
        }
    }

    /// Kill a writer once its git child (from [`Store::holding`]) runs, and
    /// check that child died with it.
    fn kill_in_git(&self, request: &Value, wrapper: Var) -> u32 {
        let pid_file = self.home.join("git.pid");
        let _ = std::fs::remove_file(&pid_file);
        let mut child = self.spawn("set", request, &[wrapper]);
        let pid: u32 = wait_text(&pid_file).trim().parse().unwrap();
        let helper = markrivet_core::git::GitChild {
            pid: Some(pid),
            spawned: std::time::UNIX_EPOCH,
            identity: markrivet_core::git::ProcessIdentity::read(pid),
        };
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(gone(helper), "git outlived Rivet");
        pid
    }

    /// The identity a recovery report gives `file`.
    fn identity(&self, out: &Value, file: &str) -> String {
        out["records"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|r| r["locks"].as_array().cloned().unwrap_or_default())
            .find(|l| l["file"] == file)
            .unwrap_or_else(|| panic!("{file} not reported: {out}"))["identity"]
            .as_str()
            .unwrap()
            .to_string()
    }
}

fn wait_text(path: &Path) -> String {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        if let Ok(text) = std::fs::read_to_string(path)
            && text.ends_with('\n')
        {
            return text;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "{} never appeared",
            path.display()
        );
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}

/// Exited, disappeared, or no longer the recorded process.
fn gone(child: markrivet_core::git::GitChild) -> bool {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if child.dead() {
            return true;
        }
        if std::time::Instant::now() > deadline {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

fn one_result(out: &Value) -> &Value {
    let records = out["records"].as_array().unwrap();
    assert_eq!(records.len(), 1, "{out}");
    &records[0]
}

#[test]
fn commit_n_writers_on_different_items_give_n_commits() {
    let s = Store::new("commit-n", "skip");
    let base = s.head();
    let ids = s.items();
    let originals: Vec<String> = ids
        .iter()
        .map(|id| {
            let (_, out) = rivet(
                &s.root,
                "get",
                json!({"path": WIDGETS, "select": {"kind": "id", "id": id}}),
            );
            out["text"].as_str().unwrap().to_string()
        })
        .collect();
    let writers: Vec<_> = ids
        .iter()
        .enumerate()
        .map(|(i, id)| {
            let request = s.set_request(id, &format!("[x] **Item {i}.** Parallel {i}."));
            s.spawn("set", &request, &[])
        })
        .collect();
    for w in writers {
        let (code, out) = finish(w);
        assert_eq!(code, 0, "{out}");
        assert_eq!(out["commit"]["status"], "committed", "{out}");
    }
    let commits = s.since(&base);
    assert_eq!(commits.len(), ids.len());
    let marker = |line: &str| line.trim_start().starts_with("<!-- stay:");
    for sha in &commits {
        assert_eq!(s.changed(sha), [WIDGETS]);
        let diff = s.git(&["diff", "-U0", &format!("{sha}^"), sha]);
        let added: Vec<&str> = diff
            .lines()
            .filter(|l| l.starts_with('+') && !l.starts_with("+++"))
            .map(|l| &l[1..])
            .collect();
        let removed: Vec<&str> = diff
            .lines()
            .filter(|l| l.starts_with('-') && !l.starts_with("---"))
            .map(|l| &l[1..])
            .collect();
        let i = (0..ids.len())
            .find(|i| added.iter().any(|l| l.contains(&format!("Parallel {i}."))))
            .expect("a writer's own content");
        for line in added.iter().filter(|l| !marker(l)) {
            assert!(
                line.contains(&format!("Parallel {i}.")),
                "{sha} added {line:?}"
            );
        }
        for line in removed.iter().filter(|l| !marker(l)) {
            assert!(
                originals[i].contains(line),
                "{sha} removed {line:?} outside item {i}"
            );
        }
    }
    assert!(s.clean());
    assert!(s.records().is_empty());
    assert_eq!(s.receipts().len(), ids.len());
}

#[test]
fn commit_a_lock_holding_committer_interleaves_with_rivet_writers() {
    let s = Store::new("commit-interleave", "skip");
    let base = s.head();
    let ids = s.items();
    let root = s.root.clone();
    let home = s.home.clone();
    // A save.sh-style committer: takes the Rivet lock, edits and commits its
    // own file with --only, releases.
    let committer = std::thread::spawn(move || {
        use std::os::fd::AsRawFd;
        for n in 0..4 {
            let lock = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(root.join(".rivet/lock"))
                .unwrap();
            // SAFETY: a blocking flock on a descriptor this thread owns.
            assert_eq!(unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) }, 0);
            std::fs::write(root.join("notes.txt"), format!("notes {n}\n")).unwrap();
            let ok = git_command(
                &root,
                &home,
                &[
                    "commit",
                    "-q",
                    "--only",
                    "-m",
                    &format!("notes {n}"),
                    "--",
                    "notes.txt",
                ],
            )
            .env("GIT_LITERAL_PATHSPECS", "1")
            .status()
            .unwrap()
            .success();
            assert!(ok);
            drop(lock);
            std::thread::sleep(std::time::Duration::from_millis(15));
        }
    });
    for (i, id) in ids.iter().enumerate() {
        let (code, out) = s.set(id, &format!("[x] **Item {i}.** Interleaved {i}."), &[]);
        assert_eq!(code, 0, "{out}");
        assert_eq!(out["commit"]["status"], "committed", "{out}");
    }
    committer.join().unwrap();
    let commits = s.since(&base);
    assert_eq!(commits.len(), ids.len() + 4);
    for sha in commits {
        let changed = s.changed(&sha);
        let ours = s.message(&sha).contains("Rivet-Op:");
        assert_eq!(changed, [if ours { WIDGETS } else { "notes.txt" }], "{sha}");
    }
    assert!(s.git(&["status", "--porcelain"]).is_empty());
}

#[test]
fn commit_a_failed_write_is_committed_by_recovery_on_a_new_head() {
    let s = Store::new("commit-failed", "skip");
    let ids = s.items();
    // git sleeps past a short deadline: HEAD does not move.
    let slow = s.script(
        "slow",
        "case \"$*\" in *\" update-ref --stdin \"*) sleep 30;; esac\nexec git \"$@\"",
    );
    let (code, out) = s.set(
        &ids[0],
        "[x] **Ship.** Late.",
        &[slow, ("RIVET_TEST_GIT_DEADLINE_MS", "300".into())],
    );
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["status"], "applied");
    assert_eq!(out["commit"]["status"], "failed", "{out}");
    assert_eq!(out["commit"]["reason"], "deadline");
    assert_eq!(out["commit"]["op"], s.receipts()[0]["op"]);
    assert_eq!(out["commit"]["published"], false);
    let op = out["commit"]["pending"].as_str().unwrap().to_string();
    assert_eq!(s.record()["state"], "applied");
    // Another writer commits a different file in the meantime.
    let (code, other) = s.set_docs("[x] **Licence.** Decided.");
    assert_eq!(code, 0, "{other}");
    assert_eq!(other["commit"]["status"], "committed");
    let docs = s.head();
    let out = s.recover(json!({}), &[]);
    assert_eq!(one_result(&out)["result"], "committed", "{out}");
    let sha = s.head();
    assert_eq!(s.git(&["rev-parse", &format!("{sha}^")]).trim(), docs);
    assert_eq!(s.changed(&sha), [WIDGETS]);
    assert_eq!(s.carrying(&op), std::slice::from_ref(&sha));
    assert!(s.clean() && s.records().is_empty());
    // The op's outcome is its latest event.
    let events = s.events(&op);
    assert_eq!(events.len(), 2, "{events:?}");
    assert_eq!(events[0]["outcome"], "failed");
    assert_eq!(events[1]["outcome"], "committed");
    assert_eq!(events[1]["recovered"], true);
    let (_, status) = s.run("status", json!({"paths": [WIDGETS]}), &[]);
    assert_eq!(status["paths"][0]["last_write"]["op"], op);
    assert_eq!(status["paths"][0]["last_write"]["outcome"], "committed");
}

#[test]
fn commit_a_crash_after_committing_is_found_by_its_trailer() {
    let s = Store::new("commit-trailer", "skip");
    let ids = s.items();
    let request = s.set_request(&ids[0], "[x] **Ship.** Committed, then killed.");
    s.kill_at("committed", "set", &request, &[]);
    let rec = s.record();
    let op = rec["op"].as_str().unwrap().to_string();
    assert_eq!(rec["state"], "committed");
    assert!(s.events(&op).is_empty());
    let (code, other) = s.set_docs("[x] **Licence.** Later.");
    assert_eq!(code, 0, "{other}");
    let out = s.recover(json!({}), &[]);
    assert_eq!(one_result(&out)["result"], "committed", "{out}");
    assert_eq!(s.carrying(&op).len(), 1, "no second commit");
    let events = s.events(&op);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["outcome"], "committed");
    assert!(s.records().is_empty());
}

#[test]
fn commit_rivet_killed_while_git_holds_the_index_or_branch_lock() {
    for (name, locks, file) in [
        ("commit-held-index", ".git/index.lock", ".git/index.lock"),
        (
            "commit-held-branch",
            ".git/refs/heads/main.lock",
            ".git/refs/heads/main.lock",
        ),
    ] {
        let s = Store::new(name, "skip");
        let ids = s.items();
        let request = s.set_request(&ids[0], "[x] **Ship.** Held.");
        let pid = s.kill_in_git(&request, s.holding(locks));
        let rec = s.record();
        assert_eq!(rec["pid"], pid);
        let base = s.head();
        let out = s.recover(json!({}), &[s.logging("quiet")]);
        let r = one_result(&out);
        assert_eq!(r["result"], "repair_required", "{out}");
        assert_eq!(r["reason"], "stale_git_lock");
        assert_eq!(r["locks"][0]["file"], file);
        assert_eq!(r["locks"][0]["record"], rec["op"]);
        assert!(s.logged("quiet").is_empty(), "git ran past a reported lock");
        assert!(s.root.join(file).exists(), "a reported lock is left");
        assert_eq!(s.records().len(), 1);
        assert_eq!(s.head(), base);
        // The person approves removing exactly the file reported.
        let identity = s.identity(&out, file);
        let out = s.recover(
            json!({"clear_lock": file, "identity": identity, "op": rec["op"]}),
            &[],
        );
        assert_eq!(out["cleared"], file, "{out}");
        assert_eq!(one_result(&out)["result"], "committed", "{out}");
        assert!(!s.root.join(file).exists());
        assert!(s.records().is_empty() && s.clean());
    }
}

#[test]
fn commit_next_index_goes_without_a_request_and_index_lock_stays() {
    let s = Store::new("commit-next-index", "skip");
    let ids = s.items();
    let request = s.set_request(&ids[0], "[x] **Ship.** Both locks.");
    // The legacy `commit --only` path's locks, as git 2.43 left them; recovery still clears them.
    let pid = s.kill_in_git(
        &request,
        s.holding(".git/index.lock .git/next-index-$$.lock"),
    );
    let next = format!(".git/next-index-{pid}.lock");
    assert!(s.root.join(&next).exists());
    let out = s.recover(json!({}), &[s.logging("quiet")]);
    let r = one_result(&out);
    assert_eq!(r["removed"], json!([next]), "{out}");
    assert!(!s.root.join(&next).exists());
    assert_eq!(r["reason"], "stale_git_lock");
    assert!(s.root.join(".git/index.lock").exists());
    assert!(s.logged("quiet").is_empty());
    let identity = s.identity(&out, ".git/index.lock");
    let out = s.recover(
        json!({"clear_lock": ".git/index.lock", "identity": identity, "op": r["op"]}),
        &[],
    );
    assert_eq!(one_result(&out)["result"], "committed", "{out}");
    assert!(s.clean());
}

fn unowned_lock_request(s: &Store, file: &str, age: u64) -> Value {
    let path = s.root.join(file);
    std::fs::write(&path, "").unwrap();
    std::fs::File::open(&path)
        .unwrap()
        .set_times(
            std::fs::FileTimes::new()
                .set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(age)),
        )
        .unwrap();
    let (code, status) = s.run("status", json!({}), &[]);
    assert_eq!(code, 0, "{status}");
    let lock = status["git"]["locks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|l| l["file"] == file)
        .unwrap();
    assert_eq!(lock["record"], Value::Null, "{lock}");
    json!({"clear_lock":file,"identity":lock["identity"],"size":lock["size"],"quiescent":true})
}

#[test]
fn commit_clear_unowned_lock_then_next_write_commits_without_changing_other_bytes() {
    for file in [
        ".git/index.lock",
        ".git/HEAD.lock",
        ".git/refs/heads/main.lock",
    ] {
        let s = Store::new(&format!("unowned-{}", file.replace('/', "-")), "refuse");
        let ids = s.items();
        let before = read(&s.root, WIDGETS);
        let other = read(&s.root, DOCS);
        let guards: Vec<_> = ids
            .iter()
            .map(|id| item_guard(&s.root, WIDGETS, id))
            .collect();
        let head = s.head();
        let request = unowned_lock_request(&s, file, 31);
        let (code, out) = s.run("commit-pending", request, &[]);
        assert_eq!(code, 0, "{out}");
        assert_eq!(out["cleared"], file);
        assert_eq!(out["records"], json!([]));
        assert_eq!(out["unresolved"], 0);
        assert_eq!(s.head(), head);
        assert_eq!(read(&s.root, WIDGETS), before);
        assert_eq!(read(&s.root, DOCS), other);
        assert!(!s.root.join(file).exists());
        let (code, out) = s.set(&ids[0], "[x] **Ship.** Cleared.", &[]);
        assert_eq!(code, 0, "{out}");
        assert_eq!(out["commit"]["status"], "committed", "{out}");
        assert_eq!(s.items(), ids);
        for (id, guard) in ids.iter().zip(&guards).skip(1) {
            assert_eq!(item_guard(&s.root, WIDGETS, id), *guard);
        }
        assert_eq!(read(&s.root, DOCS), other);
        assert!(s.clean());
    }
}

#[test]
fn commit_clear_unowned_lock_refusals_preserve_bytes() {
    let s = Store::new("unowned-refusals", "refuse");
    let file = ".git/index.lock";
    let path = s.root.join(file);
    let before = read(&s.root, WIDGETS);
    let request = unowned_lock_request(&s, file, 31);
    for (name, request, vars) in [
        (
            "identity",
            {
                let mut r = request.clone();
                r["identity"] = json!("0:0:0");
                r
            },
            vec![],
        ),
        (
            "size",
            {
                let mut r = request.clone();
                r["size"] = json!(1);
                r
            },
            vec![],
        ),
        (
            "visibility",
            request.clone(),
            vec![("RIVET_TEST_FAULT", "unowned_lock_processes".into())],
        ),
    ] {
        let (code, out) = s.run("commit-pending", request, &vars);
        assert_eq!(code, 2, "{name}: {out}");
        assert_eq!(out["error"]["code"], "clear_lock_refused", "{out}");
        assert_eq!(std::fs::read(&path).unwrap(), b"");
        assert_eq!(read(&s.root, WIDGETS), before);
    }
    let held = std::fs::File::open(&path).unwrap();
    let (code, out) = s.run("commit-pending", request.clone(), &[]);
    assert_eq!(code, 2, "{out}");
    assert_eq!(out["error"]["reason"], "lock_in_use", "{out}");
    assert_eq!(std::fs::read(&path).unwrap(), b"");
    drop(held);
    let mut missing_size = request.clone();
    missing_size.as_object_mut().unwrap().remove("size");
    let (code, out) = s.run("commit-pending", missing_size, &[]);
    assert_eq!(code, 2, "{out}");
    assert_eq!(out["error"]["code"], "invalid_request");
    let recent = unowned_lock_request(&s, file, 0);
    let (code, out) = s.run("commit-pending", recent, &[]);
    assert_eq!(code, 2, "{out}");
    assert_eq!(out["error"]["reason"], "lock_recent", "{out}");
    assert_eq!(std::fs::read(&path).unwrap(), b"");
    assert_eq!(read(&s.root, WIDGETS), before);
}

#[test]
fn commit_clear_unowned_lock_rereads_identity_and_size_under_project_lock() {
    for change in ["replace", "resize"] {
        let s = Store::new(&format!("unowned-race-{change}"), "refuse");
        let file = ".git/index.lock";
        let request = unowned_lock_request(&s, file, 31);
        let before = read(&s.root, WIDGETS);
        let pause = Pause::new(&s.root, "clear_unowned_lock");
        let writer = s.spawn("commit-pending", &request, &[pause.env()]);
        pause.wait();
        let path = s.root.join(file);
        let old_time = std::fs::metadata(&path).unwrap().modified().unwrap();
        if change == "replace" {
            std::fs::rename(&path, path.with_extension("saved")).unwrap();
        }
        let replacement = if change == "replace" {
            ""
        } else {
            "replacement bytes"
        };
        std::fs::write(&path, replacement).unwrap();
        // Keeping mtime exposes why size is independently rechecked.
        std::fs::File::open(&path)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(old_time))
            .unwrap();
        let (_, waiting) = s.run("commit-pending", json!({"lock_timeout_ms": 1}), &[]);
        assert_eq!(waiting["error"]["code"], "lock_timeout", "{waiting}");
        pause.release();
        let (code, out) = finish(writer);
        assert_eq!(code, 2, "{out}");
        assert_eq!(out["error"]["reason"], "identity_or_remove_failed", "{out}");
        assert_eq!(std::fs::read(&path).unwrap(), replacement.as_bytes());
        assert_eq!(read(&s.root, WIDGETS), before);
    }
}

#[test]
fn commit_clear_unowned_lock_refuses_a_live_git_with_closed_lock_fds() {
    let s = Store::new("unowned-live-git", "refuse");
    let request = unowned_lock_request(&s, ".git/index.lock", 31);
    let head = s.head();
    let before = read(&s.root, WIDGETS);
    let mut live = git_command(&s.root, &s.home, &["update-ref", "--stdin"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    {
        use std::io::{BufRead, Write};
        let stdin = live.stdin.as_mut().unwrap();
        writeln!(stdin, "start\nupdate refs/heads/other {head}\nprepare").unwrap();
        stdin.flush().unwrap();
        let mut out = std::io::BufReader::new(live.stdout.as_mut().unwrap());
        let mut line = String::new();
        out.read_line(&mut line).unwrap();
        out.read_line(&mut line).unwrap();
        assert!(line.contains("prepare"), "{line}");
    }
    let (code, out) = s.run("commit-pending", request, &[]);
    drop(live.stdin.take());
    live.wait().unwrap();
    assert_eq!(code, 2, "{out}");
    assert_eq!(out["error"]["code"], "clear_lock_refused");
    assert_eq!(out["error"]["reason"], "live_git_process", "{out}");
    assert_eq!(std::fs::read(s.root.join(".git/index.lock")).unwrap(), b"");
    assert_eq!(read(&s.root, WIDGETS), before);
}

#[test]
fn commit_clear_unowned_lock_requires_the_operator_assertion() {
    let s = Store::new("unowned-assertion", "refuse");
    let request = unowned_lock_request(&s, ".git/index.lock", 31);
    let before = read(&s.root, WIDGETS);
    for assertion in [None, Some(false)] {
        let mut r = request.clone();
        if let Some(value) = assertion {
            r["quiescent"] = json!(value);
        } else {
            r.as_object_mut().unwrap().remove("quiescent");
        }
        let (code, out) = s.run("commit-pending", r, &[]);
        assert_eq!(code, 2, "{out}");
        assert_eq!(out["error"]["code"], "invalid_request", "{out}");
        assert_eq!(std::fs::read(s.root.join(".git/index.lock")).unwrap(), b"");
        assert_eq!(read(&s.root, WIDGETS), before);
    }
}

#[test]
fn commit_clear_unowned_lock_rejects_unreadable_pending_entries_and_unsupported_locks() {
    for kind in [
        "malformed",
        "directory",
        "fifo",
        "symlink",
        "other-lock",
        "symlink-lock",
    ] {
        let s = Store::new(&format!("unowned-unsupported-{kind}"), "refuse");
        let mut request = unowned_lock_request(&s, ".git/index.lock", 31);
        let path = s.root.join(".rivet/pending/unknown.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        match kind {
            "malformed" => std::fs::write(&path, "{").unwrap(),
            "directory" => std::fs::create_dir(&path).unwrap(),
            "fifo" => {
                use std::os::unix::ffi::OsStrExt;
                let name = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
                assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
            }
            "symlink" => std::os::unix::fs::symlink(s.root.join("notes.txt"), &path).unwrap(),
            "other-lock" => request = unowned_lock_request(&s, ".git/config.lock", 31),
            "symlink-lock" => {
                std::fs::remove_file(s.root.join(".git/index.lock")).unwrap();
                std::os::unix::fs::symlink(
                    s.root.join("notes.txt"),
                    s.root.join(".git/index.lock"),
                )
                .unwrap();
            }
            _ => unreachable!(),
        }
        let before = read(&s.root, WIDGETS);
        let notes = read(&s.root, "notes.txt");
        let (code, out) = s.run("commit-pending", request, &[]);
        assert_eq!(code, 2, "{kind}: {out}");
        assert_eq!(out["error"]["code"], "clear_lock_refused", "{kind}: {out}");
        assert_eq!(
            out["error"]["reason"],
            if kind.ends_with("lock") {
                "unsupported_lock"
            } else {
                "pending_records"
            },
            "{kind}: {out}"
        );
        assert!(std::fs::symlink_metadata(s.root.join(".git/index.lock")).is_ok());
        assert_eq!(read(&s.root, WIDGETS), before);
        assert_eq!(read(&s.root, "notes.txt"), notes);
    }
}

#[test]
fn commit_clear_unowned_lock_leaves_an_unrelated_pending_record_recoverable() {
    let s = Store::new("unowned-unrelated-pending", "refuse");
    let ids = s.items();
    let request = s.set_request(&ids[0], "[x] **Ship.** Pending without a git child.");
    s.kill_at("after_rename", "set", &request, &[]);
    let before = read(&s.root, WIDGETS);
    let record = s.record();
    assert!(record.get("pid").is_none());
    let request = unowned_lock_request(&s, ".git/index.lock", 31);
    let (code, out) = s.run("commit-pending", request, &[]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["unresolved"], 1);
    assert_eq!(out["records"], json!([]));
    assert_eq!(s.record(), record);
    assert_eq!(read(&s.root, WIDGETS), before);
    let out = s.recover(json!({}), &[]);
    assert_eq!(one_result(&out)["result"], "committed", "{out}");
    assert!(s.records().is_empty() && s.clean());
}

#[test]
fn commit_clear_unowned_lock_cannot_bypass_pending_record_ownership() {
    let s = Store::new("unowned-pending", "skip");
    let ids = s.items();
    let request = s.set_request(&ids[0], "[x] **Ship.** Pending.");
    s.kill_in_git(&request, s.holding(".git/index.lock"));
    let before = read(&s.root, WIDGETS);
    let status = s.run("status", json!({}), &[]).1;
    let lock = status["git"]["locks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|l| l["file"] == ".git/index.lock")
        .unwrap();
    let (code, out) = s.run(
        "commit-pending",
        json!({
            "clear_lock":lock["file"],"identity":lock["identity"],"size":lock["size"],"quiescent":true
        }),
        &[],
    );
    assert_eq!(code, 2, "{out}");
    assert_eq!(out["error"]["reason"], "pending_records", "{out}");
    assert_eq!(read(&s.root, WIDGETS), before);
    assert_eq!(s.records().len(), 1);
    assert!(s.root.join(".git/index.lock").exists());
}

#[test]
fn commit_clear_lock_is_refused_for_a_replaced_lock() {
    let s = Store::new("commit-replaced-lock", "skip");
    let ids = s.items();
    let request = s.set_request(&ids[0], "[x] **Ship.** Replaced.");
    s.kill_in_git(&request, s.holding(".git/index.lock"));
    let out = s.recover(json!({}), &[]);
    let op = one_result(&out)["op"].clone();
    let identity = s.identity(&out, ".git/index.lock");
    // Another file now stands at the reported name.
    std::fs::remove_file(s.root.join(".git/index.lock")).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(10));
    std::fs::write(s.root.join(".git/index.lock"), "").unwrap();
    let (code, refused) = s.run(
        "commit-pending",
        json!({"clear_lock": ".git/index.lock", "identity": identity, "op": op}),
        &[],
    );
    assert_eq!(code, 2, "{refused}");
    assert_eq!(refused["error"]["code"], "clear_lock_refused");
    assert!(s.root.join(".git/index.lock").exists());
    assert_eq!(s.records().len(), 1);
}

#[test]
fn commit_clear_lock_refusals_leave_every_lock() {
    // An `=` in the repository's own path must not hide a git working on it.
    let s = Store::new("commit-refusals=git-dir", "skip");
    let ids = s.items();
    let lock = s.root.join(".git/index.lock");
    let clear = |op: &Value, file: &str, identity: &str| {
        s.run(
            "commit-pending",
            json!({"clear_lock": file, "identity": identity, "op": op}),
            &[],
        )
    };
    let assert_refused = |(code, out): (i32, Value), why: &str| {
        assert_eq!(code, 2, "{why}: {out}");
        assert_eq!(out["error"]["code"], "clear_lock_refused", "{why}: {out}");
        assert!(lock.exists(), "{why}: the lock was removed");
    };

    // Older than the recorded spawn: the lock was there before git ran.
    let pause = Pause::new(&s.root, "reread");
    let request = s.set_request(&ids[0], "[x] **Ship.** Older.");
    let writer = s.spawn("set", &request, &[pause.env()]);
    pause.wait();
    std::fs::write(&lock, "").unwrap();
    std::thread::sleep(std::time::Duration::from_millis(20));
    pause.release();
    let (_, out) = finish(writer);
    assert_eq!(out["commit"]["reason"], "index_locked", "{out}");
    assert!(out["commit"]["published"] == false);
    let out = s.recover(json!({}), &[]);
    let op = one_result(&out)["op"].clone();
    assert_eq!(one_result(&out)["locks"][0]["record"], Value::Null);
    let identity = s.identity(&out, ".git/index.lock");
    assert_refused(clear(&op, ".git/index.lock", &identity), "older");
    std::fs::remove_file(&lock).unwrap();
    let out = s.recover(json!({}), &[]);
    assert_eq!(one_result(&out)["result"], "committed", "{out}");

    // A lock git took for this record, with its child dead.
    let request = s.set_request(&ids[1], "[x] **Refresh.** Held.");
    s.kill_in_git(&request, s.holding(".git/index.lock"));
    let out = s.recover(json!({}), &[]);
    let op = one_result(&out)["op"].clone();
    let identity = s.identity(&out, ".git/index.lock");
    // A process holds it open.
    let held = std::fs::File::open(&lock).unwrap();
    assert_refused(clear(&op, ".git/index.lock", &identity), "held open");
    drop(held);
    // A prepared update-ref transaction runs against the repository from
    // outside it.
    let head = s.head();
    let git_dir = format!("--git-dir={}", s.root.join(".git").display());
    let mut live = git_command(
        std::path::Path::new("/"),
        &s.home,
        &[git_dir.as_str(), "update-ref", "--stdin"],
    )
    .stdin(std::process::Stdio::piped())
    .stdout(std::process::Stdio::piped())
    .spawn()
    .unwrap();
    {
        use std::io::{BufRead, Write};
        let stdin = live.stdin.as_mut().unwrap();
        writeln!(stdin, "start\nupdate refs/heads/other {head}\nprepare").unwrap();
        stdin.flush().unwrap();
        let mut line = String::new();
        let mut out = std::io::BufReader::new(live.stdout.as_mut().unwrap());
        out.read_line(&mut line).unwrap();
        out.read_line(&mut line).unwrap();
        assert!(line.contains("prepare"), "{line}");
    }
    assert_refused(clear(&op, ".git/index.lock", &identity), "update-ref");
    drop(live.stdin.take());
    live.wait().unwrap();
    // A next-index lock naming another process, and another branch's lock.
    std::fs::write(s.root.join(".git/next-index-1.lock"), "").unwrap();
    std::fs::write(s.root.join(".git/refs/heads/other.lock"), "").unwrap();
    let out = s.recover(json!({}), &[]);
    for file in [".git/next-index-1.lock", ".git/refs/heads/other.lock"] {
        let listed = s.run("status", json!({}), &[]).1["git"]["locks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|l| l["file"] == file)
            .unwrap()["identity"]
            .as_str()
            .unwrap()
            .to_string();
        assert_refused(clear(&op, file, &listed), file);
        assert!(s.root.join(file).exists());
    }
    std::fs::remove_file(s.root.join(".git/next-index-1.lock")).unwrap();
    std::fs::remove_file(s.root.join(".git/refs/heads/other.lock")).unwrap();
    let identity = s.identity(&out, ".git/index.lock");
    let out = s.recover(
        json!({"clear_lock": ".git/index.lock", "identity": identity, "op": op}),
        &[],
    );
    assert_eq!(one_result(&out)["result"], "committed", "{out}");

    // A record with no PID: git never ran for it.
    let request = s.set_request(&ids[2], "[x] **Confirm.** No child.");
    s.kill_at("after_rename", "set", &request, &[]);
    assert!(s.record().get("pid").is_none());
    std::fs::write(&lock, "").unwrap();
    let out = s.recover(json!({}), &[]);
    let op = one_result(&out)["op"].clone();
    let identity = s.identity(&out, ".git/index.lock");
    assert_refused(clear(&op, ".git/index.lock", &identity), "no pid");
}

fn proc_lock_fixture(name: &str) -> (Store, Value, String) {
    let s = Store::new(name, "skip");
    let ids = s.items();
    let request = s.set_request(&ids[0], "[x] **Ship.** Pending.");
    let pause = Pause::new(&s.root, "after_rename");
    let writer = s.spawn(
        "set",
        &request,
        &[pause.env(), s.holding(".git/index.lock")],
    );
    pause.wait();
    pause.release();
    let pid_file = s.home.join("git.pid");
    let pid: u32 = wait_text(&pid_file).trim().parse().unwrap();
    let helper = markrivet_core::git::GitChild {
        pid: Some(pid),
        spawned: std::time::UNIX_EPOCH,
        identity: markrivet_core::git::ProcessIdentity::read(pid),
    };
    let mut writer = writer;
    writer.kill().unwrap();
    writer.wait().unwrap();
    assert!(gone(helper), "git outlived Rivet");

    let out = s.recover(json!({}), &[]);
    let op = one_result(&out)["op"].clone();
    let identity = s.identity(&out, ".git/index.lock");
    (s, op, identity)
}

#[test]
fn commit_clear_lock_vetoes_proc_enumeration_errors_without_changing_bytes() {
    let (s, op, identity) = proc_lock_fixture("commit-proc-enumeration");
    let document = read(&s.root, WIDGETS);
    let lock = s.root.join(".git/index.lock");
    let lock_bytes = std::fs::read(&lock).unwrap();
    let index = std::fs::read(s.root.join(".git/index")).unwrap();
    let record = s.record();
    let head = s.head();
    for fault in [
        // Simulate /proc/self/fd being hidden from the root-descriptor lookup.
        "lock_proc_self_fd",
        "lock_proc_list",
        "lock_proc_entry",
        "lock_proc_fds",
        "lock_proc_fd_entry",
        "lock_proc_fds=EINVAL",
        "lock_proc_fd_entry=EINVAL",
        "lock_proc_fd_entry=EACCES",
        "lock_proc_entry=ENOENT",
    ] {
        let (code, out) = s.run(
            "commit-pending",
            json!({"clear_lock": ".git/index.lock", "identity": identity, "op": op}),
            &[("RIVET_TEST_FAULT", fault.into())],
        );
        assert_eq!(code, 2, "{fault}: {out}");
        assert_eq!(out["error"]["code"], "clear_lock_refused", "{fault}: {out}");
        let expected = if let Some((_, errno)) = fault.split_once('=') {
            let code = match errno {
                "EACCES" => libc::EACCES,
                "ENOENT" => libc::ENOENT,
                _ => libc::EINVAL,
            };
            std::io::Error::from_raw_os_error(code).to_string()
        } else {
            format!("injected {fault} fault")
        };
        assert!(
            out["error"]["message"]
                .as_str()
                .unwrap()
                .contains(&expected),
            "{fault}: {out}"
        );
        assert_eq!(std::fs::read(&lock).unwrap(), lock_bytes, "{fault}");
        assert_eq!(read(&s.root, WIDGETS), document, "{fault}");
        assert_eq!(
            std::fs::read(s.root.join(".git/index")).unwrap(),
            index,
            "{fault}"
        );
        assert_eq!(s.record(), record, "{fault}");
        assert_eq!(s.head(), head, "{fault}");
    }
    let out = s.recover(
        json!({"clear_lock": ".git/index.lock", "identity": identity, "op": op}),
        &[],
    );
    assert_eq!(one_result(&out)["result"], "committed", "{out}");
    assert!(!lock.exists());
    assert_eq!(read(&s.root, WIDGETS), document);
}

#[test]
fn commit_clear_lock_skips_unreadable_or_departed_processes() {
    for (point, errno) in [
        ("lock_proc_fds", "EACCES"),
        ("lock_proc_fds", "ENOENT"),
        ("lock_proc_fd_entry", "ENOENT"),
    ] {
        let (s, op, identity) = proc_lock_fixture(&format!("commit-proc-skip-{point}-{errno}"));
        let document = read(&s.root, WIDGETS);
        let out = s.recover(
            json!({"clear_lock": ".git/index.lock", "identity": identity, "op": op}),
            &[("RIVET_TEST_FAULT", format!("{point}@1={errno}"))],
        );
        assert_eq!(one_result(&out)["result"], "committed", "{errno}: {out}");
        assert!(!s.root.join(".git/index.lock").exists());
        assert_eq!(read(&s.root, WIDGETS), document, "{errno}");
    }
}

#[test]
fn commit_a_lingering_lock_makes_a_write_ineligible() {
    let s = Store::new("commit-git-locked", "skip");
    let ids = s.items();
    for (n, file) in [
        ".git/index.lock",
        ".git/HEAD.lock",
        ".git/refs/heads/main.lock",
    ]
    .into_iter()
    .enumerate()
    {
        std::fs::write(s.root.join(file), "").unwrap();
        for attempt in 0..2 {
            let (code, out) = s.set(&ids[n], &format!("[x] **Locked {n}.** Try {attempt}."), &[]);
            assert_eq!(code, 0, "{file}: {out}");
            assert_eq!(out["commit"]["status"], "ineligible", "{out}");
            assert_eq!(out["commit"]["reason"], "git_locked", "{out}");
            assert!(s.records().is_empty(), "no record");
            assert_eq!(s.receipts().last().unwrap()["outcome"], "ineligible");
        }
        std::fs::remove_file(s.root.join(file)).unwrap();
    }
}

#[test]
fn commit_lingering_lock_dirty_policies_and_explicit_adopt_recovery() {
    for policy in ["refuse", "skip"] {
        let s = Store::new(&format!("commit-lock-recovery-{policy}"), policy);
        let ids = s.items();
        let target = &ids[0];
        let before = read(&s.root, WIDGETS);
        let docs_before = read(&s.root, DOCS);
        let base = s.head();
        let index = std::fs::read(s.root.join(".git/index")).unwrap();
        let lock = s.root.join(".git/index.lock");

        // Take the Git lock while a real writer holds Rivet's project lock.
        let pause = Pause::new(&s.root, "planned");
        let first_content = "[x] **Locked write.** Lands without a commit.";
        let request = s.set_request(target, first_content);
        let first = s.spawn("set", &request, &[pause.env()]);
        pause.wait();
        std::fs::write(&lock, "held by the test\n").unwrap();
        pause.release();
        let (code, out) = finish(first);
        assert_eq!(code, 0, "{policy}: {out}");
        assert_eq!(out["status"], "applied", "{out}");
        assert_eq!(out["commit"]["status"], "ineligible", "{out}");
        assert_eq!(out["commit"]["reason"], "git_locked", "{out}");
        let landed = read(&s.root, WIDGETS);
        assert!(landed.contains(first_content));
        assert_ne!(landed, before);
        assert_eq!(s.head(), base);
        assert_eq!(std::fs::read(s.root.join(".git/index")).unwrap(), index);
        assert!(
            s.records().is_empty(),
            "a preflight skip leaves no pending commit"
        );
        assert_eq!(s.receipts().len(), 1);

        // Under refuse, the dirt takes precedence even while the lock remains.
        if policy == "refuse" {
            let (code, out) = s.set(target, "[x] **Blocked.** Lock still held.", &[]);
            assert_eq!(code, 2, "{out}");
            assert_eq!(out["error"]["code"], "commit_refused", "{out}");
            assert_eq!(read(&s.root, WIDGETS), landed);
            assert_eq!(s.receipts().len(), 1);
        }
        assert_eq!(
            std::fs::read_to_string(&lock).unwrap(),
            "held by the test\n"
        );
        std::fs::remove_file(&lock).unwrap();

        let second_content = "[x] **After the lock.** A second write.";
        let (code, out) = s.set(target, second_content, &[]);
        if policy == "refuse" {
            assert_eq!(code, 2, "{out}");
            assert_eq!(out["error"]["code"], "commit_refused", "{out}");
            assert_eq!(
                read(&s.root, WIDGETS),
                landed,
                "refusal preserves every byte"
            );
            assert_eq!(s.receipts().len(), 1);
        } else {
            assert_eq!(code, 0, "{out}");
            assert_eq!(out["status"], "applied", "{out}");
            assert_eq!(out["commit"]["status"], "skipped", "{out}");
            assert_eq!(out["commit"]["reason"], "dirty", "{out}");
            assert!(read(&s.root, WIDGETS).contains(second_content));
            assert_eq!(s.receipts().len(), 2);
        }
        assert_eq!(s.head(), base);
        assert_eq!(std::fs::read(s.root.join(".git/index")).unwrap(), index);
        assert!(s.records().is_empty());

        // Recovery cannot turn the completed ineligibility/skip into a commit.
        let dirty = read(&s.root, WIDGETS);
        let recovered = s.recover(json!({}), &[]);
        assert_eq!(recovered["unresolved"], 0, "{recovered}");
        assert_eq!(recovered["records"], json!([]), "{recovered}");
        assert_eq!(read(&s.root, WIDGETS), dirty);
        assert_eq!(s.head(), base);

        let (code, out) = s.run("adopt", json!({"path": WIDGETS}), &[]);
        assert_eq!(code, 0, "{out}");
        assert_eq!(out["commit"]["status"], "committed", "{out}");
        assert_eq!(read(&s.root, WIDGETS), dirty, "nothing needs stamping");
        assert_eq!(s.since(&base).len(), 1);
        assert!(s.clean());

        let final_content = "[x] **Recovered.** Writes commit again.";
        let (code, out) = s.set(target, final_content, &[]);
        assert_eq!(code, 0, "{out}");
        assert_eq!(out["commit"]["status"], "committed", "{out}");
        let final_text = read(&s.root, WIDGETS);
        assert!(final_text.contains(final_content));
        // The first item's edit and refreshed evidence are confined to Action.
        assert_eq!(
            final_text.split("## Notes").nth(1),
            before.split("## Notes").nth(1)
        );
        assert_eq!(read(&s.root, DOCS), docs_before);
        assert_eq!(s.items(), ids, "stays retain their item attachment");
        assert_eq!(s.since(&base).len(), 2);
        assert!(s.clean());
        assert!(s.records().is_empty());
    }
}

#[test]
fn commit_a_vetoed_next_index_is_reported_and_recovery_still_commits() {
    let s = Store::new("commit-veto-next", "skip");
    let ids = s.items();
    let request = s.set_request(&ids[0], "[x] **Ship.** Vetoed.");
    let pid = s.kill_in_git(&request, s.holding(".git/next-index-$$.lock"));
    let next = s.root.join(format!(".git/next-index-{pid}.lock"));
    let held = std::fs::File::open(&next).unwrap();
    let out = s.recover(json!({}), &[]);
    let r = one_result(&out);
    assert_eq!(r["result"], "committed", "{out}");
    assert!(r["kept"][0]["veto"].as_str().unwrap().contains("held open"));
    assert!(next.exists(), "a vetoed lock is left");
    drop(held);
}

#[test]
fn commit_a_record_with_no_pid_meets_outside_locks() {
    let s = Store::new("commit-no-pid", "skip");
    let ids = s.items();
    let request = s.set_request(&ids[0], "[x] **Ship.** Before the helper.");
    s.kill_at("after_rename", "set", &request, &[]);
    for file in [
        ".git/index.lock",
        ".git/HEAD.lock",
        ".git/refs/heads/main.lock",
    ] {
        std::fs::write(s.root.join(file), "").unwrap();
        let out = s.recover(json!({}), &[s.logging("quiet")]);
        let r = one_result(&out);
        assert_eq!(r["reason"], "stale_git_lock", "{out}");
        assert_eq!(r["locks"][0]["file"], file);
        assert_eq!(r["locks"][0]["record"], Value::Null);
        assert!(s.logged("quiet").is_empty(), "git ran");
        assert!(s.root.join(file).exists());
        std::fs::remove_file(s.root.join(file)).unwrap();
    }
    let out = s.recover(json!({}), &[]);
    assert_eq!(one_result(&out)["result"], "committed", "{out}");
}

#[test]
fn commit_a_live_git_child_is_reported_and_left() {
    let s = Store::new("commit-live-child", "skip");
    let ids = s.items();
    let request = s.set_request(&ids[0], "[x] **Ship.** Running.");
    s.kill_in_git(&request, s.holding(".git/index.lock"));
    // This test process is live throughout the test.
    let mut rec = s.record();
    rec["pid"] = json!(std::process::id());
    let identity = markrivet_core::git::ProcessIdentity::read(std::process::id()).unwrap();
    let file = s.root.join(format!(
        ".rivet/pending/{}.json",
        rec["op"].as_str().unwrap()
    ));
    let document = std::fs::read(s.root.join(WIDGETS)).unwrap();
    let lock_file = s.root.join(".git/index.lock");
    let lock_bytes = std::fs::read(&lock_file).unwrap();
    let base = s.head();
    let lock_identity = s.run("status", json!({}), &[]).1["git"]["locks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|l| l["file"] == ".git/index.lock")
        .unwrap()["identity"]
        .clone();
    for legacy in [false, true] {
        if legacy {
            rec.as_object_mut().unwrap().remove("child_identity");
        } else {
            rec["child_identity"] = serde_json::to_value(&identity).unwrap();
        }
        std::fs::write(&file, serde_json::to_vec(&rec).unwrap()).unwrap();
        let before = std::fs::read(&file).unwrap();
        let out = s.recover(json!({}), &[s.logging("quiet")]);
        assert_eq!(one_result(&out)["result"], "git_child_running", "{out}");
        for (request, expected) in [
            (
                json!({"abandon": rec["op"], "reason": "test"}),
                "pending_unresolved",
            ),
            (
                json!({"clear_lock": ".git/index.lock", "identity": lock_identity, "op": rec["op"]}),
                "clear_lock_refused",
            ),
        ] {
            let (code, out) = s.run("commit-pending", request, &[s.logging("quiet")]);
            assert_ne!(code, 0, "{out}");
            assert_eq!(out["error"]["code"], expected, "{out}");
        }
        let (code, out) = s.set(&ids[1], "[x] **Refresh.** Must be refused.", &[]);
        assert_ne!(code, 0, "{out}");
        assert_eq!(out["error"]["code"], "pending_unresolved", "{out}");
        assert_eq!(std::fs::read(&file).unwrap(), before, "record untouched");
        assert_eq!(std::fs::read(s.root.join(WIDGETS)).unwrap(), document);
        assert_eq!(std::fs::read(&lock_file).unwrap(), lock_bytes);
        assert_eq!(s.head(), base);
        assert!(s.logged("quiet").is_empty());
        assert_eq!(s.items(), ids);
    }
}

#[test]
fn commit_an_unreadable_child_identity_never_releases_git() {
    let s = Store::new("commit-no-child-identity", "skip");
    let ids = s.items();
    let head = s.head();
    let index = std::fs::read(s.root.join(".git/index")).unwrap();
    // Build the same candidate through the existing pause hook so the
    // failed commit can be checked byte for byte, including every stay.
    let request = s.set_request(&ids[0], "[x] **Ship.** Identity failure.");
    let pause = Pause::new(&s.root, "applied");
    let writer = s.spawn(
        "set",
        &request,
        &[
            pause.env(),
            s.logging("no-identity"),
            ("RIVET_TEST_FAULT", "child_identity".into()),
        ],
    );
    pause.wait();
    let candidate = std::fs::read(s.root.join(WIDGETS)).unwrap();
    pause.release();
    let (code, out) = finish(writer);
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["commit"]["status"], "failed", "{out}");
    assert_eq!(out["commit"]["published"], false, "{out}");
    assert_eq!(s.head(), head);
    assert_eq!(std::fs::read(s.root.join(".git/index")).unwrap(), index);
    assert_eq!(std::fs::read(s.root.join(WIDGETS)).unwrap(), candidate);
    assert!(
        !s.logged("no-identity").contains(" update-ref --stdin "),
        "Git was released before identity was recorded"
    );
    let rec = s.record();
    assert!(rec.get("pid").is_none(), "{rec}");
    assert!(rec.get("child_identity").is_none(), "{rec}");
    let out = s.recover(json!({}), &[]);
    assert_eq!(one_result(&out)["result"], "committed", "{out}");
    assert_eq!(std::fs::read(s.root.join(WIDGETS)).unwrap(), candidate);
    assert_eq!(s.items(), ids);
    assert!(s.records().is_empty());
}

/// A recorded PID can be alive while the recorded child is gone. Exercise
/// all consumers of that evidence through separate CLI processes.
#[test]
fn commit_recovery_survives_reused_pids_and_reboots() {
    for mismatch in ["start_time", "boot_id"] {
        for action in ["recover", "settle", "abandon", "clear_lock", "next_index"] {
            let s = Store::new(&format!("commit-reuse-{mismatch}-{action}"), "skip");
            let ids = s.items();
            let request = s.set_request(&ids[0], "[x] **Ship.** Candidate with its stay.");
            s.kill_at("pid_recorded", "set", &request, &[]);
            let candidate = std::fs::read(s.root.join(WIDGETS)).unwrap();
            let mut rec = s.record();
            let identity = markrivet_core::git::ProcessIdentity::read(std::process::id()).unwrap();
            assert!(rec["child_identity"]["boot_id"].is_string());
            assert!(rec["child_identity"]["start_time"].is_u64());
            rec["pid"] = json!(std::process::id());
            rec["child_identity"] = serde_json::to_value(identity).unwrap();
            if mismatch == "start_time" {
                rec["child_identity"]["start_time"] =
                    json!(rec["child_identity"]["start_time"].as_u64().unwrap() + 1);
            } else {
                rec["child_identity"]["boot_id"] = json!("synthetic-previous-boot");
            }
            let pending = s.root.join(format!(
                ".rivet/pending/{}.json",
                rec["op"].as_str().unwrap()
            ));
            std::fs::write(&pending, serde_json::to_vec(&rec).unwrap()).unwrap();

            let lock = if action == "clear_lock" {
                Some(".git/index.lock".to_string())
            } else if action == "next_index" {
                Some(format!(".git/next-index-{}.lock", std::process::id()))
            } else {
                None
            };
            if let Some(file) = &lock {
                std::fs::write(s.root.join(file), b"synthetic lock").unwrap();
            }
            if action == "settle" {
                // An ordinary write must settle the old record before editing.
                let (code, out) = s.set(&ids[1], "[x] **Refresh.** Later write.", &[]);
                assert_eq!(code, 0, "{out}");
                assert_eq!(out["commit"]["status"], "committed", "{out}");
                let text = read(&s.root, WIDGETS);
                assert!(text.contains("Candidate with its stay."));
                assert!(text.contains("Later write."));
            } else {
                let out = if action == "clear_lock" {
                    let out = s.recover(json!({}), &[]);
                    assert_eq!(one_result(&out)["result"], "repair_required", "{out}");
                    assert_eq!(one_result(&out)["locks"][0]["record"], rec["op"], "{out}");
                    assert_eq!(std::fs::read(s.root.join(WIDGETS)).unwrap(), candidate);
                    let identity = s.identity(&out, ".git/index.lock");
                    s.recover(json!({"clear_lock": ".git/index.lock", "identity": identity, "op": rec["op"]}), &[])
                } else if action == "abandon" {
                    s.recover(
                        json!({"abandon": rec["op"], "reason": "synthetic test"}),
                        &[],
                    )
                } else {
                    s.recover(json!({}), &[])
                };
                let expected = if action == "abandon" {
                    "abandoned"
                } else {
                    "committed"
                };
                assert_eq!(one_result(&out)["result"], expected, "{out}");
                assert_eq!(
                    std::fs::read(s.root.join(WIDGETS)).unwrap(),
                    candidate,
                    "recovery preserves all candidate bytes and stays"
                );
            }
            assert!(!pending.exists());
            if let Some(file) = lock {
                assert!(!s.root.join(file).exists());
            }
            assert_eq!(s.items(), ids, "stays keep their attachment");
            assert_eq!(
                unsafe { libc::kill(std::process::id() as libc::pid_t, 0) },
                0
            );
        }
    }
}

#[test]
fn commit_a_slashed_branch_lock_is_found_and_cleared() {
    let s = Store::new("commit-slashed", "skip");
    s.git(&["checkout", "-q", "-b", "feat/x"]);
    let ids = s.items();
    let request = s.set_request(&ids[0], "[x] **Ship.** On a slashed branch.");
    s.kill_in_git(&request, s.holding(".git/refs/heads/feat/x.lock"));
    let out = s.recover(json!({}), &[]);
    let file = ".git/refs/heads/feat/x.lock";
    assert_eq!(one_result(&out)["locks"][0]["file"], file, "{out}");
    let identity = s.identity(&out, file);
    let op = one_result(&out)["op"].clone();
    let out = s.recover(
        json!({"clear_lock": file, "identity": identity, "op": op}),
        &[],
    );
    assert_eq!(one_result(&out)["result"], "committed", "{out}");
    assert_eq!(
        s.git(&["rev-parse", "--abbrev-ref", "HEAD"]).trim(),
        "feat/x"
    );
}

/// Make the widgets tracker dirty with an outside edit outside any item.
fn dirty(s: &Store) {
    let text = read(&s.root, WIDGETS).replace("status: active", "status: paused");
    std::fs::write(s.root.join(WIDGETS), text).unwrap();
}

#[test]
fn commit_status_reports_records_receipts_owners_and_locks() {
    let s = Store::new("commit-status", "skip");
    let ids = s.items();
    // A committed write by s1 to the docs tracker.
    let (_, out) = s.set_docs("[x] **Licence.** By s1.");
    assert_eq!(out["commit"]["status"], "committed");
    let docs_op = s.receipts().last().unwrap()["op"].clone();
    let mut request = s.set_request(&ids[0], "[x] **Ship.** By s2.");
    request["session"] = json!("s2");
    s.kill_in_git(&request, s.holding(".git/index.lock"));
    let rec = s.record();
    let (code, out) = s.run("status", json!({}), &[]);
    assert_eq!(code, 0, "{out}");
    let path = |p: &str| {
        out["paths"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["path"] == p)
            .unwrap()
            .clone()
    };
    let widgets = path(WIDGETS);
    assert_eq!(widgets["pending"][0]["op"], rec["op"]);
    assert_eq!(widgets["pending"][0]["state"], "applied");
    assert_eq!(widgets["owner"], "pending:s2");
    let docs = path(DOCS);
    assert_eq!(docs["last_write"]["op"], docs_op);
    assert_eq!(docs["owner"], "receipt:");
    let lock = out["git"]["locks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|l| l["file"] == ".git/index.lock")
        .unwrap_or_else(|| panic!("{out}"));
    assert_eq!(lock["record"], rec["op"]);
    assert_eq!(out["receipts"]["pending"]["applied"], 1);
    assert!(
        s.root.join(".git/index.lock").exists(),
        "status removes nothing"
    );
    // The latest receipt per path follows sequence numbers, not file order.
    std::fs::remove_file(s.root.join(".git/index.lock")).unwrap();
    s.recover(json!({}), &[]);
    let (_, out) = s.run("status", json!({"paths": [WIDGETS]}), &[]);
    assert_eq!(out["paths"][0]["last_write"]["op"], rec["op"]);
    assert_eq!(out["paths"][0]["owner"], "receipt:s2");
    assert_eq!(out["paths"][0]["pending"], json!([]));
}

#[test]
fn commit_abandoned_records_carry_their_application_certainty() {
    let s = Store::new("commit-abandon", "skip");
    let ids = s.items();
    let abandon = |op: &Value| {
        let out = s.recover(json!({"abandon": op, "reason": "test"}), &[]);
        one_result(&out)["applied"].clone()
    };
    let last_owner = || {
        let (_, out) = s.run("status", json!({"paths": [WIDGETS]}), &[]);
        out["paths"][0]["last_write"]["op"].clone()
    };
    // Before rename: never applied.
    let mut request = s.set_request(&ids[0], "[x] **Ship.** Never.");
    request["session"] = json!("before");
    s.kill_at("pending_written", "set", &request, &[]);
    let op = s.record()["op"].clone();
    assert_eq!(abandon(&op), false);
    assert_ne!(last_owner(), op, "a write that never landed is no one's");
    // After a confirmed application (a failed commit).
    let slow = s.script(
        "slow",
        "case \"$*\" in *\" update-ref --stdin \"*) sleep 30;; esac\nexec git \"$@\"",
    );
    let mut request = s.set_request(&ids[0], "[x] **Ship.** Applied.");
    request["session"] = json!("after");
    let (_, out) = s.run(
        "set",
        request,
        &[slow, ("RIVET_TEST_GIT_DEADLINE_MS", "300".into())],
    );
    let op = json!(out["commit"]["pending"]);
    assert_eq!(abandon(&op), true);
    assert_eq!(last_owner(), op);
    let (_, status) = s.run("status", json!({"paths": [WIDGETS]}), &[]);
    assert_eq!(status["paths"][0]["owner"], "receipt:after");
    // Over third-party bytes, before the rename.
    s.git(&["checkout", "--", WIDGETS]);
    let mut request = s.set_request(&ids[1], "[x] **Refresh.** Unknown.");
    request["session"] = json!("unknown");
    s.kill_at("pending_written", "set", &request, &[]);
    let op3 = s.record()["op"].clone();
    std::fs::write(s.root.join(WIDGETS), "third party\n").unwrap();
    assert_eq!(abandon(&op3), "unknown");
    let (_, status) = s.run("status", json!({"paths": [WIDGETS]}), &[]);
    assert_eq!(
        status["paths"][0]["last_write"]["op"], op,
        "unknown never wins"
    );
    assert_eq!(status["receipts"]["unknown_by_session"]["unknown"], 1);
    assert_eq!(status["receipts"]["writes"], 1, "{status}");
    for op in [&op, &op3] {
        let events = s.events(op.as_str().unwrap());
        assert_eq!(events.last().unwrap()["outcome"], "abandoned");
    }
}

fn check_commit_answer(name: &str, setup: &str, status: &str, reason: Option<&str>) {
    let s = Store::new(name, "skip");
    let ids = s.items();
    if setup == "dirty" || setup == "staged" {
        dirty(&s);
    }
    if setup == "staged" {
        s.git(&["add", "--", WIDGETS]);
    }
    if setup == "locked" {
        std::fs::write(s.root.join(".git/index.lock"), "").unwrap();
    }
    let mut previous_op = Value::Null;
    for attempt in 0..2 {
        let (code, out) = s.set(&ids[0], &format!("[x] **Ship.** Operation {attempt}."), &[]);
        assert_eq!(code, 0, "{out}");
        assert_eq!(out["commit"]["status"], status, "{out}");
        assert_eq!(out["commit"]["reason"], json!(reason), "{out}");
        let receipts = s.receipts();
        assert_eq!(receipts.len(), attempt + 1);
        let event = receipts.last().unwrap();
        assert_eq!(event["outcome"], status);
        assert_eq!(event["path"], WIDGETS);
        assert!(event["op"].as_str().is_some_and(|op| !op.is_empty()));
        assert_eq!(out["commit"]["op"], event["op"], "{out}");
        assert_ne!(out["commit"]["op"], previous_op);
        previous_op = out["commit"]["op"].clone();
    }
}

#[test]
fn commit_committed_answer_names_its_journal_operation() {
    check_commit_answer("commit-op-clean", "clean", "committed", None);
}

#[test]
fn commit_dirty_skip_answer_names_its_journal_operation() {
    check_commit_answer("commit-op-dirty", "dirty", "skipped", Some("dirty"));
}

#[test]
fn commit_staged_ineligible_answer_names_its_journal_operation() {
    check_commit_answer("commit-op-staged", "staged", "ineligible", Some("staged"));
}

#[test]
fn commit_locked_ineligible_answer_names_its_journal_operation() {
    check_commit_answer(
        "commit-op-locked",
        "locked",
        "ineligible",
        Some("git_locked"),
    );
}

#[test]
fn commit_skip_and_ineligible_writes_never_hold_applied() {
    let s = Store::new("commit-noncommit", "skip");
    let ids = s.items();
    dirty(&s);
    for point in ["after_rename", "landed"] {
        let request = s.set_request(&ids[0], &format!("[x] **Ship.** Skipped at {point}."));
        s.kill_at(point, "set", &request, &[]);
        let rec = s.record();
        assert_eq!(rec["outcome"], "skipped");
        assert_eq!(
            rec["state"],
            if point == "landed" {
                "landed"
            } else {
                "prepared"
            }
        );
        let out = s.recover(json!({}), &[s.logging("quiet")]);
        assert_eq!(one_result(&out)["result"], "skipped", "{out}");
        assert!(
            s.logged("quiet").is_empty(),
            "git ran for a non-commit record"
        );
        assert!(s.records().is_empty());
        let op = rec["op"].as_str().unwrap();
        assert_eq!(s.events(op)[0]["applied"], true);
    }
}

#[test]
fn commit_a_path_with_an_unresolved_record_refuses_every_write() {
    let s = Store::new("commit-unresolved", "skip");
    let ids = s.items();
    let request = s.set_request(&ids[0], "[x] **Ship.** Stuck.");
    s.kill_in_git(&request, s.holding(".git/index.lock"));
    for policy in ["skip", "adopt", "refuse"] {
        s.configure(json!({"commit": "per-write", "on_dirty": policy}));
        for spelling in [
            WIDGETS,
            "tracking//TRACKING_WIDGETS.md",
            "tracking/./TRACKING_WIDGETS.md",
        ] {
            let before = read(&s.root, WIDGETS);
            let mut request = s.set_request(&ids[1], "[x] **Refresh.** Blocked.");
            request["path"] = json!(spelling);
            let (code, out) = s.run("set", request, &[]);
            assert_eq!(code, 2, "{policy} {spelling}: {out}");
            assert_eq!(out["error"]["code"], "pending_unresolved");
            assert_eq!(read(&s.root, WIDGETS), before);
        }
    }
    // Another path is not held up.
    s.configure(json!({"commit": "per-write", "on_dirty": "skip"}));
    let (code, out) = s.set_docs("[x] **Licence.** Free.");
    assert_eq!(code, 0, "{out}");
}

#[test]
fn commit_adopt_commits_an_outside_mode_change() {
    let s = Store::new("commit-adopt-mode", "adopt");
    let ids = s.items();
    let original = s.set_request(&ids[0], "x")["guard"].clone();
    let (_, item) = rivet(
        &s.root,
        "get",
        json!({"path": WIDGETS, "select": {"kind": "id", "id": ids[0]}}),
    );
    let content = item["content"].as_str().unwrap().to_string();
    assert!(original.is_string());
    // An outside edit to the item and an outside chmod.
    let edited = read(&s.root, WIDGETS).replace("The release branch is cut", "Edited outside");
    std::fs::write(s.root.join(WIDGETS), edited).unwrap();
    std::fs::set_permissions(
        s.root.join(WIDGETS),
        std::os::unix::fs::PermissionsExt::from_mode(0o755),
    )
    .unwrap();
    // The write restores HEAD's bytes for the item; the mode survives.
    let (code, out) = s.set(&ids[0], &content, &[]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["commit"]["status"], "committed", "{out}");
    let sha = s.head();
    assert_eq!(
        s.git(&["ls-tree", &sha, WIDGETS]).split(' ').next(),
        Some("100755")
    );
    assert_eq!(s.git(&["diff", "HEAD^", "HEAD"]).matches("@@").count(), 0);
    assert!(
        s.message(&sha)
            .contains("Includes-outside-edit: mode 100644 to 100755")
    );
    assert!(s.clean());
    // A mode-changing adopt whose git dies before installing the index:
    // the repair uses the candidate's mode.
    std::fs::set_permissions(
        s.root.join(WIDGETS),
        std::os::unix::fs::PermissionsExt::from_mode(0o644),
    )
    .unwrap();
    let (code, out) = s.set(&ids[1], "[x] **Refresh.** Mode back.", &[s.half_commit()]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["commit"]["status"], "failed", "{out}");
    assert_eq!(out["commit"]["published"], true);
    let out = s.recover(json!({}), &[]);
    assert_eq!(one_result(&out)["result"], "committed", "{out}");
    let staged = s.git(&["ls-files", "-s", WIDGETS]);
    assert!(staged.starts_with("100644"), "{staged}");
    assert!(s.clean());
}

#[test]
fn commit_an_adopt_restoring_head_is_unchanged_in_git() {
    let s = Store::new("commit-adopt-same", "adopt");
    let ids = s.items();
    let (_, item) = rivet(
        &s.root,
        "get",
        json!({"path": WIDGETS, "select": {"kind": "id", "id": ids[0]}}),
    );
    let content = item["content"].as_str().unwrap().to_string();
    let edited = read(&s.root, WIDGETS).replace("The release branch is cut", "Edited outside");
    std::fs::write(s.root.join(WIDGETS), edited).unwrap();
    let base = s.head();
    let (code, out) = s.set(&ids[0], &content, &[]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["commit"]["status"], "unchanged_in_git", "{out}");
    assert_eq!(out["commit"]["op"], s.receipts()[0]["op"]);
    assert_eq!(s.head(), base);
    assert!(s.records().is_empty());
}

#[test]
fn commit_a_refused_replace_and_a_crash_before_rename_end_not_applied() {
    let s = Store::new("commit-not-applied", "skip");
    let ids = s.items();
    let before = read(&s.root, WIDGETS);
    let (code, out) = s.set(
        &ids[0],
        "[x] **Ship.** Refused.",
        &[("RIVET_TEST_FAULT", "rename".into())],
    );
    assert_eq!(code, 2, "{out}");
    assert!(s.records().is_empty(), "a refused replace leaves no record");
    let last = s.receipts().last().unwrap().clone();
    assert_eq!(last["outcome"], "not_applied");
    assert_eq!(last["applied"], false);
    let request = s.set_request(&ids[0], "[x] **Ship.** Crashed.");
    s.kill_at("before_rename", "set", &request, &[]);
    let out = s.recover(json!({}), &[s.logging("quiet")]);
    assert_eq!(one_result(&out)["result"], "not_applied", "{out}");
    assert!(s.logged("quiet").is_empty());
    assert!(s.records().is_empty());
    assert_eq!(read(&s.root, WIDGETS), before);
}

#[test]
fn commit_a_message_using_a_reserved_trailer_is_refused() {
    let s = Store::new("commit-reserved", "skip");
    let ids = s.items();
    for message in [
        "fix\n\nRivet-Op : x",
        "fix\nrivet-op= y",
        "Includes-Outside-Edit: z",
    ] {
        let mut request = s.set_request(&ids[0], "[x] **Ship.** No.");
        request["message"] = json!(message);
        let (code, out) = s.run("set", request, &[]);
        assert_eq!(code, 2, "{message}: {out}");
        assert_eq!(out["error"]["code"], "invalid_request");
    }
    let mut request = s.set_request(&ids[0], "[x] **Ship.** Yes.");
    request["message"] = json!("set: ship\n\nRivet-Options are fine");
    let (code, out) = s.run("set", request, &[]);
    assert_eq!(code, 0, "{out}");
    let sha = s.head();
    assert!(s.message(&sha).starts_with("set: ship\n"));
}

#[test]
fn commit_a_kill_before_the_helper_pid_is_recorded_leaves_no_lock() {
    let s = Store::new("commit-handoff", "skip");
    let ids = s.items();
    let ran = s.home.join("git.ran");
    let marker = s.script(
        "marker",
        "case \"$*\" in *\" update-ref --stdin \"*) touch $HOME_DIR/git.ran;; esac\nexec git \"$@\"",
    );
    let base = s.head();
    let request = s.set_request(&ids[0], "[x] **Ship.** Handoff.");
    let pause = Pause::new(&s.root, "helper_spawned");
    let mut child = s.spawn("set", &request, &[marker.clone(), pause.env()]);
    pause.wait();
    // The helper is waiting for its go byte: git has not run.
    std::thread::sleep(std::time::Duration::from_millis(150));
    assert!(!ran.exists(), "git ran before its PID was recorded");
    child.kill().unwrap();
    child.wait().unwrap();
    std::thread::sleep(std::time::Duration::from_millis(150));
    assert!(!ran.exists(), "git ran after Rivet died");
    assert_eq!(s.head(), base);
    assert!(!s.root.join(".git/index.lock").exists());
    assert!(s.record().get("pid").is_none());
    let out = s.recover(json!({}), &[]);
    assert_eq!(one_result(&out)["result"], "committed", "{out}");
}

#[test]
fn commit_external_change_and_head_moved_end_skipped() {
    for (name, reason) in [
        ("commit-ext", "external_change"),
        ("commit-moved", "head_moved"),
    ] {
        let s = Store::new(name, "skip");
        let ids = s.items();
        let pause = Pause::new(&s.root, "applied");
        let request = s.set_request(&ids[0], "[x] **Ship.** Raced.");
        let writer = s.spawn("set", &request, &[pause.env()]);
        pause.wait();
        if reason == "external_change" {
            let text = read(&s.root, WIDGETS) + "\nAn editor saved.\n";
            std::fs::write(s.root.join(WIDGETS), text).unwrap();
        } else {
            std::fs::write(s.root.join("notes.txt"), "moved\n").unwrap();
            s.git(&["commit", "-qm", "outside", "--", "notes.txt"]);
        }
        pause.release();
        let (code, out) = finish(writer);
        assert_eq!(code, 0, "{out}");
        assert_eq!(out["commit"]["status"], "skipped");
        assert_eq!(out["commit"]["reason"], reason);
        assert_eq!(out["commit"]["op"], s.receipts()[0]["op"]);
        assert!(s.records().is_empty());
        let last = s.receipts().last().unwrap().clone();
        assert_eq!(
            (last["outcome"].clone(), last["reason"].clone()),
            (json!("skipped"), json!(reason))
        );
    }
}

#[test]
fn commit_a_panic_during_the_commit_kills_git_before_the_lock_is_released() {
    use std::os::fd::AsRawFd;
    let s = Store::new("commit-panic", "skip");
    let ids = s.items();
    let pid_file = s.home.join("git.pid");
    let sleeper = s.script(
        "sleeper",
        "case \"$*\" in *\" read-tree \"*) echo $$ > $HOME_DIR/git.pid; while :; do sleep 0.05; done;; esac\nexec git \"$@\"",
    );
    let pause = Pause::new(&s.root, "git_panic");
    let request = s.set_request(&ids[0], "[x] **Ship.** Panic.");
    let writer = s.spawn(
        "set",
        &request,
        &[
            sleeper,
            pause.env(),
            ("RIVET_TEST_FAULT", "git_panic".into()),
        ],
    );
    pause.wait();
    let pid: u32 = wait_text(&pid_file).trim().parse().unwrap();
    let lock = std::fs::File::open(s.root.join(".rivet/lock")).unwrap();
    pause.release();
    // The first moment the lock is free, git is already gone.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        // SAFETY: a non-blocking flock on a descriptor this test owns.
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            break;
        }
        assert!(std::time::Instant::now() < deadline);
    }
    let alive = std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .ok()
        .and_then(|s| s.rsplit_once(") ").map(|(_, r)| !r.starts_with('Z')))
        .unwrap_or(false);
    assert!(!alive, "git ran past the lock release");
    // SAFETY: release the lock this test took.
    unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_UN) };
    let (code, out) = finish(writer);
    assert_eq!(code, 3, "{out}");
    assert_eq!(out["error"]["code"], "internal");
    let out = s.recover(json!({}), &[]);
    assert_eq!(one_result(&out)["result"], "committed", "{out}");
}

#[test]
fn commit_every_fault_point_ends_committed_or_with_a_settled_record() {
    let points = [
        "pending_written",
        "before_rename",
        "after_rename",
        "applied",
        "reread",
        "helper_spawned",
        "pid_recorded",
        "committed",
        "finalise",
    ];
    for point in points {
        let s = Store::new(&format!("commit-fault-{point}"), "skip");
        let ids = s.items();
        let request = s.set_request(&ids[0], &format!("[x] **Ship.** Killed at {point}."));
        s.kill_at(point, "set", &request, &[]);
        let op = s.record()["op"].as_str().unwrap().to_string();
        let (code, out) = s.run("commit-pending", json!({}), &[]);
        let pending = s.records();
        assert_eq!(code, 0, "{point}: recovery={out}; pending={pending:#?}");
        assert!(
            pending.is_empty(),
            "{point}: recovery={out}; pending={pending:#?}"
        );
        let result = one_result(&out)["result"].as_str().unwrap().to_string();
        let events = s.events(&op);
        let terminal: Vec<&Value> = events.iter().filter(|e| e["outcome"] != "failed").collect();
        assert_eq!(terminal.len(), 1, "{point}: {events:?}");
        let committed = s.carrying(&op);
        match terminal[0]["outcome"].as_str().unwrap() {
            "committed" => {
                assert_eq!(committed.len(), 1, "{point}");
                assert!(s.clean(), "{point}");
            }
            "not_applied" => {
                assert!(committed.is_empty(), "{point}");
                assert!(s.clean(), "{point}");
            }
            other => panic!("{point}: {other} ({result})"),
        }
    }
}

#[test]
fn commit_a_killed_skipped_write_still_yields_its_receipt_in_sequence() {
    let s = Store::new("commit-seq", "skip");
    let ids = s.items();
    dirty(&s);
    let request = s.set_request(&ids[0], "[x] **Ship.** Skipped, killed.");
    s.kill_at("after_rename", "set", &request, &[]);
    let killed = s.record();
    // A later write elsewhere appends its receipt first.
    let (code, out) = s.set_docs("[x] **Licence.** Meanwhile.");
    assert_eq!(code, 0, "{out}");
    s.recover(json!({}), &[]);
    let receipts = s.receipts();
    assert_eq!(receipts.len(), 2);
    assert_eq!(receipts[1]["op"], killed["op"], "recovery appends late");
    assert_eq!(receipts[1]["seq"], killed["seq"]);
    assert!(receipts[1]["seq"].as_u64() < receipts[0]["seq"].as_u64());
    assert_eq!(receipts[1]["outcome"], "skipped");
    assert_eq!(receipts[1]["recovered"], true);
    let (_, status) = s.run("status", json!({"paths": [WIDGETS]}), &[]);
    assert_eq!(status["paths"][0]["last_write"]["op"], killed["op"]);
}

#[test]
fn commit_a_kill_between_the_terminal_event_and_removal_yields_one_outcome() {
    let s = Store::new("commit-finalise", "skip");
    let ids = s.items();
    // A committed write, then (on a dirty file) a skipped one.
    for (n, want) in ["committed", "skipped"].into_iter().enumerate() {
        if n == 1 {
            dirty(&s);
        }
        let request = s.set_request(&ids[n], &format!("[x] **Item.** Final {n}."));
        s.kill_at("finalise", "set", &request, &[]);
        assert_eq!(s.receipts().last().unwrap()["outcome"], want);
        let op = s.record()["op"].as_str().unwrap().to_string();
        assert_eq!(s.events(&op).len(), 1, "the event is in, the record too");
        for _ in 0..2 {
            s.recover(json!({}), &[]);
        }
        assert!(s.records().is_empty());
        assert_eq!(s.events(&op).len(), 1, "exactly one terminal event");
    }
}

#[test]
fn commit_a_landed_record_is_settled_without_git() {
    for (name, reason) in [
        ("commit-landed-ext", "external_change"),
        ("commit-landed-moved", "head_moved"),
    ] {
        let s = Store::new(name, "skip");
        let ids = s.items();
        // Pause at `applied` to race the write, then die at `landed`.
        let race = Pause::new(&s.root, "applied");
        let landed = Pause {
            dir: race.dir.clone(),
            point: "landed",
        };
        let both = (
            "RIVET_TEST_PAUSE",
            format!("applied,landed:{}", race.dir.display()),
        );
        let request = s.set_request(&ids[0], "[x] **Ship.** Raced, then killed.");
        let mut writer = s.spawn("set", &request, &[both]);
        race.wait();
        if reason == "external_change" {
            let text = read(&s.root, WIDGETS) + "\nAn editor saved.\n";
            std::fs::write(s.root.join(WIDGETS), text).unwrap();
        } else {
            std::fs::write(s.root.join("notes.txt"), "moved\n").unwrap();
            s.git(&["commit", "-qm", "outside", "--", "notes.txt"]);
        }
        race.release();
        landed.wait();
        writer.kill().unwrap();
        writer.wait().unwrap();
        let rec = s.record();
        assert_eq!(rec["state"], "landed");
        assert_eq!(rec["reason"], reason);
        if reason == "external_change" {
            // The file has changed again since.
            std::fs::write(s.root.join(WIDGETS), "changed again\n").unwrap();
        } else {
            assert!(
                read(&s.root, WIDGETS).contains("Raced, then killed."),
                "the file is still the candidate"
            );
        }
        let base = s.head();
        let out = s.recover(json!({}), &[s.logging("quiet")]);
        assert_eq!(one_result(&out)["result"], "skipped", "{name}: {out}");
        assert!(s.logged("quiet").is_empty(), "{name}: git ran");
        assert_eq!(s.head(), base, "{name}: a settled skip was committed");
        let op = rec["op"].as_str().unwrap();
        assert_eq!(s.events(op)[0]["reason"], reason);
        assert!(s.records().is_empty());
    }
}

#[test]
fn commit_a_failing_git_lands_ineligible_and_survives_a_crash() {
    let s = Store::new("commit-git-error", "skip");
    let ids = s.items();
    let broken = s.script("broken", "echo \"$*\" >> $HOME_DIR/broken.log\nexit 1");
    let (code, out) = s.set(
        &ids[0],
        "[x] **Ship.** No git.",
        std::slice::from_ref(&broken),
    );
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["commit"]["status"], "ineligible");
    assert_eq!(out["commit"]["reason"], "git_error");
    // Killed before rename, then after it, with git still unavailable.
    for (point, want) in [
        ("pending_written", "not_applied"),
        ("after_rename", "ineligible"),
    ] {
        let request = s.set_request(&ids[1], &format!("[x] **Refresh.** {point}."));
        s.kill_at(point, "set", &request, std::slice::from_ref(&broken));
        let rec = s.record();
        assert_eq!(rec["outcome"], "ineligible");
        assert!(
            rec.get("base_head").is_none() && rec.get("candidate").is_none(),
            "{rec}"
        );
        let calls = s.logged("broken").lines().count();
        let out = s.recover(json!({}), std::slice::from_ref(&broken));
        assert_eq!(one_result(&out)["result"], want, "{point}: {out}");
        assert_eq!(
            s.logged("broken").lines().count(),
            calls,
            "recovery ran git"
        );
    }
    // The next write is not refused.
    let (code, out) = s.set(&ids[2], "[x] **Confirm.** After.", &[broken]);
    assert_eq!(code, 0, "{out}");
}

#[test]
fn commit_a_partial_last_receipt_line_is_truncated_before_the_next() {
    let s = Store::new("commit-partial", "skip");
    let ids = s.items();
    let (_, out) = s.set(&ids[0], "[x] **Ship.** One.", &[]);
    assert_eq!(out["commit"]["status"], "committed");
    let log = s.root.join(".rivet/commits.jsonl");
    let mut text = std::fs::read_to_string(&log).unwrap();
    text.push_str("{\"op\":\"half");
    std::fs::write(&log, &text).unwrap();
    assert_eq!(s.receipts().len(), 1, "readers ignore the partial line");
    let (_, out) = s.set(&ids[1], "[x] **Refresh.** Two.", &[]);
    assert_eq!(out["commit"]["status"], "committed");
    let text = std::fs::read_to_string(&log).unwrap();
    assert!(!text.contains("half"), "{text}");
    assert_eq!(s.receipts().len(), 2);
}

#[test]
fn recovery_finds_old_terminals_beyond_the_whole_log_bound() {
    for mode in ["all", "op", "abandon"] {
        let s = Store::new(&format!("receipt-tail-{mode}"), "skip");
        let ids = s.items();
        let (_, out) = s.set(
            &ids[0],
            "[x] **Ship.** Tail recovery.",
            &[("RIVET_TEST_FAULT", "receipt_sync".into())],
        );
        assert_eq!(out["receipt"], "failed", "{out}");
        let op = s.record()["op"].clone();
        let doc = read(&s.root, WIDGETS);
        let log = s.root.join(".rivet/commits.jsonl");
        let mut bytes = std::fs::read(&log).unwrap();
        // The terminal is older than the first scan block and the lowered
        // full-log cap. A long partial suffix is ignored without a line buffer.
        bytes.extend(b"{}\n".repeat(30000));
        bytes.extend(b"x".repeat(12000));
        std::fs::write(&log, &bytes).unwrap();
        let limit = ("RIVET_TEST_LOG_BYTES", "4096".into());
        let (_, status) = s.run(
            "status",
            json!({"paths":[WIDGETS]}),
            std::slice::from_ref(&limit),
        );
        assert!(
            status["diagnostics"]
                .as_array()
                .unwrap()
                .iter()
                .any(|d| d["code"] == "limit_exceeded"),
            "{status}"
        );
        let request = match mode {
            "op" => json!({"op":op}),
            "abandon" => json!({"abandon":op,"reason":"test"}),
            _ => json!({}),
        };
        let out = s.recover(request, &[limit]);
        assert_eq!(one_result(&out)["result"], "reconciled", "{out}");
        assert!(s.records().is_empty());
        assert_eq!(
            std::fs::read(&log).unwrap(),
            bytes,
            "reconciliation appended or truncated"
        );
        assert_eq!(
            read(&s.root, WIDGETS),
            doc,
            "recovery changed document bytes or stays"
        );
        assert_eq!(s.events(op.as_str().unwrap()).len(), 1);
    }
}

#[test]
fn recovery_trims_only_the_partial_suffix_and_serializes_competing_recoverers() {
    let s = Store::new("receipt-tail-append", "skip");
    let ids = s.items();
    let request = s.set_request(&ids[0], "[x] **Ship.** Missing receipt.");
    s.kill_at("committed", "set", &request, &[]);
    let op = s.record()["op"].as_str().unwrap().to_owned();
    let doc = read(&s.root, WIDGETS);
    let log = s.root.join(".rivet/commits.jsonl");
    let prefix = b"{}\n".repeat(30000);
    let mut bytes = prefix.clone();
    bytes.extend(b"x".repeat(12000));
    std::fs::write(&log, &bytes).unwrap();
    let pause = Pause::new(&s.root, "receipt_scan");
    let limit = ("RIVET_TEST_LOG_BYTES", "4096".into());
    let first = s.spawn("commit-pending", &json!({}), &[limit.clone(), pause.env()]);
    pause.wait();
    let mut second = s.spawn("commit-pending", &json!({}), &[limit]);
    wait_for_writer_lock(&s.root, &second);
    assert!(
        second.try_wait().unwrap().is_none(),
        "second recovery bypassed the lock"
    );
    pause.release();
    let (code, out) = finish(first);
    assert_eq!(code, 0, "{out}");
    assert_eq!(one_result(&out)["result"], "committed", "{out}");
    let (code, out) = finish(second);
    assert_eq!(code, 0, "{out}");
    assert!(out["records"].as_array().unwrap().is_empty(), "{out}");
    let after = std::fs::read(&log).unwrap();
    assert_eq!(&after[..prefix.len()], &prefix);
    let event: Value = serde_json::from_slice(&after[prefix.len()..]).unwrap();
    assert_eq!(event["op"], op);
    assert_eq!(event["outcome"], "committed");
    assert_eq!(s.events(&op).len(), 1);
    assert!(s.records().is_empty());
    assert_eq!(
        read(&s.root, WIDGETS),
        doc,
        "unrelated bytes or stays moved"
    );
}

#[test]
fn an_overlong_complete_receipt_line_refuses_recovery_without_changes() {
    let s = Store::new("receipt-tail-refuse", "skip");
    let ids = s.items();
    let request = s.set_request(&ids[0], "[x] **Ship.** Refused recovery.");
    s.kill_at("committed", "set", &request, &[]);
    let doc = read(&s.root, WIDGETS);
    let pending = s.record();
    let head = s.head();
    let log = s.root.join(".rivet/commits.jsonl");
    let bytes = format!("{}\n", "x".repeat(4097));
    std::fs::write(&log, &bytes).unwrap();
    let (code, out) = s.run(
        "commit-pending",
        json!({}),
        &[("RIVET_TEST_LOG_BYTES", "4096".into())],
    );
    assert_eq!(code, 2, "{out}");
    assert_eq!(out["error"]["code"], "limit_exceeded", "{out}");
    assert!(
        out["error"]["message"]
            .as_str()
            .unwrap()
            .contains("complete log line")
    );
    assert_eq!(read(&s.root, WIDGETS), doc);
    assert_eq!(s.record(), pending);
    assert_eq!(s.head(), head);
    assert_eq!(std::fs::read(&log).unwrap(), bytes.as_bytes());
}

#[test]
fn commit_a_failed_append_leaves_the_record_and_recovery_appends_once() {
    let s = Store::new("commit-append", "skip");
    let ids = s.items();
    let (code, out) = s.set(
        &ids[0],
        "[x] **Ship.** Append fails.",
        &[("RIVET_TEST_FAULT", "receipt_append".into())],
    );
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["receipt"], "failed", "{out}");
    assert_eq!(out["commit"]["status"], "committed");
    let op = s.record()["op"].as_str().unwrap().to_string();
    assert_eq!(out["commit"]["op"], op);
    assert!(s.events(&op).is_empty(), "the partial line is not an event");
    s.recover(json!({}), &[]);
    s.recover(json!({}), &[]);
    assert_eq!(s.events(&op).len(), 1);
    assert!(s.records().is_empty());
}

#[test]
fn commit_a_failed_sync_is_reconciled_not_appended_again() {
    let s = Store::new("commit-sync", "skip");
    let ids = s.items();
    let fault = ("RIVET_TEST_FAULT", "receipt_sync".to_string());
    let (_, out) = s.set(
        &ids[0],
        "[x] **Ship.** Sync fails.",
        std::slice::from_ref(&fault),
    );
    assert_eq!(out["receipt"], "failed", "{out}");
    let op = s.record()["op"].as_str().unwrap().to_string();
    assert_eq!(s.events(&op).len(), 1, "the whole line is in");
    // The sync fails again: the record stays.
    let out = s.recover(json!({}), &[fault]);
    assert_eq!(one_result(&out)["result"], "receipt_failed", "{out}");
    assert_eq!(s.records().len(), 1);
    let out = s.recover(json!({}), &[]);
    assert_eq!(one_result(&out)["result"], "reconciled", "{out}");
    s.recover(json!({}), &[]);
    assert!(s.records().is_empty());
    assert_eq!(s.events(&op).len(), 1, "never a second event");
}

#[test]
fn commit_third_party_bytes_end_applied_unknown() {
    let s = Store::new("commit-third", "skip");
    let ids = s.items();
    for point in ["pending_written", "after_rename"] {
        // The owner's skipped write is the path's last write.
        s.git(&["checkout", "--", WIDGETS]);
        dirty(&s);
        let mut request = s.set_request(&ids[0], &format!("[x] **Ship.** Owned before {point}."));
        request["session"] = json!("owner");
        let (_, out) = s.run("set", request, &[]);
        assert_eq!(out["commit"]["status"], "skipped", "{out}");
        let before = s.run("status", json!({"paths": [WIDGETS]}), &[]).1;
        // A non-commit write killed before or after its rename, then bytes
        // nobody wrote through Rivet.
        let mut request = s.set_request(&ids[1], &format!("[x] **Refresh.** {point}."));
        request["session"] = json!("other");
        s.kill_at(point, "set", &request, &[]);
        assert_eq!(s.record()["outcome"], "skipped");
        std::fs::write(s.root.join(WIDGETS), format!("third party {point}\n")).unwrap();
        let out = s.recover(json!({}), &[]);
        assert_eq!(one_result(&out)["applied"], "unknown", "{point}: {out}");
        let after = s.run("status", json!({"paths": [WIDGETS]}), &[]).1;
        assert_eq!(
            after["paths"][0]["last_write"], before["paths"][0]["last_write"],
            "{point}"
        );
        assert_eq!(after["paths"][0]["owner"], "receipt:owner", "{point}");
        assert_eq!(after["receipts"]["writes"], before["receipts"]["writes"]);
    }
    let (_, status) = s.run("status", json!({}), &[]);
    assert_eq!(status["receipts"]["unknown_by_session"]["other"], 2);
}

#[test]
fn commit_a_later_filter_attribute_makes_recovery_require_repair() {
    let s = Store::new("commit-filter", "skip");
    let ids = s.items();
    let request = s.set_request(&ids[0], "[x] **Ship.** Before the attribute.");
    s.kill_at("after_rename", "set", &request, &[]);
    std::fs::write(
        s.root.join(".gitattributes"),
        "tracking/*.md filter=lfsish\n",
    )
    .unwrap();
    s.git(&["add", ".gitattributes"]);
    s.git(&["commit", "-qm", "attributes", "--", ".gitattributes"]);
    let base = s.head();
    let out = s.recover(json!({}), &[]);
    let r = one_result(&out);
    assert_eq!(r["result"], "repair_required", "{out}");
    assert_eq!(r["reason"], "git_conversion");
    assert_eq!(s.head(), base);
    assert_eq!(s.records().len(), 1);
}

#[test]
fn commit_records_candidate_when_an_editor_saves_after_reread() {
    for kind in ["write", "create", "adopt"] {
        let s = Store::new(&format!("commit-recorded-{kind}"), "skip");
        let ids = s.items();
        let (command, path, request) = match kind {
            "create" => ("create", NEW, create_request(NEW_CONTENT)),
            "adopt" => {
                std::fs::write(s.root.join(NEW), NEW_CONTENT).unwrap();
                ("adopt", NEW, json!({"path": NEW}))
            }
            _ => (
                "set",
                WIDGETS,
                s.set_request(&ids[0], "[x] **Ship.** Recorded."),
            ),
        };
        // An unrelated staged change must survive without joining this commit.
        std::fs::write(s.root.join("notes.txt"), "staged elsewhere\n").unwrap();
        s.git(&["add", "notes.txt"]);
        let other_entry = s.git(&["ls-files", "-s", "--", "notes.txt"]);
        let base = s.head();
        let pause = Pause::new(&s.root, "reread");
        let writer = s.spawn(command, &request, &[pause.env()]);
        pause.wait();
        let candidate = read(&s.root, path);
        let (_, before) = rivet(
            &s.root,
            "query",
            json!({"collection":"actions", "paths":[path]}),
        );
        let foreign = format!("{candidate}\nOutside editor bytes.\n");
        std::fs::write(s.root.join(path), &foreign).unwrap();
        pause.release();
        let (code, out) = finish(writer);
        assert_eq!(code, 0, "{kind}: {out}");
        assert_eq!(
            s.git(&["show", &format!("HEAD:{path}")]),
            candidate,
            "{kind}: foreign bytes committed"
        );
        assert_eq!(
            read(&s.root, path),
            foreign,
            "{kind}: editor bytes overwritten"
        );
        assert_eq!(out["commit"]["status"], "committed", "{kind}: {out}");
        assert_eq!(out["commit"]["reason"], "external_change", "{kind}: {out}");
        assert_eq!(s.since(&base).len(), 1);
        assert_eq!(s.changed(&s.head()), [path]);
        assert_eq!(s.git(&["ls-files", "-s", "--", "notes.txt"]), other_entry);
        assert_eq!(s.git(&["show", "HEAD:notes.txt"]), "notes\n");
        // Stays still address the same logical items after the foreign append.
        let (_, after) = rivet(
            &s.root,
            "query",
            json!({"collection":"actions", "paths":[path]}),
        );
        let identities = |v: &Value| {
            v["items"]
                .as_array()
                .unwrap()
                .iter()
                .map(|i| (i["id"].clone(), i["fields"]["title"].clone()))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            identities(&before),
            identities(&after),
            "{kind}: stay attachment changed"
        );
        assert!(s.records().is_empty());
        assert_eq!(s.receipts().last().unwrap()["reason"], "external_change");
    }
}

#[test]
fn commit_an_unrecognized_ref_handshake_cannot_stage_the_candidate() {
    let s = Store::new("commit-ref-handshake", "skip");
    let ids = s.items();
    let base = s.head();
    let index = std::fs::read(s.root.join(".git/index")).unwrap();
    let wrapper = s.script(
        "unprepared",
        r#"case "$*" in *" update-ref --stdin "*)
printf 'start: ok\nprepare: unknown\n'
cat >/dev/null
exit 0;;
esac
exec git "$@""#,
    );
    let (_, out) = s.set(&ids[0], "[x] **Ship.** Unsupported handshake.", &[wrapper]);
    assert_eq!(out["commit"]["status"], "failed", "{out}");
    assert_eq!(out["commit"]["published"], false);
    assert_eq!(s.head(), base);
    assert_eq!(std::fs::read(s.root.join(".git/index")).unwrap(), index);
    let rec = s.record();
    let out = s.recover(json!({}), &[]);
    assert_eq!(one_result(&out)["result"], "committed", "{out}");
    assert_eq!(s.carrying(rec["op"].as_str().unwrap()).len(), 1);
}

#[test]
fn commit_a_lost_ref_cas_preserves_the_winner() {
    let s = Store::new("commit-cas-loss", "skip");
    let ids = s.items();
    let base = s.head();
    // Construct an independent commit before Rivet locks the real index.
    std::fs::write(s.root.join("notes.txt"), "winner\n").unwrap();
    s.git(&["add", "notes.txt"]);
    let tree = s.git(&["write-tree"]);
    let winner = s.git(&[
        "commit-tree",
        tree.trim(),
        "-p",
        &base,
        "-m",
        "independent winner",
    ]);
    s.git(&["reset", "-q", &base, "--", "notes.txt"]);
    let pause = Pause::new(&s.root, "before_ref_update");
    let request = s.set_request(&ids[0], "[x] **Ship.** CAS loser.");
    let writer = s.spawn("set", &request, &[pause.env()]);
    pause.wait();
    let candidate = read(&s.root, WIDGETS);
    // Raw ref updates do not take index.lock. CAS must not overwrite this one.
    s.git(&["update-ref", "refs/heads/main", winner.trim(), &base]);
    pause.release();
    let (_, out) = finish(writer);
    assert_eq!(s.head(), winner.trim());
    assert_eq!(out["commit"]["status"], "skipped", "{out}");
    assert_eq!(out["commit"]["reason"], "head_moved", "{out}");
    assert_eq!(read(&s.root, WIDGETS), candidate);
    assert_eq!(s.since(&base).len(), 1);
    assert!(s.records().is_empty());
}

#[test]
fn commit_ref_preparation_blocks_checkout_and_index_installation_blocks_commit() {
    for point in ["ref_prepared", "ref_updated"] {
        let s = Store::new(&format!("commit-publication-lock-{point}"), "skip");
        let ids = s.items();
        let pause = Pause::new(&s.root, point);
        let request = s.set_request(&ids[0], "[x] **Ship.** Locked publication.");
        let writer = s.spawn("set", &request, &[pause.env()]);
        pause.wait();
        let candidate = read(&s.root, WIDGETS);
        let head = s.head();
        let (ok, _) = s.git_try(&["checkout", "--detach", &head]);
        assert!(!ok, "{point}: checkout crossed publication");
        let (ok, _) = s.git_try(&["commit", "--allow-empty", "-qm", "competitor"]);
        assert!(!ok, "{point}: commit saw an intermediate index");
        pause.release();
        let (_, out) = finish(writer);
        assert_eq!(out["commit"]["status"], "committed", "{out}");
        assert_eq!(s.git(&["show", &format!("HEAD:{WIDGETS}")]), candidate);
        assert!(s.clean());
    }
}

#[test]
fn commit_killed_with_a_spanning_index_lock_recovers_with_stable_evidence() {
    for point in ["index_prepared", "ref_updated"] {
        let s = Store::new(&format!("commit-index-lease-{point}"), "skip");
        let ids = s.items();
        let request = s.set_request(&ids[0], "[x] **Ship.** Stable lock.");
        s.kill_at(point, "set", &request, &[]);
        let candidate = read(&s.root, WIDGETS);
        let rec = s.record();
        assert!(rec["index_lease"].is_object());
        let out = s.recover(json!({}), &[]);
        assert_eq!(one_result(&out)["reason"], "stale_git_lock", "{out}");
        let identity = s.identity(&out, ".git/index.lock");
        let out = s.recover(
            json!({"op":rec["op"],"clear_lock":".git/index.lock","identity":identity}),
            &[],
        );
        assert_eq!(one_result(&out)["result"], "committed", "{point}: {out}");
        assert_eq!(s.carrying(rec["op"].as_str().unwrap()).len(), 1);
        assert_eq!(s.git(&["show", &format!("HEAD:{WIDGETS}")]), candidate);
        assert!(s.clean());
    }
}

#[test]
fn commit_legacy_recovery_stores_an_absent_candidate_object() {
    let s = Store::new("commit-legacy-object", "skip");
    let ids = s.items();
    let request = s.set_request(&ids[0], "[x] **Ship.** Legacy object.");
    s.kill_at("applied", "set", &request, &[]);
    let rec = s.record();
    let blob = rec["candidate"]["oid"].as_str().unwrap();
    // Emulate a pre-upgrade record: its dry hash never wrote the loose object.
    std::fs::remove_file(
        s.root
            .join(".git/objects")
            .join(&blob[..2])
            .join(&blob[2..]),
    )
    .unwrap();
    assert!(
        !s.git_try(&["cat-file", "-e", blob]).0,
        "fixture object must be absent"
    );
    let out = s.recover(json!({}), &[]);
    assert_eq!(one_result(&out)["result"], "committed", "{out}");
    assert_eq!(s.carrying(rec["op"].as_str().unwrap()).len(), 1);
    assert!(s.clean());
}

#[test]
fn commit_keeps_racy_unrelated_worktree_edits_visible_to_git() {
    let s = Store::new("commit-racy-index", "skip");
    s.git(&["config", "core.trustctime", "false"]);
    let notes = s.root.join("notes.txt");
    let timestamp = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_600_000_000);
    std::fs::File::options()
        .write(true)
        .open(&notes)
        .unwrap()
        .set_times(std::fs::FileTimes::new().set_modified(timestamp))
        .unwrap();
    s.git(&["add", "notes.txt"]);
    // Make the cached stat data ambiguous: same size/mtime, changed contents.
    std::fs::File::options()
        .write(true)
        .open(s.root.join(".git/index"))
        .unwrap()
        .set_times(std::fs::FileTimes::new().set_modified(timestamp))
        .unwrap();
    std::fs::write(&notes, "dirty\n").unwrap();
    std::fs::File::options()
        .write(true)
        .open(&notes)
        .unwrap()
        .set_times(std::fs::FileTimes::new().set_modified(timestamp))
        .unwrap();
    assert_eq!(
        s.git(&["diff-files", "--name-only", "--", "notes.txt"]),
        "notes.txt\n",
        "negative-control fixture must expose the unrelated edit before Rivet"
    );
    let ids = s.items();
    let (_, out) = s.set(&ids[0], "[x] **Ship.** Preserve stat-cache ambiguity.", &[]);
    assert_eq!(out["commit"]["status"], "committed", "{out}");
    assert_eq!(
        s.git(&["diff-files", "--name-only", "--", "notes.txt"]),
        "notes.txt\n",
        "copying the index hid an unrelated worktree edit"
    );
    assert_eq!(read(&s.root, "notes.txt"), "dirty\n");
    assert_eq!(s.git(&["show", "HEAD:notes.txt"]), "notes\n");
}

#[test]
fn commit_an_index_sync_failure_is_published_pending_and_recoverable() {
    for fault in ["index_sync", "index_directory_sync"] {
        let s = Store::new(&format!("commit-{fault}"), "skip");
        let ids = s.items();
        let base = s.head();
        let (_, out) = s.set(
            &ids[0],
            "[x] **Ship.** Sync failure.",
            &[("RIVET_TEST_FAULT", fault.into())],
        );
        assert_eq!(out["commit"]["status"], "failed", "{fault}: {out}");
        assert_eq!(out["commit"]["reason"], "index_changed");
        assert_eq!(out["commit"]["published"], true);
        assert_eq!(s.since(&base).len(), 1);
        let rec = s.record();
        // A visible index is insufficient: failed recovery sync stays pending too.
        let out = s.recover(json!({}), &[("RIVET_TEST_FAULT", fault.into())]);
        assert_eq!(
            one_result(&out)["result"],
            "repair_required",
            "{fault}: {out}"
        );
        assert_eq!(s.records().len(), 1);
        let out = s.recover(json!({}), &[]);
        assert_eq!(one_result(&out)["result"], "committed", "{fault}: {out}");
        assert_eq!(s.carrying(rec["op"].as_str().unwrap()).len(), 1);
        assert!(s.clean());
    }
}

#[test]
fn commit_comment_stripping_settings_keep_the_trailer() {
    let s = Store::new("commit-cleanup", "skip");
    s.git(&["config", "core.commentChar", "R"]);
    s.git(&["config", "commit.cleanup", "strip"]);
    let ids = s.items();
    let (_, out) = s.set(&ids[0], "[x] **Ship.** Verbatim.", &[]);
    assert_eq!(out["commit"]["status"], "committed", "{out}");
    let sha = s.head();
    assert!(
        s.message(&sha).contains("Rivet-Op: "),
        "{}",
        s.message(&sha)
    );
}

#[test]
fn commit_an_index_left_at_the_base_is_repaired() {
    let s = Store::new("commit-half", "skip");
    let ids = s.items();
    let (code, out) = s.set(&ids[0], "[x] **Ship.** Half.", &[s.half_commit()]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["commit"]["status"], "failed", "{out}");
    assert_eq!(out["commit"]["published"], true);
    let out = s.recover(json!({}), &[]);
    assert_eq!(one_result(&out)["result"], "committed", "{out}");
    assert!(s.clean());
}

#[test]
fn commit_inspection_failing_after_a_commit_is_unverified_then_recovered() {
    let s = Store::new("commit-unverified", "skip");
    let ids = s.items();
    let blind = s.script(
        "blind",
        "case \"$*\" in *\" update-ref --stdin \"*) git \"$@\"; rc=$?; touch $HOME_DIR/done; exit $rc;; esac\nif [ -e $HOME_DIR/done ]; then exit 128; fi\nexec git \"$@\"",
    );
    let (code, out) = s.set(&ids[0], "[x] **Ship.** Blind.", &[blind]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["commit"]["status"], "failed");
    assert_eq!(out["commit"]["reason"], "unverified");
    assert_eq!(out["commit"]["published"], "unknown");
    assert_eq!(s.record()["state"], "unverified");
    let op = s.record()["op"].as_str().unwrap().to_string();
    let out = s.recover(json!({}), &[]);
    assert_eq!(one_result(&out)["result"], "committed", "{out}");
    assert_eq!(s.carrying(&op).len(), 1);
}

#[test]
fn commit_a_write_that_is_not_durable_is_committed_by_recovery() {
    let s = Store::new("commit-not-durable", "skip");
    let ids = s.items();
    let (code, out) = s.set(
        &ids[0],
        "[x] **Ship.** Not durable.",
        &[("RIVET_TEST_FAULT", "dir_sync".into())],
    );
    assert_eq!(code, 3, "{out}");
    assert_eq!(out["durable"], false);
    assert_eq!(out["commit"]["status"], "pending");
    let pending_op = out["commit"]["op"].clone();
    assert_eq!(pending_op, out["commit"]["pending"]);
    let rec = s.record();
    assert_eq!(pending_op, rec["op"]);
    assert_eq!(rec["state"], "prepared");
    assert!(s.receipts().is_empty());
    let out = s.recover(json!({}), &[]);
    assert_eq!(one_result(&out)["result"], "committed", "{out}");
    assert_eq!(s.carrying(rec["op"].as_str().unwrap()).len(), 1);
    assert_eq!(pending_op, s.receipts()[0]["op"]);
}

#[test]
fn commit_off_or_absent_changes_no_write_output_and_no_bookkeeping() {
    // Seeded ids make two runs comparable byte for byte.
    let seed = [("RIVET_TEST_ID_SEED", "same".to_string())];
    let mut runs = vec![];
    for git in [None, Some(json!({"commit": "off", "on_dirty": "refuse"}))] {
        let root = project("commit-off");
        if let Some(git) = &git {
            let path = root.join(".rivet/project.json");
            let mut config: Value =
                serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
            config["git"] = git.clone();
            std::fs::write(&path, serde_json::to_string_pretty(&config).unwrap()).unwrap();
        }
        let run = |cmd: &str, request: Value| finish(spawn(&root, cmd, &request, &seed));
        let mut outputs = vec![];
        let (_, map) = run("map", json!({"path": WIDGETS}));
        let stamped = run(
            "stamp",
            json!({"path": WIDGETS, "guard": map["revision"], "session": "s", "message": "m"}),
        );
        outputs.push(stamped);
        let (_, items) = run(
            "query",
            json!({"collection": "actions", "paths": [WIDGETS]}),
        );
        let id = items["items"][0]["id"].as_str().unwrap().to_string();
        let guard = item_guard(&root, WIDGETS, &id);
        outputs.push(run("set", set_item(&id, &guard, "[x] **Ship.** Off.")));
        let (_, map) = run("map", json!({"path": WIDGETS}));
        outputs.push(run(
            "insert-item",
            json!({"path": WIDGETS, "collection": "actions", "position": "end",
                   "guard": map["collections"][0]["revision"],
                   "fields": {"title": "Added", "done": false}}),
        ));
        let (_, map) = run("map", json!({"path": WIDGETS}));
        outputs.push(run(
            "write",
            json!({"path": WIDGETS, "message": "batch", "ops": [{"op": "stamp", "select": {"kind": "document"}, "guard": map["revision"]}]}),
        ));
        for name in ["seq", "pending", "commits.jsonl"] {
            assert!(!root.join(".rivet").join(name).exists(), "{name}");
        }
        for (code, body) in &outputs {
            assert_eq!((*code, &body["status"]), (0, &json!("applied")), "{body}");
            assert!(body.get("commit").is_none(), "{body}");
        }
        runs.push((outputs, read(&root, WIDGETS)));
    }
    assert_eq!(runs[0], runs[1]);
}

/// Every write entry point once, through `bin`, with seeded ids, in a fresh
/// copy of the example. Returns each response and the final file.
fn write_sequence(bin: &Path, root: &Path) -> Value {
    let run = |cmd: &str, request: Value| -> Value {
        let mut command = std::process::Command::new(bin);
        let mut child = common::without_debug_hooks(&mut command)
            .arg("--root")
            .arg(root)
            .arg(cmd)
            .env("RIVET_TEST_ID_SEED", "compat")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        use std::io::Write;
        child
            .stdin
            .take()
            .unwrap()
            .write_all(request.to_string().as_bytes())
            .unwrap();
        let out = child.wait_with_output().unwrap();
        json!([
            out.status.code(),
            serde_json::from_slice::<Value>(&out.stdout).unwrap()
        ])
    };
    let get = |select: Value| run("get", json!({"path": WIDGETS, "select": select}))[1].clone();
    let mut outputs = vec![];
    let map = run("map", json!({"path": WIDGETS}))[1].clone();
    outputs.push(run(
        "stamp",
        json!({"path": WIDGETS, "guard": map["revision"]}),
    ));
    let items = run(
        "query",
        json!({"collection": "actions", "paths": [WIDGETS]}),
    )[1]["items"]
        .clone();
    let id = |n: usize| items[n]["id"].clone();
    let select = |n: usize| json!({"kind": "id", "id": id(n)});
    outputs.push(run(
        "set",
        json!({"path": WIDGETS, "select": select(0), "guard": get(select(0))["revision"],
               "content": "[x] **Ship the widget.** Compatible."}),
    ));
    let typed = get(select(1));
    outputs.push(run(
        "update-item",
        json!({"path": WIDGETS, "collection": "actions", "id": id(1),
               "guard": typed["typed"]["actions"]["revision"], "set": {"done": true}}),
    ));
    let log = json!({"kind": "heading_path", "path": ["Log"]});
    outputs.push(run(
        "insert",
        json!({"path": WIDGETS, "select": log, "position": "start",
               "guard": get(log.clone())["revision"], "content": "### 2026-09-05: compatible\n\nChecked.\n", "stamp": true}),
    ));
    outputs.push(run(
        "delete",
        json!({"path": WIDGETS, "select": select(2), "guard": get(select(2))["revision"]}),
    ));
    let map = run("map", json!({"path": WIDGETS}))[1].clone();
    outputs.push(run(
        "insert-item",
        json!({"path": WIDGETS, "collection": "actions", "position": "end",
               "guard": map["collections"][0]["revision"],
               "fields": {"title": "Added", "done": false}}),
    ));
    let map = run("map", json!({"path": WIDGETS}))[1].clone();
    outputs.push(run(
        "write",
        json!({"path": WIDGETS, "ops": [{"op": "stamp", "select": {"kind": "document"},
               "guard": map["revision"]}]}),
    ));
    json!({"outputs": outputs, "file": read(root, WIDGETS)})
}

const COMPAT: &str = "tests/fixtures/write_outputs_0b7fbc1.json";

/// Without the `git` key, and with `commit: "off"`, every write entry point
/// keeps the bytes and responses from before per-write commits, apart from
/// typed guards gaining the binding fingerprint.
/// The fixture was captured from that build (0b7fbc1); regenerate it only
/// from that build, with `RIVET_COMPAT_BIN` set, through the ignored test
/// below.
#[test]
fn commit_off_or_absent_matches_the_build_before_commits() {
    let want: Value = serde_json::from_str(
        &std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join(COMPAT)).unwrap(),
    )
    .unwrap();
    for git in [None, Some(json!({"commit": "off", "on_dirty": "refuse"}))] {
        let root = project("commit-compat");
        if let Some(git) = &git {
            let path = root.join(".rivet/project.json");
            let mut config: Value =
                serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
            config["git"] = git.clone();
            std::fs::write(&path, serde_json::to_string_pretty(&config).unwrap()).unwrap();
        }
        let got = write_sequence(Path::new(env!("CARGO_BIN_EXE_markrivet")), &root);
        for (n, out) in got["outputs"].as_array().unwrap().iter().enumerate() {
            assert_eq!(out[0], 0, "{git:?} step {n}: {out}");
        }
        // Typed guard inputs intentionally gain the binding fingerprint.
        // Keep the historical fixture and compare every other output byte.
        let mut comparable = got.clone();
        let mut expected = want.clone();
        let mut changed = 0;
        for (got, old) in comparable["outputs"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .zip(expected["outputs"].as_array_mut().unwrap())
        {
            for (got, old) in got[1]["ops"]
                .as_array_mut()
                .unwrap()
                .iter_mut()
                .zip(old[1]["ops"].as_array_mut().unwrap())
            {
                for key in ["revision", "collection_revision"] {
                    if old[key].as_str().is_some_and(|s| {
                        s.starts_with("rivet:v1:action:") || s.starts_with("rivet:v1:collection:")
                    }) {
                        assert_ne!(got[key], old[key]);
                        got[key] = json!("<typed revision>");
                        old[key] = json!("<typed revision>");
                        changed += 1;
                    }
                }
            }
        }
        assert_eq!(changed, 4, "the exception covers exactly two typed writes");
        assert_eq!(comparable, expected, "{git:?}");
        for name in ["seq", "pending", "commits.jsonl"] {
            assert!(!root.join(".rivet").join(name).exists(), "{name}");
        }
    }
}

#[test]
#[ignore = "regenerates the compatibility fixture from RIVET_COMPAT_BIN"]
fn regenerate_the_write_compatibility_fixture() {
    let bin = std::env::var("RIVET_COMPAT_BIN").expect("RIVET_COMPAT_BIN names the 0b7fbc1 build");
    let root = project("commit-compat-gen");
    let got = write_sequence(Path::new(&bin), &root);
    std::fs::write(
        Path::new(env!("CARGO_MANIFEST_DIR")).join(COMPAT),
        serde_json::to_string_pretty(&got).unwrap() + "\n",
    )
    .unwrap();
}

#[test]
fn commit_a_skip_whose_bookkeeping_fails_says_its_commit_is_still_intended() {
    let s = Store::new("commit-landed-fail", "skip");
    let ids = s.items();
    let pause = Pause::new(&s.root, "applied");
    let request = s.set_request(&ids[0], "[x] **Ship.** Moved, bookkeeping fails.");
    let writer = s.spawn(
        "set",
        &request,
        &[
            pause.env(),
            ("RIVET_TEST_FAULT", "landed_publish,receipt_append".into()),
        ],
    );
    pause.wait();
    std::fs::write(s.root.join("notes.txt"), "moved\n").unwrap();
    s.git(&["commit", "-qm", "outside", "--", "notes.txt"]);
    pause.release();
    let (code, out) = finish(writer);
    assert_eq!(code, 0, "{out}");
    // Not reported as a settled skip: nothing durable says it was one.
    assert_eq!(out["commit"]["status"], "failed", "{out}");
    assert_eq!(out["commit"]["reason"], "bookkeeping");
    assert_eq!(
        out["commit"]["intended"],
        json!({"status":"skipped", "reason":"head_moved"})
    );
    let pending_op = out["commit"]["op"].clone();
    let rec = s.record();
    assert_eq!(rec["outcome"], "commit");
    assert_eq!(out["commit"]["pending"], rec["op"]);
    // Recovery treats it as the commit its record still intends.
    let out = s.recover(json!({}), &[]);
    assert_eq!(one_result(&out)["result"], "committed", "{out}");

    assert_eq!(pending_op, s.receipts()[0]["op"]);

    // Publication failing, then a crash before the receipt: the same.
    let pause = Pause::new(&s.root, "applied");
    let landed = Pause {
        dir: pause.dir.clone(),
        point: "landed",
    };
    let both = (
        "RIVET_TEST_PAUSE",
        format!("applied,landed:{}", pause.dir.display()),
    );
    let request = s.set_request(&ids[1], "[x] **Refresh.** Publish fails, then a crash.");
    let mut writer = s.spawn(
        "set",
        &request,
        &[both, ("RIVET_TEST_FAULT", "landed_publish".into())],
    );
    pause.wait();
    std::fs::write(s.root.join("notes.txt"), "moved again\n").unwrap();
    s.git(&["commit", "-qm", "outside again", "--", "notes.txt"]);
    pause.release();
    landed.wait();
    writer.kill().unwrap();
    writer.wait().unwrap();
    let rec = s.record();
    assert_eq!(rec["state"], "applied");
    assert_eq!(rec["outcome"], "commit");
    let out = s.recover(json!({}), &[]);
    assert_eq!(one_result(&out)["result"], "committed", "{out}");
    assert_eq!(s.carrying(rec["op"].as_str().unwrap()).len(), 1);
}

#[test]
fn commit_a_skip_whose_receipt_is_durable_stays_a_skip_when_cleanup_fails() {
    let s = Store::new("commit-retire-fail", "skip");
    let ids = s.items();
    let pause = Pause::new(&s.root, "applied");
    let request = s.set_request(&ids[0], "[x] **Ship.** Receipt in, record left.");
    let writer = s.spawn(
        "set",
        &request,
        &[
            pause.env(),
            ("RIVET_TEST_FAULT", "landed_publish,record_retire".into()),
        ],
    );
    pause.wait();
    std::fs::write(s.root.join("notes.txt"), "moved\n").unwrap();
    s.git(&["commit", "-qm", "outside", "--", "notes.txt"]);
    pause.release();
    let (code, out) = finish(writer);
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["commit"]["status"], "skipped", "{out}");
    assert_eq!(out["commit"]["reason"], "head_moved");
    assert!(out.get("receipt").is_none(), "{out}");
    let rec = s.record();
    let op = rec["op"].as_str().unwrap().to_string();
    assert_eq!(rec["outcome"], "commit", "the landed publication failed");
    let base = s.head();
    // Reconciling syncs the receipt; a removal that fails again leaves the
    // record without calling the receipt failed.
    let out = s.recover(json!({}), &[("RIVET_TEST_FAULT", "record_retire".into())]);
    let r = one_result(&out);
    assert_eq!(r["result"], "reconciled", "{out}");
    assert_eq!(r["record_left"], true);
    assert_eq!(s.records().len(), 1);
    let out = s.recover(json!({}), &[]);
    assert_eq!(one_result(&out)["result"], "reconciled", "{out}");
    assert_eq!(s.head(), base, "a settled skip was committed");
    let events = s.events(&op);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["outcome"], "skipped");
    assert!(s.records().is_empty());
}

#[test]
fn commit_an_abandonment_whose_receipt_is_durable_is_reported_settled() {
    let s = Store::new("commit-abandon-retire", "skip");
    let ids = s.items();
    let request = s.set_request(&ids[0], "[x] **Ship.** Abandoned, record left.");
    s.kill_at("after_rename", "set", &request, &[]);
    let op = s.record()["op"].clone();
    let base = s.head();
    let (code, out) = s.run(
        "commit-pending",
        json!({"abandon": op, "reason": "test"}),
        &[("RIVET_TEST_FAULT", "record_retire".into())],
    );
    assert_eq!(code, 0, "{out}");
    let r = one_result(&out);
    assert_eq!(r["result"], "abandoned");
    assert_eq!(r["applied"], true);
    assert_eq!(r["record_left"], true);
    let out = s.recover(json!({}), &[]);
    assert_eq!(one_result(&out)["result"], "reconciled", "{out}");
    let events = s.events(op.as_str().unwrap());
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["outcome"], "abandoned");
    assert_eq!(s.head(), base, "an abandoned write was committed");
    assert!(s.records().is_empty());
}

#[test]
fn commit_a_recovered_commit_whose_record_stays_says_so() {
    let s = Store::new("commit-recover-left", "skip");
    let ids = s.items();
    let request = s.set_request(&ids[0], "[x] **Ship.** Recovered, record left.");
    s.kill_at("after_rename", "set", &request, &[]);
    let op = s.record()["op"].as_str().unwrap().to_string();
    let out = s.recover(json!({}), &[("RIVET_TEST_FAULT", "record_retire".into())]);
    let r = one_result(&out);
    assert_eq!(r["result"], "committed", "{out}");
    assert_eq!(r["record_left"], true, "{out}");
    assert!(r.get("receipt").is_none(), "{out}");
    assert_eq!(out["unresolved"], 1);
    let out = s.recover(json!({}), &[]);
    assert_eq!(one_result(&out)["result"], "reconciled", "{out}");
    assert_eq!(s.events(&op).len(), 1);
    assert_eq!(s.carrying(&op).len(), 1);
}

#[test]
fn commit_recovery_never_commits_on_a_branch_switched_under_it() {
    let s = Store::new("commit-branch-switch", "skip");
    let ids = s.items();
    let request = s.set_request(&ids[0], "[x] **Ship.** Before the switch.");
    s.kill_at("after_rename", "set", &request, &[]);
    s.git(&["branch", "other"]);
    // Switch HEAD to `other`, at the same commit, as recovery starts its
    // fresh preflight (`git version` is its first call).
    let switch = s.script(
        "switch",
        "case \"$*\" in *\" version\") if [ -e $HOME_DIR/armed ]; then rm $HOME_DIR/armed; git symbolic-ref HEAD refs/heads/other; fi;; esac\nexec git \"$@\"",
    );
    std::fs::write(s.home.join("armed"), "").unwrap();
    let main = s.git(&["rev-parse", "main"]);
    let out = s.recover(json!({}), &[switch]);
    let r = one_result(&out);
    assert_eq!(r["result"], "repair_required", "{out}");
    assert_eq!(r["reason"], "target_changed");
    assert!(!s.home.join("armed").exists(), "the switch never ran");
    assert_eq!(s.git(&["rev-parse", "main"]), main);
    assert_eq!(
        s.git(&["rev-parse", "other"]),
        main,
        "committed on the other branch"
    );
    assert_eq!(s.records().len(), 1);
}

#[test]
fn commit_recovery_keeps_a_confirmed_application() {
    let s = Store::new("commit-sticky", "skip");
    let ids = s.items();
    let request = s.set_request(&ids[0], "[x] **Ship.** Confirmed by recovery.");
    s.kill_at("after_rename", "set", &request, &[]);
    assert_eq!(s.record()["ever_applied"], false);
    // Recovery commits it, but its receipt cannot be appended.
    let out = s.recover(json!({}), &[("RIVET_TEST_FAULT", "receipt_append".into())]);
    let r = one_result(&out);
    assert_eq!(r["result"], "committed", "{out}");
    assert_eq!(r["receipt"], "failed", "a receipt failure is reported");
    let rec = s.record();
    assert_eq!(rec["state"], "committed");
    assert_eq!(rec["ever_applied"], true);
    // Third-party bytes, then abandonment: the confirmed write stays a write.
    std::fs::write(s.root.join(WIDGETS), "third party\n").unwrap();
    let out = s.recover(json!({"abandon": rec["op"], "reason": "test"}), &[]);
    assert_eq!(one_result(&out)["applied"], true, "{out}");
    let (_, status) = s.run("status", json!({"paths": [WIDGETS]}), &[]);
    assert_eq!(status["paths"][0]["last_write"]["op"], rec["op"]);
}

#[test]
fn commit_a_receipt_failure_during_recovery_is_reported_and_settled_once() {
    let s = Store::new("commit-recover-receipt", "skip");
    let ids = s.items();
    // Discovery: the transaction committed before Rivet was killed.
    let request = s.set_request(&ids[0], "[x] **Ship.** Discovered.");
    s.kill_at("committed", "set", &request, &[]);
    let op = s.record()["op"].as_str().unwrap().to_string();
    let out = s.recover(json!({}), &[("RIVET_TEST_FAULT", "receipt_append".into())]);
    assert_eq!(one_result(&out)["receipt"], "failed", "{out}");
    assert_eq!(s.records().len(), 1);
    let out = s.recover(json!({}), &[]);
    assert_eq!(one_result(&out)["result"], "committed", "{out}");
    assert_eq!(s.events(&op).len(), 1);
    // Replay: committed on the current HEAD by recovery itself.
    let request = s.set_request(&ids[1], "[x] **Refresh.** Replayed.");
    s.kill_at("after_rename", "set", &request, &[]);
    let op = s.record()["op"].as_str().unwrap().to_string();
    let out = s.recover(json!({}), &[("RIVET_TEST_FAULT", "receipt_sync".into())]);
    assert_eq!(one_result(&out)["receipt"], "failed", "{out}");
    let out = s.recover(json!({}), &[]);
    assert_eq!(one_result(&out)["result"], "reconciled", "{out}");
    assert_eq!(s.events(&op).len(), 1);
    assert_eq!(s.carrying(&op).len(), 1);
}

#[test]
fn commit_an_index_is_not_repaired_after_later_commits_touched_the_path() {
    let s = Store::new("commit-no-repair", "skip");
    let ids = s.items();
    let base = s.git(&["ls-files", "-s", WIDGETS]);
    let base_blob = base.split(' ').nth(1).unwrap().to_string();
    let request = s.set_request(&ids[0], "[x] **Ship.** Committed, then history moved.");
    s.kill_at("committed", "set", &request, &[]);
    let candidate = read(&s.root, WIDGETS);
    // Later commits change the tracker and put the candidate back.
    std::fs::write(s.root.join(WIDGETS), format!("{candidate}\nChanged.\n")).unwrap();
    s.git(&["commit", "-qm", "change", "--", WIDGETS]);
    std::fs::write(s.root.join(WIDGETS), &candidate).unwrap();
    s.git(&["commit", "-qm", "revert", "--", WIDGETS]);
    // Someone stages the original base entry on purpose.
    let info = format!("100644,{base_blob},{WIDGETS}");
    s.git(&["update-index", "--cacheinfo", &info]);
    let out = s.recover(json!({}), &[]);
    let r = one_result(&out);
    assert_eq!(r["result"], "repair_required", "{out}");
    assert_eq!(r["reason"], "index_changed");
    assert!(
        s.git(&["ls-files", "-s", WIDGETS]).contains(&base_blob),
        "the staged entry was overwritten"
    );
}

#[test]
fn commit_a_path_holding_a_newline_cannot_add_a_trailer() {
    let s = Store::new("commit-newline-path", "skip");
    let path = "tracking/odd\nRivet-Op: forged\nname.md";
    std::fs::write(s.root.join(path), "# Odd\n\n## Notes\n\nText.\n").unwrap();
    s.git(&["add", "--", path]);
    s.git(&["commit", "-qm", "odd", "--", path]);
    let select = json!({"kind": "heading_path", "path": ["Odd", "Notes"]});
    let (_, got) = rivet(&s.root, "get", json!({"path": path, "select": select}));
    let (code, out) = s.run(
        "set",
        json!({"path": path, "select": select, "part": "body", "guard": got["revision"],
               "content": "Changed.\n"}),
        &[],
    );
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["commit"]["status"], "committed", "{out}");
    let message = s.message(&s.head());
    let trailers = message
        .lines()
        .filter(|l| l.starts_with("Rivet-Op:"))
        .count();
    assert_eq!(trailers, 1, "{message}");
    assert!(message.starts_with("rivet: tracking/odd\\nRivet-Op: forged\\nname.md\n"));
}

#[test]
fn commit_the_last_write_follows_sequence_numbers_not_lines() {
    let s = Store::new("commit-last-write", "skip");
    // Three applied writes to one path. The newest (seq 9) is neither first
    // nor last by op id or by line, so neither first-wins nor last-wins
    // picks it; an unknown outcome with a higher seq never counts.
    let line = |op: &str, seq: u64, session: &str| {
        json!({"op": op, "seq": seq, "session": session, "path": WIDGETS,
               "outcome": "skipped", "reason": "dirty", "applied": true,
               "retries": 0, "git_ms": 0, "recovered": false, "at": 1})
        .to_string()
    };
    std::fs::write(
        s.root.join(".rivet/commits.jsonl"),
        format!(
            "{}\n{}\n{}\n{}\n",
            line("zzzz", 5, "middle"),
            line("mmmm", 9, "newest"),
            line("aaaa", 3, "oldest"),
            json!({"op": "nnnn", "seq": 12, "session": "unknown", "path": WIDGETS,
                   "outcome": "skipped", "applied": "unknown", "at": 1})
        ),
    )
    .unwrap();
    let (_, status) = s.run("status", json!({"paths": [WIDGETS]}), &[]);
    let p = &status["paths"][0];
    assert_eq!(p["last_write"]["op"], "mmmm", "{p}");
    assert_eq!(p["owner"], "receipt:newest");
    assert_eq!(status["receipts"]["writes"], 3);
}

#[test]
fn commit_step_8_rejects_every_commit_that_is_not_the_transaction() {
    for (variant, reason) in [
        ("orphan", "parent_mismatch"),
        ("extra", "paths_mismatch"),
        ("blob", "blob_mismatch"),
        ("twice", "trailer_mismatch"),
        ("index", "index_changed"),
    ] {
        let s = Store::new(&format!("commit-forged-{variant}"), "skip");
        let ids = s.items();
        let (code, out) = s.set(&ids[0], "[x] **Ship.** Forged.", &[s.forged(variant)]);
        assert_eq!(code, 0, "{variant}: {out}");
        assert_eq!(out["commit"]["status"], "failed", "{variant}: {out}");
        assert_eq!(out["commit"]["reason"], reason, "{variant}");
        assert_eq!(out["commit"]["published"], true, "{variant}");
        let rec = s.record();
        assert_eq!(rec["state"], "repair_required", "{variant}");
        let op = rec["op"].as_str().unwrap();
        assert!(
            s.events(op).iter().all(|e| e["outcome"] == "failed"),
            "{variant}: a terminal receipt"
        );
        // Recovery's search meets the same commit and stops there.
        let out = s.recover(json!({}), &[]);
        let r = one_result(&out);
        assert_eq!(r["result"], "repair_required", "{variant}: {out}");
        assert_eq!(s.records().len(), 1, "{variant}");
    }
}

#[test]
fn commit_status_reads_git_detail_for_an_unbound_pending_path() {
    let s = Store::new("commit-unbound", "skip");
    let ids = s.items();
    let request = s.set_request(&ids[0], "[x] **Ship.** Unbound.");
    s.kill_at("pending_written", "set", &request, &[]);
    // The collection no longer binds the widgets tracker.
    let path = s.root.join(".rivet/project.json");
    let mut config: Value = serde_json::from_str(&read(&s.root, ".rivet/project.json")).unwrap();
    config["collections"]["actions"]["files"] = json!([DOCS]);
    std::fs::write(&path, config.to_string()).unwrap();
    let (_, status) = s.run("status", json!({}), &[]);
    let p = status["paths"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["path"] == WIDGETS)
        .unwrap_or_else(|| panic!("{status}"))
        .clone();
    assert_eq!(p["base"], "known_clean", "{p}");
    assert_eq!(p["pending"].as_array().unwrap().len(), 1);
    std::fs::write(s.root.join(".gitattributes"), "tracking/*.md filter=x\n").unwrap();
    let (_, status) = s.run("status", json!({}), &[]);
    let p = status["paths"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["path"] == WIDGETS)
        .unwrap()
        .clone();
    assert_eq!(p["ineligible"]["reason"], "git_conversion", "{p}");
}

// ---------------------------------------------------------------------------
// Adopt and creation.

const NEW: &str = "tracking/TRACKING_NEW.md";
const NEW_CONTENT: &str =
    "---\nstatus: active\n---\n\n# New\n\n## Action\n\n- [ ] **First.** Opened.\n";

fn create_request(content: &str) -> Value {
    json!({"path": NEW, "content": content, "stamp": true, "session": "maker"})
}

/// An outside edit inside the first action item, which carries a stay.
fn edit_outside(s: &Store) {
    let edited = read(&s.root, WIDGETS).replace("The release branch is cut", "Edited outside");
    std::fs::write(s.root.join(WIDGETS), edited).unwrap();
}

/// A hand-written action with no stay, at the end of the queue.
fn add_by_hand(s: &Store) {
    let before = read(&s.root, WIDGETS);
    let text = before.replace(
        "- [x] `human:sam`",
        "- [ ] **Added by hand.** No id yet.\n\n- [x] `human:sam`",
    );
    assert_ne!(text, before);
    std::fs::write(s.root.join(WIDGETS), text).unwrap();
}

#[test]
fn commit_adopt_records_provenance_for_an_outside_change_inside_the_written_unit() {
    let s = Store::new("adopt-provenance", "adopt");
    let ids = s.items();
    edit_outside(&s);
    // The write touches the very item the outside edit changed.
    let (code, out) = s.set(&ids[0], "[x] **Ship.** Edited outside, then shipped.", &[]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["commit"]["status"], "committed", "{out}");
    let message = s.message(&s.head());
    assert!(
        message.contains(&format!("Includes-outside-edit: changed item {}", ids[0])),
        "{message}"
    );
    assert!(s.clean());
    // `markrivet adopt` names a hand-added item it stamps, and an outside edit.
    s.configure(json!({"commit": "per-write", "on_dirty": "skip"}));
    s.git(&["commit", "-qam", "skip policy"]);
    add_by_hand(&s);
    let edited = read(&s.root, WIDGETS).replace("Regenerate from the", "Rebuild from the");
    std::fs::write(s.root.join(WIDGETS), edited).unwrap();
    let base = s.head();
    let (code, out) = s.run("adopt", json!({"path": WIDGETS, "session": "adopter"}), &[]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["commit"]["status"], "committed", "{out}");
    assert_eq!(
        out["ops"][0]["minted"].as_array().unwrap().len(),
        1,
        "{out}"
    );
    assert_eq!(s.since(&base).len(), 1);
    let message = s.message(&s.head());
    assert!(
        message.contains(&format!("Includes-outside-edit: changed item {}", ids[1])),
        "{message}"
    );
    assert!(
        message.contains("Includes-outside-edit: unstamped item added at line"),
        "{message}"
    );
    assert!(s.clean());
    assert_eq!(s.items().len(), 4, "the hand-added item is addressable");
    let receipt = s.receipts().last().unwrap().clone();
    assert_eq!(receipt["outcome"], "committed");
    assert_eq!(receipt["session"], "adopter");
}

#[test]
fn commit_adopt_refuses_with_an_unresolved_record_before_stamping() {
    let s = Store::new("adopt-unresolved", "skip");
    let ids = s.items();
    // Killed after its rename; the file then changes by hand, so recovery
    // cannot commit the recorded candidate and the record stays.
    let request = s.set_request(&ids[0], "[x] **Ship.** Stuck.");
    s.kill_at("applied", "set", &request, &[]);
    add_by_hand(&s);
    let out = s.recover(json!({}), &[]);
    assert_eq!(one_result(&out)["result"], "repair_required", "{out}");
    assert!(!s.root.join(".git/index.lock").exists());
    let before = read(&s.root, WIDGETS);
    let base = s.head();
    let (code, out) = s.run("adopt", json!({"path": WIDGETS}), &[]);
    assert_eq!(code, 2, "{out}");
    assert_eq!(out["error"]["code"], "pending_unresolved", "{out}");
    assert_eq!(read(&s.root, WIDGETS), before, "nothing stamped");
    assert_eq!(s.head(), base);
    assert_eq!(s.records().len(), 1);
}

#[test]
fn commit_adopt_commits_an_already_stamped_dirty_file() {
    let s = Store::new("adopt-stamped", "skip");
    edit_outside(&s);
    let before = read(&s.root, WIDGETS);
    let base = s.head();
    let (code, out) = s.run("adopt", json!({"path": WIDGETS}), &[]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["commit"]["status"], "committed", "{out}");
    assert_eq!(read(&s.root, WIDGETS), before, "nothing to stamp");
    assert_eq!(s.since(&base).len(), 1);
    assert!(s.clean());
    // A clean file with nothing to stamp has nothing to commit.
    let (code, out) = s.run("adopt", json!({"path": WIDGETS}), &[]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["status"], "unchanged", "{out}");
    assert!(out.get("commit").is_none(), "{out}");
    assert_eq!(s.since(&base).len(), 1);
}

#[test]
fn commit_adopt_needs_commit_mode_and_an_eligible_path() {
    let s = Store::new("adopt-off", "skip");
    edit_outside(&s);
    let before = read(&s.root, WIDGETS);
    s.configure(json!({"commit": "off"}));
    let (code, out) = s.run("adopt", json!({"path": WIDGETS}), &[]);
    assert_eq!(code, 2, "{out}");
    assert_eq!(out["error"]["code"], "invalid_request", "{out}");
    s.configure(json!({"commit": "per-write"}));
    // Staged: adopt writes nothing rather than landing stamps uncommitted.
    add_by_hand(&s);
    s.git(&["add", WIDGETS]);
    let before_staged = read(&s.root, WIDGETS);
    let (code, out) = s.run("adopt", json!({"path": WIDGETS}), &[]);
    assert_eq!(code, 2, "{out}");
    assert_eq!(out["error"]["code"], "commit_refused", "{out}");
    assert!(
        out["error"]["message"].as_str().unwrap().contains("staged"),
        "{out}"
    );
    assert_eq!(read(&s.root, WIDGETS), before_staged);
    assert_ne!(before, before_staged);
    assert!(s.records().is_empty());
    // A lingering lock, and a conversion attribute: refused the same way.
    s.git(&["reset", "-q", "--", WIDGETS]);
    std::fs::write(s.root.join(".git/index.lock"), "").unwrap();
    let (code, out) = s.run("adopt", json!({"path": WIDGETS}), &[]);
    assert_eq!(code, 2, "{out}");
    assert!(
        out["error"]["message"]
            .as_str()
            .unwrap()
            .contains("git_locked"),
        "{out}"
    );
    std::fs::remove_file(s.root.join(".git/index.lock")).unwrap();
    std::fs::write(s.root.join(".gitattributes"), "tracking/*.md filter=x\n").unwrap();
    let (code, out) = s.run("adopt", json!({"path": WIDGETS}), &[]);
    assert_eq!(code, 2, "{out}");
    assert!(
        out["error"]["message"]
            .as_str()
            .unwrap()
            .contains("git_conversion"),
        "{out}"
    );
    assert_eq!(read(&s.root, WIDGETS), before_staged);
    assert!(s.records().is_empty());
}

#[test]
fn create_concurrent_creators_give_one_file() {
    for mode in ["per-write", "off"] {
        let s = Store::new(&format!("create-race-{mode}"), "skip");
        s.configure(json!({"commit": mode}));
        s.git(&["commit", "-qam", "mode"]);
        // Two Rivet creators: the lock orders them, the second is refused.
        let a = s.spawn("create", &json!({"path": NEW, "content": "# A\n"}), &[]);
        let b = s.spawn("create", &json!({"path": NEW, "content": "# B\n"}), &[]);
        let (ca, oa) = finish(a);
        let (cb, ob) = finish(b);
        let mut codes = [ca, cb];
        codes.sort();
        assert_eq!(codes, [0, 2], "{oa} {ob}");
        let winner = if ca == 0 { "# A\n" } else { "# B\n" };
        let loser = if ca == 0 { &ob } else { &oa };
        assert_eq!(loser["error"]["code"], "already_exists", "{loser}");
        assert_eq!(read(&s.root, NEW), winner);
        // An outside writer that creates the file between Rivet's check and
        // its rename keeps its file.
        let other = "tracking/TRACKING_OTHER.md";
        let pause = Pause::new(&s.root, "before_rename");
        let child = s.spawn(
            "create",
            &json!({"path": other, "content": "# Rivet\n"}),
            &[pause.env()],
        );
        pause.wait();
        std::fs::write(s.root.join(other), "# Outside\n").unwrap();
        pause.release();
        let (code, out) = finish(child);
        assert_eq!(code, 2, "{mode}: {out}");
        assert_eq!(out["error"]["code"], "already_exists", "{out}");
        assert_eq!(read(&s.root, other), "# Outside\n", "{mode}");
        assert!(temp_files(&s.root).is_empty());
        assert!(s.records().is_empty(), "{mode}");
        if mode == "per-write" {
            let last = s.receipts().last().unwrap().clone();
            assert_eq!(last["outcome"], "not_applied", "{last}");
            assert_eq!(last["applied"], false);
        }
    }
}

#[test]
fn create_commits_one_new_file_and_refuses_an_existing_one() {
    let s = Store::new("create-commit", "refuse");
    let base = s.head();
    let (code, out) = s.run("create", create_request(NEW_CONTENT), &[]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["commit"]["status"], "committed", "{out}");
    let commits = s.since(&base);
    assert_eq!(commits.len(), 1);
    assert_eq!(s.changed(&commits[0]), [NEW]);
    assert!(s.clean());
    assert_eq!(s.git(&["status", "--porcelain", "--", NEW]), "");
    // The new tracker's action is stamped and addressable.
    let (_, q) = rivet(
        &s.root,
        "query",
        json!({"collection": "actions", "paths": [NEW]}),
    );
    assert!(q["items"][0]["id"].is_string(), "{q}");
    let before = read(&s.root, NEW);
    let (code, out) = s.run("create", create_request("# Again\n"), &[]);
    assert_eq!(code, 2, "{out}");
    assert_eq!(out["error"]["code"], "already_exists");
    assert_eq!(read(&s.root, NEW), before);
    // An ignored path lands and is reported ineligible, never committed.
    std::fs::write(s.root.join(".gitignore"), "tracking/TRACKING_IGNORED.md\n").unwrap();
    s.git(&["add", ".gitignore"]);
    s.git(&["commit", "-qm", "ignore"]);
    s.configure(json!({"commit": "per-write", "on_dirty": "skip"}));
    let (code, out) = s.run(
        "create",
        json!({"path": "tracking/TRACKING_IGNORED.md", "content": "# I\n"}),
        &[],
    );
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["commit"]["status"], "ineligible", "{out}");
    assert_eq!(out["commit"]["reason"], "ignored", "{out}");
}

#[test]
fn create_killed_before_private_index_recovers_under_refuse() {
    let s = Store::new("create-before-add", "refuse");
    let base = s.head();
    s.kill_at("applied", "create", &create_request(NEW_CONTENT), &[]);
    let rec = s.record();
    assert_eq!(rec["state"], "applied");
    assert_eq!(rec["creation"], true);
    assert_eq!(s.git(&["ls-files", "--", NEW]), "", "never staged");
    let out = s.recover(json!({}), &[]);
    assert_eq!(one_result(&out)["result"], "committed", "{out}");
    assert_eq!(s.since(&base).len(), 1);
    assert!(s.records().is_empty());
    assert!(s.clean());
    let events = s.events(rec["op"].as_str().unwrap());
    assert_eq!(events.last().unwrap()["outcome"], "committed");
    assert_eq!(events.last().unwrap()["applied"], true);
}

#[test]
fn create_killed_after_private_index_recovers() {
    let s = Store::new("create-after-add", "skip");
    let base = s.head();
    s.kill_at("staged", "create", &create_request(NEW_CONTENT), &[]);
    let rec = s.record();
    assert_eq!(
        s.git(&["ls-files", "-s", "--", NEW]),
        "",
        "private staging must not change the real index"
    );
    assert_eq!(s.head(), base, "the commit did not");
    let out = s.recover(json!({}), &[]);
    assert_eq!(one_result(&out)["result"], "committed", "{out}");
    let commits = s.since(&base);
    assert_eq!(commits.len(), 1);
    assert!(
        s.message(&commits[0])
            .contains(&format!("Rivet-Op: {}", rec["op"].as_str().unwrap()))
    );
    assert!(s.clean());
}

#[test]
fn commit_an_equal_byte_adopt_killed_around_its_rename_is_committed_by_recovery() {
    for point in ["pending_written", "applied"] {
        let s = Store::new(&format!("adopt-equal-{point}"), "skip");
        edit_outside(&s);
        let base = s.head();
        s.kill_at(point, "adopt", &json!({"path": WIDGETS}), &[]);
        let rec = s.record();
        assert_eq!(rec["read_sha256"], rec["candidate_sha256"], "{rec}");
        let out = s.recover(json!({}), &[]);
        assert_eq!(one_result(&out)["result"], "committed", "{point}: {out}");
        assert_eq!(s.since(&base).len(), 1, "{point}");
        assert!(s.clean(), "{point}");
    }
}

#[test]
fn commit_abandoned_adopts_and_creations_carry_their_application_certainty() {
    let s = Store::new("adopt-abandon", "skip");
    let abandon = |op: &Value| {
        let out = s.recover(json!({"abandon": op, "reason": "test"}), &[]);
        one_result(&out)["applied"].clone()
    };
    let status = |path: &str| {
        let (_, out) = s.run("status", json!({"paths": [path]}), &[]);
        out
    };
    // An equal-byte adopt before its rename: nothing tells the read bytes
    // from the candidate.
    edit_outside(&s);
    s.kill_at(
        "pending_written",
        "adopt",
        &json!({"path": WIDGETS, "session": "before"}),
        &[],
    );
    let op = s.record()["op"].clone();
    assert_eq!(abandon(&op), "unknown");
    let out = status(WIDGETS);
    assert_eq!(out["receipts"]["writes"], 0, "{out}");
    assert_eq!(out["receipts"]["unknown_by_session"]["before"], 1, "{out}");
    assert_eq!(out["paths"][0]["owner"], "unknown", "{out}");
    // The same adopt after a confirmed application.
    s.kill_at(
        "applied",
        "adopt",
        &json!({"path": WIDGETS, "session": "after"}),
        &[],
    );
    let op = s.record()["op"].clone();
    assert_eq!(abandon(&op), true);
    let out = status(WIDGETS);
    assert_eq!(out["receipts"]["writes"], 1, "{out}");
    assert_eq!(out["paths"][0]["last_write"]["op"], op);
    assert_eq!(out["paths"][0]["owner"], "receipt:after");
    // A creation before its publication: no file and no index entry.
    s.kill_at(
        "pending_written",
        "create",
        &create_request(NEW_CONTENT),
        &[],
    );
    let op = s.record()["op"].clone();
    assert!(!s.root.join(NEW).exists());
    assert_eq!(abandon(&op), false);
    let out = status(NEW);
    assert_eq!(out["receipts"]["writes"], 1, "{out}");
    assert_eq!(out["paths"][0]["owner"], "unknown", "{out}");
    assert!(out["paths"][0]["last_write"].is_null(), "{out}");
}

#[test]
fn commit_a_creation_killed_before_publication_ends_not_applied() {
    let s = Store::new("create-not-applied", "skip");
    s.kill_at(
        "pending_written",
        "create",
        &create_request(NEW_CONTENT),
        &[],
    );
    let rec = s.record();
    let out = s.recover(json!({}), &[]);
    assert_eq!(one_result(&out)["result"], "not_applied", "{out}");
    assert!(s.records().is_empty());
    let events = s.events(rec["op"].as_str().unwrap());
    assert_eq!(events.last().unwrap()["applied"], false);
    // The path is free again.
    let (code, out) = s.run("create", create_request(NEW_CONTENT), &[]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["commit"]["status"], "committed", "{out}");
}

#[test]
fn commit_the_new_ops_lift_message_and_session() {
    let s = Store::new("new-ops", "skip");
    let section = |path: &[&str]| -> (Value, String) {
        let select = json!({"kind": "heading_path", "path": path});
        let (_, got) = rivet(&s.root, "get", json!({"path": WIDGETS, "select": select}));
        (select, got["revision"].as_str().unwrap().to_string())
    };
    let doc_guard = || {
        let (_, map) = rivet(&s.root, "map", json!({"path": WIDGETS}));
        map["revision"].as_str().unwrap().to_string()
    };
    let (select, guard) = section(&["Notes"]);
    let requests = [
        (
            "append",
            json!({"path": WIDGETS, "select": select, "guard": guard, "content": "One more note."}),
        ),
        (
            "add-key",
            json!({"path": WIDGETS, "key": "reviewed", "value": "2026-10-10", "guard": "doc"}),
        ),
    ];
    for (cmd, mut request) in requests {
        if request["guard"] == "doc" {
            request["guard"] = json!(doc_guard());
        }
        request["message"] = json!(format!("{cmd} by test"));
        request["session"] = json!(format!("{cmd}-session"));
        let (code, out) = s.run(cmd, request, &[]);
        assert_eq!(code, 0, "{cmd}: {out}");
        assert_eq!(out["commit"]["status"], "committed", "{cmd}: {out}");
        let sha = s.head();
        assert!(s.message(&sha).starts_with(&format!("{cmd} by test\n")));
        let receipt = s.receipts().last().unwrap().clone();
        assert_eq!(receipt["session"], format!("{cmd}-session"));
        assert!(s.clean());
    }
}

#[test]
fn commit_adopt_commits_a_creation_that_landed_uncommitted() {
    let s = Store::new("adopt-untracked", "skip");
    // A lingering lock makes the create land ineligible: the file exists,
    // untracked, and nothing else would ever commit it.
    std::fs::write(s.root.join(".git/index.lock"), "").unwrap();
    let (code, out) = s.run("create", create_request(NEW_CONTENT), &[]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["commit"]["reason"], "git_locked", "{out}");
    std::fs::remove_file(s.root.join(".git/index.lock")).unwrap();
    assert_eq!(
        s.git(&["status", "--porcelain", "--", NEW]),
        format!("?? {NEW}\n")
    );
    // Nothing to stamp, and still committed: adding the file is the point.
    let base = s.head();
    let before = read(&s.root, NEW);
    let (code, out) = s.run("adopt", json!({"path": NEW}), &[]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["commit"]["status"], "committed", "{out}");
    assert_eq!(read(&s.root, NEW), before);
    let commits = s.since(&base);
    assert_eq!(commits.len(), 1);
    assert_eq!(s.changed(&commits[0]), [NEW]);
    assert_eq!(s.git(&["status", "--porcelain", "--", NEW]), "");
    // An untracked file with an item to stamp, killed before its rename:
    // the file still holds the bytes read, so recovery says not_applied.
    let other = "tracking/TRACKING_LOOSE.md";
    std::fs::write(
        s.root.join(other),
        "# Loose\n\n## Action\n\n- [ ] **One.**\n",
    )
    .unwrap();
    s.kill_at("pending_written", "adopt", &json!({"path": other}), &[]);
    let rec = s.record();
    assert_eq!(rec["creation"], true);
    assert_ne!(rec["read_sha256"], "");
    let out = s.recover(json!({}), &[]);
    assert_eq!(one_result(&out)["result"], "not_applied", "{out}");
    let (code, out) = s.run("adopt", json!({"path": other}), &[]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["commit"]["status"], "committed", "{out}");
    assert!(s.clean());
}

#[test]
fn commit_adopt_writes_nothing_when_hash_object_fails() {
    let s = Store::new("adopt-hash-object", "skip");
    add_by_hand(&s);
    let before = read(&s.root, WIDGETS);
    let base = s.head();
    let index = s.git(&["ls-files", "-s", WIDGETS]);
    let failing = s.script(
        "no-hash",
        "case \"$*\" in *hash-object*) exit 1;; esac\nexec git \"$@\"",
    );
    let (code, out) = s.run("adopt", json!({"path": WIDGETS}), &[failing]);
    assert_eq!(code, 2, "{out}");
    assert_eq!(out["error"]["code"], "commit_refused", "{out}");
    assert_eq!(read(&s.root, WIDGETS), before, "no stamps landed");
    assert_eq!(s.head(), base);
    assert_eq!(s.git(&["ls-files", "-s", WIDGETS]), index);
    assert!(s.records().is_empty());
}

#[test]
fn commit_adopt_refuses_a_stamped_result_too_large_to_read() {
    let s = Store::new("adopt-large", "skip");
    let big = "big.md";
    let limit = 8 * 1024 * 1024;
    let head = "# Big\n\n";
    let body = "x".repeat(limit - head.len() - 1);
    std::fs::write(s.root.join(big), format!("{head}{body}\n")).unwrap();
    s.git(&["add", big]);
    s.git(&["commit", "-qm", "big"]);
    // Dirty it without growing it, so adopt has a heading to stamp.
    std::fs::write(s.root.join(big), format!("{head}y{}\n", &body[1..])).unwrap();
    let before = read(&s.root, big);
    let (code, out) = s.run("adopt", json!({"path": big}), &[]);
    assert_eq!(code, 2, "{out}");
    assert_eq!(out["error"]["code"], "limit_exceeded", "{out}");
    assert_eq!(read(&s.root, big), before);
    assert!(s.records().is_empty());
}

#[test]
fn commit_an_unpublished_creation_survives_a_failed_first_recovery() {
    let s = Store::new("create-strand", "skip");
    s.kill_at(
        "pending_written",
        "create",
        &create_request(NEW_CONTENT),
        &[],
    );
    let rec = s.record();
    // The first recovery meets a lock and requires repair.
    std::fs::write(s.root.join(".git/index.lock"), "").unwrap();
    let out = s.recover(json!({}), &[]);
    assert_eq!(one_result(&out)["result"], "repair_required", "{out}");
    assert_eq!(s.record()["state"], "repair_required");
    // Once the lock is gone, the absence still proves it never published.
    std::fs::remove_file(s.root.join(".git/index.lock")).unwrap();
    let out = s.recover(json!({}), &[]);
    assert_eq!(one_result(&out)["result"], "not_applied", "{out}");
    let events = s.events(rec["op"].as_str().unwrap());
    assert_eq!(events.last().unwrap()["applied"], false);
    let (_, status) = s.run("status", json!({"paths": [NEW]}), &[]);
    assert_eq!(status["paths"][0]["owner"], "unknown", "{status}");
    let (code, out) = s.run("create", create_request(NEW_CONTENT), &[]);
    assert_eq!(code, 0, "{out}");
}

#[test]
fn commit_the_repairs_run_as_commands_with_message_and_session() {
    let s = Store::new("repair-commands", "skip");
    // A wrapped span and a hand-typed stay in the Notes list, committed.
    let text = read(&s.root, WIDGETS).replace(
        "- The rollout is gated on the migration note, not on the build.",
        "- The rollout is gated on `the\n  migration note`, not the build. <!-- stay:handID09 -->",
    );
    std::fs::write(s.root.join(WIDGETS), &text).unwrap();
    s.git(&["commit", "-qam", "hand edits"]);
    let select = json!({"kind": "heading_path", "path": ["Notes"]});
    let (_, got) = rivet(&s.root, "get", json!({"path": WIDGETS, "select": select}));
    let line = text[..text.find("on `the").unwrap()].matches('\n').count() + 1;
    let (code, out) = s.run(
        "rejoin",
        json!({"path": WIDGETS, "select": select, "guard": got["revision"], "line": line,
               "message": "rejoin by test", "session": "rejoiner"}),
        &[],
    );
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["commit"]["status"], "committed", "{out}");
    assert!(s.message(&s.head()).starts_with("rejoin by test\n"));
    assert_eq!(s.receipts().last().unwrap()["session"], "rejoiner");
    let after = read(&s.root, WIDGETS);
    assert_eq!(
        after,
        text.replace("on `the\n  migration note`", "on\n  `the migration note`")
    );
    let (_, got) = rivet(
        &s.root,
        "get",
        json!({"path": WIDGETS, "select": {"kind": "id", "id": "handID09"}}),
    );
    assert_eq!(got["target"]["kind"], "list", "{got}");
    let (code, out) = s.run(
        "adopt-stay",
        json!({"path": WIDGETS, "id": "handID09", "guard": got["revision"],
               "message": "adopt stay by test", "session": "adopter"}),
        &[],
    );
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["commit"]["status"], "committed", "{out}");
    assert!(s.message(&s.head()).starts_with("adopt stay by test\n"));
    assert_eq!(s.receipts().last().unwrap()["session"], "adopter");
    let (_, got) = rivet(
        &s.root,
        "get",
        json!({"path": WIDGETS, "select": {"kind": "id", "id": "handID09"}}),
    );
    assert_eq!(got["target"]["kind"], "item", "{got}");
    let adopted = read(&s.root, WIDGETS);
    assert!(adopted.contains("not the build. <!-- stay:handID09 subhash=sha256:"));
    assert_eq!(
        adopted.replace(
            &adopted[adopted.find("<!-- stay:handID09").unwrap()..][..adopted
                [adopted.find("<!-- stay:handID09").unwrap()..]
                .find("-->")
                .unwrap()
                + 3],
            "<!-- stay:handID09 -->"
        ),
        after,
        "only the marker changed"
    );
    assert!(s.clean());
}

#[test]
fn an_emacs_lock_taken_while_a_write_holds_the_project_lock_refuses_it() {
    // `planned`: the candidate is built from a read with no lock beside it.
    // `before_rename`: the temporary file is written and synced too, so the
    // check runs after it, next to the final re-read.
    for point in ["planned", "before_rename"] {
        let (root, open) = stamped(&format!("emacs-race-{point}"));
        let guard = item_guard(&root, WIDGETS, &open[0].1);
        let before = std::fs::read(root.join(WIDGETS)).unwrap();
        let pause = Pause::new(&root, point);
        let writer = spawn(
            &root,
            "set",
            &set_item(&open[0].1, &guard, "[x] **Ship.** Raced an editor."),
            &[pause.env()],
        );
        pause.wait();
        // The user starts typing in an Emacs buffer visiting the file.
        let lock = emacs_lock(&root, WIDGETS, &live_lock_text(), false);
        pause.release();
        let (code, out) = finish(writer);
        assert_eq!(code, 2, "{point}: {out}");
        assert_eq!(out["error"]["code"], "editor_lock", "{point}: {out}");
        assert_eq!(
            std::fs::read(root.join(WIDGETS)).unwrap(),
            before,
            "{point}: byte-identical"
        );
        assert!(temp_files(&root).is_empty(), "{point}");
        // Saving in Emacs removes the lock; the retried write lands.
        std::fs::remove_file(lock).unwrap();
        let (code, out) = rivet(
            &root,
            "set",
            set_item(&open[0].1, &guard, "[x] **Ship.** After the save."),
        );
        assert_eq!(code, 0, "{point}: {out}");
    }
}

fn receipted_request(s: &Store, id: &str) -> Value {
    json!({"path":WIDGETS, "select":{"kind":"id","id":id},
        "guard":item_guard(&s.root,WIDGETS,id), "require_commit":true})
}

fn archive_store(name: &str) -> Store {
    let s = Store::new(name, "refuse");
    let path = s.root.join(".rivet/project.json");
    let mut config: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    config["collections"]["actions"]["archive"] = json!(["archive/**/*.md"]);
    config["prune"] = json!({"files":["tracking/*.md"]});
    std::fs::write(path, serde_json::to_vec(&config).unwrap()).unwrap();
    std::fs::create_dir_all(s.root.join("archive/month")).unwrap();
    let (code, out) = s.run(
        "create",
        json!({"path":"archive/month/closed.md","content":"# Archive\n\n## Closed\n","stamp":true}),
        &[],
    );
    assert_eq!(code, 0, "{out}");
    assert_eq!(
        out["commit"]["status"], "committed",
        "archive fixture create: {out}"
    );
    s.git(&["add", "-A"]);
    s.git(&["commit", "-qm", "configure synthetic archive"]);
    s
}

fn archive_request(s: &Store, id: &str) -> Value {
    let select = json!({"kind":"heading_path","path":["Closed"]});
    let (_, dest) = s.run(
        "get",
        json!({"path":"archive/month/closed.md","select":select}),
        &[],
    );
    let mut request = json!({"source":WIDGETS,"id":id,"guard":item_guard(&s.root,WIDGETS,id),
        "path":"archive/month/closed.md","select":select,"destination_guard":dest["revision"]});
    if let Some(parent) = section_archive::parent_stay(s, id) {
        request["parent"] = json!(parent);
    }
    request
}

#[test]
fn archive_provenance_accepts_git_commits_with_non_utf8_metadata() {
    for field in ["author", "committer", "message"] {
        let s = archive_store(&format!("archive-non-utf8-{field}"));
        let ids = s.items();
        let request = archive_request(&s, &ids[1]);
        let source = read(&s.root, WIDGETS);
        let (code, copy) = s.run("archive-copy", request.clone(), &[]);
        assert_eq!(code, 0, "{copy}");
        assert_eq!(copy["commit"]["status"], "committed", "{copy}");
        let copy_sha = copy["commit"]["sha"].as_str().unwrap();
        let archive = read(&s.root, "archive/month/closed.md");
        let mut raw = s.git(&["cat-file", "commit", copy_sha]).into_bytes();
        let prefix = if field == "message" {
            "\n\n".to_string()
        } else {
            format!("\n{field} ")
        };
        let at = raw
            .windows(prefix.len())
            .position(|w| w == prefix.as_bytes())
            .unwrap()
            + prefix.len();
        // Latin-1 is valid Git metadata. The parent and receipt trailer
        // bytes remain identical, independent of this unrelated text.
        raw[at] = 0xe9;
        let object = s.home.join("non-utf8-commit");
        std::fs::write(&object, raw).unwrap();
        let rewritten = s.git(&[
            "hash-object",
            "-t",
            "commit",
            "-w",
            object.to_str().unwrap(),
        ]);
        let rewritten = rewritten.trim();
        s.git(&["update-ref", "HEAD", rewritten, copy_sha]);

        let (code, retry) = s.run("archive-copy", request, &[]);
        assert_eq!(code, 0, "{field}: {retry}");
        assert_eq!(retry["status"], "unchanged", "{retry}");
        assert_eq!(read(&s.root, WIDGETS), source);
        assert_eq!(read(&s.root, "archive/month/closed.md"), archive);
        assert_eq!(s.head(), rewritten);

        let mut deletion = receipted_request(&s, &ids[1]);
        deletion["archive"] = json!({"path":"archive/month/closed.md",
            "copy_op":copy["commit"]["copy_op"],"commit":rewritten});
        let (code, deleted) = s.run("delete", deletion, &[]);
        assert_eq!(code, 0, "{field}: {deleted}");
        assert_eq!(deleted["commit"]["status"], "committed", "{deleted}");
        assert_eq!(read(&s.root, "archive/month/closed.md"), archive);
        assert_eq!(s.items(), [ids[0].clone(), ids[2].clone()]);
    }
}

#[test]
fn recovery_rebind_keeps_delete_and_archive_trailers_in_the_same_record() {
    use base64::Engine;
    let s = archive_store("combined-trailers");
    // Construct a synthetic composite record. Current commands issue these
    // receipts separately, but recovery must retain every typed trailer.
    let config_path = s.root.join(".rivet/project.json");
    let mut config: Value = serde_json::from_slice(&std::fs::read(&config_path).unwrap()).unwrap();
    config["collections"]["actions"]["archive"] = json!(["archive/**/*.md", WIDGETS]);
    std::fs::write(config_path, serde_json::to_vec(&config).unwrap()).unwrap();
    s.git(&["add", ".rivet/project.json"]);
    s.git(&["commit", "-qm", "configure composite receipt fixture"]);
    let (_, docs) = s.run(
        "query",
        json!({"collection":"actions", "paths":[DOCS]}),
        &[],
    );
    let id = docs["items"][0]["id"].as_str().unwrap();
    let mut copy_request = archive_request(&s, &s.items()[0]);
    copy_request["source"] = json!(DOCS);
    copy_request["id"] = json!(id);
    copy_request["guard"] = json!(item_guard(&s.root, DOCS, id));
    let (code, copy) = s.run("archive-copy", copy_request, &[]);
    assert_eq!(code, 0, "{copy}");
    let mut receipt: markrivet_core::archive::CopyReceipt =
        serde_json::from_value(copy["commit"]["archive_copy"].clone()).unwrap();

    let ids = s.items();
    let before = read(&s.root, WIDGETS);
    let docs_before = read(&s.root, DOCS);
    let archive_before = read(&s.root, "archive/month/closed.md");
    s.kill_at(
        "after_rename",
        "delete",
        &receipted_request(&s, &ids[1]),
        &[],
    );
    let candidate = read(&s.root, WIDGETS);
    let mut rec = s.record();
    receipt.op = rec["op"].as_str().unwrap().into();
    receipt.copy.path = WIDGETS.into();
    rec["archive_copy"] = json!(receipt);
    let message = format!(
        "{}Rivet-Archive: {}\n",
        rec["message"].as_str().unwrap(),
        base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(&receipt).unwrap())
    );
    rec["message"] = json!(message);
    let pending = s.root.join(format!(".rivet/pending/{}.json", receipt.op));
    std::fs::write(&pending, serde_json::to_vec(&rec).unwrap()).unwrap();
    std::fs::write(
        s.root.join(format!(".rivet/pending/{}.msg", receipt.op)),
        message,
    )
    .unwrap();
    std::fs::write(s.root.join("unrelated.txt"), "independent commit\n").unwrap();
    s.git(&["add", "unrelated.txt"]);
    s.git(&[
        "commit",
        "-qm",
        "move head without touching receipt sources",
    ]);
    let parent = s.head();

    // Crash after publishing the rebound commit. The next process must find
    // it in history and accept the complete transaction shape.
    s.kill_at("ref_updated", "commit-pending", &json!({}), &[]);
    let message = s.message(&s.head());
    assert_eq!(
        message
            .lines()
            .filter(|l| l.starts_with("Rivet-Archive: "))
            .count(),
        1,
        "{message}"
    );
    assert_eq!(
        message
            .lines()
            .filter(|l| l.starts_with("Rivet-Delete: "))
            .count(),
        1,
        "{message}"
    );
    let out = s.recover(json!({}), &[]);
    assert_eq!(one_result(&out)["reason"], "stale_git_lock", "{out}");
    let identity = s.identity(&out, ".git/index.lock");
    let out = s.recover(
        json!({"op":receipt.op,"clear_lock":".git/index.lock","identity":identity}),
        &[],
    );
    assert_eq!(one_result(&out)["result"], "committed", "{out}");
    assert_eq!(s.carrying(&receipt.op).len(), 1);
    let events = s.events(&receipt.op);
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0]["archive_copy"], json!(receipt));
    assert_eq!(events[0]["delete_receipt"]["parent"], parent);
    let (code, verified) = s.run(
        "verify-delete",
        json!({"parent":before,"candidate":candidate,"receipt":events[0]["delete_receipt"]}),
        &[],
    );
    assert_eq!(code, 0, "{verified}");
    assert_eq!(read(&s.root, WIDGETS), candidate);
    assert_eq!(read(&s.root, DOCS), docs_before);
    assert_eq!(read(&s.root, "archive/month/closed.md"), archive_before);
    assert!(s.records().is_empty());
}

#[test]
fn receipted_delete_recovery_repins_parent_and_persists_the_amended_message() {
    let s = Store::new("delete-repin", "refuse");
    let ids = s.items();
    let before = read(&s.root, WIDGETS);
    let request = receipted_request(&s, &ids[1]);
    s.kill_at("after_rename", "delete", &request, &[]);
    let original = s.record();
    assert!(original["delete_receipt"].is_object());
    let (_, other) = s.set_docs("[x] **Licence.** Independent writer.");
    assert_eq!(other["commit"]["status"], "committed", "{other}");
    let parent = s.head();
    let candidate = read(&s.root, WIDGETS);
    let out = s.recover(json!({}), &[]);
    assert_eq!(one_result(&out)["result"], "committed", "{out}");
    let msg = s.message(&s.head());
    let payload = msg
        .lines()
        .find_map(|l| l.strip_prefix("Rivet-Delete: "))
        .unwrap();
    let receipt = markrivet_core::delete_receipt::DeleteReceipt::decode(payload).unwrap();
    assert_eq!(receipt.parent, parent);
    assert_eq!(
        receipt.base,
        serde_json::from_value(original["base"].clone()).unwrap()
    );
    assert_eq!(receipt.operation.dropped(), [ids[1].clone()]);
    let (_, verified) = s.run(
        "verify-delete",
        json!({"parent":before,"candidate":candidate,"receipt":receipt}),
        &[],
    );
    assert_eq!(verified["status"], "verified", "{verified}");
    assert!(s.records().is_empty());
    let terminal = s
        .receipts()
        .into_iter()
        .filter(|e| e["op"] == original["op"] && e["outcome"] == "committed")
        .collect::<Vec<_>>();
    assert_eq!(terminal.len(), 1);
    assert_eq!(terminal[0]["delete_receipt"]["parent"], parent);
}

#[test]
fn receipted_recovery_publication_failure_launches_no_git_and_keeps_the_candidate() {
    let s = Store::new("delete-rebind-failure", "refuse");
    let ids = s.items();
    s.kill_at(
        "after_rename",
        "delete",
        &receipted_request(&s, &ids[1]),
        &[],
    );
    let candidate = read(&s.root, WIDGETS);
    let (_, other) = s.set_docs("[x] **Licence.** Independent head.");
    assert_eq!(other["commit"]["status"], "committed");
    let head = s.head();
    let out = s.recover(json!({}), &[("RIVET_TEST_FAULT", "record_publish".into())]);
    assert_eq!(one_result(&out)["result"], "repair_required", "{out}");
    assert_eq!(s.head(), head);
    assert_eq!(read(&s.root, WIDGETS), candidate);
    assert_eq!(s.records().len(), 1);
    let out = s.recover(json!({}), &[]);
    assert_eq!(one_result(&out)["result"], "committed", "{out}");
}

#[test]
fn crash_fixture_reports_a_git_deadline_before_the_pause() {
    let s = archive_store("crash-fixture-deadline");
    let ids = s.items();
    let before = read(&s.root, WIDGETS);
    let head = s.head();
    let index = s.git(&["ls-files", "-s"]);
    let slow = s.script(
        "slow-candidate",
        r#"case "$*" in *" hash-object -w --no-filters --stdin") sleep 6;; esac
exec git "$@""#,
    );
    let panic = std::panic::catch_unwind(|| {
        s.kill_at(
            "after_rename",
            "delete",
            &receipted_request(&s, &ids[1]),
            &[slow],
        );
    })
    .expect_err("the five-second Git deadline must prevent publication");
    let message = panic.downcast_ref::<String>().unwrap();
    assert!(
        message.contains("writer exited before after_rename: exit 2"),
        "{message}"
    );
    assert!(
        message.contains("git did not finish before its deadline"),
        "{message}"
    );
    assert_eq!(read(&s.root, WIDGETS), before);
    assert_eq!(s.head(), head);
    assert_eq!(s.git(&["ls-files", "-s"]), index);
    assert!(s.records().is_empty());
}

#[test]
fn large_delete_and_archive_messages_remain_readable_and_recover_after_a_crash() {
    for command in ["delete", "archive-copy"] {
        let s = archive_store("large-receipt-message");
        let ids = s.items();
        let before = read(&s.root, WIDGETS);
        let mut request = if command == "delete" {
            receipted_request(&s, &ids[1])
        } else {
            archive_request(&s, &ids[1])
        };
        request["message"] = json!(format!(
            "Large synthetic message\n\n{}",
            "m".repeat(5 * 1024 * 1024)
        ));
        s.kill_at("after_rename", command, &request, &[]);
        let project = markrivet_core::fs::Project::open(&s.root).unwrap();
        let (records, errors) = markrivet_core::commit::records(&project).unwrap();
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(records.len(), 1);
        let pending = s
            .root
            .join(format!(".rivet/pending/{}.json", records[0].op));
        assert!(std::fs::metadata(pending).unwrap().len() > 8 * 1024 * 1024);
        let out = s.recover(json!({}), &[]);
        assert_eq!(
            one_result(&out)["result"],
            "committed",
            "large-message recovery must finish each Git child within {:?}; \
             a failed/deadline result retains the pending record: {out}",
            markrivet_core::git::DEFAULT_DEADLINE
        );
        assert!(s.records().is_empty());
        if command == "archive-copy" {
            assert_eq!(read(&s.root, WIDGETS), before);
        }
    }
}

#[test]
fn unreadable_pending_record_blocks_new_writes_before_ownership_can_be_known() {
    let s = Store::new("unreadable-pending", "refuse");
    let ids = s.items();
    std::fs::create_dir_all(s.root.join(".rivet/pending")).unwrap();
    std::fs::write(s.root.join(".rivet/pending/broken.json"), b"{\"op\":").unwrap();
    let source = read(&s.root, WIDGETS);
    let head = s.head();
    let index = s.git(&["ls-files", "-s"]);
    let (code, out) = s.run("delete", receipted_request(&s, &ids[1]), &[]);
    assert_eq!(code, 2, "{out}");
    assert_eq!(out["error"]["code"], "pending_unresolved");
    assert_eq!(read(&s.root, WIDGETS), source);
    assert_eq!(s.head(), head);
    assert_eq!(s.git(&["ls-files", "-s"]), index);
}

#[test]
fn require_commit_late_candidate_hash_failure_preserves_document_head_and_index() {
    let s = Store::new("delete-late-hash", "refuse");
    let ids = s.items();
    let source = read(&s.root, WIDGETS);
    let head = s.head();
    let index = s.git(&["ls-files", "-s"]);
    // Preflight hashes the source with --path; fail only the final candidate hash.
    let failing = s.script(
        "candidate-hash",
        r#"case "$*" in *" hash-object -w --no-filters --stdin") exit 1;; esac
exec git "$@""#,
    );
    let (code, out) = s.run("delete", receipted_request(&s, &ids[1]), &[failing]);
    assert_eq!(code, 2, "{out}");
    assert_eq!(out["error"]["code"], "commit_refused");
    assert_eq!(read(&s.root, WIDGETS), source);
    assert_eq!(s.head(), head);
    assert_eq!(s.git(&["ls-files", "-s"]), index);
    assert!(s.records().is_empty());
}

#[test]
fn receipted_delete_rejects_a_changed_payload_or_parent_during_commit_and_recovery() {
    for variant in ["payload", "orphan"] {
        let s = Store::new("delete-forgery", "refuse");
        let ids = s.items();
        let wrapper = if variant == "orphan" {
            s.forged("orphan")
        } else {
            s.script(
                "alter-receipt",
                r#"case "$*" in *" commit-tree "*)
prev=""; for a in "$@"; do [ "$prev" = "-F" ] && msg="$a"; prev="$a"; done
sed -i 's/Rivet-Delete: ./Rivet-Delete: X/' "$msg";;
esac
exec git "$@""#,
            )
        };
        let (_, out) = s.run("delete", receipted_request(&s, &ids[1]), &[wrapper]);
        assert_eq!(out["commit"]["status"], "failed", "{variant}: {out}");
        assert_eq!(
            out["commit"]["reason"],
            if variant == "orphan" {
                "parent_mismatch"
            } else {
                "delete_receipt_mismatch"
            },
            "{out}"
        );
        assert_eq!(s.record()["state"], "repair_required");
        let out = s.recover(json!({}), &[]);
        assert_eq!(one_result(&out)["result"], "repair_required", "{out}");
        assert_eq!(s.records().len(), 1);
    }
}

#[test]
fn archive_copy_rereads_source_guard_before_publication() {
    for section in [false, true] {
        let s = section_archive::matrix_store("copy-source-race", section);
        let ids = section_archive::matrix_ids(&s, section);
        let request = archive_request(&s, &ids[1]);
        let archive = read(&s.root, "archive/month/closed.md");
        let head = s.head();
        let pause = Pause::new(&s.root, "planned");
        let child = s.spawn("archive-copy", &request, &[pause.env()]);
        pause.wait();
        let changed =
            read(&s.root, WIDGETS).replace("Refresh the fixtures.", "Changed the fixtures.");
        assert_ne!(changed, read(&s.root, WIDGETS));
        std::fs::write(s.root.join(WIDGETS), &changed).unwrap();
        pause.release();
        let (code, out) = finish(child);
        assert_eq!(code, 2, "{out}");
        assert_eq!(out["error"]["code"], "guard_conflict", "{out}");
        assert_eq!(read(&s.root, WIDGETS), changed);
        assert_eq!(read(&s.root, "archive/month/closed.md"), archive);
        assert_eq!(s.head(), head);
    }
}

#[test]
fn an_idempotent_archive_retry_rechecks_source_after_provenance_search() {
    for section in [false, true] {
        let s = section_archive::matrix_store("copy-retry-source-race", section);
        let ids = section_archive::matrix_ids(&s, section);
        let request = archive_request(&s, &ids[1]);
        let (_, copied) = s.run("archive-copy", request.clone(), &[]);
        assert_eq!(copied["commit"]["status"], "committed");
        let archive = read(&s.root, "archive/month/closed.md");
        let head = s.head();
        let pause = Pause::new(&s.root, "archive_retry_checked");
        let child = s.spawn("archive-copy", &request, &[pause.env()]);
        pause.wait();
        let changed =
            read(&s.root, WIDGETS).replace("Refresh the fixtures.", "Changed the fixtures.");
        std::fs::write(s.root.join(WIDGETS), &changed).unwrap();
        pause.release();
        let (code, out) = finish(child);
        assert_eq!(code, 2, "{out}");
        assert_eq!(out["error"]["code"], "guard_conflict");
        assert_eq!(read(&s.root, WIDGETS), changed);
        assert_eq!(read(&s.root, "archive/month/closed.md"), archive);
        assert_eq!(s.head(), head);
    }
}

#[test]
fn archive_copy_crashes_and_bookkeeping_faults_never_duplicate_or_change_source() {
    for section in [false, true] {
        for point in [
            "before_rename",
            "after_rename",
            "pending_written",
            "committed",
        ] {
            let s = section_archive::matrix_store(&format!("copy-crash-{point}"), section);
            let ids = section_archive::matrix_ids(&s, section);
            let source = read(&s.root, WIDGETS);
            let request = archive_request(&s, &ids[1]);
            s.kill_at(point, "archive-copy", &request, &[]);
            assert_eq!(read(&s.root, WIDGETS), source);
            let out = s.recover(json!({}), &[]);
            assert!(
                ["not_applied", "committed"]
                    .contains(&one_result(&out)["result"].as_str().unwrap()),
                "{point}: {out}"
            );
            let (code, out) = s.run("archive-copy", request, &[]);
            assert_eq!(code, 0, "{point}: {out}");
            assert!(["applied", "unchanged"].contains(&out["status"].as_str().unwrap()));
            let doc =
                markrivet_core::Document::parse(&read(&s.root, "archive/month/closed.md")).unwrap();
            assert_eq!(doc.ids[&ids[1]].len(), 1);
            assert_eq!(read(&s.root, WIDGETS), source);
        }
        for fault in [
            "rename",
            "dir_sync",
            "receipt_append",
            "receipt_sync",
            "record_retire",
        ] {
            let s = section_archive::matrix_store(&format!("copy-fault-{fault}"), section);
            let ids = section_archive::matrix_ids(&s, section);
            let request = archive_request(&s, &ids[1]);
            let source = read(&s.root, WIDGETS);
            let (_, out) = s.run(
                "archive-copy",
                request.clone(),
                &[("RIVET_TEST_FAULT", fault.into())],
            );
            assert!(
                out["status"] == "refused" || out["status"] == "applied",
                "{fault}: {out}"
            );
            let _ = s.recover(json!({}), &[]);
            let (code, out) = s.run("archive-copy", request, &[]);
            assert_eq!(code, 0, "{fault}: {out}");
            let doc =
                markrivet_core::Document::parse(&read(&s.root, "archive/month/closed.md")).unwrap();
            assert_eq!(doc.ids[&ids[1]].len(), 1);
            assert_eq!(read(&s.root, WIDGETS), source);
        }
    }
}

#[test]
fn archive_precondition_refuses_staged_or_lost_archive_in_initial_delete_and_recovery() {
    for section in [false, true] {
        for recovery in [false, true] {
            for mutation in ["staged", "deleted"] {
                let s = section_archive::matrix_store("archive-delete-precondition", section);
                let ids = section_archive::matrix_ids(&s, section);
                let (_, copy) = s.run("archive-copy", archive_request(&s, &ids[1]), &[]);
                assert_eq!(copy["commit"]["status"], "committed", "{copy}");
                let mut request = receipted_request(&s, &ids[1]);
                request["archive"] = json!({"path":"archive/month/closed.md","copy_op":copy["commit"]["copy_op"],"commit":copy["commit"]["sha"]});
                section_archive::add_parent(&s, &ids[1], &mut request);
                let source = read(&s.root, WIDGETS);
                if recovery {
                    s.kill_at("after_rename", "delete", &request, &[]);
                }
                let candidate = read(&s.root, WIDGETS);
                if mutation == "staged" {
                    let archive = format!(
                        "{}\noutside archive append\n",
                        read(&s.root, "archive/month/closed.md")
                    );
                    std::fs::write(s.root.join("archive/month/closed.md"), archive).unwrap();
                    s.git(&["add", "archive/month/closed.md"]);
                } else {
                    std::fs::remove_file(s.root.join("archive/month/closed.md")).unwrap();
                }
                let head = s.head();
                let index = s.git(&["ls-files", "-s"]);
                if recovery {
                    let out = s.recover(json!({}), &[]);
                    assert_eq!(one_result(&out)["result"], "repair_required", "{out}");
                    assert_eq!(read(&s.root, WIDGETS), candidate);
                    assert_eq!(s.records().len(), 1);
                } else {
                    let (code, out) = s.run("delete", request, &[]);
                    assert_eq!(code, 2, "{out}");
                    assert_eq!(read(&s.root, WIDGETS), source);
                }
                assert_eq!(s.head(), head);
                assert_eq!(s.git(&["ls-files", "-s"]), index);
            }
        }
    }
}

#[test]
fn archive_precondition_rejects_each_forged_provenance_field_and_nonancestral_copy() {
    for section in [false, true] {
        use base64::Engine;
        for recovery in [false, true] {
            for field in [
                "copy_op",
                "nonancestor",
                "source",
                "id",
                "guard",
                "item_sha256",
                "ids",
                "path",
            ] {
                let s = section_archive::matrix_store("archive-provenance", section);
                let ids = section_archive::matrix_ids(&s, section);
                let source = read(&s.root, WIDGETS);
                let (_, copy) = s.run("archive-copy", archive_request(&s, &ids[1]), &[]);
                assert_eq!(copy["commit"]["status"], "committed", "{copy}");
                let copy_sha = copy["commit"]["sha"].as_str().unwrap();
                let copy_op = copy["commit"]["copy_op"].as_str().unwrap();
                let mut request = receipted_request(&s, &ids[1]);
                request["archive"] =
                    json!({"path":"archive/month/closed.md","copy_op":copy_op,"commit":copy_sha});
                section_archive::add_parent(&s, &ids[1], &mut request);
                if recovery {
                    s.kill_at("after_rename", "delete", &request, &[]);
                }
                let candidate = read(&s.root, WIDGETS);
                if field == "copy_op" {
                    request["archive"]["copy_op"] = json!("ffffffffffffffff");
                    if recovery {
                        let rec = s.record();
                        let op = rec["op"].as_str().unwrap();
                        let path = s.root.join(format!(".rivet/pending/{op}.json"));
                        let mut record = rec;
                        record["archive"]["copy_op"] = json!("ffffffffffffffff");
                        std::fs::write(path, serde_json::to_vec(&record).unwrap()).unwrap();
                    }
                } else {
                    let mut receipt = copy["commit"]["archive_copy"].clone();
                    match field {
                        "source" => receipt["copy"]["source"] = json!(DOCS),
                        "id" => receipt["copy"]["id"] = json!(ids[0]),
                        "guard" => {
                            receipt["copy"]["guard"] = json!(item_guard(&s.root, WIDGETS, &ids[0]))
                        }
                        "item_sha256" => receipt["copy"]["item_sha256"] = json!("0".repeat(64)),
                        "ids" => receipt["copy"]["ids"] = json!([ids[0]]),
                        "path" => receipt["copy"]["path"] = json!("archive/month/other.md"),
                        "nonancestor" => {}
                        _ => unreachable!(),
                    }
                    let receipt: markrivet_core::archive::CopyReceipt =
                        serde_json::from_value(receipt).unwrap();
                    let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
                        .encode(serde_json::to_vec(&receipt).unwrap());
                    let message = s.home.join("forged-copy-message");
                    std::fs::write(
                    &message,
                    format!(
                        "Synthetic copy claim\n\nRivet-Op: {copy_op}\nRivet-Archive: {payload}\n"
                    ),
                )
                .unwrap();
                    let tree = s
                        .git(&["rev-parse", &format!("{copy_sha}^{{tree}}")])
                        .trim()
                        .to_string();
                    let parent = s
                        .git(&["rev-parse", &format!("{copy_sha}^")])
                        .trim()
                        .to_string();
                    let forged = s
                        .git(&[
                            "commit-tree",
                            &tree,
                            "-p",
                            &parent,
                            "-F",
                            message.to_str().unwrap(),
                        ])
                        .trim()
                        .to_string();
                    if field != "nonancestor" {
                        s.git(&["update-ref", "HEAD", &forged]);
                    }
                    request["archive"]["commit"] = json!(forged);
                    if recovery {
                        let rec = s.record();
                        let op = rec["op"].as_str().unwrap();
                        let path = s.root.join(format!(".rivet/pending/{op}.json"));
                        let mut record = rec;
                        record["archive"]["commit"] = request["archive"]["commit"].clone();
                        std::fs::write(path, serde_json::to_vec(&record).unwrap()).unwrap();
                    }
                }
                let expected = if field == "nonancestor" {
                    "not ancestral"
                } else if field == "path" {
                    "changed another path"
                } else {
                    "provenance does not describe"
                };
                let head = s.head();
                let index = s.git(&["ls-files", "-s"]);
                let archive = read(&s.root, "archive/month/closed.md");
                if recovery {
                    let out = s.recover(json!({}), &[]);
                    assert_eq!(
                        one_result(&out)["result"],
                        "repair_required",
                        "{field}: {out}"
                    );
                    assert!(
                        s.record()["repair"]["detail"]
                            .as_str()
                            .unwrap()
                            .contains(expected),
                        "{field}: {}",
                        s.record()
                    );
                    assert_eq!(read(&s.root, WIDGETS), candidate);
                } else {
                    let (code, out) = s.run("delete", request, &[]);
                    assert_eq!(code, 2, "{field}: {out}");
                    assert_eq!(out["error"]["code"], "archive_copy_refused");
                    assert!(
                        out["error"]["message"].as_str().unwrap().contains(expected),
                        "{field}: {out}"
                    );
                    assert_eq!(read(&s.root, WIDGETS), source);
                }
                assert_eq!(s.head(), head);
                assert_eq!(s.git(&["ls-files", "-s"]), index);
                assert_eq!(read(&s.root, "archive/month/closed.md"), archive);
            }
        }
    }
}

#[test]
fn receipted_delete_crash_boundaries_produce_one_terminal_event() {
    for point in [
        "pending_written",
        "before_rename",
        "after_rename",
        "committed",
        "finalise",
    ] {
        let s = Store::new("delete-crash-receipt", "refuse");
        let ids = s.items();
        let request = receipted_request(&s, &ids[1]);
        let before = read(&s.root, WIDGETS);
        s.kill_at(point, "delete", &request, &[]);
        let _ = s.recover(json!({}), &[]);
        let events = s.receipts();
        let terminal = events
            .iter()
            .filter(|e| e["outcome"] == "committed" || e["outcome"] == "not_applied")
            .collect::<Vec<_>>();
        assert_eq!(terminal.len(), 1, "{point}: {events:?}");
        assert!(s.records().is_empty());
        if ["pending_written", "before_rename"].contains(&point) {
            assert_eq!(read(&s.root, WIDGETS), before);
        } else {
            let receipt: markrivet_core::delete_receipt::DeleteReceipt =
                serde_json::from_value(terminal[0]["delete_receipt"].clone()).unwrap();
            let (code, out) = s.run(
                "verify-delete",
                json!({"parent":before,"candidate":read(&s.root,WIDGETS),"receipt":receipt}),
                &[],
            );
            assert_eq!(code, 0, "{point}: {out}");
        }
        let count = s.receipts().len();
        let _ = s.recover(json!({}), &[]);
        assert_eq!(s.receipts().len(), count);
    }
}

#[test]
fn prune_locked_recheck_scopes_symlinks_and_refuses_new_gaps_or_membership() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    for change in [
        "unrelated",
        "file-symlink",
        "directory-symlink",
        "unreadable",
        "added",
        "removed",
        "live-only-edit",
    ] {
        if change == "unreadable" && unsafe { libc::geteuid() } == 0 {
            continue; // Root bypasses mode bits; core injects the same directory-open failure.
        }
        let s = archive_store(&format!("policy-namespace-{change}"));
        let path = s.root.join(".rivet/project.json");
        let mut config: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        config["prune"]["files"] = json!(["tracking/**/*.md"]);
        config["collections"]["actions"]["files"] = json!(["tracking/**/*.md", "docs/**/*.md"]);
        std::fs::write(path, serde_json::to_vec(&config).unwrap()).unwrap();
        let live_only = "docs/live-only.md";
        std::fs::create_dir_all(s.root.join("docs")).unwrap();
        std::fs::write(s.root.join(live_only), "# Live-only input\n").unwrap();
        s.git(&["add", ".rivet/project.json", live_only]);
        s.git(&["commit", "-qm", "recursive synthetic policy namespace"]);
        let ids = s.items();
        let (code, snapshot) = s.run("prune-snapshot", json!({}), &[]);
        assert_eq!(code, 0, "{snapshot}");
        assert!(
            snapshot["snapshot"]["live"]
                .as_array()
                .unwrap()
                .contains(&json!(live_only))
        );
        assert!(
            !snapshot["snapshot"]["policy"]
                .as_array()
                .unwrap()
                .contains(&json!(live_only))
        );
        assert_eq!(
            snapshot["snapshot"]["inputs"][live_only],
            "# Live-only input\n"
        );
        let mut request = receipted_request(&s, &ids[1]);
        request["policy_guard"] = snapshot["snapshot"]["revision"].clone();
        let source = read(&s.root, WIDGETS);
        let archive = read(&s.root, "archive/month/closed.md");
        let head = s.head();
        let index = std::fs::read(s.root.join(".git/index")).unwrap();
        let pause = Pause::new(&s.root, "policy_before_read");
        let child = s.spawn("delete", &request, &[pause.env()]);
        pause.wait();
        // The delete holds the writer lock. An outside process changes discovery
        // after the caller's snapshot and before the locked re-enumeration.
        match change {
            "unrelated" => symlink("missing", s.root.join("save.sh")).unwrap(),
            "file-symlink" => symlink("missing", s.root.join("tracking/linked.md")).unwrap(),
            "directory-symlink" => symlink("missing", s.root.join("tracking/subtree")).unwrap(),
            "unreadable" => {
                std::fs::create_dir(s.root.join("tracking/subtree")).unwrap();
                std::fs::set_permissions(
                    s.root.join("tracking/subtree"),
                    std::fs::Permissions::from_mode(0o0),
                )
                .unwrap();
            }
            "added" => std::fs::write(s.root.join("tracking/new.md"), "# New member\n").unwrap(),
            "removed" => std::fs::remove_file(s.root.join(live_only)).unwrap(),
            "live-only-edit" => {
                std::fs::write(s.root.join(live_only), "# Changed live-only input\n").unwrap()
            }
            _ => unreachable!(),
        }
        pause.release();
        let (code, out) = finish(child);
        if change == "unrelated" {
            assert_eq!(code, 0, "{out}");
            assert_eq!(out["commit"]["status"], "committed", "{out}");
            let candidate = read(&s.root, WIDGETS);
            let event = s.receipts().pop().unwrap();
            let (code, verified) = s.run(
                "verify-delete",
                json!({"parent":source,"candidate":candidate,"receipt":event["delete_receipt"]}),
                &[],
            );
            assert_eq!(code, 0, "{verified}");
        } else {
            assert_eq!(code, 2, "{out}");
            assert_eq!(
                out["error"]["code"],
                match change {
                    "file-symlink" | "directory-symlink" => "path_rejected",
                    "unreadable" => "io",
                    _ => "policy_conflict",
                },
                "{out}"
            );
            assert_eq!(read(&s.root, WIDGETS), source);
            assert_eq!(s.head(), head);
            assert_eq!(std::fs::read(s.root.join(".git/index")).unwrap(), index);
        }
        assert_eq!(read(&s.root, "archive/month/closed.md"), archive);
        if change == "unreadable" {
            std::fs::set_permissions(
                s.root.join("tracking/subtree"),
                std::fs::Permissions::from_mode(0o755),
            )
            .unwrap();
        }
    }
}

#[test]
fn prune_policy_change_refuses_initial_delete_and_recovery_on_a_new_head() {
    for recovery in [false, true] {
        let s = archive_store("policy-race");
        let ids = s.items();
        let mut request = receipted_request(&s, &ids[1]);
        let (_, snapshot) = s.run("prune-snapshot", json!({}), &[]);
        request["policy_guard"] = snapshot["snapshot"]["revision"].clone();
        let source = read(&s.root, WIDGETS);
        if recovery {
            s.kill_at("after_rename", "delete", &request, &[]);
        }
        let candidate = read(&s.root, WIDGETS);
        let (_, added) = s.set_docs(&format!("[ ] **Licence.** New wait item:{}.", ids[1]));
        assert_eq!(added["commit"]["status"], "committed", "{added}");
        if recovery {
            let out = s.recover(json!({}), &[]);
            assert_eq!(one_result(&out)["result"], "repair_required", "{out}");
            assert_eq!(read(&s.root, WIDGETS), candidate);
            assert_eq!(s.records().len(), 1);
        } else {
            let (code, out) = s.run("delete", request, &[]);
            assert_eq!(code, 2, "{out}");
            assert_eq!(out["error"]["code"], "policy_conflict");
            assert_eq!(read(&s.root, WIDGETS), source);
        }
    }
}

#[test]
fn prune_policy_recovery_substitutes_source_only_with_unchanged_head() {
    for moved in [false, true] {
        let s = archive_store("policy-positive-recovery");
        let ids = s.items();
        let source = read(&s.root, WIDGETS);
        let (_, snapshot) = s.run("prune-snapshot", json!({}), &[]);
        let mut request = receipted_request(&s, &ids[1]);
        request["policy_guard"] = snapshot["snapshot"]["revision"].clone();
        s.kill_at("after_rename", "delete", &request, &[]);
        let record = s.record();
        let candidate = read(&s.root, WIDGETS);
        if moved {
            std::fs::write(s.root.join("notes.txt"), "unrelated note\n").unwrap();
            s.git(&["commit", "-qm", "unrelated note", "--", "notes.txt"]);
        }
        let head = s.head();
        let index = std::fs::read(s.root.join(".git/index")).unwrap();
        let out = s.recover(json!({}), &[]);
        assert_eq!(read(&s.root, WIDGETS), candidate);
        if moved {
            assert_eq!(one_result(&out)["result"], "repair_required", "{out}");
            assert_eq!(s.records().len(), 1);
            assert_eq!(s.head(), head);
            assert_eq!(std::fs::read(s.root.join(".git/index")).unwrap(), index);
            assert!(s.events(record["op"].as_str().unwrap()).is_empty());
            let abandoned = s.recover(
                json!({"abandon":record["op"], "reason":"reviewed stale policy candidate"}),
                &[],
            );
            assert_eq!(one_result(&abandoned)["result"], "abandoned", "{abandoned}");
            assert_eq!(one_result(&abandoned)["applied"], true);
            assert!(s.records().is_empty());
            let events = s.events(record["op"].as_str().unwrap());
            assert_eq!(events.len(), 1);
            assert_eq!(events[0]["outcome"], "abandoned");
            assert_eq!(read(&s.root, WIDGETS), candidate);
            assert_eq!(s.head(), head);
            assert_eq!(std::fs::read(s.root.join(".git/index")).unwrap(), index);
        } else {
            assert_eq!(one_result(&out)["result"], "committed", "{out}");
            assert!(s.records().is_empty());
            let events = s.events(record["op"].as_str().unwrap());
            assert_eq!(events.len(), 1);
            assert_eq!(events[0]["outcome"], "committed");
            assert_eq!(events[0]["delete_receipt"]["parent"], head);
            let (code, verified) = s.run("verify-delete",
                json!({"parent":source,"candidate":candidate,"receipt":events[0]["delete_receipt"]}), &[]);
            assert_eq!(code, 0, "{verified}");
            let _ = s.recover(json!({}), &[]);
            assert_eq!(s.events(record["op"].as_str().unwrap()).len(), 1);
        }
    }
}

#[test]
fn prune_snapshot_aba_policy_uses_immutable_a_and_final_compare_names_outside_editor_boundary() {
    let s = archive_store("policy-aba");
    let ids = s.items();
    let (_, a) = s.set_docs(&format!("[ ] **Licence.** Waits item:{}.", ids[1]));
    assert_eq!(a["commit"]["status"], "committed", "{a}");
    let (_, snapshot) = s.run("prune-snapshot", json!({}), &[]);
    let bundle: markrivet_core::prune_snapshot::Snapshot =
        serde_json::from_value(snapshot["snapshot"].clone()).unwrap();
    bundle.verify().unwrap();
    let captured = read(&s.root, DOCS);
    let (_, b) = s.set_docs("[ ] **Licence.** No wait.");
    assert_eq!(b["commit"]["status"], "committed");
    // A policy reader consumes only immutable bytes even while live state is B.
    assert!(bundle.inputs[DOCS].contains(&ids[1]));
    let policy_holds = bundle.inputs[DOCS].contains(&format!("item:{}", ids[1]));
    let (_, a) = s.set_docs(&format!("[ ] **Licence.** Waits item:{}.", ids[1]));
    assert_eq!(a["commit"]["status"], "committed");
    assert_eq!(read(&s.root, DOCS), captured);
    let (_, end) = s.run("prune-snapshot", json!({}), &[]);
    assert_ne!(
        end["snapshot"]["revision"],
        snapshot["snapshot"]["revision"]
    );
    assert!(policy_holds); // The held target never reaches the destructive call.
    assert!(read(&s.root, WIDGETS).contains(&ids[1]));

    for point in ["policy_before_read", "policy_after_read"] {
        let s = archive_store(point);
        let ids = s.items();
        let (_, snapshot) = s.run("prune-snapshot", json!({}), &[]);
        let mut request = receipted_request(&s, &ids[1]);
        request["policy_guard"] = snapshot["snapshot"]["revision"].clone();
        let source = read(&s.root, WIDGETS);
        let pause = Pause::new(&s.root, point);
        let child = s.spawn("delete", &request, &[pause.env()]);
        pause.wait();
        let outside = format!("{}\nnew dependency item:{}\n", read(&s.root, DOCS), ids[1]);
        std::fs::write(s.root.join(DOCS), &outside).unwrap();
        pause.release();
        let (code, out) = finish(child);
        if point == "policy_before_read" {
            assert_eq!(code, 2, "{out}");
            assert_eq!(read(&s.root, WIDGETS), source);
        } else {
            assert_eq!(code, 0, "{out}");
            assert_eq!(out["status"], "applied");
        }
        assert_eq!(read(&s.root, DOCS), outside);
    }
}

#[test]
fn commit_an_emacs_lock_refusal_leaves_no_pending_record_and_no_commit() {
    let s = Store::new("commit-emacs", "skip");
    let ids = s.items();
    let base = s.head();
    let before = std::fs::read(s.root.join(WIDGETS)).unwrap();
    // The pending record is on disk before the publish step checks the lock.
    let pause = Pause::new(&s.root, "pending_written");
    let request = s.set_request(&ids[0], "[x] **Ship.** Past Emacs.");
    let writer = s.spawn("set", &request, &[pause.env()]);
    pause.wait();
    assert_eq!(s.records().len(), 1, "the record exists while paused");
    emacs_lock(&s.root, WIDGETS, &live_lock_text(), false);
    pause.release();
    let (code, out) = finish(writer);
    assert_eq!(code, 2, "{out}");
    assert_eq!(out["error"]["code"], "editor_lock", "{out}");
    assert!(out.get("pending").is_none(), "{out}");
    assert!(s.records().is_empty(), "{:?}", s.records());
    assert_eq!(s.head(), base, "no commit");
    assert_eq!(std::fs::read(s.root.join(WIDGETS)).unwrap(), before);
    let last = s.receipts().last().unwrap().clone();
    assert_eq!(last["outcome"], "not_applied", "{last}");
    assert!(temp_files(&s.root).is_empty());
}

impl Store {
    fn history_request(&self, id: &str, rev: Option<&str>) -> Value {
        let (code, log) = self.run(
            "item-log",
            json!({"path":WIDGETS,"collection":"actions","id":id}),
            &[],
        );
        assert_eq!(code, 0, "{log}");
        assert!(log["restore_guard"].is_string(), "{log}");
        let mut r = json!({"path":WIDGETS,"collection":"actions","id":id,"history_tip":log["history_tip"],"guard":log["restore_guard"],"session":"restore-caller"});
        if let Some(rev) = rev {
            r["rev"] = json!(rev);
        }
        r
    }
    fn history_fixture(name: &str, policy: &str) -> (Self, String, Value) {
        let s = Store::new(name, policy);
        let id = s.items()[0].clone();
        let old = s.head();
        let (code, out) = s.set(&id, "[x] **Ship.** Current content.", &[]);
        assert_eq!(code, 0, "{out}");
        let request = s.history_request(&id, Some(&old));
        (s, id, request)
    }
    fn history_refused(&self, request: Value, code: &str) {
        let before = read(&self.root, WIDGETS);
        let head = self.head();
        let (rc, out) = self.run("revert-item", request, &[]);
        assert_eq!(rc, 2, "{out}");
        assert_eq!(out["error"]["code"], code, "{out}");
        assert_eq!(read(&self.root, WIDGETS), before);
        assert_eq!(self.head(), head);
    }
}

#[test]
fn history_disjoint_restore_and_write() {
    let (s, id, request) = Store::history_fixture("history-disjoint", "refuse");
    let base = s.head();
    let sibling = s.items()[1].clone();
    let pause = Pause::new(&s.root, "planned");
    let first = s.spawn(
        "set",
        &s.set_request(&sibling, "[x] **Sibling.** Retained."),
        &[pause.env()],
    );
    pause.wait();
    let second = s.spawn("revert-item", &request, &[]);
    pause.release();
    let (a, oa) = finish(first);
    let (b, ob) = finish(second);
    assert_eq!((a, b), (0, 0), "{oa}\n{ob}");
    assert_eq!(s.since(&base).len(), 2);
    let text = read(&s.root, WIDGETS);
    assert!(text.contains("Retained."));
    assert!(!text.contains("Current content."));
    assert!(text.contains(&id));
    // Restoring the historical whole file would discard this sibling edit.
    let old = request["rev"].as_str().unwrap();
    let old_file = s.git(&["show", &format!("{old}:{WIDGETS}")]);
    assert_ne!(text, old_file);
    assert!(!old_file.contains("Retained."));
    // Compare every non-target item byte-for-byte with the sibling writer's commit.
    let prior = s.git(&["show", &format!("HEAD^:{WIDGETS}")]);
    let old_doc = markrivet_core::Document::parse(&prior).unwrap();
    let new_doc = markrivet_core::Document::parse(&text).unwrap();
    for other in s.items().into_iter().filter(|i| i != &id) {
        let sel = markrivet_core::Selector::Id { id: other };
        let a = old_doc.resolve(&sel).unwrap();
        let b = new_doc.resolve(&sel).unwrap();
        assert_eq!(old_doc.target_revision(a), new_doc.target_revision(b));
    }
}

#[test]
fn history_same_item_stale_guard() {
    let (s, id, request) = Store::history_fixture("history-stale", "refuse");
    let pause = Pause::new(&s.root, "planned");
    let writer = s.spawn(
        "set",
        &s.set_request(&id, "[ ] **Winner.** Fresh."),
        &[pause.env()],
    );
    pause.wait();
    let restore = s.spawn("revert-item", &request, &[]);
    pause.release();
    assert_eq!(finish(writer).0, 0);
    let before = read(&s.root, WIDGETS);
    let (code, out) = finish(restore);
    assert_eq!(code, 2, "{out}");
    assert_eq!(out["error"]["code"], "guard_conflict");
    assert_eq!(read(&s.root, WIDGETS), before);
}

#[test]
fn history_git_change_back_before_preflight() {
    for sibling_only in [false, true] {
        let (s, _, request) =
            Store::history_fixture(&format!("history-preflight-{sibling_only}"), "refuse");
        let pause = Pause::new(&s.root, "planned");
        let writer = s.spawn("revert-item", &request, &[pause.env()]);
        pause.wait();
        let original = read(&s.root, WIDGETS);
        if sibling_only {
            std::fs::write(s.root.join("notes.txt"), "independent commit\n").unwrap();
            s.git(&["commit", "-qm", "sibling", "--", "notes.txt"]);
        } else {
            std::fs::write(
                s.root.join(WIDGETS),
                original.replace("Current content.", "Transient content."),
            )
            .unwrap();
            s.git(&["commit", "-qm", "change", "--", WIDGETS]);
            std::fs::write(s.root.join(WIDGETS), &original).unwrap();
            s.git(&["commit", "-qm", "change back", "--", WIDGETS]);
        }
        let head = s.head();
        pause.release();
        let (code, out) = finish(writer);
        if sibling_only {
            assert_eq!(code, 0, "{out}");
            assert_ne!(s.head(), head);
        } else {
            assert_eq!(code, 2, "{out}");
            assert_eq!(out["error"]["code"], "history_changed");
            assert_eq!(read(&s.root, WIDGETS), original);
            assert_eq!(s.head(), head);
            assert!(s.records().is_empty());
        }
    }
    for point in ["history_preflight", "pending_written"] {
        let (s, _, request) = Store::history_fixture(&format!("history-final-{point}"), "refuse");
        let before = read(&s.root, WIDGETS);
        let pause = Pause::new(&s.root, point);
        let writer = s.spawn("revert-item", &request, &[pause.env()]);
        pause.wait();
        std::fs::write(s.root.join("notes.txt"), "moved\n").unwrap();
        s.git(&["commit", "-qm", "external Git", "--", "notes.txt"]);
        let head = s.head();
        pause.release();
        let (code, out) = finish(writer);
        assert_eq!(code, 2, "{out}");
        assert_eq!(out["error"]["code"], "history_changed");
        assert_eq!(read(&s.root, WIDGETS), before);
        assert_eq!(s.head(), head);
        assert!(s.records().is_empty());
        assert_eq!(s.receipts().last().unwrap()["applied"], false);
    }
}

#[test]
fn history_move_with_identical_item_bytes() {
    for variant in ["section", "ancestor", "list", "away-back", "repeated"] {
        let (s, id, request) = Store::history_fixture(&format!("history-move-{variant}"), "refuse");
        let original = read(&s.root, WIDGETS);
        let structural = item_guard(&s.root, WIDGETS, &id);
        let doc = markrivet_core::Document::parse(&original).unwrap();
        let target = doc
            .resolve(&markrivet_core::Selector::Id { id: id.clone() })
            .unwrap();
        let markrivet_core::document::Target::Item { list, item } = target else {
            panic!()
        };
        let span = doc.lists[list].items[item].span;
        let bytes = &original[span.start..span.end];
        let rest = format!("{}{}", &original[..span.start], &original[span.end..]);
        let moved = if variant == "ancestor" {
            format!(
                "# Parent\n<!-- stay:parent01 -->\n\n{}",
                original
                    .lines()
                    .map(|l| if l.starts_with('#') {
                        format!("#{l}\n")
                    } else {
                        format!("{l}\n")
                    })
                    .collect::<String>()
            )
        } else if variant == "list" {
            format!(
                "{}{}\nSeparate paragraph.\n\n{}",
                &original[..span.start],
                bytes,
                &original[span.end..]
            )
        } else {
            format!(
                "{rest}\n## {}\n<!-- stay:newsect1 -->\n\n{bytes}",
                if variant == "repeated" {
                    "Action"
                } else {
                    "Elsewhere"
                }
            )
        };
        std::fs::write(s.root.join(WIDGETS), &moved).unwrap();
        s.git(&["commit", "-qm", "move", "--", WIDGETS]);
        if variant != "list" {
            assert_eq!(item_guard(&s.root, WIDGETS, &id), structural, "{variant}");
        }
        if variant == "away-back" {
            std::fs::write(s.root.join(WIDGETS), &original).unwrap();
            s.git(&["commit", "-qm", "move back", "--", WIDGETS]);
            s.history_refused(request, "history_location_changed");
        } else {
            // The old location-bound guard must conflict even with byte-identical item content.
            let before = read(&s.root, WIDGETS);
            let (code, out) = s.run("revert-item", request, &[]);
            assert_eq!(code, 2, "{out}");
            assert!(
                matches!(
                    out["error"]["code"].as_str(),
                    Some(
                        "guard_conflict"
                            | "history_location_unsupported"
                            | "ambiguous_selector"
                            | "invalid_item"
                    )
                ),
                "{out}"
            );
            assert_eq!(read(&s.root, WIDGETS), before);
        }
    }
}

#[test]
fn history_outside_edit_before_rename() {
    for point in ["planned", "before_rename"] {
        for kind in ["prose", "duplicate", "relocate"] {
            let (s, id, request) =
                Store::history_fixture(&format!("history-outside-{point}-{kind}"), "refuse");
            let head = s.head();
            let pause = Pause::new(&s.root, point);
            let writer = s.spawn("revert-item", &request, &[pause.env()]);
            pause.wait();
            let before = read(&s.root, WIDGETS);
            let outside = match kind {
                "prose" => before + "\nExternal prose.\n",
                "duplicate" => {
                    before
                        + &format!(
                            "\n- [ ] Duplicate <!-- stay:{id} subhash=sha256:000000000000 -->\n"
                        )
                }
                "relocate" => {
                    let doc = markrivet_core::Document::parse(&before).unwrap();
                    let target = doc.resolve(&markrivet_core::Selector::Id { id }).unwrap();
                    let span = doc.target_span(target).0;
                    format!(
                        "{}{}\n## Elsewhere\n<!-- stay:outside1 -->\n\n{}\n",
                        &before[..span.start],
                        &before[span.end..],
                        span.slice(&before).unwrap()
                    )
                }
                _ => unreachable!(),
            };
            std::fs::write(s.root.join(WIDGETS), &outside).unwrap();
            pause.release();
            let (code, out) = finish(writer);
            assert_eq!(code, 2, "{out}");
            assert_eq!(out["error"]["code"], "external_change");
            assert_eq!(read(&s.root, WIDGETS), outside);
            assert_eq!(s.head(), head);
            assert!(s.records().is_empty());
        }
    }
}

#[test]
fn history_editor_lock_and_pending() {
    let (s, id, request) = Store::history_fixture("history-editor", "refuse");
    let pause = Pause::new(&s.root, "planned");
    let writer = s.spawn("revert-item", &request, &[pause.env()]);
    pause.wait();
    let link = emacs_lock(&s.root, WIDGETS, &live_lock_text(), false);
    pause.release();
    let before = read(&s.root, WIDGETS);
    let (code, out) = finish(writer);
    assert_eq!(code, 2, "{out}");
    assert_eq!(out["error"]["code"], "editor_lock");
    assert_eq!(read(&s.root, WIDGETS), before);
    std::fs::remove_file(link).unwrap();
    let pause = Pause::new(&s.root, "pending_written");
    let mut writer = s.spawn("revert-item", &request, &[pause.env()]);
    pause.wait();
    writer.kill().unwrap();
    writer.wait().unwrap();
    let _ = std::fs::remove_file(s.root.join(".rivet/lock"));
    let all = history_tree(&s.root);
    let mut preview = request.clone();
    preview["preview"] = json!(true);
    let (code, out) = s.run("revert-item", preview, &[]);
    assert_eq!(code, 2, "{out}");
    assert_eq!(out["error"]["code"], "pending_unresolved");
    assert_eq!(history_tree(&s.root), all);
    s.run(
        "item-log",
        json!({"path":WIDGETS,"collection":"actions","id":id}),
        &[],
    );
    assert_eq!(history_tree(&s.root), all);
    s.history_refused(request, "pending_unresolved");
    assert_eq!(s.records().len(), 1);
}

fn history_tree(root: &Path) -> std::collections::BTreeMap<std::path::PathBuf, Vec<u8>> {
    fn walk(
        root: &Path,
        dir: &Path,
        out: &mut std::collections::BTreeMap<std::path::PathBuf, Vec<u8>>,
    ) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            let rel = path.strip_prefix(root).unwrap().to_path_buf();
            if entry.file_type().unwrap().is_dir() {
                out.insert(rel, vec![]);
                walk(root, &path, out);
            } else {
                out.insert(rel, std::fs::read(&path).unwrap());
            }
        }
    }
    let mut out = std::collections::BTreeMap::new();
    walk(root, root, &mut out);
    out
}

#[test]
fn history_fault_before_and_after_publication() {
    for point in ["before_rename", "after_rename", "applied"] {
        let (s, _, request) = Store::history_fixture(&format!("history-crash-{point}"), "refuse");
        let before = read(&s.root, WIDGETS);
        let head = s.head();
        s.kill_at(point, "revert-item", &request, &[]);
        let record = s.record();
        assert!(record["delete_receipt"].is_null());
        assert!(
            record["message"]
                .as_str()
                .unwrap()
                .contains(request["rev"].as_str().unwrap())
        );
        assert_eq!(record["session"], "restore-caller");
        if point == "before_rename" {
            assert_eq!(read(&s.root, WIDGETS), before);
        } else {
            assert_ne!(read(&s.root, WIDGETS), before);
        }
        let op = record["op"].as_str().unwrap();
        let _ = s.recover(json!({}), &[]);
        assert!(s.records().is_empty());
        assert_eq!(s.events(op).len(), 1);
        let _ = s.recover(json!({}), &[]);
        assert_eq!(s.events(op).len(), 1);
        if point == "before_rename" {
            assert_eq!(s.head(), head);
        } else {
            let message = s.message(&s.head());
            assert!(message.contains(request["rev"].as_str().unwrap()));
            assert!(!message.contains("Rivet-Delete:"));
        }
    }
    for fault in ["rename", "dir_sync"] {
        let (s, _, request) = Store::history_fixture(&format!("history-fault-{fault}"), "refuse");
        let before = read(&s.root, WIDGETS);
        let (code, out) = s.run(
            "revert-item",
            request,
            &[("RIVET_TEST_FAULT", fault.into())],
        );
        if fault == "rename" {
            assert_eq!(code, 2, "{out}");
            assert_eq!(read(&s.root, WIDGETS), before);
        } else {
            assert_eq!(code, 3, "{out}");
            assert_eq!(out["durable"], false);
            assert_ne!(read(&s.root, WIDGETS), before);
            assert_eq!(s.records().len(), 1);
            s.recover(json!({}), &[]);
            assert!(s.records().is_empty());
        }
    }
}

#[test]
fn history_dirty_policies_and_noop() {
    for policy in ["skip", "adopt", "refuse"] {
        let (s, _, request) = Store::history_fixture(&format!("history-dirty-{policy}"), policy);
        let before = read(&s.root, WIDGETS) + "\nOutside bytes.\n";
        std::fs::write(s.root.join(WIDGETS), &before).unwrap();
        let head = s.head();
        let (code, out) = s.run("revert-item", request, &[]);
        if policy == "refuse" {
            assert_eq!(code, 2, "{out}");
            assert_eq!(read(&s.root, WIDGETS), before);
            assert_eq!(s.head(), head);
        } else {
            assert_eq!(code, 0, "{out}");
            assert!(read(&s.root, WIDGETS).ends_with("\nOutside bytes.\n"));
            assert_eq!(
                out["commit"]["status"],
                if policy == "skip" {
                    "skipped"
                } else {
                    "committed"
                }
            );
        }
    }
}

#[test]
fn history_failed_commit_and_receipt_failure_recover_once() {
    for receipt_failure in [false, true] {
        let (s, _, request) =
            Store::history_fixture(&format!("history-outcome-{receipt_failure}"), "refuse");
        let before = read(&s.root, WIDGETS);
        let extra = if receipt_failure {
            vec![("RIVET_TEST_FAULT", "receipt_append".into())]
        } else {
            vec![
                s.script(
                    "history-slow",
                    "case \"$*\" in *\" update-ref --stdin \"*) sleep 30;; esac\nexec git \"$@\"",
                ),
                ("RIVET_TEST_GIT_DEADLINE_MS", "300".into()),
            ]
        };
        let (code, out) = s.run("revert-item", request.clone(), &extra);
        assert_eq!(code, 0, "{out}");
        assert_eq!(out["status"], "applied");
        assert_ne!(read(&s.root, WIDGETS), before);
        if receipt_failure {
            assert_eq!(out["receipt"], "failed");
        } else {
            assert_eq!(out["commit"]["status"], "failed");
        }
        let rec = s.record();
        let op = rec["op"].as_str().unwrap();
        let recovered = s.recover(json!({}), &[]);
        assert!(
            s.records().is_empty(),
            "{recovered}; pending: {:?}",
            s.records()
        );
        let count = s.events(op).len();
        s.recover(json!({}), &[]);
        assert_eq!(s.events(op).len(), count);
        assert_eq!(s.carrying(op).len(), 1);
        let message = s.message(&s.head());
        assert!(message.contains(request["rev"].as_str().unwrap()));
        assert!(!message.contains("Rivet-Delete:"));
    }
}

/// Mask precisely the permitted item and ancestor evidence marker spans.
fn history_outside_footprint(source: &str, id: &str) -> String {
    use markrivet_core::{Document, Selector, document::Target};
    let doc = Document::parse(source).unwrap();
    let target = doc.resolve(&Selector::Id { id: id.into() }).unwrap();
    let Target::Item { list, .. } = target else {
        panic!()
    };
    let mut spans = vec![doc.target_span(target).0];
    for &m in &doc.blocks[doc.lists[list].block].markers {
        spans.push(doc.markers[m].lexical.span);
    }
    let mut owner = doc.section_of(doc.blocks[doc.lists[list].block].span.start);
    while let Some(s) = owner {
        for &m in &doc.blocks[doc.sections[s].block].markers {
            spans.push(doc.markers[m].lexical.span);
        }
        owner = doc.sections[s].parent;
    }
    spans.sort_by_key(|s| s.start);
    let mut rest = String::new();
    let mut at = 0;
    for span in spans {
        rest.push_str(&source[at..span.start]);
        at = span.end;
    }
    rest.push_str(&source[at..]);
    rest
}

#[test]
fn history_restore_attachment_and_refusals() {
    let (s, id, request) = Store::history_fixture("history-footprint", "refuse");
    let sibling = s.items()[1].clone();
    let (code, out) = s.set(&sibling, "[ ] **Sibling.** Keep exact bytes.", &[]);
    assert_eq!(code, 0, "{out}");
    let before = read(&s.root, WIDGETS);
    let before_doc = markrivet_core::Document::parse(&before).unwrap();
    let (code, out) = s.run("revert-item", request, &[]);
    assert_eq!(code, 0, "{out}");
    let after = read(&s.root, WIDGETS);
    let after_doc = markrivet_core::Document::parse(&after).unwrap();
    assert_eq!(
        history_outside_footprint(&before, &id),
        history_outside_footprint(&after, &id)
    );
    for (stay, targets) in &before_doc.ids {
        assert_eq!(targets.len(), after_doc.ids[stay].len());
        let selector = markrivet_core::Selector::Id { id: stay.clone() };
        let a = before_doc.resolve(&selector).unwrap();
        let b = after_doc.resolve(&selector).unwrap();
        assert_eq!(std::mem::discriminant(&a), std::mem::discriminant(&b));
        assert_eq!(before_doc.target_ids(a), after_doc.target_ids(b));
        if stay == &id {
            assert_ne!(before_doc.target_revision(a), after_doc.target_revision(b));
        } else if matches!(a, markrivet_core::document::Target::Item { .. }) {
            assert_eq!(
                before_doc.target_span(a).0.slice(&before),
                after_doc.target_span(b).0.slice(&after)
            );
        }
    }
    // Negative control: whole-file restoration loses the independently committed sibling.
    let old = s.git(&[
        "show",
        &format!("{}:{WIDGETS}", out["resolved_rev"].as_str().unwrap()),
    ]);
    assert_ne!(
        history_outside_footprint(&old, &id),
        history_outside_footprint(&before, &id)
    );
}

#[test]
fn history_preview_without_lock_file() {
    let (s, _, mut request) = Store::history_fixture("history-preview-pure", "refuse");
    let _ = std::fs::remove_file(s.root.join(".rivet/lock"));
    request["preview"] = json!(true);
    let before = history_tree(&s.root);
    let (code, out) = s.run("revert-item", request.clone(), &[]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(history_tree(&s.root), before);
    request["guard"] = json!("rivet:v1:item:stale");
    let (code, out) = s.run("revert-item", request, &[]);
    assert_eq!(code, 2, "{out}");
    assert_eq!(out["error"]["code"], "guard_conflict");
    assert_eq!(history_tree(&s.root), before);
}

#[test]
fn history_staged_skip_keeps_standard_ineligible_outcome() {
    let (s, _, request) = Store::history_fixture("history-staged-skip", "skip");
    let text = read(&s.root, WIDGETS) + "\nStaged outside prose.\n";
    std::fs::write(s.root.join(WIDGETS), &text).unwrap();
    s.git(&["add", "--", WIDGETS]);
    let index = std::fs::read(s.root.join(".git/index")).unwrap();
    let head = s.head();
    let (code, out) = s.run("revert-item", request, &[]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["commit"]["status"], "ineligible");
    assert_eq!(out["commit"]["reason"], "staged");
    assert!(read(&s.root, WIDGETS).ends_with("\nStaged outside prose.\n"));
    assert_eq!(std::fs::read(s.root.join(".git/index")).unwrap(), index);
    assert_eq!(s.head(), head);
}

#[test]
fn history_both_writers_queue_behind_a_third_lock_holder() {
    let (s, _, request) = Store::history_fixture("history-queue", "refuse");
    let base = s.head();
    let sibling = s.items()[1].clone();
    let pause = Pause::new(&s.root, "locked");
    let holder_request = s.set_request(&s.items()[2], "[x] **Third writer.** Holds the lock.");
    let holder = s.spawn("set", &holder_request, &[pause.env()]);
    pause.wait();
    let first = s.spawn(
        "set",
        &s.set_request(&sibling, "[x] **Sibling.** Queued."),
        &[],
    );
    let second = s.spawn("revert-item", &request, &[]);
    pause.release();
    for writer in [holder, first, second] {
        let (code, out) = finish(writer);
        assert_eq!(code, 0, "{out}");
        assert_eq!(out["commit"]["status"], "committed");
    }
    let text = read(&s.root, WIDGETS);
    assert!(text.contains("Queued."));
    assert!(text.contains("Holds the lock."));
    assert!(!text.contains("Current content."));
    assert_eq!(s.since(&base).len(), 3);
}

#[test]
fn history_control_character_paths_cannot_forge_trailers_and_recover() {
    for recover in [false, true] {
        let s = Store::new(&format!("history-path-trailers-{recover}"), "refuse");
        let path = "tracking/odd\nRivet-Op: forged\nRivet-Delete: forged\nname.md";
        std::fs::write(s.root.join(path), read(&s.root, WIDGETS)).unwrap();
        s.git(&["add", "--", path]);
        s.git(&["commit", "-qm", "literal synthetic filename"]);
        let source = s.head();
        let id = s.items()[0].clone();
        let request = json!({"path":path,"select":{"kind":"id","id":id},"guard":item_guard(&s.root,path,&id),"content":"[x] **Current.** Changed."});
        let (code, out) = s.run("set", request, &[]);
        assert_eq!(code, 0, "{out}");
        assert_eq!(out["commit"]["status"], "committed");
        let (code, log) = s.run(
            "item-log",
            json!({"path":path,"collection":"actions","id":id}),
            &[],
        );
        assert_eq!(code, 0, "{log}");
        let r = json!({"path":path,"collection":"actions","id":id,"history_tip":log["history_tip"],"guard":log["restore_guard"],"rev":source});
        if recover {
            s.kill_at("applied", "revert-item", &r, &[]);
            let out = s.recover(json!({}), &[]);
            assert_eq!(one_result(&out)["result"], "committed", "{out}");
        } else {
            let (code, out) = s.run("revert-item", r, &[]);
            assert_eq!(code, 0, "{out}");
            assert_eq!(out["commit"]["status"], "committed", "{out}");
        }
        let message = s.message(&s.head());
        assert_eq!(
            message
                .lines()
                .filter(|l| l.starts_with("Rivet-Op:"))
                .count(),
            1
        );
        assert!(!message.lines().any(|l| l.starts_with("Rivet-Delete:")));
        assert!(message.contains(&source));
        assert!(s.records().is_empty());
    }
}

#[test]
fn delete_collection_rechecks_a_moved_item_after_waiting_for_the_writer_lock() {
    let root = project("delete-queue-race");
    let config_path = root.join(".rivet/project.json");
    let mut config: Value = serde_json::from_slice(&std::fs::read(&config_path).unwrap()).unwrap();
    config["collections"]["actions"]["subsections"] = json!(true);
    config["collections"]["actions"]["exclude"] = json!(["Plans"]);
    std::fs::write(config_path, serde_json::to_vec(&config).unwrap()).unwrap();
    std::fs::write(root.join(WIDGETS), "# Synthetic\n\n## Action\n\n### Batch\n\n- [ ] Move me. <!-- stay:Moving01 subhash=sha256:111111111111 -->\n- [ ] Batch keep. <!-- stay:BatchKe1 subhash=sha256:111111111111 -->\n\n### Plans\n\n- [ ] Plan keep. <!-- stay:PlanKee1 subhash=sha256:111111111111 -->\n\n## Log\n\nPreserve this suffix.\n").unwrap();
    stamp(&root, WIDGETS);
    let guard = item_guard(&root, WIDGETS, "Moving01");
    let select = json!({"kind":"heading_path","path":["Action","Batch"]});
    let (_, got) = rivet(&root, "get", json!({"path":WIDGETS,"select":select}));
    // Renaming the containing batch to an excluded title moves all its items
    // out of the queue without changing any native item revision.
    let ops = json!([{"op":"set","select":select,"part":"title","guard":got["revision"],"content":"Plans"}]);
    // Keep the old membership visible until deletion reaches the lock wait.
    // Then observe the published mover bytes while it still holds the lock.
    let pause = Pause::new(&root, "before_rename");
    let published = Pause {
        dir: pause.dir.clone(),
        point: "after_rename",
    };
    let both = (
        "RIVET_TEST_PAUSE",
        format!("before_rename,after_rename:{}", pause.dir.display()),
    );
    let mover = spawn(&root, "write", &json!({"path":WIDGETS,"ops":ops}), &[both]);
    pause.wait();
    let old_membership = read(&root, WIDGETS);
    let deleter = spawn(
        &root,
        "delete",
        &json!({"path":WIDGETS,"select":{"kind":"id","id":"Moving01"},"guard":guard,"collection":"actions"}),
        &[],
    );
    // Opening this descriptor occurs inside Project::lock. Any membership
    // check incorrectly placed before lock acquisition has already run.
    wait_for_writer_lock(&root, &deleter);
    assert_eq!(read(&root, WIDGETS), old_membership);
    pause.release();
    published.wait();
    let before_delete = read(&root, WIDGETS);
    assert_eq!(item_guard(&root, WIDGETS, "Moving01"), guard);
    published.release();
    let (code, out) = finish(mover);
    assert_eq!(code, 0, "{out}");
    let (code, out) = finish(deleter);
    assert_eq!(code, 2, "{out}");
    assert_eq!(out["error"]["code"], "collection_conflict");
    assert_eq!(read(&root, WIDGETS), before_delete);
    let (_, rows) = rivet(
        &root,
        "query",
        json!({"collection":"actions","paths":[WIDGETS]}),
    );
    assert!(
        rows["items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|row| row["id"] != "Moving01")
    );
}

#[test]
fn delete_collection_precondition_keeps_the_committed_receipt_verifiable() {
    let s = Store::new("delete-queue-receipt", "refuse");
    let ids = s.items();
    let before = read(&s.root, WIDGETS);
    let mut request = receipted_request(&s, &ids[1]);
    request["collection"] = json!("actions");
    let (code, out) = s.run("delete", request, &[]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["commit"]["status"], "committed");
    let message = s.git(&["log", "-1", "--format=%B"]);
    let payload = message
        .lines()
        .find_map(|line| line.strip_prefix("Rivet-Delete: "))
        .unwrap();
    let receipt = markrivet_core::delete_receipt::DeleteReceipt::decode(payload).unwrap();
    let (code, verified) = s.run(
        "verify-delete",
        json!({"parent":before,"candidate":read(&s.root,WIDGETS),"receipt":receipt}),
        &[],
    );
    assert_eq!(code, 0, "{verified}");
    assert_eq!(verified["dropped"], json!([ids[1]]));
}

#[test]
fn transaction_every_writer_checks_messages_before_waiting_for_the_lock() {
    let s = archive_store("transaction-message-lock");
    let id = &s.items()[0];
    let requests = [
        ("set", s.set_request(id, "[x] **Ship.** Changed.")),
        ("create", create_request(NEW_CONTENT)),
        ("adopt", json!({"path":WIDGETS})),
        ("archive-copy", archive_request(&s, id)),
        ("revert-item", s.history_request(id, None)),
    ];
    let pause = Pause::new(&s.root, "locked");
    let holder = s.spawn(
        "set",
        &s.set_request(id, "[x] **Ship.** Holder."),
        &[pause.env()],
    );
    pause.wait();
    let before = read(&s.root, WIDGETS);
    let archive = read(&s.root, "archive/month/closed.md");
    let head = s.head();
    for (cmd, mut request) in requests {
        request["message"] = json!("forged\n\nRivet-Op: forbidden");
        request["lock_timeout_ms"] = json!(20);
        let (code, out) = s.run(cmd, request.clone(), &[]);
        assert_eq!(code, 2, "{cmd}: {out}");
        assert_eq!(out["error"]["code"], "invalid_request", "{cmd}: {out}");
        request.as_object_mut().unwrap().remove("message");
        let (code, out) = s.run(cmd, request, &[]);
        assert_eq!(code, 2, "{cmd}: {out}");
        assert_eq!(out["error"]["code"], "lock_timeout", "{cmd}: {out}");
        assert_eq!(read(&s.root, WIDGETS), before);
        assert_eq!(read(&s.root, "archive/month/closed.md"), archive);
        assert!(!s.root.join(NEW).exists());
        assert_eq!(s.head(), head);
    }
    pause.release();
    assert_eq!(finish(holder).0, 0);
}

#[test]
fn transaction_every_writer_keeps_preimages_and_stay_attachments_on_rename_failure() {
    for cmd in ["set", "create", "adopt", "archive-copy", "revert-item"] {
        let s = archive_store(&format!("transaction-rename-{cmd}"));
        let id = &s.items()[0];
        let old = s.head();
        let request = match cmd {
            "set" => s.set_request(id, "[x] **Ship.** Changed."),
            "create" => create_request(NEW_CONTENT),
            "adopt" => {
                add_by_hand(&s);
                json!({"path":WIDGETS})
            }
            "archive-copy" => archive_request(&s, id),
            "revert-item" => {
                assert_eq!(s.set(id, "[x] **Ship.** Current.", &[]).0, 0);
                s.history_request(id, Some(&old))
            }
            _ => unreachable!(),
        };
        let before = std::fs::read(s.root.join(WIDGETS)).unwrap();
        let archive = std::fs::read(s.root.join("archive/month/closed.md")).unwrap();
        let head = s.head();
        let index = s.git(&["ls-files", "-s"]);
        let (_, map_before) = s.run("map", json!({"path":WIDGETS}), &[]);
        let (code, out) = s.run(cmd, request, &[("RIVET_TEST_FAULT", "rename".into())]);
        assert_eq!(code, 2, "{cmd}: {out}");
        assert_eq!(out["error"]["code"], "io", "{cmd}: {out}");
        assert_eq!(
            std::fs::read(s.root.join(WIDGETS)).unwrap(),
            before,
            "{cmd}"
        );
        assert_eq!(
            std::fs::read(s.root.join("archive/month/closed.md")).unwrap(),
            archive,
            "{cmd}"
        );
        assert!(!s.root.join(NEW).exists(), "{cmd}");
        assert_eq!(s.head(), head, "{cmd}");
        assert_eq!(s.git(&["ls-files", "-s"]), index, "{cmd}");
        let (_, map_after) = s.run("map", json!({"path":WIDGETS}), &[]);
        assert_eq!(
            map_after, map_before,
            "{cmd}: attachments and revisions survive refusal"
        );
    }
}

#[test]
fn transaction_archive_binding_is_source_only_for_fresh_copy_and_proven_retry() {
    let s = archive_store("transaction-archive-binding");
    let id = &s.items()[0];
    let request = archive_request(&s, id);
    let config_path = s.root.join(".rivet/project.json");
    let mut config: Value = serde_json::from_slice(&std::fs::read(&config_path).unwrap()).unwrap();
    config["collections"]["archive_only"] = json!({
        "schema":"missing@1", "files":["archive/**/*.md"], "section":"Closed"
    });
    std::fs::write(config_path, config.to_string()).unwrap();
    s.git(&["add", ".rivet/project.json"]);
    s.git(&["commit", "-qm", "unrelated unavailable archive schema"]);
    let before = read(&s.root, WIDGETS);
    let destination_before = read(&s.root, "archive/month/closed.md");
    let (code, first) = s.run("archive-copy", request.clone(), &[]);
    assert_eq!(code, 0, "{first}");
    assert_eq!(first["commit"]["status"], "committed", "{first}");
    assert_eq!(read(&s.root, WIDGETS), before);
    let destination = read(&s.root, "archive/month/closed.md");
    assert!(destination.starts_with(&destination_before));
    let head = s.head();
    let mut retry_request = request;
    // A proven retry precedes selector and destination guard validation too.
    retry_request["select"] = json!({"kind":"id", "id":"missing"});
    let (code, retry) = s.run("archive-copy", retry_request, &[]);
    assert_eq!(code, 0, "{retry}");
    assert_eq!(retry["status"], "unchanged", "{retry}");
    assert_eq!(retry["commit"]["sha"], first["commit"]["sha"]);
    assert_eq!(read(&s.root, "archive/month/closed.md"), destination);
    assert_eq!(read(&s.root, WIDGETS), before);
    assert_eq!(s.head(), head);
}

#[test]
fn transaction_resolves_bindings_after_waiting_for_the_writer_lock() {
    let (root, open) = stamped("transaction-locked-binding");
    let pause = Pause::new(&root, "planned");
    let holder = spawn(
        &root,
        "set",
        &set_item(
            &open[0].1,
            &item_guard(&root, WIDGETS, &open[0].1),
            "[x] **Ship.** Holder.",
        ),
        &[pause.env()],
    );
    pause.wait();
    let creator = spawn(&root, "create", &create_request(NEW_CONTENT), &[]);
    wait_for_writer_lock(&root, &creator);
    let config_path = root.join(".rivet/project.json");
    let mut config: Value = serde_json::from_slice(&std::fs::read(&config_path).unwrap()).unwrap();
    config["collections"]["actions"]["schema"] = json!("missing@1");
    std::fs::write(config_path, config.to_string()).unwrap();
    pause.release();
    let (code, out) = finish(holder);
    assert_eq!(code, 0, "{out}");
    let before = read(&root, WIDGETS);
    let (code, out) = finish(creator);
    assert_eq!(code, 2, "{out}");
    assert_eq!(out["error"]["code"], "invalid_schema", "{out}");
    assert_eq!(read(&root, WIDGETS), before);
    assert!(!root.join(NEW).exists());
}

#[test]
fn transaction_every_writer_meets_pending_ownership_before_loading_or_planning() {
    let s = archive_store("transaction-pending");
    let id = &s.items()[0];
    let requests = [
        ("set", s.set_request(id, "[x] **Ship.** Changed.")),
        ("create", create_request(NEW_CONTENT)),
        ("adopt", json!({"path":WIDGETS})),
        ("archive-copy", archive_request(&s, id)),
        ("revert-item", s.history_request(id, None)),
    ];
    std::fs::create_dir_all(s.root.join(".rivet/pending")).unwrap();
    let record = s.root.join(".rivet/pending/broken.json");
    std::fs::write(&record, "{\"op\":").unwrap();
    let before = read(&s.root, WIDGETS);
    let archive = read(&s.root, "archive/month/closed.md");
    let head = s.head();
    for (cmd, request) in requests {
        let (code, out) = s.run(cmd, request, &[]);
        assert_eq!(code, 2, "{cmd}: {out}");
        assert_eq!(out["error"]["code"], "pending_unresolved", "{cmd}: {out}");
        assert_eq!(read(&s.root, WIDGETS), before);
        assert_eq!(read(&s.root, "archive/month/closed.md"), archive);
        assert!(!s.root.join(NEW).exists());
        assert_eq!(s.head(), head);
        assert_eq!(std::fs::read_to_string(&record).unwrap(), "{\"op\":");
    }
}

#[test]
fn transaction_exposes_the_locked_and_planned_debug_boundaries() {
    let (root, open) = stamped("transaction-hooks");
    let pause = Pause::new(&root, "locked,planned");
    for point in ["locked", "planned"] {
        std::fs::write(pause.dir.join(format!("{point}.go")), "").unwrap();
    }
    let request = set_item(
        &open[0].1,
        &item_guard(&root, WIDGETS, &open[0].1),
        "[x] **Ship.** Boundaries.",
    );
    let (code, out) = finish(spawn(&root, "set", &request, &[pause.env()]));
    assert_eq!(code, 0, "{out}");
    for point in ["locked", "planned"] {
        assert!(
            pause.dir.join(format!("{point}.reached")).exists(),
            "missing {point} boundary"
        );
    }
    // The boundary assertions prove hook observability. The lock-wait and
    // rename-fault tests above independently prove locking and byte safety.
    assert!(read(&root, WIDGETS).contains("Boundaries."));
}

fn wait_for_writer_lock(root: &Path, child: &std::process::Child) {
    let descriptors = format!("/proc/{}/fd", child.id());
    let lock_path = root.join(".rivet/lock");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let waiting = std::fs::read_dir(&descriptors)
            .unwrap()
            .any(|fd| std::fs::read_link(fd.unwrap().path()).is_ok_and(|path| path == lock_path));
        if waiting {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "writer never entered the lock wait"
        );
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}

#[test]
fn prune_archive_duplicate_committed_after_decision_refuses_before_source_removal() {
    let s = archive_store("archive-policy-duplicate");
    let path = s.root.join(".rivet/project.json");
    let mut config: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    config["collections"]["secondary"] = config["collections"]["actions"].clone();
    config["collections"]["secondary"]["archive"] = json!(["other-archive/*.md"]);
    std::fs::write(path, serde_json::to_vec(&config).unwrap()).unwrap();
    std::fs::create_dir_all(s.root.join("other-archive")).unwrap();
    let (code, created) = s.run(
        "create",
        json!({"path":"other-archive/closed.md",
        "content":"# Other archive\n\n## Closed\n","stamp":true}),
        &[],
    );
    assert_eq!(code, 0, "{created}");
    s.git(&["add", ".rivet/project.json"]);
    s.git(&[
        "commit",
        "-qm",
        "configure separate synthetic archive namespace",
    ]);
    let ids = s.items();
    let (code, done) = s.set(&ids[1], "[x] **Refresh.** Done synthetic dependency.", &[]);
    assert_eq!(code, 0, "{done}");
    let (code, waiting) = s.set(
        &ids[0],
        &format!("[ ] **Ship.**\n  **Waits**: item:{}", ids[1]),
        &[],
    );
    assert_eq!(code, 0, "{waiting}");
    let (code, copy) = s.run("archive-copy", archive_request(&s, &ids[1]), &[]);
    assert_eq!(code, 0, "{copy}");
    let (code, snapshot) = s.run("prune-snapshot", json!({}), &[]);
    assert_eq!(code, 0, "{snapshot}");
    let mut request = receipted_request(&s, &ids[1]);
    request["policy_guard"] = snapshot["snapshot"]["revision"].clone();
    request["archive"] = json!({"path":"archive/month/closed.md",
        "copy_op":copy["commit"]["copy_op"],"commit":copy["commit"]["sha"]});
    let select = json!({"kind":"heading_path","path":["Closed"]});
    let (_, dest) = s.run(
        "get",
        json!({"path":"other-archive/closed.md","select":select}),
        &[],
    );
    let mut second = archive_request(&s, &ids[1]);
    second["path"] = json!("other-archive/closed.md");
    second["destination_guard"] = dest["revision"].clone();
    // A second cooperating process finishes after the caller's decision, before
    // the deleting process takes the lock. Distinct collection archive scopes
    // allow both authentic copies; the external reader unions those scopes.
    let (code, duplicate) = s.run("archive-copy", second, &[]);
    assert_eq!(code, 0, "{duplicate}");
    assert_eq!(duplicate["commit"]["status"], "committed", "{duplicate}");
    let source = read(&s.root, WIDGETS);
    let first = read(&s.root, "archive/month/closed.md");
    let other = read(&s.root, "other-archive/closed.md");
    assert!(first.contains(&format!("stay:{}", ids[1])));
    assert!(other.contains(&format!("stay:{}", ids[1])));
    let head = s.head();
    let index = std::fs::read(s.root.join(".git/index")).unwrap();
    let (code, out) = s.run("delete", request, &[]);
    assert_eq!(code, 2, "stale archive decision removed source: {out}");
    assert_eq!(out["error"]["code"], "policy_conflict", "{out}");
    assert_eq!(read(&s.root, WIDGETS), source);
    assert_eq!(read(&s.root, "archive/month/closed.md"), first);
    assert_eq!(read(&s.root, "other-archive/closed.md"), other);
    assert_eq!(s.head(), head);
    assert_eq!(std::fs::read(s.root.join(".git/index")).unwrap(), index);
    assert!(s.records().is_empty());
}

#[test]
fn prune_archive_changes_and_gaps_are_checked_under_the_delete_lock() {
    use std::os::unix::ffi::OsStringExt;
    use std::os::unix::fs::{PermissionsExt, symlink};
    for change in [
        "bytes",
        "append",
        "unrelated-commit",
        "add",
        "add-first",
        "remove-first",
        "remove",
        "aba",
        "symlink",
        "directory-symlink",
        "hidden",
        "target",
        "node_modules",
        "native",
        "unreadable",
        "helper",
        "native-helper",
    ] {
        if change == "unreadable" && unsafe { libc::geteuid() } == 0 {
            continue;
        }
        let s = archive_store(&format!("archive-locked-{change}"));
        if change == "remove-first" {
            std::fs::write(s.root.join("archive/month/a.md"), "# Earlier archive\n").unwrap();
            s.git(&["add", "archive/month/a.md"]);
            s.git(&["commit", "-qm", "synthetic earlier archive member"]);
        }
        let ids = s.items();
        let (_, snapshot) = s.run("prune-snapshot", json!({}), &[]);
        let mut request = receipted_request(&s, &ids[1]);
        request["policy_guard"] = snapshot["snapshot"]["revision"].clone();
        let source = read(&s.root, WIDGETS);
        let archive = read(&s.root, "archive/month/closed.md");
        let pause = Pause::new(&s.root, "policy_before_read");
        let writer = s.spawn("delete", &request, &[pause.env()]);
        pause.wait();
        // Only raw filesystem/Git writers can mutate while the project lock is held.
        match change {
            "bytes" => std::fs::write(
                s.root.join("archive/month/closed.md"),
                format!("{archive}\nchanged\n"),
            )
            .unwrap(),
            "append" => {
                std::fs::write(
                    s.root.join("archive/month/closed.md"),
                    format!("{archive}\nharmless append\n"),
                )
                .unwrap();
                s.git(&[
                    "commit",
                    "-qm",
                    "later archive append",
                    "--",
                    "archive/month/closed.md",
                ]);
            }
            "unrelated-commit" => {
                std::fs::write(s.root.join("notes.txt"), "unrelated HEAD movement\n").unwrap();
                s.git(&[
                    "commit",
                    "-qm",
                    "unrelated HEAD movement",
                    "--",
                    "notes.txt",
                ]);
            }
            "add" => {
                std::fs::write(s.root.join("archive/month/new.md"), "# New archive\n").unwrap()
            }
            "add-first" => {
                std::fs::write(s.root.join("archive/month/a.md"), "# Earlier archive\n").unwrap()
            }
            "remove-first" => std::fs::remove_file(s.root.join("archive/month/a.md")).unwrap(),
            "remove" => std::fs::remove_file(s.root.join("archive/month/closed.md")).unwrap(),
            "aba" => {
                std::fs::write(
                    s.root.join("archive/month/closed.md"),
                    format!("{archive}\nB\n"),
                )
                .unwrap();
                s.git(&[
                    "commit",
                    "-qm",
                    "archive B",
                    "--",
                    "archive/month/closed.md",
                ]);
                std::fs::write(s.root.join("archive/month/closed.md"), &archive).unwrap();
                s.git(&[
                    "commit",
                    "-qm",
                    "archive A again",
                    "--",
                    "archive/month/closed.md",
                ]);
            }
            "symlink" => symlink("missing", s.root.join("archive/month/linked.md")).unwrap(),
            "directory-symlink" => symlink("missing", s.root.join("archive/subtree")).unwrap(),
            "hidden" | "target" | "node_modules" => {
                let name = if change == "hidden" {
                    ".hidden"
                } else {
                    change
                };
                std::fs::create_dir(s.root.join("archive").join(name)).unwrap();
                std::fs::write(
                    s.root.join("archive").join(name).join("new.md"),
                    "# Untracked archive\n",
                )
                .unwrap();
            }
            "native" => {
                let name = std::ffi::OsString::from_vec(b"archive/month/\xff.md".to_vec());
                std::fs::write(s.root.join(name), "# Native\n").unwrap();
            }
            "unreadable" => {
                std::fs::create_dir(s.root.join("archive/subtree")).unwrap();
                std::fs::set_permissions(
                    s.root.join("archive/subtree"),
                    std::fs::Permissions::from_mode(0o0),
                )
                .unwrap();
            }
            "helper" => symlink("missing", s.root.join("save.sh")).unwrap(),
            "native-helper" => {
                std::fs::write(
                    s.root
                        .join(std::ffi::OsString::from_vec(b"helper-\xff.txt".to_vec())),
                    "unrelated\n",
                )
                .unwrap();
            }
            _ => unreachable!(),
        }
        let head = s.head();
        let index = std::fs::read(s.root.join(".git/index")).unwrap();
        let current_archive = std::fs::read(s.root.join("archive/month/closed.md")).ok();
        let first_archive = std::fs::read(s.root.join("archive/month/a.md")).ok();
        pause.release();
        let (code, out) = finish(writer);
        if ["helper", "native-helper"].contains(&change) {
            assert_eq!(code, 0, "{change}: {out}");
            assert_eq!(out["commit"]["status"], "committed");
            let receipt = s.receipts().pop().unwrap()["delete_receipt"].clone();
            let (code, verified) = s.run(
                "verify-delete",
                json!({"parent":source,
                "candidate":read(&s.root,WIDGETS),"receipt":receipt}),
                &[],
            );
            assert_eq!(code, 0, "{verified}");
        } else {
            assert_eq!(code, 2, "{change}: {out}");
            assert_eq!(out["error"]["code"], "policy_conflict", "{change}: {out}");
            if ["bytes", "add", "remove", "add-first", "remove-first"].contains(&change) {
                let expected = match change {
                    "add" => "archive/month/new.md",
                    "add-first" | "remove-first" => "archive/month/a.md",
                    _ => "archive/month/closed.md",
                };
                assert_eq!(out["error"]["path"], expected, "{out}");
            }
            assert_eq!(read(&s.root, WIDGETS), source);
            assert_eq!(s.head(), head);
            assert_eq!(std::fs::read(s.root.join(".git/index")).unwrap(), index);
        }
        assert_eq!(
            std::fs::read(s.root.join("archive/month/closed.md")).ok(),
            current_archive
        );
        assert_eq!(
            std::fs::read(s.root.join("archive/month/a.md")).ok(),
            first_archive
        );
        assert!(s.records().is_empty(), "{change}: {:?}", s.records());
        if change == "unreadable" {
            std::fs::set_permissions(
                s.root.join("archive/subtree"),
                std::fs::Permissions::from_mode(0o755),
            )
            .unwrap();
        }
    }
}

#[test]
fn prune_capture_after_own_copy_preserves_bytes_and_surviving_stays() {
    let s = archive_store("archive-own-copy");
    let ids = s.items();
    let source = read(&s.root, WIDGETS);
    let survivor = item_guard(&s.root, WIDGETS, &ids[0]);
    let (_, before) = s.run("prune-snapshot", json!({}), &[]);
    let (code, copied) = s.run("archive-copy", archive_request(&s, &ids[1]), &[]);
    assert_eq!(code, 0, "{copied}");
    let archive = read(&s.root, "archive/month/closed.md");
    let (code, captured) = s.run("prune-snapshot", json!({}), &[]);
    assert_eq!(code, 0, "{captured}");
    assert_eq!(captured["snapshot"]["version"], 2);
    assert_eq!(
        captured["snapshot"]["archives"],
        json!(["archive/month/closed.md"])
    );
    assert_eq!(
        captured["snapshot"]["inputs"]["archive/month/closed.md"],
        archive
    );
    assert_ne!(
        captured["snapshot"]["revision"],
        before["snapshot"]["revision"]
    );
    let mut request = receipted_request(&s, &ids[1]);
    request["policy_guard"] = captured["snapshot"]["revision"].clone();
    request["archive"] = json!({"path":"archive/month/closed.md",
        "copy_op":copied["commit"]["copy_op"],"commit":copied["commit"]["sha"]});
    let head = s.head();
    let index = std::fs::read(s.root.join(".git/index")).unwrap();
    let (code, refused) = s.run(
        "delete",
        request.clone(),
        &[("RIVET_TEST_FAULT", "rename".into())],
    );
    assert_eq!(code, 2, "{refused}");
    assert_eq!(read(&s.root, WIDGETS), source);
    assert_eq!(read(&s.root, "archive/month/closed.md"), archive);
    assert_eq!(s.head(), head);
    assert_eq!(std::fs::read(s.root.join(".git/index")).unwrap(), index);
    assert!(s.records().is_empty());
    let (code, out) = s.run("delete", request, &[]);
    assert_eq!(code, 0, "{out}");
    let candidate = read(&s.root, WIDGETS);
    assert_eq!(
        candidate.split_once("## Action").unwrap().0,
        source.split_once("## Action").unwrap().0
    );
    assert_eq!(
        candidate.split_once("## Notes").unwrap().1,
        source.split_once("## Notes").unwrap().1
    );
    assert_eq!(item_guard(&s.root, WIDGETS, &ids[0]), survivor);
    assert_eq!(read(&s.root, "archive/month/closed.md"), archive);
    let receipt = s.receipts().pop().unwrap()["delete_receipt"].clone();
    let (code, out) = s.run(
        "verify-delete",
        json!({"parent":source,"candidate":candidate,"receipt":receipt}),
        &[],
    );
    assert_eq!(code, 0, "{out}");
    assert!(s.records().is_empty());
}

#[test]
fn prune_archive_pending_recovery_refuses_changed_archives_and_old_guards() {
    for change in ["dirty", "committed", "v1-uncommitted", "v1-committed"] {
        let s = archive_store(&format!("archive-recovery-{change}"));
        let ids = s.items();
        let (_, snapshot) = s.run("prune-snapshot", json!({}), &[]);
        let mut request = receipted_request(&s, &ids[1]);
        request["policy_guard"] = snapshot["snapshot"]["revision"].clone();
        s.kill_at(
            if change == "v1-committed" {
                "committed"
            } else {
                "after_rename"
            },
            "delete",
            &request,
            &[],
        );
        let candidate = read(&s.root, WIDGETS);
        let operation = s.record()["op"].as_str().unwrap().to_string();
        if change.starts_with("v1") {
            let mut rec = s.record();
            // The pending record stores the opaque v1 revision, not a bundle.
            let bundle: markrivet_core::prune_snapshot::Snapshot =
                serde_json::from_value(snapshot["snapshot"].clone()).unwrap();
            let manifest: std::collections::BTreeMap<_, _> = bundle
                .manifest
                .into_iter()
                .filter(|(path, _)| !bundle.archives.contains(path) || bundle.live.contains(path))
                .collect();
            let bytes = serde_json::to_vec(&(1_u32, bundle.live, bundle.policy, manifest)).unwrap();
            rec["policy_guard"] = json!(format!(
                "prune:sha256:{}",
                markrivet_core::delete_receipt::hash(&bytes)
            ));
            std::fs::write(
                s.root.join(format!(
                    ".rivet/pending/{}.json",
                    rec["op"].as_str().unwrap()
                )),
                serde_json::to_vec(&rec).unwrap(),
            )
            .unwrap();
        } else {
            std::fs::write(
                s.root.join("archive/month/closed.md"),
                "# Changed archive\n",
            )
            .unwrap();
            if change == "committed" {
                s.git(&[
                    "commit",
                    "-qm",
                    "archive changed during recovery",
                    "--",
                    "archive/month/closed.md",
                ]);
            }
        }
        let archive = read(&s.root, "archive/month/closed.md");
        let head = s.head();
        let index = std::fs::read(s.root.join(".git/index")).unwrap();
        let out = s.recover(json!({}), &[]);
        if change == "v1-committed" {
            assert_eq!(one_result(&out)["result"], "committed", "{out}");
            let events = s.events(&operation);
            assert_eq!(events.len(), 1);
            assert_eq!(events[0]["outcome"], "committed");
            assert_eq!(events[0]["sha"], head, "{events:?}");
            let repeated = s.recover(json!({}), &[]);
            assert_eq!(repeated["records"], json!([]));
            assert_eq!(s.events(&operation).len(), 1);
            assert!(s.records().is_empty(), "{out}");
        } else {
            assert_eq!(
                one_result(&out)["result"],
                "repair_required",
                "{change}: {out}"
            );
            if change == "dirty" {
                let detail = one_result(&out)["detail"].as_str().unwrap();
                assert!(detail.contains("archive/month/closed.md"), "{out}");
                assert!(
                    s.record()["repair"]["detail"]
                        .as_str()
                        .unwrap()
                        .contains("archive/month/closed.md")
                );
            }
            assert_eq!(s.records().len(), 1);
        }
        assert_eq!(read(&s.root, WIDGETS), candidate);
        assert_eq!(read(&s.root, "archive/month/closed.md"), archive);
        assert_eq!(s.head(), head);
        assert_eq!(std::fs::read(s.root.join(".git/index")).unwrap(), index);
    }
}

#[test]
fn prune_archive_committed_names_modes_and_wildcard_exclusions_refuse_explicitly() {
    use std::os::unix::{ffi::OsStringExt, fs::symlink};
    for shape in [
        "native-missing",
        "control-name",
        "native-helper",
        "symlink-missing",
        "gitlink",
        "rivet-wildcard",
        "git-wildcard",
        "unsupported-suffix",
        "invalid-utf8",
        "overlap",
    ] {
        let s = archive_store(&format!("archive-tree-{shape}"));
        let policy_guard = if ["invalid-utf8", "native-missing", "control-name"].contains(&shape) {
            let (code, snapshot) = s.run("prune-snapshot", json!({}), &[]);
            assert_eq!(code, 0, "{snapshot}");
            Some(snapshot["snapshot"]["revision"].clone())
        } else {
            None
        };
        let config_path = s.root.join(".rivet/project.json");
        let mut config: Value =
            serde_json::from_slice(&std::fs::read(&config_path).unwrap()).unwrap();
        match shape {
            "native-missing" | "native-helper" => {
                let name = std::ffi::OsString::from_vec(if shape == "native-missing" {
                    b"archive/month/\xff.txt".to_vec()
                } else {
                    b"helper-\xff.txt".to_vec()
                });
                std::fs::write(s.root.join(&name), "# Native\n").unwrap();
                let output = git_command(&s.root, &s.home, &["add", "--"])
                    .arg(&name)
                    .output()
                    .unwrap();
                assert!(output.status.success());
                s.git(&["commit", "-qm", "synthetic native name"]);
                std::fs::remove_file(s.root.join(name)).unwrap();
                config["collections"]["actions"]["archive"] = json!(["archive/**/*"]);
            }
            "control-name" => {
                let path = "archive/month/bad\nname.md";
                std::fs::write(s.root.join(path), "# Control name\n").unwrap();
                s.git(&["add", "--", path]);
                s.git(&["commit", "-qm", "synthetic control-character name"]);
            }
            "symlink-missing" => {
                symlink("closed.md", s.root.join("archive/month/link.md")).unwrap();
                s.git(&["add", "archive/month/link.md"]);
                s.git(&["commit", "-qm", "synthetic committed archive symlink"]);
                std::fs::remove_file(s.root.join("archive/month/link.md")).unwrap();
            }
            "gitlink" => {
                s.git(&[
                    "update-index",
                    "--add",
                    "--cacheinfo",
                    &format!("160000,{},archive/module.md", s.head()),
                ]);
                s.git(&["commit", "-qm", "synthetic gitlink"]);
            }
            "rivet-wildcard" => {
                config["collections"]["actions"]["archive"] = json!([".rivet/*.md"])
            }
            "git-wildcard" => config["collections"]["actions"]["archive"] = json!([".git/*.md"]),
            "unsupported-suffix" => {
                config["collections"]["actions"]["archive"] = json!(["archive/**/*"]);
                std::fs::write(s.root.join("archive/month/data.txt"), "# Archive input\n").unwrap();
                s.git(&["add", "archive/month/data.txt"]);
                s.git(&["commit", "-qm", "unsupported archive suffix"]);
            }
            "invalid-utf8" => {
                std::fs::write(s.root.join("archive/month/closed.md"), b"# Archive\n\xff").unwrap();
                s.git(&[
                    "commit",
                    "-qm",
                    "synthetic invalid archive bytes",
                    "--",
                    "archive/month/closed.md",
                ]);
            }
            "overlap" => {
                config["collections"]["actions"]["archive"] =
                    json!(["archive/**/*.md", "archive/month/*.md"])
            }
            _ => unreachable!(),
        }
        std::fs::write(config_path, serde_json::to_vec(&config).unwrap()).unwrap();
        s.git(&["add", ".rivet/project.json"]);
        let (ok, _) = s.git_try(&["commit", "-qm", "archive binding test"]);
        // An unchanged configuration needs no additional commit.
        if !ok {
            assert!(s.git(&["diff", "--cached"]).is_empty());
        }
        let source = read(&s.root, WIDGETS);
        let head = s.head();
        let index = std::fs::read(s.root.join(".git/index")).unwrap();
        let (code, out) = s.run("prune-snapshot", json!({}), &[]);
        if ["native-helper", "overlap"].contains(&shape) {
            assert_eq!(code, 0, "{shape}: {out}");
            assert_eq!(
                out["snapshot"]["archives"],
                json!(["archive/month/closed.md"])
            );
        } else {
            assert_eq!(code, 2, "{shape}: {out}");
            assert_eq!(out["error"]["code"], "policy_conflict", "{shape}: {out}");
            if shape == "native-missing" {
                assert_eq!(out["error"]["path"], "archive/month", "{out}");
            }
            if shape == "control-name" {
                assert_eq!(out["error"]["path"], "archive/month/bad\nname.md", "{out}");
            }
            if [
                "symlink-missing",
                "gitlink",
                "unsupported-suffix",
                "invalid-utf8",
            ]
            .contains(&shape)
            {
                assert!(
                    out["error"]["path"]
                        .as_str()
                        .unwrap()
                        .starts_with("archive/"),
                    "{out}"
                );
            }
        }
        if let Some(guard) = policy_guard {
            let mut request = receipted_request(&s, &s.items()[1]);
            request["policy_guard"] = guard;
            let (code, refused) = s.run("delete", request, &[]);
            assert_eq!(code, 2, "{refused}");
            assert_eq!(refused["error"]["code"], "policy_conflict");
            let expected = match shape {
                "native-missing" => "archive/month",
                "control-name" => "archive/month/bad\nname.md",
                _ => "archive/month/closed.md",
            };
            assert_eq!(refused["error"]["path"], expected, "{refused}");
        }
        assert_eq!(read(&s.root, WIDGETS), source);
        assert_eq!(s.head(), head);
        assert_eq!(std::fs::read(s.root.join(".git/index")).unwrap(), index);
        assert!(s.records().is_empty());
    }
}

/// Add a >1 MiB committed namespace without expanding the working-tree walk.
fn widen_snapshot_tree(s: &Store, directory: Option<&str>) {
    use std::io::Write;
    use std::process::Stdio;

    fn tree(s: &Store, bytes: &str) -> String {
        let mut child = git_command(&s.root, &s.home, &["mktree"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(bytes.as_bytes())
            .unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success());
        String::from_utf8(output.stdout).unwrap().trim().to_string()
    }

    let revision = directory.map_or("HEAD".to_string(), |d| format!("HEAD:{d}"));
    let mut entries = s.git(&["ls-tree", &revision]);
    let blob = s.git(&["rev-parse", "HEAD:notes.txt"]).trim().to_string();
    for n in 0..6000 {
        entries.push_str(&format!(
            "100644 blob {blob}\thelper-{n:05}-{}\n",
            "x".repeat(160)
        ));
    }
    assert!(entries.len() > 1024 * 1024);
    let mut oid = tree(s, &entries);
    if let Some(directory) = directory {
        let root = s
            .git(&["ls-tree", "HEAD"])
            .lines()
            .map(|line| {
                if line.ends_with(&format!("\t{directory}")) {
                    format!("040000 tree {oid}\t{directory}\n")
                } else {
                    format!("{line}\n")
                }
            })
            .collect::<String>();
        oid = tree(s, &root);
    }
    let commit = s.git(&[
        "commit-tree",
        &oid,
        "-p",
        &s.head(),
        "-m",
        "synthetic wide tree",
    ]);
    s.git(&["update-ref", "HEAD", commit.trim()]);
}

#[test]
fn prune_snapshot_archive_prefixes_keep_unscoped_membership_and_revision() {
    let s = archive_store("prune-archive-prefixes");
    for path in [
        "archive/other/a.md",
        "archive/month/b.md",
        "archived/month/decoy.md",
        "other/month/c.md",
        "archive[box]/month/d.md",
        "archiveb/month/decoy.md",
    ] {
        let path = s.root.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "# Synthetic archive\n").unwrap();
    }
    s.git(&["add", "-A"]);
    s.git(&["commit", "-qm", "synthetic archive namespaces"]);
    // Reproduce the pre-change tree command while keeping all other capture work.
    let unscoped = s.script(
        "unscoped",
        r#"unscoped() {
    while [ "$1" = "-c" ]; do shift 2; done
    if [ "$1" = "--literal-pathspecs" ] && [ "$2" = "ls-tree" ]; then
        printf 'unscoped\n' >> $HOME_DIR/unscoped.log
        exec git ls-tree -r -z --full-tree "$6"
    fi
}
unscoped "$@"
exec git "$@""#,
    );
    let config_path = s.root.join(".rivet/project.json");
    let mut config: Value = serde_json::from_slice(&std::fs::read(&config_path).unwrap()).unwrap();
    for (case, (globs, expected)) in [
        (
            json!(["archive/**/*.md"]),
            json!([
                "archive/month/b.md",
                "archive/month/closed.md",
                "archive/other/a.md"
            ]),
        ),
        (
            json!([
                "archive/month/*.md",
                "other/**/*.md",
                "archive/**/*.md",
                "archive/month/b.md"
            ]),
            json!([
                "archive/month/b.md",
                "archive/month/closed.md",
                "archive/other/a.md",
                "other/month/c.md"
            ]),
        ),
        (
            json!(["archive[box]/**/*.md"]),
            json!(["archive[box]/month/d.md"]),
        ),
        (json!(["archive/month/b.md"]), json!(["archive/month/b.md"])),
        (
            json!(["archive/month/b.md/**"]),
            json!(["archive/month/b.md"]),
        ),
        (
            json!(["archive/month/b.md/**/**"]),
            json!(["archive/month/b.md"]),
        ),
        (
            json!(["archive/**/*.md", "?ther/month/c.md"]),
            json!([
                "archive/month/b.md",
                "archive/month/closed.md",
                "archive/other/a.md",
                "other/month/c.md"
            ]),
        ),
        (
            json!(["?rchive[box]/**/d.md"]),
            json!(["archive[box]/month/d.md"]),
        ),
    ]
    .into_iter()
    .enumerate()
    {
        config["collections"]["actions"]["archive"] = globs.clone();
        std::fs::write(&config_path, serde_json::to_vec(&config).unwrap()).unwrap();
        s.git(&["add", ".rivet/project.json"]);
        s.git(&[
            "commit",
            "--allow-empty",
            "-qm",
            "configure synthetic prefixes",
        ]);
        let source = read(&s.root, WIDGETS);
        let head = s.head();
        let index = std::fs::read(s.root.join(".git/index")).unwrap();
        let logging = s.logging("prefixes");
        let (code, scoped) = s.run("prune-snapshot", json!({}), &[logging]);
        assert_eq!(code, 0, "{globs}: {scoped}");
        let (code, old) = s.run("prune-snapshot", json!({}), std::slice::from_ref(&unscoped));
        assert_eq!(code, 0, "{globs}: {old}");
        assert_eq!(s.logged("unscoped").lines().count(), case + 1);
        assert_eq!(scoped["snapshot"]["archives"], expected, "{globs}");
        assert_eq!(scoped["snapshot"], old["snapshot"], "{globs}");
        let bundle: markrivet_core::prune_snapshot::Snapshot =
            serde_json::from_value(scoped["snapshot"].clone()).unwrap();
        bundle.verify().unwrap();
        assert_eq!(read(&s.root, WIDGETS), source);
        assert_eq!(s.head(), head);
        assert_eq!(std::fs::read(s.root.join(".git/index")).unwrap(), index);
        assert!(s.records().is_empty());
    }
    let calls = s.logged("prefixes");
    assert!(
        calls.contains(" -- archive/ archive/month/ other/"),
        "{calls}"
    );
    assert!(calls.contains(" -- archive[box]/"), "{calls}");
    assert!(
        calls
            .lines()
            .any(|line| line.contains(" ls-tree ") && !line.contains(" -- ")),
        "{calls}"
    );
}

#[test]
fn prune_snapshot_terminal_recursive_globs_keep_committed_file_membership() {
    for glob in [
        "archive/month/closed.md/**",
        "archive/month/closed.md/**/**",
    ] {
        let s = archive_store("prune-terminal-recursive");
        let path = s.root.join(".rivet/project.json");
        let mut config: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        config["collections"]["actions"]["archive"] = json!([glob]);
        std::fs::write(path, serde_json::to_vec(&config).unwrap()).unwrap();
        s.git(&["add", ".rivet/project.json"]);
        s.git(&["commit", "-qm", "configure terminal recursive glob"]);
        let archive_path = s.root.join("archive/month/closed.md");
        let archive = std::fs::read(&archive_path).unwrap();
        let head = s.head();
        let index = std::fs::read(s.root.join(".git/index")).unwrap();
        std::fs::remove_file(&archive_path).unwrap();
        let (code, out) = s.run("prune-snapshot", json!({}), &[]);
        assert_eq!(code, 2, "{glob}: {out}");
        assert_eq!(out["error"]["code"], "policy_conflict", "{out}");
        assert_eq!(out["error"]["path"], "archive/month/closed.md", "{out}");
        assert!(!archive_path.exists());
        assert_eq!(s.head(), head);
        assert_eq!(std::fs::read(s.root.join(".git/index")).unwrap(), index);
        assert!(s.records().is_empty());
        std::fs::write(&archive_path, archive).unwrap();
        let (code, out) = s.run("prune-snapshot", json!({}), &[]);
        assert_eq!(code, 0, "{glob}: {out}");
        assert_eq!(
            out["snapshot"]["archives"],
            json!(["archive/month/closed.md"])
        );
    }
}

#[test]
fn prune_snapshot_archive_prefix_ignores_an_oversized_unrelated_tree() {
    let s = archive_store("prune-archive-wide-unrelated");
    widen_snapshot_tree(&s, None);
    let source = read(&s.root, WIDGETS);
    let archive = read(&s.root, "archive/month/closed.md");
    let head = s.head();
    let index = std::fs::read(s.root.join(".git/index")).unwrap();
    let logging = s.logging("scoped");
    let (code, out) = s.run("prune-snapshot", json!({}), &[logging]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(
        out["snapshot"]["archives"],
        json!(["archive/month/closed.md"])
    );
    let bundle: markrivet_core::prune_snapshot::Snapshot =
        serde_json::from_value(out["snapshot"].clone()).unwrap();
    bundle.verify().unwrap();
    assert!(s.logged("scoped").contains(" -- archive/"));
    assert_eq!(read(&s.root, WIDGETS), source);
    assert_eq!(read(&s.root, "archive/month/closed.md"), archive);
    assert_eq!(s.head(), head);
    assert_eq!(std::fs::read(s.root.join(".git/index")).unwrap(), index);
    assert!(s.records().is_empty());

    // A wildcard-first binding still reads the full tree under the same cap.
    let path = s.root.join(".rivet/project.json");
    let mut config: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    config["collections"]["actions"]["archive"] = json!(["?rchive/month/*.md"]);
    std::fs::write(path, serde_json::to_vec(&config).unwrap()).unwrap();
    let (code, out) = s.run("prune-snapshot", json!({}), &[]);
    assert_eq!(code, 2, "{out}");
    assert_eq!(out["error"]["code"], "policy_conflict", "{out}");
    assert!(
        out["error"]["message"]
            .as_str()
            .unwrap()
            .contains("git_stdout"),
        "{out}"
    );
    assert_eq!(read(&s.root, WIDGETS), source);
    assert_eq!(read(&s.root, "archive/month/closed.md"), archive);
    assert_eq!(s.head(), head);
    assert_eq!(std::fs::read(s.root.join(".git/index")).unwrap(), index);
    assert!(s.records().is_empty());
}

#[test]
fn prune_snapshot_without_archive_globs_skips_tree_listing_and_keeps_v2_revision() {
    use std::collections::BTreeMap;

    let s = Store::new("prune-no-archive-tree", "refuse");
    let path = s.root.join(".rivet/project.json");
    let mut config: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    config["prune"] = json!({"files":["tracking/*.md"]});
    std::fs::write(path, serde_json::to_vec(&config).unwrap()).unwrap();
    s.git(&["add", ".rivet/project.json"]);
    s.git(&["commit", "-qm", "configure synthetic policy"]);
    let logging = s.logging("snapshot");
    let (code, before) = s.run("prune-snapshot", json!({}), std::slice::from_ref(&logging));
    assert_eq!(code, 0, "{before}");
    let bundle: markrivet_core::prune_snapshot::Snapshot =
        serde_json::from_value(before["snapshot"].clone()).unwrap();
    bundle.verify().unwrap();
    assert_eq!(bundle.version, 2);
    assert!(bundle.archives.is_empty());
    // The pre-change engine hashes this exact tuple, including [] for archives.
    let empty_archives: Vec<String> = vec![];
    let manifest: BTreeMap<_, _> = bundle.manifest.clone();
    let legacy_bytes = serde_json::to_vec(&(
        2_u32,
        &bundle.head,
        &bundle.live,
        &bundle.policy,
        &empty_archives,
        &manifest,
    ))
    .unwrap();
    assert_eq!(
        bundle.revision,
        format!(
            "prune:sha256:{}",
            markrivet_core::delete_receipt::hash(&legacy_bytes)
        )
    );
    let calls = s.logged("snapshot");
    assert!(
        !calls.lines().any(|line| line.contains(" ls-tree ")),
        "{calls}"
    );
    assert_eq!(
        calls
            .lines()
            .filter(|line| line.ends_with(" rev-parse -q --verify HEAD^{commit}"))
            .count(),
        2,
        "{calls}"
    );
    assert_eq!(
        calls
            .lines()
            .filter(|line| line.ends_with(" symbolic-ref -q HEAD"))
            .count(),
        2,
        "{calls}"
    );

    widen_snapshot_tree(&s, None);
    let source = read(&s.root, WIDGETS);
    let docs = read(&s.root, DOCS);
    let head = s.head();
    let index = std::fs::read(s.root.join(".git/index")).unwrap();
    let (code, after) = s.run("prune-snapshot", json!({}), &[logging]);
    assert_eq!(code, 0, "{after}");
    assert_eq!(after["snapshot"]["archives"], json!([]));
    assert_eq!(after["snapshot"]["head"], head);
    assert_ne!(
        after["snapshot"]["revision"],
        before["snapshot"]["revision"]
    );
    for field in ["live", "policy", "manifest", "inputs"] {
        assert_eq!(
            after["snapshot"][field], before["snapshot"][field],
            "{field}"
        );
    }
    let calls = s.logged("snapshot");
    assert!(
        !calls.lines().any(|line| line.contains(" ls-tree ")),
        "{calls}"
    );
    assert_eq!(read(&s.root, WIDGETS), source);
    assert_eq!(read(&s.root, DOCS), docs);
    assert_eq!(s.head(), head);
    assert_eq!(std::fs::read(s.root.join(".git/index")).unwrap(), index);
    assert!(s.records().is_empty());
}

#[test]
fn prune_snapshot_document_aggregate_and_git_listing_caps_refuse_without_writes() {
    for cap in ["document", "aggregate", "listing"] {
        let s = archive_store(&format!("archive-cap-{cap}"));
        match cap {
            "document" => {
                std::fs::OpenOptions::new()
                    .write(true)
                    .open(s.root.join("archive/month/closed.md"))
                    .unwrap()
                    .set_len(markrivet_core::fs::MAX_FILE_BYTES + 1)
                    .unwrap();
                s.git(&[
                    "commit",
                    "-qm",
                    "synthetic oversized archive",
                    "--",
                    "archive/month/closed.md",
                ]);
            }
            "aggregate" => {
                let bytes = vec![b'x'; markrivet_core::fs::MAX_FILE_BYTES as usize];
                for n in 0..9 {
                    std::fs::write(s.root.join(format!("tracking/cap-{n}.md")), &bytes).unwrap();
                }
            }
            "listing" => widen_snapshot_tree(&s, Some("archive")),
            _ => unreachable!(),
        }
        let source = read(&s.root, WIDGETS);
        let archive = std::fs::read(s.root.join("archive/month/closed.md")).unwrap();
        let head = s.head();
        let index = std::fs::read(s.root.join(".git/index")).unwrap();
        let (code, out) = s.run("prune-snapshot", json!({}), &[]);
        assert_eq!(code, 2, "{cap}: {out}");
        assert_eq!(out["error"]["code"], "policy_conflict", "{cap}: {out}");
        assert!(
            out["error"]["message"]
                .as_str()
                .unwrap()
                .contains(match cap {
                    "document" => "cap",
                    "aggregate" => "64 MiB",
                    "listing" => "exceeded",
                    _ => unreachable!(),
                }),
            "{cap}: {out}"
        );
        assert_eq!(read(&s.root, WIDGETS), source);
        assert_eq!(
            std::fs::read(s.root.join("archive/month/closed.md")).unwrap(),
            archive
        );
        assert_eq!(s.head(), head);
        assert_eq!(std::fs::read(s.root.join(".git/index")).unwrap(), index);
        assert!(s.records().is_empty());
    }
}

#[test]
fn prune_archive_index_only_changes_are_not_policy_inputs_and_keep_their_stage() {
    let s = archive_store("archive-index-only");
    let ids = s.items();
    let source = read(&s.root, WIDGETS);
    let archive = read(&s.root, "archive/month/closed.md");
    let (_, before) = s.run("prune-snapshot", json!({}), &[]);
    // Stage a different archive blob while leaving working bytes equal to HEAD.
    let staged = s.git(&["rev-parse", "HEAD:notes.txt"]).trim().to_string();
    s.git(&[
        "update-index",
        "--cacheinfo",
        &format!("100644,{staged},archive/month/closed.md"),
    ]);
    let (_, after) = s.run("prune-snapshot", json!({}), &[]);
    assert_eq!(
        after["snapshot"]["revision"], before["snapshot"]["revision"],
        "{after}"
    );
    let stage = s.git(&["ls-files", "-s", "--", "archive/month/closed.md"]);
    let mut request = receipted_request(&s, &ids[1]);
    request["policy_guard"] = after["snapshot"]["revision"].clone();
    let (code, out) = s.run("delete", request, &[]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(
        s.git(&["ls-files", "-s", "--", "archive/month/closed.md"]),
        stage
    );
    assert_eq!(
        s.git(&["rev-parse", "HEAD:archive/month/closed.md"]),
        s.git(&["rev-parse", "HEAD^:archive/month/closed.md"])
    );
    assert_eq!(read(&s.root, "archive/month/closed.md"), archive);
    let event = s.receipts().pop().unwrap();
    let (code, out) = s.run(
        "verify-delete",
        json!({"parent":source,
        "candidate":read(&s.root,WIDGETS),"receipt":event["delete_receipt"]}),
        &[],
    );
    assert_eq!(code, 0, "{out}");
}

#[test]
fn prune_snapshot_root_native_names_and_empty_inputs_omit_empty_error_paths() {
    use std::os::unix::ffi::OsStringExt;
    for shape in ["working-name", "committed-name", "empty-input"] {
        let s = archive_store(&format!("policy-root-{shape}"));
        let path = s.root.join(".rivet/project.json");
        let mut config: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        if shape == "empty-input" {
            config["prune"]["inputs"] = json!([""]);
        } else {
            // Select the root file without making .git or .rivet reachable.
            config["collections"]["actions"]["archive"] = json!(["?.md"]);
        }
        std::fs::write(path, serde_json::to_vec(&config).unwrap()).unwrap();
        s.git(&["add", ".rivet/project.json"]);
        s.git(&["commit", "-qm", "synthetic root policy scope"]);
        let name = std::ffi::OsString::from_vec(b"\xff.md".to_vec());
        if shape != "empty-input" {
            std::fs::write(s.root.join(&name), "# Root native name\n").unwrap();
            if shape == "committed-name" {
                let output = git_command(&s.root, &s.home, &["add", "--"])
                    .arg(&name)
                    .output()
                    .unwrap();
                assert!(output.status.success());
                s.git(&["commit", "-qm", "synthetic committed root native name"]);
                std::fs::remove_file(s.root.join(&name)).unwrap();
            }
        }
        let source = read(&s.root, WIDGETS);
        let archive = read(&s.root, "archive/month/closed.md");
        let native = std::fs::read(s.root.join(&name)).ok();
        let head = s.head();
        let index = std::fs::read(s.root.join(".git/index")).unwrap();
        let (code, out) = s.run("prune-snapshot", json!({}), &[]);
        assert_eq!(code, 2, "{shape}: {out}");
        assert_eq!(
            out["error"]["code"],
            if shape == "empty-input" {
                "path_rejected"
            } else {
                "policy_conflict"
            },
            "{out}"
        );
        assert!(out["error"].get("path").is_none(), "{shape}: {out}");
        assert_eq!(read(&s.root, WIDGETS), source);
        assert_eq!(read(&s.root, "archive/month/closed.md"), archive);
        assert_eq!(std::fs::read(s.root.join(&name)).ok(), native);
        assert_eq!(s.head(), head);
        assert_eq!(std::fs::read(s.root.join(".git/index")).unwrap(), index);
        assert!(s.records().is_empty());
    }
}

#[test]
fn archive_only_delete_recovery_accepts_a_clean_preserving_append() {
    for section in [false, true] {
        let s = section_archive::matrix_store("archive-only-recovery-append", section);
        let ids = section_archive::matrix_ids(&s, section);
        let source = read(&s.root, WIDGETS);
        let (code, copy) = s.run("archive-copy", archive_request(&s, &ids[1]), &[]);
        assert_eq!(code, 0, "{copy}");
        let mut request = receipted_request(&s, &ids[1]);
        request["archive"] = json!({"path":"archive/month/closed.md",
        "copy_op":copy["commit"]["copy_op"],"commit":copy["commit"]["sha"]});
        section_archive::add_parent(&s, &ids[1], &mut request);
        // No policy_guard: only the authenticated copied item's preservation is required.
        s.kill_at("after_rename", "delete", &request, &[]);
        let op = s.record()["op"].as_str().unwrap().to_string();
        let candidate = read(&s.root, WIDGETS);
        let appended = format!(
            "{}\n## Later\n\nA synthetic surrounding append.\n",
            read(&s.root, "archive/month/closed.md")
        );
        std::fs::write(s.root.join("archive/month/closed.md"), &appended).unwrap();
        s.git(&[
            "commit",
            "-qm",
            "clean preserving archive append",
            "--",
            "archive/month/closed.md",
        ]);
        let parent = s.head();
        let archive_stage = s.git(&["ls-files", "-s", "--", "archive/month/closed.md"]);
        let recovered = s.recover(json!({}), &[]);
        assert_eq!(one_result(&recovered)["result"], "committed", "{recovered}");
        assert!(s.records().is_empty());
        assert_eq!(read(&s.root, WIDGETS), candidate);
        assert_eq!(read(&s.root, "archive/month/closed.md"), appended);
        assert_eq!(
            s.git(&["ls-files", "-s", "--", "archive/month/closed.md"]),
            archive_stage
        );
        assert_eq!(s.git(&["rev-parse", "HEAD^"]).trim(), parent);
        let events = s.events(&op);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["outcome"], "committed");
        assert_eq!(events[0]["delete_receipt"]["parent"], parent);
        let (code, verified) = s.run(
            "verify-delete",
            json!({"parent":source,
        "candidate":candidate,"receipt":events[0]["delete_receipt"]}),
            &[],
        );
        assert_eq!(code, 0, "{verified}");
        let _ = s.recover(json!({}), &[]);
        assert_eq!(s.events(&op).len(), 1);
    }
}

#[path = "concurrency/log_item_archive.rs"]
mod log_item_archive;
#[path = "concurrency/section_archive.rs"]
mod section_archive;

#[path = "concurrency/ref_phase.rs"]
mod ref_phase;

// ---------------------------------------------------------------------------
// add_key_yaml and set_key_yaml: real processes, ordered by the pause hooks.

const YAML_FM: &str = "---\n# lead\nscope: 'old scope' # old tail\nlenses: [a, b]   # list tail\ngenerated: {by: 'process:synthetic', at: old}\nsources:\n  - one\n  - two\nstatus: active\n---\n";

/// Replace the widgets tracker's frontmatter with the YAML fixture, keeping
/// its body, and stamp the body.
fn yaml_frontmatter(root: &Path) {
    let text = read(root, WIDGETS);
    let body = &text[text.find("\n---\n").unwrap() + 5..];
    std::fs::write(root.join(WIDGETS), format!("{YAML_FM}{body}")).unwrap();
}

fn yaml_project(name: &str) -> std::path::PathBuf {
    let root = project(name);
    yaml_frontmatter(&root);
    stamp(&root, WIDGETS);
    root
}

fn yaml_key_guard(root: &Path, key: &str) -> String {
    let (code, out) = rivet(
        root,
        "get",
        json!({"path": WIDGETS, "select": {"kind": "frontmatter_key", "key": key}}),
    );
    assert_eq!(code, 0, "{out}");
    out["revision"].as_str().unwrap().to_string()
}

fn yaml_doc_guard(root: &Path) -> String {
    let (code, out) = rivet(root, "map", json!({"path": WIDGETS}));
    assert_eq!(code, 0, "{out}");
    out["revision"].as_str().unwrap().to_string()
}

fn yaml_set(key: &str, yaml: &str, guard: &str, replace_line: bool) -> Value {
    json!({"path": WIDGETS, "message": "set_key_yaml: synthetic", "session": "test:yaml",
        "ops": [{"op": "set_key_yaml", "key": key, "yaml": yaml, "guard": guard, "replace_line": replace_line}]})
}

fn yaml_add(key: &str, yaml: &str, guard: &str) -> Value {
    json!({"path": WIDGETS, "message": "add_key_yaml: synthetic", "session": "test:yaml",
        "ops": [{"op": "add_key_yaml", "key": key, "yaml": yaml, "guard": guard}]})
}

/// Every stay in the file with the revision and text its `get` returns.
fn yaml_stays(root: &Path) -> Vec<(String, Value)> {
    let text = read(root, WIDGETS);
    let stays: Vec<(String, Value)> = text
        .split("<!-- stay:")
        .skip(1)
        .map(|rest| {
            let id: String = rest
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'))
                .collect();
            let (code, out) = rivet(
                root,
                "get",
                json!({"path": WIDGETS, "select": {"kind": "id", "id": id}}),
            );
            assert_eq!(code, 0, "{out}");
            (id, json!([out["revision"], out["text"]]))
        })
        .collect();
    assert!(stays.len() >= 2, "the fixture body is stamped");
    stays
}

fn yaml_applied(child: std::process::Child) -> Value {
    let (code, out) = finish(child);
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["status"], "applied", "{out}");
    out
}

fn yaml_refused(child: std::process::Child, error: &str) -> Value {
    let (code, out) = finish(child);
    assert_eq!(code, 2, "{out}");
    assert_eq!(
        out["error"]["code"],
        error,
        "the queued writer must take the lock within {:?} to re-read: {out}",
        markrivet_core::fs::DEFAULT_LOCK_TIMEOUT
    );
    out
}

#[test]
fn yaml_same_key_comment_race() {
    for queued_replace_line in [true, false] {
        let root = yaml_project(&format!("yaml-comment-race-{queued_replace_line}"));
        let stays = yaml_stays(&root);
        let guard = yaml_key_guard(&root, "scope");
        // A removes the comment with the same token, holding the lock.
        let pause = Pause::new(&root, "planned");
        let first = spawn(
            &root,
            "write",
            &yaml_set("scope", "'old scope'", &guard, true),
            &[pause.env()],
        );
        pause.wait();
        // B queues with the same, now superseded guard and an identical token.
        let queued = spawn(
            &root,
            "write",
            &yaml_set("scope", "'old scope'", &guard, queued_replace_line),
            &[],
        );
        // B is blocked on the project lock A holds before A is let go.
        wait_for_writer_lock(&root, &queued);
        pause.release();
        yaml_applied(first);
        let landed = read(&root, WIDGETS);
        assert!(landed.contains("\nscope: 'old scope'\nlenses:"), "{landed}");
        let out = yaml_refused(queued, "guard_conflict");
        assert_eq!(out["error"]["current"], yaml_key_guard(&root, "scope"));
        assert_eq!(read(&root, WIDGETS), landed);
        assert_eq!(yaml_stays(&root), stays);
    }
}

#[test]
fn yaml_sibling_write_survives() {
    // (A's request builder, B's target key, B's old token on its line)
    type First = fn(&Path) -> Value;
    let firsts: [(&str, First, &str, &str); 5] = [
        (
            "flow-sequence",
            |r| yaml_set("lenses", "[c, 'd e']", &yaml_key_guard(r, "lenses"), false),
            "scope",
            "'old scope'",
        ),
        (
            "flow-mapping",
            |r| json!({"path": WIDGETS, "ops": [{"op": "set_key_field", "key_path": ["generated", "at"], "value": "new", "guard": yaml_key_guard(r, "generated")}]}),
            "lenses",
            "[a, b]",
        ),
        (
            "block-sequence",
            |r| json!({"path": WIDGETS, "ops": [{"op": "delete_key", "key": "sources", "guard": yaml_key_guard(r, "sources")}]}),
            "lenses",
            "[a, b]",
        ),
        (
            "scalar",
            |r| yaml_set("scope", "[s]", &yaml_key_guard(r, "scope"), true),
            "lenses",
            "[a, b]",
        ),
        (
            "added-key",
            |r| yaml_add("added", "[z]", &yaml_doc_guard(r)),
            "scope",
            "'old scope'",
        ),
    ];
    for (name, first, target, old) in firsts {
        let root = yaml_project(&format!("yaml-sibling-{name}"));
        let stays = yaml_stays(&root);
        let guard = yaml_key_guard(&root, target);
        let pause = Pause::new(&root, "planned");
        let a = spawn(&root, "write", &first(&root), &[pause.env()]);
        pause.wait();
        // B queues on the lock, then stops before its rename, so A's
        // postimage is read before B can write.
        let b_pause = Pause::new(&root, "before_rename");
        let b = spawn(
            &root,
            "write",
            &yaml_set(target, "[x, y]", &guard, false),
            &[b_pause.env()],
        );
        wait_for_writer_lock(&root, &b);
        pause.release();
        yaml_applied(a);
        let b = b_pause.wait_for(b);
        let after_a = read(&root, WIDGETS);
        b_pause.release();
        yaml_applied(b);
        assert_eq!(
            read(&root, WIDGETS),
            after_a.replacen(&format!("{target}: {old}"), &format!("{target}: [x, y]"), 1),
            "{name}: B re-read under the lock and kept A's bytes"
        );
        assert_eq!(yaml_stays(&root), stays, "{name}");
    }
}

#[test]
fn yaml_add_document_guard_race() {
    // A body edit or a sibling key edit makes a queued add's guard stale.
    for name in ["body", "sibling"] {
        let root = yaml_project(&format!("yaml-add-race-{name}"));
        let doc = yaml_doc_guard(&root);
        let first = if name == "body" {
            let (_, id, _) = open_actions(&root)
                .into_iter()
                .find(|a| !a.1.is_empty())
                .unwrap();
            let guard = item_guard(&root, WIDGETS, &id);
            set_item(
                &id,
                &guard,
                "[x] **Ship the widget. (2026-09-01)** Done by A.",
            )
        } else {
            yaml_set("lenses", "[c]", &yaml_key_guard(&root, "lenses"), false)
        };
        let command = if name == "body" { "set" } else { "write" };
        let pause = Pause::new(&root, "planned");
        let a = spawn(&root, command, &first, &[pause.env()]);
        pause.wait();
        let b = spawn(&root, "write", &yaml_add("added", "[z]", &doc), &[]);
        wait_for_writer_lock(&root, &b);
        pause.release();
        yaml_applied(a);
        let landed = read(&root, WIDGETS);
        yaml_refused(b, "guard_conflict");
        assert_eq!(read(&root, WIDGETS), landed, "{name}");
    }
    // Two adds from one snapshot: one winner, and the loser's document guard
    // is stale before its key's existence is considered.
    for (a_key, b_key) in [("added", "added"), ("added", "other")] {
        let root = yaml_project(&format!("yaml-add-pair-{b_key}"));
        let stays = yaml_stays(&root);
        let doc = yaml_doc_guard(&root);
        let pause = Pause::new(&root, "planned");
        let a = spawn(
            &root,
            "write",
            &yaml_add(a_key, "[a]", &doc),
            &[pause.env()],
        );
        pause.wait();
        let b = spawn(&root, "write", &yaml_add(b_key, "'b'", &doc), &[]);
        wait_for_writer_lock(&root, &b);
        pause.release();
        yaml_applied(a);
        let landed = read(&root, WIDGETS);
        assert!(
            landed.contains(&format!("\n{a_key}: [a]\n---\n")),
            "{landed}"
        );
        yaml_refused(b, "guard_conflict");
        assert_eq!(read(&root, WIDGETS), landed);
        assert_eq!(yaml_stays(&root), stays);
    }
}

#[test]
fn yaml_noop_checks_guard_after_wait() {
    // A changes only the same key's comment; B would be a token no-op.
    let root = yaml_project("yaml-noop-stale");
    let guard = yaml_key_guard(&root, "scope");
    let pause = Pause::new(&root, "planned");
    let a = spawn(
        &root,
        "write",
        &yaml_set("scope", "'old scope'", &guard, true),
        &[pause.env()],
    );
    pause.wait();
    let b = spawn(
        &root,
        "write",
        &yaml_set("scope", "'old scope'", &guard, false),
        &[],
    );
    wait_for_writer_lock(&root, &b);
    pause.release();
    yaml_applied(a);
    let landed = read(&root, WIDGETS);
    yaml_refused(b, "guard_conflict");
    assert_eq!(read(&root, WIDGETS), landed);
    // Control, in commit mode: a sibling-only A leaves B's guard current,
    // so B is an unchanged write with no commit or receipt of its own.
    let s = Store::new("yaml-noop-control", "skip");
    yaml_frontmatter(&s.root);
    s.git(&["commit", "-qam", "yaml fixture"]);
    let base = s.head();
    let guard = yaml_key_guard(&s.root, "scope");
    let pause = Pause::new(&s.root, "planned");
    let a = s.spawn(
        "write",
        &yaml_set("lenses", "[c]", &yaml_key_guard(&s.root, "lenses"), false),
        &[pause.env()],
    );
    pause.wait();
    let b = s.spawn(
        "write",
        &yaml_set("scope", "'old scope'", &guard, false),
        &[],
    );
    wait_for_writer_lock(&s.root, &b);
    pause.release();
    committed_writer(a);
    let landed = read(&s.root, WIDGETS);
    let receipts = s.receipts().len();
    let (code, out) = finish(b);
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["status"], "unchanged", "{out}");
    assert_eq!(read(&s.root, WIDGETS), landed);
    assert_eq!(s.since(&base).len(), 1, "only A committed");
    assert_eq!(s.receipts().len(), receipts, "B wrote no receipt");
    assert!(s.clean());
}

#[test]
fn yaml_outside_edit_before_rename() {
    for point in ["planned", "before_rename"] {
        for (name, old, new) in [
            ("comment", "# lead\n", "# edited lead\n"),
            ("sibling", "status: active\n", "status: paused\n"),
        ] {
            for request in ["set", "add"] {
                let root = yaml_project(&format!("yaml-outside-{point}-{name}-{request}"));
                let request = if request == "set" {
                    yaml_set("scope", "[x]", &yaml_key_guard(&root, "scope"), false)
                } else {
                    yaml_add("added", "[x]", &yaml_doc_guard(&root))
                };
                let pause = Pause::new(&root, point);
                let writer = spawn(&root, "write", &request, &[pause.env()]);
                pause.wait();
                // An editor that ignores the lock saves while Rivet holds it.
                let edited = read(&root, WIDGETS).replacen(old, new, 1);
                std::fs::write(root.join(WIDGETS), &edited).unwrap();
                pause.release();
                yaml_refused(writer, "external_change");
                assert_eq!(read(&root, WIDGETS), edited, "{point} {name}");
                assert!(temp_files(&root).is_empty());
            }
        }
    }
}

#[test]
fn yaml_replacement_faults_preserve_bytes() {
    type Request = fn(&Path) -> Value;
    let ops: [(&str, Request, &str, &str); 2] = [
        (
            "set",
            |r| yaml_set("scope", "[x, 'y z']", &yaml_key_guard(r, "scope"), true),
            "scope: 'old scope' # old tail\n",
            "scope: [x, 'y z']\n",
        ),
        (
            "add",
            |r| yaml_add("added", "[x, 'y z']", &yaml_doc_guard(r)),
            "status: active\n---\n",
            "status: active\nadded: [x, 'y z']\n---\n",
        ),
    ];
    for (name, request, old, new) in ops {
        let root = yaml_project(&format!("yaml-faults-{name}"));
        let before = read(&root, WIDGETS);
        let stays = yaml_stays(&root);
        let candidate = before.replacen(old, new, 1);
        let (code, out) = finish(spawn(
            &root,
            "write",
            &request(&root),
            &[("RIVET_TEST_FAULT", "rename".into())],
        ));
        assert_eq!(code, 2, "{out}");
        assert_eq!(out["error"]["code"], "io");
        assert_eq!(read(&root, WIDGETS), before, "{name}: rename failure");
        assert!(temp_files(&root).is_empty());
        let pause = Pause::new(&root, "before_rename");
        let mut writer = pause.wait_for(spawn(&root, "write", &request(&root), &[pause.env()]));
        writer.kill().unwrap();
        writer.wait().unwrap();
        assert_eq!(read(&root, WIDGETS), before, "{name}: killed before rename");
        assert_eq!(yaml_stays(&root), stays);
        let (code, out) = finish(spawn(
            &root,
            "write",
            &request(&root),
            &[("RIVET_TEST_FAULT", "dir_sync".into())],
        ));
        assert_eq!(code, 3, "{out}");
        assert_eq!(out["status"], "applied");
        assert_eq!(out["durable"], false);
        assert_eq!(
            read(&root, WIDGETS),
            candidate,
            "{name}: applied, not durable"
        );
        assert_eq!(yaml_stays(&root), stays);
        // Back to the preimage, then kill after the rename: the whole
        // candidate is there, never part of it.
        std::fs::write(root.join(WIDGETS), &before).unwrap();
        let pause = Pause::new(&root, "after_rename");
        let mut writer = pause.wait_for(spawn(&root, "write", &request(&root), &[pause.env()]));
        writer.kill().unwrap();
        writer.wait().unwrap();
        assert_eq!(
            read(&root, WIDGETS),
            candidate,
            "{name}: killed after rename"
        );
        assert_eq!(yaml_stays(&root), stays);
    }
}

#[test]
fn yaml_commit_mode_replay_and_faults() {
    let s = Store::new("yaml-commit", "skip");
    yaml_frontmatter(&s.root);
    s.git(&["commit", "-qam", "yaml fixture"]);
    let stays = yaml_stays(&s.root);
    let base = s.head();
    let before = read(&s.root, WIDGETS);
    // Preview: no bytes, no commit, no receipt, no record.
    let mut preview = yaml_set("scope", "[p]", &yaml_key_guard(&s.root, "scope"), false);
    preview["preview"] = json!(true);
    let (code, out) = s.run("write", preview, &[]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["status"], "preview");
    assert_eq!(read(&s.root, WIDGETS), before);
    assert_eq!(s.head(), base);
    assert!(s.receipts().is_empty() && s.records().is_empty() && s.clean());
    // Each op commits once, with the per-write message and trailer.
    let (code, out) = s.run(
        "write",
        yaml_add("added", "[a, 'b c']", &yaml_doc_guard(&s.root)),
        &[],
    );
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["commit"]["status"], "committed", "{out}");
    let add_op = out["commit"]["op"].as_str().unwrap().to_string();
    let added = read(&s.root, WIDGETS);
    assert_eq!(
        added,
        before.replacen(
            "status: active\n---\n",
            "status: active\nadded: [a, 'b c']\n---\n",
            1
        )
    );
    let set = yaml_set("scope", "[x]", &yaml_key_guard(&s.root, "scope"), true);
    let (code, out) = s.run("write", set.clone(), &[]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["commit"]["status"], "committed", "{out}");
    let set_op = out["commit"]["op"].as_str().unwrap().to_string();
    let after = added.replacen("scope: 'old scope' # old tail", "scope: [x]", 1);
    assert_eq!(read(&s.root, WIDGETS), after);
    let commits = s.since(&base);
    assert_eq!(commits.len(), 2);
    for (sha, op) in commits.iter().zip([&add_op, &set_op]) {
        assert_eq!(s.changed(sha), [WIDGETS]);
        assert!(
            s.message(sha).contains(&format!("Rivet-Op: {op}")),
            "{}",
            s.message(sha)
        );
        assert_eq!(s.carrying(op).len(), 1);
    }
    assert!(s.clean());
    assert_eq!(yaml_stays(&s.root), stays);
    // A fresh unchanged set adds no commit; the stale replay refuses.
    let head = s.head();
    let receipts = s.receipts().len();
    let (code, out) = s.run("write", set, &[]);
    assert_eq!(code, 2, "{out}");
    assert_eq!(out["error"]["code"], "guard_conflict");
    let (code, out) = s.run(
        "write",
        yaml_set("scope", "[x]", &yaml_key_guard(&s.root, "scope"), true),
        &[],
    );
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["status"], "unchanged");
    assert_eq!(s.head(), head);
    assert_eq!(s.receipts().len(), receipts);
    // A refused rename and a kill before publication end not applied.
    for request in [
        yaml_set("lenses", "[r]", &yaml_key_guard(&s.root, "lenses"), false),
        yaml_add("refused", "[r]", &yaml_doc_guard(&s.root)),
    ] {
        let (code, out) = s.run(
            "write",
            request.clone(),
            &[("RIVET_TEST_FAULT", "rename".into())],
        );
        assert_eq!(code, 2, "{out}");
        assert!(s.records().is_empty(), "a refused replace leaves no record");
        assert_eq!(s.receipts().last().unwrap()["outcome"], "not_applied");
        s.kill_at("before_rename", "write", &request, &[]);
        let out = s.recover(json!({}), &[]);
        assert_eq!(one_result(&out)["result"], "not_applied", "{out}");
        assert!(s.records().is_empty());
        assert_eq!(read(&s.root, WIDGETS), after);
        assert_eq!(s.head(), head);
        assert!(s.clean());
    }
    // A kill after the rename is reconciled by recovery, never reported
    // unchanged: the replay with its original guard is stale afterwards.
    type Request = fn(&Path) -> Value;
    let killed: [(Request, &str, &str); 2] = [
        (
            |r| yaml_set("lenses", "'k'", &yaml_key_guard(r, "lenses"), true),
            "lenses: [a, b]   # list tail",
            "lenses: 'k'",
        ),
        (
            |r| yaml_add("killed", "[k]", &yaml_doc_guard(r)),
            "added: [a, 'b c']\n",
            "added: [a, 'b c']\nkilled: [k]\n",
        ),
    ];
    for (request, old, new) in killed {
        let request = request(&s.root);
        let landed = read(&s.root, WIDGETS).replacen(old, new, 1);
        let head = s.head();
        s.kill_at("after_rename", "write", &request, &[]);
        assert_eq!(read(&s.root, WIDGETS), landed);
        let out = s.recover(json!({}), &[]);
        assert_eq!(one_result(&out)["result"], "committed", "{out}");
        assert_eq!(s.since(&head).len(), 1);
        assert!(s.records().is_empty() && s.clean());
        let (code, out) = s.run("write", request, &[]);
        assert_eq!(code, 2, "{out}");
        assert_eq!(out["error"]["code"], "guard_conflict");
        assert_eq!(read(&s.root, WIDGETS), landed);
    }
    assert_eq!(yaml_stays(&s.root), stays);
}

#[test]
fn yaml_commit_every_fault_point_settles() {
    let points = [
        "pending_written",
        "before_rename",
        "after_rename",
        "applied",
        "reread",
        "helper_spawned",
        "pid_recorded",
        "committed",
        "finalise",
    ];
    for point in points {
        for kind in ["set", "add"] {
            let s = Store::new(&format!("yaml-fault-{point}-{kind}"), "skip");
            yaml_frontmatter(&s.root);
            s.git(&["commit", "-qam", "yaml fixture"]);
            let request = if kind == "set" {
                yaml_set("scope", "[f]", &yaml_key_guard(&s.root, "scope"), false)
            } else {
                yaml_add("added", "[f]", &yaml_doc_guard(&s.root))
            };
            s.kill_at(point, "write", &request, &[]);
            let op = s.record()["op"].as_str().unwrap().to_string();
            let (code, out) = s.run("commit-pending", json!({}), &[]);
            assert_eq!(code, 0, "{point} {kind}: {out}");
            assert!(s.records().is_empty(), "{point} {kind}");
            let events = s.events(&op);
            let terminal: Vec<&Value> =
                events.iter().filter(|e| e["outcome"] != "failed").collect();
            assert_eq!(terminal.len(), 1, "{point} {kind}: {events:?}");
            match terminal[0]["outcome"].as_str().unwrap() {
                "committed" => assert_eq!(s.carrying(&op).len(), 1, "{point} {kind}"),
                "not_applied" => assert!(s.carrying(&op).is_empty(), "{point} {kind}"),
                other => panic!("{point} {kind}: {other}"),
            }
            assert!(s.clean(), "{point} {kind}");
        }
    }
}

/// Single-path recovery outcomes after the tree holds the candidate: each
/// refuses with its reason and changes nothing.
#[test]
fn single_path_recovery_refuses_a_moved_target_a_changed_policy_and_an_unbindable_receipt() {
    for case in [
        "target_changed",
        "commit_policy_changed",
        "delete_receipt_rebind",
    ] {
        let s = Store::new(&format!("recover-git-{case}"), "refuse");
        let ids = s.items();
        if case == "delete_receipt_rebind" {
            s.kill_at(
                "after_rename",
                "delete",
                &receipted_request(&s, &ids[1]),
                &[],
            );
        } else {
            let mut request = s.set_request(&ids[0], "[x] Recovered or refused.");
            request["require_commit"] = json!(true);
            s.kill_at("applied", "set", &request, &[]);
        }
        let record = s.record();
        let op = record["op"].as_str().unwrap().to_string();
        match case {
            "target_changed" => {
                let other = s.git_stdin(&["hash-object", "-w", "--stdin"], "other\n");
                let entry = format!("100644,{},{WIDGETS}", other.trim());
                s.git(&["update-index", "--cacheinfo", &entry]);
            }
            "commit_policy_changed" => {
                assert_eq!(record["require_commit"], true, "{record}");
                s.configure(json!({"commit": "per-write", "on_dirty": "skip"}));
            }
            _ => {
                // The legacy shape: a receipt with no separate user message.
                let path = s.root.join(format!(".rivet/pending/{op}.json"));
                let mut edited = record.clone();
                edited.as_object_mut().unwrap().remove("user_message");
                std::fs::write(&path, serde_json::to_vec_pretty(&edited).unwrap()).unwrap();
            }
        }
        let before = (
            read(&s.root, WIDGETS),
            s.head(),
            std::fs::read(s.root.join(".git/index")).unwrap(),
        );
        let out = s.recover(json!({}), &[]);
        assert_eq!(out["unresolved"], 1, "{case}: {out}");
        assert_eq!(out["records"][0]["reason"], case, "{out}");
        assert_eq!(
            (
                read(&s.root, WIDGETS),
                s.head(),
                std::fs::read(s.root.join(".git/index")).unwrap()
            ),
            before,
            "{case}"
        );
        assert!(
            s.events(&op).iter().all(|e| e["outcome"] == "failed"),
            "{case}"
        );
    }
}

const RECORD: &str = "notes/a.md";
const RECORD_TEXT: &str = "---\nstatus: open\nowner: sam\n---\n# A\n\nBody.\n";

/// The tracker example plus a file collection holding one record.
fn record_project(name: &str) -> (std::path::PathBuf, String) {
    let root = project(name);
    let config = root.join(".rivet/project.json");
    let mut c: Value = serde_json::from_slice(&std::fs::read(&config).unwrap()).unwrap();
    c["collections"]["notes"] = json!({"schema": "note@1", "files": ["notes/*.md"]});
    std::fs::write(&config, c.to_string()).unwrap();
    std::fs::write(
        root.join(".rivet/schemas/note.json"),
        r#"{"id": "note", "version": 1, "item": "file", "fields": {
          "status": {"type": "enum", "from": "key", "key": "status", "values": ["open", "done"]},
          "owner": {"type": "string", "from": "key", "key": "owner"}}}"#,
    )
    .unwrap();
    std::fs::create_dir_all(root.join("notes")).unwrap();
    std::fs::write(root.join(RECORD), RECORD_TEXT).unwrap();
    let (code, map) = rivet(&root, "map", json!({"path": RECORD}));
    assert_eq!(code, 0, "{map}");
    let guard = map["collections"][0]["revision"]
        .as_str()
        .unwrap()
        .to_string();
    (root, guard)
}

fn record_write(guard: &str, set: Value) -> Value {
    json!({"path": RECORD, "collection": "notes", "guard": guard, "set": set})
}

#[test]
fn a_record_writer_holding_the_lock_wins_and_the_stale_one_refuses() {
    let (root, guard) = record_project("record-stale");
    let pause = Pause::new(&root, "planned");
    let first = spawn(
        &root,
        "update-item",
        &record_write(&guard, json!({"status": "done", "owner": "kim"})),
        &[pause.env()],
    );
    pause.wait();
    // B starts with the same guard and waits on A's lock.
    let second = spawn(
        &root,
        "update-item",
        &record_write(&guard, json!({"owner": "lee"})),
        &[],
    );
    pause.release();
    let (ca, ra) = finish(first);
    let (cb, rb) = finish(second);
    assert_eq!(ca, 0, "{ra}");
    assert_eq!(cb, 2, "{rb}");
    assert_eq!(rb["error"]["code"], "guard_conflict", "{rb}");
    assert_eq!(rb["error"]["current"], ra["ops"][0]["revision"], "{rb}");
    assert_eq!(
        read(&root, RECORD),
        "---\nstatus: done\nowner: kim\n---\n# A\n\nBody.\n"
    );
}

#[test]
fn an_outside_edit_during_a_record_write_is_kept() {
    let (root, guard) = record_project("record-external");
    let pause = Pause::new(&root, "planned");
    let writer = spawn(
        &root,
        "update-item",
        &record_write(&guard, json!({"status": "done"})),
        &[pause.env()],
    );
    pause.wait();
    // An editor that ignores the lock saves while Rivet holds it.
    let edited = RECORD_TEXT.replace("Body.", "Edited in an editor.");
    std::fs::write(root.join(RECORD), &edited).unwrap();
    pause.release();
    let (code, out) = finish(writer);
    assert_eq!(code, 2, "{out}");
    assert_eq!(out["error"]["code"], "external_change", "{out}");
    assert_eq!(read(&root, RECORD), edited, "the outside edit is preserved");
}
