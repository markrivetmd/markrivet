//! Per-write commits: with `"git": {"commit": "per-write"}`, a Rivet write
//! to a file that was clean before it becomes exactly one git commit, made
//! under the project lock.
//!
//! Every write that will land in commit mode is first described by a
//! pending record in `.rivet/pending/<op>.json`, published before the
//! rename and removed only after the write's terminal outcome is fsynced to
//! `.rivet/commits.jsonl`. A crash at any point leaves a record that
//! `markrivet commit-pending` settles: by the bytes alone for a write that was
//! never going to commit, and through git (finding the transaction commit by
//! its `Rivet-Op` trailer, or committing the recorded candidate on the
//! current HEAD) for one that was.
use crate::fs::{LogFailure, Project, WriterLock, hook, hook_with};
use crate::git::{
    self, Decision, Entry, FileState, Git, GitChild, GitError, IndexEntry, Ineligible, OnDirty,
    Repo,
};
use crate::schema::Bound;
use crate::{Diagnostic, ErrorCode};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// The receipt log, under `.rivet/`.
pub const RECEIPTS: &str = "commits.jsonl";
const PENDING: &str = crate::fs::STAGING;
/// Further attempts to acquire a contended index lock.
const INDEX_RETRIES: u32 = 3;
/// How far back recovery walks first-parent history for a transaction.
const HISTORY_LIMIT: &str = "10000";
/// Includes every supported caller message plus raw commit headers and trailers.
const HISTORY_OBJECT_BYTES: usize = 64 * 1024 * 1024;
/// A 16 MiB request can retain caller text in both composed and user messages.
/// Bookkeeping has its own limit, separate from the 8 MiB document cap.
pub const MAX_RECORD_BYTES: u64 = 40 * 1024 * 1024;
/// Rivet's own trailer keys, refused in a caller's message.
const RESERVED: [&str; 5] = [
    "rivet-op",
    "includes-outside-edit",
    "rivet-delete",
    "rivet-archive",
    "rivet-move",
];

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum CommitMode {
    #[default]
    #[serde(rename = "off")]
    Off,
    #[serde(rename = "per-write")]
    PerWrite,
}

/// The `git` key of `.rivet/project.json`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GitConfig {
    #[serde(default)]
    pub commit: CommitMode,
    #[serde(default)]
    pub on_dirty: OnDirty,
}

/// The dirty policy when commit mode is on, `None` when it is off.
pub fn per_write(config: Option<&GitConfig>) -> Option<OnDirty> {
    config
        .filter(|c| c.commit == CommitMode::PerWrite)
        .map(|c| c.on_dirty)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    /// Written; the rename has not happened.
    Prepared,
    /// The candidate is in the tree, durably.
    Applied,
    /// A skipped or ineligible write is in the tree; only its receipt is owed.
    Landed,
    /// Git ran, but inspecting HEAD or the index failed.
    Unverified,
    /// HEAD and the index both carry the candidate.
    Committed,
    /// Rivet cannot finish safely; `repair` says why.
    RepairRequired,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// A record's intent only: the write is to be committed.
    Commit,
    Committed,
    UnchangedInGit,
    Skipped,
    Ineligible,
    Failed,
    Abandoned,
    NotApplied,
}

impl Outcome {
    pub fn terminal(self) -> bool {
        !matches!(self, Outcome::Commit | Outcome::Failed)
    }

    fn name(self) -> &'static str {
        match self {
            Outcome::Commit => "commit",
            Outcome::Committed => "committed",
            Outcome::UnchangedInGit => "unchanged_in_git",
            Outcome::Skipped => "skipped",
            Outcome::Ineligible => "ineligible",
            Outcome::Failed => "failed",
            Outcome::Abandoned => "abandoned",
            Outcome::NotApplied => "not_applied",
        }
    }
}

/// Whether an outcome's write is in the tree: `true`, `false`, or
/// `"unknown"` when the file is neither the bytes read nor the candidate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Applied {
    Yes,
    No,
    Unknown,
}

impl Serialize for Applied {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Applied::Yes => s.serialize_bool(true),
            Applied::No => s.serialize_bool(false),
            Applied::Unknown => s.serialize_str("unknown"),
        }
    }
}

impl<'de> Deserialize<'de> for Applied {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        match Value::deserialize(d)? {
            Value::Bool(true) => Ok(Applied::Yes),
            Value::Bool(false) => Ok(Applied::No),
            Value::String(s) if s == "unknown" => Ok(Applied::Unknown),
            other => Err(serde::de::Error::custom(format!(
                "applied must be true, false or \"unknown\", not {other}"
            ))),
        }
    }
}

/// The write phase is independent of a recovery blocker. Legacy blocked records
/// have no recorded phase; their `ever_applied` evidence remains authoritative.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Prepared,
    Applied,
    Landed,
    Unverified,
    Committed,
}

#[derive(Debug, Clone)]
pub enum Trailer {
    Delete(crate::delete_receipt::DeleteReceipt),
    Archive(crate::archive::CopyReceipt),
    Move(Box<crate::relocate::MoveReceipt>),
}

#[derive(Debug, Clone)]
pub enum Guard {
    Archive(crate::archive::ArchiveGuard),
    Policy(String),
}

/// A pending record: typed transaction evidence plus shared bookkeeping.
/// Only StoredRecord defines its backwards-compatible on-disk representation.
#[derive(Debug, Clone, Deserialize)]
#[serde(from = "StoredRecord")]
pub struct Record {
    pub trailers: Vec<Trailer>,
    pub archive_parent: Option<String>,
    pub guards: Vec<Guard>,
    phase: Option<Phase>,
    blocked: bool,
    data: RecordData,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecordData {
    /// Validated caller text, kept separately so recovery can rebind trailers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_message: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub require_commit: bool,
    pub op: String,
    pub seq: u64,
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peer: Option<Peer>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fence: Option<git::Fence>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub fence_waived: bool,
    /// The intended outcome (`commit`, `skipped`, `ineligible`); a `landed`
    /// record carries its final one.
    pub outcome: Outcome,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// The dirty units a `skipped: dirty` write found.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub units: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    /// SHA-256 of the bytes read under the lock, and of the candidate. Empty
    /// for a creation, which read nothing: no file hashes to it.
    pub read_sha256: String,
    pub candidate_sha256: String,
    /// Git fields: whatever preflight read. A commit-intent record has all.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_head: Option<String>,
    /// Guarded adopt may commit only with this certified parent, even in recovery.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expect_head: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<Entry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub candidate: Option<Entry>,
    /// The whole commit message, trailers included.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// Adopted outside changes, one per `Includes-outside-edit:` trailer.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub provenance: Vec<String>,
    /// The git child (the launch helper's PID, which git keeps) and when it
    /// was spawned, in nanoseconds since the epoch. No PID: git never ran.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spawned_ns: Option<u64>,
    /// Absent in legacy records, which retain PID-only liveness checks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub child_identity: Option<git::ProcessIdentity>,
    /// Stable acquisition evidence while the real index lock spans Git children.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub index_lease: Option<git::IndexLease>,
    /// The record reached `applied` or `landed` at some point.
    #[serde(default)]
    pub ever_applied: bool,
    /// The path is not tracked at the base: no base entry. A `create` (`read_sha256` empty) or a `markrivet
    /// adopt` of an untracked file.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub creation: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha: Option<String>,
    #[serde(default)]
    pub retries: u32,
    #[serde(default)]
    pub git_ms: u64,
    /// Recovery detail can outlive a blocker, as in legacy committed records.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repair: Option<Value>,
    pub created: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Peer {
    pub path: String,
    pub read_sha256: String,
    pub candidate_sha256: String,
    pub base: Entry,
    pub candidate: Entry,
}

impl Record {
    pub fn paths(&self) -> impl Iterator<Item = &str> {
        std::iter::once(self.path.as_str()).chain(self.peer.iter().map(|p| p.path.as_str()))
    }

    fn entries(&self) -> Vec<(&str, Option<&Entry>, Option<&Entry>)> {
        std::iter::once((
            self.path.as_str(),
            self.base.as_ref(),
            self.candidate.as_ref(),
        ))
        .chain(
            self.peer
                .iter()
                .map(|p| (p.path.as_str(), Some(&p.base), Some(&p.candidate))),
        )
        .collect()
    }

    fn move_receipt(&self) -> Option<&crate::relocate::MoveReceipt> {
        self.trailers.iter().find_map(|t| match t {
            Trailer::Move(r) => Some(r.as_ref()),
            _ => None,
        })
    }
}
#[path = "commit/pair.rs"]
mod pair;
pub(crate) use pair::{PairFile, land_pair, preflight_pair};

impl std::ops::Deref for Record {
    type Target = RecordData;
    fn deref(&self) -> &RecordData {
        &self.data
    }
}
impl std::ops::DerefMut for Record {
    fn deref_mut(&mut self) -> &mut RecordData {
        &mut self.data
    }
}

/// Compatibility boundary: keep every field name, omission and enum value used
/// by the installed engine. New trailer/guard kinds need an explicit wire decision.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredRecord {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    archive_parent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    archive_copy: Option<crate::archive::CopyReceipt>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    move_receipt: Option<Box<crate::relocate::MoveReceipt>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    archive: Option<crate::archive::ArchiveGuard>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    delete_receipt: Option<crate::delete_receipt::DeleteReceipt>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    policy_guard: Option<String>,
    state: State,
    #[serde(flatten)]
    data: RecordData,
}

impl From<StoredRecord> for Record {
    fn from(stored: StoredRecord) -> Self {
        let mut trailers = Vec::new();
        trailers.extend(stored.delete_receipt.map(Trailer::Delete));
        trailers.extend(stored.archive_copy.map(Trailer::Archive));
        trailers.extend(stored.move_receipt.map(Trailer::Move));
        let mut guards = Vec::new();
        guards.extend(stored.policy_guard.map(Guard::Policy));
        guards.extend(stored.archive.map(Guard::Archive));
        let phase = match stored.state {
            State::Prepared => Some(Phase::Prepared),
            State::Applied => Some(Phase::Applied),
            State::Landed => Some(Phase::Landed),
            State::Unverified => Some(Phase::Unverified),
            State::Committed => Some(Phase::Committed),
            State::RepairRequired => None,
        };
        Self {
            trailers,
            archive_parent: stored.archive_parent,
            guards,
            phase,
            blocked: stored.state == State::RepairRequired,
            data: stored.data,
        }
    }
}

impl TryFrom<Record> for StoredRecord {
    type Error = &'static str;

    fn try_from(rec: Record) -> Result<Self, Self::Error> {
        // The legacy format has one slot per kind. Refuse an unrepresentable
        // record rather than silently dropping evidence while serializing it.
        fn put<T>(slot: &mut Option<T>, value: T) -> Result<(), &'static str> {
            if slot.replace(value).is_some() {
                return Err("duplicate trailer or guard kind in pending record");
            }
            Ok(())
        }
        let (mut archive_copy, mut delete_receipt, mut move_receipt) = (None, None, None);
        for trailer in &rec.trailers {
            match trailer {
                Trailer::Delete(r) => put(&mut delete_receipt, r.clone())?,
                Trailer::Archive(r) => put(&mut archive_copy, r.clone())?,
                Trailer::Move(r) => put(&mut move_receipt, r.clone())?,
            }
        }
        let (mut archive, mut policy_guard) = (None, None);
        for guard in &rec.guards {
            match guard {
                Guard::Archive(g) => put(&mut archive, g.clone())?,
                Guard::Policy(g) => put(&mut policy_guard, g.clone())?,
            }
        }
        Ok(Self {
            archive_parent: rec.archive_parent.clone(),
            archive_copy,
            move_receipt,
            archive,
            delete_receipt,
            policy_guard,
            state: rec.state(),
            data: rec.data,
        })
    }
}

impl Serialize for Record {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        StoredRecord::try_from(self.clone())
            .map_err(serde::ser::Error::custom)?
            .serialize(serializer)
    }
}

impl Record {
    fn state(&self) -> State {
        if self.blocked {
            return State::RepairRequired;
        }
        match self.phase {
            Some(Phase::Prepared) => State::Prepared,
            Some(Phase::Applied) => State::Applied,
            Some(Phase::Landed) => State::Landed,
            Some(Phase::Unverified) => State::Unverified,
            Some(Phase::Committed) => State::Committed,
            None => State::RepairRequired,
        }
    }

    fn repair(&self) -> Option<&Value> {
        self.data.repair.as_ref()
    }

    fn delete_receipt(&self) -> Option<&crate::delete_receipt::DeleteReceipt> {
        self.trailers.iter().find_map(|t| match t {
            Trailer::Delete(r) => Some(r),
            _ => None,
        })
    }

    fn archive_copy(&self) -> Option<&crate::archive::CopyReceipt> {
        self.trailers.iter().find_map(|t| match t {
            Trailer::Archive(r) => Some(r),
            _ => None,
        })
    }

    fn compose(&self, user: Option<&str>) -> Result<String, Diagnostic> {
        let mut message = compose(user, &self.path, &self.op, &self.provenance);
        for trailer in &self.trailers {
            message.push_str(&trailer.line()?);
            message.push('\n');
        }
        Ok(message)
    }

    fn check_guards(
        &self,
        project: &Project,
        lock: &WriterLock,
        source: &str,
        candidate: &str,
    ) -> Result<(), Diagnostic> {
        for trailer in &self.trailers {
            if let Trailer::Archive(copy) = trailer {
                crate::archive::check_source(
                    project,
                    &copy.copy,
                    self.archive_parent.as_deref(),
                    Some(candidate),
                )?;
            }
        }
        for guard in &self.guards {
            guard.check(project, lock, self, source, false)?;
        }
        if let Some(receipt) = self.delete_receipt() {
            crate::delete_receipt::check_list_citations(
                project,
                &self.path,
                &crate::Document::parse(source)?,
                candidate,
                &receipt.operation,
                &crate::fs::WalkFaults::default(),
            )?;
        }
        Ok(())
    }

    fn child(&self) -> GitChild {
        GitChild {
            pid: self.pid,
            identity: self.child_identity.clone(),
            spawned: UNIX_EPOCH + Duration::from_nanos(self.spawned_ns.unwrap_or(0)),
        }
    }

    fn commits(&self) -> bool {
        self.outcome == Outcome::Commit
    }

    fn msg_name(&self) -> String {
        format!("{}.msg", self.op)
    }

    /// A `create`: there was no file to read.
    fn fresh(&self) -> bool {
        self.creation && self.read_sha256.is_empty()
    }
}

impl Trailer {
    fn key(&self) -> &'static str {
        match self {
            Self::Delete(_) => "rivet-delete",
            Self::Archive(_) => "rivet-archive",
            Self::Move(_) => "rivet-move",
        }
    }

    fn mismatch(&self) -> &'static str {
        match self {
            Self::Delete(_) => "delete_receipt_mismatch",
            Self::Archive(_) => "archive_receipt_mismatch",
            Self::Move(_) => "move_receipt_mismatch",
        }
    }

    fn line(&self) -> Result<String, Diagnostic> {
        match self {
            Self::Delete(r) => r.encode().map(|payload| format!("Rivet-Delete: {payload}")),
            Self::Move(r) => r.encode().map(|payload| format!("Rivet-Move: {payload}")),
            Self::Archive(r) => r
                .encode()
                .map(|payload| format!("Rivet-Archive: {payload}")),
        }
    }

    fn identifies(&self, rec: &Record, parent: &str) -> bool {
        match self {
            Self::Delete(r) => {
                r.parent == parent
                    && r.op == rec.op
                    && r.path == rec.path
                    && Some(&r.base) == rec.base.as_ref()
                    && Some(&r.candidate) == rec.candidate.as_ref()
                    && r.read_sha256 == rec.read_sha256
                    && r.candidate_sha256 == rec.candidate_sha256
            }
            Self::Archive(r) => r.op == rec.op && r.copy.path == rec.path,
            Self::Move(r) => r.identifies(rec),
        }
    }

    fn matches(&self, rec: &Record, parent: &str, message: &str) -> bool {
        let Ok(line) = self.line() else {
            return false;
        };
        let actual: Vec<_> = message
            .lines()
            .filter(|l| reserved_key(l) == Some(self.key()))
            .collect();
        actual == [line] && self.identifies(rec, parent)
    }

    fn rebind(
        &self,
        parent: &str,
        base: Option<&Entry>,
        candidate: Option<&Entry>,
    ) -> Result<Self, String> {
        Ok(match self {
            Self::Delete(old) => {
                let mut receipt = old.clone();
                receipt.parent = parent.into();
                receipt.base = base.cloned().ok_or("missing checked base")?;
                receipt.candidate = candidate.cloned().ok_or("missing candidate")?;
                Self::Delete(receipt)
            }
            Self::Archive(_) | Self::Move(_) => self.clone(),
        })
    }

    fn check_recovery(
        &self,
        project: &Project,
        lock: &WriterLock,
        git: &Git,
        rec: &Record,
        file: &FileState,
    ) -> Result<(), (&'static str, String)> {
        match self {
            Self::Move(receipt) => receipt.check_recovery(project),
            Self::Archive(copy) => crate::archive::check_source(
                project,
                &copy.copy,
                rec.archive_parent.as_deref(),
                match file {
                    FileState::Present { bytes, .. } => std::str::from_utf8(bytes).ok(),
                    _ => None,
                },
            )
            .map_err(|e| ("archive_source_changed", e.detail())),
            Self::Delete(old) => (|| {
                if !self.identifies(rec, rec.base_head.as_deref().unwrap_or_default()) {
                    return Err("record and typed delete receipt disagree".into());
                }
                let base = git
                    .inspect(&["cat-file", "blob", &old.base.oid], None)
                    .map_err(|e| e.to_string())?;
                if !base.ok() {
                    return Err(base.failure("git cat-file"));
                }
                let parent = String::from_utf8(base.stdout).map_err(|e| e.to_string())?;
                let candidate = match file {
                    FileState::Present { bytes, .. } => {
                        String::from_utf8(bytes.clone()).map_err(|e| e.to_string())?
                    }
                    _ => return Err("candidate missing".into()),
                };
                crate::delete_receipt::verify(&crate::delete_receipt::VerifyRequest {
                    parent: parent.clone(),
                    candidate: candidate.clone(),
                    receipt: old.clone(),
                })
                .map_err(Diagnostic::detail)?;
                crate::delete_receipt::check_list_citations(
                    project,
                    &rec.path,
                    &crate::Document::parse(&parent).map_err(Diagnostic::detail)?,
                    &candidate,
                    &old.operation,
                    &crate::fs::WalkFaults::default(),
                )
                .map_err(Diagnostic::detail)?;
                for guard in &rec.guards {
                    guard
                        .check(project, lock, rec, &parent, true)
                        .map_err(Diagnostic::detail)?;
                }
                Ok(())
            })()
            .map_err(|e| ("delete_receipt_rebind", e)),
        }
    }
}

impl Guard {
    fn check(
        &self,
        project: &Project,
        lock: &WriterLock,
        rec: &Record,
        parent: &str,
        recovering: bool,
    ) -> Result<(), Diagnostic> {
        match self {
            Self::Policy(expected) => {
                crate::prune_snapshot::check(
                    project,
                    lock,
                    expected,
                    recovering.then_some((&rec.path, parent)),
                )?;
            }
            Self::Archive(archive) => {
                let receipt = rec.delete_receipt().ok_or_else(|| {
                    Diagnostic::new(
                        ErrorCode::InvalidRequest,
                        "archive requires a single delete",
                    )
                })?;
                let crate::delete_receipt::DeleteIntent::Delete {
                    select,
                    guard,
                    list,
                    ..
                } = &receipt.operation;
                let doc = crate::Document::parse(parent)?;
                crate::archive::check_guard(
                    project,
                    archive,
                    &rec.path,
                    &doc,
                    select,
                    guard,
                    list.as_ref(),
                )?;
            }
        }
        Ok(())
    }
}

impl Record {
    fn lock_child(&self, file: &str, identity: &str) -> GitChild {
        let mut child = self.child();
        if let Some(lease) = &self.index_lease
            && lease.matches(file, identity)
        {
            child.spawned = child
                .spawned
                .min(UNIX_EPOCH + Duration::from_nanos(lease.acquired_ns));
        }
        child
    }
}

/// One outcome event in `.rivet/commits.jsonl`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archive_copy: Option<crate::archive::CopyReceipt>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub move_receipt: Option<crate::relocate::MoveReceipt>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peer_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peer_applied: Option<Applied>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delete_receipt: Option<crate::delete_receipt::DeleteReceipt>,
    pub op: String,
    pub seq: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    pub path: String,
    pub outcome: Outcome,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub applied: Applied,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha: Option<String>,
    /// For `failed`: whether the commit reached HEAD.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub published: Option<Value>,
    #[serde(default)]
    pub retries: u32,
    #[serde(default)]
    pub git_ms: u64,
    #[serde(default)]
    pub recovered: bool,
    pub at: u64,
}

/// The receipt log as readers see it: whole lines only, in file order.
#[derive(Debug, Default)]
pub struct Receipts {
    pub events: Vec<Event>,
    /// Whole lines that did not parse as an event.
    pub unreadable: usize,
}

impl Receipts {
    pub fn read(project: &Project) -> Result<Receipts, Diagnostic> {
        let bytes = project.log_read(RECEIPTS)?;
        let mut receipts = Receipts::default();
        let whole = match bytes.iter().rposition(|b| *b == b'\n') {
            Some(i) => &bytes[..=i],
            // A partial last line is ignored.
            None => &[][..],
        };
        for line in whole.split(|b| *b == b'\n').filter(|l| !l.is_empty()) {
            match serde_json::from_slice::<Event>(line) {
                Ok(e) => receipts.events.push(e),
                Err(_) => receipts.unreadable += 1,
            }
        }
        Ok(receipts)
    }

    pub fn terminal(&self, op: &str) -> Option<&Event> {
        self.events
            .iter()
            .find(|e| e.op == op && e.outcome.terminal())
    }

    /// Recovery needs only selected ops' terminal events. Valid logs carry one
    /// terminal per op; if duplicated, use the newest, as for latest outcomes.
    /// Missing receipts require reaching the beginning, never a fixed tail window.
    fn for_pending<'a>(
        project: &Project,
        ops: impl IntoIterator<Item = &'a str>,
    ) -> Result<Self, Diagnostic> {
        let mut wanted: BTreeSet<&str> = ops.into_iter().collect();
        let mut receipts = Self::default();
        if wanted.is_empty() {
            return Ok(receipts);
        }
        project.log_visit_reverse(RECEIPTS, |line| {
            if !line.is_empty() {
                match serde_json::from_slice::<Event>(line) {
                    Ok(e) if e.outcome.terminal() && wanted.remove(e.op.as_str()) => {
                        receipts.events.push(e);
                    }
                    Ok(_) => {}
                    Err(_) => receipts.unreadable += 1,
                }
            }
            !wanted.is_empty()
        })?;
        Ok(receipts)
    }

    /// Each op's latest event, which is its outcome.
    pub fn latest(&self) -> BTreeMap<&str, &Event> {
        let mut out = BTreeMap::new();
        for e in &self.events {
            out.insert(e.op.as_str(), e);
        }
        out
    }

    pub fn last_writes(&self) -> BTreeMap<&str, &Event> {
        let mut out: BTreeMap<&str, &Event> = BTreeMap::new();
        for e in self.latest().into_values() {
            for path in std::iter::once((e.path.as_str(), e.applied))
                .chain(e.peer_path.as_deref().zip(e.peer_applied))
                .filter_map(|(p, applied)| (applied == Applied::Yes).then_some(p))
            {
                if out.get(path).is_none_or(|previous| previous.seq <= e.seq) {
                    out.insert(path, e);
                }
            }
        }
        out
    }
}

pub(crate) fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn now_ns(at: SystemTime) -> u64 {
    at.duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as u64)
}

fn random_op() -> String {
    use std::io::Read;
    let mut buf = [0u8; 8];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut buf))
        .expect("the OS random source is readable");
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

/// A line that git's trailer parser would read as one of Rivet's trailer
/// keys: the key case-insensitively, optional whitespace, then `:` or `=`.
pub(crate) fn reserved_key(line: &str) -> Option<&'static str> {
    let lower = line.trim_start().to_ascii_lowercase();
    RESERVED.into_iter().find(|key| {
        lower
            .strip_prefix(key)
            .is_some_and(|rest| rest.trim_start().starts_with([':', '=']))
    })
}

/// Refuse a caller's message that carries a line in Rivet's trailer keys.
pub fn check_message(message: &str) -> Result<(), Diagnostic> {
    if let Some(line) = message.lines().find(|l| reserved_key(l).is_some()) {
        return Err(Diagnostic::new(
            ErrorCode::InvalidRequest,
            format!("message line {line:?} uses a trailer key Rivet reserves"),
        ));
    }
    Ok(())
}

/// The values of every `Rivet-Op` trailer line in a commit message.
pub(crate) fn op_trailers(message: &str) -> Vec<String> {
    message
        .lines()
        .filter(|l| reserved_key(l) == Some("rivet-op"))
        .map(|l| {
            let rest = &l.trim_start()["rivet-op".len()..];
            rest.trim_start()[1..].trim().to_string()
        })
        .collect()
}

pub(crate) fn escaped_path(path: &str) -> String {
    path.chars()
        .map(|c| {
            if c.is_control() {
                c.escape_default().to_string()
            } else {
                c.to_string()
            }
        })
        .collect()
}

fn compose(message: Option<&str>, path: &str, op: &str, provenance: &[String]) -> String {
    // A path may hold control characters; the default subject escapes them
    // so no path can add a line, a trailer among them.
    let subject = match message.map(str::trim_end) {
        Some(m) if !m.trim().is_empty() => m.to_string(),
        _ => format!("rivet: {}", escaped_path(path)),
    };
    let mut text = format!("{subject}\n\nRivet-Op: {op}\n");
    for p in provenance {
        text.push_str(&format!("Includes-outside-edit: {p}\n"));
    }
    text
}

fn publish(project: &Project, lock: &WriterLock, rec: &Record) -> Result<(), Diagnostic> {
    crate::fs::hook("record_publish")?;
    crate::fs::fault("record_publish")
        .map_err(|e| Diagnostic::new(ErrorCode::Io, e.to_string()))?;
    let bytes = serde_json::to_vec_pretty(rec)
        .map_err(|e| Diagnostic::new(ErrorCode::InvalidRequest, e.to_string()))?;
    if bytes.len() as u64 > MAX_RECORD_BYTES {
        return Err(Diagnostic::new(
            ErrorCode::LimitExceeded,
            "pending record exceeds the 40 MiB recoverable bookkeeping limit",
        ));
    }
    project.store_put(lock, Some(PENDING), &format!("{}.json", rec.op), &bytes)
}

fn retire(project: &Project, lock: &WriterLock, rec: &Record) -> Result<(), Diagnostic> {
    crate::fs::fault("record_retire")
        .map_err(|e| Diagnostic::new(ErrorCode::Io, format!(".rivet/pending: {e}")))?;
    let mut names = vec![format!("{}.json", rec.op), rec.msg_name()];
    for purpose in ["tree", "sync"] {
        let name = format!(
            "index-{}",
            sha256(format!("{}:{purpose}", rec.op).as_bytes())
        );
        names.push(format!("{name}.lock"));
        names.push(name);
    }
    project.store_remove(
        lock,
        PENDING,
        &names.iter().map(String::as_str).collect::<Vec<_>>(),
    )
}

/// Every pending record, in sequence order, and a diagnostic for each file
/// that does not parse. A nonregular .json entry makes ownership unknown.
pub fn records(project: &Project) -> Result<(Vec<Record>, Vec<Diagnostic>), Diagnostic> {
    let mut found = vec![];
    let mut errors = vec![];
    let files = project.store_list_bounded_strict(PENDING, ".json", MAX_RECORD_BYTES)?;
    for (name, bytes) in files {
        match serde_json::from_slice::<Record>(&bytes) {
            Ok(r) => found.push(r),
            Err(e) => errors.push(Diagnostic::new(
                ErrorCode::Io,
                format!(".rivet/pending/{name} is not a pending record: {e}"),
            )),
        }
    }
    found.sort_by_key(|r| r.seq);
    Ok((found, errors))
}

/// Ownership checks have one fail-closed read: unreadable, oversized and
/// nonregular records are as unknown as a regular record that fails to parse.
pub(crate) fn pending_for_ownership(project: &Project) -> Result<Vec<Record>, Diagnostic> {
    let unknown = |error: Diagnostic| {
        Diagnostic::new(
            ErrorCode::PendingUnresolved,
            format!(
                "cannot establish pending-record ownership: {}",
                error.detail()
            ),
        )
    };
    let (pending, errors) = records(project).map_err(unknown)?;
    if let Some(error) = errors.into_iter().next() {
        return Err(unknown(error));
    }
    Ok(pending)
}

fn event(rec: &Record, outcome: Outcome, applied: Applied, recovered: bool) -> Event {
    Event {
        archive_copy: rec.archive_copy().cloned(),
        move_receipt: rec.move_receipt().cloned(),
        peer_path: rec.peer.as_ref().map(|p| p.path.clone()),
        peer_applied: rec.peer.as_ref().map(|_| applied),
        delete_receipt: rec.delete_receipt().cloned(),
        op: rec.op.clone(),
        seq: rec.seq,
        session: rec.session.clone(),
        path: rec.path.clone(),
        outcome,
        reason: rec.reason.clone(),
        applied,
        sha: rec.sha.clone(),
        published: None,
        retries: rec.retries,
        git_ms: rec.git_ms,
        recovered,
        at: git::now_millis(),
    }
}

fn append(project: &Project, lock: &WriterLock, e: &Event) -> Result<(), LogFailure> {
    let line = serde_json::to_vec(e).expect("events serialise");
    project.log_append(lock, RECEIPTS, &line)
}

/// Receipt durability and record cleanup are distinct conclusions.
enum Concluded {
    Retired,
    RecordLeft(Diagnostic),
    ReceiptFailed(Diagnostic),
}

impl Concluded {
    fn record_left(&self) -> bool {
        matches!(self, Self::RecordLeft(_))
    }
    fn receipt_failed(&self) -> bool {
        matches!(self, Self::ReceiptFailed(_))
    }
    fn pending(&self) -> bool {
        !matches!(self, Self::Retired)
    }
    fn mark_left(&self, v: &mut Value) {
        if self.record_left() {
            v["record_left"] = json!(true);
        }
    }
}

/// Append a terminal event, fsynced, then remove the record. Recovery never
/// appends a second terminal event for an operation with a durable receipt.
fn conclude(project: &Project, lock: &WriterLock, rec: &Record, e: &Event) -> Concluded {
    if let Err(f) = append(project, lock, e) {
        return Concluded::ReceiptFailed(f.diagnostic().clone());
    }
    let _ = hook("finalise");
    match retire(project, lock, rec) {
        Ok(()) => Concluded::Retired,
        Err(error) => Concluded::RecordLeft(error),
    }
}

/// What a write sends back for its commit.
pub struct Landed {
    pub durable: bool,
    pub commit: Value,
    /// The receipt could not be appended; the record stays for recovery.
    pub receipt_failed: bool,
}

/// Bind a commit response to its retained record and journal events.
pub(crate) fn with_op(op: &str, mut commit: Value) -> Value {
    commit["op"] = json!(op);
    commit
}

/// A refusal, and the op of a record it left behind, if any.
pub type Refusal = (Diagnostic, Option<String>);

/// One commit-mode write, from preflight on.
pub struct Write<'a> {
    pub archive_copy: Option<crate::archive::CopyIntent>,
    pub archive_parent: Option<String>,
    pub archive: Option<crate::archive::ArchiveGuard>,
    pub delete_intent: Option<crate::delete_receipt::DeleteIntent>,
    pub require_commit: bool,
    pub policy_guard: Option<String>,
    pub project: &'a Project,
    pub lock: &'a WriterLock,
    pub path: &'a str,
    pub on_dirty: OnDirty,
    pub session: Option<String>,
    pub message: Option<String>,
    pub bound: Vec<Bound>,
    /// `markrivet adopt`: commit the whole file or write nothing. An ineligible
    /// path is refused rather than landed, and a clean file with nothing to
    /// stamp answers with no commit at all.
    pub adopting: bool,
    pub expect_head: Option<&'a str>,
}

impl<'a> Write<'a> {
    /// Neutral construction only. Operation-specific trailer and guard
    /// combinations are validated by landing, not by this constructor.
    pub fn new(
        project: &'a Project,
        lock: &'a WriterLock,
        path: &'a str,
        on_dirty: OnDirty,
    ) -> Self {
        Self {
            project,
            lock,
            path,
            on_dirty,
            session: None,
            message: None,
            bound: vec![],
            adopting: false,
            expect_head: None,
            require_commit: false,
            policy_guard: None,
            archive: None,
            archive_copy: None,
            archive_parent: None,
            delete_intent: None,
        }
    }
}

/// A write to `path` meets any unresolved record for it first: recovery
/// runs for that path's records, and if one remains the write is refused
/// under every policy.
pub fn settle(project: &Project, lock: &WriterLock, path: &str) -> Result<(), Diagnostic> {
    let pending = pending_for_ownership(project)?;
    if !pending.iter().any(|r| r.paths().any(|p| p == path)) {
        return Ok(());
    }
    let report = recover(project, lock, |r| r.paths().any(|p| p == path))?;
    let left = pending_for_ownership(project)?;
    if let Some(r) = left.iter().find(|r| r.paths().any(|p| p == path)) {
        let why = report
            .iter()
            .find(|v| v["op"] == r.op.as_str())
            .cloned()
            .unwrap_or(Value::Null);
        return Err(Diagnostic::new(
            ErrorCode::PendingUnresolved,
            format!(
                "{path} has an unresolved pending record {} ({}); run `markrivet commit-pending` or abandon it: {why}",
                r.op,
                serde_json::to_value(r.state()).expect("state"),
            ),
        )
        .with_path(path));
    }
    Ok(())
}

pub(crate) fn pair_barrier(
    project: &Project,
    lock: Option<&WriterLock>,
    path: Option<&str>,
    may_settle: bool,
) -> Result<(), Diagnostic> {
    let pending = pending_for_ownership(project)?;
    let selected: Vec<_> = pending
        .iter()
        .filter(|r| {
            r.peer.is_some() && (path.is_none_or(|p| r.paths().any(|x| x == p)) || !r.fence_waived)
        })
        .map(|r| r.op.clone())
        .collect();
    if selected.is_empty() {
        return Ok(());
    }
    if may_settle && let Some(lock) = lock {
        recover(project, lock, |r| selected.contains(&r.op))?;
        let left = pending_for_ownership(project)?;
        if !left.iter().any(|r| selected.contains(&r.op)) {
            return Ok(());
        }
    }
    Err(Diagnostic::new(
        ErrorCode::PendingUnresolved,
        "a two-path move owns this document or the git index; run commit-pending",
    ))
}

fn no_pending_restore_peer(project: &Project, path: &str, own_op: &str) -> Result<(), Diagnostic> {
    let records = pending_for_ownership(project)?;
    if records
        .iter()
        .any(|r| r.paths().any(|p| p == path) && r.op != own_op)
    {
        return Err(Diagnostic::new(
            ErrorCode::PendingUnresolved,
            "pending ownership changed during restore",
        ));
    }
    Ok(())
}

/// Steps 2 to 8 of a commit-mode write whose candidate differs from the
/// bytes read: preflight, the pending record, the rename, and the commit.
pub fn land(w: &Write, source: &str, candidate: &str) -> Result<Landed, Refusal> {
    land_with(w, Some(source), candidate, None)
}

pub(crate) fn land_restore(
    w: &Write,
    source: &str,
    candidate: &str,
    assertion: &crate::history::Assertion,
) -> Result<Landed, Refusal> {
    land_with(w, Some(source), candidate, Some(assertion))
}

/// The creation variant: the base is absence, publication never replaces
/// a file, and the commit is preceded by `git add`.
pub fn create(w: &Write, candidate: &str) -> Result<Landed, Refusal> {
    land_with(w, None, candidate, None)
}

fn land_with(
    w: &Write,
    source: Option<&str>,
    candidate: &str,
    restore: Option<&crate::history::Assertion>,
) -> Result<Landed, Refusal> {
    let (project, lock, path) = (w.project, w.lock, w.path);
    if ((w.archive.is_some() || w.policy_guard.is_some()) && w.delete_intent.is_none())
        || ((w.archive.is_some() || w.policy_guard.is_some() || w.archive_copy.is_some())
            && !w.require_commit)
    {
        return Err((
            Diagnostic::new(
                ErrorCode::InvalidRequest,
                "archive and policy preconditions need typed intent and require_commit",
            ),
            None,
        ));
    }
    let creation = source.is_none();
    let file = match (project.read_raw(path), source) {
        (Err(e), _) => return Err((e, None)),
        (Ok(None), None) => FileState::Missing,
        (Ok(Some(_)), None) => {
            return Err((
                Diagnostic::new(ErrorCode::AlreadyExists, format!("{path:?} already exists")),
                None,
            ));
        }
        (Ok(None), Some(_)) => {
            return Err((
                Diagnostic::new(ErrorCode::NotFound, format!("{path:?} does not exist")),
                None,
            ));
        }
        (Ok(Some((bytes, mode))), Some(source)) => {
            if bytes != source.as_bytes() {
                return Err((
                    Diagnostic::new(
                        ErrorCode::ExternalChange,
                        "the file changed outside Rivet while this write was prepared; nothing written",
                    ),
                    None,
                ));
            }
            FileState::Present { bytes, mode }
        }
    };
    let read_sha256 = source.map_or(String::new(), |s| sha256(s.as_bytes()));
    let source = source.unwrap_or("");
    let repo = Repo::inspect(project);
    let mut pf = git::preflight(&repo, path, &file, creation);
    let mut creation = creation;
    // `markrivet adopt` adds a file nothing tracks yet, such as one a `create`
    // left uncommitted, as a creation of its present bytes.
    if w.adopting
        && pf
            .ineligible
            .as_ref()
            .is_some_and(|i| i.reason == "untracked")
        && pf.index.is_empty()
    {
        pf = git::preflight(&repo, path, &file, git::Creation::Untracked);
        creation = true;
    }
    if let Some(expected) = w.expect_head {
        if pf.head.as_deref() != Some(expected) {
            return Err((Diagnostic::new(ErrorCode::GuardConflict,
                format!("HEAD is not the certified commit (expected {expected}, found {}); nothing written",
                    pf.head.as_deref().unwrap_or("unborn"))).with_path(path), None));
        }
        if repo.file_mode
            && let Some(head) = &pf.head_entry
            && file.git_mode() != Some(head.mode.as_str())
        {
            return Err((
                Diagnostic::new(
                    ErrorCode::GuardConflict,
                    "file mode is not the certified HEAD mode; nothing written",
                )
                .with_path(path),
                None,
            ));
        }
    }
    if let Some(assertion) = restore {
        assertion
            .check(project, pf.head.as_deref(), pf.branch.as_deref())
            .map_err(|e| (e, None))?;
        hook("history_preflight").map_err(|e| (e, None))?;
    }
    let mut ineligible = pf.ineligible.clone();
    if ineligible.is_none()
        && let Some(lock_file) = git::lingering_lock(project, pf.branch.as_deref())
    {
        ineligible = Some(Ineligible {
            reason: "git_locked",
            detail: format!("{lock_file} is present"),
        });
    }
    let decision = git::decide(pf.base, ineligible.as_ref(), w.on_dirty);
    if let Decision::Refuse(base) = decision {
        return Err((
            Diagnostic::new(
                ErrorCode::CommitRefused,
                format!(
                    "on_dirty is refuse and the file's base state is {}; nothing written",
                    serde_json::to_value(base).expect("base")
                ),
            ),
            None,
        ));
    }
    if w.adopting {
        match &decision {
            Decision::Ineligible(i) => {
                return Err((
                    Diagnostic::new(
                        ErrorCode::CommitRefused,
                        format!(
                            "{path} cannot be committed ({}: {}); nothing written",
                            i.reason, i.detail
                        ),
                    ),
                    None,
                ));
            }
            Decision::Commit if candidate == source && pf.base == git::Base::KnownClean => {
                return Ok(Landed {
                    durable: true,
                    commit: Value::Null,
                    receipt_failed: false,
                });
            }
            _ => {}
        }
    }
    if w.require_commit && (w.on_dirty != OnDirty::Refuse || decision != Decision::Commit) {
        return Err((
            Diagnostic::new(
                ErrorCode::CommitRefused,
                "require_commit needs an eligible clean commit with on_dirty refuse; nothing written",
            ),
            None,
        ));
    }
    let seq = project.next_seq(lock).map_err(|e| (e, None))?;
    let mut rec = Record {
        trailers: vec![],
        archive_parent: w.archive_parent.clone(),
        guards: w
            .policy_guard
            .clone()
            .map(Guard::Policy)
            .into_iter()
            .chain(w.archive.clone().map(Guard::Archive))
            .collect(),
        phase: Some(Phase::Prepared),
        blocked: false,
        data: RecordData {
            user_message: None,
            require_commit: w.require_commit,
            op: random_op(),
            seq,
            path: path.to_string(),
            peer: None,
            fence: None,
            fence_waived: false,
            outcome: Outcome::Commit,
            reason: None,
            detail: None,
            units: vec![],
            session: w.session.clone(),
            read_sha256,
            candidate_sha256: sha256(candidate.as_bytes()),
            branch: pf.branch.clone(),
            base_head: pf.head.clone(),
            expect_head: w.expect_head.map(str::to_owned),
            base: pf.head_entry.clone(),
            candidate: None,
            message: None,
            provenance: vec![],
            pid: None,
            spawned_ns: None,
            child_identity: None,
            index_lease: None,
            ever_applied: false,
            creation,
            sha: None,
            retries: 0,
            git_ms: 0,
            repair: None,
            created: git::now_millis(),
        },
    };
    let receipt_eligible = decision == Decision::Commit;
    let bound: Vec<&Bound> = w.bound.iter().collect();
    match decision {
        Decision::Refuse(_) => unreachable!("refused above"),
        Decision::Ineligible(i) => {
            rec.outcome = Outcome::Ineligible;
            rec.reason = Some(i.reason.into());
            rec.detail = Some(i.detail);
        }
        Decision::Dirty if w.on_dirty == OnDirty::Skip => {
            let (units, _) =
                crate::status::outside_edits(pf.head_blob.as_deref(), source.as_bytes(), &bound);
            rec.outcome = Outcome::Skipped;
            rec.reason = Some("dirty".into());
            rec.units = units;
        }
        Decision::Dirty | Decision::Commit => {
            if decision == Decision::Dirty {
                let (_, mut lines) = crate::status::outside_edits(
                    pf.head_blob.as_deref(),
                    source.as_bytes(),
                    &bound,
                );
                let file_mode = file.git_mode().unwrap_or("100644");
                if let Some(head) = &pf.head_entry
                    && repo.file_mode
                    && head.mode != file_mode
                {
                    lines.push(format!("mode {} to {file_mode}", head.mode));
                }
                rec.provenance = lines;
            }
            let mode = match (&pf.head_entry, repo.file_mode) {
                (Some(head), false) => head.mode.clone(),
                _ => file.git_mode().unwrap_or("100644").to_string(),
            };
            match repo.git.store_blob(candidate.as_bytes()) {
                Ok(out) if out.ok() => {
                    rec.candidate = Some(Entry {
                        oid: String::from_utf8_lossy(&out.stdout).trim().to_string(),
                        mode,
                    });
                    rec.message = Some(String::new());
                }
                Ok(out) => {
                    rec.outcome = Outcome::Ineligible;
                    rec.reason = Some("git_error".into());
                    rec.detail = Some(out.failure("git hash-object"));
                }
                Err(e) => {
                    rec.outcome = Outcome::Ineligible;
                    rec.reason = Some("git_error".into());
                    rec.detail = Some(e.to_string());
                }
            }
        }
    }
    // `markrivet adopt` commits or writes nothing, whichever check finds the
    // path cannot commit, `git hash-object` included.
    if (w.adopting || w.require_commit) && !rec.commits() {
        return Err((
            Diagnostic::new(
                ErrorCode::CommitRefused,
                format!(
                    "{path} cannot be committed ({}: {}); nothing written",
                    rec.reason.as_deref().unwrap_or(""),
                    rec.detail.as_deref().unwrap_or("")
                ),
            ),
            None,
        ));
    }
    if receipt_eligible
        && rec.commits()
        && let Some(intent) = &w.delete_intent
    {
        if path.chars().any(char::is_control) {
            return Err((
                Diagnostic::new(
                    ErrorCode::PathRejected,
                    "committed single-delete receipts require control-free canonical paths",
                ),
                None,
            ));
        }
        let receipt = crate::delete_receipt::DeleteReceipt {
            version: intent.version(),
            op: rec.op.clone(),
            path: rec.path.clone(),
            parent: rec.base_head.clone().expect("eligible HEAD"),
            base: rec.base.clone().expect("eligible tracked base"),
            candidate: rec.candidate.clone().expect("candidate hashed"),
            read_sha256: rec.read_sha256.clone(),
            candidate_sha256: rec.candidate_sha256.clone(),
            operation: intent.clone(),
        };
        rec.trailers.push(Trailer::Delete(receipt));
    }
    if let Some(intent) = &w.archive_copy {
        crate::archive::check_source(
            project,
            intent,
            w.archive_parent.as_deref(),
            Some(candidate),
        )
        .map_err(|e| (e, None))?;
        let receipt = crate::archive::CopyReceipt {
            version: 1,
            op: rec.op.clone(),
            copy: intent.clone(),
        };
        rec.trailers.push(Trailer::Archive(receipt));
    }
    if rec.message.is_some() {
        if !rec.trailers.is_empty() {
            rec.user_message = Some(w.message.clone().unwrap_or_default());
        }
        rec.message = Some(rec.compose(w.message.as_deref()).map_err(|e| (e, None))?);
    }
    if let Some(message) = &rec.message {
        project
            .store_put(lock, Some(PENDING), &rec.msg_name(), message.as_bytes())
            .map_err(|e| (e, None))?;
    }
    if let Err(e) = publish(project, lock, &rec) {
        let _ = project.store_remove(lock, PENDING, &[&rec.msg_name()]);
        return Err((e, None));
    }
    let _ = hook("pending_written");
    let preconditions = (|| {
        if let Some(expected) = w.expect_head {
            let conflict = |detail: &str| {
                Diagnostic::new(
                    ErrorCode::GuardConflict,
                    format!("{detail} after guarded adopt preflight; nothing applied"),
                )
                .with_path(path)
            };
            let (branch, head) = git::head_state(&repo.git).map_err(|e| conflict(&e))?;
            if branch != rec.branch || head.as_deref() != Some(expected) {
                return Err(conflict("HEAD or branch moved"));
            }
            let (_, index) =
                git::path_entries(&repo.git, expected, path).map_err(|e| conflict(&e))?;
            if !index_is(&index, rec.base.as_ref()) {
                return Err(conflict("the path's index entry changed"));
            }
        }
        if restore.is_some() {
            no_pending_restore_peer(project, path, &rec.op)?;
            let (branch, head) = git::head_state(&repo.git)
                .map_err(|e| Diagnostic::new(ErrorCode::HistoryChanged, e))?;
            if branch != rec.branch || head != rec.base_head {
                return Err(Diagnostic::new(
                    ErrorCode::HistoryChanged,
                    "HEAD or branch moved after restore preflight; nothing written",
                ));
            }
        }
        rec.check_guards(project, lock, source, candidate)?;
        Ok::<_, Diagnostic>(())
    })();
    let published = if let Err(e) = preconditions {
        Err(e)
    } else if rec.fresh() {
        project.create(lock, path, candidate)
    } else {
        project.replace(lock, path, source, candidate)
    };
    match published {
        Err(e) if e.code == ErrorCode::CommitUncertain => return Err((e, Some(rec.op.clone()))),
        Err(e) => {
            let left = conclude(
                project,
                lock,
                &rec,
                &event(&rec, Outcome::NotApplied, Applied::No, false),
            )
            .pending();
            return Err((e, left.then_some(rec.op.clone())));
        }
        Ok(crate::fs::Commit::AppliedNotDurable) => {
            return Ok(Landed {
                durable: false,
                commit: with_op(
                    &rec.op,
                    json!({"status": "pending", "reason": "not_durable", "pending": rec.op}),
                ),
                receipt_failed: false,
            });
        }
        Ok(crate::fs::Commit::Applied) => {}
    }
    rec.ever_applied = true;
    if !rec.commits() {
        let mut commit_json = json!({"status": rec.outcome.name(), "reason": rec.reason});
        if !rec.units.is_empty() {
            commit_json["units"] = json!(rec.units);
        }
        if let Some(d) = &rec.detail {
            commit_json["detail"] = json!(d);
        }
        rec.phase = Some(Phase::Landed);
        rec.blocked = false;
        let _ = publish(project, lock, &rec);
        let _ = hook("landed");
        let failed = conclude(
            project,
            lock,
            &rec,
            &event(&rec, rec.outcome, Applied::Yes, false),
        )
        .receipt_failed();
        return Ok(Landed {
            durable: true,
            commit: with_op(&rec.op, commit_json),
            receipt_failed: failed,
        });
    }
    rec.phase = Some(Phase::Applied);
    rec.blocked = false;
    let _ = publish(project, lock, &rec);
    let _ = hook("applied");
    // 5. The tree holds the candidate, read again through the held root.
    let now = FileState::read(project, path);
    if !holds_candidate(&now, &rec, repo.file_mode) {
        return Ok(land_skip(
            project,
            lock,
            &mut rec,
            "external_change",
            None,
            false,
        ));
    }
    let _ = hook("reread");
    // 6. Nothing moved in git since preflight.
    match unmoved(&repo.git, &rec) {
        Ok(true) => {}
        Ok(false) => {
            return Ok(land_skip(
                project,
                lock,
                &mut rec,
                "head_moved",
                None,
                false,
            ));
        }
        Err(e) => {
            return Ok(land_skip(
                project,
                lock,
                &mut rec,
                "git_error",
                Some(e),
                false,
            ));
        }
    }
    let (commit, failed) = commit_record(project, lock, &repo.git, &mut rec, false);
    Ok(Landed {
        durable: true,
        commit,
        receipt_failed: failed,
    })
}

fn holds_candidate(file: &FileState, rec: &Record, file_mode: bool) -> bool {
    let FileState::Present { bytes, .. } = file else {
        return false;
    };
    let mode_ok = !file_mode
        || rec
            .candidate
            .as_ref()
            .is_some_and(|c| file.git_mode() == Some(c.mode.as_str()));
    sha256(bytes) == rec.candidate_sha256 && mode_ok
}

/// A commit-intent write that landed and will not commit: the record is
/// republished `landed` with its final outcome, then its receipt concludes it.
fn land_skip(
    project: &Project,
    lock: &WriterLock,
    rec: &mut Record,
    reason: &str,
    detail: Option<String>,
    recovered: bool,
) -> Landed {
    assert!(rec.peer.is_none(), "a pair must never be skipped");
    rec.outcome = if detail.is_some() {
        Outcome::Ineligible
    } else {
        Outcome::Skipped
    };
    rec.reason = Some(reason.into());
    rec.detail = detail;
    rec.phase = Some(Phase::Landed);
    rec.blocked = false;
    let published =
        crate::fs::fault("landed_publish").is_ok() && publish(project, lock, rec).is_ok();
    let _ = hook("landed");
    let failed = conclude(
        project,
        lock,
        rec,
        &event(rec, rec.outcome, Applied::Yes, recovered),
    )
    .receipt_failed();
    let mut commit = json!({"status": rec.outcome.name(), "reason": reason});
    if let Some(d) = &rec.detail {
        commit["detail"] = json!(d);
    }
    if !published && failed {
        // Neither the `landed` record nor the receipt is durable: the record
        // on disk still intends a commit, and recovery will treat it so.
        commit = json!({
            "status": "failed",
            "reason": "bookkeeping",
            "published": false,
            "pending": rec.op,
            "intended": commit,
        });
    }
    Landed {
        durable: true,
        commit: with_op(&rec.op, commit),
        receipt_failed: failed,
    }
}

fn unmoved(git: &Git, rec: &Record) -> Result<bool, String> {
    let (branch, head) = git::head_state(git)?;
    if branch != rec.branch || head != rec.base_head {
        return Ok(false);
    }
    let Some(head) = head else {
        return Ok(false);
    };
    for (path, base, _) in rec.entries() {
        let (entry, index) = git::path_entries(git, &head, path)?;
        if entry.as_ref() != base || !index_is(&index, base) {
            return Ok(false);
        }
    }
    Ok(true)
}

fn recovery_preflight_matches(
    pf: &git::Preflight,
    branch: &Option<String>,
    head: &str,
    base: Option<&Entry>,
) -> bool {
    &pf.branch == branch && pf.head.as_deref() == Some(head) && pf.head_entry.as_ref() == base
}

fn index_is(index: &[IndexEntry], entry: Option<&Entry>) -> bool {
    match entry {
        Some(e) => matches!(index, [only] if only.is(e)),
        None => index.is_empty(),
    }
}

/// What step 8 saw.
enum Seen {
    NotMoved,
    /// HEAD advanced exactly as intended; the index entry is the
    /// candidate's (`Ready`), the base's (`IndexBase`), or anything else.
    Ready(String),
    IndexBase(String),
    IndexOther(String),
    Mismatch(&'static str, String),
}

pub(crate) fn commit_parts(git: &Git, sha: &str) -> Result<(Vec<String>, String), String> {
    let out = git
        .inspect_input_limit(&["cat-file", "commit", sha], None, HISTORY_OBJECT_BYTES)
        .map_err(|e| e.to_string())?;
    if !out.ok() {
        return Err(out.failure("git cat-file commit"));
    }
    raw_commit_parts(&out.stdout, sha.len())
}

fn raw_commit_parts(bytes: &[u8], oid_len: usize) -> Result<(Vec<String>, String), String> {
    // Keep receipt inspection's existing lossy policy for metadata while
    // preserving ASCII trailer bytes, regardless of the commit encoding header.
    let text = String::from_utf8_lossy(bytes);
    let commit =
        crate::git_object::parse_commit(text.as_bytes(), oid_len).map_err(str::to_string)?;
    Ok((
        commit.parents.into_iter().map(str::to_string).collect(),
        commit.message.to_string(),
    ))
}

/// First-parent publication proofs inspect raw objects, never Git's decoded
/// pretty messages. Explicit stream caps also cover complete object headers.
fn history_commits(git: &Git, range: &str) -> Result<Vec<(String, Vec<String>, String)>, String> {
    let git = git
        .clone()
        .bounded(git::ReadBudget::new(HISTORY_OBJECT_BYTES + 2 * 1024 * 1024));
    let walk = git
        .inspect(
            &[
                "rev-list",
                "--first-parent",
                "--max-count",
                HISTORY_LIMIT,
                range,
            ],
            None,
        )
        .map_err(|e| e.to_string())?;
    if !walk.ok() {
        return Err(walk.failure("git rev-list"));
    }
    let text = std::str::from_utf8(&walk.stdout).map_err(|_| "invalid history object ids")?;
    let ids: Vec<&str> = text.lines().collect();
    if ids.iter().any(|id| {
        !crate::git_object::valid_oid(id, id.len(), crate::git_object::OidCase::Canonical)
    }) {
        return Err("invalid history object id".into());
    }
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let objects = git
        .inspect_input_limit(
            &["cat-file", "--batch"],
            Some(&walk.stdout),
            HISTORY_OBJECT_BYTES,
        )
        .map_err(|e| e.to_string())?;
    if !objects.ok() {
        return Err(objects.failure("git cat-file --batch"));
    }
    let mut remaining = objects.stdout.as_slice();
    let mut commits = Vec::with_capacity(ids.len());
    for id in ids {
        let end = remaining
            .iter()
            .position(|b| *b == b'\n')
            .ok_or("missing batch object header")?;
        let header =
            std::str::from_utf8(&remaining[..end]).map_err(|_| "invalid batch object header")?;
        let fields: Vec<&str> = header.split(' ').collect();
        let [oid, "commit", size] = fields.as_slice() else {
            return Err("missing or non-commit history object".into());
        };
        if *oid != id {
            return Err("history object order differs".into());
        }
        let size = size
            .parse::<usize>()
            .map_err(|_| "invalid history object size")?;
        remaining = &remaining[end + 1..];
        let bytes = remaining.get(..size).ok_or("truncated history object")?;
        if remaining.get(size) != Some(&b'\n') {
            return Err("missing history object terminator".into());
        }
        let (parents, message) = raw_commit_parts(bytes, id.len())?;
        commits.push((id.to_string(), parents, message));
        remaining = &remaining[size + 1..];
    }
    if !remaining.is_empty() {
        return Err("unexpected history object output".into());
    }
    Ok(commits)
}

enum HistoryPurpose {
    /// Legacy reconciliation searches for a commit before its existing guarded
    /// preflight. Not finding it does not authorize a pair fence release.
    FindCommit,
    /// Pair waiver/abandonment require a complete proof of absence.
    ProveAbsence,
}

/// One raw search; absence authorization additionally proves the chain to base.
fn find_history_op(
    git: &Git,
    base: &str,
    tip: &str,
    op: &str,
    purpose: HistoryPurpose,
) -> Result<Option<(String, Vec<String>, String)>, String> {
    let history = history_commits(git, &format!("{base}..{tip}"))?;
    let count = history.len();
    let mut next = tip.to_string();
    for (sha, parents, message) in history {
        if sha != next {
            return Err("incomplete first-parent history chain".into());
        }
        if op_trailers(&message).iter().any(|id| id == op) {
            return Ok(Some((sha, parents, message)));
        }
        next = parents.first().cloned().unwrap_or_default();
    }
    if matches!(purpose, HistoryPurpose::ProveAbsence)
        && count >= HISTORY_LIMIT.parse::<usize>().expect("limit")
    {
        return Err("first-parent history audit exceeded its limit".into());
    }
    if matches!(purpose, HistoryPurpose::ProveAbsence) && next != base {
        return Err("first-parent history audit did not reach the recorded base".into());
    }
    Ok(None)
}

/// The paths a single-parent commit changed, with their new entries.
pub(crate) fn changed_paths(git: &Git, sha: &str) -> Result<Vec<(String, Entry)>, String> {
    let out = git
        .inspect(
            &[
                "diff-tree",
                "-r",
                "-z",
                "--no-renames",
                "--no-commit-id",
                sha,
            ],
            None,
        )
        .map_err(|e| e.to_string())?;
    if !out.ok() {
        return Err(out.failure("git diff-tree"));
    }
    let fields: Vec<&[u8]> = out.stdout.split(|b| *b == 0).collect();
    let mut changed = vec![];
    let mut i = 0;
    while i + 1 < fields.len() {
        let meta = String::from_utf8_lossy(fields[i]);
        let Some(meta) = meta.strip_prefix(':') else {
            break;
        };
        let parts: Vec<&str> = meta.split(' ').collect();
        let [_, new_mode, _, new_oid, _] = parts.as_slice() else {
            return Err(format!("unreadable diff-tree record {meta:?}"));
        };
        changed.push((
            String::from_utf8_lossy(fields[i + 1]).into_owned(),
            Entry {
                oid: new_oid.to_string(),
                mode: new_mode.to_string(),
            },
        ));
        i += 2;
    }
    Ok(changed)
}

/// Whether `sha`, a commit with `parents`, is exactly this record's
/// transaction: one parent, only the path changed, to the candidate, and
/// exactly one `Rivet-Op` trailer naming the op. `Err` names the mismatch.
fn transaction_shape(
    git: &Git,
    rec: &Record,
    sha: &str,
    parents: &[String],
    message: &str,
) -> Result<Result<(), &'static str>, String> {
    if parents.len() != 1 || Some(&parents[0]) != rec.base_head.as_ref() {
        return Ok(Err("parent_mismatch"));
    }
    let changed = changed_paths(git, sha)?;
    if changed.len() != rec.paths().count()
        || changed.iter().any(|(p, _)| !rec.paths().any(|x| x == p))
    {
        return Ok(Err("paths_mismatch"));
    }
    for (path, _, candidate) in rec.entries() {
        if changed.iter().find(|(p, _)| p == path).map(|(_, e)| e) != candidate {
            return Ok(Err("blob_mismatch"));
        }
    }
    if op_trailers(message) != [rec.op.clone()] {
        return Ok(Err("trailer_mismatch"));
    }
    for trailer in &rec.trailers {
        if !trailer.matches(rec, &parents[0], message) {
            return Ok(Err(trailer.mismatch()));
        }
    }
    Ok(Ok(()))
}

fn observe(git: &Git, rec: &Record) -> Result<Seen, String> {
    let (branch, head) = git::head_state(git)?;
    let head = head.ok_or("HEAD is unborn")?;
    if branch != rec.branch {
        return Ok(Seen::Mismatch("branch_mismatch", head));
    }
    if Some(&head) == rec.base_head.as_ref() {
        return Ok(Seen::NotMoved);
    }
    let (parents, message) = commit_parts(git, &head)?;
    if let Err(why) = transaction_shape(git, rec, &head, &parents, &message)? {
        return Ok(Seen::Mismatch(why, head));
    }
    let mut ready = true;
    let mut base_only = true;
    for (path, base, candidate) in rec.entries() {
        let (_, index) = git::path_entries(git, &head, path)?;
        ready &= index_is(&index, candidate);
        base_only &= index_is(&index, base);
    }
    Ok(if ready {
        Seen::Ready(head)
    } else if base_only {
        Seen::IndexBase(head)
    } else {
        Seen::IndexOther(head)
    })
}

fn record_child(
    project: &Project,
    lock: &WriterLock,
    rec: &mut Record,
    pid: u32,
    at: SystemTime,
) -> Result<(), Diagnostic> {
    let _ = hook_with("helper_spawned", &pid.to_string());
    let identity = git::ProcessIdentity::read(pid).ok_or_else(|| {
        Diagnostic::new(
            ErrorCode::Io,
            format!("cannot read git child {pid}'s process identity"),
        )
    })?;
    rec.pid = Some(pid);
    rec.child_identity = Some(identity);
    rec.spawned_ns = Some(now_ns(at));
    publish(project, lock, rec)?;
    let _ = hook_with("pid_recorded", &pid.to_string());
    Ok(())
}

fn mutate(
    project: &Project,
    lock: &WriterLock,
    git: &Git,
    rec: &mut Record,
    args: &[&str],
) -> Result<git::Output, GitError> {
    git.mutate(args, |pid, at| record_child(project, lock, rec, pid, at))
}

fn index_locked(out: &git::Output) -> bool {
    String::from_utf8_lossy(&out.stderr).contains("index.lock")
}

fn run_git(
    project: &Project,
    lock: &WriterLock,
    git: &Git,
    rec: &mut Record,
    args: &[&str],
) -> Result<git::Output, &'static str> {
    let mut retries = 0;
    let result = loop {
        match mutate(project, lock, git, rec, args) {
            Ok(out) if out.ok() => break Ok(out),
            Ok(out) if index_locked(&out) && retries < INDEX_RETRIES => {
                retries += 1;
                std::thread::sleep(Duration::from_millis(50));
            }
            Ok(out) if index_locked(&out) => break Err("index_locked"),
            Ok(_) => break Err("git_error"),
            Err(GitError::Deadline) => break Err("deadline"),
            Err(_) => break Err("git_error"),
        }
    };
    rec.retries += retries;
    result
}

fn lock_index(
    project: &Project,
    lock: &WriterLock,
    rec: &mut Record,
) -> Result<git::IndexLock, &'static str> {
    if rec.peer.is_some() {
        return pair::fence_acquire_durable(project, lock, rec);
    }
    let mut attempts = 0;
    let real = loop {
        match git::IndexLock::acquire(project) {
            Ok(real) => break real,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && attempts < INDEX_RETRIES => {
                attempts += 1;
                rec.retries += 1;
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => return Err("index_locked"),
            Err(_) => return Err("git_error"),
        }
    };
    rec.index_lease = Some(real.lease.clone());
    publish(project, lock, rec).map_err(|_| "git_error")?;
    let _ = hook("index_locked");
    Ok(real)
}

fn sync_ready_index(
    project: &Project,
    lock: &WriterLock,
    git: &Git,
    rec: &mut Record,
    head: &str,
) -> Result<(), &'static str> {
    let real = lock_index(project, lock, rec)?;
    let (_, current) = git::head_state(git).map_err(|_| "index_changed")?;
    if current.as_deref() != Some(head) {
        return Err("index_changed");
    }
    for (path, _, _) in rec.entries() {
        let (at_head, index) = git::path_entries(git, head, path).map_err(|_| "index_changed")?;
        if !index_is(&index, at_head.as_ref()) {
            return Err("index_changed");
        }
    }
    real.sync_existing().map_err(|_| "index_changed")?;
    if rec.peer.is_some() {
        real.release().map_err(|_| "index_changed")?;
    }
    Ok(())
}

fn prepare_index(
    project: &Project,
    lock: &WriterLock,
    git: &Git,
    rec: &mut Record,
    head: &str,
) -> Result<(git::IndexLock, git::PrivateIndex), &'static str> {
    let real = lock_index(project, lock, rec)?;
    let (branch, current) = git::head_state(git).map_err(|_| "git_error")?;
    if branch != rec.branch || current.as_deref() != Some(head) {
        return Err("head_moved");
    }
    for (path, base, candidate) in rec.entries() {
        let (_, index) = git::path_entries(git, head, path).map_err(|_| "git_error")?;
        if !index_is(&index, base) && !(rec.creation && index_is(&index, candidate)) {
            return Err("head_moved");
        }
    }
    let private = git::PrivateIndex::new(project, &sha256(format!("{}:sync", rec.op).as_bytes()))
        .map_err(|_| "git_error")?;
    let indexed = git.with_index(&private);
    if let Some(bytes) = real.bytes().map_err(|_| "git_error")? {
        private.seed(&bytes).map_err(|_| "git_error")?;
    } else {
        run_git(project, lock, &indexed, rec, &["read-tree", "--empty"])?;
    }
    let entries: Vec<_> = rec
        .entries()
        .into_iter()
        .map(|(p, _, c)| {
            let c = c.ok_or("git_error")?;
            Ok(format!("{},{},{}", c.mode, c.oid, p))
        })
        .collect::<Result<_, &'static str>>()?;
    for info in entries {
        run_git(
            project,
            lock,
            &indexed,
            rec,
            &["update-index", "--add", "--cacheinfo", &info],
        )?;
    }
    let _ = hook("index_prepared");
    Ok((real, private))
}

fn install_index((real, private): (git::IndexLock, git::PrivateIndex)) -> Result<(), &'static str> {
    let bytes = private.bytes().map_err(|_| "index_changed")?;
    real.install(&bytes).map_err(|_| "index_changed")
}

fn install_record_index(
    project: &Project,
    git: &Git,
    rec: &Record,
    index: (git::IndexLock, git::PrivateIndex),
) -> Result<(), &'static str> {
    if rec.peer.is_none() {
        return install_index(index);
    }
    let (real, private) = index;
    let bytes = private.bytes().map_err(|_| "index_changed")?;
    let mut failure = "index_changed";
    real.install_checked(&bytes, || {
        let validate = || -> Result<(), &'static str> {
            let (branch, head) = git::head_state(git).map_err(|_| "git_error")?;
            let head = head.ok_or("head_moved")?;
            if branch != rec.branch {
                return Err("head_moved");
            }
            for (path, _, candidate) in rec.entries() {
                let (entry, _) = git::path_entries(git, &head, path).map_err(|_| "git_error")?;
                if entry.as_ref() != candidate {
                    return Err("target_changed");
                }
            }
            if !pair::files_are_candidates(project, rec) {
                return Err("worktree_differs");
            }
            Ok(())
        };
        validate().map_err(|reason| {
            failure = reason;
            std::io::Error::other(reason)
        })
    })
    .map_err(|_| failure)
}

fn oid(out: git::Output) -> Result<String, &'static str> {
    let value = String::from_utf8(out.stdout).map_err(|_| "git_error")?;
    let value = value.trim();
    if !crate::git_object::valid_oid(value, value.len(), crate::git_object::OidCase::Either) {
        return Err("git_error");
    }
    Ok(value.to_owned())
}

/// Construct an immutable transaction, then prepare the real index and ref
/// transaction before publication. No command stages worktree bytes.
fn publish_candidate(
    project: &Project,
    lock: &WriterLock,
    git: &Git,
    rec: &mut Record,
) -> Result<(), &'static str> {
    let base = rec.base_head.clone().ok_or("git_error")?;
    let branch = rec.branch.clone().ok_or("git_error")?;
    let private = git::PrivateIndex::new(project, &sha256(format!("{}:tree", rec.op).as_bytes()))
        .map_err(|_| "git_error")?;
    let indexed = git.with_index(&private);
    run_git(project, lock, &indexed, rec, &["read-tree", &base])?;
    let entries: Vec<_> = rec
        .entries()
        .into_iter()
        .map(|(p, _, c)| {
            let c = c.ok_or("git_error")?;
            Ok(format!("{},{},{}", c.mode, c.oid, p))
        })
        .collect::<Result<_, &'static str>>()?;
    for info in entries {
        run_git(
            project,
            lock,
            &indexed,
            rec,
            &["update-index", "--add", "--cacheinfo", &info],
        )?;
    }
    let _ = hook("staged"); // A private tree entry; the real index is untouched.
    let tree = oid(run_git(project, lock, &indexed, rec, &["write-tree"])?)?;
    let msg = format!(".rivet/{PENDING}/{}", rec.msg_name());
    let commit = oid(run_git(
        project,
        lock,
        git,
        rec,
        &["commit-tree", &tree, "-p", &base, "-F", &msg],
    )?)?;
    let index = prepare_index(project, lock, git, rec, &base)?;
    let _ = hook("before_ref_update");
    let input = format!("start\nupdate {branch} {commit} {base}\nprepare\n");
    let prepared = std::cell::Cell::new(false);
    let result = git.publish(
        input.as_bytes(),
        |pid, at| record_child(project, lock, rec, pid, at),
        |until| {
            // Both reads share the validation budget.
            let (current_branch, current_head) = git::head_state(&git.within(until))
                .map_err(|e| Diagnostic::new(ErrorCode::Io, e))?;
            if current_branch.as_deref() != Some(branch.as_str())
                || current_head.as_deref() != Some(base.as_str())
            {
                return Err(Diagnostic::new(ErrorCode::ExternalChange, "head_moved"));
            }
            prepared.set(true);
            hook("ref_prepared")?;
            Ok(())
        },
    );
    // A moved HEAD settles the write as a skip only when Git ended by itself:
    // a child killed at a bound, or one whose reaping failed, may have left
    // its ref locks, so its record must stay for recovery.
    let ended = !matches!(result, Err(GitError::Deadline | GitError::Spawn(_)));
    if ended
        && !prepared.get()
        && git::head_state(git)
            .is_ok_and(|(b, h)| b.as_ref() != Some(&branch) || h.as_ref() != Some(&base))
    {
        return Err("head_moved");
    }
    match result {
        Ok(out) if out.ok() => {}
        Err(GitError::Aborted(e)) if e == "head_moved" => return Err("head_moved"),
        Err(GitError::Deadline) => return Err("deadline"),
        _ => return Err("git_error"),
    }
    let _ = hook("ref_updated");
    install_record_index(project, git, rec, index)
}

/// Publish the recorded object, inspect what happened, and record the outcome.
fn commit_record(
    project: &Project,
    lock: &WriterLock,
    git: &Git,
    rec: &mut Record,
    recovered: bool,
) -> (Value, bool) {
    if rec.expect_head.is_some() && rec.base_head != rec.expect_head {
        let mut v = guard_head_moved(project, lock, rec);
        v["pending"] = json!(rec.op);
        v["status"] = json!("repair_required");
        return (with_op(&rec.op, v), false);
    }
    if rec.peer.is_none() && rec.candidate.is_some() && rec.candidate == rec.base {
        let concluded = conclude(
            project,
            lock,
            rec,
            &event(rec, Outcome::UnchangedInGit, Applied::Yes, recovered),
        );
        let mut v = json!({"status": "unchanged_in_git"});
        concluded.mark_left(&mut v);
        return (with_op(&rec.op, v), concluded.receipt_failed());
    }
    let message = rec.message.clone().unwrap_or_default();
    if project
        .store_put(lock, Some(PENDING), &rec.msg_name(), message.as_bytes())
        .is_err()
    {
        return fail(project, lock, rec, "git_error", json!(false), recovered);
    }
    let started = Instant::now();
    let reason = publish_candidate(project, lock, git, rec).err();
    rec.git_ms += started.elapsed().as_millis() as u64;
    if rec.peer.is_some() && matches!(reason, Some("head_moved" | "git_error" | "deadline")) {
        return fail(
            project,
            lock,
            rec,
            reason.expect("reason"),
            json!("unknown"),
            recovered,
        );
    }
    if reason == Some("head_moved") && rec.peer.is_none() {
        let landed = land_skip(project, lock, rec, "head_moved", None, recovered);
        return (landed.commit, landed.receipt_failed);
    }
    match observe(git, rec) {
        Err(_) => {
            rec.phase = Some(Phase::Unverified);
            rec.blocked = false;
            fail(
                project,
                lock,
                rec,
                "unverified",
                json!("unknown"),
                recovered,
            )
        }
        Ok(Seen::NotMoved) => {
            rec.phase = Some(Phase::Applied);
            rec.blocked = false;
            fail(
                project,
                lock,
                rec,
                reason.unwrap_or("git_error"),
                json!(false),
                recovered,
            )
        }
        Ok(Seen::Ready(sha)) if reason == Some("index_changed") => {
            rec.sha = Some(sha);
            require_repair(
                rec,
                "index_changed",
                "the installed index is not confirmed durable",
            );
            fail(project, lock, rec, "index_changed", json!(true), recovered)
        }
        Ok(Seen::Ready(sha)) => committed(project, lock, rec, sha, recovered),
        Ok(Seen::IndexBase(sha) | Seen::IndexOther(sha)) => {
            rec.sha = Some(sha);
            require_repair(
                rec,
                "index_changed",
                "the published transaction's index installation is incomplete",
            );
            fail(project, lock, rec, "index_changed", json!(true), recovered)
        }
        Ok(Seen::Mismatch(why, sha)) => {
            rec.sha = Some(sha);
            require_repair(rec, why, "HEAD moved, but not as this transaction");
            fail(project, lock, rec, why, json!(true), recovered)
        }
    }
}

fn require_repair(rec: &mut Record, reason: &str, detail: &str) {
    rec.blocked = true;
    rec.repair = Some(json!({"reason": reason, "detail": detail}));
}

fn guard_head_moved(project: &Project, lock: &WriterLock, rec: &mut Record) -> Value {
    require_repair(
        rec,
        "guard_head_moved",
        "guarded adopt can commit only on the certified HEAD; abandon the record and check the file again",
    );
    let _ = publish(project, lock, rec);
    repair_report(rec)
}

fn committed(
    project: &Project,
    lock: &WriterLock,
    rec: &mut Record,
    sha: String,
    recovered: bool,
) -> (Value, bool) {
    if !matches!(FileState::read(project, &rec.path),
        FileState::Present { bytes, .. } if sha256(&bytes) == rec.candidate_sha256)
        || (rec.peer.is_some() && !pair::files_are_candidates(project, rec))
    {
        rec.reason = Some("external_change".into());
    }
    rec.phase = Some(Phase::Committed);
    rec.blocked = false;
    rec.sha = Some(sha.clone());
    let _ = publish(project, lock, rec);
    let _ = hook("committed");
    let concluded = conclude(
        project,
        lock,
        rec,
        &event(rec, Outcome::Committed, Applied::Yes, recovered),
    );
    let mut v = json!({"status": "committed", "sha": sha, "retries": rec.retries});
    if let Some(reason) = &rec.reason {
        v["reason"] = json!(reason);
    }
    if let Some(copy) = rec.archive_copy() {
        v["copy_op"] = json!(rec.op);
        v["archive_copy"] = json!(copy);
    }
    concluded.mark_left(&mut v);
    (with_op(&rec.op, v), concluded.receipt_failed())
}

/// A `failed` event (not terminal): the record stays in the state set on it.
fn fail(
    project: &Project,
    lock: &WriterLock,
    rec: &mut Record,
    reason: &str,
    published: Value,
    recovered: bool,
) -> (Value, bool) {
    let _ = publish(project, lock, rec);
    let mut e = event(rec, Outcome::Failed, Applied::Yes, recovered);
    e.reason = Some(reason.into());
    e.published = Some(published.clone());
    let failed = append(project, lock, &e).is_err();
    (
        with_op(
            &rec.op,
            json!({"status": "failed", "reason": reason, "published": published, "pending": rec.op}),
        ),
        failed,
    )
}

// ---------------------------------------------------------------------------
// Recovery.

/// `markrivet commit-pending`'s stdin: `{}`, `{"op"}`, `{"abandon", "reason"}`
/// or `{"clear_lock", "identity", "op"}` / `{"clear_lock", "identity", "size", "quiescent": true}`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PendingRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub op: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub abandon: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clear_lock: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quiescent: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lock_timeout_ms: Option<u64>,
}

impl PendingRequest {
    /// The lock timeout and the one thing asked for.
    pub fn parse(self) -> Result<(Option<u64>, Request), Diagnostic> {
        let request = match (
            self.op,
            self.abandon,
            self.reason,
            self.clear_lock,
            self.identity,
            self.size,
            self.quiescent,
        ) {
            (None, None, None, None, None, None, None) => Request::All,
            (Some(op), None, None, None, None, None, None) => Request::Op(op),
            (None, Some(op), Some(reason), None, None, None, None) if !reason.trim().is_empty() => {
                Request::Abandon { op, reason }
            }
            (Some(op), None, None, Some(file), Some(identity), None, None) => {
                Request::ClearLock { file, identity, op }
            }
            (None, None, None, Some(file), Some(identity), Some(size), Some(true)) => {
                Request::ClearUnownedLock {
                    file,
                    identity,
                    size,
                }
            }
            _ => {
                return Err(Diagnostic::new(
                    ErrorCode::InvalidRequest,
                    "commit-pending takes {}, {\"op\"}, {\"abandon\", \"reason\"} with a non-empty reason, or {\"clear_lock\", \"identity\", \"op\"} / {\"clear_lock\", \"identity\", \"size\", \"quiescent\": true}",
                ));
            }
        };
        Ok((self.lock_timeout_ms, request))
    }
}

/// What `markrivet commit-pending` was asked to do.
#[derive(Debug, Clone)]
pub enum Request {
    All,
    Op(String),
    Abandon {
        op: String,
        reason: String,
    },
    ClearUnownedLock {
        file: String,
        identity: String,
        size: u64,
    },
    ClearLock {
        file: String,
        identity: String,
        op: String,
    },
}

/// `markrivet commit-pending`: settle records, retire one, or clear one
/// reported lock and then settle its record.
pub fn commit_pending(
    project: &Project,
    lock: &WriterLock,
    request: Request,
) -> Result<Value, Diagnostic> {
    let clearing = matches!(
        &request,
        Request::ClearUnownedLock { .. } | Request::ClearLock { .. }
    );
    let (pending, unreadable) = records(project).map_err(|e| {
        if clearing {
            Diagnostic::new(ErrorCode::ClearLockRefused, e.message)
                .with_lock_reason(crate::contract::LockClearReason::PendingRecords)
        } else {
            e
        }
    })?;
    // No generic clear may remove a live pair's lifetime fence, even through
    // an older record whose timestamp would accept it. Unknown pending bytes
    // cannot prove that no pair owns the index lock either.
    let clear = match &request {
        Request::ClearLock { file, op, .. } => Some((file, Some(op.as_str()))),
        Request::ClearUnownedLock { file, .. } => Some((file, None)),
        _ => None,
    };
    if let Some((file, selected)) = clear {
        if !unreadable.is_empty() && (selected.is_none() || file == ".git/index.lock") {
            return Err(Diagnostic::new(
                ErrorCode::ClearLockRefused,
                format!(
                    "pending ownership is unknown: {}",
                    serde_json::to_string(&unreadable).expect("json")
                ),
            )
            .with_lock_reason(crate::contract::LockClearReason::PendingRecords));
        }
        if file == ".git/index.lock"
            && pending
                .iter()
                .any(|r| r.peer.is_some() && !r.fence_waived && selected != Some(r.op.as_str()))
        {
            return Err(Diagnostic::new(ErrorCode::ClearLockRefused,
                "pending pair ownership requires its own explicit fence waiver; index lock left in place")
                .with_lock_reason(crate::contract::LockClearReason::PendingRecords));
        }
    }
    let find =
        |op: &str| {
            pending.iter().find(|r| r.op == op).cloned().ok_or_else(|| {
                Diagnostic::new(ErrorCode::NotFound, format!("no pending record {op}"))
            })
        };
    let mut cleared = None;
    let results = match request {
        Request::ClearUnownedLock {
            file,
            identity,
            size,
        } => {
            let (branch, _) = git::head_state(&git::Git::new(project)).map_err(|e| {
                Diagnostic::new(ErrorCode::ClearLockRefused, e)
                    .with_lock_reason(crate::contract::LockClearReason::RepositoryUnknown)
            })?;
            cleared = Some(git::clear_unowned_lock(
                project,
                branch.as_deref(),
                &file,
                &identity,
                size,
                |l| {
                    pending.iter().any(|r| {
                        lock_attributed(r, l)
                            || (r.commits() && r.pid.is_some() && !r.child().dead())
                    })
                },
            )?);
            vec![]
        }
        Request::All => recover(project, lock, |_| true)?,
        Request::Op(op) => {
            find(&op)?;
            recover(project, lock, |r| r.op == op)?
        }
        Request::Abandon { op, reason } => vec![abandon(project, lock, find(&op)?, &reason)?],
        Request::ClearLock { file, identity, op } => {
            let mut rec = find(&op)?;
            if rec.peer.is_some() && file == ".git/index.lock" {
                let result = pair::waive(project, lock, &mut rec, &identity)?;
                cleared = Some(file);
                vec![result]
            } else {
                if !rec.commits() {
                    return Err(Diagnostic::new(
                        ErrorCode::ClearLockRefused,
                        format!("record {op} never runs git, so no lock is its own"),
                    ));
                }
                let removed = git::clear_lock(
                    project,
                    rec.branch.as_deref(),
                    &rec.lock_child(&file, &identity),
                    &file,
                    &identity,
                )
                .map_err(|stale| {
                    Diagnostic::new(
                        ErrorCode::ClearLockRefused,
                        format!(
                            "{} left in place: {}",
                            stale.file,
                            serde_json::to_string(&stale).expect("json")
                        ),
                    )
                })?;
                cleared = Some(removed);
                recover(project, lock, |r| r.op == op)?
            }
        }
    };
    let (left, _) = records(project)?;
    let mut body = json!({
        "status": "applied",
        "records": results,
        "unresolved": left.len(),
        "diagnostics": unreadable,
    });
    if let Some(file) = cleared {
        body["cleared"] = json!(file);
    }
    Ok(body)
}

fn report(rec: &Record, result: &str) -> Value {
    let mut v = json!({"op": rec.op, "seq": rec.seq, "path": rec.path, "result": result});
    if let Some(peer) = &rec.peer {
        v["peer_path"] = json!(peer.path);
        v["blobs"] = json!([
            {"path":rec.path, "read":rec.base, "candidate":rec.candidate},
            {"path":peer.path, "read":peer.base, "candidate":peer.candidate}
        ]);
    }
    if let Some(r) = &rec.reason {
        v["reason"] = json!(r);
    }
    v
}

/// Settle the selected records in sequence order: first every disposition
/// that needs no git, then the commit records.
fn recover(
    project: &Project,
    lock: &WriterLock,
    select: impl Fn(&Record) -> bool,
) -> Result<Vec<Value>, Diagnostic> {
    let (pending, _) = records(project)?;
    let pending: Vec<_> = pending.into_iter().filter(select).collect();
    let receipts = Receipts::for_pending(project, pending.iter().map(|r| r.op.as_str()))?;
    let mut results = vec![];
    let mut later = vec![];
    for rec in pending {
        if let Some(e) = receipts.terminal(&rec.op) {
            results.push(reconcile(project, lock, &rec, e));
            continue;
        }
        match settle_without_git(project, lock, &rec) {
            Some(v) => results.push(v),
            None => later.push(rec),
        }
    }
    for mut rec in later {
        results.push(recover_commit(project, lock, &mut rec));
    }
    Ok(results)
}

/// The op's terminal event is already in the log: sync the log, then
/// retire the record. Never a second event.
fn reconcile(project: &Project, lock: &WriterLock, rec: &Record, e: &Event) -> Value {
    let mut v = report(rec, "reconciled");
    v["outcome"] = json!(e.outcome);
    if let Err(d) = project.log_sync(RECEIPTS) {
        v["result"] = json!("receipt_failed");
        v["error"] = json!(d);
    } else if let Err(d) = retire(project, lock, rec) {
        // Settled; only the record outlived it.
        v["record_left"] = json!(true);
        v["error"] = json!(d);
    }
    v
}

enum Bytes {
    Read,
    Candidate,
    Neither,
}

fn file_is(project: &Project, rec: &Record) -> Bytes {
    bytes_at(project, &rec.path, &rec.read_sha256, &rec.candidate_sha256)
}

/// What a recorded path holds, the candidate checked first (a write whose
/// bytes read equal its candidate counts as applied).
fn bytes_at(project: &Project, path: &str, read: &str, candidate: &str) -> Bytes {
    match project.read_raw(path) {
        Ok(Some((bytes, _))) => {
            let h = sha256(&bytes);
            if h == candidate {
                Bytes::Candidate
            } else if h == read {
                Bytes::Read
            } else {
                Bytes::Neither
            }
        }
        _ => Bytes::Neither,
    }
}

fn concluded(
    project: &Project,
    lock: &WriterLock,
    rec: &Record,
    outcome: Outcome,
    applied: Applied,
) -> Value {
    let mut v = report(rec, outcome.name());
    v["applied"] = json!(applied);
    match conclude(project, lock, rec, &event(rec, outcome, applied, true)) {
        Concluded::Retired => {}
        Concluded::RecordLeft(error) => {
            v["record_left"] = json!(true);
            v["error"] = json!(error);
        }
        Concluded::ReceiptFailed(error) => {
            v["result"] = json!("receipt_failed");
            v["error"] = json!(error);
        }
    }
    v
}

/// The first pass: `landed` records, non-commit `prepared` records, and
/// commit records whose rename never happened. No git, no git fields, no
/// lock check. `None` sends the record to the commit pass.
fn settle_without_git(project: &Project, lock: &WriterLock, rec: &Record) -> Option<Value> {
    if rec.peer.is_some() {
        return None;
    }
    let differs = rec.read_sha256 != rec.candidate_sha256;
    match (rec.state(), rec.commits()) {
        // The outcome was decided before the crash; the file may have
        // changed since, and is not looked at.
        (State::Landed, _) => Some(concluded(project, lock, rec, rec.outcome, Applied::Yes)),
        (State::Prepared, false) => Some(match file_is(project, rec) {
            Bytes::Candidate => concluded(project, lock, rec, rec.outcome, Applied::Yes),
            Bytes::Read if differs => {
                concluded(project, lock, rec, Outcome::NotApplied, Applied::No)
            }
            _ => concluded(project, lock, rec, rec.outcome, Applied::Unknown),
        }),
        (State::Prepared, true) if differs && matches!(file_is(project, rec), Bytes::Read) => {
            // Git runs only after `applied`, so the rename never happened.
            Some(concluded(
                project,
                lock,
                rec,
                Outcome::NotApplied,
                Applied::No,
            ))
        }
        (_, false) => Some(concluded(project, lock, rec, rec.outcome, Applied::Unknown)),
        _ => None,
    }
}

fn fence_attributed(rec: &Record, lock: &git::LockFile) -> bool {
    lock.file == ".git/index.lock"
        && rec.peer.is_some()
        && rec
            .fence
            .as_ref()
            .zip(lock.fence.as_ref())
            .is_some_and(|(a, b)| a == b)
}

/// Prefer a pair's exact inode/birth identity over an older child's time floor.
/// An active pair with an unrecorded/unknown fence never falls back to a guess.
fn attributed_record(records: &[Record], lock: &git::LockFile) -> Option<String> {
    if let Some(rec) = records
        .iter()
        .find(|r| fence_attributed(r, lock) && r.child().dead())
    {
        return Some(rec.op.clone());
    }
    if lock.file == ".git/index.lock" && records.iter().any(|r| r.peer.is_some() && !r.fence_waived)
    {
        return None;
    }
    records.iter().find_map(|r| attribute(r, lock))
}

/// Whether the record's child or index lease could have created this lock.
fn lock_attributed(rec: &Record, lock: &git::LockFile) -> bool {
    if lock.file == ".git/index.lock" && rec.peer.is_some() {
        return fence_attributed(rec, lock);
    }
    let Some(pid) = rec.pid else {
        return false;
    };
    match lock.kind {
        git::LockKind::NextIndex => lock.pid == Some(pid),
        _ => lock.modified >= rec.lock_child(&lock.file, &lock.identity).spawned,
    }
}

/// The owning record, reported only after its child is dead.
fn attribute(rec: &Record, lock: &git::LockFile) -> Option<String> {
    (lock_attributed(rec, lock) && rec.child().dead()).then(|| rec.op.clone())
}

fn stale_report(rec: &mut Record, evidence: Value, detail: &str) -> Value {
    require_repair(rec, "stale_git_lock", detail);
    if let Some(r) = rec.data.repair.as_mut() {
        r["locks"] = evidence.clone();
    }
    let mut v = report(rec, "repair_required");
    v["reason"] = json!("stale_git_lock");
    v["locks"] = evidence;
    v
}

/// The second pass, for one commit record.
fn recover_commit(project: &Project, lock: &WriterLock, rec: &mut Record) -> Value {
    if let Some(pid) = rec.pid
        && !rec.child().dead()
    {
        let mut v = report(rec, "git_child_running");
        v["pid"] = json!(pid);
        return v;
    }
    if rec.peer.is_some() {
        return pair::recover_pair(project, lock, rec);
    }
    let branch = rec.branch.clone();
    let (removed, kept) = git::clear_next_index(project, branch.as_deref(), &rec.child());
    let mut extra = json!({});
    if !removed.is_empty() {
        extra["removed"] = json!(removed);
    }
    if !kept.is_empty() {
        extra["kept"] = json!(kept);
    }
    let with = |mut v: Value, extra: &Value| {
        for (k, x) in extra.as_object().expect("object") {
            v[k] = x.clone();
        }
        v
    };
    let locks = match git::list_locks(project, branch.as_deref()) {
        Ok(locks) => locks,
        Err(stale) => {
            let v = stale_report(rec, json!([stale]), "the lock listing was refused");
            let _ = publish(project, lock, rec);
            return with(v, &extra);
        }
    };
    let (owners, ownership_errors) = match records(project) {
        Ok(found) => found,
        Err(e) => (vec![], vec![e]),
    };
    let blocking: Vec<Value> = locks
        .iter()
        .filter(|l| l.blocking())
        .map(|l| {
            let mut e = json!(l);
            e["record"] = json!(
                ownership_errors
                    .is_empty()
                    .then(|| attributed_record(&owners, l))
                    .flatten()
            );
            if !ownership_errors.is_empty() {
                e["ownership_diagnostics"] = json!(ownership_errors);
            }
            e["veto"] = json!(git::lock_veto(project, l));
            e
        })
        .collect();
    if !blocking.is_empty() {
        // No git runs past a lock that stops it, whoever left it; only a
        // `clear_lock` request or the person removes it.
        let v = stale_report(
            rec,
            json!(blocking),
            "a git lock is present; clear it with clear_lock or by hand",
        );
        let _ = publish(project, lock, rec);
        return with(v, &extra);
    }
    let git = Git::new(project);
    let v = match recover_with_git(project, lock, &git, rec) {
        Ok(v) => v,
        Err(e) => {
            require_repair(rec, "git_error", &e);
            let _ = publish(project, lock, rec);
            let mut v = report(rec, "repair_required");
            v["reason"] = json!("git_error");
            v["detail"] = json!(e);
            v
        }
    };
    with(v, &extra)
}

fn recover_with_git(
    project: &Project,
    lock: &WriterLock,
    git: &Git,
    rec: &mut Record,
) -> Result<Value, String> {
    let base_head = rec.base_head.clone().ok_or("the record has no base HEAD")?;
    let (branch, head) = git::head_state(git)?;
    let head = head.ok_or("HEAD is unborn")?;
    // 1. Find the transaction commit by its trailer, on first-parent
    //    history back to the base.
    if let Some((sha, parents, message)) =
        find_history_op(git, &base_head, &head, &rec.op, HistoryPurpose::FindCommit)?
    {
        if let Err(why) = transaction_shape(git, rec, &sha, &parents, &message)? {
            rec.sha = Some(sha);
            require_repair(
                rec,
                why,
                "the commit carrying this op is not its transaction",
            );
            let _ = publish(project, lock, rec);
            let mut v = report(rec, "repair_required");
            v["reason"] = json!(why);
            return Ok(v);
        }
        // The transaction is in history, so its write reached the tree.
        rec.ever_applied = true;
        let (at_head, index) = git::path_entries(git, &head, &rec.path)?;
        if index_is(&index, at_head.as_ref()) {
            if sync_ready_index(project, lock, git, rec, &head).is_err() {
                rec.sha = Some(sha);
                require_repair(
                    rec,
                    "index_changed",
                    "the installed index is not confirmed durable",
                );
                publish(project, lock, rec).map_err(Diagnostic::detail)?;
                return Ok(repair_report(rec));
            }
            let (v, failed) = committed(project, lock, rec, sha, true);
            return Ok(recovered_report(rec, v, failed));
        }
        // Repair only an index nothing after the transaction could explain.
        let later = format!("{sha}..{head}");
        let touched = git
            .log(&[
                "--first-parent",
                "--full-history",
                "--format=%H",
                &later,
                "--",
                &rec.path,
            ])
            .map_err(|e| e.to_string())?;
        if !touched.ok() {
            return Err(touched.failure("git log"));
        }
        if index_is(&index, rec.base.as_ref())
            && at_head == rec.candidate
            && touched.stdout.is_empty()
        {
            if prepare_index(project, lock, git, rec, &head)
                .and_then(install_index)
                .is_ok()
            {
                let (v, failed) = committed(project, lock, rec, sha, true);
                return Ok(recovered_report(rec, v, failed));
            }
            rec.sha = Some(sha);
            require_repair(rec, "index_changed", "the index repair did not take");
        } else if !touched.stdout.is_empty() {
            rec.sha = Some(sha);
            require_repair(
                rec,
                "index_changed",
                "commits after the transaction changed the path; the index is left as it is",
            );
        } else {
            rec.sha = Some(sha);
            require_repair(
                rec,
                "index_changed",
                "the index entry is neither HEAD's nor the base's",
            );
        }
        let _ = publish(project, lock, rec);
        let mut v = report(rec, "repair_required");
        v["reason"] = json!("index_changed");
        return Ok(v);
    }
    let file = FileState::read(project, &rec.path);
    // 3. A creation whose publication never happened: no file, no index
    //    entry, nothing at HEAD. Git runs only after `applied`.
    // `ever_applied`, not the state: a first recovery that met a lock or no
    // git republished the record `repair_required`, and that must not
    // strand a creation that never published.
    if rec.fresh() && !rec.ever_applied && matches!(file, FileState::Missing) {
        let (at_head, index) = git::path_entries(git, &head, &rec.path)?;
        if at_head.is_none() && index.is_empty() {
            return Ok(concluded(
                project,
                lock,
                rec,
                Outcome::NotApplied,
                Applied::No,
            ));
        }
    }
    // 2. Not committed, and the tree holds the candidate.
    let repo = Repo::inspect(project);
    if !holds_candidate(&file, rec, repo.file_mode) {
        require_repair(
            rec,
            "file_changed",
            "the file is no longer the recorded candidate",
        );
        let _ = publish(project, lock, rec);
        return Ok(repair_report(rec));
    }
    // Legacy records hashed without -w. Reconstruct only from the bytes and
    // mode just verified against the record, never from a later editor save.
    if let FileState::Present { bytes, .. } = &file {
        let stored = git.store_blob(bytes).map_err(|e| e.to_string())?;
        if !stored.ok()
            || oid(stored).ok().as_deref() != rec.candidate.as_ref().map(|c| c.oid.as_str())
        {
            return Err("candidate object could not be stored with its recorded identity".into());
        }
    }
    // The tree holds the candidate: the write was applied, whatever follows.
    rec.ever_applied = true;
    project
        .sync_document(&rec.path)
        .map_err(Diagnostic::detail)?;
    let (at_head, index) = git::path_entries(git, &head, &rec.path)?;
    // A creation may have been staged by its own `git add` before it died.
    let index_ok = index_is(&index, rec.base.as_ref())
        || (rec.creation && index_is(&index, rec.candidate.as_ref()));
    if branch != rec.branch || at_head != rec.base || !index_ok {
        require_repair(
            rec,
            "target_changed",
            "the path moved in git since the write; its candidate cannot be committed on top",
        );
        let _ = publish(project, lock, rec);
        return Ok(repair_report(rec));
    }
    // A later commit can have changed what makes the path eligible
    // (attributes, configuration) without touching it.
    let creation = match (&rec.candidate, rec.creation) {
        (Some(c), true) => git::Creation::Recovering(c),
        _ => git::Creation::No,
    };
    let pf = git::preflight(&repo, &rec.path, &file, creation);
    if let Some(i) = pf.ineligible {
        require_repair(rec, i.reason, &i.detail);
        let _ = publish(project, lock, rec);
        return Ok(repair_report(rec));
    }
    if !recovery_preflight_matches(&pf, &rec.branch, &head, rec.base.as_ref()) {
        require_repair(
            rec,
            "target_changed",
            "the branch, HEAD or the path moved while recovery read them",
        );
        let _ = publish(project, lock, rec);
        return Ok(repair_report(rec));
    }
    if rec.require_commit
        && crate::commit::per_write(
            crate::schema::load_config(project)
                .map_err(Diagnostic::detail)?
                .git
                .as_ref(),
        ) != Some(OnDirty::Refuse)
    {
        require_repair(
            rec,
            "commit_policy_changed",
            "require_commit policy is no longer per-write/refuse",
        );
        publish(project, lock, rec).map_err(Diagnostic::detail)?;
        return Ok(repair_report(rec));
    }
    for trailer in &rec.trailers {
        if let Err((reason, detail)) = trailer.check_recovery(project, lock, git, rec, &file) {
            require_repair(rec, reason, &detail);
            publish(project, lock, rec).map_err(Diagnostic::detail)?;
            return Ok(repair_report(rec));
        }
    }
    let rebound = (|| {
        let trailers = rec
            .trailers
            .iter()
            .map(|t| t.rebind(&head, pf.head_entry.as_ref(), rec.candidate.as_ref()))
            .collect::<Result<Vec<_>, _>>()?;
        let mut rebound = rec.clone();
        rebound.trailers = trailers;
        if !rebound.trailers.is_empty() {
            let user = rebound
                .user_message
                .as_deref()
                .ok_or("receipt has no separate user message")?;
            check_message(user).map_err(Diagnostic::detail)?;
            rebound.message = Some(rebound.compose(Some(user)).map_err(Diagnostic::detail)?);
        }
        Ok::<_, String>(rebound)
    })();
    match rebound {
        Ok(rebound) => *rec = rebound,
        Err(e) => {
            require_repair(rec, "delete_receipt_rebind", &e);
            publish(project, lock, rec).map_err(Diagnostic::detail)?;
            return Ok(repair_report(rec));
        }
    }
    if rec.delete_receipt().is_some() {
        project
            .store_put(
                lock,
                Some(PENDING),
                &rec.msg_name(),
                rec.message.as_deref().unwrap_or_default().as_bytes(),
            )
            .map_err(Diagnostic::detail)?;
    }
    if rec
        .expect_head
        .as_ref()
        .is_some_and(|expected| expected != &head)
    {
        return Ok(guard_head_moved(project, lock, rec));
    }
    rec.base_head = Some(head);
    rec.phase = Some(Phase::Applied);
    rec.blocked = false;
    rec.repair = None;
    if rec.delete_receipt().is_some() || rec.require_commit {
        publish(project, lock, rec).map_err(Diagnostic::detail)?;
    } else {
        let _ = publish(project, lock, rec);
    }
    let (v, failed) = commit_record(project, lock, git, rec, true);
    Ok(recovered_report(rec, v, failed))
}

fn repair_report(rec: &Record) -> Value {
    let mut v = report(rec, "repair_required");
    if let Some(r) = rec.repair() {
        v["reason"] = r["reason"].clone();
        v["detail"] = r["detail"].clone();
    }
    v
}

fn recovered_report(rec: &Record, commit: Value, receipt_failed: bool) -> Value {
    let status = commit["status"].as_str().unwrap_or("failed").to_string();
    let mut v = report(
        rec,
        if status == "failed" {
            "failed"
        } else {
            &status
        },
    );
    if commit["record_left"] == true {
        v["record_left"] = json!(true);
    }
    v["commit"] = commit;
    if receipt_failed {
        v["receipt"] = json!("failed");
    }
    v
}

/// Retire one record as `abandoned`, carrying the application certainty it
/// had: never inventing a write, never dropping a confirmed one.
fn abandon(
    project: &Project,
    lock: &WriterLock,
    rec: Record,
    reason: &str,
) -> Result<Value, Diagnostic> {
    if let Some(pid) = rec.pid
        && !rec.child().dead()
    {
        return Err(Diagnostic::new(
            ErrorCode::PendingUnresolved,
            format!("record {}'s git child {pid} is still running", rec.op),
        ));
    }
    let receipts = Receipts::for_pending(project, [rec.op.as_str()])?;
    if let Some(e) = receipts.terminal(&rec.op) {
        return Ok(reconcile(project, lock, &rec, e));
    }
    if rec.peer.is_some() {
        return pair::abandon_pair(project, lock, rec, reason);
    }
    let differs = rec.read_sha256 != rec.candidate_sha256;
    let applied = if rec.ever_applied
        || matches!(
            rec.state(),
            State::Applied | State::Landed | State::Unverified | State::Committed
        ) {
        Applied::Yes
    } else if rec.fresh() {
        // Nothing was there to read: absence of the file and of an index
        // entry is what proves the creation never landed.
        match project.read_raw(&rec.path) {
            Ok(Some((bytes, _))) if sha256(&bytes) == rec.candidate_sha256 => Applied::Yes,
            Ok(None) if creation_unstaged(project, &rec) => Applied::No,
            _ => Applied::Unknown,
        }
    } else {
        match file_is(project, &rec) {
            Bytes::Candidate if differs => Applied::Yes,
            Bytes::Read if differs => Applied::No,
            _ => Applied::Unknown,
        }
    };
    let mut e = event(&rec, Outcome::Abandoned, applied, true);
    e.reason = Some(reason.to_string());
    finish_abandon(project, lock, &rec, &e)
}

fn finish_abandon(
    project: &Project,
    lock: &WriterLock,
    rec: &Record,
    e: &Event,
) -> Result<Value, Diagnostic> {
    let concluded = conclude(project, lock, rec, e);
    let mut v = report(rec, "abandoned");
    v["reason"] = json!(e.reason);
    v["applied"] = json!(e.applied);
    if let Some(peer_applied) = e.peer_applied {
        v["peer_applied"] = json!(peer_applied);
    }
    match concluded {
        Concluded::Retired => {}
        Concluded::RecordLeft(error) => {
            v["record_left"] = json!(true);
            v["error"] = json!(error);
        }
        Concluded::ReceiptFailed(error) => return Err(error),
    }
    Ok(v)
}

/// A creation's path has no index entry and nothing at HEAD. A git
/// failure answers `false`, which leaves the abandonment `unknown`.
fn creation_unstaged(project: &Project, rec: &Record) -> bool {
    let git = Git::new(project);
    git::head_state(&git)
        .ok()
        .and_then(|(_, head)| head)
        .and_then(|head| git::path_entries(&git, &head, &rec.path).ok())
        .is_some_and(|(at_head, index)| at_head.is_none() && index.is_empty())
}

/// Pending records and receipts for `status`: per path, the unresolved
/// records and the last write, plus the log's totals.
pub struct Bookkeeping {
    pending_complete: bool,
    pub records: Vec<Record>,
    pub receipts: Receipts,
    pub diagnostics: Vec<Diagnostic>,
}

impl Bookkeeping {
    pub fn read(project: &Project) -> Bookkeeping {
        let mut diagnostics = vec![];
        let (records, pending_complete) = match records(project) {
            Ok((r, errors)) => {
                let complete = errors.is_empty();
                diagnostics.extend(errors);
                (r, complete)
            }
            Err(e) => {
                diagnostics.push(e);
                (vec![], false)
            }
        };
        let receipts = Receipts::read(project).unwrap_or_else(|e| {
            diagnostics.push(e);
            Receipts::default()
        });
        Bookkeeping {
            pending_complete,
            records,
            receipts,
            diagnostics,
        }
    }

    pub fn paths(&self) -> BTreeSet<String> {
        self.records
            .iter()
            .flat_map(|r| r.paths().map(str::to_owned))
            .collect()
    }

    /// The `pending`, `last_write` and `owner` fields of one path's report.
    pub fn for_path(&self, path: &str) -> Value {
        let pending: Vec<Value> = self
            .records
            .iter()
            .filter(|r| r.paths().any(|p| p == path))
            .map(|r| {
                let mut v = json!({
                    "op": r.op, "seq": r.seq, "state": r.state(), "outcome": r.outcome,
                    "session": r.session, "created": r.created,
                });
                if let Some(reason) = &r.reason {
                    v["reason"] = json!(reason);
                }
                if let Some(repair) = r.repair() {
                    v["repair"] = repair.clone();
                }
                if let Some(pid) = r.pid {
                    v["pid"] = json!(pid);
                }
                v
            })
            .collect();
        let last = self.receipts.last_writes().get(path).map(|e| (*e).clone());
        let owner = match (
            self.records.iter().find(|r| r.paths().any(|p| p == path)),
            &last,
        ) {
            (Some(r), _) => format!("pending:{}", r.session.as_deref().unwrap_or("")),
            (None, Some(e)) => format!("receipt:{}", e.session.as_deref().unwrap_or("")),
            (None, None) => "unknown".into(),
        };
        json!({"pending": pending, "last_write": last, "owner": owner})
    }

    /// Landed-write totals by outcome (`applied: true` only), the
    /// `applied: "unknown"` outcomes by session, and records by state.
    pub fn summary(&self) -> Value {
        let mut writes: BTreeMap<&str, u64> = BTreeMap::new();
        let mut unknown: BTreeMap<String, u64> = BTreeMap::new();
        let mut total = 0;
        for e in self.receipts.latest().into_values() {
            match e.applied {
                Applied::Yes => {
                    total += 1;
                    *writes.entry(e.outcome.name()).or_default() += 1;
                }
                Applied::Unknown => {
                    *unknown
                        .entry(e.session.clone().unwrap_or_default())
                        .or_default() += 1;
                }
                Applied::No => {}
            }
        }
        let mut states: BTreeMap<String, u64> = BTreeMap::new();
        for r in &self.records {
            let s = serde_json::to_value(r.state()).expect("state");
            *states.entry(s.as_str().unwrap_or("").into()).or_default() += 1;
        }
        json!({
            "writes": total,
            "by_outcome": writes,
            "unknown_by_session": unknown,
            "pending": states,
            "unreadable_lines": self.receipts.unreadable,
        })
    }

    /// Prefer a matching durable pair fence before legacy child-time attribution.
    pub fn lock_owner(&self, lock: &git::LockFile) -> Option<String> {
        if lock.file == ".git/index.lock" && !self.pending_complete {
            return None;
        }
        attributed_record(&self.records, lock)
    }
}

#[cfg(test)]
mod record_tests {
    use super::*;

    #[test]
    fn pending_receipts_keep_only_selected_terminals_and_ignore_failed_or_partial_events() {
        let root = crate::test_support::scratch("pending-receipts");
        let project = Project::init(&root).unwrap();
        let line = |op: &str, outcome: &str| {
            json!({
                "op":op, "seq":1, "path":"docs/example.md", "outcome":outcome,
                "applied":true, "at":1
            })
            .to_string()
                + "\n"
        };
        let bytes = line("old", "skipped")
            + &line("duplicate", "skipped")
            + &line("other", "committed")
            + "bad json\n"
            + &line("failed", "failed")
            + &line("duplicate", "committed")
            + line("partial", "committed").trim_end();
        std::fs::write(root.join(".rivet/commits.jsonl"), bytes).unwrap();
        let found =
            Receipts::for_pending(&project, ["old", "duplicate", "failed", "partial"]).unwrap();
        assert_eq!(found.events.len(), 2);
        assert_eq!(found.terminal("old").unwrap().outcome, Outcome::Skipped);
        assert_eq!(
            found.terminal("duplicate").unwrap().outcome,
            Outcome::Committed
        );
        assert!(found.terminal("failed").is_none());
        assert!(found.terminal("partial").is_none());
        assert!(found.terminal("other").is_none());
        assert_eq!(found.unreadable, 1);
        // No pending ops means no read, even if the old log is inaccessible.
        std::fs::remove_file(root.join(".rivet/commits.jsonl")).unwrap();
        std::os::unix::fs::symlink("missing", root.join(".rivet/commits.jsonl")).unwrap();
        assert!(
            Receipts::for_pending(&project, [])
                .unwrap()
                .events
                .is_empty()
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn installed_engine_record_and_delete_trailer_keep_their_wire_bytes() {
        // Captured from the installed engine in a synthetic project, with
        // runtime PID, boot, inode and timing fields replaced by synthetic values.
        let value: Value =
            serde_json::from_str(include_str!("../tests/fixtures/pending_6d3ce8e.json")).unwrap();
        let record: Record = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(serde_json::to_value(&record).unwrap(), value);
        assert_eq!(
            record.compose(record.user_message.as_deref()).unwrap(),
            record.message.as_deref().unwrap()
        );
    }

    #[test]
    fn pre_move_single_path_record_keeps_exact_serialized_bytes() {
        // Produced by the unmodified 04560d1 Record codec from the existing
        // installed-engine synthetic fixture, rather than this serializer.
        let wire = include_str!("../tests/fixtures/pending_04560d1.compact.json");
        let record: Record = serde_json::from_str(wire).unwrap();
        assert_eq!(serde_json::to_string(&record).unwrap(), wire);
        assert_eq!(record.paths().collect::<Vec<_>>(), [record.path.as_str()]);
        assert!(record.peer.is_none() && record.fence.is_none() && !record.fence_waived);
    }

    #[test]
    fn guarded_adopt_record_round_trip_and_republication_keep_the_expected_head() {
        let wire = include_str!("../tests/fixtures/pending_04560d1.compact.json");
        let plain: Record = serde_json::from_str(wire).unwrap();
        assert!(plain.expect_head.is_none());
        assert_eq!(serde_json::to_string(&plain).unwrap(), wire);
        let mut record = plain;
        record.expect_head = record.base_head.clone();
        let encoded = serde_json::to_string(&record).unwrap();
        assert_eq!(encoded.matches("\"expect_head\"").count(), 1);
        let mut decoded: Record = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded.expect_head, record.base_head);
        let root = crate::test_support::scratch("guard-record-codec");
        let project = Project::init(&root).unwrap();
        let lock = project.lock(crate::fs::DEFAULT_LOCK_TIMEOUT).unwrap();
        publish(&project, &lock, &decoded).unwrap();
        decoded = records(&project).unwrap().0.remove(0);
        assert_eq!(decoded.expect_head, record.base_head);
        publish(&project, &lock, &decoded).unwrap();
        assert_eq!(
            records(&project).unwrap().0[0].expect_head,
            record.base_head
        );
    }

    #[test]
    fn guarded_adopt_commit_choke_point_precedes_even_unchanged_in_git() {
        for equal in [false, true] {
            let root = crate::test_support::scratch("guard-commit-choke");
            let home = root.join("home");
            std::fs::create_dir_all(&home).unwrap();
            let git_fixture = |args: &[&str]| {
                let out = crate::test_support::git(&root, &home, args)
                    .output()
                    .unwrap();
                assert!(out.status.success(), "{args:?}: {:?}", out.stderr);
                String::from_utf8(out.stdout).unwrap().trim().to_string()
            };
            git_fixture(&["init", "-q", "-b", "main"]);
            git_fixture(&["config", "user.name", "Tester"]);
            git_fixture(&["config", "user.email", "tester@example.invalid"]);
            std::fs::write(root.join("page.md"), "# Base\n").unwrap();
            git_fixture(&["add", "page.md"]);
            git_fixture(&["commit", "-qm", "base"]);
            let head = git_fixture(&["rev-parse", "HEAD"]);
            let index = std::fs::read(root.join(".git/index")).unwrap();
            let project = Project::init(&root).unwrap();
            let lock = project.lock(crate::fs::DEFAULT_LOCK_TIMEOUT).unwrap();
            let git = Git::new(&project);
            let (base, _) = git::path_entries(&git, &head, "page.md").unwrap();
            let mut record: Record = serde_json::from_value(json!({
                "op":"0123456789abcdef", "seq":1, "path":"page.md", "state":"applied",
                "outcome":"commit", "read_sha256":sha256(b"# Base\n"),
                "candidate_sha256":sha256(b"# Candidate\n"), "created":1,
                "branch":"refs/heads/main", "base_head":head, "expect_head":"a".repeat(40),
                "base":base, "candidate":base, "message":"checked adopt\n\nRivet-Op: 0123456789abcdef\n",
                "ever_applied":true
            })).unwrap();
            if !equal {
                let blob = git.store_blob(b"# Candidate\n").unwrap();
                assert!(blob.ok());
                record.candidate = Some(Entry {
                    oid: String::from_utf8(blob.stdout).unwrap().trim().into(),
                    mode: "100644".into(),
                });
            }
            publish(&project, &lock, &record).unwrap();
            let (out, failed_receipt) = commit_record(&project, &lock, &git, &mut record, false);
            assert!(!failed_receipt);
            assert_eq!(out["status"], "repair_required", "{out}");
            assert_eq!(out["reason"], "guard_head_moved");
            assert_eq!(out["pending"], record.op);
            assert_eq!(record.state(), State::RepairRequired);
            let stored = records(&project).unwrap().0.remove(0);
            assert_eq!(stored.state(), State::RepairRequired);
            assert_eq!(stored.base_head.as_deref(), Some(head.as_str()));
            assert_eq!(stored.expect_head, record.expect_head);
            assert_eq!(stored.repair().unwrap()["reason"], "guard_head_moved");
            assert_eq!(git_fixture(&["rev-parse", "HEAD"]), head);
            assert_eq!(std::fs::read(root.join(".git/index")).unwrap(), index);
            assert_eq!(std::fs::read(root.join("page.md")).unwrap(), b"# Base\n");
            assert!(!root.join(".rivet/commits.jsonl").exists());
        }
    }

    fn stored() -> Value {
        json!({
            "op":"synthetic-op", "seq":1, "path":"tracking/example.md",
            "state":"prepared", "outcome":"commit", "read_sha256":"read",
            "candidate_sha256":"candidate", "created":1,
            "ever_applied":false, "retries":0, "git_ms":0,
            "archive_copy": {"version":1, "op":"synthetic-op", "copy":{
                "source":"tracking/source.md", "id":"source01", "guard":"revision",
                "path":"tracking/example.md", "item_sha256":"item", "ids":["source01"]}},
            "archive":{"path":"archive/example.md", "copy_op":"copy-op", "commit":"commit"},
            "policy_guard":"policy-revision"
        })
    }

    #[test]
    fn item_archive_wire_matches_b79930c_bytes() {
        for bytes in [
            include_str!("../tests/fixtures/archive_item_copy_pending_b79930c.json"),
            include_str!("../tests/fixtures/archive_item_delete_pending_b79930c.json"),
        ] {
            let record: Record = serde_json::from_str(bytes).unwrap();
            assert_eq!(serde_json::to_vec(&record).unwrap(), bytes.as_bytes());
        }
        let bytes = include_str!("../tests/fixtures/archive_item_receipt_b79930c.json");
        let receipt: crate::archive::CopyReceipt = serde_json::from_str(bytes).unwrap();
        assert_eq!(serde_json::to_vec(&receipt).unwrap(), bytes.as_bytes());
    }

    #[test]
    fn legacy_record_fields_and_states_round_trip_without_a_new_wire_shape() {
        for state in [
            "prepared",
            "applied",
            "landed",
            "unverified",
            "committed",
            "repair_required",
        ] {
            for detail in [None, Some(json!({"reason":"stale_git_lock", "locks":[]}))] {
                let mut value = stored();
                value["state"] = json!(state);
                if let Some(detail) = detail {
                    value["repair"] = detail;
                }
                let record: Record = serde_json::from_value(value.clone()).unwrap();
                assert_eq!(serde_json::to_value(record).unwrap(), value);
            }
        }
        let mut unknown = stored();
        unknown["future_evidence"] = json!({});
        assert!(serde_json::from_value::<Record>(unknown).is_err());
    }

    #[test]
    fn duplicate_evidence_is_refused_instead_of_dropped_by_the_legacy_adapter() {
        let mut record: Record = serde_json::from_value(stored()).unwrap();
        record.trailers.push(record.trailers[0].clone());
        assert!(serde_json::to_value(record).is_err());
        let mut record: Record = serde_json::from_value(stored()).unwrap();
        record.guards.push(record.guards[0].clone());
        assert!(serde_json::to_value(record).is_err());
    }
}
