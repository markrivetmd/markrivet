//! The peer record owns its fence across calls and process exits. A held
//! descriptor may close; the name is retained until the release invariant holds.
use super::*;

pub(crate) struct PairFile<'a> {
    pub path: &'a str,
    pub read: &'a str,
    pub candidate: &'a str,
}

pub(crate) fn preflight_pair(
    project: &Project,
    a: (&str, &str),
    b: (&str, &str),
) -> Result<(Repo, Vec<git::Preflight>), Diagnostic> {
    let repo = Repo::inspect(project);
    let mut pfs = Vec::new();
    for (path, read) in [a, b] {
        let file = FileState::read(project, path);
        if !matches!(&file,FileState::Present{bytes,..} if bytes==read.as_bytes()) {
            return Err(Diagnostic::new(
                ErrorCode::ExternalChange,
                "move input changed; nothing written",
            ));
        }
        let pf = git::preflight(&repo, path, &file, false);
        if pf.base != git::Base::KnownClean || pf.ineligible.is_some() {
            return Err(Diagnostic::new(
                ErrorCode::CommitRefused,
                format!("move requires clean eligible file {path}"),
            ));
        }
        pfs.push(pf);
    }
    if pfs[0].head != pfs[1].head
        || pfs[0].branch != pfs[1].branch
        || git::lingering_lock(project, pfs[0].branch.as_deref()).is_some()
    {
        return Err(Diagnostic::new(
            ErrorCode::CommitRefused,
            "move preflight changed or a git lock is present",
        ));
    }
    Ok((repo, pfs))
}

pub(crate) fn land_pair(
    project: &Project,
    lock: &WriterLock,
    a: PairFile<'_>,
    b: PairFile<'_>,
    mut receipt: crate::relocate::MoveReceipt,
    message: Option<String>,
    session: Option<String>,
) -> Result<Landed, Refusal> {
    let before = |e| (e, None);
    let (repo, pfs) =
        preflight_pair(project, (a.path, a.read), (b.path, b.read)).map_err(before)?;
    let blob = |f: &PairFile<'_>, mode: &str| -> Result<Entry, Diagnostic> {
        let out = repo
            .git
            .store_blob(f.candidate.as_bytes())
            .map_err(|e| Diagnostic::new(ErrorCode::CommitRefused, e.to_string()))?;
        let oid = oid(out).map_err(|e| Diagnostic::new(ErrorCode::CommitRefused, e))?;
        Ok(Entry {
            oid,
            mode: mode.into(),
        })
    };
    let base_a = pfs[0].head_entry.clone().expect("clean tracked file");
    let base_b = pfs[1].head_entry.clone().expect("clean tracked file");
    let candidate_a = blob(&a, &base_a.mode).map_err(before)?;
    let candidate_b = blob(&b, &base_b.mode).map_err(before)?;
    let op = random_op();
    receipt.op = op.clone();
    let mut rec = Record {
        trailers: vec![Trailer::Move(Box::new(receipt))],
        archive_parent: None,
        guards: vec![],
        phase: Some(Phase::Prepared),
        blocked: false,
        data: RecordData {
            user_message: Some(message.clone().unwrap_or_default()),
            require_commit: true,
            op,
            seq: project.next_seq(lock).map_err(before)?,
            path: a.path.into(),
            peer: Some(Peer {
                path: b.path.into(),
                read_sha256: sha256(b.read.as_bytes()),
                candidate_sha256: sha256(b.candidate.as_bytes()),
                base: base_b,
                candidate: candidate_b,
            }),
            fence: None,
            fence_waived: false,
            outcome: Outcome::Commit,
            reason: None,
            detail: None,
            units: vec![],
            session,
            read_sha256: sha256(a.read.as_bytes()),
            candidate_sha256: sha256(a.candidate.as_bytes()),
            branch: pfs[0].branch.clone(),
            base_head: pfs[0].head.clone(),
            expect_head: None,
            base: Some(base_a),
            candidate: Some(candidate_a),
            message: None,
            provenance: vec![],
            pid: None,
            spawned_ns: None,
            child_identity: None,
            index_lease: None,
            ever_applied: false,
            creation: false,
            sha: None,
            retries: 0,
            git_ms: 0,
            repair: None,
            created: git::now_millis(),
        },
    };
    if !rec
        .move_receipt()
        .is_some_and(|receipt| receipt.identifies(&rec))
    {
        return Err(before(Diagnostic::new(
            ErrorCode::Internal,
            "move receipt and pair disagree before publication",
        )));
    }
    rec.message = Some(rec.compose(message.as_deref()).map_err(before)?);
    // No-replace rename is proved before the record exists, so a filesystem
    // without it refuses with no record, staging entry or lock to settle.
    let proof = git::NoReplace::prove(project).map_err(|e| {
        before(match e {
            git::FenceError::Unsupported(e) => fence_unsupported(&e),
            other => Diagnostic::new(
                ErrorCode::CommitRefused,
                format!(
                    "move fence unavailable; nothing written: {}",
                    fence_detail(other)
                ),
            ),
        })
    })?;
    // The record exists before its fence is named, so no crash leaves an
    // index.lock it does not name. Until a document is replaced, a refusal
    // withdraws it.
    if let Err(e) = publish(project, lock, &rec) {
        return Err(withdraw(project, lock, &rec, None, e));
    }
    let _ = hook("move_record_published");
    let mut fence = match fence_publish(project, lock, &mut rec, proof) {
        Ok(fence) => fence,
        Err(fence_error) => {
            let unavailable = |detail: String| {
                Diagnostic::new(
                    ErrorCode::CommitRefused,
                    format!("move fence unavailable; nothing written: {detail}"),
                )
            };
            return Err(match fence_error {
                git::FenceError::Unsupported(e) => {
                    withdraw(project, lock, &rec, None, fence_unsupported(&e))
                }
                git::FenceError::Io(e) => {
                    withdraw(project, lock, &rec, None, unavailable(e.to_string()))
                }
                // A staging entry or a named fence remains: the record stays
                // pending so it is reported, and nothing Rivet did not
                // create is removed.
                git::FenceError::Staged(detail) => (unavailable(detail), Some(rec.op.clone())),
                git::FenceError::Named(e) => (unavailable(e.to_string()), Some(rec.op.clone())),
            });
        }
    };
    let checked = hook("move_fenced").and_then(|()| {
        if !unmoved(&repo.git, &rec).map_err(|e| Diagnostic::new(ErrorCode::CommitRefused, e))? {
            return Err(Diagnostic::new(
                ErrorCode::CommitRefused,
                "head_moved before move publication; nothing written",
            ));
        }
        rec.move_receipt()
            .expect("receipt")
            .check_recovery(project)
            .map_err(|(reason, detail)| {
                Diagnostic::new(ErrorCode::CommitRefused, format!("{reason}: {detail}"))
            })
    });
    if let Err(e) = checked {
        return Err(withdraw(project, lock, &rec, Some(fence), e));
    }
    fence.retain();
    hook("move_pending_written").map_err(|e| (e, Some(rec.op.clone())))?;
    let destination = crate::fs::fault("move_destination_rename")
        .map_err(|e| Diagnostic::new(ErrorCode::Io, e.to_string()))
        .and_then(|_| project.replace(lock, b.path, b.read, b.candidate));
    match destination {
        Ok(crate::fs::Commit::Applied) => {}
        Ok(crate::fs::Commit::AppliedNotDurable) => return Ok(pending(&rec, "not_durable")),
        Err(e) if e.code != ErrorCode::CommitUncertain => {
            if all_read(project, &rec)
                && clean_pair(project, &repo.git, &rec).unwrap_or(false)
                && sync_pair(project, &rec).is_ok()
                && fence.release().is_ok()
            {
                let left = conclude(
                    project,
                    lock,
                    &rec,
                    &event(&rec, Outcome::NotApplied, Applied::No, false),
                )
                .pending();
                return Err((e, left.then_some(rec.op.clone())));
            }
            return Ok(pending(&rec, "destination_replace"));
        }
        Err(_) => return Ok(pending(&rec, "commit_uncertain")),
    }
    let _ = hook("move_destination_replaced");
    let source = crate::fs::fault("move_source_rename")
        .map_err(|e| Diagnostic::new(ErrorCode::Io, e.to_string()))
        .and_then(|_| project.replace(lock, a.path, a.read, a.candidate));
    match source {
        Ok(crate::fs::Commit::Applied) => {}
        Ok(crate::fs::Commit::AppliedNotDurable) => return Ok(pending(&rec, "not_durable")),
        Err(e) if e.code != ErrorCode::CommitUncertain => {
            // Never remove B's copy unless A is still the exact recorded source.
            if matches!(file_is(project, &rec), Bytes::Read)
                && peer_is(project, &rec, false)
                && crate::fs::fault("move_compensate").is_ok()
                && matches!(
                    project.replace(lock, b.path, b.candidate, b.read),
                    Ok(crate::fs::Commit::Applied)
                )
            {
                if matches!(file_is(project, &rec), Bytes::Read) {
                    if sync_pair(project, &rec).is_ok()
                        && clean_pair(project, &repo.git, &rec).unwrap_or(false)
                        && fence.release().is_ok()
                    {
                        let left = conclude(
                            project,
                            lock,
                            &rec,
                            &event(&rec, Outcome::NotApplied, Applied::No, false),
                        )
                        .pending();
                        return Err((e, left.then_some(rec.op.clone())));
                    }
                } else {
                    let _ = project.replace(lock, b.path, b.read, b.candidate);
                }
            }
            return Ok(pending(&rec, "compensation_unconfirmed"));
        }
        Err(_) => return Ok(pending(&rec, "commit_uncertain")),
    }
    rec.phase = Some(Phase::Applied);
    rec.ever_applied = true;
    if publish(project, lock, &rec).is_err() {
        return Ok(pending(&rec, "bookkeeping"));
    }
    let _ = hook("move_applied");
    drop(fence); // Name remains owned by the durable record.
    let value = recover_pair(project, lock, &mut rec);
    let committed = value["result"] == "committed" || value["result"] == "recovered";
    let commit = if value["commit"].is_object() {
        value["commit"].clone()
    } else {
        with_op(
            &rec.op,
            json!({"status":"pending", "pending":rec.op, "reason":value["reason"], "detail":value["detail"]}),
        )
    };
    Ok(Landed {
        durable: committed,
        commit,
        receipt_failed: value["receipt"] == "failed",
    })
}

fn pending(rec: &Record, reason: &str) -> Landed {
    Landed {
        durable: false,
        commit: with_op(
            &rec.op,
            json!({"status":"pending", "reason":reason, "pending":rec.op}),
        ),
        receipt_failed: false,
    }
}

fn peer_is(project: &Project, rec: &Record, read: bool) -> bool {
    let p = rec.peer.as_ref().expect("pair");
    match bytes_at(project, &p.path, &p.read_sha256, &p.candidate_sha256) {
        Bytes::Read => read,
        Bytes::Candidate => !read,
        Bytes::Neither => false,
    }
}

pub(super) fn files_are_candidates(project: &Project, rec: &Record) -> bool {
    let file_mode = Repo::inspect(project).file_mode;
    let peer = rec.peer.as_ref().expect("pair");
    [
        (&rec.path, &rec.candidate_sha256, rec.candidate.as_ref()),
        (&peer.path, &peer.candidate_sha256, Some(&peer.candidate)),
    ]
    .into_iter()
    .all(|(path, hash, candidate)| {
        let Some(candidate) = candidate else {
            return false;
        };
        let file = FileState::read(project, path);
        matches!(&file, FileState::Present { bytes, .. } if sha256(bytes) == *hash)
            && (!file_mode || file.git_mode() == Some(candidate.mode.as_str()))
    })
}

fn all_read(project: &Project, rec: &Record) -> bool {
    matches!(file_is(project, rec), Bytes::Read) && peer_is(project, rec, true)
}

fn sync_pair(project: &Project, rec: &Record) -> Result<(), Diagnostic> {
    let p = rec.peer.as_ref().expect("pair");
    // The gaining path must survive a crash before the losing path is synced.
    for path in [&p.path, &rec.path] {
        crate::fs::fault("move_recovery_sync")
            .map_err(|e| Diagnostic::new(ErrorCode::Io, e.to_string()))?;
        project.sync_document(path)?;
    }
    Ok(())
}

/// Withdraw a published pair that has replaced no document: release its
/// fence durably, then retire the record without an event, so a refused
/// move leaves no trace. Release comes first: a crash between the two leaves
/// a record naming an absent fence, never a fence no record names. Any
/// failure leaves the record for recovery, which concludes it `not_applied`.
fn withdraw(
    project: &Project,
    lock: &WriterLock,
    rec: &Record,
    fence: Option<git::IndexLock>,
    e: Diagnostic,
) -> Refusal {
    let released = fence.map_or(Ok(()), git::IndexLock::release);
    if released.is_ok() && retire(project, lock, rec).is_ok() {
        return (e, None);
    }
    (e, Some(rec.op.clone()))
}

/// Name a new fence through [`git::IndexLock::acquire_published`], first
/// publishing its identity in the record. A named (or possibly named) fence
/// updates the record in memory as on disk; any other failure keeps its
/// previous fence in memory.
fn fence_publish(
    project: &Project,
    lock: &WriterLock,
    rec: &mut Record,
    proof: git::NoReplace,
) -> Result<git::IndexLock, git::FenceError> {
    let mut next = rec.clone();
    let held = git::IndexLock::acquire_published(&rec.op, proof, |fence| {
        next.fence = Some(fence.clone());
        publish(project, lock, &next)?;
        hook("move_fence_recorded")
    });
    match held {
        Ok(held) => {
            *rec = next;
            Ok(held)
        }
        Err(e @ git::FenceError::Named(_)) => {
            *rec = next;
            Err(e)
        }
        Err(e) => Err(e),
    }
}

pub(super) fn fence_acquire_durable(
    project: &Project,
    lock: &WriterLock,
    rec: &mut Record,
) -> Result<git::IndexLock, &'static str> {
    fence_acquire_explained(project, lock, rec).map_err(|(why, _)| why)
}

/// The `fence_unsupported` refusal for a missing filesystem capability.
fn fence_unsupported(e: &std::io::Error) -> Diagnostic {
    crate::relocate::refused(
        crate::MoveReason::FenceUnsupported,
        format!("move fence requires filesystem birth time and no-replace rename: {e}"),
    )
}

/// What a fence error says, for a refusal or repair detail.
fn fence_detail(e: git::FenceError) -> String {
    match e {
        git::FenceError::Unsupported(e) | git::FenceError::Io(e) | git::FenceError::Named(e) => {
            e.to_string()
        }
        git::FenceError::Staged(detail) => detail,
    }
}

/// A staging entry is reported before any takeover, waived or not, whatever
/// holds index.lock, so the operator step is never hidden behind a lock.
fn staging_absent(project: &Project, rec: &Record) -> Result<(), (&'static str, String)> {
    match git::staging_present(project, &rec.op) {
        Ok(Some(detail)) => Err(("stale_git_lock", detail)),
        Ok(None) => Ok(()),
        Err(e) => Err((
            "stale_git_lock",
            format!("cannot check the fence staging entry: {e}"),
        )),
    }
}

/// [`fence_acquire_durable`] with the detail a repair report carries.
fn fence_acquire_explained(
    project: &Project,
    lock: &WriterLock,
    rec: &mut Record,
) -> Result<git::IndexLock, (&'static str, String)> {
    let generic = |why| (why, "cannot acquire the record's fence".to_string());
    if rec.fence_waived {
        return Err(generic("fence_waived"));
    }
    staging_absent(project, rec)?;
    if let Some(expected) = &rec.fence {
        match git::IndexLock::take_over(project, expected) {
            Ok(held) => return Ok(held),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err(generic("stale_git_lock")),
        }
    }
    // A fresh fence needs no-replace rename: proved here, before any
    // staging entry, with the probe's own detail in the report.
    let proof = git::NoReplace::prove(project).map_err(|e| match e {
        git::FenceError::Unsupported(e) => ("fence_unsupported", e.to_string()),
        other => ("stale_git_lock", fence_detail(other)),
    })?;
    // A replacement is published before it is named, so the record always
    // owns it; a staging entry left behind is reported, never removed.
    let held = fence_publish(project, lock, rec, proof).map_err(|e| match e {
        git::FenceError::Unsupported(_) => generic("fence_unsupported"),
        git::FenceError::Staged(detail) => ("stale_git_lock", detail),
        git::FenceError::Named(e) => (
            "stale_git_lock",
            format!(
                "the fence rename was not confirmed ({e}); .git/index.lock may be this \
                 record's fence: run commit-pending again to take it over"
            ),
        ),
        git::FenceError::Io(_) => generic("stale_git_lock"),
    })?;
    let _ = hook("move_fence_reacquired");
    Ok(held)
}

fn history_op(git: &Git, rec: &Record) -> Result<Option<(String, Vec<String>, String)>, String> {
    let branch = rec.branch.as_deref().ok_or("move has no recorded branch")?;
    let bound_branch = || -> Result<Option<String>, String> {
        let (current, head) = git::head_state(git)?;
        if current.as_deref() != Some(branch) {
            return Err("recorded branch changed; restore it before proving move history".into());
        }
        Ok(head)
    };
    let audited_head = bound_branch()?.ok_or("recorded branch has no tip")?;
    if let Some(found) = find_history_op(
        git,
        rec.base_head.as_deref().ok_or("record has no base HEAD")?,
        &audited_head,
        &rec.op,
        HistoryPurpose::ProveAbsence,
    )? {
        return Ok(Some(found));
    }
    hook("move_history_audited").map_err(Diagnostic::detail)?;
    if bound_branch()? != Some(audited_head) {
        return Err("recorded branch advanced during move history audit; retry".into());
    }
    Ok(None)
}

fn clean_pair(project: &Project, git: &Git, rec: &Record) -> Result<bool, String> {
    let file_mode = Repo::inspect(project).file_mode;
    let (branch, head) = git::head_state(git)?;
    if branch != rec.branch {
        return Ok(false);
    }
    let head = head.ok_or("unborn HEAD")?;
    for (path, _, _) in rec.entries() {
        let (entry, index) = git::path_entries(git, &head, path)?;
        let Some(entry) = entry else {
            return Ok(false);
        };
        if !index_is(&index, Some(&entry)) {
            return Ok(false);
        }
        let out = git
            .inspect(&["cat-file", "blob", &entry.oid], None)
            .map_err(|e| e.to_string())?;
        if !out.ok() {
            return Err(out.failure("git cat-file"));
        }
        let file = FileState::read(project, path);
        if !matches!(&file, FileState::Present { bytes, .. } if *bytes == out.stdout)
            || (file_mode && file.git_mode() != Some(entry.mode.as_str()))
        {
            return Ok(false);
        }
    }
    Ok(true)
}

fn repair(
    project: &Project,
    lock: &WriterLock,
    rec: &mut Record,
    reason: &str,
    detail: &str,
) -> Value {
    require_repair(rec, reason, detail);
    let _ = publish(project, lock, rec);
    repair_report(rec)
}

pub(super) fn recover_pair(project: &Project, lock: &WriterLock, rec: &mut Record) -> Value {
    let value = recover_pair_once(project, lock, rec);
    if value["result"] == "failed" && value["commit"]["reason"] == "head_moved" {
        return recover_pair_once(project, lock, rec);
    }
    value
}

fn recover_pair_once(project: &Project, lock: &WriterLock, rec: &mut Record) -> Value {
    if rec.fence_waived {
        let mut value = report(rec, "repair_required");
        value["reason"] = json!("fence_waived");
        value["detail"] = json!("hands-off pair; commit both paths together, then abandon");
        return value;
    }
    if !rec
        .move_receipt()
        .is_some_and(|receipt| receipt.identifies(rec))
    {
        return repair(
            project,
            lock,
            rec,
            "move_receipt_mismatch",
            "pair record and move receipt disagree",
        );
    }
    let git = Git::new(project);
    let mut held = match fence_acquire_explained(project, lock, rec) {
        Ok(h) => h,
        Err((why, detail)) => return repair(project, lock, rec, why, &detail),
    };
    held.retain();
    let locks = match git::list_locks(project, rec.branch.as_deref()) {
        Ok(l) => l,
        Err(e) => return repair(project, lock, rec, "stale_git_lock", &format!("{e:?}")),
    };
    if locks
        .iter()
        .any(|l| l.blocking() && l.file != ".git/index.lock")
    {
        return repair(
            project,
            lock,
            rec,
            "stale_git_lock",
            "clear the recorded ref lock; the index fence stays owned",
        );
    }
    let result = (|| -> Result<Value, (&'static str, String)> {
        let err = |e: String| ("git_error", e);
        let (branch, head) = git::head_state(&git).map_err(err)?;
        let head = head.ok_or_else(|| err("unborn HEAD".into()))?;
        if branch != rec.branch {
            return Err(("branch_mismatch", "branch changed".into()));
        }
        if let Some((sha, parents, message)) = history_op(&git, rec).map_err(err)? {
            transaction_shape(&git, rec, &sha, &parents, &message)
                .map_err(err)?
                .map_err(|why| {
                    (
                        why,
                        "commit carrying the op has a different transaction shape".into(),
                    )
                })?;
            // Never consume the fence over a stale index while a worktree file
            // differs from HEAD, including a manual restoration of read bytes.
            for (path, _, _) in rec.entries() {
                let (entry, _) = git::path_entries(&git, &head, path).map_err(err)?;
                let entry = entry.ok_or_else(|| err("missing HEAD entry".into()))?;
                let out = git
                    .inspect(&["cat-file", "blob", &entry.oid], None)
                    .map_err(|e| err(e.to_string()))?;
                if !out.ok() {
                    return Err(err(out.failure("git cat-file")));
                }
                if !project
                    .read_raw(path)
                    .map_err(|e| err(e.detail()))?
                    .is_some_and(|(b, _)| b == out.stdout)
                {
                    return Err((
                        "worktree_differs",
                        format!(
                            "restore the HEAD blob with git show HEAD:{path} > {path}, then commit-pending"
                        ),
                    ));
                }
            }
            sync_pair(project, rec).map_err(|e| ("not_durable", e.detail()))?;
            if clean_pair(project, &git, rec).map_err(err)? {
                held.sync_existing()
                    .map_err(|e| ("index_changed", e.to_string()))?;
                held.release()
                    .map_err(|e| ("index_changed", e.to_string()))?;
            } else {
                // With a fence, no later git writer can legitimately have
                // changed either target. Refuse all unexplained entries.
                for (path, base, candidate) in rec.entries() {
                    let (entry, index) = git::path_entries(&git, &head, path).map_err(err)?;
                    if entry.as_ref() != candidate
                        || !(index_is(&index, base) || index_is(&index, candidate))
                    {
                        return Err((
                            "index_changed",
                            "unexplained pair index or HEAD entries".into(),
                        ));
                    }
                }
                drop(held);
                prepare_index(project, lock, &git, rec, &head)
                    .and_then(|index| install_record_index(project, &git, rec, index))
                    .map_err(|why| (why, "pair index repair failed".into()))?;
            }
            let (v, failed) = committed(project, lock, rec, sha, true);
            return Ok(recovered_report(rec, v, failed));
        }
        // Validate every precondition before any roll-forward worktree write.
        rec.move_receipt()
            .ok_or(("move_receipt_mismatch", "pair lacks receipt".into()))?
            .check_recovery(project)?;
        let a_read = matches!(file_is(project, rec), Bytes::Read);
        let b_read = peer_is(project, rec, true);
        if !(a_read || matches!(file_is(project, rec), Bytes::Candidate))
            || !(b_read || peer_is(project, rec, false))
        {
            return Err((
                "file_changed",
                "a move file is neither read nor candidate; outside edits preserved".into(),
            ));
        }
        hook("move_recovery_preflight").map_err(|e| ("git_error", e.detail()))?;
        let repo = Repo::inspect(project);
        for (path, base, _) in rec.entries() {
            let (entry, index) = git::path_entries(&git, &head, path).map_err(err)?;
            if entry.as_ref() != base || !index_is(&index, base) {
                return Err(("target_changed", "a pair path moved in git".into()));
            }
            let pf = git::preflight(&repo, path, &FileState::read(project, path), false);
            if let Some(i) = &pf.ineligible {
                return Err((i.reason, i.detail.clone()));
            }
            if !recovery_preflight_matches(&pf, &rec.branch, &head, base)
                || !index_is(&pf.index, base)
            {
                return Err((
                    "target_changed",
                    "branch, HEAD or a pair entry changed during recovery preflight".into(),
                ));
            }
        }
        if a_read && b_read {
            sync_pair(project, rec).map_err(|e| ("not_durable", e.detail()))?;
            if !clean_pair(project, &git, rec).map_err(err)? {
                return Err(("worktree_differs", "pair changed during recovery".into()));
            }
            held.release().map_err(|e| ("not_durable", e.to_string()))?;
            return Ok(concluded(
                project,
                lock,
                rec,
                Outcome::NotApplied,
                Applied::No,
            ));
        }
        let p = rec.peer.as_ref().expect("pair").clone();
        // Destination first in every cell, including candidate/read.
        for (path, read_hash, read_entry, candidate_hash, candidate_entry) in [
            (
                p.path.clone(),
                p.read_sha256.clone(),
                p.base.clone(),
                p.candidate_sha256.clone(),
                p.candidate.clone(),
            ),
            (
                rec.path.clone(),
                rec.read_sha256.clone(),
                rec.base.clone().expect("base"),
                rec.candidate_sha256.clone(),
                rec.candidate.clone().expect("candidate"),
            ),
        ] {
            let bytes = project
                .read(&path)
                .map_err(|e| ("file_changed", e.detail()))?;
            let hash = sha256(bytes.as_bytes());
            if hash == read_hash {
                let read = git
                    .inspect(&["cat-file", "blob", &read_entry.oid], None)
                    .map_err(|e| err(e.to_string()))?;
                let candidate = git
                    .inspect(&["cat-file", "blob", &candidate_entry.oid], None)
                    .map_err(|e| err(e.to_string()))?;
                if !read.ok()
                    || !candidate.ok()
                    || sha256(&read.stdout) != read_hash
                    || sha256(&candidate.stdout) != candidate_hash
                {
                    return Err((
                        "object_mismatch",
                        "recorded pair object missing or hash differs".into(),
                    ));
                }
                let text = String::from_utf8(candidate.stdout).map_err(|e| err(e.to_string()))?;
                if !matches!(
                    project.replace(lock, &path, &bytes, &text),
                    Ok(crate::fs::Commit::Applied)
                ) {
                    return Err((
                        "not_durable",
                        "roll-forward rename not confirmed durable".into(),
                    ));
                }
            } else if hash != candidate_hash {
                return Err(("file_changed", "pair changed during recovery".into()));
            }
            crate::fs::fault("move_recovery_sync").map_err(|e| ("not_durable", e.to_string()))?;
            project
                .sync_document(&path)
                .map_err(|e| ("not_durable", e.detail()))?;
        }
        rec.base_head = Some(head);
        rec.phase = Some(Phase::Applied);
        rec.blocked = false;
        rec.repair = None;
        rec.ever_applied = true;
        publish(project, lock, rec).map_err(|e| err(e.detail()))?;
        drop(held);
        let (v, failed) = commit_record(project, lock, &git, rec, true);
        Ok(recovered_report(rec, v, failed))
    })();
    match result {
        Ok(v) => v,
        Err((why, detail)) => repair(project, lock, rec, why, &detail),
    }
}

fn check_abandonable(project: &Project, git: &Git, rec: &Record) -> Result<(), Diagnostic> {
    let prove = || -> Result<(), String> {
        if history_op(git, rec)?.is_some() {
            return Err("published move cannot be abandoned; recover it".into());
        }
        if !clean_pair(project, git, rec)? {
            return Err("both files must equal HEAD and the index before abandoning a move".into());
        }
        pair_agrees(project, rec)
    };
    prove().map_err(|e| Diagnostic::new(ErrorCode::PendingUnresolved, e))
}

/// Each side's application certainty, as an abandoned event reports it.
fn sides(project: &Project, rec: &Record) -> (Applied, Applied) {
    let certainty =
        |path: &str, read: &str, candidate: &str| match bytes_at(project, path, read, candidate) {
            Bytes::Candidate => Applied::Yes,
            Bytes::Read => Applied::No,
            Bytes::Neither => Applied::Unknown,
        };
    let peer = rec.peer.as_ref().expect("pair");
    (
        certainty(&rec.path, &rec.read_sha256, &rec.candidate_sha256),
        certainty(&peer.path, &peer.read_sha256, &peer.candidate_sha256),
    )
}

/// An abandoned pair never leaves a carried stay in both files, nor, where
/// Rivet's own bytes decide it (one side its read bytes, the other its
/// candidate), in neither. An outside edit on either side is the operator's.
fn pair_agrees(project: &Project, rec: &Record) -> Result<(), String> {
    let peer = rec.peer.as_ref().expect("pair");
    let receipt = rec.move_receipt().ok_or("pair lacks its move receipt")?;
    let carried = |path: &str| -> Result<crate::citations::IdMap, String> {
        let text = project
            .read(path)
            .map_err(|e| format!("cannot prove what {path} carries: {}", e.detail()))?;
        let doc = crate::Document::parse(&text)
            .map_err(|e| format!("cannot prove what {path} carries: {}", e.detail()))?;
        let mut map = crate::citations::IdMap::default();
        map.add(path, &doc, false);
        Ok(map)
    };
    let (source, destination) = (carried(&rec.path)?, carried(&peer.path)?);
    let finish = "commit both files with the move finished (source and destination candidates) \
                  or undone (both read bytes), then abandon";
    if let Some(id) = receipt
        .ids
        .iter()
        .find(|id| source.carries(id) && destination.carries(id))
    {
        return Err(format!(
            "abandoning would leave stay {id} in both files, {} and {}; {finish}",
            rec.path, peer.path
        ));
    }
    match sides(project, rec) {
        (Applied::Yes, Applied::No) => Err(format!(
            "{} holds its move candidate and {} its read bytes; abandoning would leave stay {} in neither file; {finish}",
            rec.path, peer.path, receipt.id
        )),
        (Applied::No, Applied::Yes) => Err(format!(
            "{} holds its read bytes and {} its move candidate; {finish}",
            rec.path, peer.path
        )),
        _ => Ok(()),
    }
}

pub(super) fn abandon_pair(
    project: &Project,
    lock: &WriterLock,
    mut rec: Record,
    reason: &str,
) -> Result<Value, Diagnostic> {
    let git = Git::new(project);
    check_abandonable(project, &git, &rec)?;
    if rec.fence_waived {
        staging_absent(project, &rec).map_err(|(why, detail)| {
            Diagnostic::new(ErrorCode::PendingUnresolved, format!("{why}: {detail}"))
        })?;
        let expected = rec.fence.as_ref().ok_or_else(|| {
            Diagnostic::new(
                ErrorCode::PendingUnresolved,
                "waived pair lacks its fence identity",
            )
        })?;
        match git::IndexLock::take_over(project, expected) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            _ => {
                return Err(Diagnostic::new(
                    ErrorCode::PendingUnresolved,
                    "waived pair still meets an index fence; explicitly clear its verified fence before abandonment",
                ));
            }
        }
    } else {
        let held = fence_acquire_explained(project, lock, &mut rec).map_err(|(why, detail)| {
            Diagnostic::new(ErrorCode::PendingUnresolved, format!("{why}: {detail}"))
        })?;
        sync_pair(project, &rec)?;
        check_abandonable(project, &git, &rec).map_err(|mut d| {
            d.message = format!("{}; the record's fence stays at .git/index.lock", d.message);
            d
        })?;
        held.release()
            .map_err(|e| Diagnostic::new(ErrorCode::Io, e.to_string()))?;
    }
    let (applied, peer_applied) = sides(project, &rec);
    let mut e = event(&rec, Outcome::Abandoned, applied, true);
    e.peer_applied = Some(peer_applied);
    e.reason = Some(reason.into());
    finish_abandon(project, lock, &rec, &e)
}

pub(super) fn waive(
    project: &Project,
    lock: &WriterLock,
    rec: &mut Record,
    identity: &str,
) -> Result<Value, Diagnostic> {
    let refuse = |s: String| Diagnostic::new(ErrorCode::ClearLockRefused, s);
    if !rec.child().dead() {
        return Err(refuse("recorded child is alive".into()));
    }
    if history_op(&Git::new(project), rec)
        .map_err(refuse)?
        .is_some()
    {
        return Err(refuse(
            "published move cannot waive its fence; restore both HEAD blobs and recover".into(),
        ));
    }
    let listed =
        git::list_locks(project, rec.branch.as_deref()).map_err(|e| refuse(format!("{e:?}")))?;
    if !listed.iter().any(|l| {
        l.file == ".git/index.lock"
            && l.identity == identity
            && git::lock_veto(project, l).is_none()
    }) {
        return Err(refuse("fence identity changed or lock has a veto".into()));
    }
    let held = git::IndexLock::take_over(
        project,
        rec.fence
            .as_ref()
            .ok_or_else(|| refuse("no recorded fence".into()))?,
    )
    .map_err(|e| refuse(e.to_string()))?;
    // Persist hands-off semantics before removing the lock. A crash between
    // these steps leaves a waived record and a clearable owned fence.
    rec.fence_waived = true;
    publish(project, lock, rec)?;
    hook("move_fence_waived")?;
    held.release().map_err(|e| refuse(e.to_string()))?;
    Ok(report(rec, "fence_waived"))
}
