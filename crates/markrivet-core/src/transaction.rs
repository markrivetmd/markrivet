//! Shared locked write lifecycle. Operation planners supply verified candidates;
//! this module owns publication and the exceptions for previews and history.
use crate::api::{self, Response};
use crate::commit;
use crate::document::Document;
use crate::edit;
use crate::fs::{Commit, DEFAULT_LOCK_TIMEOUT, MAX_FILE_BYTES, Project, hook};
use crate::git::OnDirty;
use crate::schema::{self, Bound};
use crate::{Diagnostic, ErrorCode};
use serde_json::{Value, json};
use std::time::Duration;

#[cfg(test)]
#[path = "../tests/transaction/internal.rs"]
mod tests;

pub(crate) struct Attribution {
    pub message: Option<String>,
    pub session: Option<String>,
}

#[derive(Clone, Copy)]
pub(crate) enum Kind<'a> {
    Edit,
    Create(&'a str),
    Adopt(Option<&'a str>),
    ArchiveCopy,
}

#[derive(Clone, Copy, Default)]
pub(crate) enum Pending {
    #[default]
    Settle,
    /// History must observe the caller's HEAD without implicit recovery.
    Refuse,
}

pub(crate) struct Options<'a> {
    pub kind: Kind<'a>,
    pub preview: bool,
    pub timeout: Option<u64>,
    pub by: Attribution,
    pub require_commit: bool,
    pub pending: Pending,
}

pub(crate) struct Context<'a> {
    project: &'a Project,
    path: &'a str,
    kind: Kind<'a>,
}

pub(crate) struct Loaded {
    pub doc: Document,
    pub bindings: Vec<Bound>,
}

impl Context<'_> {
    /// Called within the transaction's lock (or without a lock for preview).
    /// Archive calls this after its source proof and idempotency check, keeping
    /// retries independent of the stale destination guard and selector.
    pub fn load(&self) -> Result<Loaded, Diagnostic> {
        let doc = match self.kind {
            Kind::Create(content) => edit::check_new_document(content),
            _ => self
                .project
                .read(self.path)
                .and_then(|s| Document::parse(&s)),
        }
        .map_err(|e| e.with_path(self.path))?;
        edit::check_write_blocker(&doc).map_err(|e| e.with_path(self.path))?;
        let bindings = if matches!(self.kind, Kind::ArchiveCopy) {
            // Archive proves its source binding, not a typed destination edit.
            vec![]
        } else {
            api::bound_for(self.project, self.path)?
        };
        Ok(Loaded { doc, bindings })
    }
}

pub(crate) enum Plan {
    Ready(Box<Draft>),
    /// A proven restore noop or archive retry has its own response shape.
    Unchanged(Value),
}

pub(crate) struct Draft {
    pub before: Loaded,
    pub candidate: String,
    pub body: Value,
    pub delete_intent: Option<crate::delete_receipt::DeleteIntent>,
    pub policy_guard: Option<String>,
    pub archive: Option<crate::archive::ArchiveGuard>,
    pub archive_copy: Option<crate::archive::CopyIntent>,
    pub archive_parent: Option<String>,
    pub restore: Option<crate::history::Assertion>,
    pub message: Option<String>,
}

impl Draft {
    pub fn new(before: Loaded, candidate: String, body: Value) -> Self {
        Self {
            before,
            candidate,
            body,
            delete_intent: None,
            policy_guard: None,
            archive: None,
            archive_copy: None,
            archive_parent: None,
            restore: None,
            message: None,
        }
    }

    pub fn ready(self) -> Plan {
        Plan::Ready(Box::new(self))
    }
}

/// All document writers enter here, including restore via the edit planner.
pub(crate) fn transact(
    project: &Project,
    path: &str,
    opts: Options<'_>,
    plan_fn: impl FnOnce(&Context<'_>) -> Result<Plan, Diagnostic>,
) -> Response {
    transact_with_source(project, path, opts, None, plan_fn)
}

pub(crate) fn transact_with_source(
    project: &Project,
    path: &str,
    opts: Options<'_>,
    source: Option<&str>,
    plan_fn: impl FnOnce(&Context<'_>) -> Result<Plan, Diagnostic>,
) -> Response {
    if let Some(message) = &opts.by.message
        && let Err(e) = commit::check_message(message)
    {
        return api::refused(e);
    }
    let lock = if opts.preview {
        None
    } else {
        match project.lock(
            opts.timeout
                .map_or(DEFAULT_LOCK_TIMEOUT, Duration::from_millis),
        ) {
            Ok(lock) => Some(lock),
            Err(e) => return api::refused(e),
        }
    };
    if let Err(e) = hook("locked") {
        return api::refused(e);
    }
    let mut config_error = None;
    let commit_mode = if lock.is_some() || matches!(opts.pending, Pending::Refuse) {
        let config = match schema::load_config(project) {
            Err(e) if matches!(opts.kind, Kind::ArchiveCopy) => {
                config_error = Some(e);
                None
            }
            config => config.ok(),
        };
        config.and_then(|c| commit::per_write(c.git.as_ref()))
    } else {
        None
    };
    if lock.is_some()
        && let Err(e) = commit::pair_barrier(
            project,
            lock.as_ref(),
            Some(path),
            commit_mode.is_some() && matches!(opts.pending, Pending::Settle),
        )
    {
        return api::refused(e);
    }
    if let Some(source) = source
        && lock.is_some()
        && let Err(e) =
            commit::pair_barrier(project, lock.as_ref(), Some(source), commit_mode.is_some())
    {
        return api::refused(e);
    }
    if let Some(e) = config_error {
        return api::refused(e);
    }
    let policy_error = match opts.kind {
        Kind::Adopt(_) if commit_mode.is_none() => Some(Diagnostic::new(
            ErrorCode::InvalidRequest,
            "adopt commits a file; it needs \"git\": {\"commit\": \"per-write\"} in .rivet/project.json",
        )),
        Kind::ArchiveCopy if commit_mode != Some(OnDirty::Refuse) => Some(Diagnostic::new(
            ErrorCode::ArchiveCopyRefused,
            "archive-copy requires per-write with on_dirty refuse",
        )),
        _ if opts.require_commit && !opts.preview && commit_mode != Some(OnDirty::Refuse) => {
            Some(Diagnostic::new(
                ErrorCode::CommitRefused,
                "require_commit needs per-write with on_dirty refuse",
            ))
        }
        _ => None,
    };
    if let Some(e) = policy_error {
        return api::refused(e);
    }
    let pending = match opts.pending {
        Pending::Refuse => crate::history::no_pending(
            project,
            path,
            if matches!(opts.kind, Kind::Adopt(_)) {
                "guarded adopt"
            } else {
                "restore"
            },
        ),
        Pending::Settle => match (commit_mode, &lock) {
            (Some(_), Some(lock)) => commit::settle(project, lock, path),
            _ => Ok(()),
        },
    };
    if let Err(e) = pending {
        return api::refused(e);
    }
    let context = Context {
        project,
        path,
        kind: opts.kind,
    };
    let mut draft = match plan_fn(&context) {
        Ok(Plan::Ready(draft)) => *draft,
        Ok(Plan::Unchanged(body)) => return api::ok(body),
        Err(e) => return api::refused(e),
    };
    if draft.candidate.len() as u64 > MAX_FILE_BYTES {
        let e = match opts.kind {
            Kind::ArchiveCopy => Diagnostic::new(
                ErrorCode::ArchiveCopyRefused,
                "archive candidate exceeds 8 MiB",
            ),
            _ => {
                let subject = match opts.kind {
                    Kind::Create(_) => "document",
                    Kind::Adopt(_) => "stamped file",
                    _ => "result",
                };
                Diagnostic::new(ErrorCode::LimitExceeded, format!("the {subject} would exceed {MAX_FILE_BYTES} bytes, the largest file Rivet reads")).with_path(path)
            }
        };
        return api::refused(e);
    }
    let creation = matches!(opts.kind, Kind::Create(_));
    if creation {
        match project.read_raw(path) {
            Ok(Some(_)) => {
                return api::refused(
                    Diagnostic::new(ErrorCode::AlreadyExists, format!("{path:?} already exists"))
                        .with_path(path),
                );
            }
            Err(e) => return api::refused(e),
            Ok(None) => {}
        }
    }
    let changed = creation || draft.candidate != draft.before.doc.source;
    if opts.preview || (!changed && !matches!(opts.kind, Kind::Adopt(_))) {
        draft.body["status"] = json!(if opts.preview { "preview" } else { "unchanged" });
        draft.body["changed"] = json!(changed);
        if !changed {
            draft.body["revision"] = json!(draft.before.doc.revision());
        }
        return api::ok(draft.body);
    }
    if let Err(e) = hook("planned") {
        return api::refused(e);
    }
    if let Some(copy) = &draft.archive_copy
        && let Err(e) = crate::archive::check_source(
            project,
            copy,
            draft.archive_parent.as_deref(),
            Some(&draft.candidate),
        )
    {
        return api::refused(e);
    }
    let lock = lock.expect("non-preview writes hold the lock");
    if let Some(on_dirty) = commit_mode {
        let mut w = commit::Write::new(project, &lock, path, on_dirty);
        w.session = opts.by.session;
        w.message = draft.message.or(opts.by.message);
        w.bound = draft.before.bindings;
        w.adopting = matches!(opts.kind, Kind::Adopt(_));
        if w.adopting {
            w.on_dirty = OnDirty::Adopt;
        }
        w.expect_head = match opts.kind {
            Kind::Adopt(head) => head,
            _ => None,
        };
        w.require_commit = opts.require_commit;
        w.policy_guard = draft.policy_guard;
        w.archive = draft.archive;
        w.archive_copy = draft.archive_copy;
        w.archive_parent = draft.archive_parent;
        w.delete_intent = draft.delete_intent;
        let landing = if creation {
            commit::create(&w, &draft.candidate)
        } else if let Some(assertion) = &draft.restore {
            commit::land_restore(&w, &draft.before.doc.source, &draft.candidate, assertion)
        } else {
            commit::land(&w, &draft.before.doc.source, &draft.candidate)
        };
        return match landing {
            Ok(done) if w.adopting && done.commit.is_null() => {
                draft.body["status"] = json!("unchanged");
                draft.body["changed"] = json!(false);
                draft.body["revision"] = json!(draft.before.doc.revision());
                api::ok(draft.body)
            }
            Ok(done) => api::commit_landed(draft.body, done),
            Err((e, pending)) => api::commit_refused(e, path, pending),
        };
    }
    let landing = if creation {
        project.create(&lock, path, &draft.candidate)
    } else {
        project.replace(&lock, path, &draft.before.doc.source, &draft.candidate)
    };
    match landing {
        Ok(Commit::Applied) => api::landed(draft.body, true),
        Ok(Commit::AppliedNotDurable) => api::landed(draft.body, false),
        Err(e) => api::refused(e.with_path(path)),
    }
}

pub(crate) struct PairDraft {
    pub source: String,
    pub source_candidate: String,
    pub destination: String,
    pub destination_candidate: String,
    pub receipt: crate::relocate::MoveReceipt,
}

pub(crate) enum PairPlan {
    Ready(Box<PairDraft>),
    Unchanged(Value),
}

pub(crate) fn transact_pair(
    project: &Project,
    source: &str,
    destination: &str,
    timeout: Option<u64>,
    by: Attribution,
    planner: impl FnOnce(&schema::ProjectConfig) -> Result<PairPlan, Diagnostic>,
) -> Response {
    let run = || -> Result<Response, Diagnostic> {
        crate::delete_receipt::require_canonical(source)?;
        crate::delete_receipt::require_canonical(destination)?;
        if source == destination {
            return Err(Diagnostic::new(
                ErrorCode::InvalidRequest,
                "move paths must differ",
            ));
        }
        if let Some(message) = &by.message {
            commit::check_message(message)?;
        }
        let lock = project.lock(timeout.map_or(DEFAULT_LOCK_TIMEOUT, Duration::from_millis))?;
        hook("locked")?;
        let config = schema::load_config(project);
        let policy = config.as_ref().is_ok_and(crate::relocate::commit_policy);
        commit::pair_barrier(project, Some(&lock), Some(source), policy)?;
        commit::pair_barrier(project, Some(&lock), Some(destination), policy)?;
        if !policy {
            return Err(Diagnostic::new(
                ErrorCode::CommitRefused,
                "move requires per-write with on_dirty refuse",
            ));
        }
        commit::settle(project, &lock, source)?;
        commit::settle(project, &lock, destination)?;
        let draft = match planner(&config?)? {
            PairPlan::Unchanged(value) => return Ok(api::ok(value)),
            PairPlan::Ready(draft) => draft,
        };
        for candidate in [&draft.source_candidate, &draft.destination_candidate] {
            if candidate.len() as u64 > MAX_FILE_BYTES {
                return Err(Diagnostic::new(
                    ErrorCode::LimitExceeded,
                    "move candidate exceeds 8 MiB",
                ));
            }
        }
        hook("planned")?;
        let landing = commit::land_pair(
            project,
            &lock,
            commit::PairFile {
                path: source,
                read: &draft.source,
                candidate: &draft.source_candidate,
            },
            commit::PairFile {
                path: destination,
                read: &draft.destination,
                candidate: &draft.destination_candidate,
            },
            draft.receipt,
            by.message,
            by.session,
        );
        Ok(match landing {
            Ok(done) if !done.durable => Response {
                body: json!({"ok":true, "status":"pending", "durable":false,
                    "source":source, "destination":destination, "commit":done.commit}),
                exit: 3,
            },
            Ok(done) => {
                api::commit_landed(json!({"source":source, "destination":destination}), done)
            }
            Err((error, pending)) => api::commit_refused(error, source, pending),
        })
    };
    match run() {
        Ok(r) => r,
        Err(e) => api::refused(e),
    }
}
