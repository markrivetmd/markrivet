//! The owned JSON contract. Every request names an `op`; every response has
//! `ok` and `status`. Reads never change a file. Writes take the project lock,
//! re-read the file, resolve and verify against that preimage, and replace it
//! durably or not at all.
use crate::document::{Document, Target};
use crate::edit::{self, Op, Planner};
use crate::fs::{DEFAULT_LOCK_TIMEOUT, Project, canonical_path, hook};
use crate::schema::{self, Bound, Clause};
use crate::transaction::{self, Attribution, Draft, Kind, Options, Pending, Plan};
use crate::{Diagnostic, ErrorCode, Selector, Span, inspect};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::time::Duration;

pub const DEFAULT_MAX_BYTES: usize = 64 * 1024;

/// A certificate supplied by a caller that checked one document snapshot at HEAD.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(try_from = "RawExpect")]
pub struct AdoptExpect {
    pub revision: String,
    pub head: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawExpect {
    revision: String,
    head: String,
}

impl TryFrom<RawExpect> for AdoptExpect {
    type Error = String;

    fn try_from(raw: RawExpect) -> Result<Self, Self::Error> {
        let lowercase_hex = |s: &str| {
            s.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        };
        if !raw
            .revision
            .strip_prefix("rivet:v1:document:")
            .is_some_and(|hash| hash.len() == 32 && lowercase_hex(hash))
        {
            return Err("expect.revision must be rivet:v1:document: followed by 32 lowercase hex characters".into());
        }
        if !matches!(raw.head.len(), 40 | 64) || !lowercase_hex(&raw.head) {
            return Err(
                "expect.head must be a full 40- or 64-character lowercase hex commit id".into(),
            );
        }
        Ok(Self {
            revision: raw.revision,
            head: raw.head,
        })
    }
}

/// Default handles absence; a present null must fail instead of selecting plain adopt.
fn present_expect<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<AdoptExpect>, D::Error> {
    AdoptExpect::deserialize(deserializer).map(Some)
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum Request {
    Analyze {
        query: crate::analytics::Query,
    },
    RunView {
        view: String,
    },
    Export {
        request: crate::analytics::bundle::Export,
    },
    Replay {
        bundle: String,
    },
    Drilldown {
        request: crate::analytics::Drilldown,
    },
    /// Read-only parser view of inline source; needs no project.
    Inspect {
        source: String,
    },
    Map {
        path: String,
        #[serde(default)]
        limit: Option<usize>,
        #[serde(default)]
        cursor: Option<String>,
        #[serde(default)]
        max_depth: Option<u8>,
        /// Also list every direct list item, each with how it is addressed.
        #[serde(default)]
        items: bool,
    },
    ItemLog {
        path: String,
        collection: String,
        id: String,
    },
    RevertItem {
        path: String,
        collection: String,
        id: String,
        history_tip: String,
        guard: String,
        #[serde(default)]
        rev: Option<String>,
        #[serde(default)]
        preview: bool,
        #[serde(default)]
        lock_timeout_ms: Option<u64>,
        #[serde(default)]
        message: Option<String>,
        #[serde(default)]
        session: Option<String>,
    },
    Get {
        path: String,
        select: Selector,
        #[serde(default)]
        max_bytes: Option<usize>,
    },
    /// Literal text search; one document, or every document a collection binds.
    Find {
        text: String,
        #[serde(default)]
        path: Option<String>,
        #[serde(default)]
        match_case: bool,
        #[serde(default)]
        limit: Option<usize>,
    },
    ArchiveMkdir {
        path: String,
        #[serde(default)]
        lock_timeout_ms: Option<u64>,
    },
    ArchiveCopy {
        request: crate::archive::CopyRequest,
    },
    Move {
        request: crate::relocate::MoveRequest,
    },
    VerifyDelete {
        request: crate::delete_receipt::VerifyRequest,
    },
    PruneSnapshot {
        #[serde(default)]
        lock_timeout_ms: Option<u64>,
    },
    Write {
        #[serde(default)]
        require_commit: bool,
        #[serde(default)]
        policy_guard: Option<String>,
        #[serde(default)]
        archive: Option<crate::archive::ArchiveGuard>,
        path: String,
        ops: Vec<Op>,
        #[serde(default)]
        preview: bool,
        #[serde(default)]
        lock_timeout_ms: Option<u64>,
        /// Commit mode: the commit message subject and body.
        #[serde(default)]
        message: Option<String>,
        /// Commit mode: who wrote, for receipts and ownership.
        #[serde(default)]
        session: Option<String>,
    },
    /// Create a document that does not exist yet; never replaces one.
    Create {
        path: String,
        content: String,
        /// Mint stays as `stamp` would with no selector.
        #[serde(default)]
        stamp: bool,
        #[serde(default)]
        preview: bool,
        #[serde(default)]
        lock_timeout_ms: Option<u64>,
        #[serde(default)]
        message: Option<String>,
        #[serde(default)]
        session: Option<String>,
    },
    /// Commit mode: stamp a dirty file's hand-added items and commit the
    /// whole file, outside edits included, as one transaction.
    Adopt {
        path: String,
        #[serde(
            default,
            deserialize_with = "present_expect",
            skip_serializing_if = "Option::is_none"
        )]
        expect: Option<AdoptExpect>,
        #[serde(default)]
        lock_timeout_ms: Option<u64>,
        #[serde(default)]
        message: Option<String>,
        #[serde(default)]
        session: Option<String>,
    },
    /// Settle pending commit-mode records, retire one, or clear a reported
    /// git lock and settle its record.
    CommitPending {
        request: crate::commit::PendingRequest,
    },
    Doctor {
        #[serde(default)]
        collection: Option<String>,
        #[serde(default)]
        paths: Option<Vec<String>>,
        #[serde(default)]
        strict: bool,
    },
    /// Git state of bound documents against the tree; read-only.
    Status {
        #[serde(default)]
        paths: Option<Vec<String>>,
    },
    Query {
        collection: String,
        #[serde(default, rename = "where")]
        clauses: Vec<Clause>,
        #[serde(default)]
        fields: Option<Vec<String>>,
        #[serde(default)]
        paths: Option<Vec<String>>,
        #[serde(default)]
        limit: Option<usize>,
        #[serde(default)]
        cursor: Option<String>,
        /// Open items only, each with its wait resolved across the collection.
        #[serde(default)]
        ready: Option<crate::ready::ReadyRequest>,
    },
}

/// Exit status: 0 success, 2 refused with nothing changed, 3 a write that
/// may have been applied (never retry without re-reading). Doctor also returns
/// 4 for complete findings and extends 2 with an incomplete, read-only report.
pub struct Response {
    pub body: Value,
    pub exit: i32,
}

pub(crate) fn refused(error: Diagnostic) -> Response {
    let uncertain = error.code == ErrorCode::CommitUncertain;
    Response {
        body: json!({
            "ok": false,
            "status": if uncertain { "uncertain" } else { "refused" },
            "error": error,
        }),
        exit: if uncertain { 3 } else { 2 },
    }
}

pub(crate) fn ok(mut body: Value) -> Response {
    body["ok"] = json!(true);
    Response { body, exit: 0 }
}

pub fn execute(project: Option<&Project>, request: Request) -> Response {
    let request = match canonical(request) {
        Ok(r) => r,
        Err(e) => return refused(e),
    };
    if project.is_none() && matches!(&request, Request::Doctor { .. }) {
        return crate::doctor::unavailable(
            std::path::Path::new("."),
            &request,
            Diagnostic::new(
                ErrorCode::NotFound,
                "this request needs a project (.rivet directory)",
            ),
        );
    }
    let need = |p: Option<&Project>| {
        p.cloned().ok_or_else(|| {
            Diagnostic::new(
                ErrorCode::NotFound,
                "this request needs a project (.rivet directory)",
            )
        })
    };
    let result = match request {
        Request::Analyze { query } => {
            need(project).and_then(|p| crate::analytics::execute(&p, query))
        }
        Request::RunView { view } => need(project).and_then(|p| crate::analytics::view(&p, &view)),
        Request::Export { request } => {
            need(project).and_then(|p| crate::analytics::bundle::export(&p, request))
        }
        Request::Replay { bundle } => {
            need(project).and_then(|p| crate::analytics::bundle::replay(&p, &bundle))
        }
        Request::Drilldown { request } => {
            need(project).and_then(|p| crate::analytics::drilldown(&p, request))
        }
        Request::Inspect { source } => {
            let mut body = serde_json::to_value(inspect(&source)).expect("inspection serialises");
            body["status"] = json!("read");
            Ok(body)
        }
        Request::Map {
            path,
            limit,
            cursor,
            max_depth,
            items,
        } => need(project).and_then(|p| map(&p, &path, limit, cursor, max_depth, items)),
        Request::ItemLog {
            path,
            collection,
            id,
        } => need(project).and_then(|p| crate::history::item_log(&p, &path, &collection, &id)),
        Request::RevertItem {
            path,
            collection,
            id,
            history_tip,
            guard,
            rev,
            preview,
            lock_timeout_ms,
            message,
            session,
        } => {
            return match need(project) {
                Ok(p) => write(
                    &p,
                    &path,
                    &[],
                    preview,
                    lock_timeout_ms,
                    Attribution { message, session },
                    Preconditions {
                        restore: Some(crate::history::Restore {
                            collection: &collection,
                            id: &id,
                            history_tip: &history_tip,
                            guard: &guard,
                            rev: rev.as_deref(),
                        }),
                        ..Preconditions::default()
                    },
                ),
                Err(e) => refused(e),
            };
        }
        Request::Get {
            path,
            select,
            max_bytes,
        } => need(project).and_then(|p| get(&p, &path, &select, max_bytes)),
        Request::Find {
            text,
            path,
            match_case,
            limit,
        } => need(project).and_then(|p| find(&p, &text, path, match_case, limit)),
        Request::ArchiveMkdir {
            path,
            lock_timeout_ms,
        } => need(project).and_then(|p| {
            let lock =
                p.lock(lock_timeout_ms.map_or(DEFAULT_LOCK_TIMEOUT, Duration::from_millis))?;
            p.archive_mkdir(&lock, &path)?;
            Ok(json!({"status":"applied", "path":path}))
        }),
        Request::ArchiveCopy { request } => {
            return match need(project) {
                Ok(p) => crate::archive::copy(&p, request),
                Err(e) => refused(e),
            };
        }
        Request::Move { request } => {
            return match need(project) {
                Ok(p) => crate::relocate::move_unit(&p, request),
                Err(e) => refused(e),
            };
        }
        Request::VerifyDelete { request } => crate::delete_receipt::verify(&request)
            .map(|dropped| json!({"status":"verified", "version":request.receipt.version, "dropped":dropped})),
        Request::PruneSnapshot { lock_timeout_ms } => need(project).and_then(|p| {
            let lock =
                p.lock(lock_timeout_ms.map_or(DEFAULT_LOCK_TIMEOUT, Duration::from_millis))?;
            hook("locked")?;
            let start = std::time::Instant::now();
            let snapshot = crate::prune_snapshot::capture(&p, &lock, None)?;
            let capture_us = start.elapsed().as_micros() as u64;
            Ok(json!({"status":"read", "snapshot":snapshot, "capture_us":capture_us}))
        }),
        Request::Write {
            archive,
            policy_guard,
            require_commit,
            path,
            ops,
            preview,
            lock_timeout_ms,
            message,
            session,
        } => {
            return match need(project) {
                Ok(p) => write(
                    &p,
                    &path,
                    &ops,
                    preview,
                    lock_timeout_ms,
                    Attribution { message, session },
                    Preconditions {
                        require_commit,
                        policy_guard,
                        archive,
                        restore: None,
                    },
                ),
                Err(e) => refused(e),
            };
        }
        Request::Create {
            path,
            content,
            stamp,
            preview,
            lock_timeout_ms,
            message,
            session,
        } => {
            return match need(project) {
                Ok(p) => create(
                    &p,
                    &path,
                    &content,
                    stamp,
                    preview,
                    lock_timeout_ms,
                    Attribution { message, session },
                ),
                Err(e) => refused(e),
            };
        }
        Request::Adopt {
            path,
            expect,
            lock_timeout_ms,
            message,
            session,
        } => {
            let mut response = match need(project) {
                Ok(p) => adopt(&p, &path, lock_timeout_ms, Attribution { message, session }, expect.as_ref()),
                Err(e) => refused(e),
            };
            if let Some(expect) = expect {
                response.body["expect"] = json!(expect);
            }
            return response;
        }
        Request::CommitPending { request } => need(project).and_then(|p| {
            let (timeout, request) = request.parse()?;
            let lock = p.lock(timeout.map_or(DEFAULT_LOCK_TIMEOUT, Duration::from_millis))?;
            crate::commit::commit_pending(&p, &lock, request)
        }),
        Request::Doctor {
            collection,
            paths,
            strict,
        } => {
            return match need(project) {
                Ok(p) => crate::doctor::run(&p, collection, paths, strict),
                Err(e) => refused(e),
            };
        }
        Request::Status { paths } => need(project).and_then(|p| crate::status::run(&p, paths)),
        Request::Query {
            collection,
            clauses,
            fields,
            paths,
            limit,
            cursor,
            ready,
        } => need(project).and_then(|p| {
            query(
                &p,
                &collection,
                &clauses,
                fields,
                paths,
                limit,
                cursor,
                ready,
            )
        }),
    };
    match result {
        Ok(body) => ok(body),
        Err(e) => refused(e),
    }
}

/// Document paths in their one canonical spelling, before anything binds,
/// reads or reports them. Doctor keeps its own rule: it refuses any spelling
/// that is not already canonical.
fn canonical(mut request: Request) -> Result<Request, Diagnostic> {
    // Selector shape first, before any path or retry check.
    match &request {
        Request::Write { ops, .. } => {
            for (n, op) in ops.iter().enumerate() {
                op.check_selectors().map_err(|mut e| {
                    e.message = format!("op {n}: {}", e.message);
                    e
                })?;
            }
        }
        // Before archive-copy's committed-copy retry, which answers
        // `unchanged` before the destination selector is ever resolved.
        Request::ArchiveCopy { request } => {
            request.select.require_not_position("archive_copy.select")?
        }
        _ => {}
    }
    match &mut request {
        Request::Map { path, .. }
        | Request::Get { path, .. }
        | Request::ItemLog { path, .. }
        | Request::RevertItem { path, .. }
        | Request::Write { path, .. }
        | Request::Create { path, .. }
        | Request::Adopt { path, .. }
        | Request::ArchiveMkdir { path, .. }
        | Request::Find {
            path: Some(path), ..
        } => {
            *path = canonical_path(path)?;
        }
        Request::ArchiveCopy { request } => {
            request.source = canonical_path(&request.source)?;
            request.path = canonical_path(&request.path)?;
        }
        Request::Status { paths: Some(paths) }
        | Request::Query {
            paths: Some(paths), ..
        } => {
            for path in paths.iter_mut() {
                *path = canonical_path(path)?;
            }
            paths.sort();
            paths.dedup();
        }
        // These operations either carry no document paths or validate their own
        // immutable evidence / strict path contracts. Keep this exhaustive so a
        // new request must make its path policy explicit.
        Request::Analyze { .. }
        | Request::Move { .. }
        | Request::RunView { .. }
        | Request::Export { .. }
        | Request::Replay { .. }
        | Request::Drilldown { .. }
        | Request::Inspect { .. }
        | Request::VerifyDelete { .. }
        | Request::PruneSnapshot { .. }
        | Request::CommitPending { .. }
        | Request::Doctor { .. }
        | Request::Status { paths: None }
        | Request::Query { paths: None, .. }
        | Request::Find { path: None, .. } => {}
    }
    Ok(request)
}

fn load(project: &Project, path: &str) -> Result<Document, Diagnostic> {
    let source = project.read(path).map_err(|e| e.with_path(path))?;
    Document::parse(&source).map_err(|e| e.with_path(path))
}

/// `load`, except that a file which reads but does not decode or parse still
/// feeds its bytes to `digest`, so editing it invalidates a cursor.
fn load_hashed(project: &Project, path: &str, digest: &mut Sha256) -> Result<Document, Diagnostic> {
    let bytes = project.read_bytes(path).map_err(|e| e.with_path(path))?;
    std::str::from_utf8(&bytes)
        .map_err(|_| {
            Diagnostic::new(
                ErrorCode::UnsupportedSyntax,
                format!("{path:?} is not UTF-8"),
            )
        })
        .and_then(Document::parse)
        .map_err(|e| {
            digest.update(Sha256::digest(&bytes));
            e.with_path(path)
        })
}

fn describe(doc: &Document, target: Target) -> Value {
    let (span, kind) = doc.target_span(target);
    let ids = doc.target_ids(target);
    let mut v = json!({
        "kind": match target {
            Target::Section(_) => "section",
            Target::Item { .. } => "item",
            Target::Block(b) if doc.blocks[b].list.is_some() => "list",
            Target::Block(_) => "block",
            Target::FrontmatterKey(_) => "frontmatter_key",
            Target::Document => "document",
        },
        "revision_kind": kind,
        // The first id that selects this unit, so a duplicated first id is
        // passed over for one a `get` accepts; else the first, as `doctor`
        // reports the duplicate.
        "id": crate::find::address(doc, target).or_else(|| ids.first().cloned()),
        "line": doc.file_line(span.start),
        "end_line": doc.file_line(span.end.max(span.start + 1) - 1),
    });
    if ids.len() > 1 {
        v["ids"] = json!(ids);
    }
    v
}

pub(crate) fn bound_for(project: &Project, path: &str) -> Result<Vec<Bound>, Diagnostic> {
    let config = schema::load_config(project)?;
    Ok(schema::bound_paths(project, &config, &[path])?
        .pop()
        .expect("one path"))
}

fn map(
    project: &Project,
    path: &str,
    limit: Option<usize>,
    cursor: Option<String>,
    max_depth: Option<u8>,
    items: bool,
) -> Result<Value, Diagnostic> {
    let doc = load(project, path)?;
    let mut diagnostics = doc.diagnostics.clone();
    let revision = doc.revision();
    let limit = limit.unwrap_or(200).clamp(1, 1000);
    let offset = match cursor {
        None => 0,
        Some(c) => {
            let (rev, n) = c
                .rsplit_once(':')
                .ok_or_else(|| Diagnostic::new(ErrorCode::InvalidRequest, "malformed cursor"))?;
            if rev != revision {
                return Err(Diagnostic::new(
                    ErrorCode::StaleCursor,
                    "the document changed; start the map again",
                ));
            }
            n.parse()
                .map_err(|_| Diagnostic::new(ErrorCode::InvalidRequest, "malformed cursor"))?
        }
    };
    let depth = max_depth.unwrap_or(6);
    let mut outline: Vec<(usize, Value)> = vec![];
    for (i, s) in doc.sections.iter().enumerate() {
        if s.level > depth {
            continue;
        }
        let mut entry = describe(&doc, Target::Section(i));
        entry["level"] = json!(s.level);
        entry["title"] = json!(s.path.last());
        entry["path"] = json!(s.path);
        entry["bytes"] = json!(s.whole.len());
        entry["revision"] = json!(doc.target_revision(Target::Section(i)));
        outline.push((s.whole.start, entry));
    }
    for (b, block) in doc.blocks.iter().enumerate() {
        if block.heading.is_some() {
            continue;
        }
        let ids = doc.stay_ids(&block.markers);
        if let Some(l) = block.list {
            let list = &doc.lists[l];
            let mut entry = describe(&doc, Target::Block(b));
            entry["items"] = json!(list.items.len());
            entry["checkbox_items"] =
                json!(list.items.iter().filter(|i| i.checkbox.is_some()).count());
            entry["stamped_items"] =
                json!(list.items.iter().filter(|i| !i.markers.is_empty()).count());
            entry["revision"] = json!(doc.target_revision(Target::Block(b)));
            outline.push((block.span.start, entry));
            if items {
                for (i, it) in list.items.iter().enumerate() {
                    let target = Target::Item { list: l, item: i };
                    let (entry, refusal) = item_entry(&doc, target);
                    diagnostics.extend(
                        refusal.map(|e| e.with_path(path).with_line(doc.file_line(it.span.start))),
                    );
                    outline.push((it.span.start, entry));
                }
            }
        } else if !ids.is_empty() {
            let mut entry = describe(&doc, Target::Block(b));
            entry["block"] = json!(block.kind);
            entry["revision"] = json!(doc.target_revision(Target::Block(b)));
            outline.push((block.span.start, entry));
        }
    }
    outline.sort_by_key(|(pos, _)| *pos);
    let total = outline.len();
    let page: Vec<Value> = outline
        .into_iter()
        .skip(offset)
        .take(limit)
        .map(|(_, v)| v)
        .collect();
    let next = (offset + page.len() < total).then(|| format!("{revision}:{}", offset + page.len()));
    let mut collections = vec![];
    for bound in bound_for(project, path)? {
        if bound.is_file() {
            // The file is the record, and `map` hands a writer its guard.
            let fields = schema::record(&doc, &bound);
            collections.push(json!({
                "name": bound.name,
                "schema": bound.key,
                "item": "file",
                "valid": fields.is_ok(),
                "errors": fields.err().map(|e| e.messages()),
                "revision": schema::record_revision(&doc, path, &bound),
            }));
            continue;
        }
        let entry = match schema::instance(&doc, &bound) {
            Ok(Some(inst)) => {
                let invalid = inst
                    .items
                    .iter()
                    .filter(|(l, i)| {
                        schema::extract(&doc.item_content(*l, *i), &bound.schema).is_err()
                    })
                    .count();
                let mut entry = json!({
                    "name": bound.name,
                    "schema": bound.key,
                    "section": bound.config.section,
                    "items": inst.items.len(),
                    "unstamped": inst.items.iter().filter(|(l, i)| doc.lists[*l].items[*i].markers.is_empty()).count(),
                    "invalid": invalid,
                    "revision": schema::collection_revision(&doc, &bound, &inst),
                });
                if bound.config.subsections {
                    let depth = doc.sections[inst.section].path.len();
                    entry["subsections"] = json!(
                        inst.members[1..]
                            .iter()
                            .map(|&m| &doc.sections[m].path[depth..])
                            .collect::<Vec<_>>()
                    );
                }
                entry
            }
            Ok(None) => {
                json!({"name": bound.name, "schema": bound.key, "missing_section": bound.config.section})
            }
            Err(e) => json!({"name": bound.name, "schema": bound.key, "error": e}),
        };
        collections.push(entry);
    }
    let frontmatter = match (&doc.frontmatter.span, &doc.frontmatter.fields) {
        (None, _) => json!({"status": "absent"}),
        (Some(_), Ok(fields)) => json!({
            "status": "ok",
            "keys": fields.iter().map(|f| &f.key).collect::<Vec<_>>(),
            "writable": fields
                .iter()
                .filter(|f| f.writable)
                .map(|f| &f.key)
                .collect::<Vec<_>>(),
        }),
        (Some(_), Err(_)) => json!({"status": "unsupported"}),
    };
    Ok(json!({
        "status": "read",
        "path": path,
        "revision": revision,
        "bytes": doc.source.len(),
        "lines": doc.file_line(doc.source.len()),
        "frontmatter": frontmatter,
        "outline": page,
        "total": total,
        "truncated": next.is_some(),
        "next_cursor": next,
        "collections": collections,
        "diagnostics": diagnostics,
    }))
}

/// A `kind: "item"` entry naming one direct list item the way every read
/// does ([`Document::address_item`]), and the refusal a position would get
/// when it is not addressable.
fn item_entry(doc: &Document, target: Target) -> (Value, Option<Diagnostic>) {
    let Target::Item { list, item } = target else {
        unreachable!("an item target")
    };
    let addressing = doc.address_item(list, item);
    let mut entry = describe(doc, target);
    entry["id"] = json!(addressing.id());
    entry["select"] = addressing.select();
    entry["addressing"] = json!(addressing.label());
    entry["revision"] = json!(doc.target_revision(target));
    if let Some(cb) = doc.lists[list].items[item].checkbox {
        entry["checked"] = json!(cb.checked);
    }
    (entry, addressing.refusal().cloned())
}

fn get(
    project: &Project,
    path: &str,
    select: &Selector,
    max_bytes: Option<usize>,
) -> Result<Value, Diagnostic> {
    schema::validate_path_bindings(&schema::load_config(project)?, path)?;
    let doc = load(project, path)?;
    let target = doc.resolve(select).map_err(|e| e.with_path(path))?;
    let (span, _) = doc.target_span(target);
    let limit = max_bytes.unwrap_or(DEFAULT_MAX_BYTES);
    if span.len() > limit {
        return Err(Diagnostic::new(
            ErrorCode::LimitExceeded,
            format!(
                "target is {} bytes; raise max_bytes or select something smaller",
                span.len()
            ),
        ));
    }
    let described = match target {
        Target::Item { .. } => item_entry(&doc, target).0,
        _ => describe(&doc, target),
    };
    let mut body = json!({
        "status": "read",
        "path": path,
        "target": described,
        "revision": doc.target_revision(target),
        "text": &doc.source[span.start..span.end],
    });
    match target {
        Target::Item { list, item } => {
            body["content"] = json!(doc.item_content(list, item));
            if let Some(cb) = doc.lists[list].items[item].checkbox {
                body["checked"] = json!(cb.checked);
            }
            let mut typed = Map::new();
            for bound in bound_for(project, path)? {
                if let Ok(Some(inst)) = schema::instance(&doc, &bound)
                    && inst.items.contains(&(list, item))
                {
                    let fields = schema::extract(&doc.item_content(list, item), &bound.schema);
                    let mut entry = json!({
                        "schema": bound.key,
                        "revision": schema::item_revision(&doc, &bound, list, item),
                        "fields": fields.as_ref().ok(),
                        "errors": fields.err(),
                    });
                    if bound.config.subsections {
                        entry["subsection"] = json!(inst.subsection(&doc, list));
                    }
                    typed.insert(bound.name.clone(), entry);
                }
            }
            if !typed.is_empty() {
                body["typed"] = Value::Object(typed);
            }
        }
        Target::Section(s) => {
            let section = &doc.sections[s];
            body["level"] = json!(section.level);
            body["title"] = json!(section.path.last());
            body["body"] = json!(Span::slice(section.body, &doc.source));
        }
        Target::FrontmatterKey(k) => {
            body["value"] = doc.scalars()[k].value.clone();
        }
        _ => {}
    }
    Ok(body)
}

/// `find`: each unit holding a match, in path then document order. Without a
/// path it reads every document a collection binds, as `query` does.
fn find(
    project: &Project,
    text: &str,
    path: Option<String>,
    match_case: bool,
    limit: Option<usize>,
) -> Result<Value, Diagnostic> {
    let needle = crate::find::needle(text, match_case)?;
    let one = path.is_some();
    let (files, mut diagnostics) = match path {
        Some(path) => (vec![path], vec![]),
        None => {
            let config = schema::load_config(project)?;
            let globs: Vec<String> = config
                .collections
                .values()
                .flat_map(|c| c.files.iter().cloned())
                .collect();
            let (mut files, diagnostics) = schema::files(project, &globs);
            files.retain(|f| config.collections.values().any(|c| c.selects(f)));
            (files, diagnostics)
        }
    };
    let limit = limit.unwrap_or(20).clamp(1, 1000);
    let mut matches = vec![];
    let mut total = 0;
    let mut scanned = 0;
    for path in &files {
        let doc = match load(project, path) {
            Ok(doc) => doc,
            Err(e) if one => return Err(e),
            Err(e) => {
                diagnostics.push(e);
                continue;
            }
        };
        scanned += 1;
        let hits = crate::find::search(&doc, &needle, match_case);
        // Typed guards only for the items a hit names; a binding that does
        // not resolve here is `map`'s and `doctor`'s to report.
        let instances: Vec<_> = if hits.iter().any(|h| matches!(h.target, Target::Item { .. })) {
            match bound_for(project, path) {
                Ok(bound) => bound
                    .into_iter()
                    .filter_map(|b| Some((schema::instance(&doc, &b).ok()??, b)))
                    .collect(),
                Err(e) => {
                    diagnostics.push(e.with_path(path.as_str()));
                    vec![]
                }
            }
        } else {
            vec![]
        };
        for hit in hits {
            total += 1;
            if matches.len() == limit {
                continue;
            }
            let unit = describe(&doc, hit.target);
            let addressing = match hit.target {
                Target::Item { list, item } => Some(doc.address_item(list, item)),
                _ => None,
            };
            let id = match &addressing {
                Some(a) => a.id().map(str::to_string),
                None => crate::find::address(&doc, hit.target),
            };
            // The selector `get` and the untyped writes take, so a caller
            // copies it rather than rebuilding one from `kind` and `id`.
            let select = match (&id, hit.target) {
                _ if addressing.is_some() => addressing.as_ref().expect("item").select(),
                (Some(id), _) => json!({"kind": "id", "id": id}),
                (None, Target::Document) => json!({"kind": "document"}),
                (None, Target::Section(s)) if hit.unresolved.is_none() => {
                    json!({"kind": "heading_path", "path": doc.sections[s].path})
                }
                _ => Value::Null,
            };
            let mut entry = json!({
                "path": path,
                "kind": unit["kind"],
                "id": id,
                "select": select,
                "revision": doc.target_revision(hit.target),
                "line": doc.file_line(hit.at),
            });
            if let Some(a) = &addressing {
                entry["addressing"] = json!(a.label());
            }
            if let Some(s) = match hit.target {
                Target::Section(s) => Some(s),
                Target::Document => None,
                _ => doc.section_of(hit.at),
            } {
                entry["heading_path"] = json!(doc.sections[s].path);
            }
            if let Target::Item { list, item } = hit.target {
                let typed: Map<String, Value> = instances
                    .iter()
                    .filter(|(inst, _)| inst.items.contains(&(list, item)))
                    .map(|(_, b)| {
                        let guard = schema::item_revision(&doc, b, list, item);
                        (b.name.clone(), json!(guard))
                    })
                    .collect();
                if !typed.is_empty() {
                    entry["typed"] = Value::Object(typed);
                }
            }
            if let Some(e) = hit.unresolved {
                diagnostics.push(e.with_path(path.as_str()).with_line(doc.file_line(hit.at)));
            }
            entry["count"] = json!(hit.count);
            entry["excerpt"] = json!(hit.excerpt);
            matches.push(entry);
        }
    }
    Ok(json!({
        "status": "read",
        "files": scanned,
        "matches": matches,
        "total": total,
        "truncated": total > limit,
        "diagnostics": diagnostics,
    }))
}

/// Where the stay ids a write drops are still mentioned, bare or prefixed,
/// once it lands: the candidate stands in for `path`, and an id another
/// collection or archive file still carries is not dangling. A file that does
/// not read or list is passed over, as `doctor` reports it.
fn dangling_citations(
    project: &Project,
    path: &str,
    candidate: &str,
    dropped: &BTreeSet<String>,
) -> Vec<Value> {
    let _ = crate::fs::hook("dangling_citations");
    let Ok(config) = schema::load_config(project) else {
        return vec![];
    };
    let (mut files, _, _) =
        crate::citations::scope(project, &config, &crate::fs::WalkFaults::default());
    if !files.iter().any(|(p, _)| p == path) {
        files.push((path.to_string(), false));
    }
    let docs: Vec<(String, Document, bool)> = files
        .into_iter()
        .filter_map(|(p, archived)| {
            let doc = if p == path {
                Document::parse(candidate)
            } else {
                project.read(&p).and_then(|s| Document::parse(&s))
            };
            doc.ok().map(|d| (p, d, archived))
        })
        .collect();
    let mut ids = crate::citations::IdMap::default();
    for (p, doc, archived) in &docs {
        ids.add(p, doc, *archived);
    }
    let gone: BTreeSet<String> = dropped
        .iter()
        .filter(|id| !ids.carries(id))
        .cloned()
        .collect();
    if gone.is_empty() {
        return vec![];
    }
    docs.iter()
        .flat_map(|(p, doc, _)| {
            crate::citations::mentions(doc, &gone)
                .into_iter()
                .map(move |(id, at)| json!({"id": id, "path": p, "line": doc.file_line(at)}))
        })
        .collect()
}

#[derive(Default)]
struct Preconditions<'a> {
    require_commit: bool,
    archive: Option<crate::archive::ArchiveGuard>,
    policy_guard: Option<String>,
    restore: Option<crate::history::Restore<'a>>,
}

fn write(
    project: &Project,
    path: &str,
    ops: &[Op],
    preview: bool,
    timeout: Option<u64>,
    by: Attribution,
    preconditions: Preconditions<'_>,
) -> Response {
    let Preconditions {
        require_commit,
        policy_guard,
        archive,
        restore,
    } = preconditions;
    let list_delete = ops
        .iter()
        .any(|op| matches!(op, Op::Delete { list: Some(_), .. }));
    let require_commit =
        require_commit || policy_guard.is_some() || archive.is_some() || list_delete;
    if (policy_guard.is_some() || archive.is_some() || list_delete)
        && !matches!(ops, [Op::Delete { .. }])
    {
        return refused(Diagnostic::new(
            ErrorCode::InvalidRequest,
            "archive, policy and list preconditions require exactly one delete",
        ));
    }
    if ops.len() > 1 && ops.iter().any(|op| matches!(op, Op::Refresh { .. })) {
        return refused(Diagnostic::new(
            ErrorCode::InvalidRequest,
            "evidence refresh is its own write; send refresh as the only op",
        ));
    }
    if ops.is_empty() && restore.is_none() {
        return refused(Diagnostic::new(
            ErrorCode::InvalidRequest,
            "a write needs at least one op",
        ));
    }
    let restore_message = by.message.clone();
    let mut citations = None;
    let mut response = transaction::transact(
        project,
        path,
        Options {
            kind: Kind::Edit,
            preview,
            timeout,
            by,
            require_commit,
            pending: if restore.is_some() {
                Pending::Refuse
            } else {
                Pending::Settle
            },
        },
        |tx| {
            let before = tx.load().map_err(|e| e.with_path(path))?;
            let doc = &before.doc;
            let bindings = &before.bindings;
            let prepared = restore
                .as_ref()
                .map(|r| crate::history::prepare(project, path, doc, r))
                .transpose()
                .map_err(|e| e.with_path(path))?;
            if let Some(p) = &prepared
                && p.equal
            {
                let mut body = p.response.clone();
                body["path"] = json!(path);
                body["status"] = json!(if preview { "preview" } else { "unchanged" });
                body["changed"] = json!(false);
                body["revision"] = json!(doc.revision());
                return Ok(Plan::Unchanged(body));
            }
            let ops = prepared
                .as_ref()
                .map_or(ops, |p| std::slice::from_ref(&p.op));
            let result = (|| {
                let find = |name: &str| {
                    bindings
                        .iter()
                        .find(|b| b.name == name)
                        .cloned()
                        .ok_or_else(|| {
                            Diagnostic::new(
                                ErrorCode::InvalidRequest,
                                format!("{path} is not in collection {name:?}"),
                            )
                        })
                };
                let mut planner = Planner::new(doc);
                let mut changes = vec![];
                for (n, op) in ops.iter().enumerate() {
                    let change = match op {
                        Op::UpdateItem {
                            collection,
                            id,
                            select,
                            guard,
                            guard_scope,
                            set,
                        } => (|| {
                            let bound = find(collection)?;
                            if bound.is_file() {
                                if id.is_some() || select.is_some() {
                                    return Err(Diagnostic::new(
                                        ErrorCode::InvalidRequest,
                                        format!(
                                            "collection {collection} holds file records, which \
                                             are addressed by path; drop id and select"
                                        ),
                                    ));
                                }
                                return schema::plan_record_update(
                                    &mut planner,
                                    &bound,
                                    path,
                                    guard,
                                    *guard_scope,
                                    set,
                                );
                            }
                            let locator = match (id, select) {
                                (Some(id), None) => schema::ItemLocator::Id(id.clone()),
                                (None, Some(Selector::Id { id })) => {
                                    schema::ItemLocator::Id(id.clone())
                                }
                                (None, Some(select @ Selector::Position { .. })) => {
                                    schema::ItemLocator::Position(select.clone())
                                }
                                // Neither: only a file record is named by its path.
                                _ => {
                                    op.check_selectors()?;
                                    return Err(Diagnostic::new(
                                        ErrorCode::InvalidRequest,
                                        crate::edit::UPDATE_ITEM_NAMING,
                                    ));
                                }
                            };
                            schema::plan_update(
                                &mut planner,
                                &bound,
                                &locator,
                                guard,
                                *guard_scope,
                                set,
                            )
                        })(),
                        Op::InsertItem {
                            collection,
                            position,
                            under,
                            guard,
                            fields,
                        } => schema::plan_insert(
                            &mut planner,
                            &find(collection)?,
                            *position,
                            under.as_deref(),
                            guard,
                            fields,
                        ),
                        Op::Delete {
                            collection: Some(collection),
                            ..
                        } => (|| {
                            let bound = find(collection).map_err(|_| Diagnostic::new(
                        ErrorCode::CollectionConflict,
                        format!("the delete target is not an item of collection {collection:?}"),
                    ))?;
                            bound.refuse_file()?;
                            planner.delete_in_collection(op, &bound)
                        })(),
                        Op::Set {
                            collection,
                            typed_guard,
                            ..
                        } if collection.is_some() || typed_guard.is_some() => {
                            schema::plan_bound_set(&mut planner, bindings, op)
                        }
                        Op::Stamp { .. } => schema::plan_stamp(&mut planner, bindings, op),
                        _ => planner.structural(op),
                    }
                    .map_err(|mut e| {
                        e.message = format!("op {n}: {}", e.message);
                        e.with_path(path)
                    })?;
                    changes.push(change);
                }
                let outcome = edit::plan(doc, changes).map_err(|e| e.with_path(path))?;
                Ok::<_, Diagnostic>(outcome)
            })();
            let outcome = result?;
            let candidate_revision =
                crate::revision("document", "", outcome.candidate.as_bytes(), &[]);
            // A write answers with the guards a caller needs next and the news it
            // could not predict: the drift it found. The digests it refreshed as a
            // matter of course are a count, and the preimage revision is the guard
            // the caller already sent.
            let refreshed = outcome.evidence.iter().filter(|e| e.rewritten).count();
            let drift: Vec<_> = outcome
                .evidence
                .into_iter()
                .filter(|e| e.prior_drift && !e.requested)
                .collect();
            let mut body = json!({
                "path": path,
                "revision": candidate_revision,
                "ops": outcome.ops,
                "evidence_refreshed": refreshed,
            });
            if let Some(p) = &prepared {
                body.as_object_mut()
                    .expect("body")
                    .extend(p.response.as_object().expect("restore fields").clone());
            }
            if !drift.is_empty() {
                body["drift"] = json!(drift);
            }
            let dropped: BTreeSet<String> = outcome
                .ops
                .iter()
                .flat_map(|o| o.dropped.iter().cloned())
                .collect();
            if !dropped.is_empty() {
                // Keep this write's candidate even if another writer changes
                // the file before the unlocked citation walk.
                citations = Some((outcome.candidate.clone(), dropped.clone()));
            }
            let mut draft = Draft::new(before, outcome.candidate, body);
            draft.delete_intent =
                crate::delete_receipt::DeleteIntent::from_ops(ops, dropped.into_iter().collect());
            if let Some(intent) = &draft.delete_intent {
                crate::delete_receipt::check_list_citations(
                    project,
                    path,
                    &draft.before.doc,
                    &draft.candidate,
                    intent,
                    &crate::fs::WalkFaults::default(),
                )?;
            }
            draft.policy_guard = policy_guard;
            draft.archive = archive;
            if let Some(p) = prepared {
                let source_oid = p.response["resolved_rev"].as_str().expect("resolved OID");
                let mut message = restore_message.unwrap_or_else(|| {
                    format!(
                        "rivet: restore item in {}",
                        crate::commit::escaped_path(path)
                    )
                });
                message.push_str(&format!("\n\nRestored item from snapshot {source_oid}.\n"));
                draft.message = Some(message);
                draft.restore = Some(p.assertion);
            }
            Ok(draft.ready())
        },
    );
    // transact owns the writer lock and has released it before returning.
    // Preview and applied-but-not-durable responses keep their warnings too.
    if response.body["ok"] == true
        && let Some((candidate, dropped)) = citations
    {
        let dangling = dangling_citations(project, path, &candidate, &dropped);
        if !dangling.is_empty() {
            response.body["dangling_citations"] = json!(dangling);
        }
    }
    response
}

/// The response to a write that landed, commit mode or not.
pub(crate) fn landed(mut body: Value, durable: bool) -> Response {
    body["status"] = json!("applied");
    body["durable"] = json!(durable);
    if durable {
        return ok(body);
    }
    body["warning"] = json!(
        "the directory sync failed: the change is visible but may not survive a crash; re-read before any retry"
    );
    let mut r = ok(body);
    r.exit = 3;
    r
}

pub(crate) fn commit_refused(e: Diagnostic, path: &str, pending: Option<String>) -> Response {
    let e = if matches!(
        e.code,
        ErrorCode::PolicyConflict | ErrorCode::ListCitationRefused
    ) && e.path.is_some()
    {
        e
    } else {
        e.with_path(path)
    };
    let mut r = refused(e);
    if let Some(op) = pending {
        r.body["pending"] = json!(op);
    }
    r
}

fn create(
    project: &Project,
    path: &str,
    content: &str,
    stamp: bool,
    preview: bool,
    timeout: Option<u64>,
    by: Attribution,
) -> Response {
    transaction::transact(
        project,
        path,
        Options {
            kind: Kind::Create(content),
            preview,
            timeout,
            by,
            require_commit: false,
            pending: Pending::Settle,
        },
        |tx| {
            let before = tx.load()?;
            let (candidate, ops) = if stamp {
                let mut planner = Planner::new(&before.doc);
                let change = schema::plan_stamp_collections(
                    &mut planner,
                    &before.bindings,
                    &before.doc.revision(),
                )
                .map_err(|e| e.with_path(path))?;
                let outcome =
                    edit::plan(&before.doc, vec![change]).map_err(|e| e.with_path(path))?;
                (outcome.candidate, outcome.ops)
            } else {
                (content.to_string(), vec![])
            };
            let body = json!({
                "path": path,
                "revision": crate::revision("document", "", candidate.as_bytes(), &[]),
                "ops": ops,
            });
            Ok(Draft::new(before, candidate, body).ready())
        },
    )
}

pub(crate) fn commit_landed(mut body: Value, done: crate::commit::Landed) -> Response {
    body["commit"] = done.commit;
    if done.receipt_failed {
        body["receipt"] = json!("failed");
    }
    landed(body, done.durable)
}

/// Stamp and commit the whole file. A certificate refuses pending recovery and
/// binds the transaction, including later recovery, to the checked snapshot.
fn adopt(
    project: &Project,
    path: &str,
    timeout: Option<u64>,
    by: Attribution,
    expect: Option<&AdoptExpect>,
) -> Response {
    transaction::transact(
        project,
        path,
        Options {
            kind: Kind::Adopt(expect.map(|e| e.head.as_str())),
            preview: false,
            timeout,
            by,
            require_commit: false,
            pending: if expect.is_some() {
                Pending::Refuse
            } else {
                Pending::Settle
            },
        },
        |tx| {
            let before = tx.load().map_err(|e| e.with_path(path))?;
            if let Some(expect) = expect
                && before.doc.revision() != expect.revision
            {
                return Err(Diagnostic::new(ErrorCode::GuardConflict,
                    format!("document revision is not the certified revision (expected {}, found {}); nothing written",
                        expect.revision, before.doc.revision())).with_path(path));
            }
            let mut planner = Planner::new(&before.doc);
            let change = schema::plan_stamp_collections(
                &mut planner,
                &before.bindings,
                &before.doc.revision(),
            )
            .map_err(|e| e.with_path(path))?;
            let outcome = edit::plan(&before.doc, vec![change]).map_err(|e| e.with_path(path))?;
            if expect.is_some() && outcome.candidate != before.doc.source {
                return Err(Diagnostic::new(
                    ErrorCode::GuardConflict,
                    "stamp planning would change the certified document; nothing written",
                )
                .with_path(path));
            }
            let body = json!({
                "path": path,
                "revision": crate::revision("document", "", outcome.candidate.as_bytes(), &[]),
                "ops": outcome.ops,
            });
            Ok(Draft::new(before, outcome.candidate, body).ready())
        },
    )
}

#[allow(clippy::too_many_arguments)]
fn query(
    project: &Project,
    collection: &str,
    clauses: &[Clause],
    fields: Option<Vec<String>>,
    paths: Option<Vec<String>>,
    limit: Option<usize>,
    cursor: Option<String>,
    ready: Option<crate::ready::ReadyRequest>,
) -> Result<Value, Diagnostic> {
    let bound = schema::bind(project, collection)?;
    for clause in clauses {
        schema::check_clause(&bound.schema, clause)?;
    }
    if bound.is_file() && ready.is_some() {
        return Err(Diagnostic::new(
            ErrorCode::InvalidRequest,
            format!("collection {collection} holds file records, which have no ready waits"),
        ));
    }
    if let Some(r) = &ready {
        r.check(&bound.schema)?;
    }
    if let Some(fields) = &fields {
        for f in fields {
            if (f != "id" || bound.is_file()) && !bound.schema.fields.contains_key(f) {
                return Err(Diagnostic::new(
                    ErrorCode::InvalidRequest,
                    format!("{f} is not a field of {}", bound.schema.id),
                ));
            }
        }
    }
    let (mut files, mut diagnostics) = schema::files(project, &bound.config.files);
    files.retain(|f| bound.config.selects(f));
    let every = files.clone();
    if let Some(only) = &paths {
        for p in only {
            if !bound.config.selects(p) {
                return Err(Diagnostic::new(
                    ErrorCode::InvalidRequest,
                    format!("{p} is not in collection {collection}"),
                ));
            }
        }
        files.retain(|f| only.contains(f));
    }
    let limit = limit.unwrap_or(50).clamp(1, 1000);
    let mut digest = Sha256::new();
    digest.update(
        serde_json::to_vec(&json!([
            collection,
            bound.binding,
            bound.digest,
            clauses,
            fields,
            paths
        ]))
        .expect("json"),
    );
    // Waits resolve across every collection and archive file, whatever
    // `paths` lists, and each file is still read once.
    let mut loaded: std::collections::BTreeMap<String, Result<Document, Diagnostic>> =
        Default::default();
    let mut verdicts = Default::default();
    let mut readiness = crate::ready::counts();
    let mut extra = 0;
    if let Some(r) = &ready {
        digest.update(serde_json::to_vec(r).expect("json"));
        let (archive, errors) = schema::files(project, &bound.config.archive);
        diagnostics.extend(errors);
        let mut resolver = crate::ready::Resolver::default();
        let archive: Vec<String> = archive.into_iter().filter(|a| !every.contains(a)).collect();
        for path in every.iter().chain(&archive) {
            let member = every.contains(path);
            let doc = load_hashed(project, path, &mut digest);
            if !files.contains(path) {
                match &doc {
                    Ok(doc) => {
                        extra += 1;
                        digest.update(path.as_bytes());
                        digest.update(doc.revision().as_bytes());
                    }
                    Err(e) => {
                        digest.update(path.as_bytes());
                        diagnostics.push(e.clone());
                    }
                }
            }
            if let Ok(doc) = &doc {
                resolver.add(path, doc, member.then_some((&bound, r.field.as_str())));
            }
            if files.contains(path) {
                loaded.insert(path.clone(), doc);
            }
        }
        verdicts = resolver.finish();
    }
    let mut matched = vec![];
    let mut scanned = 0;
    for path in &files {
        let doc = match loaded
            .remove(path)
            .unwrap_or_else(|| load_hashed(project, path, &mut digest))
        {
            Ok(doc) => doc,
            Err(e) => {
                digest.update(path.as_bytes());
                diagnostics.push(e);
                continue;
            }
        };
        scanned += 1;
        digest.update(path.as_bytes());
        digest.update(doc.revision().as_bytes());
        if bound.is_file() {
            match schema::record(&doc, &bound) {
                Err(e) => diagnostics.push(e.diagnostic().with_path(path.as_str())),
                Ok(values) => {
                    if clauses
                        .iter()
                        .all(|c| schema::matches_in(&bound.schema, &values, None, c))
                    {
                        let projected: Map<String, Value> = match &fields {
                            Some(keep) => values
                                .into_iter()
                                .filter(|(k, _)| keep.contains(k))
                                .collect(),
                            None => values,
                        };
                        matched.push(json!({
                            "path": path,
                            "revision": schema::record_revision(&doc, path, &bound),
                            "fields": projected,
                        }));
                    }
                }
            }
            continue;
        }
        let inst = match schema::instance(&doc, &bound) {
            Ok(Some(inst)) => inst,
            Ok(None) => continue,
            Err(e) => {
                diagnostics.push(e.with_path(path.as_str()));
                continue;
            }
        };
        for &(list, item) in &inst.items {
            let it = &doc.lists[list].items[item];
            let addressing = doc.address_item(list, item);
            let id = addressing.id().map(str::to_string);
            let line = doc.file_line(it.span.start);
            match schema::extract(&doc.item_content(list, item), &bound.schema) {
                Err(errors) => {
                    let mut d = Diagnostic::new(ErrorCode::InvalidItem, errors.join("; "))
                        .with_path(path.as_str())
                        .with_line(line);
                    d.id = id.clone();
                    diagnostics.push(d);
                }
                Ok(values) => {
                    if !clauses
                        .iter()
                        .all(|c| schema::matches_in(&bound.schema, &values, id.as_deref(), c))
                    {
                        continue;
                    }
                    let mut wait = None;
                    if let Some(r) = &ready {
                        let Some(v) = verdicts.get(&(path.clone(), list, item)) else {
                            continue; // done: only open items have a readiness
                        };
                        *readiness.get_mut(&v.readiness).expect("every class") += 1;
                        diagnostics.extend(v.diagnostic(path, line, id.as_deref()));
                        if !r.lists(v.readiness) {
                            continue;
                        }
                        wait = Some(v.to_json());
                    }
                    if let Some(e) = addressing.refusal() {
                        diagnostics.push(e.clone().with_path(path.as_str()).with_line(line));
                    }
                    let projected: Map<String, Value> = match &fields {
                        Some(keep) => values
                            .into_iter()
                            .filter(|(k, _)| keep.contains(k))
                            .collect(),
                        None => values,
                    };
                    let mut entry = json!({
                        "path": path,
                        "id": id,
                        "select": addressing.select(),
                        "addressing": addressing.label(),
                        "line": line,
                        "revision": schema::item_revision(&doc, &bound, list, item),
                        "fields": projected,
                    });
                    if bound.config.subsections {
                        entry["subsection"] = json!(inst.subsection(&doc, list));
                    }
                    if let Some(wait) = wait {
                        entry["wait"] = wait;
                    }
                    matched.push(entry);
                }
            }
        }
    }
    let token = format!("{:x}", digest.finalize());
    let offset = match cursor {
        None => 0,
        Some(c) => {
            let (t, n) = c
                .split_once(':')
                .ok_or_else(|| Diagnostic::new(ErrorCode::InvalidRequest, "malformed cursor"))?;
            if t != token {
                return Err(Diagnostic::new(
                    ErrorCode::StaleCursor,
                    "a scanned file or the schema changed; run the query again",
                ));
            }
            n.parse()
                .map_err(|_| Diagnostic::new(ErrorCode::InvalidRequest, "malformed cursor"))?
        }
    };
    let total = matched.len();
    let page: Vec<Value> = matched.into_iter().skip(offset).take(limit).collect();
    let next = (offset + page.len() < total).then(|| format!("{token}:{}", offset + page.len()));
    let mut body = json!({
        "status": "read",
        "collection": collection,
        "schema": bound.key,
        "consistency": "per_file",
        "files": scanned,
        "items": page,
        "total": total,
        "truncated": next.is_some(),
        "next_cursor": next,
        "diagnostics": diagnostics,
    });
    if bound.is_file() {
        body["item"] = json!("file");
    }
    if ready.is_some() {
        body["readiness"] = json!(readiness);
        body["resolved_from"] = json!(extra);
    }
    Ok(body)
}

#[cfg(test)]
mod request_contract_tests {
    use super::*;

    #[test]
    fn policy_refusals_use_the_source_when_the_root_event_path_is_empty() {
        for (location, expected) in [
            (None, "source.md"),
            (Some(""), "source.md"),
            (Some("archive/month"), "archive/month"),
        ] {
            let mut error = Diagnostic::new(ErrorCode::PolicyConflict, "synthetic traversal event");
            if let Some(path) = location {
                error = error.with_path(path);
            }
            let response = commit_refused(error, "source.md", None);
            assert_eq!(response.body["error"]["path"], expected);
            assert_eq!(response.exit, 2);
        }
    }

    #[test]
    fn adopt_guard_is_canonical_and_plain_request_omits_it() {
        let plain = json!({"op":"adopt", "path":"tracking/T.md"});
        let decoded: Request = serde_json::from_value(plain).unwrap();
        assert!(
            serde_json::to_value(decoded)
                .unwrap()
                .get("expect")
                .is_none()
        );
        for length in [40, 64] {
            let expect = json!({"revision":format!("rivet:v1:document:{}", "a".repeat(32)), "head":"b".repeat(length)});
            let decoded: Request = serde_json::from_value(
                json!({"op":"adopt", "path":"tracking//./T.md", "expect":expect}),
            )
            .unwrap();
            let wire = serde_json::to_value(canonical(decoded).unwrap()).unwrap();
            assert_eq!(wire["path"], "tracking/T.md");
            assert_eq!(wire["expect"], expect);
            let decoded: Request = serde_json::from_value(wire.clone()).unwrap();
            assert_eq!(serde_json::to_value(decoded).unwrap(), wire);
        }
    }

    fn fixture_request(case: &Value) -> Value {
        let command = case["command"].as_str().unwrap();
        let input = case["input"].clone();
        let mut value = match command {
            "analyze" => json!({"query":input}),
            "archive-copy" | "move" | "verify-delete" | "export" | "drilldown"
            | "commit-pending" => {
                json!({"request":input})
            }
            _ => input,
        };
        value["op"] = json!(command.replace('-', "_"));
        value
    }

    #[test]
    fn request_paths_follow_the_shared_command_contract() {
        let cases: Vec<Value> =
            serde_json::from_str(include_str!("../tests/fixtures/request-contract.json")).unwrap();
        for case in cases {
            let value = fixture_request(&case);
            let original: Request = serde_json::from_value(value.clone()).unwrap();
            let expected = serde_json::to_value(canonical(original).unwrap()).unwrap();
            for pointer in case["paths"].as_array().unwrap() {
                let pointer = pointer.as_str().unwrap();
                for spelling in ["./", "embedded"] {
                    let mut alternate = value.clone();
                    let slot = alternate.pointer_mut(pointer).unwrap();
                    let alternate_path = |path: &str| {
                        if spelling == "embedded" {
                            path.replacen('/', "//./", 1)
                        } else {
                            format!("{spelling}{path}")
                        }
                    };
                    *slot = match slot {
                        Value::String(path) => json!(alternate_path(path)),
                        Value::Array(paths) => json!(
                            paths
                                .iter()
                                .map(|p| alternate_path(p.as_str().unwrap()))
                                .collect::<Vec<_>>()
                        ),
                        _ => panic!("path slot must be a string or array"),
                    };
                    let request = serde_json::from_value(alternate).unwrap();
                    assert_eq!(
                        serde_json::to_value(canonical(request).unwrap()).unwrap(),
                        expected,
                        "{case}"
                    );
                }
                let mut bad = value.clone();
                let slot = bad.pointer_mut(pointer).unwrap();
                *slot = if slot.is_array() {
                    json!(["../escape.md"])
                } else {
                    json!("../escape.md")
                };
                assert_eq!(
                    canonical(serde_json::from_value(bad).unwrap())
                        .unwrap_err()
                        .code,
                    ErrorCode::PathRejected,
                    "{case}"
                );
            }
            // Strict contracts, especially signed receipt paths, must reach
            // their own verifier without a normalization that hides tampering.
            if let Some(pointers) = case["strict"].as_array() {
                for pointer in pointers {
                    let mut strict = value.clone();
                    let slot = strict.pointer_mut(pointer.as_str().unwrap()).unwrap();
                    *slot = if slot.is_array() {
                        json!(["./tracking/T.md"])
                    } else {
                        json!("./tracking/T.md")
                    };
                    let request: Request = serde_json::from_value(strict).unwrap();
                    let expected = serde_json::to_value(&request).unwrap();
                    assert_eq!(
                        serde_json::to_value(canonical(request).unwrap()).unwrap(),
                        expected
                    );
                }
            }
        }
    }
}
