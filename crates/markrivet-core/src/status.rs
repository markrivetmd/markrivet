//! `markrivet status`: each bound document's state in git against the working
//! tree, which units differ, and the commit-mode bookkeeping for it (pending
//! records, the last write's receipt, ownership, git lock files). Read-only:
//! no Rivet lock, every git call with `GIT_OPTIONAL_LOCKS=0`, one pinned HEAD
//! for the whole report, and each observation stamped with when it was read.
use crate::commit::Bookkeeping;
use crate::document::{Document, Target};
use crate::fs::{Project, WalkEvent, WalkFaults};
use crate::git::{self, FileState, PathGit, Repo, now_millis};
use crate::schema::{self, Bound};
use crate::{Diagnostic, Span};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

#[cfg(test)]
mod tests;

pub fn run(project: &Project, paths: Option<Vec<String>>) -> Result<Value, Diagnostic> {
    run_with(project, paths, &WalkFaults::default())
}

fn run_with(
    project: &Project,
    paths: Option<Vec<String>>,
    faults: &WalkFaults,
) -> Result<Value, Diagnostic> {
    let config = schema::load_config(project)?;
    let mut diagnostics = vec![];
    let mut bindings: BTreeMap<String, Bound> = BTreeMap::new();
    for name in config.collections.keys() {
        match schema::bind(project, name) {
            Ok(b) => {
                bindings.insert(name.clone(), b);
            }
            Err(e) => diagnostics.push(e),
        }
    }
    let collections_of = |path: &str| -> Vec<String> {
        config
            .collections
            .iter()
            .filter(|(name, c)| {
                schema::validate_binding_paths(name, c).is_ok()
                    && schema::matches_file_patterns(&c.files, path)
            })
            .map(|(name, _)| name.clone())
            .collect()
    };
    // A path with a pending record is reported whether or not a collection
    // binds it, so its git detail is read with the rest.
    let books = Bookkeeping::read(project);
    let books_read_at = now_millis();
    diagnostics.extend(books.diagnostics.iter().cloned());
    let pending_paths = books.paths();
    let wanted = |path: &str| match &paths {
        Some(p) => p.iter().any(|q| q == path),
        None => {
            pending_paths.contains(path)
                || (crate::analytics::safe_document_path(path)
                    && !path.starts_with(".rivet/")
                    && !collections_of(path).is_empty())
        }
    };
    let repo = Repo::inspect(project);
    // Completeness describes path enumeration, not per-path git detail or
    // document parsing. Explicit paths already define the whole requested set.
    let mut complete = paths.is_some()
        || (repo.error.is_none() && repo.layout.is_none() && books.diagnostics.is_empty());
    let mut git_error = None;
    let mut git_events = vec![];
    let listing = if repo.error.is_none() && repo.layout.is_none() {
        match repo.paths_observed(None, wanted, |p| {
            if !p.starts_with(b".rivet/") {
                git_events.push(WalkEvent::native_path(p));
            }
        }) {
            Ok(found) => found,
            Err(e) => {
                if paths.is_none() {
                    complete = false;
                }
                git_error = Some(e);
                BTreeMap::new()
            }
        }
    } else {
        BTreeMap::new()
    };
    let mut candidates: BTreeSet<String> = listing.keys().filter(|p| wanted(p)).cloned().collect();
    match &paths {
        Some(p) => candidates.extend(p.iter().cloned()),
        None => {
            let globs: Vec<String> = config
                .collections
                .values()
                .flat_map(|c| c.files.iter().cloned())
                .collect();
            let (files, events) = project.observe_with(&globs, None, faults);
            for event in git_events.iter().chain(&events) {
                complete &= !event.is_gap(&globs);
                if let Some(diagnostic) = event.document_diagnostic(&globs) {
                    diagnostics.push(diagnostic);
                }
            }
            candidates.extend(files.into_iter().filter(|f| wanted(f)));
            candidates.extend(pending_paths.iter().cloned());
        }
    }
    let mut reports = vec![];
    for path in candidates {
        let mut entry = listing.get(&path).cloned().unwrap_or_default();
        if let Some(e) = &git_error {
            entry.error = Some(e.clone());
        }
        let read_at = now_millis();
        let file = FileState::read(project, &path);
        let (base, ineligible) = repo.assess(&entry, &file, git::Creation::No);
        let staged = match &entry.head {
            Some(head) => {
                !matches!(entry.index.as_slice(), [only] if only.stage == 0 && only.oid == head.oid && only.mode == head.mode)
            }
            None => !entry.index.is_empty(),
        };
        let tree = match &file {
            FileState::Missing => json!({"exists": false}),
            FileState::Present { bytes, .. } => {
                json!({"exists": true, "mode": file.git_mode(), "bytes": bytes.len()})
            }
            FileState::Failed(e) => json!({"exists": null, "error": e}),
        };
        let names = collections_of(&path);
        let mut report = json!({
            "path": path,
            "collections": names,
            "base": base,
            "ineligible": ineligible,
            "staged": staged,
            "head": entry.head,
            "index": entry.index,
            "tree": tree,
            "read_at": read_at,
        });
        for (k, v) in books.for_path(&path).as_object().expect("object") {
            report[k] = v.clone();
        }
        let bound: Vec<&Bound> = names.iter().filter_map(|n| bindings.get(n)).collect();
        // A side git or the filesystem could not produce is unknown, not
        // empty: comparing against it would invent additions or deletions.
        let head_known = repo.error.is_none()
            && repo.layout.is_none()
            && entry.error.is_none()
            && (entry.head.is_none() || entry.head_blob.is_some());
        let tree_known = !matches!(file, FileState::Failed(_));
        if head_known
            && tree_known
            && let Some(units) = units(&entry, &file, &bound)
        {
            report["units"] = units;
        }
        reports.push(report);
    }
    // Lock files are listed, never removed, each with the record whose dead
    // git child spawned before it.
    let locks = if repo.layout.is_none() {
        match git::list_locks(project, repo.branch.as_deref()) {
            Ok(found) => json!(
                found
                    .iter()
                    .map(|l| {
                        let mut v = json!(l);
                        v["record"] = json!(books.lock_owner(l));
                        v
                    })
                    .collect::<Vec<_>>()
            ),
            Err(e) => json!({"error": e}),
        }
    } else {
        json!([])
    };
    Ok(json!({
        "status": "read",
        "complete": complete,
        "git": {
            "version": repo.version,
            "head": repo.head,
            "branch": repo.branch,
            "ineligible": repo.blocked(),
            "read_at": repo.read_at,
            "locks": locks,
        },
        "receipts": books.summary(),
        "receipts_read_at": books_read_at,
        "paths": reports,
        "diagnostics": diagnostics,
    }))
}

struct Unit {
    kind: &'static str,
    revision: String,
    /// The enclosing section's title path, for reports.
    context: Vec<String>,
    /// The enclosing section's identity: its id, or its title path and
    /// occurrence when it has none, so two sections with one title differ.
    key: String,
    line: usize,
    start: usize,
    end: usize,
}

fn kind(doc: &Document, target: Target) -> &'static str {
    match target {
        Target::Section(_) => "section",
        Target::Item { .. } => "item",
        Target::Block(b) if doc.blocks[b].list.is_some() => "list",
        Target::Block(_) => "block",
        Target::FrontmatterKey(_) => "frontmatter_key",
        Target::Document => "document",
    }
}

/// The innermost section enclosing `pos`.
fn enclosing(doc: &Document, pos: usize) -> Option<usize> {
    doc.sections
        .iter()
        .enumerate()
        .filter(|(_, s)| s.whole.start <= pos && pos < s.whole.end)
        .max_by_key(|(_, s)| s.path.len())
        .map(|(i, _)| i)
}

fn section_key(doc: &Document, section: Option<usize>) -> (Vec<String>, String) {
    let Some(s) = section else {
        return (vec![], String::new());
    };
    let path = doc.sections[s].path.clone();
    let key = match doc.section_ids(s).first() {
        Some(id) => format!("id:{id}"),
        None => {
            let nth = doc.sections[..s].iter().filter(|o| o.path == path).count();
            format!("path:{}#{nth}", serde_json::to_string(&path).expect("json"))
        }
    };
    (path, key)
}

/// Every unit addressed by exactly one id, and the ids that address more.
fn collect(doc: &Document) -> (BTreeMap<String, Unit>, Vec<String>, Vec<Span>) {
    let mut units = BTreeMap::new();
    let mut duplicates = vec![];
    let mut spans = vec![];
    for (id, targets) in &doc.ids {
        let [target] = targets.as_slice() else {
            duplicates.push(id.clone());
            continue;
        };
        // An orphan or unaddressed stay resolves to the document sentinel;
        // it addresses no unit.
        if matches!(target, Target::Document | Target::FrontmatterKey(_)) {
            continue;
        }
        let (span, _) = doc.target_span(*target);
        spans.push(span);
        let (ctx, key) = match *target {
            Target::Section(s) => section_key(doc, doc.sections[s].parent),
            _ => section_key(doc, enclosing(doc, span.start)),
        };
        units.insert(
            id.clone(),
            Unit {
                kind: kind(doc, *target),
                revision: doc.target_revision(*target),
                context: ctx,
                key,
                line: doc.file_line(span.start),
                start: span.start,
                end: span.end,
            },
        );
    }
    (units, duplicates, spans)
}

/// The bytes outside every unit, in order.
fn residual(source: &str, mut spans: Vec<Span>) -> Vec<u8> {
    spans.sort_by_key(|s| (s.start, s.end));
    let mut out = vec![];
    let mut at = 0;
    for s in spans {
        if s.start > at {
            out.extend_from_slice(&source.as_bytes()[at..s.start]);
            // A separator keeps text either side of a unit from joining up.
            out.push(0);
        }
        at = at.max(s.end);
    }
    if at < source.len() {
        out.extend_from_slice(&source.as_bytes()[at..]);
    }
    out
}

/// Ids not in a longest increasing run of `tree` positions taken in `head`
/// order. Ids are unique, so this is the longest common subsequence in
/// O(n log n).
fn reordered(head: &[&String], tree: &[&String]) -> BTreeSet<String> {
    let at: BTreeMap<&String, usize> = tree.iter().enumerate().map(|(i, id)| (*id, i)).collect();
    let seq: Vec<usize> = head.iter().filter_map(|id| at.get(id).copied()).collect();
    // tails[k]: index into seq of the smallest tail of a run of length k + 1.
    let mut tails: Vec<usize> = vec![];
    let mut prev: Vec<Option<usize>> = vec![None; seq.len()];
    for (i, &v) in seq.iter().enumerate() {
        let k = tails.partition_point(|&t| seq[t] < v);
        prev[i] = k.checked_sub(1).map(|k| tails[k]);
        if k == tails.len() {
            tails.push(i);
        } else {
            tails[k] = i;
        }
    }
    let mut kept = BTreeSet::new();
    let mut cur = tails.last().copied();
    while let Some(i) = cur {
        kept.insert(tree[seq[i]].clone());
        cur = prev[i];
    }
    head.iter()
        .filter(|id| !kept.contains(**id))
        .map(|id| (*id).clone())
        .collect()
}

/// Ids of `units` in `ctx` that are in `ctx` on the other side too, in
/// document order.
fn order<'a>(
    units: &'a BTreeMap<String, Unit>,
    other: &BTreeMap<String, Unit>,
    ctx: &str,
) -> Vec<&'a String> {
    // A container starts where its first child does; it sorts first.
    let mut ids: Vec<(usize, std::cmp::Reverse<usize>, &String)> = units
        .iter()
        .filter(|(id, u)| u.key == ctx && other.get(*id).is_some_and(|o| o.key == ctx))
        .map(|(id, u)| (u.start, std::cmp::Reverse(u.end), id))
        .collect();
    ids.sort();
    ids.into_iter().map(|(_, _, id)| id).collect()
}

/// Unstamped collection items, as (line, item digest) per side.
fn unstamped(doc: &Document, bound: &[&Bound]) -> Vec<(usize, String)> {
    let mut out = vec![];
    for b in bound {
        if let Ok(Some(inst)) = schema::instance(doc, b) {
            for &(list, item) in &inst.items {
                let it = &doc.lists[list].items[item];
                if it.markers.is_empty() {
                    out.push((doc.file_line(it.span.start), doc.item_hash(list, item)));
                }
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

fn parse(bytes: &[u8]) -> Result<Document, Diagnostic> {
    let text = std::str::from_utf8(bytes).map_err(|_| {
        Diagnostic::new(crate::ErrorCode::UnsupportedSyntax, "the file is not UTF-8")
    })?;
    Document::parse(text)
}

/// What differs between HEAD and the tree, unit by unit. `None` when the
/// bytes are the same on both sides.
fn units(entry: &PathGit, file: &FileState, bound: &[&Bound]) -> Option<Value> {
    let head = entry.head_blob.as_deref();
    let tree = match file {
        FileState::Present { bytes, .. } => Some(bytes.as_slice()),
        _ => None,
    };
    if head == tree {
        return None;
    }
    let empty = || Document::parse("").expect("an empty document parses");
    let mut parse_errors = serde_json::Map::new();
    let mut side = |name: &str, bytes: Option<&[u8]>| match bytes.map(parse) {
        None => Some(empty()),
        Some(Ok(doc)) => Some(doc),
        Some(Err(e)) => {
            parse_errors.insert(name.into(), json!(e));
            None
        }
    };
    let (Some(old), Some(new)) = (side("head", head), side("tree", tree)) else {
        return Some(json!({"parse": parse_errors}));
    };
    let (a, a_dup, a_spans) = collect(&old);
    let (b, b_dup, b_spans) = collect(&new);
    let brief = |id: &String, u: &Unit| json!({"id": id, "kind": u.kind, "line": u.line});
    let added: Vec<Value> = b
        .iter()
        .filter(|(id, _)| !a.contains_key(*id))
        .map(|(id, u)| brief(id, u))
        .collect();
    let deleted: Vec<Value> = a
        .iter()
        .filter(|(id, _)| !b.contains_key(*id))
        .map(|(id, u)| brief(id, u))
        .collect();
    let changed: Vec<Value> = b
        .iter()
        .filter(|(id, u)| a.get(*id).is_some_and(|o| o.revision != u.revision))
        .map(|(id, u)| brief(id, u))
        .collect();
    let mut moved = vec![];
    let mut contexts: BTreeSet<&String> = BTreeSet::new();
    for (id, u) in &b {
        if let Some(o) = a.get(id) {
            if o.key != u.key {
                moved.push(json!({"id": id, "kind": u.kind, "line": u.line, "from": o.context, "to": u.context}));
            } else {
                contexts.insert(&u.key);
            }
        }
    }
    for ctx in contexts {
        for id in reordered(&order(&a, &b, ctx), &order(&b, &a, ctx)) {
            let u = &b[&id];
            moved.push(json!({"id": id, "kind": u.kind, "line": u.line, "from": u.context, "to": u.context}));
        }
    }
    let mut out = json!({
        "added": added,
        "deleted": deleted,
        "changed": changed,
        "moved": moved,
    });
    let (ua, ub) = (unstamped(&old, bound), unstamped(&new, bound));
    let mut count: BTreeMap<&String, i64> = BTreeMap::new();
    for (_, h) in &ua {
        *count.entry(h).or_default() += 1;
    }
    let mut new_unstamped = vec![];
    for (line, h) in &ub {
        let c = count.entry(h).or_default();
        if *c > 0 {
            *c -= 1;
        } else {
            new_unstamped.push(json!({"side": "tree", "line": line}));
        }
    }
    let mut count: BTreeMap<&String, i64> = BTreeMap::new();
    for (_, h) in &ub {
        *count.entry(h).or_default() += 1;
    }
    for (line, h) in &ua {
        let c = count.entry(h).or_default();
        if *c > 0 {
            *c -= 1;
        } else {
            new_unstamped.push(json!({"side": "head", "line": line}));
        }
    }
    out["unstamped"] = json!(new_unstamped);
    let (ra, rb) = (
        residual(&old.source, a_spans),
        residual(&new.source, b_spans),
    );
    if ra != rb {
        out["residual"] = json!({"head_bytes": ra.len(), "tree_bytes": rb.len()});
    }
    let mut duplicates: Vec<String> = a_dup.into_iter().chain(b_dup).collect();
    duplicates.sort();
    duplicates.dedup();
    if !duplicates.is_empty() {
        out["duplicate_ids"] = json!(duplicates);
    }
    Some(out)
}

/// The unit-level differences from `head` to `tree`: the ids that differ,
/// and one line per difference for an `Includes-outside-edit:` trailer.
pub(crate) fn outside_edits(
    head: Option<&[u8]>,
    tree: &[u8],
    bound: &[&Bound],
) -> (Vec<String>, Vec<String>) {
    let entry = PathGit {
        head_blob: head.map(<[u8]>::to_vec),
        ..PathGit::default()
    };
    let file = FileState::Present {
        bytes: tree.to_vec(),
        mode: 0o644,
    };
    let Some(diff) = units(&entry, &file, bound) else {
        return (vec![], vec![]);
    };
    let mut ids = vec![];
    let mut lines = vec![];
    for kind in ["added", "deleted", "changed", "moved"] {
        for unit in diff[kind].as_array().into_iter().flatten() {
            if let Some(id) = unit["id"].as_str() {
                if !ids.iter().any(|i| i == id) {
                    ids.push(id.to_string());
                }
                lines.push(format!(
                    "{kind} {} {id}",
                    unit["kind"].as_str().unwrap_or("unit")
                ));
            }
        }
    }
    for u in diff["unstamped"].as_array().into_iter().flatten() {
        let side = if u["side"] == "head" {
            "removed"
        } else {
            "added"
        };
        lines.push(format!("unstamped item {side} at line {}", u["line"]));
    }
    if diff.get("residual").is_some() {
        lines.push("bytes outside any unit".into());
    }
    if diff.get("parse").is_some() {
        lines.push("whole file (a side does not parse)".into());
    }
    (ids, lines)
}
