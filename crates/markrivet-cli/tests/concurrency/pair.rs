use super::*;
use std::os::unix::fs::MetadataExt;

#[path = "pair_outcomes.rs"]
mod outcomes;

const A: &str = "pair-a.md";
const B: &str = "pair-b.md";
const UNIT: &str = "- [ ] Payload <!-- stay:pairUnit -->\n";
const READ_A: &str = "# Source\n\n- [ ] Payload <!-- stay:pairUnit -->\n";
const READ_B: &str = "# Destination\n\n";

pub(super) fn fixture(name: &str) -> Store {
    let s = Store::new(name, "refuse");
    std::fs::write(s.root.join(A), READ_A).unwrap();
    std::fs::write(s.root.join(B), READ_B).unwrap();
    s.git(&["add", A, B]);
    s.git(&["commit", "-qm", "pair fixture"]);
    s
}

fn assert_fence(s: &Store, record: &Value) {
    let meta = std::fs::metadata(s.root.join(".git/index.lock")).expect("pending pair fence");
    assert_eq!(record["fence"]["dev"], meta.dev());
    assert_eq!(record["fence"]["ino"], meta.ino());
    assert_eq!(
        record["fence"]["btime_ns"],
        meta.created()
            .unwrap()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos() as u64
    );
    for path in [A, B] {
        let (ok, text) = s.git_try(&["commit", "-qm", "must not split", "--", path]);
        assert!(!ok && text.contains("index.lock"), "{path}: {text}");
    }
}

fn fence(s: &Store) {
    assert_fence(s, &s.record());
}

fn settled(s: &Store, committed: bool) {
    let record = s.record();
    fence(s);
    let out = s.recover(json!({}), &[]);
    assert_eq!(out["unresolved"], 0, "{out}");
    assert!(!s.root.join(".git/index.lock").exists());
    let commits = s.carrying(record["op"].as_str().unwrap());
    assert_eq!(commits.len(), usize::from(committed));
    let (a, b) = (read(&s.root, A), read(&s.root, B));
    if committed {
        assert_eq!(s.changed(&commits[0]), [A, B]);
        let message = s.message(&commits[0]);
        assert_eq!(message.matches("Rivet-Op:").count(), 1);
        assert_eq!(message.matches("Rivet-Move:").count(), 1);
        assert_eq!(a, "# Source\n\n");
        assert_eq!(b, format!("{READ_B}{UNIT}"));
    } else {
        assert_eq!((a.as_str(), b.as_str()), (READ_A, READ_B));
    }
    let terminal: Vec<_> = s
        .events(record["op"].as_str().unwrap())
        .into_iter()
        .filter(|e| e["outcome"] != "failed")
        .collect();
    assert_eq!(terminal.len(), 1);
    assert_eq!(terminal[0]["peer_path"], B);
    assert_eq!(terminal[0]["peer_applied"], committed);
}

#[test]
fn pair_committed_is_one_transaction_of_two_paths() {
    let s = fixture("pair-success");
    let (code, out) = s.run("__test-pair", json!({}), &[]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["commit"]["status"], "committed", "{out}");
    assert_eq!(s.changed(&s.head()), [A, B]);
    assert!(!s.root.join(".git/index.lock").exists());
    assert!(s.records().is_empty());
}

fn two_pending_records(name: &str) -> (Store, String, Value) {
    let s = fixture(name);
    let id = s.items()[0].clone();
    let request = s.set_request(&id, "[x] Older unrelated write.");
    s.kill_at("staged", "set", &request, &[]);
    let older = s.record()["op"].as_str().unwrap().to_owned();
    assert!(!s.root.join(".git/index.lock").exists());
    s.kill_at("move_destination_replaced", "__test-pair", &json!({}), &[]);
    let pair = s
        .records()
        .into_iter()
        .find(|r| r["peer"].is_object())
        .unwrap();
    assert_eq!(s.records().len(), 2);
    (s, older, pair)
}

#[test]
fn pair_an_older_record_cannot_remove_its_live_index_fence() {
    let (s, older, pair) = two_pending_records("pair-other-clear-direct");
    let project = markrivet_core::fs::Project::open(&s.root).unwrap();
    let locks = markrivet_core::git::list_locks(&project, Some("refs/heads/main")).unwrap();
    let lock = locks.iter().find(|l| l.file == ".git/index.lock").unwrap();
    let before = (
        read(&s.root, A),
        read(&s.root, B),
        s.head(),
        std::fs::read(s.root.join(".git/index")).unwrap(),
        s.records(),
        s.receipts(),
    );
    let (code, out) = s.run(
        "commit-pending",
        json!({"clear_lock":lock.file,"identity":lock.identity,"op":older}),
        &[],
    );
    assert_eq!(code, 2, "{out}");
    assert_eq!(out["error"]["code"], "clear_lock_refused", "{out}");
    assert_eq!(
        (
            read(&s.root, A),
            read(&s.root, B),
            s.head(),
            std::fs::read(s.root.join(".git/index")).unwrap(),
            s.records(),
            s.receipts()
        ),
        before
    );
    assert_fence(&s, &pair);
}

#[test]
fn pair_unsupported_pending_entries_block_clear_writes_and_snapshots() {
    use std::os::unix::{
        ffi::{OsStrExt, OsStringExt},
        fs::FileTypeExt,
    };
    for kind in ["directory", "fifo", "hidden", "native"] {
        let (s, older, pair) = two_pending_records(&format!("pair-nonregular-{kind}"));
        let pair_op = pair["op"].as_str().unwrap();
        let original = s.root.join(format!(".rivet/pending/{pair_op}.json"));
        let backup = original.with_extension("saved");
        std::fs::rename(&original, &backup).unwrap();
        let pending = match kind {
            "hidden" => s.root.join(format!(".rivet/pending/.{pair_op}.json")),
            "native" => s
                .root
                .join(".rivet/pending")
                .join(std::ffi::OsString::from_vec(b"unknown-\xff.json".to_vec())),
            _ => original,
        };
        if kind == "directory" {
            std::fs::create_dir(&pending).unwrap();
        } else if kind == "fifo" {
            let name = std::ffi::CString::new(pending.as_os_str().as_bytes()).unwrap();
            // SAFETY: confined synthetic fixture path, valid NUL-terminated name.
            assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        } else {
            std::fs::copy(&backup, &pending).unwrap();
        }
        let project = markrivet_core::fs::Project::open(&s.root).unwrap();
        let locks = markrivet_core::git::list_locks(&project, Some("refs/heads/main")).unwrap();
        let index_lock = locks.iter().find(|l| l.file == ".git/index.lock").unwrap();
        let older_path = s.root.join(format!(".rivet/pending/{older}.json"));
        let before = (
            read(&s.root, A),
            read(&s.root, B),
            s.head(),
            std::fs::read(s.root.join(".git/index")).unwrap(),
            std::fs::read(&backup).unwrap(),
            std::fs::read(&older_path).unwrap(),
            s.receipts(),
        );
        for request in [
            json!({"clear_lock":index_lock.file,"identity":index_lock.identity,"op":older}),
            json!({"clear_lock":index_lock.file,"identity":index_lock.identity,"size":index_lock.size,"quiescent":true}),
        ] {
            let (code, out) = s.run("commit-pending", request, &[]);
            assert_eq!(code, 2, "{kind}: {out}");
            assert_eq!(out["error"]["code"], "clear_lock_refused", "{out}");
            assert_eq!(out["error"]["reason"], "pending_records", "{out}");
        }
        let (code, status) = s.run("status", json!({"paths":[A,B]}), &[]);
        assert_eq!(code, 0, "{status}");
        assert!(
            !status["diagnostics"].as_array().unwrap().is_empty(),
            "{status}"
        );
        let index = status["git"]["locks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|l| l["file"] == ".git/index.lock")
            .unwrap();
        assert!(index["record"].is_null(), "{status}");
        for off in [false, true] {
            if off {
                s.configure(json!({"commit":"off"}));
            }
            for path in [A, B] {
                let doc = markrivet_core::Document::parse(&read(&s.root, path)).unwrap();
                let (code,out)=s.run("stamp",
                    json!({"path":path,"guard":doc.target_revision(markrivet_core::document::Target::Document)}),&[]);
                assert_eq!(code, 2, "{kind}, off={off}: {out}");
                assert_eq!(out["error"]["code"], "pending_unresolved", "{out}");
            }
            let (code, out) = s.run("prune-snapshot", json!({}), &[]);
            assert_eq!(code, 2, "{out}");
            assert_eq!(out["error"]["code"], "pending_unresolved", "{out}");
        }
        assert_eq!(
            (
                read(&s.root, A),
                read(&s.root, B),
                s.head(),
                std::fs::read(s.root.join(".git/index")).unwrap(),
                std::fs::read(&backup).unwrap(),
                std::fs::read(&older_path).unwrap(),
                s.receipts()
            ),
            before
        );
        let file_type = std::fs::symlink_metadata(&pending).unwrap().file_type();
        assert!(match kind {
            "directory" => file_type.is_dir(),
            "fifo" => file_type.is_fifo(),
            _ => file_type.is_file(),
        });
        assert_fence(&s, &pair);
    }
}

#[test]
fn pair_other_record_cannot_clear_or_claim_its_index_fence() {
    for corrupt in [false, true] {
        let (s, older, pair) = two_pending_records(&format!("pair-other-clear-{corrupt}"));
        let pair_op = pair["op"].as_str().unwrap();
        let (code, status) = s.run("status", json!({"paths":[A,B]}), &[]);
        assert_eq!(code, 0, "{status}");
        let lock = status["git"]["locks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|l| l["file"] == ".git/index.lock")
            .unwrap();
        assert_eq!(lock["record"], pair_op, "{status}");
        let identity = lock["identity"].clone();
        let size = lock["size"].clone();
        // Recovery's lock report uses the same ownership rule as status.
        let out = s.recover(json!({"op":older}), &[]);
        let lock = out["records"][0]["locks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|l| l["file"] == ".git/index.lock")
            .unwrap();
        assert_eq!(lock["record"], pair_op, "{out}");
        let pending = s.root.join(format!(".rivet/pending/{pair_op}.json"));
        if corrupt {
            std::fs::write(&pending, "{").unwrap();
            let (code, status) = s.run("status", json!({"paths":[A,B]}), &[]);
            assert_eq!(code, 0, "{status}");
            let lock = status["git"]["locks"]
                .as_array()
                .unwrap()
                .iter()
                .find(|l| l["file"] == ".git/index.lock")
                .unwrap();
            assert!(lock["record"].is_null(), "{status}");
        }
        let before = (
            read(&s.root, A),
            read(&s.root, B),
            read(&s.root, WIDGETS),
            s.head(),
            std::fs::read(s.root.join(".git/index")).unwrap(),
            std::fs::read(&pending).unwrap(),
            s.receipts(),
        );
        for request in [
            json!({"clear_lock":".git/index.lock","identity":identity,"op":older}),
            json!({"clear_lock":".git/index.lock","identity":identity,"size":size,"quiescent":true}),
        ] {
            let (code, out) = s.run("commit-pending", request, &[]);
            assert_eq!(code, 2, "{out}");
            assert_eq!(out["error"]["code"], "clear_lock_refused", "{out}");
            assert_eq!(out["error"]["reason"], "pending_records", "{out}");
            assert_eq!(
                (
                    read(&s.root, A),
                    read(&s.root, B),
                    read(&s.root, WIDGETS),
                    s.head(),
                    std::fs::read(s.root.join(".git/index")).unwrap(),
                    std::fs::read(&pending).unwrap(),
                    s.receipts()
                ),
                before
            );
            assert_fence(&s, &pair);
        }
        if !corrupt {
            let out = s.recover(
                json!({"clear_lock":".git/index.lock","identity":identity,"op":pair_op}),
                &[],
            );
            assert_eq!(out["unresolved"], 2, "{out}");
            assert!(!s.root.join(".git/index.lock").exists());
            assert!(
                s.records().iter().find(|r| r["op"] == pair_op).unwrap()["fence_waived"]
                    .as_bool()
                    .unwrap()
            );
            assert_eq!(read(&s.root, A), before.0);
            assert_eq!(read(&s.root, B), before.1);
            assert_eq!(s.head(), before.3);
        }
    }
}

#[test]
fn pair_foreign_index_fence_never_falls_back_to_an_older_childs_time_floor() {
    let (s, older, _pair) = two_pending_records("pair-other-foreign");
    std::fs::rename(
        s.root.join(".git/index.lock"),
        s.root.join(".git/saved-fence"),
    )
    .unwrap();
    std::fs::write(s.root.join(".git/index.lock"), "foreign").unwrap();
    let (code, out) = s.run("status", json!({"paths":[A,B]}), &[]);
    assert_eq!(code, 0, "{out}");
    let lock = out["git"]["locks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|l| l["file"] == ".git/index.lock")
        .unwrap();
    assert!(lock["record"].is_null(), "{out}");
    let (code, refused) = s.run(
        "commit-pending",
        json!({"clear_lock":".git/index.lock","identity":lock["identity"],"op":older}),
        &[],
    );
    assert_eq!(code, 2, "{refused}");
    assert_eq!(read(&s.root, ".git/index.lock"), "foreign");
}

#[test]
fn pair_crash_boundaries_take_over_the_owned_fence() {
    for hook in [
        "move_pending_written",
        "move_destination_replaced",
        "move_applied",
        "index_prepared",
        "before_ref_update",
        "ref_updated",
    ] {
        let s = fixture(&format!("pair-kill-{hook}"));
        s.kill_at(hook, "__test-pair", &json!({}), &[]);
        settled(&s, hook != "move_pending_written");
    }
}

#[test]
fn pair_recovery_matrix_preserves_outside_edits() {
    for a in ["read", "candidate", "neither"] {
        for b in ["read", "candidate", "neither"] {
            let s = fixture(&format!("pair-matrix-{a}-{b}"));
            s.kill_at("move_destination_replaced", "__test-pair", &json!({}), &[]);
            let aa = match a {
                "read" => READ_A,
                "candidate" => "# Source\n\n",
                _ => "outside A\n",
            };
            let bb = match b {
                "read" => READ_B.to_string(),
                "candidate" => format!("{READ_B}{UNIT}"),
                _ => "outside B\n".into(),
            };
            std::fs::write(s.root.join(A), aa).unwrap();
            std::fs::write(s.root.join(B), &bb).unwrap();
            if a == "neither" || b == "neither" {
                let out = s.recover(json!({}), &[]);
                assert_eq!(out["records"][0]["reason"], "file_changed", "{out}");
                assert_eq!(read(&s.root, A), aa);
                assert_eq!(read(&s.root, B), bb);
                fence(&s);
            } else {
                settled(&s, a != "read" || b != "read");
            }
        }
    }
}

#[test]
fn pair_compensation_and_non_durable_exits_keep_the_unit() {
    for fault in [
        "move_source_rename",
        "move_destination_rename",
        "dir_sync",
        "index_sync",
        "index_directory_sync",
    ] {
        let s = fixture(&format!("pair-fault-{fault}"));
        let (code, out) = s.run(
            "__test-pair",
            json!({}),
            &[("RIVET_TEST_FAULT", fault.into())],
        );
        if fault == "move_source_rename" || fault == "move_destination_rename" {
            assert_eq!(code, 2, "{fault}: {out}");
            assert_eq!(read(&s.root, A), READ_A);
            assert_eq!(read(&s.root, B), READ_B);
            assert!(s.records().is_empty());
            assert!(!s.root.join(".git/index.lock").exists());
        } else {
            assert_eq!(code, 3, "{fault}: {out}");
            // A completed index rename consumes the fence. Recovery publishes
            // a fresh one before syncing or changing any index state.
            if fault == "index_directory_sync" {
                let out = s.recover(json!({}), &[]);
                assert_eq!(out["unresolved"], 0, "{out}");
                assert!(!s.root.join(".git/index.lock").exists());
            } else {
                settled(&s, true);
            }
        }
    }
}

#[test]
fn pair_barrier_refuses_commit_off_and_broken_config() {
    for broken in [false, true] {
        let s = fixture(&format!("pair-barrier-{broken}"));
        s.kill_at("move_destination_replaced", "__test-pair", &json!({}), &[]);
        if broken {
            std::fs::write(s.root.join(".rivet/project.json"), "broken").unwrap();
        } else {
            s.configure(json!({"commit":"off"}));
        }
        for path in [A, B] {
            let before = read(&s.root, path);
            let (code, out) = s.run("stamp", json!({"path":path,"guard":"unused"}), &[]);
            assert_eq!(code, 2, "{out}");
            assert_eq!(out["error"]["code"], "pending_unresolved");
            assert_eq!(read(&s.root, path), before);
        }
        fence(&s);
    }
}

#[test]
fn pair_abandon_needs_no_published_op_and_clean_pair() {
    for hook in ["move_destination_replaced", "ref_updated"] {
        let s = fixture(&format!("pair-abandon-{hook}"));
        s.kill_at(hook, "__test-pair", &json!({}), &[]);
        let op = s.record()["op"].as_str().unwrap().to_string();
        let (code, _) = s.run("commit-pending", json!({"abandon":op,"reason":"test"}), &[]);
        assert_eq!(code, 2);
        fence(&s);
        std::fs::write(s.root.join(A), READ_A).unwrap();
        std::fs::write(s.root.join(B), READ_B).unwrap();
        let (code, out) = s.run(
            "commit-pending",
            json!({"abandon":op,"reason":"restored"}),
            &[],
        );
        if hook == "ref_updated" {
            assert_eq!(code, 2, "{out}");
            let out = s.recover(json!({}), &[]);
            assert_eq!(out["records"][0]["reason"], "worktree_differs", "{out}");
            fence(&s);
            std::fs::write(s.root.join(A), "# Source\n\n").unwrap();
            std::fs::write(s.root.join(B), format!("{READ_B}{UNIT}")).unwrap();
            settled(&s, true);
        } else {
            assert_eq!(code, 0, "{out}");
            assert!(s.records().is_empty());
            assert!(!s.root.join(".git/index.lock").exists());
        }
    }
}

#[test]
fn pair_recovery_sync_failure_never_removes_the_source_copy() {
    let s = fixture("pair-sync-order");
    s.kill_at("move_destination_replaced", "__test-pair", &json!({}), &[]);
    let out = s.recover(
        json!({}),
        &[("RIVET_TEST_FAULT", "move_recovery_sync".into())],
    );
    assert_eq!(out["records"][0]["reason"], "not_durable", "{out}");
    assert_eq!(read(&s.root, A), READ_A);
    fence(&s);
    settled(&s, true);
}

#[test]
fn pair_unrelated_write_settles_the_crashed_pair_first() {
    let s = fixture("pair-unrelated");
    s.kill_at("move_destination_replaced", "__test-pair", &json!({}), &[]);
    let op = s.record()["op"].as_str().unwrap().to_string();
    let (code, out) = s.set(&s.items()[0], "[x] Unrelated edit.", &[]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(s.changed(&s.carrying(&op)[0]), [A, B]);
    assert!(!s.root.join(".git/index.lock").exists());
}

#[test]
fn pair_head_change_after_index_preparation_commits_on_the_new_head() {
    let s = fixture("pair-head-change");
    let base = s.head();
    let pause = Pause::new(&s.root, "index_prepared");
    let child = pause.wait_for(s.spawn("__test-pair", &json!({}), &[pause.env()]));
    fence(&s);
    let tree = s.git(&["rev-parse", "HEAD^{tree}"]);
    let foreign = s.git(&[
        "commit-tree",
        tree.trim(),
        "-p",
        &base,
        "-m",
        "foreign empty commit",
    ]);
    s.git(&["update-ref", "HEAD", foreign.trim(), &base]);
    pause.release();
    let (code, out) = finish(child);
    assert_eq!(code, 0, "{out}");
    assert_eq!(s.git(&["rev-parse", "HEAD^"]).trim(), foreign.trim());
    assert_eq!(s.changed(&s.head()), [A, B]);
    assert!(!s.root.join(".git/index.lock").exists());
}

#[test]
fn pair_recovery_retries_a_head_change_before_ref_publication_once() {
    let s = fixture("pair-recovery-head-retry");
    s.kill_at("move_applied", "__test-pair", &json!({}), &[]);
    let record = s.record();
    let op = record["op"].as_str().unwrap();
    let base = s.head();
    let pause = Pause::new(&s.root, "before_ref_update");
    let child = pause.wait_for(s.spawn("commit-pending", &json!({}), &[pause.env()]));
    assert_fence(&s, &record);
    let tree = s.git(&["rev-parse", "HEAD^{tree}"]);
    let foreign = s.git(&[
        "commit-tree",
        tree.trim(),
        "-p",
        &base,
        "-m",
        "foreign empty commit",
    ]);
    s.git(&["update-ref", "HEAD", foreign.trim(), &base]);
    pause.release();
    let (code, out) = finish(child);
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["unresolved"], 0, "{out}");
    assert_eq!(s.git(&["rev-parse", "HEAD^"]).trim(), foreign.trim());
    assert_eq!(s.changed(&s.head()), [A, B]);
    assert_eq!(s.carrying(op), [s.head()]);
    assert!(!s.root.join(".git/index.lock").exists());
    assert!(s.records().is_empty());
    let events = s.events(op);
    assert!(
        events.iter().any(|e| e["outcome"] == "failed"),
        "{events:?}"
    );
    let terminal: Vec<_> = events.iter().filter(|e| e["outcome"] != "failed").collect();
    assert_eq!(terminal.len(), 1, "{events:?}");
    assert_eq!(terminal[0]["applied"], true);
    assert_eq!(terminal[0]["peer_applied"], true);
    assert_eq!(terminal[0]["peer_path"], B);
    let (code, status) = s.run("status", json!({"paths":[A,B]}), &[]);
    assert_eq!(code, 0, "{status}");
    for path in status["paths"].as_array().unwrap() {
        assert_eq!(path["last_write"]["op"], op, "{status}");
        assert_eq!(path["last_write"]["outcome"], "committed", "{status}");
    }
}

#[test]
fn pair_partial_index_install_is_truncated_on_recovery() {
    let s = fixture("pair-partial-index");
    let base = s.head();
    let index_len = std::fs::metadata(s.root.join(".git/index")).unwrap().len();
    s.kill_at("index_partial_write", "__test-pair", &json!({}), &[]);
    assert_ne!(
        s.head(),
        base,
        "the ref must publish before the real index write"
    );
    let lock_path = s.root.join(".git/index.lock");
    let partial = std::fs::metadata(&lock_path).unwrap().len();
    assert!(
        partial > 0 && partial < index_len,
        "real fence was not partially written"
    );
    // A previous partial retry may also have left an oversized tail. The
    // installation must truncate it, not merely overwrite its prefix.
    use std::io::Write;
    std::fs::OpenOptions::new()
        .append(true)
        .open(lock_path)
        .unwrap()
        .write_all(&vec![b'x'; index_len as usize * 2])
        .unwrap();
    settled(&s, true);
    assert!(s.git_try(&["ls-files", "--stage"]).0);
}

#[test]
fn pair_fence_waiver_keeps_outside_edits_and_never_recovers_automatically() {
    let s = fixture("pair-waiver");
    s.kill_at("move_destination_replaced", "__test-pair", &json!({}), &[]);
    let op = s.record()["op"].as_str().unwrap().to_string();
    std::fs::write(s.root.join(A), "# Source\n\nOutside edit.\n").unwrap();
    let locks = markrivet_core::git::list_locks(
        &markrivet_core::fs::Project::open(&s.root).unwrap(),
        Some("refs/heads/main"),
    )
    .unwrap();
    let identity = &locks
        .iter()
        .find(|l| l.file == ".git/index.lock")
        .unwrap()
        .identity;
    let (code, out) = s.run(
        "commit-pending",
        json!({"clear_lock":".git/index.lock", "identity":identity,"op":op}),
        &[],
    );
    assert_eq!(code, 0, "{out}");
    assert!(s.record()["fence_waived"].as_bool().unwrap());
    assert!(!s.root.join(".git/index.lock").exists());
    let before = (read(&s.root, A), read(&s.root, B), s.head());
    let out = s.recover(json!({}), &[]);
    assert_eq!(out["records"][0]["reason"], "fence_waived", "{out}");
    assert_eq!((read(&s.root, A), read(&s.root, B), s.head()), before);
    assert!(!s.root.join(".git/index.lock").exists());
    for path in [A, B] {
        let (code, out) = s.run("stamp", json!({"path":path,"guard":"unused"}), &[]);
        assert_eq!(code, 2, "{out}");
        assert_eq!(out["error"]["code"], "pending_unresolved");
    }
    s.git(&["commit", "-qm", "keep edited pair together", "--", A, B]);
    let out = s.recover(json!({"abandon":op, "reason":"hand committed both"}), &[]);
    assert_eq!(out["unresolved"], 0, "{out}");
}

/// Waive a crashed pair's verified fence, as an operator keeping an edit does.
fn waive(s: &Store, op: &str) {
    let locks = markrivet_core::git::list_locks(
        &markrivet_core::fs::Project::open(&s.root).unwrap(),
        Some("refs/heads/main"),
    )
    .unwrap();
    let identity = &locks
        .iter()
        .find(|l| l.file == ".git/index.lock")
        .unwrap()
        .identity;
    let (code, out) = s.run(
        "commit-pending",
        json!({"clear_lock":".git/index.lock", "identity":identity,"op":op}),
        &[],
    );
    assert_eq!(code, 0, "{out}");
    assert!(!s.root.join(".git/index.lock").exists());
}

/// Everything an abandon refusal must leave as it was, as raw bytes.
fn abandon_state(s: &Store) -> impl PartialEq + std::fmt::Debug {
    let pending: Vec<_> = std::fs::read_dir(s.root.join(".rivet/pending"))
        .map(|dir| {
            let mut entries: Vec<_> = dir
                .map(|e| {
                    let path = e.unwrap().path();
                    (path.clone(), std::fs::read(path).unwrap_or_default())
                })
                .collect();
            entries.sort();
            entries
        })
        .unwrap_or_default();
    (
        std::fs::read(s.root.join(A)).unwrap(),
        std::fs::read(s.root.join(B)).unwrap(),
        s.head(),
        std::fs::read(s.root.join(".git/index")).unwrap(),
        pending,
        std::fs::read(s.root.join(".rivet/commits.jsonl")).unwrap_or_default(),
        s.root.join(".git/index.lock").exists(),
    )
}

#[test]
fn pair_abandon_agreement_matrix_after_a_waiver() {
    let outside_a = "# Source\n\nOutside edit.\n".as_bytes().to_vec();
    let candidate_a = b"# Source\n\n".to_vec();
    let candidate_b = format!("{READ_B}{UNIT}").into_bytes();
    // (case, source bytes, destination bytes, abandon allowed)
    let cases: Vec<(&str, Vec<u8>, Vec<u8>, bool)> = vec![
        ("read-read", READ_A.into(), READ_B.into(), true),
        (
            "candidate-candidate",
            candidate_a.clone(),
            candidate_b.clone(),
            true,
        ),
        ("unknown-read", outside_a.clone(), READ_B.into(), true),
        (
            "unknown-candidate",
            outside_a.clone(),
            candidate_b.clone(),
            true,
        ),
        (
            "unknown-unknown",
            outside_a.clone(),
            b"# Destination\n\nOutside edit.\n".to_vec(),
            true,
        ),
        (
            "unknown-unknown-both",
            format!("# Source\n\nKept.\n\n{UNIT}").into_bytes(),
            format!("# Destination\n\nCopied.\n\n{UNIT}").into_bytes(),
            false,
        ),
        (
            "loose-claim",
            b"# Source\n\nA note. <!-- stay:pairUnit -->\n".to_vec(),
            candidate_b.clone(),
            false,
        ),
        (
            "non-utf8",
            b"# Source\n\n\xff\n".to_vec(),
            candidate_b.clone(),
            false,
        ),
    ];
    for (case, a, b, allowed) in cases {
        let s = fixture(&format!("pair-abandon-matrix-{case}"));
        s.kill_at("move_destination_replaced", "__test-pair", &json!({}), &[]);
        let op = s.record()["op"].as_str().unwrap().to_string();
        waive(&s, &op);
        std::fs::write(s.root.join(A), &a).unwrap();
        std::fs::write(s.root.join(B), &b).unwrap();
        s.git(&["commit", "-q", "--allow-empty", "-am", "both by hand"]);
        let before = abandon_state(&s);
        let (code, out) = s.run("commit-pending", json!({"abandon":op, "reason":case}), &[]);
        if allowed {
            assert_eq!(code, 0, "{case}: {out}");
            assert!(s.records().is_empty(), "{case}");
            assert_eq!(s.events(&op).pop().unwrap()["outcome"], "abandoned");
        } else {
            assert_eq!(code, 2, "{case}: {out}");
            assert_eq!(out["error"]["code"], "pending_unresolved", "{case}: {out}");
            assert_eq!(abandon_state(&s), before, "{case}");
        }
        assert_eq!(std::fs::read(s.root.join(A)).unwrap(), a, "{case}");
        assert_eq!(std::fs::read(s.root.join(B)).unwrap(), b, "{case}");
    }
}

#[test]
fn pair_abandon_refuses_a_one_sided_hand_commit() {
    // (source, destination) bytes committed by hand after the waiver, and
    // whether the stay ends up in both files or in neither.
    for (a, b, both) in [
        (READ_A.to_string(), format!("{READ_B}{UNIT}"), true),
        ("# Source\n\n".to_string(), READ_B.to_string(), false),
    ] {
        let s = fixture(&format!("pair-abandon-disagree-{both}"));
        s.kill_at("move_destination_replaced", "__test-pair", &json!({}), &[]);
        let op = s.record()["op"].as_str().unwrap().to_string();
        waive(&s, &op);
        std::fs::write(s.root.join(A), &a).unwrap();
        std::fs::write(s.root.join(B), &b).unwrap();
        // Only the side that changed from HEAD has anything to commit.
        s.git(&["commit", "-qam", "one side by hand"]);
        let before = abandon_state(&s);
        let (code, out) = s.run(
            "commit-pending",
            json!({"abandon":op, "reason":"one side"}),
            &[],
        );
        assert_eq!(code, 2, "{out}");
        assert_eq!(out["error"]["code"], "pending_unresolved", "{out}");
        let message = out["error"]["message"].as_str().unwrap();
        assert!(message.contains("pairUnit"), "{message}");
        assert!(
            message.contains(if both { "both files" } else { "neither file" }),
            "{message}"
        );
        assert_eq!(abandon_state(&s), before);
        // Finishing the move by hand makes both sides agree.
        std::fs::write(s.root.join(A), "# Source\n\n").unwrap();
        std::fs::write(s.root.join(B), format!("{READ_B}{UNIT}")).unwrap();
        s.git(&["commit", "-qam", "finish the move by hand"]);
        let out = s.recover(json!({"abandon":op, "reason":"finished by hand"}), &[]);
        assert_eq!(out["unresolved"], 0, "{out}");
        let event = s.events(&op).pop().unwrap();
        assert_eq!(event["outcome"], "abandoned");
        assert_eq!(
            (&event["applied"], &event["peer_applied"]),
            (&json!(true), &json!(true))
        );
    }
}

#[test]
fn pair_abandon_refuses_an_edited_pair_that_keeps_the_stay_twice() {
    let s = fixture("pair-abandon-edited-duplicate");
    s.kill_at("move_destination_replaced", "__test-pair", &json!({}), &[]);
    let op = s.record()["op"].as_str().unwrap().to_string();
    waive(&s, &op);
    // An outside edit (neither read nor candidate) that keeps the unit, beside
    // a destination that carries it too.
    std::fs::write(
        s.root.join(A),
        format!("# Source\n\nOutside edit.\n\n{UNIT}"),
    )
    .unwrap();
    s.git(&["commit", "-qam", "edited pair together"]);
    let before = abandon_state(&s);
    let (code, out) = s.run("commit-pending", json!({"abandon":op, "reason":"dup"}), &[]);
    assert_eq!(code, 2, "{out}");
    assert_eq!(out["error"]["code"], "pending_unresolved", "{out}");
    assert_eq!(abandon_state(&s), before);
}

#[test]
fn pair_published_move_cannot_waive_its_fence() {
    let s = fixture("pair-published-waiver");
    s.kill_at("ref_updated", "__test-pair", &json!({}), &[]);
    let op = s.record()["op"].as_str().unwrap().to_string();
    let project = markrivet_core::fs::Project::open(&s.root).unwrap();
    let locks = markrivet_core::git::list_locks(&project, Some("refs/heads/main")).unwrap();
    let identity = &locks
        .iter()
        .find(|l| l.file == ".git/index.lock")
        .unwrap()
        .identity;
    let (code, out) = s.run(
        "commit-pending",
        json!({"clear_lock":".git/index.lock", "identity":identity,"op":op}),
        &[],
    );
    assert_eq!(code, 2, "{out}");
    fence(&s);
    settled(&s, true);
}

#[test]
fn pair_published_move_audit_is_independent_of_git_message_encodings() {
    for key in ["i18n.logOutputEncoding", "i18n.commitEncoding"] {
        for encoding in ["UTF-16LE", "UTF-16BE", "UTF-32LE"] {
            let s = fixture(&format!("pair-waive-{key}-{encoding}"));
            if key == "i18n.commitEncoding" {
                s.git(&["config", key, encoding]);
            }
            s.kill_at("ref_updated", "__test-pair", &json!({}), &[]);
            if key == "i18n.logOutputEncoding" {
                s.git(&["config", key, encoding]);
            }
            let record = s.record();
            let op = record["op"].as_str().unwrap();
            assert!(s.git(&["cat-file", "commit", "HEAD"]).contains(op));
            let project = markrivet_core::fs::Project::open(&s.root).unwrap();
            let locks = markrivet_core::git::list_locks(&project, Some("refs/heads/main")).unwrap();
            let index = locks.iter().find(|l| l.file == ".git/index.lock").unwrap();
            let pending = s.root.join(format!(".rivet/pending/{op}.json"));
            let before = (
                read(&s.root, A),
                read(&s.root, B),
                s.head(),
                std::fs::read(s.root.join(".git/index")).unwrap(),
                std::fs::read(&pending).unwrap(),
                s.receipts(),
            );
            let (code, out) = s.run(
                "commit-pending",
                json!({
                    "clear_lock":index.file,"identity":index.identity,"op":op
                }),
                &[],
            );
            assert_eq!(code, 2, "{encoding}: {out}");
            assert_eq!(out["error"]["code"], "clear_lock_refused", "{out}");
            assert_eq!(
                (
                    read(&s.root, A),
                    read(&s.root, B),
                    s.head(),
                    std::fs::read(s.root.join(".git/index")).unwrap(),
                    std::fs::read(&pending).unwrap(),
                    s.receipts()
                ),
                before
            );
            assert_fence(&s, &record);
            let recovered = s.recover(json!({}), &[]);
            assert_eq!(recovered["unresolved"], 0, "{encoding}: {recovered}");
            assert!(s.records().is_empty());
            assert!(!s.root.join(".git/index.lock").exists());
            // Inspect raw objects too: a misleading stored encoding header can
            // hide the trailer from the test's plain formatted Git log reader.
            assert!(s.git(&["cat-file", "commit", "HEAD"]).contains(op));
            assert_eq!(
                s.git(&[
                    "rev-list",
                    &format!("{}..HEAD", record["base_head"].as_str().unwrap())
                ])
                .lines()
                .collect::<Vec<_>>(),
                [s.head().as_str()]
            );
            assert_eq!(s.changed(&s.head()), [A, B]);
            assert_eq!(read(&s.root, A), "# Source\n\n");
            assert_eq!(read(&s.root, B), format!("{READ_B}{UNIT}"));
            let events = s.events(op);
            let terminal: Vec<_> = events.iter().filter(|e| e["outcome"] != "failed").collect();
            assert_eq!(terminal.len(), 1, "{events:?}");
            assert_eq!(terminal[0]["applied"], true);
            assert_eq!(terminal[0]["peer_applied"], true);
        }
    }
}

#[test]
fn pair_missing_or_malformed_history_objects_never_prove_absence() {
    for kind in [
        "missing",
        "truncated",
        "wrong_oid",
        "non_commit",
        "extra",
        "oversized",
    ] {
        let s = fixture(&format!("pair-history-object-{kind}"));
        s.kill_at("ref_updated", "__test-pair", &json!({}), &[]);
        let record = s.record();
        let op = record["op"].as_str().unwrap();
        let head = s.head();
        let response = match kind {
            "missing" => format!("{head} missing\n'"),
            "truncated" => format!("{head} commit 99999\nshort\n"),
            "wrong_oid" => format!("{} commit 0\n\n", "0".repeat(head.len())),
            "non_commit" => format!("{head} blob 0\n\n"),
            "oversized" => String::new(),
            "extra" => {
                let bytes = s.git(&["cat-file", "commit", "HEAD"]);
                format!("{head} commit {}\n{bytes}\nextra", bytes.len())
            }
            _ => unreachable!(),
        };
        let response = response.replace('\'', "'\\\''");
        let emit = if kind == "oversized" {
            "head -c 131072 /dev/zero >&2".to_string()
        } else {
            format!("printf '%s' '{response}'")
        };
        let wrapper = s.script(
            "history-gap",
            &format!(
                "case \"$*\" in *\" cat-file --batch\") {emit}; exit 0;; esac\nexec git \"$@\""
            ),
        );
        let project = markrivet_core::fs::Project::open(&s.root).unwrap();
        let locks = markrivet_core::git::list_locks(&project, Some("refs/heads/main")).unwrap();
        let index = locks.iter().find(|l| l.file == ".git/index.lock").unwrap();
        let pending = s.root.join(format!(".rivet/pending/{op}.json"));
        let before = (
            read(&s.root, A),
            read(&s.root, B),
            s.head(),
            std::fs::read(s.root.join(".git/index")).unwrap(),
            std::fs::read(&pending).unwrap(),
            s.receipts(),
        );
        let (code, out) = s.run(
            "commit-pending",
            json!({
                "clear_lock":index.file,"identity":index.identity,"op":op
            }),
            &[wrapper],
        );
        assert_eq!(code, 2, "{kind}: {out}");
        assert_eq!(out["error"]["code"], "clear_lock_refused", "{out}");
        let expected = match kind {
            "missing" | "non_commit" => "missing or non-commit history object",
            "truncated" => "truncated history object",
            "wrong_oid" => "history object order differs",
            "extra" => "unexpected history object output",
            "oversized" => "git read exceeded git_stderr",
            _ => unreachable!(),
        };
        assert!(
            out["error"]["message"].as_str().unwrap().contains(expected),
            "{kind}: {out}"
        );
        assert_eq!(
            (
                read(&s.root, A),
                read(&s.root, B),
                s.head(),
                std::fs::read(s.root.join(".git/index")).unwrap(),
                std::fs::read(&pending).unwrap(),
                s.receipts()
            ),
            before
        );
        assert_fence(&s, &record);
        settled(&s, true);
    }
}

#[test]
fn pair_and_single_path_large_messages_recover_after_publication() {
    for command in ["set", "delete", "archive-copy", "__test-pair"] {
        let s = if command == "__test-pair" {
            fixture("pair-large-published-message")
        } else {
            archive_store("single-large-published-message")
        };
        let message = format!("Large synthetic message\n\n{}", "m".repeat(5 * 1024 * 1024));
        if command == "__test-pair" {
            // The debug planner remains fixed and takes no metadata. Supply
            // the same supported attribution through its durable test record.
            s.kill_at("move_applied", command, &json!({}), &[]);
            let mut record = s.record();
            let old = record["message"].as_str().unwrap();
            let trailers = &old[old.find("\n\nRivet-Op:").unwrap()..];
            record["message"] = json!(format!("{message}{trailers}"));
            record["user_message"] = json!(message);
            let op = record["op"].as_str().unwrap();
            std::fs::write(
                s.root.join(format!(".rivet/pending/{op}.json")),
                serde_json::to_vec(&record).unwrap(),
            )
            .unwrap();
            s.kill_at("committed", "commit-pending", &json!({}), &[]);
        } else {
            let id = s.items()[1].clone();
            let mut request = match command {
                "set" => s.set_request(&id, "[x] **Synthetic.** Changed."),
                "delete" => receipted_request(&s, &id),
                "archive-copy" => archive_request(&s, &id),
                _ => unreachable!(),
            };
            request["message"] = json!(message);
            s.kill_at("committed", command, &request, &[]);
        }
        let record = s.record();
        let op = record["op"].as_str().unwrap();
        let head = s.head();
        assert!(s.git(&["cat-file", "commit", "HEAD"]).contains(op));
        let index = std::fs::read(s.root.join(".git/index")).unwrap();
        let paths = if command == "__test-pair" {
            vec![A, B]
        } else if command == "archive-copy" {
            vec![WIDGETS, "archive/month/closed.md"]
        } else {
            vec![WIDGETS]
        };
        let bytes: Vec<_> = paths.iter().map(|p| read(&s.root, p)).collect();
        let out = s.recover(json!({}), &[]);
        assert_eq!(out["unresolved"], 0, "{command}: {out}");
        assert_eq!(s.head(), head, "{command}: {out}");
        assert_eq!(std::fs::read(s.root.join(".git/index")).unwrap(), index);
        assert_eq!(
            paths.iter().map(|p| read(&s.root, p)).collect::<Vec<_>>(),
            bytes
        );
        assert!(s.records().is_empty());
        assert!(!s.root.join(".git/index.lock").exists());
        assert_eq!(s.carrying(op), [head]);
        let events = s.events(op);
        let terminal: Vec<_> = events.iter().filter(|e| e["outcome"] != "failed").collect();
        assert_eq!(terminal.len(), 1, "{command}: {events:?}");
        assert_eq!(terminal[0]["outcome"], "committed");
        if command == "__test-pair" {
            assert_eq!(terminal[0]["peer_applied"], true);
        }
    }
}

#[test]
fn pair_shallow_history_cannot_hide_a_published_move() {
    let s = fixture("pair-waive-shallow");
    s.kill_at("ref_updated", "__test-pair", &json!({}), &[]);
    let record = s.record();
    let op = record["op"].as_str().unwrap();
    let published = s.head();
    let tree = s.git(&["rev-parse", "HEAD^{tree}"]);
    let later = s.git(&[
        "commit-tree",
        tree.trim(),
        "-p",
        &published,
        "-m",
        "synthetic later commit",
    ]);
    s.git(&["update-ref", "HEAD", later.trim(), &published]);
    std::fs::write(s.root.join(".git/shallow"), format!("{}\n", later.trim())).unwrap();
    assert!(s.git(&["cat-file", "commit", &published]).contains(op));
    assert!(s.git(&["cat-file", "commit", "HEAD"]).contains(&published));
    let project = markrivet_core::fs::Project::open(&s.root).unwrap();
    let locks = markrivet_core::git::list_locks(&project, Some("refs/heads/main")).unwrap();
    let index = locks.iter().find(|l| l.file == ".git/index.lock").unwrap();
    let pending = s.root.join(format!(".rivet/pending/{op}.json"));
    let before = (
        read(&s.root, A),
        read(&s.root, B),
        s.head(),
        std::fs::read(s.root.join(".git/index")).unwrap(),
        std::fs::read(&pending).unwrap(),
        s.receipts(),
    );
    let (code, out) = s.run(
        "commit-pending",
        json!({
            "clear_lock":index.file,"identity":index.identity,"op":op
        }),
        &[],
    );
    assert_eq!(code, 2, "{out}");
    assert_eq!(out["error"]["code"], "clear_lock_refused", "{out}");
    assert!(
        out["error"]["message"]
            .as_str()
            .unwrap()
            .contains("recorded base"),
        "{out}"
    );
    assert_eq!(
        (
            read(&s.root, A),
            read(&s.root, B),
            s.head(),
            std::fs::read(s.root.join(".git/index")).unwrap(),
            std::fs::read(&pending).unwrap(),
            s.receipts()
        ),
        before
    );
    assert_fence(&s, &record);
    std::fs::remove_file(s.root.join(".git/shallow")).unwrap();
    settled(&s, true);
}

#[test]
fn pair_waiver_rechecks_the_audited_tip_before_releasing_a_fence() {
    let s = fixture("pair-waive-audit-tip");
    s.kill_at("ref_updated", "__test-pair", &json!({}), &[]);
    let record = s.record();
    let op = record["op"].as_str().unwrap();
    let published = s.head();
    let base = record["base_head"].as_str().unwrap();
    s.git(&["update-ref", "HEAD", base, &published]);
    let project = markrivet_core::fs::Project::open(&s.root).unwrap();
    let locks = markrivet_core::git::list_locks(&project, Some("refs/heads/main")).unwrap();
    let index = locks.iter().find(|l| l.file == ".git/index.lock").unwrap();
    let pending = s.root.join(format!(".rivet/pending/{op}.json"));
    let before = (
        read(&s.root, A),
        read(&s.root, B),
        std::fs::read(s.root.join(".git/index")).unwrap(),
        std::fs::read(&pending).unwrap(),
        s.receipts(),
    );
    let pause = Pause::new(&s.root, "move_history_audited");
    let child = pause.wait_for(s.spawn(
        "commit-pending",
        &json!({
            "clear_lock":index.file,"identity":index.identity,"op":op
        }),
        &[pause.env()],
    ));
    // Restore a genuine move commit after the absence scan, on the same branch.
    s.git(&["update-ref", "HEAD", &published, base]);
    pause.release();
    let (code, out) = finish(child);
    assert_eq!(code, 2, "{out}");
    assert_eq!(out["error"]["code"], "clear_lock_refused", "{out}");
    assert_eq!(s.head(), published);
    assert_eq!(
        (
            read(&s.root, A),
            read(&s.root, B),
            std::fs::read(s.root.join(".git/index")).unwrap(),
            std::fs::read(&pending).unwrap(),
            s.receipts()
        ),
        before
    );
    assert_fence(&s, &record);
    settled(&s, true);
}

#[test]
fn pair_waiver_cannot_hide_a_published_op_by_switching_head() {
    for hook in ["ref_updated", "move_destination_replaced"] {
        let s = fixture(&format!("pair-waive-branch-{hook}"));
        s.kill_at(hook, "__test-pair", &json!({}), &[]);
        let record = s.record();
        let op = record["op"].as_str().unwrap();
        if hook == "ref_updated" {
            assert!(
                s.git(&["log", "refs/heads/main", "--format=%B"])
                    .contains(op)
            );
        }
        s.git(&[
            "update-ref",
            "refs/heads/foreign",
            record["base_head"].as_str().unwrap(),
        ]);
        s.git(&["symbolic-ref", "HEAD", "refs/heads/foreign"]);
        let project = markrivet_core::fs::Project::open(&s.root).unwrap();
        let locks = markrivet_core::git::list_locks(&project, Some("refs/heads/main")).unwrap();
        let index = locks.iter().find(|l| l.file == ".git/index.lock").unwrap();
        let pending = s.root.join(format!(".rivet/pending/{op}.json"));
        let before = (
            read(&s.root, A),
            read(&s.root, B),
            s.head(),
            std::fs::read(s.root.join(".git/index")).unwrap(),
            std::fs::read(&pending).unwrap(),
            s.receipts(),
        );
        let (code, out) = s.run(
            "commit-pending",
            json!({"clear_lock":index.file,"identity":index.identity,"op":op}),
            &[],
        );
        assert_eq!(code, 2, "{hook}: {out}");
        assert_eq!(out["error"]["code"], "clear_lock_refused", "{out}");
        assert!(
            out["error"]["message"].as_str().unwrap().contains("branch"),
            "{out}"
        );
        assert_eq!(
            (
                read(&s.root, A),
                read(&s.root, B),
                s.head(),
                std::fs::read(s.root.join(".git/index")).unwrap(),
                std::fs::read(&pending).unwrap(),
                s.receipts()
            ),
            before
        );
        assert_fence(&s, &record);
        s.git(&["symbolic-ref", "HEAD", "refs/heads/main"]);
        settled(&s, true);
    }
}

#[test]
fn pair_shared_archive_identity_check_refuses_a_conflicted_legacy_copy_claim() {
    let s = archive_store("pair-legacy-archive-copy");
    let id = s.items()[0].clone();
    let other = "archive/month/conflict.md";
    std::fs::write(
        s.root.join(other),
        format!("# Synthetic\n\n## Archived {{#{id}}}\n<!-- stay:Foreign1 -->\n"),
    )
    .unwrap();
    s.git(&["add", other]);
    s.git(&["commit", "-qm", "synthetic conflicting archive"]);
    let select = json!({"kind":"heading_path","path":["Closed"]});
    let (code, destination) = s.run(
        "get",
        json!({"path":"archive/month/closed.md","select":select}),
        &[],
    );
    assert_eq!(code, 0, "{destination}");
    let request = json!({"source":WIDGETS,"id":id,"guard":item_guard(&s.root,WIDGETS,&id),
        "path":"archive/month/closed.md","select":select,"destination_guard":destination["revision"]});
    let before = (
        read(&s.root, WIDGETS),
        read(&s.root, "archive/month/closed.md"),
        read(&s.root, other),
        s.head(),
        std::fs::read(s.root.join(".git/index")).unwrap(),
        s.receipts(),
    );
    let (code, out) = s.run("archive-copy", request, &[]);
    assert_eq!(code, 2, "{out}");
    assert_eq!(out["error"]["code"], "archive_copy_refused", "{out}");
    assert_eq!(
        (
            read(&s.root, WIDGETS),
            read(&s.root, "archive/month/closed.md"),
            read(&s.root, other),
            s.head(),
            std::fs::read(s.root.join(".git/index")).unwrap(),
            s.receipts()
        ),
        before
    );
    assert!(s.records().is_empty());
}

#[test]
fn pair_foreign_fence_is_never_adopted() {
    let s = fixture("pair-foreign-fence");
    s.kill_at("move_destination_replaced", "__test-pair", &json!({}), &[]);
    let saved = s.root.join(".git/old-fence");
    std::fs::rename(s.root.join(".git/index.lock"), &saved).unwrap();
    std::fs::write(s.root.join(".git/index.lock"), "foreign").unwrap();
    let before = (read(&s.root, A), read(&s.root, B), s.head());
    let out = s.recover(json!({}), &[]);
    assert_eq!(out["records"][0]["reason"], "stale_git_lock", "{out}");
    assert_eq!((read(&s.root, A), read(&s.root, B), s.head()), before);
    assert_eq!(read(&s.root, ".git/index.lock"), "foreign");
}

#[test]
fn pair_preflight_head_movement_withdraws_its_record_before_any_rename() {
    let s = fixture("pair-preflight-head");
    let pause = Pause::new(&s.root, "move_fenced");
    let child = pause.wait_for(s.spawn("__test-pair", &json!({}), &[pause.env()]));
    // The record names the fence before the checks under it run.
    fence(&s);
    let tree = s.git(&["rev-parse", "HEAD^{tree}"]);
    let sha = s.git(&["commit-tree", tree.trim(), "-p", &s.head(), "-m", "foreign"]);
    s.git(&["update-ref", "HEAD", sha.trim()]);
    let index = std::fs::read(s.root.join(".git/index")).unwrap();
    pause.release();
    let (code, out) = finish(child);
    assert_eq!(code, 2, "{out}");
    assert!(out.get("pending").is_none(), "{out}");
    assert_eq!(read(&s.root, A), READ_A);
    assert_eq!(read(&s.root, B), READ_B);
    assert_eq!(std::fs::read(s.root.join(".git/index")).unwrap(), index);
    assert_eq!(s.head(), sha.trim());
    assert!(s.records().is_empty());
    assert!(!s.root.join(".git/index.lock").exists());
    no_staging(&s);
    assert!(s.receipts().is_empty());
}

#[test]
fn pair_reacquired_fence_is_published_before_a_second_crash() {
    for hook in ["move_destination_replaced", "ref_updated"] {
        let s = fixture(&format!("pair-reacquire-{hook}"));
        s.kill_at(hook, "__test-pair", &json!({}), &[]);
        // A controlled stand-in for loss of the fence name after a crash.
        std::fs::remove_file(s.root.join(".git/index.lock")).unwrap();
        s.kill_at("move_fence_reacquired", "commit-pending", &json!({}), &[]);
        fence(&s);
        settled(&s, true);
    }
}

#[test]
fn pair_replacement_fence_publication_failure_leaves_its_staging_entry() {
    let s = fixture("pair-reacquire-publish");
    s.kill_at("move_destination_replaced", "__test-pair", &json!({}), &[]);
    let before = (
        read(&s.root, A),
        read(&s.root, B),
        s.head(),
        std::fs::read(s.root.join(".git/index")).unwrap(),
    );
    let record = s.record();
    let op = record["op"].as_str().unwrap().to_string();
    // A controlled stand-in for loss of the fence name after a crash.
    std::fs::remove_file(s.root.join(".git/index.lock")).unwrap();
    let out = s.recover(json!({}), &[("RIVET_TEST_FAULT", "record_publish".into())]);
    assert_eq!(out["records"][0]["reason"], "stale_git_lock", "{out}");
    assert_eq!(out["unresolved"], 1, "{out}");
    assert_eq!(s.record(), record);
    // An identity that was never published is never named; its staging
    // entry stays for the operator.
    assert!(!s.root.join(".git/index.lock").exists());
    assert!(staging(&s, &op).is_file());
    assert_eq!(
        (
            read(&s.root, A),
            read(&s.root, B),
            s.head(),
            std::fs::read(s.root.join(".git/index")).unwrap()
        ),
        before
    );
    assert!(s.receipts().is_empty());
    staged_repair(&s, &op);
    std::fs::remove_file(staging(&s, &op)).unwrap();
    let out = s.recover(json!({}), &[]);
    assert_eq!(out["unresolved"], 0, "{out}");
    let commits = s.carrying(&op);
    assert_eq!(commits.len(), 1);
    assert_eq!(s.changed(&commits[0]), [A, B]);
}

#[test]
fn pair_replacement_fence_is_owned_by_its_record_on_every_acquisition_error() {
    for fault in [
        "move_fence_birth",
        "move_fence_noreplace",
        "move_fence_sync",
        "move_fence_index_read",
    ] {
        let s = fixture(&format!("pair-reacquire-early-{fault}"));
        s.kill_at("move_destination_replaced", "__test-pair", &json!({}), &[]);
        let record = s.record();
        let op = record["op"].as_str().unwrap().to_string();
        let before = (
            read(&s.root, A),
            read(&s.root, B),
            s.head(),
            std::fs::read(s.root.join(".git/index")).unwrap(),
        );
        std::fs::remove_file(s.root.join(".git/index.lock")).unwrap();
        let out = s.recover(json!({}), &[("RIVET_TEST_FAULT", fault.into())]);
        // Birth time is checked before anything is created; the rename leaves
        // its staging entry; sync and the index read fail after the record
        // already names the new fence.
        let reason = if fault == "move_fence_birth" {
            "fence_unsupported"
        } else {
            "stale_git_lock"
        };
        assert_eq!(out["records"][0]["reason"], reason, "{fault}: {out}");
        assert_eq!(out["unresolved"], 1, "{out}");
        assert_eq!(
            (
                read(&s.root, A),
                read(&s.root, B),
                s.head(),
                std::fs::read(s.root.join(".git/index")).unwrap()
            ),
            before
        );
        assert!(s.receipts().is_empty());
        match fault {
            "move_fence_birth" => {
                no_staging(&s);
                assert_eq!(s.record()["fence"], record["fence"]);
                assert!(!s.root.join(".git/index.lock").exists(), "{fault}");
                let out = s.recover(json!({}), &[]);
                assert_eq!(out["unresolved"], 0, "{out}");
            }
            "move_fence_noreplace" => {
                staged_repair(&s, &op);
                std::fs::remove_file(staging(&s, &op)).unwrap();
                let out = s.recover(json!({}), &[]);
                assert_eq!(out["unresolved"], 0, "{out}");
            }
            _ => {
                no_staging(&s);
                assert_ne!(s.record()["fence"], record["fence"]);
                fence(&s);
                settled(&s, true);
            }
        }
    }
}

#[test]
fn pair_compensation_never_removes_the_remaining_live_copy() {
    let s = fixture("pair-no-compensate");
    let pause = Pause::new(&s.root, "move_destination_replaced");
    let child = pause.wait_for(s.spawn(
        "__test-pair",
        &json!({}),
        &[
            pause.env(),
            ("RIVET_TEST_FAULT", "move_source_rename".into()),
        ],
    ));
    std::fs::write(s.root.join(A), "outside save removed the unit\n").unwrap();
    pause.release();
    let (code, out) = finish(child);
    assert_eq!(code, 3, "{out}");
    assert_eq!(read(&s.root, B), format!("{READ_B}{UNIT}"));
    assert_eq!(read(&s.root, A), "outside save removed the unit\n");
    fence(&s);
    let out = s.recover(json!({}), &[]);
    assert_eq!(out["records"][0]["reason"], "file_changed", "{out}");
}

#[test]
fn pair_compensation_failure_keeps_fence_for_roll_forward() {
    let s = fixture("pair-compensation-failed");
    let (code, out) = s.run(
        "__test-pair",
        json!({}),
        &[(
            "RIVET_TEST_FAULT",
            "move_source_rename,move_compensate".into(),
        )],
    );
    assert_eq!(code, 3, "{out}");
    fence(&s);
    settled(&s, true);
}

#[test]
fn pair_missing_birth_time_refuses_without_writes() {
    let s = fixture("pair-no-birth");
    let head = s.head();
    let index = std::fs::read(s.root.join(".git/index")).unwrap();
    let (code, out) = s.run(
        "__test-pair",
        json!({}),
        &[("RIVET_TEST_FAULT", "move_fence_birth".into())],
    );
    assert_eq!(code, 2, "{out}");
    assert_eq!(read(&s.root, A), READ_A);
    assert_eq!(read(&s.root, B), READ_B);
    assert_eq!(s.head(), head);
    assert_eq!(std::fs::read(s.root.join(".git/index")).unwrap(), index);
    assert!(s.records().is_empty());
    assert!(s.receipts().is_empty());
    assert!(!s.root.join(".git/index.lock").exists());
}

#[test]
fn pair_prune_snapshot_never_settles_its_pending_inputs() {
    let s = fixture("pair-snapshot");
    s.kill_at("move_destination_replaced", "__test-pair", &json!({}), &[]);
    let before = (read(&s.root, A), read(&s.root, B), s.head());
    let (code, out) = s.run("prune-snapshot", json!({}), &[]);
    assert_eq!(code, 2, "{out}");
    assert_eq!(out["error"]["code"], "pending_unresolved", "{out}");
    assert_eq!((read(&s.root, A), read(&s.root, B), s.head()), before);
    fence(&s);
}

#[test]
fn pair_archive_copy_checks_its_source_ownership_before_policy_or_planning() {
    for broken in [false, true] {
        let s = fixture(&format!("pair-archive-barrier-{broken}"));
        s.kill_at("move_destination_replaced", "__test-pair", &json!({}), &[]);
        if broken {
            std::fs::write(s.root.join(".rivet/project.json"), "broken").unwrap();
        }
        let (_, out) = s.run(
            "archive-copy",
            json!({"source":A,"id":"pairUnit","guard":"unused",
            "path":"archive/target.md","select":{"kind":"heading_path","path":["Archived"]},
            "destination_guard":"unused"}),
            &[],
        );
        if broken {
            assert_eq!(out["error"]["code"], "pending_unresolved", "{out}");
            fence(&s);
        } else {
            assert!(s.records().is_empty(), "{out}");
            assert_eq!(s.changed(&s.head()), [A, B]);
            assert!(!s.root.join(".git/index.lock").exists());
        }
    }
}

#[test]
fn pair_published_index_repair_syncs_documents_before_consuming_the_fence() {
    let s = fixture("pair-repair-durability");
    s.kill_at("ref_updated", "__test-pair", &json!({}), &[]);
    let index = std::fs::read(s.root.join(".git/index")).unwrap();
    let out = s.recover(
        json!({}),
        &[("RIVET_TEST_FAULT", "move_recovery_sync".into())],
    );
    assert_eq!(out["records"][0]["reason"], "not_durable", "{out}");
    assert_eq!(std::fs::read(s.root.join(".git/index")).unwrap(), index);
    fence(&s);
    settled(&s, true);
}

#[test]
fn pair_abandonment_reports_receipt_append_and_sync_failures() {
    for fault in ["receipt_append", "receipt_sync"] {
        let s = fixture(&format!("pair-abandon-receipt-{fault}"));
        s.kill_at("move_pending_written", "__test-pair", &json!({}), &[]);
        let op = s.record()["op"].as_str().unwrap().to_string();
        let (code, out) = s.run(
            "commit-pending",
            json!({"abandon":op,"reason":"test"}),
            &[("RIVET_TEST_FAULT", fault.into())],
        );
        assert_eq!(code, 2, "{fault}: {out}");
        assert!(out.get("error").is_some(), "{out}");
        assert_eq!(s.records().len(), 1);
        assert_eq!(read(&s.root, A), READ_A);
        assert_eq!(read(&s.root, B), READ_B);
        let out = s.recover(json!({}), &[]);
        assert_eq!(out["unresolved"], 0, "{out}");
        assert!(!s.root.join(".git/index.lock").exists());
    }
}

#[test]
fn pair_abandonment_rechecks_after_reacquiring_and_syncing_its_fence() {
    for change in ["file", "history"] {
        let s = fixture(&format!("pair-abandon-recheck-{change}"));
        s.kill_at("move_pending_written", "__test-pair", &json!({}), &[]);
        let op = s.record()["op"].as_str().unwrap().to_string();
        let request = json!({"abandon":op,"reason":"synthetic"});
        let (code, out) = s.run(
            "commit-pending",
            request.clone(),
            &[("RIVET_TEST_FAULT", "receipt_append".into())],
        );
        assert_eq!(code, 2, "{out}");
        assert!(!s.root.join(".git/index.lock").exists());
        let pause = Pause::new(&s.root, "move_fence_reacquired");
        let child = pause.wait_for(s.spawn("commit-pending", &request, &[pause.env()]));
        if change == "file" {
            std::fs::write(s.root.join(A), "# Source\n\n").unwrap();
        } else {
            let old = s.head();
            let tree = s.git(&["rev-parse", "HEAD^{tree}"]);
            let message = format!("outside publication\n\nRivet-Op: {op}");
            let sha = s.git(&["commit-tree", tree.trim(), "-p", &old, "-m", &message]);
            s.git(&["update-ref", "HEAD", sha.trim(), &old]);
        }
        pause.release();
        let (code, out) = finish(child);
        assert_eq!(code, 2, "{out}");
        let message = out["error"]["message"].as_str().unwrap();
        assert!(
            message.ends_with("fence stays at .git/index.lock"),
            "{message}"
        );
        assert_eq!(s.records().len(), 1);
        assert!(s.receipts().is_empty());
        fence(&s);
        assert_eq!(read(&s.root, B), READ_B);
        if change == "file" {
            assert_eq!(read(&s.root, A), "# Source\n\n");
        }
    }
}

#[test]
fn pair_conflicting_legacy_archive_ids_refuse_publication_and_roll_forward() {
    for recovery in [false, true] {
        let s = fixture(&format!("pair-archive-conflict-{recovery}"));
        let config_path = s.root.join(".rivet/project.json");
        let mut config: Value =
            serde_json::from_slice(&std::fs::read(&config_path).unwrap()).unwrap();
        config["collections"]["actions"]["archive"] = json!(["archive/*.md"]);
        std::fs::write(config_path, serde_json::to_vec(&config).unwrap()).unwrap();
        std::fs::create_dir_all(s.root.join("archive")).unwrap();
        if recovery {
            s.kill_at("move_destination_replaced", "__test-pair", &json!({}), &[]);
        }
        std::fs::write(
            s.root.join("archive/conflict.md"),
            "## Archived {#pairUnit}\n<!-- stay:Foreign1 -->\n",
        )
        .unwrap();
        let before = (
            read(&s.root, A),
            read(&s.root, B),
            s.head(),
            std::fs::read(s.root.join(".git/index")).unwrap(),
        );
        if recovery {
            let out = s.recover(json!({}), &[]);
            assert_eq!(out["records"][0]["reason"], "archived_id", "{out}");
            fence(&s);
        } else {
            let (code, out) = s.run("__test-pair", json!({}), &[]);
            assert_eq!(code, 2, "{out}");
            assert!(s.records().is_empty());
            assert!(!s.root.join(".git/index.lock").exists());
        }
        assert_eq!(
            (
                read(&s.root, A),
                read(&s.root, B),
                s.head(),
                std::fs::read(s.root.join(".git/index")).unwrap()
            ),
            before
        );
        assert!(s.receipts().is_empty());
    }
}

#[test]
fn pair_recovery_rechecks_branch_in_its_fresh_preflight_before_roll_forward() {
    let s = fixture("pair-fresh-preflight");
    s.kill_at("move_destination_replaced", "__test-pair", &json!({}), &[]);
    let before = (read(&s.root, A), read(&s.root, B));
    let pause = Pause::new(&s.root, "move_recovery_preflight");
    let child = pause.wait_for(s.spawn("commit-pending", &json!({}), &[pause.env()]));
    s.git(&["update-ref", "refs/heads/foreign", &s.head()]);
    s.git(&["symbolic-ref", "HEAD", "refs/heads/foreign"]);
    pause.release();
    let (code, out) = finish(child);
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["records"][0]["reason"], "target_changed", "{out}");
    assert_eq!((read(&s.root, A), read(&s.root, B)), before);
    fence(&s);
    s.git(&["symbolic-ref", "HEAD", "refs/heads/main"]);
    settled(&s, true);
}

#[test]
fn pair_waived_recovery_is_only_a_report() {
    let s = fixture("pair-waived-read");
    s.kill_at("move_pending_written", "__test-pair", &json!({}), &[]);
    let record = s.record();
    let project = markrivet_core::fs::Project::open(&s.root).unwrap();
    let locks = markrivet_core::git::list_locks(&project, Some("refs/heads/main")).unwrap();
    let identity = &locks
        .iter()
        .find(|l| l.file == ".git/index.lock")
        .unwrap()
        .identity;
    s.recover(
        json!({"op":record["op"],"clear_lock":".git/index.lock","identity":identity}),
        &[],
    );
    let pending = s.record();
    let events = s.receipts();
    s.recover(json!({}), &[]);
    assert_eq!(s.record(), pending);
    assert_eq!(s.receipts(), events);
    assert!(!s.root.join(".git/index.lock").exists());
}

#[test]
fn pair_index_repair_rereads_each_worktree_file_before_installing() {
    for path in [A, B] {
        let s = fixture(&format!("pair-repair-reread-{path}"));
        s.kill_at("ref_updated", "__test-pair", &json!({}), &[]);
        let index = std::fs::read(s.root.join(".git/index")).unwrap();
        let pause = Pause::new(&s.root, "index_prepared");
        let child = pause.wait_for(s.spawn("commit-pending", &json!({}), &[pause.env()]));
        std::fs::write(s.root.join(path), "outside save during index repair\n").unwrap();
        pause.release();
        let (code, out) = finish(child);
        assert_eq!(code, 0, "{out}");
        assert_eq!(out["records"][0]["reason"], "worktree_differs", "{out}");
        assert_eq!(read(&s.root, path), "outside save during index repair\n");
        assert_eq!(std::fs::read(s.root.join(".git/index")).unwrap(), index);
        fence(&s);
        // The documented lock-free route runs while the fence blocks Git writers.
        let head_bytes = s.git(&["show", &format!("HEAD:{path}")]);
        std::fs::write(s.root.join(path), head_bytes).unwrap();
        settled(&s, true);
    }
}

#[test]
fn pair_index_repair_keeps_the_fence_when_a_worktree_mode_changes() {
    use std::os::unix::fs::PermissionsExt;
    for path in [A, B] {
        let s = fixture(&format!("pair-mode-{path}"));
        s.git(&["config", "core.fileMode", "true"]);
        s.kill_at("ref_updated", "__test-pair", &json!({}), &[]);
        let pause = Pause::new(&s.root, "index_prepared");
        let child = pause.wait_for(s.spawn("commit-pending", &json!({}), &[pause.env()]));
        std::fs::set_permissions(s.root.join(path), std::fs::Permissions::from_mode(0o755))
            .unwrap();
        pause.release();
        let (code, out) = finish(child);
        assert_eq!(code, 0, "{out}");
        assert_eq!(out["records"][0]["reason"], "worktree_differs", "{out}");
        fence(&s);
        assert_eq!(
            std::fs::metadata(s.root.join(path))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o755
        );
        std::fs::set_permissions(s.root.join(path), std::fs::Permissions::from_mode(0o644))
            .unwrap();
        settled(&s, true);
    }
}

#[test]
fn pair_waiver_crash_cannot_orphan_its_still_present_fence_on_abandonment() {
    let s = fixture("pair-waiver-crash");
    s.kill_at("move_pending_written", "__test-pair", &json!({}), &[]);
    let op = s.record()["op"].as_str().unwrap().to_string();
    let project = markrivet_core::fs::Project::open(&s.root).unwrap();
    let locks = markrivet_core::git::list_locks(&project, Some("refs/heads/main")).unwrap();
    let identity = locks
        .iter()
        .find(|l| l.file == ".git/index.lock")
        .unwrap()
        .identity
        .clone();
    let request = json!({"op":op,"clear_lock":".git/index.lock","identity":identity});
    s.kill_at("move_fence_waived", "commit-pending", &request, &[]);
    assert_eq!(s.record()["fence_waived"], true);
    let (code, out) = s.run(
        "commit-pending",
        json!({"abandon":op,"reason":"still fenced"}),
        &[],
    );
    assert_eq!(code, 2, "{out}");
    fence(&s);
    s.recover(request, &[]);
    assert!(!s.root.join(".git/index.lock").exists());
    let out = s.recover(json!({"abandon":op,"reason":"explicitly cleared"}), &[]);
    assert_eq!(out["unresolved"], 0, "{out}");
    assert_eq!(read(&s.root, A), READ_A);
    assert_eq!(read(&s.root, B), READ_B);
}

#[test]
fn pair_index_repair_refuses_a_target_changed_in_git_after_preparation() {
    let s = fixture("pair-repair-head");
    s.kill_at("ref_updated", "__test-pair", &json!({}), &[]);
    let old = s.head();
    let base = s.git(&["rev-parse", "HEAD^"]);
    let tree = s.git(&["rev-parse", &format!("{}^{{tree}}", base.trim())]);
    let foreign = s.git(&[
        "commit-tree",
        tree.trim(),
        "-p",
        &old,
        "-m",
        "foreign target change",
    ]);
    let pause = Pause::new(&s.root, "index_prepared");
    let child = pause.wait_for(s.spawn("commit-pending", &json!({}), &[pause.env()]));
    s.git(&["update-ref", "HEAD", foreign.trim(), &old]);
    pause.release();
    let (code, out) = finish(child);
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["records"][0]["reason"], "target_changed", "{out}");
    fence(&s);
}

#[test]
fn pair_index_install_rechecks_after_blocking_index_io() {
    for change in ["head", "source", "destination"] {
        let s = fixture(&format!("pair-final-index-{change}"));
        s.kill_at("ref_updated", "__test-pair", &json!({}), &[]);
        let original_index = std::fs::read(s.root.join(".git/index")).unwrap();
        let old = s.head();
        let base = s.git(&["rev-parse", "HEAD^"]);
        let tree = s.git(&["rev-parse", &format!("{}^{{tree}}", base.trim())]);
        let foreign = s.git(&[
            "commit-tree",
            tree.trim(),
            "-p",
            &old,
            "-m",
            "outside change",
        ]);
        let pause = Pause::new(&s.root, "index_partial_write");
        let child = pause.wait_for(s.spawn("commit-pending", &json!({}), &[pause.env()]));
        if change == "head" {
            s.git(&["update-ref", "HEAD", foreign.trim(), &old]);
        } else {
            let path = if change == "source" { A } else { B };
            std::fs::write(s.root.join(path), "outside save during index I/O\n").unwrap();
        }
        pause.release();
        let (code, out) = finish(child);
        assert_eq!(code, 0, "{out}");
        assert_eq!(
            out["records"][0]["reason"],
            if change == "head" {
                "target_changed"
            } else {
                "worktree_differs"
            },
            "{out}"
        );
        assert_eq!(
            std::fs::read(s.root.join(".git/index")).unwrap(),
            original_index
        );
        fence(&s);
    }
}

#[test]
fn pair_completed_recovery_honors_ignored_executable_modes() {
    use std::os::unix::fs::PermissionsExt;
    let s = fixture("pair-ignored-mode");
    s.git(&["config", "core.fileMode", "false"]);
    for path in [A, B] {
        std::fs::set_permissions(s.root.join(path), std::fs::Permissions::from_mode(0o755))
            .unwrap();
    }
    s.kill_at("committed", "__test-pair", &json!({}), &[]);
    let out = s.recover(json!({}), &[]);
    assert_eq!(out["unresolved"], 0, "{out}");
    assert!(!s.root.join(".git/index.lock").exists());
    assert!(s.records().is_empty());
    assert_eq!(s.changed(&s.head()), [A, B]);
}

#[test]
fn pair_history_audit_accepts_git_legacy_log_encoding() {
    let s = fixture("pair-legacy-log");
    s.kill_at("move_applied", "__test-pair", &json!({}), &[]);
    let old = s.head();
    let tree = s.git(&["rev-parse", "HEAD^{tree}"]);
    let foreign = s.git(&[
        "commit-tree",
        tree.trim(),
        "-p",
        &old,
        "-m",
        "synthetic café",
    ]);
    s.git(&["update-ref", "HEAD", foreign.trim(), &old]);
    s.git(&["config", "i18n.logOutputEncoding", "ISO-8859-1"]);
    let raw = std::process::Command::new("git")
        .current_dir(&s.root)
        .args(["log", "-1", "--format=%B"])
        .output()
        .unwrap();
    assert!(raw.status.success());
    assert!(std::str::from_utf8(&raw.stdout).is_err());
    settled(&s, true);
}

#[test]
fn pair_unowned_foreign_fence_uses_the_existing_quiescent_clear() {
    // A lock no record names (an older engine's crash, or another tool's) is
    // still cleared only by the operator's quiescent assertion.
    let s = fixture("pair-unowned");
    std::fs::write(s.root.join(".git/index.lock"), "").unwrap();
    assert!(s.records().is_empty());
    std::fs::File::open(s.root.join(".git/index.lock"))
        .unwrap()
        .set_times(
            std::fs::FileTimes::new()
                .set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(31)),
        )
        .unwrap();
    let project = markrivet_core::fs::Project::open(&s.root).unwrap();
    let locks = markrivet_core::git::list_locks(&project, Some("refs/heads/main")).unwrap();
    let lock = locks.iter().find(|l| l.file == ".git/index.lock").unwrap();
    let out = s.recover(
        json!({"clear_lock":lock.file,"identity":lock.identity,"size":lock.size,"quiescent":true}),
        &[],
    );
    assert_eq!(out["unresolved"], 0, "{out}");
    assert!(!s.root.join(".git/index.lock").exists());
    assert_eq!(read(&s.root, A), READ_A);
    assert_eq!(read(&s.root, B), READ_B);
}

/// No Rivet-named entry is left in `.git` besides what Git itself keeps.
fn no_staging(s: &Store) {
    let left: Vec<_> = std::fs::read_dir(s.root.join(".git"))
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .filter(|n| n.starts_with("rivet-fence-"))
        .collect();
    assert!(left.is_empty(), "{left:?}");
}

/// The staging entry a record's fence passes through, `.git/rivet-fence-<op>`.
fn staging(s: &Store, op: &str) -> std::path::PathBuf {
    s.root.join(format!(".git/rivet-fence-{op}"))
}

/// Recovery refuses while a staging entry is present, names it, and leaves it.
fn staged_repair(s: &Store, op: &str) {
    let before = std::fs::read(staging(s, op)).unwrap();
    let out = s.recover(json!({}), &[]);
    assert_eq!(out["unresolved"], 1, "{out}");
    let record = &out["records"][0];
    assert_eq!(record["reason"], "stale_git_lock", "{out}");
    let detail = record["detail"].as_str().unwrap();
    assert!(detail.contains(&format!("rivet-fence-{op}")), "{detail}");
    assert_eq!(std::fs::read(staging(s, op)).unwrap(), before);
    assert!(!s.root.join(".git/index.lock").exists());
}

#[test]
fn pair_crash_between_fence_and_checks_is_settled_by_its_record() {
    // move_fenced: the fence is named and owned. move_fence_recorded: the
    // record names an inode only the staging entry holds, which Rivet never
    // removes; the operator does, then recovery settles.
    for hook in ["move_fenced", "move_fence_recorded"] {
        let s = fixture(&format!("pair-window-{hook}"));
        let head = s.head();
        s.kill_at(hook, "__test-pair", &json!({}), &[]);
        let record = s.record();
        let op = record["op"].as_str().unwrap().to_string();
        assert!(record["fence"].is_object(), "{record}");
        if hook == "move_fenced" {
            fence(&s);
        } else {
            assert!(!s.root.join(".git/index.lock").exists());
            staged_repair(&s, &op);
            std::fs::remove_file(staging(&s, &op)).unwrap();
        }
        let out = s.recover(json!({}), &[]);
        assert_eq!(out["unresolved"], 0, "{out}");
        assert_eq!(out["records"][0]["result"], "not_applied", "{out}");
        assert!(!s.root.join(".git/index.lock").exists());
        no_staging(&s);
        assert_eq!(
            (read(&s.root, A), read(&s.root, B)),
            (READ_A.into(), READ_B.into())
        );
        assert_eq!(s.head(), head);
        let event = s.events(&op).pop().unwrap();
        assert_eq!(event["outcome"], "not_applied");
        // Per-write commits work again without any lock clear.
        let (code, out) = s.set_docs("[x] Committed after the crashed move.");
        assert_eq!(code, 0, "{out}");
        assert_eq!(out["commit"]["status"], "committed", "{out}");
    }
}
