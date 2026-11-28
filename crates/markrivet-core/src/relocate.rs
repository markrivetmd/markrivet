//! A move is one receipted transaction over two live documents.
use crate::archive::unit_ids;
use crate::commit::{Record, sha256};
use crate::fs::Project;
use crate::{Diagnostic, ErrorCode};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};

pub(crate) fn refused(reason: crate::MoveReason, message: impl Into<String>) -> Diagnostic {
    Diagnostic::new(ErrorCode::MoveRefused, message).with_move_reason(reason)
}

pub(crate) fn commit_policy(config: &crate::schema::ProjectConfig) -> bool {
    crate::commit::per_write(config.git.as_ref()) == Some(crate::git::OnDirty::Refuse)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MoveRequest {
    pub source: String,
    pub id: String,
    pub guard: String,
    pub collection: Option<String>,
    pub typed_guard: Option<String>,
    pub destination: String,
    pub under: Option<String>,
    pub source_parent: Option<String>,
    pub destination_parent: Option<String>,
    pub before: Option<String>,
    pub lock_timeout_ms: Option<u64>,
    pub message: Option<String>,
    pub session: Option<String>,
}

pub fn move_unit(project: &Project, request: MoveRequest) -> crate::api::Response {
    let mut response = crate::transaction::transact_pair(
        project,
        &request.source,
        &request.destination,
        request.lock_timeout_ms,
        crate::transaction::Attribution {
            message: request.message.clone(),
            session: request.session.clone(),
        },
        |config| plan_move(project, config, &request),
    );
    if response.body["ok"] == true {
        response.body["id"] = serde_json::json!(request.id);
    }
    response
}

fn namespaces(
    config: &crate::schema::ProjectConfig,
    request: &MoveRequest,
) -> Result<(), Diagnostic> {
    use crate::{MoveReason as R, schema};
    for (path, archive, live) in [
        (&request.source, R::SourceArchive, R::SourceNotLive),
        (
            &request.destination,
            R::DestinationArchive,
            R::DestinationNotLive,
        ),
    ] {
        if schema::matches_file_patterns(&config.archive_patterns(), path) {
            return Err(refused(
                archive,
                format!("{path} matches an archive namespace"),
            ));
        }
        // Live by membership, so a file only an `exclude_files` entry keeps
        // out of its collection is no endpoint.
        if !config.live_member(path) {
            return Err(refused(live, format!("{path} matches no live namespace")));
        }
    }
    Ok(())
}

fn plan_move(
    project: &Project,
    config: &crate::schema::ProjectConfig,
    request: &MoveRequest,
) -> Result<crate::transaction::PairPlan, Diagnostic> {
    use crate::{
        Document, MoveReason as R, Selector,
        document::Target,
        edit::{self, Op, Planner},
        schema,
        transaction::{PairDraft, PairPlan},
    };
    namespaces(config, request)?;
    let source = project.read(&request.source)?;
    let destination = project.read(&request.destination)?;
    let a = Document::parse(&source)?;
    let b = Document::parse(&destination)?;
    if !crate::citations::carries_any(&a, std::slice::from_ref(&request.id)) {
        return replay(project, request, &b).map(PairPlan::Unchanged);
    }
    let select = Selector::Id {
        id: request.id.clone(),
    };
    let target = a.resolve(&select)?;
    let kind = match target {
        Target::Item { .. } => "item",
        Target::Section(_) => "section",
        _ => {
            return Err(refused(
                R::NotNative,
                "move requires an item or section-owned native stay",
            ));
        }
    };
    request_kind(request, kind)?;
    let state = move_config(project, config, &request.source, &request.destination)?;
    crate::commit::preflight_pair(
        project,
        (&request.source, &source),
        (&request.destination, &destination),
    )?;
    edit::check_write_blocker(&a)?;
    edit::check_write_blocker(&b)?;
    if a.newline != b.newline {
        return Err(refused(
            R::NewlineMismatch,
            "move endpoints use different newline styles",
        ));
    }
    edit::check_guard(&a, target, &request.guard)?;
    let source_bound = match target {
        Target::Item { list, item } => {
            if a.lists[list].items.len() == 1 {
                return Err(refused(
                    R::SoleItem,
                    "item moves refuse the sole list item (101 E1); move it by hand first",
                ));
            }
            let conflict = || {
                Diagnostic::new(
                    ErrorCode::CollectionConflict,
                    "source or destination is not bound to the requested collection",
                )
            };
            if let Some(name) = request
                .collection
                .as_ref()
                .filter(|n| state.file_collections.contains(*n))
            {
                return Err(Diagnostic::new(
                    ErrorCode::InvalidRequest,
                    format!("collection {name} holds file records, not checkbox items"),
                ));
            }
            let bound = state
                .source
                .iter()
                .find(|b| Some(&b.name) == request.collection.as_ref())
                .ok_or_else(conflict)?;
            let inst = schema::instance(&a, bound)?.ok_or_else(conflict)?;
            if !inst.items.contains(&(list, item)) {
                return Err(conflict());
            }
            edit::Guard::new(
                edit::GuardSubject::Item {
                    bound: bound.clone(),
                    locator: schema::ItemLocator::Id(request.id.clone()),
                },
                request.typed_guard.as_deref().expect("item guard"),
            )
            .check(&a)?;
            Some(bound)
        }
        Target::Section(s) => {
            if a.sections[s].level == 1 {
                return Err(refused(
                    R::ProtectedUnit,
                    "move cannot take a level-1 title",
                ));
            }
            crate::native::preconditions(&a, target, &request.id, request.source_parent.as_deref())
                .map_err(|e| refused(R::NotNative, e.message))?;
            let whole = a.sections[s].whole;
            for bound in &state.source {
                if schema::instance(&a, bound)?.is_some_and(|i| {
                    i.members
                        .iter()
                        .any(|m| whole.contains(a.sections[*m].whole.start))
                }) {
                    return Err(refused(
                        R::ProtectedUnit,
                        "move cannot take a collection queue or member, or a section containing one",
                    ));
                }
            }
            None
        }
        _ => unreachable!(),
    };
    crate::native::markers(&a, target).map_err(|e| refused(R::NotNative, e.message))?;
    let extent = match target {
        Target::Item { list, item } => {
            let it = &a.lists[list].items[item];
            let end = a.lists[list]
                .items
                .get(item + 1)
                .map_or(a.blocks[a.lists[list].block].span.end, |i| i.span.start)
                .max(it.span.end);
            crate::Span {
                start: it.span.start,
                end,
            }
        }
        Target::Section(s) => a.sections[s].whole,
        _ => unreachable!(),
    };
    if crate::markdown::has_reference_definition(extent.slice(&source).expect("unit"))? {
        return Err(refused(
            R::ReferenceDefinition,
            "move unit carries a link reference definition",
        ));
    }
    let ids = unit_ids(&a, target);
    if crate::citations::carries_any(&b, &ids) {
        return Err(refused(
            R::DestinationCarriesId,
            "destination already carries a unit id",
        ));
    }
    archive_absent_in(project, config, &ids).map_err(|e| {
        if e.code == ErrorCode::InvalidRequest {
            refused(R::ArchivedId, e.message)
        } else {
            e
        }
    })?;
    let delete = Op::Delete {
        select,
        guard: request.guard.clone(),
        collection: request.collection.clone(),
        list: None,
    };
    let deletion = match source_bound {
        Some(bound) => Planner::new(&a).delete_in_collection(&delete, bound)?,
        None => Planner::new(&a).structural(&delete)?,
    };
    let source_candidate = edit::plan(&a, vec![deletion])?.candidate;
    let (insertion, new_list) = match target {
        Target::Item { .. } => {
            let bound = state
                .destination
                .iter()
                .find(|b| Some(&b.name) == request.collection.as_ref())
                .ok_or_else(|| {
                    Diagnostic::new(
                        ErrorCode::CollectionConflict,
                        "destination is not bound to the requested collection",
                    )
                })?;
            edit::move_item(&a, target, &b, bound, request.under.as_deref())?
        }
        Target::Section(s) => (
            edit::move_section(
                &a,
                s,
                &b,
                request
                    .destination_parent
                    .as_deref()
                    .expect("section parent"),
                request.before.as_deref(),
            )?,
            None,
        ),
        _ => unreachable!(),
    };
    let destination_candidate = insertion.candidate;
    let ca = Document::parse(&source_candidate)?;
    let cb = Document::parse(&destination_candidate)?;
    let unit = unit_text(&a, target)?;
    verify_pair(&a, target, &b, &ca, &cb, &state, &ids, new_list.as_deref())?;
    let receipt = MoveReceipt {
        version: 1,
        op: String::new(),
        kind: kind.into(),
        id: request.id.clone(),
        ids,
        unit_sha256: sha256(unit.as_bytes()),
        config_sha256: state.digest,
        bindings: state.bindings,
        source: SourceEndpoint {
            path: request.source.clone(),
            guard: request.guard.clone(),
            typed_guard: request.typed_guard.clone(),
            collection: request.collection.clone(),
            parent: request.source_parent.clone(),
            read_sha256: sha256(source.as_bytes()),
            candidate_sha256: sha256(source_candidate.as_bytes()),
        },
        destination: DestinationEndpoint {
            path: request.destination.clone(),
            collection: request.collection.clone(),
            under: request.under.clone(),
            parent: request.destination_parent.clone(),
            before: request.before.clone(),
            read_sha256: sha256(destination.as_bytes()),
            candidate_sha256: sha256(destination_candidate.as_bytes()),
        },
    };
    Ok(PairPlan::Ready(Box::new(PairDraft {
        source,
        source_candidate,
        destination,
        destination_candidate,
        receipt,
    })))
}

fn request_kind(request: &MoveRequest, kind: &str) -> Result<(), Diagnostic> {
    let valid = match kind {
        "item" => {
            request.collection.is_some()
                && request.typed_guard.is_some()
                && request.source_parent.is_none()
                && request.destination_parent.is_none()
                && request.before.is_none()
        }
        "section" => {
            request.collection.is_none()
                && request.typed_guard.is_none()
                && request.under.is_none()
                && request.source_parent.is_some()
                && request.destination_parent.is_some()
        }
        _ => false,
    };
    if valid {
        Ok(())
    } else {
        Err(Diagnostic::new(
            ErrorCode::InvalidRequest,
            "item moves require collection and typed_guard; section moves require source_parent and destination_parent; fields of the other kind are invalid",
        ))
    }
}

fn unit_text(doc: &crate::Document, target: crate::document::Target) -> Result<&str, Diagnostic> {
    let span = match target {
        crate::document::Target::Section(s) => doc
            .section_content_span(s)
            .map_err(|e| refused(crate::MoveReason::NotNative, e.message))?,
        _ => doc.target_span(target).0,
    };
    Ok(span.slice(&doc.source).expect("unit extent"))
}

pub(crate) fn matches_unit(
    doc: &crate::Document,
    id: &str,
    raw: &str,
    ids: &[String],
) -> Result<crate::document::Target, Diagnostic> {
    use crate::{MoveReason as R, Selector, document::Target};
    let target = doc
        .resolve(&Selector::Id { id: id.to_string() })
        .map_err(|e| refused(R::UnitMatches, e.message))?;
    if !matches!(target, Target::Item { .. } | Target::Section(_))
        || unit_text(doc, target)? != raw
        || unit_ids(doc, target) != ids
    {
        return Err(refused(
            R::UnitMatches,
            "moved bytes, identities or item ownership differ",
        ));
    }
    crate::native::markers(doc, target).map_err(|e| refused(R::UnitMatches, e.message))?;
    Ok(target)
}

struct UnitEvidence {
    text: String,
    markers: Option<Vec<crate::archive::SectionMarker>>,
}

impl UnitEvidence {
    fn from_source(
        doc: &crate::Document,
        target: crate::document::Target,
    ) -> Result<Self, Diagnostic> {
        let markers = match target {
            crate::document::Target::Section(s) => Some(
                crate::archive::section_markers(doc, s)
                    .map_err(|e| refused(crate::MoveReason::NotNative, e.message))?,
            ),
            _ => None,
        };
        Ok(Self {
            text: unit_text(doc, target)?.to_string(),
            markers,
        })
    }

    fn matches(
        &self,
        doc: &crate::Document,
        id: &str,
        ids: &[String],
    ) -> Result<crate::document::Target, Diagnostic> {
        let target = matches_unit(doc, id, &self.text, ids)?;
        match (&self.markers, target) {
            (Some(markers), crate::document::Target::Section(s)) => {
                crate::archive::section_matches(doc, s, &self.text, ids, markers)
                    .map_err(|e| refused(crate::MoveReason::UnitMatches, e.message))?;
            }
            (None, crate::document::Target::Item { .. }) => {}
            _ => {
                return Err(refused(
                    crate::MoveReason::UnitMatches,
                    "moved unit kind differs from its provenance",
                ));
            }
        }
        Ok(target)
    }
}

#[derive(PartialEq, Eq)]
struct Membership {
    sections: Vec<SectionMembership>,
    instances: std::collections::BTreeMap<String, Option<Vec<String>>>,
    items: std::collections::BTreeMap<(String, Vec<String>), usize>,
}

#[derive(PartialEq, Eq)]
struct SectionMembership {
    level: u8,
    path: Vec<String>,
    ids: Vec<String>,
    holding: Vec<String>,
}

fn membership(
    doc: &crate::Document,
    bounds: &[crate::schema::Bound],
    skip: &[String],
) -> Result<Membership, Diagnostic> {
    use std::collections::{BTreeMap, BTreeSet};
    let moved = moved_section(doc, skip);
    let outside = |at| moved.is_none_or(|span| !span.contains(at));
    let mut instances = BTreeMap::new();
    let mut members = Vec::new();
    let mut section_members = Vec::new();
    for bound in bounds {
        let inst = crate::schema::instance(doc, bound)?;
        instances.insert(
            bound.name.clone(),
            inst.as_ref().map(|i| {
                let ids = doc.section_ids(i.section);
                if ids.is_empty() {
                    doc.sections[i.section].path.clone()
                } else {
                    ids
                }
            }),
        );
        section_members.push((
            bound.name.clone(),
            inst.as_ref()
                .map_or_else(BTreeSet::new, |i| i.members.iter().copied().collect()),
        ));
        members.push((
            bound.name.clone(),
            inst.map_or_else(BTreeSet::new, |i| i.items.into_iter().collect()),
        ));
    }
    let mut items = BTreeMap::new();
    for (li, list) in doc.lists.iter().enumerate() {
        for (ii, item) in list.items.iter().enumerate() {
            let ids = doc.stay_ids(&item.markers);
            if !outside(item.span.start) || ids.iter().any(|id| skip.contains(id)) {
                continue;
            }
            let key = if ids.is_empty() {
                format!(
                    "raw:{}",
                    sha256(item.span.slice(&doc.source).expect("item").as_bytes())
                )
            } else {
                format!("ids:{}", serde_json::to_string(&ids).expect("ids"))
            };
            let holding = members
                .iter()
                .filter(|(_, set)| set.contains(&(li, ii)))
                .map(|(name, _)| name.clone())
                .collect();
            *items.entry((key, holding)).or_default() += 1;
        }
    }
    let sections = doc
        .sections
        .iter()
        .enumerate()
        .filter(|(_, s)| outside(s.whole.start))
        .map(|(s, x)| SectionMembership {
            level: x.level,
            path: x.path.clone(),
            ids: doc.section_ids(s),
            holding: section_members
                .iter()
                .filter(|(_, set)| set.contains(&s))
                .map(|(name, _)| name.clone())
                .collect(),
        })
        .collect();
    Ok(Membership {
        sections,
        instances,
        items,
    })
}

fn moved_section(doc: &crate::Document, ids: &[String]) -> Option<crate::Span> {
    let id = ids.first()?;
    match doc.resolve(&crate::Selector::Id { id: id.clone() }).ok()? {
        crate::document::Target::Section(s) => Some(doc.sections[s].whole),
        _ => None,
    }
}

fn subtree_membership(
    doc: &crate::Document,
    section: usize,
    bounds: &[crate::schema::Bound],
) -> Result<Vec<Vec<String>>, Diagnostic> {
    let mut instances = Vec::new();
    for bound in bounds {
        if let Some(instance) = crate::schema::instance(doc, bound)
            .map_err(|e| refused(crate::MoveReason::MembershipChanged, e.message))?
        {
            instances.push((&bound.name, instance));
        }
    }
    let whole = doc.sections[section].whole;
    let mut subjects = Vec::new();
    for (s, x) in doc
        .sections
        .iter()
        .enumerate()
        .filter(|(_, x)| whole.contains(x.whole.start))
    {
        subjects.push((
            x.whole.start,
            instances
                .iter()
                .filter(|(_, i)| i.members.contains(&s))
                .map(|(name, _)| (*name).clone())
                .collect(),
        ));
    }
    for (l, list) in doc.lists.iter().enumerate() {
        for (i, item) in list
            .items
            .iter()
            .enumerate()
            .filter(|(_, i)| i.checkbox.is_some() && whole.contains(i.span.start))
        {
            subjects.push((
                item.span.start,
                instances
                    .iter()
                    .filter(|(_, inst)| inst.items.contains(&(l, i)))
                    .map(|(name, _)| (*name).clone())
                    .collect(),
            ));
        }
    }
    subjects.sort_by_key(|(at, _)| *at);
    Ok(subjects.into_iter().map(|(_, holding)| holding).collect())
}

fn list_shapes(
    doc: &crate::Document,
    without: Option<&str>,
    skip: &[String],
) -> Vec<(bool, bool, String)> {
    doc.lists
        .iter()
        .filter(|list| {
            moved_section(doc, skip)
                .is_none_or(|span| !span.contains(doc.blocks[list.block].span.start))
                && without.is_none_or(|id| {
                    !doc.stay_ids(&doc.blocks[list.block].markers)
                        .iter()
                        .any(|x| x == id)
                })
        })
        .map(|list| {
            let prefix = list.items[0]
                .prefix
                .slice(&doc.source)
                .expect("prefix")
                .trim_start_matches([' ', '\t']);
            let marker = if list.ordered {
                prefix
                    .trim_start_matches(|c: char| c.is_ascii_digit())
                    .chars()
                    .next()
            } else {
                prefix.chars().next()
            };
            (
                list.ordered,
                list.tight,
                marker.map_or_else(String::new, |c| c.to_string()),
            )
        })
        .collect()
}

fn replay_error(message: impl Into<String>) -> Diagnostic {
    let message = message.into();
    Diagnostic::new(
        if message.starts_with("git read exceeded") {
            ErrorCode::LimitExceeded
        } else {
            ErrorCode::HistoryUnavailable
        },
        message,
    )
}

fn retry_key(receipt: &MoveReceipt, request: &MoveRequest) -> bool {
    receipt.id == request.id
        && receipt.source.path == request.source
        && receipt.source.guard == request.guard
        && receipt.source.typed_guard == request.typed_guard
        && receipt.source.collection == request.collection
        && receipt.source.parent == request.source_parent
        && receipt.destination.path == request.destination
        && receipt.destination.collection == request.collection
        && receipt.destination.under == request.under
        && receipt.destination.parent == request.destination_parent
        && receipt.destination.before == request.before
}

fn replay_blob(git: &crate::git::Git, oid: &str) -> Result<String, Diagnostic> {
    let out = git
        .inspect_input_limit(
            &["cat-file", "blob", oid],
            None,
            crate::fs::MAX_FILE_BYTES as usize,
        )
        .map_err(|e| replay_error(e.to_string()))?;
    if !out.ok() {
        return Err(replay_error(out.failure("git cat-file blob")));
    }
    String::from_utf8(out.stdout).map_err(|_| replay_error("move history blob is not UTF-8"))
}

fn replay_evidence(
    git: &crate::git::Git,
    sha: &str,
    parents: &[String],
    receipt: &MoveReceipt,
) -> Result<Option<UnitEvidence>, Diagnostic> {
    use crate::{Document, Selector, document::Target};
    let [parent] = parents else { return Ok(None) };
    let changed = crate::commit::changed_paths(git, sha).map_err(replay_error)?;
    let mut paths: Vec<_> = changed.iter().map(|(p, _)| p.as_str()).collect();
    paths.sort();
    let mut expected = [
        receipt.source.path.as_str(),
        receipt.destination.path.as_str(),
    ];
    expected.sort();
    if paths != expected {
        return Ok(None);
    }
    let mut blobs = Vec::new();
    for (path, read, candidate) in [
        (
            &receipt.source.path,
            &receipt.source.read_sha256,
            &receipt.source.candidate_sha256,
        ),
        (
            &receipt.destination.path,
            &receipt.destination.read_sha256,
            &receipt.destination.candidate_sha256,
        ),
    ] {
        let Some(base) = crate::git::tree_entry(git, parent, path).map_err(replay_error)? else {
            return Ok(None);
        };
        let Some((_, after)) = changed.iter().find(|(p, _)| p == path) else {
            return Ok(None);
        };
        if !["100644", "100755"].contains(&base.mode.as_str()) || base.mode != after.mode {
            return Ok(None);
        }
        let before = replay_blob(git, &base.oid)?;
        let after = replay_blob(git, &after.oid)?;
        if sha256(before.as_bytes()) != *read || sha256(after.as_bytes()) != *candidate {
            return Ok(None);
        }
        blobs.push((before, after));
    }
    let a = Document::parse(&blobs[0].0)?;
    let ca = Document::parse(&blobs[0].1)?;
    let cb = Document::parse(&blobs[1].1)?;
    let target = match a.resolve(&Selector::Id {
        id: receipt.id.clone(),
    }) {
        Ok(target @ (Target::Item { .. } | Target::Section(_))) => target,
        _ => return Ok(None),
    };
    let proof = UnitEvidence::from_source(&a, target)?;
    let raw = &proof.text;
    if let Target::Section(s) = target {
        if receipt.kind != "section" {
            return Ok(None);
        }
        if crate::native::preconditions(&a, target, &receipt.id, receipt.source.parent.as_deref())
            .is_err()
        {
            return Ok(None);
        }
        let Some(destination_parent) = receipt.destination.parent.as_deref() else {
            return Ok(None);
        };
        let b = Document::parse(&blobs[1].0)?;
        let candidate = match crate::edit::move_section(
            &a,
            s,
            &b,
            destination_parent,
            receipt.destination.before.as_deref(),
        ) {
            Ok(outcome) => outcome.candidate,
            Err(_) => return Ok(None),
        };
        if candidate != cb.source {
            return Ok(None);
        }
    } else if receipt.kind != "item" {
        return Ok(None);
    }
    if a.target_revision(target) != receipt.source.guard
        || unit_ids(&a, target) != receipt.ids
        || sha256(raw.as_bytes()) != receipt.unit_sha256
        || crate::citations::carries_any(&ca, &receipt.ids)
        || proof.matches(&cb, &receipt.id, &receipt.ids).is_err()
    {
        return Ok(None);
    }
    Ok(Some(proof))
}

fn replay(
    project: &Project,
    request: &MoveRequest,
    current: &crate::Document,
) -> Result<serde_json::Value, Diagnostic> {
    use crate::{
        commit,
        git::{Git, ReadBudget},
    };
    let git = Git::new(project).bounded(ReadBudget::new(96 * 1024 * 1024));
    let history = git
        .log(&[
            "--first-parent",
            "--max-count=4096",
            "--format=%H",
            "--",
            &request.destination,
        ])
        .map_err(|e| replay_error(e.to_string()))?;
    if !history.ok() {
        return Err(replay_error(history.failure("git log")));
    }
    let ids = std::str::from_utf8(&history.stdout)
        .map_err(|_| replay_error("invalid history object ids"))?;
    for sha in ids.lines() {
        if !crate::git_object::valid_oid(sha, sha.len(), crate::git_object::OidCase::Canonical) {
            return Err(replay_error("invalid move history object id"));
        }
        let (parents, message) = commit::commit_parts(&git, sha).map_err(replay_error)?;
        let values: Vec<_> = message
            .lines()
            .filter(|line| commit::reserved_key(line) == Some("rivet-move"))
            .collect();
        if values.is_empty() {
            continue;
        }
        let [line] = values.as_slice() else {
            return Err(Diagnostic::new(
                ErrorCode::InvalidRequest,
                "move history has duplicate move trailers",
            ));
        };
        let payload = line.strip_prefix("Rivet-Move: ").ok_or_else(|| {
            Diagnostic::new(ErrorCode::InvalidRequest, "invalid move trailer spelling")
        })?;
        let receipt = MoveReceipt::decode(payload)?;
        if receipt.id == request.id
            && receipt.source.path == request.source
            && receipt.destination.path == request.destination
        {
            request_kind(request, &receipt.kind)?;
        }
        if !retry_key(&receipt, request) {
            continue;
        }
        if commit::op_trailers(&message) != [receipt.op.clone()] {
            continue;
        }
        let Some(raw) = replay_evidence(&git, sha, &parents, &receipt)? else {
            continue;
        };
        let status = if !crate::citations::carries_any(current, &receipt.ids) {
            "absent"
        } else if raw.matches(current, &receipt.id, &receipt.ids).is_ok() {
            "matches"
        } else {
            "changed"
        };
        return Ok(
            serde_json::json!({"status":"unchanged","source":request.source,"destination":request.destination,"id":request.id,
            "commit":{"status":"committed","sha":sha,"op":receipt.op},"current":status}),
        );
    }
    Err(Diagnostic::new(
        ErrorCode::NotFound,
        "source id is absent and no verifiable move receipt matches every request precondition",
    ))
}

#[allow(clippy::too_many_arguments)]
fn verify_pair(
    a: &crate::Document,
    target: crate::document::Target,
    b: &crate::Document,
    ca: &crate::Document,
    cb: &crate::Document,
    state: &MoveConfig,
    ids: &[String],
    new_list: Option<&str>,
) -> Result<(), Diagnostic> {
    use crate::{MoveReason as R, document::Target};
    if crate::citations::carries_any(ca, ids) {
        return Err(refused(
            R::UnitMatches,
            "source candidate still carries a moved identity",
        ));
    }
    let proof = UnitEvidence::from_source(a, target)?;
    let moved = proof.matches(cb, &ids[0], ids)?;
    if let (Target::Section(s), Target::Section(d)) = (target, moved)
        && subtree_membership(a, s, &state.source)?
            != subtree_membership(cb, d, &state.destination)?
    {
        return Err(refused(
            R::MembershipChanged,
            "a moved section descendant would join a different collection",
        ));
    }
    if list_shapes(a, None, ids) != list_shapes(ca, None, ids)
        || list_shapes(b, None, ids) != list_shapes(cb, new_list, ids)
    {
        return Err(refused(
            R::ListShape,
            "move would split a list or change its marker or tightness",
        ));
    }
    let membership = |doc: &crate::Document, bounds: &[crate::schema::Bound]| {
        self::membership(doc, bounds, ids).map_err(|e| refused(R::MembershipChanged, e.message))
    };
    if membership(a, &state.source)? != membership(ca, &state.source)?
        || membership(b, &state.destination)? != membership(cb, &state.destination)?
    {
        return Err(refused(
            R::MembershipChanged,
            "a pre-existing item, member or collection instance would change membership",
        ));
    }
    if matches!(target, Target::Section(_)) {
        return Ok(());
    }
    let holding = |doc: &crate::Document,
                   bounds: &[crate::schema::Bound],
                   target: Target|
     -> Result<
        std::collections::BTreeMap<String, serde_json::Map<String, serde_json::Value>>,
        Diagnostic,
    > {
        let Target::Item { list, item } = target else {
            return Err(refused(
                R::UnitMatches,
                "unit no longer resolves to an item",
            ));
        };
        let mut fields = std::collections::BTreeMap::new();
        for bound in bounds {
            if crate::schema::instance(doc, bound)?.is_some_and(|i| i.items.contains(&(list, item)))
            {
                fields.insert(
                    bound.name.clone(),
                    crate::schema::extract(&doc.item_content(list, item), &bound.schema)
                        .map_err(|e| Diagnostic::new(ErrorCode::InvalidItem, e.join("; ")))?,
                );
            }
        }
        Ok(fields)
    };
    let before = holding(a, &state.source, target)?;
    let after = holding(cb, &state.destination, moved)?;
    if before.keys().ne(after.keys()) {
        return Err(refused(
            R::MembershipChanged,
            "moved item would join a different set of collections",
        ));
    }
    if before != after {
        return Err(refused(
            R::FieldsChanged,
            "moved item's typed fields would change",
        ));
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceEndpoint {
    pub path: String,
    pub guard: String,
    pub typed_guard: Option<String>,
    pub collection: Option<String>,
    pub parent: Option<String>,
    pub read_sha256: String,
    pub candidate_sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DestinationEndpoint {
    pub path: String,
    pub collection: Option<String>,
    pub under: Option<String>,
    pub parent: Option<String>,
    pub before: Option<String>,
    pub read_sha256: String,
    pub candidate_sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Binding {
    pub collection: String,
    pub binding: String,
    pub digest: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MoveReceipt {
    pub version: u32,
    pub op: String,
    pub kind: String,
    pub id: String,
    pub ids: Vec<String>,
    pub unit_sha256: String,
    pub config_sha256: String,
    pub source: SourceEndpoint,
    pub destination: DestinationEndpoint,
    pub bindings: Vec<Binding>,
}

impl MoveReceipt {
    pub(crate) fn encode(&self) -> Result<String, Diagnostic> {
        let bytes = serde_json::to_vec(self).expect("move receipt serialises");
        if self.version != 1 || bytes.len() > crate::delete_receipt::MAX_RECEIPT {
            return Err(Diagnostic::new(
                ErrorCode::InvalidRequest,
                "unsupported move receipt or receipt exceeds 64 KiB",
            ));
        }
        Ok(URL_SAFE_NO_PAD.encode(bytes))
    }

    pub fn decode(value: &str) -> Result<Self, Diagnostic> {
        let bad = || {
            Diagnostic::new(
                ErrorCode::InvalidRequest,
                "invalid or noncanonical move receipt",
            )
        };
        if value.len() > crate::delete_receipt::MAX_ENCODED_RECEIPT {
            return Err(bad());
        }
        let bytes = URL_SAFE_NO_PAD.decode(value).map_err(|_| bad())?;
        let receipt: Self = serde_json::from_slice(&bytes).map_err(|_| bad())?;
        if receipt.encode()? != value {
            return Err(bad());
        }
        Ok(receipt)
    }

    pub(crate) fn identifies(&self, rec: &Record) -> bool {
        rec.peer.as_ref().is_some_and(|p| {
            self.op == rec.op
                && self.source.path == rec.path
                && self.destination.path == p.path
                && self.source.read_sha256 == rec.read_sha256
                && self.source.candidate_sha256 == rec.candidate_sha256
                && self.destination.read_sha256 == p.read_sha256
                && self.destination.candidate_sha256 == p.candidate_sha256
        })
    }

    pub(crate) fn check_recovery(&self, project: &Project) -> Result<(), (&'static str, String)> {
        let config =
            crate::schema::load_config(project).map_err(|e| ("config_changed", e.detail()))?;
        if !commit_policy(&config) {
            return Err((
                "commit_policy_changed",
                "move requires per-write/refuse".into(),
            ));
        }
        let state = move_config(project, &config, &self.source.path, &self.destination.path)
            .map_err(|e| ("config_changed", e.detail()))?;
        // The digest covers raw globs, so an exclusion added since the move
        // was planned is caught here rather than by it.
        if [&self.source.path, &self.destination.path]
            .iter()
            .any(|p| !config.live_member(p))
        {
            return Err((
                "config_changed",
                "a move endpoint is no longer in a live namespace".into(),
            ));
        }
        if state.digest != self.config_sha256 || state.bindings != self.bindings {
            return Err((
                "config_changed",
                "move bindings or namespaces changed".into(),
            ));
        }
        archive_absent_in(project, &config, &self.ids).map_err(|e| ("archived_id", e.detail()))
    }
}

struct MoveConfig {
    /// Checkbox bindings only: file records are not move units.
    source: Vec<crate::schema::Bound>,
    destination: Vec<crate::schema::Bound>,
    /// File collections binding either endpoint, named so a move that asks
    /// for one is refused by shape.
    file_collections: std::collections::BTreeSet<String>,
    bindings: Vec<Binding>,
    digest: String,
}

fn move_config(
    project: &Project,
    config: &crate::schema::ProjectConfig,
    a: &str,
    b: &str,
) -> Result<MoveConfig, Diagnostic> {
    let mut paths = crate::schema::bound_paths(project, config, &[a, b])?;
    let destination = paths.pop().expect("destination");
    let source = paths.pop().expect("source");
    let file_collections = source
        .iter()
        .chain(&destination)
        .filter(|b| b.is_file())
        .map(|b| b.name.clone())
        .collect();
    let source: Vec<_> = source.into_iter().filter(|b| !b.is_file()).collect();
    let destination: Vec<_> = destination.into_iter().filter(|b| !b.is_file()).collect();
    let mut bindings: Vec<Binding> = source
        .iter()
        .chain(&destination)
        .map(|bound| Binding {
            collection: bound.name.clone(),
            binding: bound.binding.clone(),
            digest: bound.digest.clone(),
        })
        .collect();
    bindings.sort_by(|a, b| {
        (&a.collection, &a.binding, &a.digest).cmp(&(&b.collection, &b.binding, &b.digest))
    });
    bindings.dedup();
    let mut live = config.live_patterns();
    let mut archives = config.archive_patterns();
    live.sort();
    archives.sort();
    let digest = sha256(&serde_json::to_vec(&(&bindings, live, archives)).expect("config digest"));
    Ok(MoveConfig {
        source,
        destination,
        file_collections,
        bindings,
        digest,
    })
}

fn archive_absent_in(
    project: &Project,
    config: &crate::schema::ProjectConfig,
    ids: &[String],
) -> Result<(), Diagnostic> {
    let (paths, errors) = crate::schema::files(project, &config.archive_patterns());
    if let Some(e) = errors.into_iter().next() {
        return Err(e);
    }
    for path in paths {
        let doc = crate::Document::parse(&project.read(&path)?)?;
        if crate::citations::carries_any(&doc, ids) {
            return Err(Diagnostic::new(
                ErrorCode::InvalidRequest,
                format!("archive {path} carries a move id"),
            ));
        }
    }
    Ok(())
}

#[cfg(debug_assertions)]
pub fn test_pair(project: &Project) -> crate::api::Response {
    use crate::transaction::{self, Attribution, PairDraft, PairPlan};
    const UNIT: &str = "- [ ] Payload <!-- stay:pairUnit -->\n";
    transaction::transact_pair(
        project,
        "pair-a.md",
        "pair-b.md",
        None,
        Attribution {
            message: None,
            session: None,
        },
        |config| {
            let source = project.read("pair-a.md")?;
            let destination = project.read("pair-b.md")?;
            if !source.contains(UNIT) && destination.contains(UNIT) {
                return Ok(PairPlan::Unchanged(
                    serde_json::json!({"status":"unchanged"}),
                ));
            }
            if source.matches(UNIT).count() != 1 || destination.contains(UNIT) {
                return Err(Diagnostic::new(
                    ErrorCode::InvalidRequest,
                    "synthetic pair fixture differs",
                ));
            }
            let source_candidate = source.replacen(UNIT, "", 1);
            let destination_candidate = format!("{destination}{UNIT}");
            let state = move_config(project, config, "pair-a.md", "pair-b.md")?;
            let (config_sha256, bindings) = (state.digest, state.bindings);
            let receipt = MoveReceipt {
                version: 1,
                op: String::new(),
                kind: "item".into(),
                id: "pairUnit".into(),
                ids: vec!["pairUnit".into()],
                unit_sha256: sha256(UNIT.as_bytes()),
                config_sha256,
                bindings,
                source: SourceEndpoint {
                    path: "pair-a.md".into(),
                    guard: "synthetic".into(),
                    typed_guard: None,
                    collection: None,
                    parent: None,
                    read_sha256: sha256(source.as_bytes()),
                    candidate_sha256: sha256(source_candidate.as_bytes()),
                },
                destination: DestinationEndpoint {
                    path: "pair-b.md".into(),
                    collection: None,
                    under: None,
                    parent: None,
                    before: None,
                    read_sha256: sha256(destination.as_bytes()),
                    candidate_sha256: sha256(destination_candidate.as_bytes()),
                },
            };
            Ok(PairPlan::Ready(Box::new(PairDraft {
                source,
                source_candidate,
                destination,
                destination_candidate,
                receipt,
            })))
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn receipt() -> MoveReceipt {
        serde_json::from_value(json!({
            "version":1, "op":"0123456789abcdef", "kind":"item", "id":"unit0001",
            "ids":["unit0001"], "unit_sha256":"unit-hash", "config_sha256":"config-hash",
            "source":{"path":"tracking/A.md","guard":"native","typed_guard":"typed",
                "collection":"actions","parent":null,"read_sha256":"before-a","candidate_sha256":"after-a"},
            "destination":{"path":"tracking/B.md","collection":"actions","under":"Action",
                "parent":null,"before":null,"read_sha256":"before-b","candidate_sha256":"after-b"},
            "bindings":[]
        })).unwrap()
    }
    #[test]
    fn move_receipt_round_trip_caps_and_version_refusal() {
        let original = receipt();
        let encoded = original.encode().unwrap();
        assert_eq!(MoveReceipt::decode(&encoded).unwrap(), original);
        let bytes = URL_SAFE_NO_PAD.decode(&encoded).unwrap();
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["source"].as_object().unwrap().len(), 7);
        assert_eq!(value["destination"].as_object().unwrap().len(), 7);
        assert!(value["source"].get("under").is_none());
        assert!(value["destination"].get("guard").is_none());
        let mut unsupported = original.clone();
        unsupported.version = 2;
        let wire = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&unsupported).unwrap());
        assert!(MoveReceipt::decode(&wire).is_err());
        let mut oversized = original;
        oversized.id = "x".repeat(65536);
        assert!(oversized.encode().is_err());
        assert!(
            MoveReceipt::decode(&"x".repeat(crate::delete_receipt::MAX_ENCODED_RECEIPT + 1))
                .is_err()
        );
    }
}
