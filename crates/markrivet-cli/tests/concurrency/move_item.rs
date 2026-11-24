use super::move_fixture::{A, B, ID, MoveStore};
use super::*;
use std::os::unix::fs::MetadataExt;

pub(super) fn killed(s: &MoveStore, point: &'static str, request: &Value, extra: &[Var]) {
    let pause = Pause::new(&s.root, point);
    let mut vars = extra.to_vec();
    vars.push(pause.env());
    let mut child = pause.wait_for(s.spawn("move", request, &vars));
    child.kill().unwrap();
    child.wait().unwrap();
    for record in s.records() {
        if let Some(pid) = record["pid"].as_u64() {
            assert!(gone(markrivet_core::git::GitChild {
                pid: Some(pid.try_into().unwrap()),
                spawned: std::time::UNIX_EPOCH,
                identity: serde_json::from_value(record["child_identity"].clone()).unwrap()
            }));
        }
    }
}

pub(super) fn fence(s: &MoveStore) {
    let records = s.records();
    assert_eq!(records.len(), 1);
    let meta = std::fs::metadata(s.root.join(".git/index.lock")).unwrap();
    assert_eq!(records[0]["fence"]["dev"], meta.dev());
    assert_eq!(records[0]["fence"]["ino"], meta.ino());
    assert_eq!(
        records[0]["fence"]["btime_ns"],
        meta.created()
            .unwrap()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos() as u64
    );
    for path in [A, B] {
        let (ok, out) = s.git_try(&["commit", "-qm", "must not split", "--", path]);
        assert!(!ok && out.contains("index.lock"), "{out}");
    }
}

pub(super) fn recovered(s: &MoveStore, committed: bool) -> Value {
    let record = s.records()[0].clone();
    fence(s);
    let out = s.ok("commit-pending", json!({}));
    assert_eq!(out["unresolved"], 0, "{out}");
    assert!(s.records().is_empty());
    assert!(!s.root.join(".git/index.lock").exists());
    let commits = s.git(&[
        "log",
        "--format=%H",
        "--fixed-strings",
        &format!("--grep=Rivet-Op: {}", record["op"].as_str().unwrap()),
    ]);
    assert_eq!(commits.lines().count(), usize::from(committed));
    if committed {
        assert_eq!(
            s.git(&[
                "diff-tree",
                "-r",
                "--no-commit-id",
                "--name-only",
                commits.trim()
            ])
            .lines()
            .collect::<Vec<_>>(),
            [A, B]
        );
    }
    let events: Vec<Value> = String::from_utf8(s.receipts())
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str::<Value>(l).unwrap())
        .filter(|e| e["op"] == record["op"] && e["outcome"] != "failed")
        .collect();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["peer_path"], B);
    assert_eq!(events[0]["peer_applied"], committed);
    out
}

#[test]
fn move_item_crash_boundaries_and_retry_settle_both_paths() {
    for point in [
        "move_pending_written",
        "move_destination_replaced",
        "move_applied",
        "index_prepared",
        "before_ref_update",
        "ref_updated",
    ] {
        let s = MoveStore::new("tight", false);
        let request = s.request();
        let reads = (s.read(A), s.read(B));
        killed(&s, point, &request, &[]);
        if point == "move_destination_replaced" {
            assert!(s.read(A).contains(ID) && s.read(B).contains(ID));
        }
        let rec = s.records()[0].clone();
        assert_eq!(rec["path"], A);
        assert_eq!(rec["peer"]["path"], B);
        assert_eq!(
            rec["move_receipt"]["ids"],
            json!([ID, "child001", "child002"])
        );
        recovered(&s, point != "move_pending_written");
        if point == "move_pending_written" {
            assert_eq!((s.read(A), s.read(B)), reads);
        } else {
            let before = s.snapshot();
            let out = s.ok("move", request);
            assert_eq!(out["status"], "unchanged");
            assert_eq!(out["current"], "matches");
            assert_eq!(s.snapshot(), before);
        }
    }
    let s = MoveStore::new("tight", false);
    let request = s.request();
    killed(&s, "move_destination_replaced", &request, &[]);
    let out = s.ok("move", request);
    assert_eq!(out["status"], "unchanged");
    assert!(s.records().is_empty());
    assert!(!s.root.join(".git/index.lock").exists());
}

#[test]
fn move_item_compensation_and_durability_faults() {
    for fault in [
        "move_source_rename",
        "move_source_rename,move_compensate",
        "dir_sync",
    ] {
        let s = MoveStore::new("tight", false);
        let request = s.request();
        let reads = (s.read(A), s.read(B));
        let (code, out) = s.run("move", request, &[("RIVET_TEST_FAULT", fault.into())]);
        if fault == "move_source_rename" {
            assert_eq!(code, 2, "{out}");
            assert_eq!((s.read(A), s.read(B)), reads);
            assert!(s.records().is_empty());
            assert!(!s.root.join(".git/index.lock").exists());
        } else {
            assert_eq!(code, 3, "{out}");
            assert_eq!(s.read(A), reads.0);
            recovered(&s, true);
        }
    }
}

#[test]
fn move_item_recovery_matrix_keeps_outside_edits_and_fence() {
    for a in ["read", "candidate", "neither"] {
        for b in ["read", "candidate", "neither"] {
            let s = MoveStore::new("tight", false);
            let request = s.request();
            let reads = (s.read(A), s.read(B));
            killed(&s, "move_applied", &request, &[]);
            let candidates = (s.read(A), s.read(B));
            let aa = match a {
                "read" => reads.0.clone(),
                "candidate" => candidates.0.clone(),
                _ => format!("{}\nOutside A.\n", candidates.0),
            };
            let bb = match b {
                "read" => reads.1.clone(),
                "candidate" => candidates.1.clone(),
                _ => format!("{}\nOutside B.\n", candidates.1),
            };
            s.put(A, &aa);
            s.put(B, &bb);
            if a == "neither" || b == "neither" {
                let out = s.ok("commit-pending", json!({}));
                assert_eq!(out["unresolved"], 1, "{out}");
                assert_eq!(out["records"][0]["reason"], "file_changed", "{out}");
                assert_eq!((s.read(A), s.read(B)), (aa, bb));
                fence(&s);
                let before = s.snapshot();
                let (code, out) = s.run("move", request, &[]);
                assert_eq!(code, 2, "{out}");
                assert_eq!(out["error"]["code"], "pending_unresolved");
                assert_eq!(s.snapshot(), before);
            } else {
                recovered(&s, a != "read" || b != "read");
            }
        }
    }
}

#[test]
fn move_item_concurrent_destination_edit_is_kept_and_source_guard_never_refreshed() {
    for source in [false, true] {
        let s = MoveStore::new("tight", false);
        let request = s.request();
        let path = if source { A } else { B };
        let id = if source { ID } else { "dest0001" };
        let edit = json!({"path":path,"select":{"kind":"id","id":id},"guard":item_guard(&s.root,path,id),"content":"[x] **Edited.** Competing change."});
        let pause = Pause::new(&s.root, "planned");
        let first = pause.wait_for(s.spawn("set", &edit, &[pause.env()]));
        let second = s.spawn("move", &request, &[]);
        pause.release();
        let (code, out) = finish(first);
        assert_eq!(code, 0, "{out}");
        let (code, out) = finish(second);
        if source {
            assert_eq!(code, 2, "{out}");
            assert_eq!(out["error"]["code"], "guard_conflict");
            assert!(!s.read(B).contains(ID));
        } else {
            assert_eq!(code, 0, "{out}");
            assert!(s.read(B).contains("Competing change.") && s.read(B).contains(ID));
            assert!(!s.read(A).contains(ID));
        }
    }
}

#[test]
fn move_item_config_and_schema_changes_before_publication_or_recovery_require_repair() {
    for recovery in [false, true] {
        for kind in [
            "binding",
            "schema",
            "section_archives",
            "commit_off",
            "adopt",
        ] {
            let s = MoveStore::new("tight", false);
            let request = s.request();
            let pause = Pause::new(
                &s.root,
                if recovery {
                    "move_destination_replaced"
                } else {
                    "planned"
                },
            );
            let mut child = pause.wait_for(s.spawn("move", &request, &[pause.env()]));
            let child = if recovery {
                child.kill().unwrap();
                child.wait().unwrap();
                None
            } else {
                Some(child)
            };
            let reads = (s.read(A), s.read(B));
            if kind == "schema" {
                let path = ".rivet/schemas/tracker-action.json";
                s.put(path, &format!("{}\n", s.read(path)));
            } else {
                let mut config = s.config();
                if kind == "binding" {
                    config["collections"]["actions"]["subsections"] = json!(true);
                } else if kind == "commit_off" {
                    config["git"] = json!({"commit":"off"});
                } else if kind == "adopt" {
                    config["git"] = json!({"commit":"per-write","on_dirty":"adopt"});
                } else {
                    config["section_archives"] =
                        json!({"history":{"files":["tracking/*.md"],"archive":["history/*.md"]}});
                }
                s.set_config(config);
            }
            if recovery {
                let out = s.ok("commit-pending", json!({}));
                assert_eq!(out["unresolved"], 1, "{out}");
                assert_eq!(
                    out["records"][0]["reason"],
                    if ["commit_off", "adopt"].contains(&kind) {
                        "commit_policy_changed"
                    } else {
                        "config_changed"
                    },
                    "{out}"
                );
                fence(&s);
            } else {
                let before = s.snapshot();
                pause.release();
                let (code, out) = finish(child.unwrap());
                assert_eq!(code, 2, "{out}");
                assert!(s.records().is_empty());
                assert!(!s.root.join(".git/index.lock").exists());
                assert_eq!(s.snapshot(), before);
            }
            assert_eq!((s.read(A), s.read(B)), reads);
        }
    }
}

#[test]
fn move_item_pending_blocks_unrelated_writers_even_with_commits_off_or_bad_config() {
    for kind in ["off", "unreadable"] {
        let s = MoveStore::new("tight", false);
        let request = s.request();
        let select = json!({"kind":"heading_path","path":["Notes"]});
        let got = s.ok("get", json!({"path":WIDGETS,"select":select}));
        let edit = json!({"path":WIDGETS,"select":select,"guard":got["revision"],"part":"body","content":"Unrelated change.\n"});
        killed(&s, "move_applied", &request, &[]);
        if kind == "off" {
            let mut config = s.config();
            config["git"] = json!({"commit":"off"});
            s.set_config(config);
        } else {
            s.put(".rivet/project.json", "{broken");
        }
        let before = s.snapshot();
        let third = s.read(WIDGETS);
        let (code, out) = s.run("set", edit, &[]);
        assert_eq!(code, 2, "{kind}: {out}");
        assert_eq!(out["error"]["code"], "pending_unresolved", "{out}");
        assert_eq!(s.snapshot(), before);
        assert_eq!(s.read(WIDGETS), third);
        fence(&s);
    }
}

#[test]
fn move_item_compensation_never_overwrites_an_outside_source_edit() {
    let s = MoveStore::new("tight", false);
    let request = s.request();
    let pause = Pause::new(&s.root, "move_destination_replaced");
    let child = pause.wait_for(s.spawn(
        "move",
        &request,
        &[
            pause.env(),
            ("RIVET_TEST_FAULT", "move_source_rename".into()),
        ],
    ));
    let edited = format!("{}\nOutside source edit.\n", s.read(A));
    s.put(A, &edited);
    pause.release();
    let (code, out) = finish(child);
    assert_eq!(code, 3, "{out}");
    assert_eq!(s.read(A), edited);
    assert!(s.read(B).contains(ID));
    fence(&s);
    let out = s.ok("commit-pending", json!({}));
    assert_eq!(out["unresolved"], 1, "{out}");
    assert_eq!(s.read(A), edited);
    fence(&s);
}

#[test]
fn move_item_recovery_rechecks_archive_union_absence() {
    for section in [false, true] {
        let s = MoveStore::new("tight", false);
        let mut config = s.config();
        if section {
            config["section_archives"] =
                json!({"history":{"files":["tracking/*.md"],"archive":["history/*.md"]}});
        } else {
            config["collections"]["actions"]["archive"] = json!(["history/*.md"]);
        }
        s.set_config(config);
        s.commit();
        let request = s.request();
        killed(&s, "move_destination_replaced", &request, &[]);
        std::fs::create_dir(s.root.join("history")).unwrap();
        s.put(
            "history/closed.md",
            "# Archive\n\nEarlier <!-- stay:child001 -->\n",
        );
        let before = (s.read(A), s.read(B));
        let out = s.ok("commit-pending", json!({}));
        assert_eq!(out["unresolved"], 1, "{out}");
        assert_eq!((s.read(A), s.read(B)), before);
        fence(&s);
    }
}
