//! File-scoped first-parent evidence and pure restore preparation.
//! Reads never create persistent state or settle pending records.
use crate::document::{Document, Target};
use crate::fs::{MAX_FILE_BYTES, Project};
use crate::git::{Git, GitError, ReadBudget, Repo};
use crate::git_object::{OidCase, parse_commit, valid_oid};
use crate::{Diagnostic, ErrorCode, Selector};
use serde::{Serialize, Serializer};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::io::Write;
use std::sync::{Arc, atomic::Ordering};

const MIB: usize = 1024 * 1024;
const MAX_COMMITS: usize = 10_000;
const MAX_BLOBS: usize = 1_000;
const MAX_BYTES: usize = 64 * MIB;
/// Debug tests lower the shared history budget without a 64 MiB fixture.
fn max_bytes() -> usize {
    #[cfg(debug_assertions)]
    if let Some(limit) = std::env::var("RIVET_TEST_HISTORY_BYTES")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .filter(|n| *n > ENVELOPE_RESERVE && *n < MAX_BYTES)
    {
        return limit;
    }
    MAX_BYTES
}
const MAX_METADATA: usize = 256 * 1024;
/// Debug tests lower the per-commit object bound independently of the request budget.
fn max_metadata() -> usize {
    #[cfg(debug_assertions)]
    if let Some(limit) = std::env::var("RIVET_TEST_HISTORY_METADATA_BYTES")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .filter(|n| *n > 0 && *n < MAX_METADATA)
    {
        return limit;
    }
    MAX_METADATA
}
// Leave room for counters, diagnostics, punctuation, and the API's ok field.
const ENVELOPE_RESERVE: usize = 16 * 1024;

#[derive(Clone, Debug, Serialize, PartialEq)]
struct State {
    status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    revision: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    kind: Option<String>,
    ids: Vec<String>,
    location: Option<Value>,
    // Structural history alone does not certify current-schema restoration.
    replayable: bool,
}

impl State {
    fn empty(status: &'static str, reason: Option<&'static str>) -> Self {
        Self {
            status,
            reason,
            content: None,
            revision: None,
            kind: None,
            ids: vec![],
            location: None,
            replayable: false,
        }
    }
}

fn shared<S: Serializer>(state: &Arc<State>, serializer: S) -> Result<S::Ok, S::Error> {
    state.as_ref().serialize(serializer)
}

#[derive(Clone, Debug, Serialize, PartialEq)]
struct Snapshot {
    blob: Option<String>,
    mode: Option<String>,
    #[serde(serialize_with = "shared")]
    state: Arc<State>,
}

impl Snapshot {
    fn absent() -> Self {
        Self {
            blob: None,
            mode: None,
            state: Arc::new(State::empty("absent", None)),
        }
    }
    fn gap(reason: &'static str) -> Self {
        Self {
            blob: None,
            mode: None,
            state: Arc::new(State::empty("gap", Some(reason))),
        }
    }
}

#[derive(Serialize)]
struct Transition {
    commit: String,
    first_parent: Option<String>,
    before_rev: Option<String>,
    after_rev: Option<String>,
    event: &'static str,
    integration: bool,
    author: String,
    committer: String,
    message: String,
    // Git metadata is forgeable. No receipt or creator Session is inferred.
    writer_session: Option<String>,
    before: Snapshot,
    after: Snapshot,
}

#[derive(Default, Serialize)]
struct Coverage {
    complete: bool,
    root_reached: bool,
    shallow: bool,
    commits_scanned: usize,
    distinct_blobs: usize,
    decoded_bytes: usize,
    git_received_bytes: usize,
    retained_detail_bytes: usize,
    compact_response_bytes: usize,
    pretty_response_bytes: usize,
    boundary: Option<String>,
    diagnostics: Vec<Value>,
}

enum Failure {
    Limit(&'static str),
    Gap(&'static str),
}

struct Reader {
    git: Git,
    oid_len: usize,
    cache: BTreeMap<String, Arc<State>>,
    coverage: Coverage,
}

impl Reader {
    fn command(&self, args: &[&str], cap: usize) -> Result<Vec<u8>, Failure> {
        let out = self.git.inspect_limit(args, cap).map_err(|e| match e {
            GitError::OutputLimit(b) => Failure::Limit(b),
            GitError::Deadline => Failure::Gap("git_deadline"),
            _ => Failure::Gap("git_unavailable"),
        })?;
        if !out.ok() {
            return Err(Failure::Gap("missing_object"));
        }
        Ok(out.stdout)
    }

    fn object(&self, kind: &str, oid: &str, cap: usize) -> Result<Vec<u8>, Failure> {
        if !valid_oid(oid, self.oid_len, OidCase::Canonical) {
            return Err(Failure::Gap("invalid_object_id"));
        }
        let size = self.command(&["cat-file", "-s", oid], 64)?;
        let size = std::str::from_utf8(&size)
            .ok()
            .and_then(|s| s.trim().parse::<usize>().ok())
            .ok_or(Failure::Gap("invalid_object_size"))?;
        if size > cap {
            return Err(Failure::Limit(if kind == "blob" {
                "blob_bytes"
            } else {
                "metadata_bytes"
            }));
        }
        let bytes = self.command(&["cat-file", kind, oid], cap)?;
        if bytes.len() != size {
            return Err(Failure::Gap("object_size_changed"));
        }
        Ok(bytes)
    }

    fn snapshot(&mut self, rev: &str, path: &str, id: &str) -> Result<Snapshot, Failure> {
        let tree = self.command(&["ls-tree", "-z", rev, "--", path], MIB)?;
        if tree.is_empty() {
            return Ok(Snapshot::absent());
        }
        let mut entries = tree.split(|b| *b == 0).filter(|s| !s.is_empty());
        let entry = entries.next().ok_or(Failure::Gap("invalid_tree_entry"))?;
        if entries.next().is_some() {
            return Err(Failure::Gap("ambiguous_tree_entry"));
        }
        let split = entry
            .iter()
            .position(|b| *b == b'\t')
            .ok_or(Failure::Gap("invalid_tree_entry"))?;
        if &entry[split + 1..] != path.as_bytes() {
            return Err(Failure::Gap("invalid_tree_path"));
        }
        let meta =
            std::str::from_utf8(&entry[..split]).map_err(|_| Failure::Gap("invalid_tree_entry"))?;
        let fields: Vec<_> = meta.split(' ').collect();
        let [mode, kind, oid] = fields.as_slice() else {
            return Err(Failure::Gap("invalid_tree_entry"));
        };
        if !valid_oid(oid, self.oid_len, OidCase::Canonical) {
            return Err(Failure::Gap("invalid_object_id"));
        }
        if !matches!(*mode, "100644" | "100755") || *kind != "blob" {
            return Ok(Snapshot {
                blob: Some((*oid).into()),
                mode: Some((*mode).into()),
                state: Arc::new(State::empty("gap", Some("unsupported_snapshot_mode"))),
            });
        }
        let state = if let Some(state) = self.cache.get(*oid) {
            state.clone()
        } else {
            if self.cache.len() >= MAX_BLOBS {
                return Err(Failure::Limit("distinct_blobs"));
            }
            let remaining = max_bytes().saturating_sub(self.coverage.decoded_bytes);
            // Probe size before receiving a blob, including the cumulative cap.
            let bytes = match self.object("blob", oid, (MAX_FILE_BYTES as usize).min(remaining)) {
                Err(Failure::Limit("blob_bytes")) if remaining < MAX_FILE_BYTES as usize => {
                    return Err(Failure::Limit("decoded_bytes"));
                }
                result => result?,
            };
            self.coverage.decoded_bytes += bytes.len();
            let state = match std::str::from_utf8(&bytes) {
                Ok(source) => extract(source, id),
                Err(_) => State::empty("gap", Some("invalid_utf8")),
            };
            let state = Arc::new(state);
            self.cache.insert((*oid).into(), state.clone());
            self.coverage.distinct_blobs = self.cache.len();
            state
        };
        Ok(Snapshot {
            blob: Some((*oid).into()),
            mode: Some((*mode).into()),
            state,
        })
    }

    fn observe(&mut self, rev: &str, path: &str, id: &str) -> Result<Snapshot, Failure> {
        match self.snapshot(rev, path, id) {
            Ok(s) => Ok(s),
            Err(Failure::Gap(reason)) => Ok(Snapshot::gap(reason)),
            Err(e) => Err(e),
        }
    }
}

fn extract(source: &str, id: &str) -> State {
    let doc = match Document::parse(source) {
        Ok(doc) => doc,
        Err(_) => return State::empty("gap", Some("unsupported_parser_input")),
    };
    if doc.write_blocker.is_some() {
        return State::empty("gap", Some("unsupported_parser_input"));
    }
    if !doc.ids.contains_key(id) && !doc.conflicted.iter().any(|i| i == id) {
        // Malformed carriers could have hidden the requested identity.
        if doc
            .diagnostics
            .iter()
            .any(|d| d.code == ErrorCode::MalformedMarker)
        {
            return State::empty("gap", Some("unsupported_parser_input"));
        }
        return State::empty("absent", None);
    }
    let target = match doc.resolve(&Selector::Id { id: id.into() }) {
        Ok(target) => target,
        Err(e) => {
            return State::empty(
                "gap",
                Some(if e.code == ErrorCode::DuplicateId {
                    "history_identity_ambiguous"
                } else {
                    "unaddressed_identity"
                }),
            );
        }
    };
    let mut ids = doc.target_ids(target);
    ids.sort();
    let mut state = State::empty("present", None);
    state.ids = ids;
    state.revision = Some(doc.target_revision(target));
    match target {
        Target::Item { list, item } => {
            state.kind = Some("item".into());
            state.content = Some(doc.item_content(list, item));
            state.location = Some(location(&doc, list));
        }
        Target::Section(_) => state.kind = Some("section".into()),
        Target::Block(b) => state.kind = Some(doc.blocks[b].kind.into()),
        _ => state.kind = Some("other".into()),
    }
    state
}

fn location(doc: &Document, list: usize) -> Value {
    let block = doc.lists[list].block;
    let owner = doc.section_of(doc.blocks[block].span.start);
    let mut chain = vec![];
    let mut section = owner;
    let mut supported = true;
    while let Some(s) = section {
        let mut ids = doc.section_ids(s);
        ids.sort();
        supported &= !ids.is_empty()
            && ids.iter().all(|id| {
                doc.resolve(&Selector::Id { id: id.clone() }).ok() == Some(Target::Section(s))
            });
        chain.push(ids);
        section = doc.sections[s].parent;
    }
    chain.reverse();
    let mut list_ids = doc.target_ids(Target::Block(block));
    list_ids.sort();
    let list_supported = !list_ids.is_empty()
        && list_ids.iter().all(|id| {
            doc.resolve(&Selector::Id { id: id.clone() }).ok() == Some(Target::Block(block))
        });
    let direct_lists = doc
        .lists
        .iter()
        .filter(|l| doc.section_of(doc.blocks[l.block].span.start) == owner)
        .count();
    let parent = if list_supported {
        json!({"ids": list_ids})
    } else if list_ids.is_empty() && direct_lists == 1 {
        json!({"sole_direct_list": true})
    } else {
        supported = false;
        Value::Null
    };
    json!({"supported": supported, "sections": chain, "parent_list": parent})
}

fn failure(error: Failure) -> Diagnostic {
    match error {
        Failure::Limit(reason) => Diagnostic::new(ErrorCode::HistoryResourceLimit, reason),
        Failure::Gap(reason) => Diagnostic::new(ErrorCode::HistoryGap, reason),
    }
}

fn reader(project: &Project, budget: Arc<ReadBudget>) -> Result<(Reader, Repo), Diagnostic> {
    let git = Git::new(project).bounded(budget.clone());
    let repo = Repo::inspect_with(git.clone(), project);
    if let Some(boundary) = *budget.exceeded.lock().expect("budget mutex") {
        return Err(Diagnostic::new(ErrorCode::HistoryResourceLimit, boundary));
    }
    if let Some(reason) = repo.error.as_ref().or(repo.layout.as_ref()) {
        return Err(Diagnostic::new(
            ErrorCode::HistoryUnavailable,
            reason.clone(),
        ));
    }
    if repo.too_old || repo.head.is_none() || repo.branch.is_none() {
        return Err(Diagnostic::new(
            ErrorCode::HistoryUnavailable,
            "restoration needs a supported repository with committed HEAD on a branch",
        ));
    }
    let len = repo.head.as_ref().expect("HEAD").len();
    if !matches!(len, 40 | 64) {
        return Err(Diagnostic::new(
            ErrorCode::HistoryUnavailable,
            "unsupported object format",
        ));
    }
    Ok((
        Reader {
            git,
            oid_len: len,
            cache: BTreeMap::new(),
            coverage: Coverage::default(),
        },
        repo,
    ))
}

fn present(state: &State) -> Result<(), Diagnostic> {
    let error = match state.status {
        "absent" => Some(ErrorCode::HistoryTargetAbsent),
        "gap" => Some(if state.reason == Some("history_identity_ambiguous") {
            ErrorCode::HistoryIdentityAmbiguous
        } else {
            ErrorCode::HistoryGap
        }),
        _ if state.kind.as_deref() != Some("item") => Some(ErrorCode::HistoryKindChanged),
        _ if state
            .location
            .as_ref()
            .is_none_or(|l| l["supported"] != true) =>
        {
            Some(ErrorCode::HistoryLocationUnsupported)
        }
        _ => None,
    };
    match error {
        Some(code) => Err(Diagnostic::new(
            code,
            state
                .reason
                .unwrap_or("snapshot is not a supported live item"),
        )),
        None => Ok(()),
    }
}

fn same_identity(state: &State, current: &State) -> Result<(), Diagnostic> {
    present(state)?;
    if state.ids != current.ids {
        return Err(Diagnostic::new(
            ErrorCode::HistoryLineageBoundary,
            "the item's own ID set changed",
        ));
    }
    if state.location != current.location {
        return Err(Diagnostic::new(
            ErrorCode::HistoryLocationChanged,
            "the item's stable location changed",
        ));
    }
    Ok(())
}

fn editable(state: &State) -> Option<&str> {
    // Use the same trailing-block normalization as set_item's renderer.
    state
        .content
        .as_deref()
        .map(|s| s.trim_end_matches([' ', '\t', '\r', '\n']))
}

fn current_bound(
    project: &Project,
    path: &str,
    collection: &str,
    doc: &Document,
    id: &str,
) -> Result<(crate::schema::Bound, State), Diagnostic> {
    let bound = crate::schema::bind(project, collection)?;
    bound.refuse_file()?;
    if !bound.config.selects(path) {
        return Err(Diagnostic::new(
            ErrorCode::InvalidRequest,
            "path is outside this collection",
        ));
    }
    let state = extract(&doc.source, id);
    present(&state)?;
    let Target::Item { list, item } = doc.resolve(&Selector::Id { id: id.into() })? else {
        unreachable!("present item")
    };
    let inst = crate::schema::instance(doc, &bound)?;
    if !inst.is_some_and(|i| i.items.contains(&(list, item))) {
        return Err(Diagnostic::new(
            ErrorCode::InvalidItem,
            "item is outside the current collection",
        ));
    }
    Ok((bound, state))
}

fn restore_guard(
    path: &str,
    id: &str,
    state: &State,
    bound: &crate::schema::Bound,
    tip: &str,
    branch: &str,
) -> String {
    let identity = serde_json::to_vec(&(&state.ids, &state.location)).expect("identity");
    crate::revision(
        "item_restore",
        id,
        state.revision.as_ref().expect("revision").as_bytes(),
        &[
            path.as_bytes(),
            &identity,
            bound.name.as_bytes(),
            bound.binding.as_bytes(),
            bound.digest.as_bytes(),
            tip.as_bytes(),
            branch.as_bytes(),
        ],
    )
}

/// Immutable proof inputs carried from the locked document to landing preflight.
pub(crate) struct Assertion {
    path: String,
    id: String,
    tip: String,
    branch: String,
    baseline: State,
    budget: Arc<ReadBudget>,
}

impl Assertion {
    /// Validate against the exact HEAD/branch which landing will put in its Record.
    pub(crate) fn check(
        &self,
        project: &Project,
        head: Option<&str>,
        branch: Option<&str>,
    ) -> Result<(), Diagnostic> {
        if branch != Some(self.branch.as_str()) {
            return Err(Diagnostic::new(
                ErrorCode::HistoryChanged,
                "the observed branch changed",
            ));
        }
        let (mut reader, _) = reader(project, self.budget.clone())?;
        self.check_with(&mut reader, head)
    }

    fn check_with(&self, reader: &mut Reader, head: Option<&str>) -> Result<(), Diagnostic> {
        let head =
            head.ok_or_else(|| Diagnostic::new(ErrorCode::HistoryChanged, "HEAD disappeared"))?;
        let chain = reader.chain(head, &self.tip)?;
        for oid in chain {
            let state = reader
                .snapshot(&oid, &self.path, &self.id)
                .map_err(failure)?
                .state;
            same_identity(&state, &self.baseline)?;
            if editable(&state) != editable(&self.baseline) {
                return Err(Diagnostic::new(
                    ErrorCode::HistoryChanged,
                    "the target changed after the supplied history tip",
                ));
            }
        }
        Ok(())
    }
}

impl Reader {
    fn chain(&self, from: &str, until: &str) -> Result<Vec<String>, Diagnostic> {
        let mut rev = Some(from.to_owned());
        let mut chain = vec![];
        for _ in 0..MAX_COMMITS {
            let oid = rev.take().ok_or_else(|| {
                Diagnostic::new(
                    ErrorCode::HistoryRevisionUnreachable,
                    "supplied OID is not on the selected first-parent chain",
                )
            })?;
            let done = oid == until;
            if !done {
                rev = self.parent(&oid).map_err(failure)?;
            }
            chain.push(oid);
            if done {
                return Ok(chain);
            }
        }
        Err(Diagnostic::new(ErrorCode::HistoryResourceLimit, "commits"))
    }

    fn parent(&self, oid: &str) -> Result<Option<String>, Failure> {
        let bytes = self.object("commit", oid, max_metadata())?;
        let commit = parse_commit(&bytes, self.oid_len).map_err(Failure::Gap)?;
        Ok(commit.parents.first().map(|p| (*p).to_owned()))
    }
}

/// Refuse pending ownership uncertainty without recovering or creating bookkeeping.
pub(crate) fn no_pending(project: &Project, path: &str, operation: &str) -> Result<(), Diagnostic> {
    let records = crate::commit::pending_for_ownership(project)?;
    if records.iter().any(|r| r.paths().any(|p| p == path)) {
        let next = if operation == "guarded adopt" {
            "run markrivet commit-pending, then check again"
        } else {
            "run markrivet commit-pending separately"
        };
        return Err(Diagnostic::new(
            ErrorCode::PendingUnresolved,
            format!("{operation} does not settle pending records; {next}"),
        )
        .with_path(path));
    }
    Ok(())
}

/// Output consumed by the common locked writer, rather than another public write.
pub(crate) struct Prepared {
    pub op: crate::edit::Op,
    pub assertion: Assertion,
    pub response: Value,
    pub equal: bool,
}

pub(crate) struct Restore<'a> {
    pub collection: &'a str,
    pub id: &'a str,
    pub history_tip: &'a str,
    pub guard: &'a str,
    pub rev: Option<&'a str>,
}

pub(crate) fn prepare(
    project: &Project,
    path: &str,
    doc: &Document,
    request: &Restore,
) -> Result<Prepared, Diagnostic> {
    use crate::schema;
    if crate::commit::per_write(schema::load_config(project)?.git.as_ref()).is_none() {
        return Err(Diagnostic::new(
            ErrorCode::CommitRefused,
            "revert-item requires per-write commit mode",
        ));
    }
    no_pending(project, path, "restore")?;
    let (bound, current) = current_bound(project, path, request.collection, doc, request.id)?;
    let budget = ReadBudget::new(max_bytes());
    let (mut reader, repo) = reader(project, budget.clone())?;
    let branch = repo.branch.as_deref().expect("branch");
    if !valid_oid(request.history_tip, reader.oid_len, OidCase::Canonical)
        || request
            .rev
            .is_some_and(|r| !valid_oid(r, reader.oid_len, OidCase::Canonical))
    {
        return Err(Diagnostic::new(
            ErrorCode::InvalidRequest,
            "tip and rev must be full lowercase commit OIDs",
        ));
    }
    let guard = restore_guard(
        path,
        request.id,
        &current,
        &bound,
        request.history_tip,
        branch,
    );
    if guard != request.guard {
        return Err(Diagnostic::new(
            ErrorCode::GuardConflict,
            "item_restore guard changed or has the wrong revision kind",
        ));
    }
    let tip = reader
        .snapshot(request.history_tip, path, request.id)
        .map_err(failure)?;
    same_identity(&tip.state, &current)?;
    let assertion = Assertion {
        path: path.into(),
        id: request.id.into(),
        tip: request.history_tip.into(),
        branch: branch.into(),
        baseline: (*tip.state).clone(),
        budget,
    };
    assertion.check_with(&mut reader, repo.head.as_deref())?;
    if let Some(rev) = request.rev {
        reader
            .object("commit", rev, max_metadata())
            .map_err(|e| match e {
                Failure::Gap("missing_object") => Diagnostic::new(
                    ErrorCode::HistoryRevisionMissing,
                    "selected commit object is unavailable",
                ),
                e => failure(e),
            })?;
        reader.chain(request.history_tip, rev)?;
    } else if editable(&current) != editable(&tip.state) {
        return Err(Diagnostic::new(
            ErrorCode::HistoryChanged,
            "default requires live content to match the supplied tip",
        ));
    }
    let mut next = Some(request.history_tip.to_owned());
    let mut selected = None;
    for _ in 0..MAX_COMMITS {
        let Some(oid) = next.take() else { break };
        let snap = reader.snapshot(&oid, path, request.id).map_err(failure)?;
        if snap.state.status == "absent" {
            return Err(Diagnostic::new(
                if request.rev.is_some() {
                    ErrorCode::HistoryLineageBoundary
                } else {
                    ErrorCode::HistoryNoPreviousVersion
                },
                "selection crosses an absent preimage",
            ));
        }
        same_identity(&snap.state, &current)?;
        if request.rev == Some(oid.as_str())
            || (request.rev.is_none() && editable(&snap.state) != editable(&tip.state))
        {
            selected = Some((oid, snap));
            break;
        }
        next = reader.parent(&oid).map_err(failure)?;
    }
    let (resolved, selected) = selected.ok_or_else(|| {
        Diagnostic::new(
            if next.is_some() {
                ErrorCode::HistoryResourceLimit
            } else if request.rev.is_some() {
                ErrorCode::HistoryRevisionUnreachable
            } else {
                ErrorCode::HistoryNoPreviousVersion
            },
            "no selectable snapshot in the bounded first-parent interval",
        )
    })?;
    // Validate historical carrier/descendant support with the same set planner.
    let blob = selected.blob.as_deref().expect("present blob");
    let bytes = reader
        .object("blob", blob, MAX_FILE_BYTES as usize)
        .map_err(failure)?;
    let historical = Document::parse(
        std::str::from_utf8(&bytes)
            .map_err(|_| Diagnostic::new(ErrorCode::HistoryGap, "invalid UTF-8"))?,
    )?;
    let Target::Item { list, item } = historical.resolve(&Selector::Id {
        id: request.id.into(),
    })?
    else {
        unreachable!("item")
    };
    let content = selected.state.content.as_ref().expect("item content");
    crate::edit::rewrite_prerequisite(&historical, list, item)?;
    let Target::Item { list, item } = doc.resolve(&Selector::Id {
        id: request.id.into(),
    })?
    else {
        unreachable!("item")
    };
    crate::edit::rewrite_prerequisite(doc, list, item)?;
    let fields = |content: &str| -> Result<Value, Diagnostic> {
        let typed = schema::extract(content, &bound.schema)
            .map_err(|e| Diagnostic::new(ErrorCode::InvalidItem, e.join("; ")))?;
        let authored = schema::discover(content, &bound.schema)
            .map_err(|e| Diagnostic::new(ErrorCode::InvalidItem, e.join("; ")))?;
        Ok(json!({"fields":typed, "authored":authored}))
    };
    let current_fields = fields(current.content.as_ref().expect("content"))?;
    let selected_fields = fields(content)?;
    let response = json!({"resolved_rev":resolved,"history_tip":request.history_tip,
        "restore_guard":guard,"content":content, "current":current_fields,"selected":selected_fields});
    if sizes(&response, max_bytes() - ENVELOPE_RESERVE).is_none() {
        return Err(Diagnostic::new(
            ErrorCode::HistoryResourceLimit,
            "restore_response_bytes",
        ));
    }
    Ok(Prepared {
        op: crate::edit::Op::Set {
            collection: None,
            typed_guard: None,
            select: Selector::Id {
                id: request.id.into(),
            },
            guard: doc.target_revision(Target::Item { list, item }),
            part: None,
            content: json!(content),
            allow_drop: false,
        },
        equal: editable(&current) == editable(&selected.state),
        assertion,
        response,
    })
}

// Count without allocating escaped JSON. Pretty rows acquire four extra spaces
// per line when placed inside the response's transitions array.
#[derive(Default)]
struct Counter {
    bytes: usize,
    lines: usize,
    cap: usize,
}
impl Write for Counter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.bytes = self.bytes.saturating_add(bytes.len());
        self.lines += bytes.iter().filter(|b| **b == b'\n').count();
        if self.bytes > self.cap {
            return Err(std::io::Error::other("response budget"));
        }
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn sizes(value: &impl Serialize, cap: usize) -> Option<(usize, usize)> {
    let mut compact = Counter {
        cap,
        ..Counter::default()
    };
    serde_json::to_writer(&mut compact, value).ok()?;
    let mut pretty = Counter {
        cap,
        ..Counter::default()
    };
    serde_json::to_writer_pretty(&mut pretty, value).ok()?;
    Some((compact.bytes, pretty.bytes + 4 * pretty.lines))
}

fn note(coverage: &mut Coverage, reason: &str) {
    coverage.complete = false;
    if coverage.boundary.is_none() {
        coverage.boundary = Some(reason.into());
    }
    // A bounded summary; states on each row retain individual gap reasons.
    if !coverage.diagnostics.iter().any(|v| v["reason"] == reason) {
        coverage
            .diagnostics
            .push(json!({"code": if reason.starts_with("history_") { reason }
            else { "history_gap" }, "reason": reason}));
    }
}

/// Keep the two unborn-HEAD inspection calls consistent with bounded reads.
fn head_inspection_error(coverage: &mut Coverage, error: GitError) -> Result<(), Diagnostic> {
    match error {
        GitError::OutputLimit(boundary) => {
            note(coverage, "history_resource_limit");
            coverage.boundary = Some(boundary.into());
        }
        GitError::Deadline => note(coverage, "git_deadline"),
        error => {
            return Err(Diagnostic::new(
                ErrorCode::HistoryUnavailable,
                error.to_string(),
            ));
        }
    }
    Ok(())
}

pub fn item_log(
    project: &Project,
    path: &str,
    collection: &str,
    id: &str,
) -> Result<Value, Diagnostic> {
    if id.is_empty() || collection.is_empty() || id.contains('\0') || collection.contains('\0') {
        return Err(Diagnostic::new(
            ErrorCode::InvalidRequest,
            "item-log requires a collection and stay ID",
        ));
    }
    if sizes(&(path, collection, id), max_bytes() - ENVELOPE_RESERVE).is_none() {
        return Err(Diagnostic::new(
            ErrorCode::HistoryResourceLimit,
            "response envelope exceeds response_bytes",
        ));
    }
    let budget = ReadBudget::new(max_bytes());
    let git = Git::new(project).bounded(budget.clone());
    let repo = Repo::inspect_with(git.clone(), project);
    let mut coverage = Coverage {
        complete: true,
        ..Coverage::default()
    };
    let mut transitions = vec![];
    let mut tip = repo.head.clone();
    let mut branch = repo.branch.clone();
    if budget.exceeded.lock().expect("budget mutex").is_some() {
        note(&mut coverage, "history_resource_limit");
        coverage.boundary = budget
            .exceeded
            .lock()
            .expect("budget mutex")
            .map(str::to_owned);
        tip = None;
        branch = None;
    } else if let Some(reason) = repo.error.as_ref().or(repo.layout.as_ref()) {
        return Err(Diagnostic::new(
            ErrorCode::HistoryUnavailable,
            reason.clone(),
        ));
    } else if repo.too_old {
        return Err(Diagnostic::new(
            ErrorCode::HistoryUnavailable,
            "Git 2.36 or later is required",
        ));
    }
    // A peeled HEAD lookup exits 1 for both an unborn ref and a missing tip
    // object. Preserve the raw OID when present; only a verified absent symbolic
    // reference is complete empty history.
    if coverage.complete && tip.is_none() {
        match git.inspect_limit(&["rev-parse", "-q", "--verify", "HEAD"], 128) {
            Ok(out) if out.ok() => {
                let raw = std::str::from_utf8(&out.stdout).unwrap_or("").trim();
                if !valid_oid(raw, raw.len(), OidCase::Canonical) {
                    return Err(Diagnostic::new(
                        ErrorCode::HistoryUnavailable,
                        "invalid raw HEAD",
                    ));
                }
                tip = Some(raw.to_owned());
            }
            Ok(out) if out.code == Some(1) => {
                let Some(reference) = branch.as_deref() else {
                    return Err(Diagnostic::new(
                        ErrorCode::HistoryUnavailable,
                        "unreadable detached HEAD",
                    ));
                };
                match git.inspect_limit(&["show-ref", "--verify", "--quiet", reference], 128) {
                    Ok(out) if out.code == Some(1) => {}
                    Err(error) => head_inspection_error(&mut coverage, error)?,
                    _ => {
                        return Err(Diagnostic::new(
                            ErrorCode::HistoryUnavailable,
                            "HEAD is not a verified unborn reference",
                        ));
                    }
                }
            }
            Err(error) => head_inspection_error(&mut coverage, error)?,
            _ => {
                return Err(Diagnostic::new(
                    ErrorCode::HistoryUnavailable,
                    "cannot pin HEAD",
                ));
            }
        }
    }
    let envelope = sizes(
        &(path, collection, id, &tip, &branch),
        max_bytes() - ENVELOPE_RESERVE,
    )
    .ok_or_else(|| {
        Diagnostic::new(
            ErrorCode::HistoryResourceLimit,
            "response envelope exceeds response_bytes",
        )
    })?;
    if coverage.complete
        && let Some(head) = &tip
    {
        let mut reader = Reader {
            git,
            oid_len: head.len(),
            cache: BTreeMap::new(),
            coverage,
        };
        if !valid_oid(head, head.len(), OidCase::Canonical) {
            return Err(Diagnostic::new(
                ErrorCode::HistoryUnavailable,
                "unsupported Git object format",
            ));
        }
        let result = read_history(&mut reader, head, path, id, envelope, &mut transitions);
        match result {
            Err(Failure::Limit(boundary)) => {
                note(&mut reader.coverage, "history_resource_limit");
                reader.coverage.boundary = Some(boundary.into());
            }
            Err(Failure::Gap(reason)) => note(&mut reader.coverage, reason),
            Ok(()) => {}
        }
        coverage = reader.coverage;
    } // Unborn HEAD is complete empty committed history, not an insertion.
    coverage.git_received_bytes = budget.received.load(Ordering::Relaxed);
    // Label repeated appearances only when older observed evidence establishes
    // an earlier incarnation. A gap resets that proof.
    let mut appeared = false;
    for row in transitions.iter_mut().rev() {
        if row.before.state.status == "gap" || row.after.state.status == "gap" {
            appeared = false;
        }
        if row.event == "appearance" && appeared {
            row.event = "reappearance";
        }
        appeared |= row.after.state.status == "present";
    }
    let mut body = json!({"status": "read", "path": path, "collection": collection, "id": id,
        "history_tip": tip, "branch": branch, "traversal": "first_parent", "follows_renames": false,
        "restore_guard": null, "coverage": coverage, "transitions": transitions});
    if let (Some(tip), Some(branch)) = (tip.as_deref(), branch.as_deref()) {
        let eligible = (|| {
            let doc = Document::parse(&project.read(path)?)?;
            let (bound, state) = current_bound(project, path, collection, &doc, id)?;
            if crate::commit::per_write(crate::schema::load_config(project)?.git.as_ref()).is_none()
            {
                return Err(Diagnostic::new(
                    ErrorCode::CommitRefused,
                    "restoration requires per-write mode",
                ));
            }
            no_pending(project, path, "restore")?;
            Ok::<_, Diagnostic>((
                restore_guard(path, id, &state, &bound, tip, branch),
                state.location,
            ))
        })();
        match eligible {
            Ok((guard, location)) => {
                body["restore_guard"] = json!(guard);
                body["current_location"] = json!(location);
            }
            Err(error) => body["restore_diagnostic"] = json!(error),
        }
    }
    // This final count includes the entire envelope and the API's ok field.
    body["ok"] = json!(true);
    finish_response(body)
}

/// Count the entire envelope as well as rows. If the response shape grows in a
/// future change, preserve complete rows and report a cutoff instead of panic.
fn finish_response(mut body: Value) -> Result<Value, Diagnostic> {
    loop {
        let mut compact = Counter {
            cap: max_bytes() - 1,
            ..Counter::default()
        };
        let mut pretty = Counter {
            cap: max_bytes() - 1,
            ..Counter::default()
        };
        let fits = serde_json::to_writer(&mut compact, &body).is_ok()
            && serde_json::to_writer_pretty(&mut pretty, &body).is_ok();
        if fits {
            let c = compact.bytes + 1;
            let p = pretty.bytes + 1;
            if body["coverage"]["compact_response_bytes"] == c
                && body["coverage"]["pretty_response_bytes"] == p
            {
                return Ok(body);
            }
            body["coverage"]["compact_response_bytes"] = json!(c);
            body["coverage"]["pretty_response_bytes"] = json!(p);
            continue;
        }
        let Some(row) = body["transitions"]
            .as_array_mut()
            .expect("transitions array")
            .pop()
        else {
            return Err(Diagnostic::new(
                ErrorCode::HistoryResourceLimit,
                "response envelope exceeds response_bytes",
            ));
        };
        if let Some((_, charged)) = sizes(&row, max_bytes()) {
            let retained = body["coverage"]["retained_detail_bytes"]
                .as_u64()
                .unwrap_or(0);
            body["coverage"]["retained_detail_bytes"] =
                json!(retained.saturating_sub(charged as u64));
        }
        body["coverage"]["complete"] = json!(false);
        body["coverage"]["boundary"] = json!("response_bytes");
        let diagnostics = body["coverage"]["diagnostics"]
            .as_array_mut()
            .expect("diagnostics array");
        if !diagnostics
            .iter()
            .any(|d| d["code"] == "history_resource_limit")
        {
            diagnostics.push(json!({"code":"history_resource_limit", "reason":"response_bytes"}));
        }
    }
}

fn read_history(
    reader: &mut Reader,
    tip: &str,
    path: &str,
    id: &str,
    envelope: (usize, usize),
    transitions: &mut Vec<Transition>,
) -> Result<(), Failure> {
    let shallow = reader.command(&["rev-parse", "--is-shallow-repository"], 64)?;
    reader.coverage.shallow = shallow == b"true\n";
    let walk = reader.command(
        &["rev-list", "--first-parent", "--max-count=10001", tip],
        MIB,
    );
    let walk = match walk {
        Ok(bytes) => String::from_utf8(bytes).map_err(|_| Failure::Gap("invalid_history_walk"))?,
        Err(Failure::Gap(reason)) => {
            note(&mut reader.coverage, reason);
            // Preserve the readable tip even if rev-list cannot traverse a
            // missing parent. Raw commit headers still distinguish it from root.
            format!("{tip}\n")
        }
        Err(e) => return Err(e),
    };
    // The full escaped envelope, including branch metadata, was charged before rows.
    let mut compact = envelope.0 + ENVELOPE_RESERVE;
    let mut pretty = envelope.1 + ENVELOPE_RESERVE;
    let walk_len = walk.lines().count();
    let mut expected = Some(tip.to_owned());
    for (i, rev) in walk.lines().enumerate() {
        if i == MAX_COMMITS {
            return Err(Failure::Limit("commits"));
        }
        if expected.as_deref() != Some(rev) {
            return Err(Failure::Gap("invalid_history_walk"));
        }
        let bytes = reader.object("commit", rev, max_metadata())?;
        let commit = parse_commit(&bytes, reader.oid_len).map_err(Failure::Gap)?;
        let parent = commit.parents.first().copied();
        expected = parent.map(str::to_owned);
        let after = reader.observe(rev, path, id)?;
        let before = match parent {
            Some(p) => reader.observe(p, path, id)?,
            None => {
                reader.coverage.root_reached = true;
                Snapshot::absent()
            }
        };
        reader.coverage.commits_scanned += 1;
        for state in [&before.state, &after.state] {
            if state.status == "gap" {
                note(&mut reader.coverage, state.reason.unwrap_or("parse_gap"));
            }
        }
        let last = i + 1 == walk_len;
        if last && parent.is_some() && reader.coverage.shallow {
            note(&mut reader.coverage, "shallow_boundary");
        }
        let event = if before.state.status == "gap" || after.state.status == "gap" {
            "gap"
        } else if before.state.status == "absent" && after.state.status == "present" {
            "appearance"
        } else if before.state.status == "present" && after.state.status == "absent" {
            "deletion"
        } else if before.state.kind != after.state.kind {
            "kind_change"
        } else if before.state.content != after.state.content {
            "content_update"
        } else if before.state.location != after.state.location {
            "location_change"
        } else {
            "structural_only"
        };
        if (before.state.status == "absent" && after.state.status == "absent")
            || (before.state == after.state && before.mode == after.mode)
        {
            continue;
        }
        let row = Transition {
            commit: rev.into(),
            first_parent: parent.map(str::to_owned),
            before_rev: if before.state.status == "present" {
                parent.map(str::to_owned)
            } else {
                None
            },
            after_rev: (after.state.status == "present").then(|| rev.into()),
            event,
            integration: commit.parents.len() > 1,
            author: commit.author.into(),
            committer: commit.committer.into(),
            message: commit.message.into(),
            writer_session: None,
            before,
            after,
        };
        // This borrowed serialization precedes every response copy of cached
        // content. Counting distinct blobs alone cannot enforce this budget.
        let (c, p) = sizes(&row, max_bytes()).ok_or(Failure::Limit("response_bytes"))?;
        if compact + c + 1 > max_bytes() || pretty + p + 6 > max_bytes() {
            return Err(Failure::Limit("response_bytes"));
        }
        compact += c + 1;
        pretty += p + 6;
        reader.coverage.retained_detail_bytes += p;
        if reader.coverage.retained_detail_bytes > max_bytes() {
            return Err(Failure::Limit("retained_details"));
        }
        transitions.push(row);
    }
    if !reader.coverage.root_reached && reader.coverage.boundary.is_none() {
        note(&mut reader.coverage, "history_walk_incomplete");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn release_resource_limits() {
        assert_eq!(MAX_BYTES, 64 * 1024 * 1024);
        assert_eq!(MAX_METADATA, 256 * 1024);
    }

    #[test]
    fn an_unbudgeted_envelope_field_trims_rows_or_refuses_without_panicking() {
        // Isolate the override from other unit tests running in this process.
        #[cfg(debug_assertions)]
        if std::env::var("RIVET_TEST_HISTORY_BYTES").as_deref() != Ok("1048576") {
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "history::tests::an_unbudgeted_envelope_field_trims_rows_or_refuses_without_panicking", "--nocapture"])
                .env("RIVET_TEST_HISTORY_BYTES", "1048576")
                .status()
                .unwrap();
            assert!(status.success());
            return;
        }
        #[cfg(debug_assertions)]
        assert_eq!(max_bytes(), 1024 * 1024);
        // Model a future field added after row accounting. Final validation
        // must still return valid capped JSON or a small explicit refusal.
        let body = json!({"ok":true,"coverage":Coverage::default(),
            "transitions":[{"content":"x".repeat(max_bytes() - ENVELOPE_RESERVE)}],
            "extra_metadata":"b".repeat(ENVELOPE_RESERVE * 2)});
        let out = finish_response(body).unwrap();
        assert_eq!(out["transitions"], json!([]));
        assert_eq!(out["coverage"]["boundary"], "response_bytes");
        assert!(serde_json::to_vec_pretty(&out).unwrap().len() < max_bytes());
        let body = json!({"coverage":Coverage::default(), "transitions":[],
            "extra_metadata":"b".repeat(max_bytes())});
        assert_eq!(
            finish_response(body).unwrap_err().code,
            ErrorCode::HistoryResourceLimit
        );
    }
}
