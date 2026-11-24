use super::*;
use std::os::unix::fs::PermissionsExt;

fn certificate(s: &Store, path: &str) -> Value {
    let (code, map) = s.run("map", json!({"path": path}), &[]);
    assert_eq!(code, 0, "{map}");
    json!({"revision": map["revision"], "head": s.head()})
}

fn request(path: &str, expect: &Value) -> Value {
    json!({"path": path, "expect": expect, "message": "adopt checked edits", "session": "checked-writer"})
}

// Capture after the external mutation, immediately before releasing the writer.
// Include every pending file (messages too), not just decoded record counts.
fn snapshot(s: &Store, paths: &[&str]) -> Value {
    let mut pending = vec![];
    if let Ok(entries) = std::fs::read_dir(s.root.join(".rivet/pending")) {
        for entry in entries {
            let path = entry.unwrap().path();
            pending.push((
                path.file_name().unwrap().to_str().unwrap().to_owned(),
                std::fs::read_to_string(&path).unwrap(),
            ));
        }
    }
    pending.sort();
    json!({
        "files": paths.iter().map(|path| json!({"path":path,
            "bytes":std::fs::read_to_string(s.root.join(path)).unwrap(),
            "mode":std::fs::metadata(s.root.join(path)).unwrap().permissions().mode()})).collect::<Vec<_>>(),
        "head":s.head(), "index":s.git(&["ls-files", "-s"]),
        "seq":std::fs::read_to_string(s.root.join(".rivet/seq")).unwrap_or_else(|_| "0".into()),
        "pending":pending, "receipts":std::fs::read_to_string(s.root.join(".rivet/commits.jsonl")).unwrap_or_default(),
    })
}

fn refused(
    s: &Store,
    before: Value,
    answer: (i32, &Value),
    expect: &Value,
    part: &str,
    paths: &[&str],
) {
    let (code, out) = answer;
    assert_eq!(code, 2, "{out}");
    assert_eq!(out["error"]["code"], "guard_conflict", "{out}");
    assert_eq!(out["error"]["path"], paths[0]);
    assert!(
        out["error"]["message"].as_str().unwrap().contains(part),
        "{out}"
    );
    assert_eq!(&out["expect"], expect);
    assert_eq!(
        snapshot(s, paths),
        before,
        "early refusal wrote state: {out}"
    );
}

fn other_commit(s: &Store) {
    std::fs::write(s.root.join("notes.txt"), "another client's commit\n").unwrap();
    s.git(&["commit", "-qm", "other path", "--", "notes.txt"]);
}

#[test]
fn guarded_adopt_revision_refusals_before_and_inside_lock_preserve_every_byte() {
    for locked in [false, true] {
        let s = Store::new(&format!("guard-revision-{locked}"), "refuse");
        let expect = certificate(&s, WIDGETS);
        let pause = Pause::new(&s.root, "locked");
        let child = locked
            .then(|| pause.wait_for(s.spawn("adopt", &request(WIDGETS, &expect), &[pause.env()])));
        add_by_hand(&s);
        let before = snapshot(&s, &[WIDGETS, DOCS]);
        let (code, out) = if let Some(child) = child {
            pause.release();
            finish(child)
        } else {
            s.run("adopt", request(WIDGETS, &expect), &[])
        };
        refused(
            &s,
            before,
            (code, &out),
            &expect,
            "revision",
            &[WIDGETS, DOCS],
        );
    }
}

#[test]
fn guarded_adopt_head_refusals_precede_bookkeeping_and_clean_noop() {
    for (locked, dirty) in [(false, true), (true, true), (false, false)] {
        let s = Store::new(&format!("guard-head-{locked}-{dirty}"), "refuse");
        if dirty {
            edit_outside(&s);
        }
        let expect = certificate(&s, WIDGETS);
        let pause = Pause::new(&s.root, "locked");
        let child = locked
            .then(|| pause.wait_for(s.spawn("adopt", &request(WIDGETS, &expect), &[pause.env()])));
        other_commit(&s);
        let before = snapshot(&s, &[WIDGETS, DOCS]);
        let (code, out) = if let Some(child) = child {
            pause.release();
            finish(child)
        } else {
            s.run("adopt", request(WIDGETS, &expect), &[])
        };
        refused(&s, before, (code, &out), &expect, "HEAD", &[WIDGETS, DOCS]);
    }
}

#[test]
fn guarded_adopt_mode_refusal_precedes_writes() {
    let s = Store::new("guard-mode", "refuse");
    s.git(&["config", "core.fileMode", "true"]);
    let expect = certificate(&s, WIDGETS);
    std::fs::set_permissions(s.root.join(WIDGETS), std::fs::Permissions::from_mode(0o755)).unwrap();
    let before = snapshot(&s, &[WIDGETS, DOCS]);
    let (code, out) = s.run("adopt", request(WIDGETS, &expect), &[]);
    refused(&s, before, (code, &out), &expect, "mode", &[WIDGETS, DOCS]);
}

#[test]
fn guarded_adopt_late_head_and_index_refusals_retire_only_not_applied() {
    for index_only in [false, true] {
        let s = Store::new(&format!("guard-late-{index_only}"), "refuse");
        edit_outside(&s);
        let expect = certificate(&s, WIDGETS);
        let initial = snapshot(&s, &[WIDGETS, DOCS]);
        let pause = Pause::new(&s.root, "pending_written");
        let child = pause.wait_for(s.spawn("adopt", &request(WIDGETS, &expect), &[pause.env()]));
        if index_only {
            let blob = s.git(&["hash-object", "-w", "notes.txt"]);
            s.git(&[
                "update-index",
                "--cacheinfo",
                &format!("100644,{},{}", blob.trim(), WIDGETS),
            ]);
        } else {
            other_commit(&s);
        }
        let before = snapshot(&s, &[WIDGETS, DOCS]);
        let record = s.record();
        assert_eq!(record["expect_head"], expect["head"]);
        assert_eq!(record["base_head"], expect["head"]);
        pause.release();
        let (code, out) = finish(child);
        assert_eq!(code, 2, "{out}");
        assert_eq!(out["error"]["code"], "guard_conflict", "{out}");
        assert_eq!(out["expect"], expect);
        assert!(
            out["error"]["message"]
                .as_str()
                .unwrap()
                .contains("nothing applied")
        );
        let after = snapshot(&s, &[WIDGETS, DOCS]);
        for field in ["files", "head", "index", "seq"] {
            assert_eq!(after[field], before[field], "{field}");
        }
        assert!(s.records().is_empty());
        assert_eq!(after["pending"], json!([]));
        let seq = |v: &Value| v["seq"].as_str().unwrap().trim().parse::<u64>().unwrap();
        assert_eq!(seq(&after), seq(&initial) + 1);
        let events = s.events(record["op"].as_str().unwrap());
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["outcome"], "not_applied");
        assert_eq!(events[0]["applied"], false);
        assert_eq!(
            s.receipts().len(),
            initial["receipts"].as_str().unwrap().lines().count() + 1
        );
    }
}

#[test]
fn guarded_adopt_refuses_stamps_from_binding_changes_after_certification() {
    for locked in [false, true] {
        let s = Store::new(&format!("guard-stamp-config-{locked}"), "refuse");
        // Notes is outside the Action binding, but its heading is already stamped.
        let source = read(&s.root, WIDGETS).replace(
            "\n## Log",
            "\n- [ ] **Deferred work.** Leave this unbound action unstamped.\n\n## Log",
        );
        std::fs::write(s.root.join(WIDGETS), &source).unwrap();
        edit_outside(&s);
        let source = read(&s.root, WIDGETS);
        let ids = s.items();
        let keep = item_guard(&s.root, WIDGETS, &ids[1]);
        let expect = certificate(&s, WIDGETS);

        let pause = Pause::new(&s.root, "locked");
        let child = locked
            .then(|| pause.wait_for(s.spawn("adopt", &request(WIDGETS, &expect), &[pause.env()])));
        let config_path = s.root.join(".rivet/project.json");
        let mut config: Value =
            serde_json::from_slice(&std::fs::read(&config_path).unwrap()).unwrap();
        let mut notes = config["collections"]["actions"].clone();
        notes["section"] = json!("Notes");
        config["collections"]["notes"] = notes;
        std::fs::write(&config_path, config.to_string()).unwrap();
        let paths = [WIDGETS, DOCS, ".rivet/project.json"];
        let before = snapshot(&s, &paths);
        let index = std::fs::read(s.root.join(".git/index")).unwrap();
        let (code, out) = if let Some(child) = child {
            pause.release();
            finish(child)
        } else {
            s.run("adopt", request(WIDGETS, &expect), &[])
        };
        refused(&s, before, (code, &out), &expect, "stamp", &paths);
        assert_eq!(std::fs::read(s.root.join(".git/index")).unwrap(), index);
        assert_eq!(item_guard(&s.root, WIDGETS, &ids[1]), keep);

        // Restoring the binding allows exactly the same certificate and bytes.
        config["collections"]
            .as_object_mut()
            .unwrap()
            .remove("notes");
        std::fs::write(&config_path, config.to_string()).unwrap();
        let (code, out) = s.run("adopt", request(WIDGETS, &expect), &[]);
        assert_eq!(code, 0, "{out}");
        assert_eq!(out["commit"]["status"], "committed", "{out}");
        assert_eq!(out["expect"], expect);
        assert_eq!(
            s.git(&["rev-parse", "HEAD^"]).trim(),
            expect["head"].as_str().unwrap()
        );
        assert_eq!(read(&s.root, WIDGETS), source);
        assert_eq!(s.git(&["show", &format!("HEAD:{WIDGETS}")]), source);
        assert_eq!(item_guard(&s.root, WIDGETS, &ids[1]), keep);
        assert!(s.records().is_empty());
    }
}

#[test]
fn guarded_adopt_requires_stamped_bytes_while_plain_adopt_can_stamp() {
    for unstamped in [false, true] {
        let s = Store::new(&format!("guard-success-{unstamped}"), "refuse");
        let twin = Store::new(&format!("guard-twin-{unstamped}"), "refuse");
        let ids = s.items();
        std::fs::write(twin.root.join(WIDGETS), read(&s.root, WIDGETS)).unwrap();
        twin.git(&["add", WIDGETS]);
        twin.git(&["commit", "-qm", "same baseline", "--allow-empty"]);
        edit_outside(&s);
        if unstamped {
            add_by_hand(&s);
        }
        let source = read(&s.root, WIDGETS);
        std::fs::write(twin.root.join(WIDGETS), &source).unwrap();
        let keep = item_guard(&s.root, WIDGETS, &ids[1]);
        let other = read(&s.root, DOCS);
        let expect = certificate(&s, WIDGETS);
        let seed = [("RIVET_TEST_ID_SEED", "checked-adopt".into())];
        let before = snapshot(&s, &[WIDGETS, DOCS]);
        let (code, out) = s.run("adopt", request(WIDGETS, &expect), &seed);
        if unstamped {
            refused(&s, before, (code, &out), &expect, "stamp", &[WIDGETS, DOCS]);
        } else {
            assert_eq!(code, 0, "{out}");
            assert_eq!(out["commit"]["status"], "committed", "{out}");
            assert_eq!(out["expect"], expect);
            assert_eq!(
                s.git(&["rev-parse", "HEAD^"]).trim(),
                expect["head"].as_str().unwrap()
            );
            assert_eq!(s.git(&["show", &format!("HEAD:{WIDGETS}")]), source);
        }
        assert_eq!(read(&s.root, WIDGETS), source);
        assert_eq!(read(&s.root, DOCS), other);
        assert_eq!(item_guard(&s.root, WIDGETS, &ids[1]), keep);
        assert!(s.records().is_empty());

        let (code, plain) = twin.run("adopt", json!({"path":WIDGETS}), &seed);
        assert_eq!(code, 0, "{plain}");
        assert_eq!(plain["commit"]["status"], "committed", "{plain}");
        assert!(plain.get("expect").is_none());
        let candidate = read(&twin.root, WIDGETS);
        assert_eq!(candidate != source, unstamped);
        assert_eq!(twin.git(&["show", &format!("HEAD:{WIDGETS}")]), candidate);
        assert_eq!(item_guard(&twin.root, WIDGETS, &ids[1]), keep);
        assert!(ids.iter().all(|id| twin.items().contains(id)));
        assert!(twin.records().is_empty());
    }
}

#[test]
fn guarded_adopt_refuses_recoverable_pending_without_settling() {
    for guarded in [false, true] {
        let s = Store::new(&format!("guard-pending-{guarded}"), "refuse");
        let req = s.set_request(&s.items()[0], "[x] **Ship.** Ready.");
        s.kill_at("applied", "set", &req, &[]);
        let expect = certificate(&s, WIDGETS);
        let before = snapshot(&s, &[WIDGETS, DOCS]);
        let req = if guarded {
            request(WIDGETS, &expect)
        } else {
            json!({"path":WIDGETS})
        };
        let (code, out) = s.run("adopt", req, &[]);
        if guarded {
            assert_eq!(code, 2, "{out}");
            assert_eq!(out["error"]["code"], "pending_unresolved");
            assert_eq!(out["error"]["path"], WIDGETS);
            assert!(
                out["error"]["message"]
                    .as_str()
                    .unwrap()
                    .contains("guarded adopt")
            );
            assert_eq!(out["expect"], expect);
            assert_eq!(snapshot(&s, &[WIDGETS, DOCS]), before);
        } else {
            assert_eq!(code, 0, "{out}");
            assert_eq!(out["status"], "unchanged", "{out}");
            assert!(s.records().is_empty() && s.clean());
        }
    }
}

#[test]
fn guarded_adopt_refuses_pending_pair_on_target_or_unrelated_fence() {
    for path in ["pair-a.md", WIDGETS] {
        for guarded in [false, true] {
            let s = pair::fixture(&format!("guard-pair-{}-{guarded}", path.replace('/', "-")));
            s.kill_at("move_destination_replaced", "__test-pair", &json!({}), &[]);
            let expect = certificate(&s, path);
            let paths = [path, "pair-a.md", "pair-b.md", WIDGETS, DOCS];
            let before = snapshot(&s, &paths);
            let fence = std::fs::read(s.root.join(".git/index.lock")).unwrap();
            let req = if guarded {
                request(path, &expect)
            } else {
                json!({"path":path})
            };
            let (code, out) = s.run("adopt", req, &[]);
            if guarded {
                assert_eq!(code, 2, "{out}");
                assert_eq!(out["error"]["code"], "pending_unresolved", "{out}");
                assert_eq!(snapshot(&s, &paths), before);
                assert_eq!(
                    std::fs::read(s.root.join(".git/index.lock")).unwrap(),
                    fence
                );
                assert_eq!(out["expect"], expect);
            } else {
                assert_eq!(code, 0, "{out}");
                assert!(s.records().is_empty());
                assert!(!s.root.join(".git/index.lock").exists());
                assert!(read(&s.root, "pair-a.md").starts_with("# Source\n"));
                assert!(!read(&s.root, "pair-a.md").contains("pairUnit"));
                assert!(read(&s.root, "pair-b.md").contains("stay:pairUnit"));
            }
        }
    }
}

#[test]
fn guarded_adopt_failed_commit_recovers_only_on_certified_head() {
    for moved in [false, true] {
        let s = Store::new(&format!("guard-recover-{moved}"), "refuse");
        edit_outside(&s);
        let expect = certificate(&s, WIDGETS);
        let slow = s.script(
            "slow-guard",
            "case \"$*\" in *\" update-ref --stdin \"*) sleep 30;; esac\nexec git \"$@\"",
        );
        let (code, out) = s.run(
            "adopt",
            request(WIDGETS, &expect),
            &[slow, ("RIVET_TEST_GIT_DEADLINE_MS", "300".into())],
        );
        assert_eq!(code, 0, "{out}");
        assert_eq!(out["status"], "applied");
        assert_eq!(out["commit"]["status"], "failed", "{out}");
        assert_eq!(out["commit"]["reason"], "deadline");
        assert_eq!(s.record()["state"], "applied");
        let op = s.record()["op"].as_str().unwrap().to_owned();
        if moved {
            other_commit(&s);
        }
        let before = snapshot(&s, &[WIDGETS, DOCS]);
        let recovery = s.recover(json!({}), &[]);
        if moved {
            assert_guarded_repair(&s, &expect, &op, &recovery, &before);
            // Later plain settlement uses the same guard and cannot bypass it.
            let (code, out) = s.run("adopt", json!({"path":WIDGETS}), &[]);
            assert_eq!(code, 2, "{out}");
            assert_eq!(out["error"]["code"], "pending_unresolved");
            assert_eq!(s.record()["base_head"], expect["head"]);
            assert!(s.carrying(&op).is_empty());
        } else {
            assert_eq!(one_result(&recovery)["result"], "committed", "{recovery}");
            assert_eq!(
                s.git(&["rev-parse", "HEAD^"]).trim(),
                expect["head"].as_str().unwrap()
            );
            assert_eq!(s.carrying(&op).len(), 1);
            assert!(s.records().is_empty());
        }
    }
}

fn assert_guarded_repair(s: &Store, expect: &Value, op: &str, out: &Value, before: &Value) {
    assert_eq!(one_result(out)["result"], "repair_required", "{out}");
    assert_eq!(one_result(out)["reason"], "guard_head_moved", "{out}");
    let record = s.record();
    assert_eq!(record["state"], "repair_required");
    assert_eq!(record["repair"]["reason"], "guard_head_moved");
    assert_eq!(record["base_head"], expect["head"]);
    assert_eq!(record["expect_head"], expect["head"]);
    assert!(s.carrying(op).is_empty());
    let after = snapshot(s, &[WIDGETS, DOCS]);
    for field in ["files", "index", "head", "seq"] {
        assert_eq!(after[field], before[field], "{field}");
    }
    let complete = |v: &Value| {
        let text = v["receipts"].as_str().unwrap();
        text[..text.rfind('\n').map_or(0, |i| i + 1)].to_owned()
    };
    assert_eq!(complete(&after), complete(before));
}

#[test]
fn guarded_adopt_late_refusal_with_failed_receipt_cannot_rebind() {
    let s = Store::new("guard-refusal-receipt", "refuse");
    edit_outside(&s); // Already stamped: source == candidate even before rename.
    let expect = certificate(&s, WIDGETS);
    let pause = Pause::new(&s.root, "pending_written");
    let child = pause.wait_for(s.spawn(
        "adopt",
        &request(WIDGETS, &expect),
        &[pause.env(), ("RIVET_TEST_FAULT", "receipt_append".into())],
    ));
    other_commit(&s);
    let before = snapshot(&s, &[WIDGETS, DOCS]);
    let op = s.record()["op"].as_str().unwrap().to_owned();
    pause.release();
    let (code, out) = finish(child);
    assert_eq!(code, 2, "{out}");
    assert_eq!(out["error"]["code"], "guard_conflict");
    assert_eq!(out["expect"], expect);
    assert_eq!(s.record()["state"], "prepared");
    let after = snapshot(&s, &[WIDGETS, DOCS]);
    for field in ["files", "head", "index", "seq", "pending"] {
        assert_eq!(after[field], before[field], "{field}");
    }
    assert_eq!(s.receipts().len(), 0); // The injected append leaves only a partial line.
    let recovery = s.recover(json!({}), &[]);
    assert_guarded_repair(&s, &expect, &op, &recovery, &before);
}

#[test]
fn guarded_adopt_committed_record_after_later_commit_settles_and_repairs_index() {
    for repair in [false, true] {
        let s = Store::new(&format!("guard-history-{repair}"), "refuse");
        edit_outside(&s);
        let expect = certificate(&s, WIDGETS);
        s.kill_at("committed", "adopt", &request(WIDGETS, &expect), &[]);
        let record = s.record();
        let candidate = read(&s.root, WIDGETS);
        let op = record["op"].as_str().unwrap();
        let committed = s.head();
        assert_eq!(
            s.git(&["rev-parse", "HEAD^"]).trim(),
            expect["head"].as_str().unwrap()
        );
        other_commit(&s); // Commit before staging the base entry.
        if repair {
            s.git(&[
                "update-index",
                "--cacheinfo",
                &format!(
                    "{},{},{}",
                    record["base"]["mode"].as_str().unwrap(),
                    record["base"]["oid"].as_str().unwrap(),
                    WIDGETS
                ),
            ]);
        }
        let head = s.head();
        let recovery = s.recover(json!({}), &[]);
        assert_eq!(one_result(&recovery)["result"], "committed", "{recovery}");
        assert_eq!(s.head(), head);
        assert_eq!(s.carrying(op), [committed]);
        assert_eq!(s.git(&["show", &format!("HEAD:{WIDGETS}")]), candidate);
        assert_eq!(s.git(&["show", &format!("HEAD^:{WIDGETS}")]), candidate);
        assert_eq!(s.git(&["show", &format!(":{WIDGETS}")]), candidate);
        assert_eq!(read(&s.root, WIDGETS), candidate);
        assert!(s.records().is_empty());
    }
}

#[test]
fn guarded_adopt_malformed_certificates_fail_before_opening_a_project() {
    let root = std::env::temp_dir().join(format!("rivet-guard-no-project-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    let valid =
        json!({"revision":format!("rivet:v1:document:{}", "a".repeat(32)), "head":"b".repeat(40)});
    let mut malformed = vec![
        Value::Null,
        json!(7),
        json!({"revision":valid["revision"]}),
        json!({"head":valid["head"]}),
    ];
    for (key, values) in [
        (
            "revision",
            vec![
                Value::Null,
                json!(7),
                json!(format!("rivet:v1:section:{}", "a".repeat(32))),
                json!(format!("rivet:v1:document:{}", "A".repeat(32))),
            ],
        ),
        (
            "head",
            vec![
                Value::Null,
                json!(7),
                json!("abc123"),
                json!("HEAD"),
                json!("refs/heads/main"),
                json!("B".repeat(40)),
            ],
        ),
    ] {
        for value in values {
            let mut bad = valid.clone();
            bad[key] = value;
            malformed.push(bad);
        }
    }
    let mut extra = valid.clone();
    extra["extra"] = json!(true);
    malformed.push(extra);
    for expect in malformed {
        let (code, out) = rivet(&root, "adopt", request(WIDGETS, &expect));
        assert_eq!(code, 1, "{expect}: {out}");
        assert_eq!(out["error"]["code"], "invalid_request", "{out}");
        assert!(!root.join(".rivet").exists());
    }
    for length in [40, 64] {
        let mut expect = valid.clone();
        expect["head"] = json!("b".repeat(length));
        let (code, out) = rivet(&root, "adopt", request(WIDGETS, &expect));
        assert_eq!(code, 2, "{out}"); // Parsed, then project open failed.
        assert!(!root.join(".rivet").exists());
    }
}
