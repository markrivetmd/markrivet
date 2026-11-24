//! One test per pair recovery and pending outcome: each
//! asserts its reason, that recovery wrote nothing it must not, and the fence.
use super::*;

/// Documents, HEAD and the real index, which a repair outcome leaves alone.
fn untouched(s: &Store) -> (String, String, String, Vec<u8>) {
    (
        read(&s.root, A),
        read(&s.root, B),
        s.head(),
        std::fs::read(s.root.join(".git/index")).unwrap(),
    )
}

/// Recover once and return the single record's report, still unresolved.
fn repaired(s: &Store, extra: &[Var]) -> Value {
    let out = s.recover(json!({}), extra);
    assert_eq!(out["unresolved"], 1, "{out}");
    out["records"][0].clone()
}

fn crashed(name: &str, point: &'static str) -> (Store, String) {
    let s = fixture(name);
    s.kill_at(point, "__test-pair", &json!({}), &[]);
    let op = s.record()["op"].as_str().unwrap().to_string();
    (s, op)
}

/// A commit on top of HEAD whose tree sets `path` to `bytes`, published with
/// `update-ref`, which needs no index lock.
fn hand_commit(s: &Store, path: &str, bytes: &str, message: &str) -> String {
    let blob = s.git_stdin(&["hash-object", "-w", "--stdin"], bytes);
    let index = s.root.join(".git/hand-index");
    let env = [("GIT_INDEX_FILE", index.display().to_string())];
    s.git_env(&["read-tree", "HEAD"], &env);
    let entry = format!("100644,{},{path}", blob.trim());
    s.git_env(&["update-index", "--cacheinfo", &entry], &env);
    let tree = s.git_env(&["write-tree"], &env);
    std::fs::remove_file(&index).unwrap();
    let sha = s.git(&["commit-tree", tree.trim(), "-p", &s.head(), "-m", message]);
    s.git(&["update-ref", "HEAD", sha.trim()]);
    sha.trim().to_string()
}

/// HEAD advanced by one commit that changes no path.
fn empty_commit(s: &Store) {
    let tree = s.git(&["rev-parse", "HEAD^{tree}"]);
    let sha = s.git(&[
        "commit-tree",
        tree.trim(),
        "-p",
        &s.head(),
        "-m",
        "unrelated",
    ]);
    s.git(&["update-ref", "HEAD", sha.trim()]);
}

#[test]
fn pair_damaged_move_receipt_requires_repair() {
    let (s, op) = crashed("pair-out-receipt", "move_destination_replaced");
    let path = s.root.join(format!(".rivet/pending/{op}.json"));
    let original = std::fs::read(&path).unwrap();
    let mut record: Value = serde_json::from_slice(&original).unwrap();
    record["move_receipt"]["destination"]["candidate_sha256"] = json!("0".repeat(64));
    std::fs::write(&path, serde_json::to_vec_pretty(&record).unwrap()).unwrap();
    let before = untouched(&s);
    let out = repaired(&s, &[]);
    assert_eq!(out["reason"], "move_receipt_mismatch", "{out}");
    assert_eq!(untouched(&s), before);
    fence(&s);
    std::fs::write(&path, original).unwrap();
    settled(&s, true);
}

#[test]
fn pair_ref_locks_and_unlistable_refs_require_repair_before_any_git() {
    for case in ["head_lock", "ref_symlink"] {
        let (s, _) = crashed(&format!("pair-out-{case}"), "move_destination_replaced");
        let made = if case == "head_lock" {
            let lock = s.root.join(".git/HEAD.lock");
            std::fs::write(&lock, "").unwrap();
            lock
        } else {
            let link = s.root.join(".git/refs/heads/elsewhere");
            std::os::unix::fs::symlink("main", &link).unwrap();
            link
        };
        let before = untouched(&s);
        let out = repaired(&s, &[]);
        assert_eq!(out["reason"], "stale_git_lock", "{case}: {out}");
        if case == "head_lock" {
            assert!(
                out["detail"].as_str().unwrap().contains("ref lock"),
                "{out}"
            );
        }
        assert_eq!(untouched(&s), before, "{case}");
        fence(&s);
        std::fs::remove_file(made).unwrap();
        settled(&s, true);
    }
}

#[test]
fn pair_unborn_or_switched_head_requires_repair() {
    for case in ["unborn", "branch"] {
        let (s, _) = crashed(&format!("pair-out-{case}"), "move_destination_replaced");
        let head = s.head();
        if case == "unborn" {
            s.git(&["update-ref", "-d", "refs/heads/main"]);
        } else {
            s.git(&["update-ref", "refs/heads/other", &head]);
            s.git(&["symbolic-ref", "HEAD", "refs/heads/other"]);
        }
        let files = (read(&s.root, A), read(&s.root, B));
        let out = repaired(&s, &[]);
        let reason = if case == "unborn" {
            "git_error"
        } else {
            "branch_mismatch"
        };
        assert_eq!(out["reason"], reason, "{out}");
        assert_eq!((read(&s.root, A), read(&s.root, B)), files);
        fence(&s);
        if case == "unborn" {
            s.git(&["update-ref", "refs/heads/main", &head]);
        } else {
            s.git(&["symbolic-ref", "HEAD", "refs/heads/main"]);
        }
        settled(&s, true);
    }
}

#[test]
fn pair_op_carried_by_a_commit_of_another_shape_requires_repair() {
    let (s, op) = crashed("pair-out-shape", "move_destination_replaced");
    let candidate = format!("{READ_B}{UNIT}");
    hand_commit(
        &s,
        B,
        &candidate,
        &format!("only one side\n\nRivet-Op: {op}\n"),
    );
    let before = untouched(&s);
    let out = repaired(&s, &[]);
    assert_eq!(out["reason"], "paths_mismatch", "{out}");
    assert_eq!(untouched(&s), before);
    fence(&s);
}

#[test]
fn pair_published_move_sync_failure_keeps_the_fence() {
    let (s, _) = crashed("pair-out-published-sync", "ref_updated");
    let before = untouched(&s);
    let out = repaired(&s, &[("RIVET_TEST_FAULT", "move_recovery_sync".into())]);
    assert_eq!(out["reason"], "not_durable", "{out}");
    assert_eq!(untouched(&s), before);
    fence(&s);
    settled(&s, true);
}

#[test]
fn pair_published_clean_index_sync_or_release_failure_is_index_changed() {
    // The index was installed (consuming the fence) but its directory sync
    // failed; recovery publishes a replacement, then fails to sync the
    // installed index (`index_directory_sync`) or to release the replacement
    // (`move_fence_sync@2`: the first is the replacement's own creation).
    for fault in ["index_directory_sync", "move_fence_sync@2"] {
        let s = fixture(&format!("pair-out-clean-{fault}"));
        let (code, out) = s.run(
            "__test-pair",
            json!({}),
            &[("RIVET_TEST_FAULT", "index_directory_sync".into())],
        );
        assert_eq!(code, 3, "{out}");
        let out = repaired(&s, &[("RIVET_TEST_FAULT", fault.into())]);
        assert_eq!(out["reason"], "index_changed", "{fault}: {out}");
        let out = s.recover(json!({}), &[]);
        assert_eq!(out["unresolved"], 0, "{out}");
        assert!(!s.root.join(".git/index.lock").exists());
    }
}

#[test]
fn pair_published_move_with_an_unexplained_index_entry_is_left_alone() {
    let (s, _) = crashed("pair-out-unexplained", "ref_updated");
    let saved = std::fs::read(s.root.join(".git/index")).unwrap();
    let other = s.git_stdin(&["hash-object", "-w", "--stdin"], "other\n");
    let forged = s.root.join(".git/forged-index");
    let env = [("GIT_INDEX_FILE", forged.display().to_string())];
    s.git_env(&["read-tree", "HEAD"], &env);
    let entry = format!("100644,{},{A}", other.trim());
    s.git_env(&["update-index", "--cacheinfo", &entry], &env);
    std::fs::rename(&forged, s.root.join(".git/index")).unwrap();
    let before = untouched(&s);
    let out = repaired(&s, &[]);
    assert_eq!(out["reason"], "index_changed", "{out}");
    assert_eq!(untouched(&s), before);
    fence(&s);
    std::fs::write(s.root.join(".git/index"), saved).unwrap();
    settled(&s, true);
}

#[test]
fn pair_published_index_repair_failure_keeps_the_fence() {
    let (s, _) = crashed("pair-out-index-repair", "ref_updated");
    let files = (read(&s.root, A), read(&s.root, B), s.head());
    let out = repaired(&s, &[("RIVET_TEST_FAULT", "index_sync".into())]);
    assert_eq!(out["reason"], "index_changed", "{out}");
    assert_eq!((read(&s.root, A), read(&s.root, B), s.head()), files);
    fence(&s);
    settled(&s, true);
}

#[test]
fn pair_changed_policy_or_configuration_requires_repair() {
    for case in ["commit_policy_changed", "config_changed"] {
        let (s, _) = crashed(&format!("pair-out-{case}"), "move_destination_replaced");
        let path = s.root.join(".rivet/project.json");
        let saved = std::fs::read(&path).unwrap();
        if case == "commit_policy_changed" {
            s.configure(json!({"commit":"off"}));
        } else {
            let mut config: Value = serde_json::from_slice(&saved).unwrap();
            config["collections"]["extra"] = json!({"schema":"tracker-action@1",
                "files":["elsewhere/*.md"],"section":"Action","level":2});
            std::fs::write(&path, serde_json::to_vec(&config).unwrap()).unwrap();
        }
        let before = untouched(&s);
        let out = repaired(&s, &[]);
        assert_eq!(out["reason"], case, "{out}");
        assert_eq!(untouched(&s), before);
        fence(&s);
        std::fs::write(&path, saved).unwrap();
        settled(&s, true);
    }
}

#[test]
fn pair_path_moved_in_git_requires_repair() {
    let (s, _) = crashed("pair-out-target", "move_destination_replaced");
    hand_commit(&s, A, "moved in git\n", "change the source");
    let before = untouched(&s);
    let out = repaired(&s, &[]);
    assert_eq!(out["reason"], "target_changed", "{out}");
    assert_eq!(untouched(&s), before);
    fence(&s);
}

#[test]
fn pair_conversion_attribute_makes_the_pair_ineligible() {
    let (s, _) = crashed("pair-out-ineligible", "move_destination_replaced");
    let attributes = s.root.join(".gitattributes");
    std::fs::write(&attributes, "pair-*.md filter=rot\n").unwrap();
    let before = untouched(&s);
    let out = repaired(&s, &[]);
    assert_eq!(out["reason"], "git_conversion", "{out}");
    assert_eq!(untouched(&s), before);
    fence(&s);
    std::fs::remove_file(attributes).unwrap();
    settled(&s, true);
}

/// Pause a recovery at `move_recovery_preflight`, run `during`, release it.
fn during_preflight(s: &Store, during: impl FnOnce(&Store)) -> Value {
    let pause = Pause::new(&s.root, "move_recovery_preflight");
    let child = pause.wait_for(s.spawn("commit-pending", &json!({}), &[pause.env()]));
    during(s);
    pause.release();
    let (code, out) = finish(child);
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["unresolved"], 1, "{out}");
    out["records"][0].clone()
}

#[test]
fn pair_head_moving_during_recovery_preflight_is_target_changed() {
    let (s, _) = crashed("pair-out-preflight-race", "move_destination_replaced");
    let files = (read(&s.root, A), read(&s.root, B));
    let out = during_preflight(&s, empty_commit);
    assert_eq!(out["reason"], "target_changed", "{out}");
    assert!(
        out["detail"].as_str().unwrap().contains("preflight"),
        "{out}"
    );
    assert_eq!((read(&s.root, A), read(&s.root, B)), files);
    fence(&s);
    settled(&s, true);
}

#[test]
fn pair_outside_edit_during_recovery_is_preserved() {
    // read/read: the closing check finds the edit; read/candidate: the
    // roll-forward finds it before replacing the source.
    for (point, reason) in [
        ("move_pending_written", "worktree_differs"),
        ("move_destination_replaced", "file_changed"),
    ] {
        let (s, _) = crashed(&format!("pair-out-race-{point}"), point);
        let b = read(&s.root, B);
        let out = during_preflight(&s, |s| {
            std::fs::write(s.root.join(A), "outside A\n").unwrap()
        });
        assert_eq!(out["reason"], reason, "{point}: {out}");
        assert_eq!(read(&s.root, A), "outside A\n");
        assert_eq!(read(&s.root, B), b);
        fence(&s);
    }
}

#[test]
fn pair_unapplied_release_failure_keeps_the_record() {
    let (s, _) = crashed("pair-out-release", "move_pending_written");
    let out = repaired(&s, &[("RIVET_TEST_FAULT", "move_fence_sync".into())]);
    assert_eq!(out["reason"], "not_durable", "{out}");
    assert_eq!(
        (read(&s.root, A), read(&s.root, B)),
        (READ_A.into(), READ_B.into())
    );
    let out = s.recover(json!({}), &[]);
    assert_eq!(out["unresolved"], 0, "{out}");
    assert!(!s.root.join(".git/index.lock").exists());
}

#[test]
fn pair_missing_recorded_object_refuses_the_roll_forward() {
    let (s, _) = crashed("pair-out-object", "move_destination_replaced");
    let oid = s.record()["candidate"]["oid"].as_str().unwrap().to_string();
    let object = s
        .root
        .join(format!(".git/objects/{}/{}", &oid[..2], &oid[2..]));
    let saved = std::fs::read(&object).unwrap();
    std::fs::remove_file(&object).unwrap();
    let before = untouched(&s);
    let out = repaired(&s, &[]);
    assert_eq!(out["reason"], "object_mismatch", "{out}");
    assert_eq!(untouched(&s), before);
    fence(&s);
    std::fs::write(&object, saved).unwrap();
    settled(&s, true);
}

#[test]
fn pair_roll_forward_that_is_not_durable_keeps_the_fence() {
    let (s, _) = crashed("pair-out-roll-durable", "move_destination_replaced");
    let out = repaired(&s, &[("RIVET_TEST_FAULT", "dir_sync".into())]);
    assert_eq!(out["reason"], "not_durable", "{out}");
    assert!(
        out["detail"].as_str().unwrap().contains("roll-forward"),
        "{out}"
    );
    // The rename happened; only its durability is unconfirmed.
    assert_eq!(read(&s.root, A), "# Source\n\n");
    fence(&s);
    settled(&s, true);
}

#[test]
fn pair_roll_forward_bookkeeping_failure_is_retried() {
    let (s, _) = crashed("pair-out-roll-publish", "move_destination_replaced");
    let record = s.record();
    let out = repaired(&s, &[("RIVET_TEST_FAULT", "record_publish".into())]);
    assert_eq!(out["reason"], "git_error", "{out}");
    assert_eq!(s.record(), record);
    assert_eq!(read(&s.root, A), "# Source\n\n");
    fence(&s);
    settled(&s, true);
}

#[test]
fn pair_land_pending_branches_keep_the_unit() {
    // (fault, pending reason, source holds its candidate afterwards)
    for (fault, reason, applied) in [
        (
            "move_destination_rename,move_recovery_sync",
            "destination_replace",
            false,
        ),
        ("dir_sync", "not_durable", false),
        ("dir_sync@2", "not_durable", true),
        ("record_publish@3", "bookkeeping", true),
    ] {
        let s = fixture(&format!("pair-out-land-{reason}-{applied}"));
        let (code, out) = s.run(
            "__test-pair",
            json!({}),
            &[("RIVET_TEST_FAULT", fault.into())],
        );
        assert_eq!(code, 3, "{fault}: {out}");
        assert_eq!(out["commit"]["reason"], reason, "{fault}: {out}");
        let source = if applied { "# Source\n\n" } else { READ_A };
        assert_eq!(read(&s.root, A), source, "{fault}");
        settled(&s, reason != "destination_replace");
    }
}

#[test]
fn pair_land_uncertain_rename_on_either_side_is_pending() {
    // An outside save lands between a side's last check and its rename, and
    // the rename then fails: the side's state is unknown.
    for (point, fault, side) in [
        ("rename_ready", "rename", B),
        ("rename_ready@2", "rename@2", A),
    ] {
        let s = fixture(&format!("pair-out-uncertain-{side}"));
        let pause = Pause::new(&s.root, point);
        let vars = [pause.env(), ("RIVET_TEST_FAULT", fault.into())];
        let child = pause.wait_for(s.spawn("__test-pair", &json!({}), &vars));
        std::fs::write(s.root.join(side), "outside save\n").unwrap();
        pause.release();
        let (code, out) = finish(child);
        assert_eq!(code, 3, "{side}: {out}");
        assert_eq!(out["commit"]["reason"], "commit_uncertain", "{side}: {out}");
        assert_eq!(read(&s.root, side), "outside save\n");
        fence(&s);
        let out = repaired(&s, &[]);
        assert_eq!(out["reason"], "file_changed", "{side}: {out}");
        assert_eq!(read(&s.root, side), "outside save\n");
    }
}

#[test]
fn pair_lock_taken_before_the_fence_is_named_keeps_its_record_and_entry() {
    let s = fixture("pair-out-taken");
    let pause = Pause::new(&s.root, "move_fence_recorded");
    let child = pause.wait_for(s.spawn("__test-pair", &json!({}), &[pause.env()]));
    std::fs::write(s.root.join(".git/index.lock"), "foreign").unwrap();
    pause.release();
    let (code, out) = finish(child);
    assert_eq!(code, 2, "{out}");
    let op = s.record()["op"].as_str().unwrap().to_string();
    assert_eq!(out["pending"], op, "{out}");
    let message = out["error"]["message"].as_str().unwrap();
    assert!(message.contains("index.lock is held"), "{message}");
    assert!(message.contains(&format!("rivet-fence-{op}")), "{message}");
    assert_eq!(
        (read(&s.root, A), read(&s.root, B)),
        (READ_A.into(), READ_B.into())
    );
    assert_eq!(read(&s.root, ".git/index.lock"), "foreign");
    assert!(staging(&s, &op).is_file());
    assert!(s.receipts().is_empty());
    // Once the other writer is gone and the operator removes the entry.
    std::fs::remove_file(s.root.join(".git/index.lock")).unwrap();
    staged_repair(&s, &op);
    std::fs::remove_file(staging(&s, &op)).unwrap();
    let out = s.recover(json!({}), &[]);
    assert_eq!(out["unresolved"], 0, "{out}");
    assert_eq!(out["records"][0]["result"], "not_applied", "{out}");
}

#[test]
fn pair_withdrawal_that_cannot_finish_leaves_its_record_not_a_lock() {
    // Retirement fails: the fence is already released, the record remains.
    // The owned name is replaced: release refuses, the foreign lock survives.
    for case in ["retire", "replaced"] {
        let s = fixture(&format!("pair-out-withdraw-{case}"));
        let pause = Pause::new(&s.root, "move_fenced");
        let mut vars = vec![pause.env()];
        if case == "retire" {
            vars.push(("RIVET_TEST_FAULT", "record_retire".into()));
        }
        let child = pause.wait_for(s.spawn("__test-pair", &json!({}), &vars));
        if case == "replaced" {
            std::fs::rename(s.root.join(".git/index.lock"), s.root.join(".git/kept")).unwrap();
            std::fs::write(s.root.join(".git/index.lock"), "foreign").unwrap();
        }
        empty_commit(&s);
        pause.release();
        let (code, out) = finish(child);
        assert_eq!(code, 2, "{case}: {out}");
        let op = s.record()["op"].as_str().unwrap().to_string();
        assert_eq!(out["pending"], op, "{case}: {out}");
        assert!(s.receipts().is_empty());
        if case == "retire" {
            assert!(!s.root.join(".git/index.lock").exists());
            let out = s.recover(json!({}), &[]);
            assert_eq!(out["unresolved"], 0, "{out}");
            assert_eq!(out["records"][0]["result"], "not_applied", "{out}");
        } else {
            assert_eq!(read(&s.root, ".git/index.lock"), "foreign");
            let out = repaired(&s, &[]);
            assert_eq!(out["reason"], "stale_git_lock", "{out}");
            assert_eq!(read(&s.root, ".git/index.lock"), "foreign");
        }
        assert_eq!(
            (read(&s.root, A), read(&s.root, B)),
            (READ_A.into(), READ_B.into())
        );
    }
}

#[test]
fn pair_first_record_publication_failure_writes_nothing() {
    let s = fixture("pair-out-first-publish");
    let (code, out) = s.run(
        "__test-pair",
        json!({}),
        &[("RIVET_TEST_FAULT", "record_publish".into())],
    );
    assert_eq!(code, 2, "{out}");
    assert!(out.get("pending").is_none(), "{out}");
    assert!(s.records().is_empty());
    assert!(s.receipts().is_empty());
    assert!(!s.root.join(".git/index.lock").exists());
    no_staging(&s);
    assert_eq!(
        (read(&s.root, A), read(&s.root, B)),
        (READ_A.into(), READ_B.into())
    );
}

#[test]
fn pair_replaced_staging_name_is_never_installed_or_removed() {
    let s = fixture("pair-out-staging-swap");
    let pause = Pause::new(&s.root, "move_fence_recorded");
    let child = pause.wait_for(s.spawn("__test-pair", &json!({}), &[pause.env()]));
    let op = s.record()["op"].as_str().unwrap().to_string();
    let kept = intrude(&s, &op);
    pause.release();
    let (code, out) = finish(child);
    assert_eq!(code, 2, "{out}");
    assert_eq!(out["pending"], op, "{out}");
    assert!(!s.root.join(".git/index.lock").exists());
    survived(&s, &op, &kept);
    assert!(s.receipts().is_empty());
    assert_eq!(
        (read(&s.root, A), read(&s.root, B)),
        (READ_A.into(), READ_B.into())
    );
    staged_repair(&s, &op);
    survived(&s, &op, &kept);
}

#[test]
fn pair_rename_error_not_proven_unrenamed_keeps_the_record() {
    let s = fixture("pair-out-rename-error");
    let (code, out) = s.run(
        "__test-pair",
        json!({}),
        &[("RIVET_TEST_FAULT", "move_fence_rename".into())],
    );
    assert_eq!(code, 2, "{out}");
    let record = s.record();
    let op = record["op"].as_str().unwrap().to_string();
    assert_eq!(out["pending"], op, "{out}");
    // The record kept the identity it published; the entry is left in place.
    assert!(record["fence"].is_object(), "{record}");
    assert!(!s.root.join(".git/index.lock").exists());
    assert!(staging(&s, &op).is_file());
    staged_repair(&s, &op);
    std::fs::remove_file(staging(&s, &op)).unwrap();
    let out = s.recover(json!({}), &[]);
    assert_eq!(out["unresolved"], 0, "{out}");
    assert_eq!(out["records"][0]["result"], "not_applied", "{out}");
    assert!(!s.root.join(".git/index.lock").exists());
    assert_eq!(
        (read(&s.root, A), read(&s.root, B)),
        (READ_A.into(), READ_B.into())
    );
}

#[test]
fn pair_recovery_never_removes_a_staging_entry_its_own_or_foreign() {
    for foreign in [false, true] {
        let (s, op) = crashed(
            &format!("pair-out-staging-recovery-{foreign}"),
            "move_fence_recorded",
        );
        let kept = foreign.then(|| intrude(&s, &op));
        staged_repair(&s, &op);
        if let Some(kept) = &kept {
            survived(&s, &op, kept);
        }
        assert_eq!(
            (read(&s.root, A), read(&s.root, B)),
            (READ_A.into(), READ_B.into())
        );
        // The operator removes the entry they checked; recovery then settles.
        std::fs::remove_file(staging(&s, &op)).unwrap();
        let out = s.recover(json!({}), &[]);
        assert_eq!(out["unresolved"], 0, "{out}");
        assert_eq!(out["records"][0]["result"], "not_applied", "{out}");
    }
}

#[test]
fn pair_error_after_naming_keeps_a_fence_its_record_owns() {
    let s = fixture("pair-out-after-naming");
    let (code, out) = s.run(
        "__test-pair",
        json!({}),
        &[("RIVET_TEST_FAULT", "move_fence_renamed".into())],
    );
    assert_eq!(code, 2, "{out}");
    let record = s.record();
    assert_eq!(out["pending"], record["op"], "{out}");
    fence(&s);
    no_staging(&s);
    assert_eq!(
        (read(&s.root, A), read(&s.root, B)),
        (READ_A.into(), READ_B.into())
    );
    let out = s.recover(json!({}), &[]);
    assert_eq!(out["unresolved"], 0, "{out}");
    assert_eq!(out["records"][0]["result"], "not_applied", "{out}");
    assert!(!s.root.join(".git/index.lock").exists());
}

/// Replace the staging entry with another process's file, keeping the
/// original inode alive elsewhere so its number cannot be reused. Returns
/// the kept original's path.
fn intrude(s: &Store, op: &str) -> std::path::PathBuf {
    let kept = s.root.join(format!(".git/kept-{op}"));
    std::fs::rename(staging(s, op), &kept).unwrap();
    std::fs::write(staging(s, op), "intruder bytes\n").unwrap();
    kept
}

/// The replacement's bytes are still at the staging name, and the original
/// inode is still where the intruder put it.
fn survived(s: &Store, op: &str, kept: &std::path::Path) {
    assert_eq!(std::fs::read(staging(s, op)).unwrap(), b"intruder bytes\n");
    assert!(kept.is_file());
}

#[test]
fn pair_publication_failure_after_the_entry_was_replaced_keeps_the_replacement() {
    // The fence identity's publication (the second record publish) pauses,
    // the entry is replaced, then that publication fails.
    let s = fixture("pair-out-publish-replaced");
    let pause = Pause::new(&s.root, "record_publish@2");
    let child = pause.wait_for(s.spawn(
        "__test-pair",
        &json!({}),
        &[pause.env(), ("RIVET_TEST_FAULT", "record_publish@2".into())],
    ));
    let op = s.record()["op"].as_str().unwrap().to_string();
    let kept = intrude(&s, &op);
    pause.release();
    let (code, out) = finish(child);
    assert_eq!(code, 2, "{out}");
    assert_eq!(out["pending"], op, "{out}");
    survived(&s, &op, &kept);
    assert!(!s.root.join(".git/index.lock").exists());
    assert!(
        s.record()["fence"].is_null(),
        "the failed publication named nothing"
    );
    assert!(s.receipts().is_empty());
    staged_repair(&s, &op);
    survived(&s, &op, &kept);
    std::fs::remove_file(staging(&s, &op)).unwrap();
    let out = s.recover(json!({}), &[]);
    assert_eq!(out["unresolved"], 0, "{out}");
    assert!(kept.is_file());
}

#[test]
fn pair_entry_replaced_where_cleanup_once_ran_keeps_the_replacement() {
    // The rename is refused (another writer holds index.lock), and the entry
    // is replaced at move_fence_refused, the gap between the old ownership
    // check and its unlink.
    let s = fixture("pair-out-refused-replaced");
    let recorded = Pause::new(&s.root, "move_fence_recorded");
    let refused = Pause {
        dir: recorded.dir.clone(),
        point: "move_fence_refused",
    };
    let env = (
        "RIVET_TEST_PAUSE",
        format!(
            "move_fence_recorded,move_fence_refused:{}",
            recorded.dir.display()
        ),
    );
    let child = recorded.wait_for(s.spawn("__test-pair", &json!({}), &[env]));
    std::fs::write(s.root.join(".git/index.lock"), "foreign lock").unwrap();
    recorded.release();
    let child = refused.wait_for(child);
    let op = s.record()["op"].as_str().unwrap().to_string();
    let kept = intrude(&s, &op);
    refused.release();
    let (code, out) = finish(child);
    assert_eq!(code, 2, "{out}");
    assert_eq!(out["pending"], op, "{out}");
    survived(&s, &op, &kept);
    assert_eq!(read(&s.root, ".git/index.lock"), "foreign lock");
    assert!(s.receipts().is_empty());
    assert_eq!(
        (read(&s.root, A), read(&s.root, B)),
        (READ_A.into(), READ_B.into())
    );
}

#[test]
fn pair_staging_entry_is_reported_before_a_foreign_lock_on_recovery_and_abandon() {
    let s = fixture("pair-out-staging-and-lock");
    let pause = Pause::new(&s.root, "move_fence_recorded");
    let child = pause.wait_for(s.spawn("__test-pair", &json!({}), &[pause.env()]));
    std::fs::write(s.root.join(".git/index.lock"), "foreign lock").unwrap();
    pause.release();
    let (code, out) = finish(child);
    assert_eq!(code, 2, "{out}");
    let op = s.record()["op"].as_str().unwrap().to_string();
    let entry = std::fs::read(staging(&s, &op)).unwrap();
    let kept = |s: &Store| {
        assert_eq!(read(&s.root, ".git/index.lock"), "foreign lock");
        assert_eq!(std::fs::read(staging(s, &op)).unwrap(), entry);
        assert_eq!(
            (read(&s.root, A), read(&s.root, B)),
            (READ_A.into(), READ_B.into())
        );
    };
    // Recovery names the entry and says index.lock is not this record's.
    let out = s.recover(json!({}), &[]);
    assert_eq!(out["unresolved"], 1, "{out}");
    assert_eq!(out["records"][0]["reason"], "stale_git_lock", "{out}");
    let detail = out["records"][0]["detail"].as_str().unwrap();
    assert!(detail.contains(&format!("rivet-fence-{op}")), "{detail}");
    assert!(detail.contains("index.lock is held too"), "{detail}");
    kept(&s);
    // Abandonment gives the same operator step and changes nothing.
    let records = s.records();
    let (code, out) = s.run(
        "commit-pending",
        json!({"abandon":op, "reason":"both names"}),
        &[],
    );
    assert_eq!(code, 2, "{out}");
    assert_eq!(out["error"]["code"], "pending_unresolved", "{out}");
    let message = out["error"]["message"].as_str().unwrap();
    assert!(message.contains(&format!("rivet-fence-{op}")), "{message}");
    kept(&s);
    assert_eq!(s.records(), records);
    assert!(s.receipts().is_empty());
}

#[test]
fn pair_preparation_failure_after_the_entry_exists_keeps_it_and_the_record() {
    // The entry is created, then its identity read (`move_fence_birth@2`, after
    // the directory pre-check) or its own sync fails, before anything is
    // published. Each runs with Rivet's entry and with one replaced at the
    // hook just before the failing step.
    let cases = [
        ("move_fence_created", "move_fence_birth@2"),
        ("move_fence_identified", "move_fence_staging_sync"),
    ];
    for (point, fault) in cases {
        for replaced in [false, true] {
            let case = format!("{point}-{replaced}");
            let s = fixture(&format!("pair-out-prepare-{case}"));
            let before = untouched(&s);
            let pause = Pause::new(&s.root, point);
            let vars = [pause.env(), ("RIVET_TEST_FAULT", fault.into())];
            let child = pause.wait_for(s.spawn("__test-pair", &json!({}), &vars));
            let op = s.record()["op"].as_str().unwrap().to_string();
            let kept = replaced.then(|| intrude(&s, &op));
            pause.release();
            let (code, out) = finish(child);
            assert_eq!(code, 2, "{case}: {out}");
            assert_eq!(out["pending"], op, "{case}: {out}");
            let record = s.record();
            assert_eq!(record["op"], op, "{case}: {record}");
            assert!(record["fence"].is_null(), "{case}: nothing was published");
            let message = out["error"]["message"].as_str().unwrap();
            assert!(
                message.contains(&format!("injected {fault} fault")),
                "{message}"
            );
            assert!(message.contains(&format!("rivet-fence-{op}")), "{message}");
            assert!(message.contains("writers stopped"), "{message}");
            assert!(message.contains("commit-pending"), "{message}");
            match &kept {
                Some(kept) => survived(&s, &op, kept),
                None => assert_eq!(std::fs::read(staging(&s, &op)).unwrap(), b""),
            }
            assert_eq!(untouched(&s), before, "{case}");
            assert!(!s.root.join(".git/index.lock").exists());
            assert!(s.receipts().is_empty(), "{case}");
            // Recovery reports the entry and leaves it; the operator removes
            // it, and recovery then settles.
            staged_repair(&s, &op);
            std::fs::remove_file(staging(&s, &op)).unwrap();
            let out = s.recover(json!({}), &[]);
            assert_eq!(out["unresolved"], 0, "{case}: {out}");
            assert_eq!(out["records"][0]["result"], "not_applied", "{case}: {out}");
            assert_eq!(untouched(&s), before, "{case}");
            if let Some(kept) = &kept {
                assert!(kept.is_file());
            }
        }
    }
}

#[test]
fn pair_ambiguous_rename_error_keeps_a_replacement_made_where_cleanup_once_ran() {
    // The rename fails with an error that does not prove it failed (`Named`);
    // the entry is replaced at move_fence_refused, where the old cleanup ran.
    let s = fixture("pair-out-named-replaced");
    let before = untouched(&s);
    let pause = Pause::new(&s.root, "move_fence_refused");
    let vars = [
        pause.env(),
        ("RIVET_TEST_FAULT", "move_fence_rename".into()),
    ];
    let child = pause.wait_for(s.spawn("__test-pair", &json!({}), &vars));
    let op = s.record()["op"].as_str().unwrap().to_string();
    let kept = intrude(&s, &op);
    pause.release();
    let (code, out) = finish(child);
    assert_eq!(code, 2, "{out}");
    assert_eq!(out["pending"], op, "{out}");
    assert!(
        s.record()["fence"].is_object(),
        "the record keeps its identity"
    );
    survived(&s, &op, &kept);
    assert!(!s.root.join(".git/index.lock").exists());
    assert!(s.receipts().is_empty());
    assert_eq!(untouched(&s), before);
    staged_repair(&s, &op);
    survived(&s, &op, &kept);
    std::fs::remove_file(staging(&s, &op)).unwrap();
    let out = s.recover(json!({}), &[]);
    assert_eq!(out["unresolved"], 0, "{out}");
    assert_eq!(out["records"][0]["result"], "not_applied", "{out}");
    assert!(kept.is_file());
}

#[test]
fn pair_without_no_replace_rename_stays_pending_with_its_entry_until_the_rename_works() {
    // The backstop (plan section 10, P6): the no-replace probe passes, but the
    // fence's own rename is then refused as unsupported (a filesystem that
    // answers differently for the fence file). The move has created its entry
    // and published the identity, so it ends pending and the entry stays.
    let s = fixture("pair-out-noreplace");
    let before = untouched(&s);
    let noreplace = [("RIVET_TEST_FAULT", "move_fence_noreplace".to_string())];
    let (code, out) = s.run("__test-pair", json!({}), &noreplace);
    assert_eq!(code, 2, "{out}");
    let record = s.record();
    let op = record["op"].as_str().unwrap().to_string();
    assert_eq!(out["pending"], op, "{out}");
    assert!(record["fence"].is_object(), "{record}");
    let message = out["error"]["message"].as_str().unwrap();
    assert!(
        message.contains("no-replace rename is unsupported"),
        "{message}"
    );
    assert!(message.contains(&format!("rivet-fence-{op}")), "{message}");
    assert!(message.contains("writers stopped"), "{message}");
    assert_eq!(std::fs::read(staging(&s, &op)).unwrap(), b"");
    assert!(!s.root.join(".git/index.lock").exists());
    assert!(s.receipts().is_empty());
    assert_eq!(untouched(&s), before);
    // While the entry is present, recovery reports it and takes no fence.
    staged_repair(&s, &op);
    // The operator removes it; on the same filesystem the next recovery needs
    // the same rename, so it leaves a new entry and refuses again.
    std::fs::remove_file(staging(&s, &op)).unwrap();
    let (code, out) = s.run("commit-pending", json!({}), &noreplace);
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["unresolved"], 1, "{out}");
    assert_eq!(out["records"][0]["reason"], "stale_git_lock", "{out}");
    let detail = out["records"][0]["detail"].as_str().unwrap();
    assert!(
        detail.contains("no-replace rename is unsupported"),
        "{detail}"
    );
    assert!(staging(&s, &op).is_file());
    assert_eq!(untouched(&s), before);
    // Abandonment needs the fence too: refused while the entry is present,
    // and again on the same filesystem once it is removed.
    let records = s.records();
    for remove in [false, true] {
        if remove {
            std::fs::remove_file(staging(&s, &op)).unwrap();
        }
        let (code, out) = s.run(
            "commit-pending",
            json!({"abandon": op, "reason": "no rename"}),
            &noreplace,
        );
        assert_eq!(code, 2, "{remove}: {out}");
        assert_eq!(out["error"]["code"], "pending_unresolved", "{out}");
        let message = out["error"]["message"].as_str().unwrap();
        assert!(message.contains(&format!("rivet-fence-{op}")), "{message}");
        assert!(staging(&s, &op).is_file());
        assert_eq!(s.records().len(), 1);
        assert_eq!(s.records()[0]["op"], records[0]["op"]);
        assert!(s.receipts().is_empty());
        assert_eq!(untouched(&s), before);
    }
    // Once a run can rename without replacing, the operator removes the entry
    // and recovery settles.
    std::fs::remove_file(staging(&s, &op)).unwrap();
    let out = s.recover(json!({}), &[]);
    assert_eq!(out["unresolved"], 0, "{out}");
    assert_eq!(out["records"][0]["result"], "not_applied", "{out}");
    assert!(!s.root.join(".git/index.lock").exists());
    no_staging(&s);
    assert_eq!(untouched(&s), before);
}

#[test]
fn pair_waived_abandon_reports_a_staging_entry_another_process_made() {
    // A waived pair, undone by hand to both read bytes, then another process
    // puts its own file at the record's staging name. Rivet made no entry here.
    let s = fixture("pair-out-waived-staging");
    s.kill_at("move_destination_replaced", "__test-pair", &json!({}), &[]);
    let op = s.record()["op"].as_str().unwrap().to_string();
    waive(&s, &op);
    std::fs::write(s.root.join(B), READ_B).unwrap();
    no_staging(&s);
    std::fs::write(staging(&s, &op), "another process\n").unwrap();
    let before = abandon_state(&s);
    let request = json!({"abandon": op, "reason": "undone by hand"});
    let (code, out) = s.run("commit-pending", request.clone(), &[]);
    assert_eq!(code, 2, "{out}");
    assert_eq!(out["error"]["code"], "pending_unresolved", "{out}");
    let message = out["error"]["message"].as_str().unwrap();
    assert!(message.starts_with("stale_git_lock: "), "{message}");
    assert!(message.contains(&format!("rivet-fence-{op}")), "{message}");
    assert!(message.contains("writers stopped"), "{message}");
    // The record is not retired, and the entry and every byte are kept.
    assert_eq!(abandon_state(&s), before);
    assert!(s.record()["fence_waived"].as_bool().unwrap());
    assert_eq!(
        std::fs::read(staging(&s, &op)).unwrap(),
        b"another process\n"
    );
    assert!(s.events(&op).is_empty());
    // The operator removes the entry; abandonment then proceeds.
    std::fs::remove_file(staging(&s, &op)).unwrap();
    let out = s.recover(request, &[]);
    assert_eq!(out["unresolved"], 0, "{out}");
    assert!(s.records().is_empty());
    assert_eq!(s.events(&op).len(), 1, "{:?}", s.receipts());
    assert_eq!(
        s.events(&op)[0]["outcome"],
        "abandoned",
        "{:?}",
        s.receipts()
    );
}

// Plan section 10: no-replace rename is proved before a pair record exists.

/// `.git/rivet-noreplace-<which>`, the persistent probe's two names.
fn probe(s: &Store, which: &str) -> std::path::PathBuf {
    s.root.join(format!(".git/rivet-noreplace-{which}"))
}

/// The probe names present in `.git`, sorted.
fn probes(s: &Store) -> Vec<String> {
    let mut found: Vec<_> = std::fs::read_dir(s.root.join(".git"))
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .filter(|n| n.starts_with("rivet-noreplace-"))
        .collect();
    found.sort();
    found
}

/// Every entry name directly under `.git`.
fn git_entries(s: &Store) -> std::collections::BTreeSet<String> {
    std::fs::read_dir(s.root.join(".git"))
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect()
}

/// A move refused before its record was published: nothing to settle and
/// nothing a writer waits on.
fn refused_unpublished(
    s: &Store,
    code: i32,
    out: &Value,
    before: &(String, String, String, Vec<u8>),
) {
    assert_eq!(code, 2, "{out}");
    assert!(out.get("pending").is_none(), "{out}");
    let pending: Vec<_> = std::fs::read_dir(s.root.join(".rivet/pending"))
        .map(|d| d.map(|e| e.unwrap().file_name()).collect())
        .unwrap_or_default();
    assert!(pending.is_empty(), "{pending:?}");
    no_staging(s);
    assert!(!s.root.join(".git/index.lock").exists());
    assert!(s.receipts().is_empty());
    assert_eq!(&untouched(s), before);
}

/// An obstructed probe's refusal names both probe names and the step.
fn obstructed(out: &Value) {
    assert_eq!(out["error"]["code"], "commit_refused", "{out}");
    let message = out["error"]["message"].as_str().unwrap();
    assert!(message.contains(".git/rivet-noreplace-a"), "{message}");
    assert!(message.contains(".git/rivet-noreplace-b"), "{message}");
    assert!(message.contains("writers stopped"), "{message}");
}

#[test]
fn p1_unsupported_no_replace_rename_refuses_before_any_record() {
    for (errno, armed) in [
        ("EINVAL", false),
        ("EINVAL", true),
        ("ENOSYS", false),
        ("ENOSYS", true),
    ] {
        let s = fixture(&format!("pair-noreplace-unsupported-{errno}-{armed}"));
        let before = untouched(&s);
        // Armed: faults on the record's first publication and on its
        // retirement, which a refusal that published first would hit.
        let mut fault = format!("move_noreplace_rename={errno}");
        if armed {
            fault.push_str(",record_publish@1,record_retire");
        }
        let (code, out) = s.run("__test-pair", json!({}), &[("RIVET_TEST_FAULT", fault)]);
        refused_unpublished(&s, code, &out, &before);
        assert_eq!(out["error"]["code"], "move_refused", "{errno}: {out}");
        assert_eq!(
            out["error"]["move_reason"], "fence_unsupported",
            "{errno}: {out}"
        );
        let message = out["error"]["message"].as_str().unwrap();
        assert!(
            message.contains("birth time and no-replace rename"),
            "{message}"
        );
        assert!(!message.contains("record_publish"), "{message}");
        assert_eq!(probes(&s), ["rivet-noreplace-a"]);
        assert_eq!(std::fs::read(probe(&s, "a")).unwrap(), b"");
        // No writer is blocked and nothing is left to settle.
        let (code, out) = s.set(&s.items()[0], "[x] Unrelated after the refusal.", &[]);
        assert_eq!(code, 0, "{errno}: {out}");
        let out = s.recover(json!({}), &[]);
        assert_eq!(out["unresolved"], 0, "{errno}: {out}");
    }
}

/// Put the pair fixture back to its read bytes in a new commit.
fn reset_pair(s: &Store) {
    std::fs::write(s.root.join(A), READ_A).unwrap();
    std::fs::write(s.root.join(B), READ_B).unwrap();
    s.git(&["commit", "-qm", "pair fixture again", "--", A, B]);
}

#[test]
fn p2_supported_moves_probe_once_each_and_alternate_the_probe_name() {
    let s = fixture("pair-noreplace-supported");
    assert!(probes(&s).is_empty());
    let entries = git_entries(&s);
    // A second probe in the same move would meet the armed @2 fault.
    let (code, out) = s.run(
        "__test-pair",
        json!({}),
        &[("RIVET_TEST_FAULT", "move_noreplace_rename@2=EINVAL".into())],
    );
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["commit"]["status"], "committed", "{out}");
    assert_eq!(probes(&s), ["rivet-noreplace-b"]);
    let mut expected = entries.clone();
    expected.insert("rivet-noreplace-b".into());
    let after: std::collections::BTreeSet<_> = git_entries(&s)
        .into_iter()
        .filter(|n| entries.contains(n) || n.starts_with("rivet-"))
        .collect();
    assert_eq!(after, expected, "only the probe is new in .git");
    reset_pair(&s);
    let entries = git_entries(&s);
    let (code, out) = s.run("__test-pair", json!({}), &[]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(probes(&s), ["rivet-noreplace-a"]);
    assert_eq!(std::fs::read(probe(&s, "a")).unwrap(), b"");
    let mut expected = entries.clone();
    expected.remove("rivet-noreplace-b");
    expected.insert("rivet-noreplace-a".into());
    let after: std::collections::BTreeSet<_> = git_entries(&s)
        .into_iter()
        .filter(|n| entries.contains(n) || n.starts_with("rivet-"))
        .collect();
    assert_eq!(after, expected, "only the probe moved in .git");
}

#[test]
fn p3_obstructed_probe_refuses_before_publication_and_keeps_every_byte() {
    use std::os::unix::fs::MetadataExt;
    type Setup = fn(&Store);
    let cases: [(&str, Setup, &str); 6] = [
        (
            "nonempty",
            |s| std::fs::write(probe(s, "a"), "foreign bytes\n").unwrap(),
            "",
        ),
        (
            "directory",
            |s| {
                std::fs::create_dir(probe(s, "a")).unwrap();
                std::fs::write(probe(s, "a").join("inside"), "kept\n").unwrap();
            },
            "",
        ),
        (
            "symlink",
            |s| std::os::unix::fs::symlink("HEAD", probe(s, "a")).unwrap(),
            "",
        ),
        (
            "both",
            |s| {
                std::fs::write(probe(s, "a"), "").unwrap();
                std::fs::write(probe(s, "b"), "").unwrap();
            },
            "",
        ),
        ("eexist", |_| {}, "move_noreplace_rename=EEXIST"),
        ("exdev", |_| {}, "move_noreplace_rename=EXDEV"),
    ];
    for (case, setup, fault) in cases {
        let s = fixture(&format!("pair-noreplace-obstructed-{case}"));
        setup(&s);
        let snapshot = |s: &Store| {
            ["a", "b"].map(|w| {
                let p = probe(s, w);
                std::fs::symlink_metadata(&p).ok().map(|m| {
                    let content = if m.is_dir() {
                        std::fs::read(p.join("inside")).unwrap()
                    } else if m.file_type().is_symlink() {
                        std::fs::read_link(&p)
                            .unwrap()
                            .into_os_string()
                            .into_encoded_bytes()
                    } else {
                        std::fs::read(&p).unwrap()
                    };
                    (m.ino(), content)
                })
            })
        };
        let probe_before = snapshot(&s);
        let before = untouched(&s);
        let vars: Vec<Var> = if fault.is_empty() {
            vec![]
        } else {
            vec![("RIVET_TEST_FAULT", fault.into())]
        };
        let (code, out) = s.run("__test-pair", json!({}), &vars);
        refused_unpublished(&s, code, &out, &before);
        obstructed(&out);
        if fault.is_empty() {
            assert_eq!(snapshot(&s), probe_before, "{case}");
        } else {
            // Rivet's own empty probe was created and stays where it was.
            assert_eq!(probes(&s), ["rivet-noreplace-a"], "{case}");
            assert_eq!(std::fs::read(probe(&s, "a")).unwrap(), b"");
        }
    }
}

#[test]
fn p3_a_real_target_is_never_replaced() {
    use std::os::unix::fs::MetadataExt;
    let s = fixture("pair-noreplace-real-target");
    let before = untouched(&s);
    let pause = Pause::new(&s.root, "move_noreplace_probe");
    let child = pause.wait_for(s.spawn("__test-pair", &json!({}), &[pause.env()]));
    std::fs::write(probe(&s, "b"), "foreign target\n").unwrap();
    let ino = |w| std::fs::metadata(probe(&s, w)).unwrap().ino();
    let (a, b) = (ino("a"), ino("b"));
    pause.release();
    let (code, out) = finish(child);
    refused_unpublished(&s, code, &out, &before);
    obstructed(&out);
    assert_eq!((ino("a"), ino("b")), (a, b));
    assert_eq!(std::fs::read(probe(&s, "a")).unwrap(), b"");
    assert_eq!(std::fs::read(probe(&s, "b")).unwrap(), b"foreign target\n");
}

#[test]
fn p4_an_entry_swapped_in_at_the_probe_is_moved_and_reported_never_deleted() {
    use std::os::unix::fs::MetadataExt;
    for case in ["foreign", "empty", "written"] {
        let s = fixture(&format!("pair-noreplace-swap-{case}"));
        let before = untouched(&s);
        let pause = Pause::new(&s.root, "move_noreplace_probe");
        let child = pause.wait_for(s.spawn("__test-pair", &json!({}), &[pause.env()]));
        let side = s.root.join(".git/kept-probe");
        match case {
            "foreign" => {
                std::fs::rename(probe(&s, "a"), &side).unwrap();
                std::fs::write(probe(&s, "a"), "foreign bytes\n").unwrap();
            }
            "empty" => {
                std::fs::rename(probe(&s, "a"), &side).unwrap();
                std::fs::write(probe(&s, "a"), "").unwrap();
            }
            _ => std::fs::write(probe(&s, "a"), "written in the gap\n").unwrap(),
        }
        let swapped = std::fs::metadata(probe(&s, "a")).unwrap().ino();
        let original = side
            .exists()
            .then(|| std::fs::metadata(&side).unwrap().ino());
        pause.release();
        let (code, out) = finish(child);
        refused_unpublished(&s, code, &out, &before);
        obstructed(&out);
        // The rename moved what was there, with its bytes; nothing deleted.
        assert_eq!(probes(&s), ["rivet-noreplace-b"], "{case}");
        assert_eq!(std::fs::metadata(probe(&s, "b")).unwrap().ino(), swapped);
        let bytes = std::fs::read(probe(&s, "b")).unwrap();
        match case {
            "foreign" => assert_eq!(bytes, b"foreign bytes\n"),
            "empty" => assert_eq!(bytes, b""),
            _ => assert_eq!(bytes, b"written in the gap\n"),
        }
        if let Some(original) = original {
            assert_eq!(std::fs::metadata(&side).unwrap().ino(), original);
            assert_eq!(std::fs::read(&side).unwrap(), b"");
        }
    }
}

#[test]
fn p5_recovery_needing_a_fresh_fence_reports_the_probe_and_creates_no_entry() {
    let (s, op) = crashed("pair-noreplace-recovery", "move_record_published");
    let record = s.record();
    assert!(record["fence"].is_null(), "{record}");
    no_staging(&s);
    let before = untouched(&s);
    let einval = [(
        "RIVET_TEST_FAULT",
        "move_noreplace_rename=EINVAL".to_string(),
    )];
    // Unsupported: recovery and abandonment both carry the probe's detail.
    let out = repaired(&s, &einval);
    assert_eq!(out["reason"], "fence_unsupported", "{out}");
    let detail = out["detail"].as_str().unwrap();
    assert!(
        detail.contains("no-replace rename is unsupported"),
        "{detail}"
    );
    let abandon = json!({"abandon": op, "reason": "no rename"});
    let (code, out) = s.run("commit-pending", abandon.clone(), &einval);
    assert_eq!(code, 2, "{out}");
    assert_eq!(out["error"]["code"], "pending_unresolved", "{out}");
    let message = out["error"]["message"].as_str().unwrap();
    assert!(message.starts_with("fence_unsupported: "), "{message}");
    assert!(
        message.contains("no-replace rename is unsupported"),
        "{message}"
    );
    no_staging(&s);
    assert_eq!(untouched(&s), before);
    // Obstructed: another process's file at the probe's other name.
    let other = if probe(&s, "a").exists() { "b" } else { "a" };
    std::fs::write(probe(&s, other), "foreign probe\n").unwrap();
    let out = repaired(&s, &[]);
    assert_eq!(out["reason"], "stale_git_lock", "{out}");
    let detail = out["detail"].as_str().unwrap();
    for name in [
        ".git/rivet-noreplace-a",
        ".git/rivet-noreplace-b",
        "writers stopped",
    ] {
        assert!(detail.contains(name), "{detail}");
    }
    let (code, out) = s.run("commit-pending", abandon, &[]);
    assert_eq!(code, 2, "{out}");
    let message = out["error"]["message"].as_str().unwrap();
    assert!(message.starts_with("stale_git_lock: "), "{message}");
    assert!(message.contains(".git/rivet-noreplace-a"), "{message}");
    assert_eq!(std::fs::read(probe(&s, other)).unwrap(), b"foreign probe\n");
    no_staging(&s);
    assert_eq!(untouched(&s), before);
    assert_eq!(s.record()["op"], op);
    assert!(s.record()["fence"].is_null());
    assert!(s.receipts().is_empty());
    // The operator removes the foreign file; with the rename working,
    // recovery settles.
    std::fs::remove_file(probe(&s, other)).unwrap();
    let out = s.recover(json!({}), &[]);
    assert_eq!(out["unresolved"], 0, "{out}");
    assert_eq!(out["records"][0]["result"], "not_applied", "{out}");
    assert_eq!(untouched(&s), before);
}

#[test]
fn p7_the_probe_runs_under_the_writer_lock() {
    use super::move_fixture::MoveStore;
    let m = MoveStore::new("tight", false);
    m.put(A, READ_A);
    m.put(B, READ_B);
    m.git(&["add", A, B]);
    m.git(&["commit", "-qm", "pair fixture"]);
    let request = m.request();
    // Writer A, the pair move, pauses at its probe holding the lock.
    let held = Pause::new(&m.root, "move_noreplace_probe");
    let a = held.wait_for(m.spawn("__test-pair", &json!({}), &[held.env()]));
    // Writer B, a distinct move, pauses as soon as it holds the lock and at
    // its own probe; neither fires while A holds the lock.
    let dir = m.root.with_extension("pause-writer-b");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let env = (
        "RIVET_TEST_PAUSE",
        format!("locked,move_noreplace_probe:{}", dir.display()),
    );
    let mut b = m.spawn("move", &request, &[env]);
    std::thread::sleep(std::time::Duration::from_millis(500));
    assert!(b.try_wait().unwrap().is_none(), "writer B is waiting");
    assert!(
        !dir.join("locked.reached").exists(),
        "B never took the lock"
    );
    assert!(!dir.join("move_noreplace_probe.reached").exists());
    held.release();
    let (code, out) = finish(a);
    assert_eq!(code, 0, "{out}");
    let locked = Pause {
        dir: dir.clone(),
        point: "locked",
    };
    let b = locked.wait_for(b);
    locked.release();
    let probing = Pause {
        dir,
        point: "move_noreplace_probe",
    };
    let b = probing.wait_for(b);
    probing.release();
    let (code, out) = finish(b);
    assert_eq!(code, 0, "{out}");
    let names: Vec<_> = std::fs::read_dir(m.root.join(".git"))
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .filter(|n| n.starts_with("rivet-noreplace-"))
        .collect();
    assert_eq!(names, ["rivet-noreplace-a"]);
}

#[test]
fn p8_status_and_doctor_say_nothing_about_the_probe_file() {
    let s = fixture("pair-noreplace-status");
    let (code, out) = s.run("__test-pair", json!({}), &[]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(probes(&s), ["rivet-noreplace-b"]);
    // Read times aside, the reports must match.
    fn timeless(v: &mut Value) {
        match v {
            Value::Object(map) => {
                map.retain(|k, _| !k.ends_with("read_at"));
                map.values_mut().for_each(timeless);
            }
            Value::Array(items) => items.iter_mut().for_each(timeless),
            _ => {}
        }
    }
    let report = |s: &Store| {
        ["status", "doctor"].map(|cmd| {
            let (code, mut out) = s.run(cmd, json!({}), &[]);
            assert!(!out.to_string().contains("rivet-noreplace"), "{cmd}: {out}");
            timeless(&mut out);
            (code, out)
        })
    };
    let with_probe = report(&s);
    // The same reports with the probe moved out of .git, as an operator may.
    let aside = s.root.with_extension("probe-aside");
    std::fs::rename(probe(&s, "b"), &aside).unwrap();
    assert_eq!(report(&s), with_probe);
    std::fs::rename(&aside, probe(&s, "b")).unwrap();
}
