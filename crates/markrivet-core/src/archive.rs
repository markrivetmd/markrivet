//! Confined archive copies and the immutable provenance required by deletion.
use crate::transaction::{self, Attribution, Draft, Kind, Options, Pending, Plan};
use crate::{
    Diagnostic, Document, ErrorCode, Selector, Span,
    api::{self, Response},
    commit,
    delete_receipt::{hash, require_canonical},
    document::{Attach, Target},
    edit::{self, Op, Planner},
    fs::{MAX_FILE_BYTES, Project, hook},
    git::{self, FileState, Git, Repo},
    schema,
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use serde_json::json;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CopyRequest {
    pub source: String,
    pub id: String,
    pub guard: String,
    pub path: String,
    pub select: Selector,
    pub destination_guard: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    #[serde(default)]
    pub lock_timeout_ms: Option<u64>,
    #[serde(default)]
    pub message: Option<String>,
    #[serde(default)]
    pub session: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CopyIntent {
    pub source: String,
    pub id: String,
    pub guard: String,
    pub path: String,
    pub item_sha256: String,
    pub ids: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CopyReceipt {
    pub version: u32,
    pub op: String,
    pub copy: CopyIntent,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArchiveGuard {
    pub path: String,
    pub copy_op: String,
    pub commit: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
}

fn refused(message: impl Into<String>) -> Diagnostic {
    Diagnostic::new(ErrorCode::ArchiveCopyRefused, message)
}

impl CopyReceipt {
    pub(crate) fn encode(&self) -> Result<String, Diagnostic> {
        let bytes = serde_json::to_vec(self).expect("archive receipt serialises");
        if bytes.len() > crate::delete_receipt::MAX_RECEIPT {
            return Err(refused("archive receipt exceeds 64 KiB"));
        }
        Ok(URL_SAFE_NO_PAD.encode(bytes))
    }
    fn decode(value: &str) -> Result<Self, Diagnostic> {
        if value.len() > crate::delete_receipt::MAX_ENCODED_RECEIPT {
            return Err(refused("archive receipt exceeds limit"));
        }
        let bytes = URL_SAFE_NO_PAD
            .decode(value)
            .map_err(|e| refused(e.to_string()))?;
        let receipt: Self = serde_json::from_slice(&bytes).map_err(|e| refused(e.to_string()))?;
        if receipt.version != 1 || receipt.encode()? != value {
            return Err(refused("unsupported or noncanonical archive receipt"));
        }
        Ok(receipt)
    }
}

fn native_item(doc: &Document, id: &str, guard: &str) -> Result<Target, Diagnostic> {
    let target = doc.resolve(&Selector::Id { id: id.into() })?;
    let list = match target {
        Target::Item { list, .. } if doc.lists[list].items.len() == 1 => {
            Some(crate::edit::ListGuard {
                guard: doc.target_revision(Target::Block(doc.lists[list].block)),
            })
        }
        _ => None,
    };
    native_item_with_list(doc, id, guard, list.as_ref())
}

fn native_item_with_list(
    doc: &Document,
    id: &str,
    guard: &str,
    list: Option<&crate::edit::ListGuard>,
) -> Result<Target, Diagnostic> {
    let select = Selector::Id { id: id.into() };
    let target = doc.resolve(&select)?;
    if !matches!(target, Target::Item { .. }) {
        return Err(refused("source id does not name an action item"));
    }
    let change = Planner::new(doc).structural(&Op::Delete {
        collection: None,
        select,
        guard: guard.into(),
        list: list.cloned(),
    })?;
    edit::plan(doc, vec![change])?;
    Ok(target)
}

/// Kind is derived from the source, never part of a receipt or retry key.
#[derive(Debug, Clone)]
pub(crate) enum Unit {
    Item,
    Section {
        id: String,
        markers: Vec<SectionMarker>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SectionMarker {
    pub id: String,
    pub kind: &'static str,
    marker: Span,
    owner: Span,
}

impl Unit {
    pub(crate) fn from_source(
        doc: &Document,
        target: Target,
        id: &str,
    ) -> Result<Self, Diagnostic> {
        match target {
            Target::Section(s) => Ok(Self::Section {
                id: id.into(),
                markers: section_markers(doc, s)?,
            }),
            _ => Ok(Self::Item),
        }
    }
}

/// Marker provenance includes the physical occurrence and its exact owning
/// block/item, since reference definitions outside a subtree can change which
/// occurrence is live without changing the subtree's bytes or identity set.
pub(crate) fn section_markers(
    doc: &Document,
    section: usize,
) -> Result<Vec<SectionMarker>, Diagnostic> {
    let whole = doc.sections[section].whole;
    let relative = |span: Span| -> Result<Span, Diagnostic> {
        if span.start < whole.start || span.end > whole.end {
            return Err(refused(
                "section archive marker owner extends outside its section",
            ));
        }
        Ok(Span {
            start: span.start - whole.start,
            end: span.end - whole.start,
        })
    };
    let mut markers = Vec::new();
    for m in doc
        .markers
        .iter()
        .filter(|m| whole.start <= m.lexical.span.start && m.lexical.span.end <= whole.end)
    {
        let id = m
            .lexical
            .id
            .clone()
            .ok_or_else(|| refused("section archive marker has no identity"))?;
        let (target, owner, kind) = match m.attach {
            Attach::Item { list, item } => (
                Target::Item { list, item },
                doc.lists[list].items[item].span,
                "item",
            ),
            Attach::Block(b) => {
                let block = &doc.blocks[b];
                let target = if block.heading.is_some() {
                    Target::Section(
                        doc.sections
                            .iter()
                            .position(|s| s.block == b)
                            .expect("heading section"),
                    )
                } else {
                    Target::Block(b)
                };
                (
                    target,
                    Span {
                        start: block.span.start,
                        end: block.region_end,
                    },
                    if block.heading.is_some() {
                        "section"
                    } else {
                        "block"
                    },
                )
            }
            _ => {
                return Err(refused(format!(
                    "section archive has unaddressed stay {id}"
                )));
            }
        };
        if !doc
            .resolve(&Selector::Id { id: id.clone() })
            .is_ok_and(|resolved| resolved == target)
        {
            return Err(refused(format!(
                "section archive stay {id} does not resolve uniquely to its owner"
            )));
        }
        markers.push(SectionMarker {
            id,
            kind,
            marker: relative(m.lexical.span)?,
            owner: relative(owner)?,
        });
    }
    Ok(markers)
}

pub(crate) fn section_matches(
    doc: &Document,
    section: usize,
    text: &str,
    ids: &[String],
    markers: &[SectionMarker],
) -> Result<(), Diagnostic> {
    if doc.section_content_span(section)?.slice(&doc.source) != Some(text)
        || edit::ids_in(doc, doc.sections[section].whole) != ids
        || section_markers(doc, section)? != markers
    {
        return Err(refused(
            "archive section bytes, ids or marker ownership differ from the source",
        ));
    }
    Ok(())
}

pub(crate) fn unit_ids(doc: &Document, target: Target) -> Vec<String> {
    match target {
        Target::Section(s) => edit::ids_in(doc, doc.sections[s].whole),
        _ => doc.target_ids(target),
    }
}

pub(crate) fn unit_span(doc: &Document, target: Target) -> Result<Span, Diagnostic> {
    match target {
        Target::Section(s) => doc.section_content_span(s),
        _ => Ok(doc.target_span(target).0),
    }
}

/// Where an item source's archive authority comes from. Derived from the
/// project and the document, never from the request, so a missing or extra
/// `parent` cannot change which rules apply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ItemSource {
    /// A member of a bound collection: today's item rules.
    Collection,
    /// A direct plain bullet of a heading named by a section binding's `items`.
    Heading { section: usize },
}

fn item_source(
    project: &Project,
    source: &str,
    doc: &Document,
    target: Target,
) -> Result<ItemSource, Diagnostic> {
    let Target::Item { list, item } = target else {
        return Err(refused("source must be an item"));
    };
    let config = schema::load_config(project)?;
    if !config
        .section_archives
        .values()
        .any(|b| b.items.is_some() && schema::matches_file_patterns(&b.files, source))
    {
        return Ok(ItemSource::Collection);
    }
    for (name, c) in &config.collections {
        if !schema::matches_file_patterns(&c.files, source) {
            continue;
        }
        let bound = schema::bind(project, name)?;
        if schema::instance(doc, &bound)?.is_some_and(|i| i.items.contains(&(list, item))) {
            return Ok(ItemSource::Collection);
        }
    }
    if doc.lists[list].items[item].checkbox.is_some() {
        return Err(refused(
            "a task-list item outside a collection is not an archive item source",
        ));
    }
    if doc.lists[list].ordered {
        return Err(refused("a section-binding item source must be a bullet"));
    }
    let section = doc
        .section_of(doc.blocks[doc.lists[list].block].span.start)
        .ok_or_else(|| refused("a section-binding item source must sit under a heading"))?;
    Ok(ItemSource::Heading { section })
}

/// True when `section` is exactly `title / path[0] / path[1] / ...`, each
/// heading one level below the previous one. A skipped level never matches.
fn heading_path_is(doc: &Document, title: usize, section: usize, path: &[String]) -> bool {
    let mut chain = vec![section];
    while let Some(p) = doc.sections[*chain.last().expect("non-empty")].parent {
        chain.push(p);
    }
    chain.reverse();
    chain.len() == path.len() + 1
        && chain[0] == title
        && chain
            .iter()
            .enumerate()
            .all(|(depth, s)| usize::from(doc.sections[*s].level) == depth + 1)
        && chain[1..]
            .iter()
            .zip(path)
            .all(|(s, title)| doc.sections[*s].path.last() == Some(title))
}

/// The one section binding that authorizes `section`'s direct bullets into
/// `path`, and that binding's heading authorization.
fn heading_namespace(
    project: &Project,
    source: &str,
    doc: &Document,
    section: usize,
    item: Span,
    path: &str,
) -> Result<Vec<String>, Diagnostic> {
    if source == path {
        return Err(refused("source and archive must be distinct"));
    }
    let config = schema::load_config(project)?;
    let matched: Vec<_> = config
        .section_archives
        .iter()
        .filter(|(_, b)| {
            schema::matches_file_patterns(&b.files, source)
                && schema::matches_file_patterns(&b.archive, path)
        })
        .collect();
    let [(name, binding)] = matched.as_slice() else {
        return Err(refused(
            "source must match exactly one section_archives binding with this configured archive",
        ));
    };
    let items = binding.items.as_ref().ok_or_else(|| {
        refused(format!(
            "section_archives {name:?} names no items headings for this archive"
        ))
    })?;
    let titles: Vec<usize> = (0..doc.sections.len())
        .filter(|s| doc.sections[*s].level == 1)
        .collect();
    let [title] = titles.as_slice() else {
        return Err(refused(
            "a section-binding item source requires exactly one level-1 title",
        ));
    };
    let mut authorized = false;
    for heading in &items.headings {
        let matching = (0..doc.sections.len())
            .filter(|s| heading_path_is(doc, *title, *s, heading))
            .count();
        if heading_path_is(doc, *title, section, heading) {
            if matching != 1 {
                return Err(refused(format!(
                    "{matching} headings match items heading path {heading:?}"
                )));
            }
            authorized = true;
        }
    }
    if !authorized {
        return Err(refused(format!(
            "heading {:?} is not named in section_archives {name:?} items",
            doc.sections[section].path
        )));
    }
    // Only the item itself must lie outside every collection's protected
    // subtree; a collection under a child heading of the Log is unrelated.
    protected_overlap(project, source, doc, item)?;
    Ok(config.archive_patterns())
}

/// Refuse a unit inside any bound collection's whole protected subtree.
fn protected_overlap(
    project: &Project,
    source: &str,
    doc: &Document,
    whole: Span,
) -> Result<(), Diagnostic> {
    let config = schema::load_config(project)?;
    for (name, c) in &config.collections {
        if !schema::matches_file_patterns(&c.files, source) {
            continue;
        }
        let bound = schema::bind(project, name)?;
        if let Some(instance) = schema::instance(doc, &bound)? {
            let protected = doc.sections[instance.section].whole;
            if whole.start < protected.end && protected.start < whole.end {
                return Err(refused(format!(
                    "section archive overlaps collection {name:?} protected subtree"
                )));
            }
        }
    }
    Ok(())
}

/// The one validation every route for a heading item source passes through:
/// initial copy, copy retry, copy recovery, guarded delete and its recovery.
/// It never carries list authority, is never its list's sole item, and names
/// its stamped heading as `parent`, so no list-version receipt can arise.
fn heading_item(
    doc: &Document,
    target: Target,
    section: usize,
    parent: Option<&str>,
    list: Option<&crate::edit::ListGuard>,
) -> Result<(), Diagnostic> {
    let Target::Item { list: l, .. } = target else {
        return Err(refused("source must be an item"));
    };
    if list.is_some() {
        return Err(refused(
            "a section-binding item source never carries list authority",
        ));
    }
    if doc.lists[l].items.len() == 1 {
        return Err(refused(
            "a section-binding item source cannot be the sole item of its list",
        ));
    }
    let parent = parent
        .ok_or_else(|| refused("a section-binding item source requires a parent precondition"))?;
    let p = doc.resolve(&Selector::Id { id: parent.into() })?;
    if !matches!(p, Target::Section(p) if p == section && crate::native::heading_stay(doc, p, parent))
    {
        return Err(refused(
            "item parent precondition does not name the stamped heading directly above the item",
        ));
    }
    Ok(())
}

fn native_unit(
    project: &Project,
    source: &str,
    doc: &Document,
    id: &str,
    guard: &str,
    parent: Option<&str>,
) -> Result<Target, Diagnostic> {
    let select = Selector::Id { id: id.into() };
    let target = doc.resolve(&select)?;
    let Target::Section(s) = target else {
        if let Target::Item { .. } = target
            && let ItemSource::Heading { section } = item_source(project, source, doc, target)?
        {
            heading_item(doc, target, section, parent, None)?;
            let target = native_item_with_list(doc, id, guard, None)?;
            crate::native::markers(doc, target)?;
            return Ok(target);
        }
        crate::native::preconditions(doc, target, id, parent)?;
        let target = native_item(doc, id, guard)?;
        crate::native::markers(doc, target)?;
        return Ok(target);
    };
    if doc.sections[s].level == 1 {
        return Err(refused("section archive cannot copy the level-1 title"));
    }
    crate::native::preconditions(doc, target, id, parent)?;
    let change = Planner::new(doc).structural(&Op::Delete {
        collection: None,
        list: None,
        select,
        guard: guard.into(),
    })?;
    edit::plan(doc, vec![change])?;
    let whole = doc.sections[s].whole;
    crate::native::markers(doc, target)?;
    // Include nested tasks, which the top-level item model intentionally omits.
    let arena = comrak::Arena::new();
    let root = comrak::parse_document(
        &arena,
        whole.slice(&doc.source).expect("section"),
        &crate::markdown::options(crate::markdown::ParseMode::Document),
    );
    if root
        .descendants()
        .any(|n| matches!(n.data.borrow().value, comrak::nodes::NodeValue::TaskItem(_)))
    {
        return Err(refused("section archive cannot contain any task-list item"));
    }
    unit_span(doc, target)?;
    section_markers(doc, s)?;
    Ok(target)
}

/// Collection item discovery remains limited to its own collection's archives;
/// sections and heading items search every archive namespace.
fn archive_namespace(
    project: &Project,
    source: &str,
    doc: &Document,
    target: Target,
    path: &str,
) -> Result<Vec<String>, Diagnostic> {
    let Target::Section(s) = target else {
        if let ItemSource::Heading { section } = item_source(project, source, doc, target)? {
            return heading_namespace(
                project,
                source,
                doc,
                section,
                doc.target_span(target).0,
                path,
            );
        }
        return Ok(binding(project, source, doc, target, path)?.config.archive);
    };
    if source == path {
        return Err(refused("source and archive must be distinct"));
    }
    let config = schema::load_config(project)?;
    let matched = config
        .section_archives
        .values()
        .filter(|b| {
            schema::matches_file_patterns(&b.files, source)
                && schema::matches_file_patterns(&b.archive, path)
        })
        .count();
    if matched != 1 {
        return Err(refused(
            "source must match exactly one section_archives binding with this configured archive",
        ));
    }
    protected_overlap(project, source, doc, doc.sections[s].whole)?;
    Ok(config.archive_patterns())
}

fn binding(
    project: &Project,
    source: &str,
    doc: &Document,
    target: Target,
    path: &str,
) -> Result<schema::Bound, Diagnostic> {
    if source == path {
        return Err(refused("source and archive must be distinct"));
    }
    let config = schema::load_config(project)?;
    let Target::Item { list, item } = target else {
        return Err(refused("source must be an item"));
    };
    let mut matched = Vec::new();
    for (name, collection) in &config.collections {
        if !collection.files.iter().any(|g| schema::glob(g, source)) {
            continue;
        }
        let bound = schema::bind(project, name)?;
        if schema::instance(doc, &bound)?.is_some_and(|i| i.items.contains(&(list, item)))
            && collection.archive.iter().any(|g| schema::glob(g, path))
        {
            matched.push(bound);
        }
    }
    match matched.len() {
        1 => Ok(matched.remove(0)),
        _ => Err(refused(
            "source must belong to exactly one bound collection with this configured archive",
        )),
    }
}

fn clean(project: &Project, path: &str) -> Result<(), Diagnostic> {
    let repo = Repo::inspect(project);
    let pf = git::preflight(&repo, path, &FileState::read(project, path), false);
    if let Some(i) = pf.ineligible {
        return Err(refused(format!("{path}: {}: {}", i.reason, i.detail)));
    }
    if pf.base != git::Base::KnownClean {
        return Err(refused(format!(
            "{path}: HEAD, index and worktree must agree"
        )));
    }
    Ok(())
}

fn unit_matches(
    doc: &Document,
    intent: &CopyIntent,
    bytes: &str,
    unit: &Unit,
) -> Result<(), Diagnostic> {
    let target = doc.resolve(&Selector::Id {
        id: intent.id.clone(),
    })?;
    if let Unit::Section { markers, .. } = unit {
        let Target::Section(s) = target else {
            return Err(refused(
                "archive source did not retain its section attachment",
            ));
        };
        return section_matches(doc, s, bytes, &intent.ids, markers);
    }
    if !matches!(target, Target::Item { .. })
        || unit_span(doc, target)?.slice(&doc.source) != Some(bytes)
        || unit_ids(doc, target) != intent.ids
    {
        return Err(refused(
            "archive item bytes, ids or attachment differ from the source",
        ));
    }
    Ok(())
}

fn blob(git: &Git, spec: &str) -> Result<String, Diagnostic> {
    let out = git
        .inspect(&["cat-file", "blob", spec], None)
        .map_err(|e| refused(e.to_string()))?;
    if !out.ok() {
        return Err(refused(out.failure("git cat-file blob")));
    }
    if out.stdout.len() as u64 > MAX_FILE_BYTES {
        return Err(refused("archive blob exceeds 8 MiB"));
    }
    String::from_utf8(out.stdout).map_err(|e| refused(e.to_string()))
}

pub(crate) fn evidence(git: &Git, sha: &str) -> Result<CopyReceipt, Diagnostic> {
    if !crate::git_object::valid_oid(sha, sha.len(), crate::git_object::OidCase::Either) {
        return Err(refused("archive commit must be a full object id"));
    }
    let (parents, message) = commit::commit_parts(git, sha).map_err(refused)?;
    if parents.len() != 1 {
        return Err(refused("archive commit must have one parent"));
    }
    let trailers: Vec<_> = message
        .lines()
        .filter(|l| commit::reserved_key(l) == Some("rivet-archive"))
        .collect();
    let [line] = trailers.as_slice() else {
        return Err(refused(
            "archive commit must have exactly one reserved archive trailer",
        ));
    };
    let payload = line
        .strip_prefix("Rivet-Archive: ")
        .ok_or_else(|| refused("invalid archive trailer spelling"))?;
    let receipt = CopyReceipt::decode(payload)?;
    if commit::op_trailers(&message) != [receipt.op.clone()] {
        return Err(refused("archive transaction op mismatch"));
    }
    let changed = commit::changed_paths(git, sha).map_err(refused)?;
    if changed.len() != 1 || changed[0].0 != receipt.copy.path {
        return Err(refused("archive commit changed another path"));
    }
    let (base, _) = git::path_entries(git, &parents[0], &receipt.copy.path).map_err(refused)?;
    if base.is_none_or(|base| {
        base.mode != changed[0].1.mode || !["100644", "100755"].contains(&base.mode.as_str())
    }) {
        return Err(refused(
            "archive copy requires an existing regular file with unchanged mode",
        ));
    }
    Ok(receipt)
}

/// Validate a committed copy against the source, then against today's clean archive.
/// Later appends are allowed; the individual archived item must remain exact.
pub(crate) fn check_guard(
    project: &Project,
    guard: &ArchiveGuard,
    source: &str,
    doc: &Document,
    select: &Selector,
    native_guard: &str,
    list: Option<&crate::edit::ListGuard>,
) -> Result<(), Diagnostic> {
    require_canonical(&guard.path)?;
    let Selector::Id { id } = select else {
        return Err(refused("archive precondition requires an item id selector"));
    };
    let resolved = doc.resolve(select)?;
    let heading = match resolved {
        Target::Item { .. } => match item_source(project, source, doc, resolved)? {
            ItemSource::Heading { section } => Some(section),
            ItemSource::Collection => None,
        },
        _ => None,
    };
    let target = if let Some(section) = heading {
        heading_item(doc, resolved, section, guard.parent.as_deref(), list)?;
        native_unit(
            project,
            source,
            doc,
            id,
            native_guard,
            guard.parent.as_deref(),
        )?
    } else if guard.parent.is_none() {
        native_item_with_list(doc, id, native_guard, list)?
    } else {
        if list.is_some() {
            return Err(refused("list authority requires an item target"));
        }
        native_unit(
            project,
            source,
            doc,
            id,
            native_guard,
            guard.parent.as_deref(),
        )?
    };
    archive_namespace(project, source, doc, target, &guard.path)?;
    let git = Git::new(project);
    let ancestry = git
        .inspect(
            &["merge-base", "--is-ancestor", &guard.commit, "HEAD"],
            None,
        )
        .map_err(|e| refused(e.to_string()))?;
    if !ancestry.ok() {
        return Err(refused("archive copy commit is not ancestral"));
    }
    let receipt = evidence(&git, &guard.commit)?;
    let bytes = unit_span(doc, target)?
        .slice(&doc.source)
        .expect("parsed extent");
    if receipt.op != guard.copy_op
        || receipt.copy.path != guard.path
        || receipt.copy.source != source
        || receipt.copy.id != *id
        || receipt.copy.guard != native_guard
        || receipt.copy.item_sha256 != hash(bytes.as_bytes())
        || receipt.copy.ids != unit_ids(doc, target)
    {
        return Err(refused(
            "archive copy provenance does not describe this exact source item",
        ));
    }
    let committed = Document::parse(&blob(&git, &format!("{}:{}", guard.commit, guard.path))?)?;
    unit_matches(
        &committed,
        &receipt.copy,
        bytes,
        &Unit::from_source(doc, target, id)?,
    )?;
    clean(project, &guard.path)?;
    let current = Document::parse(&project.read(&guard.path)?)?;
    unit_matches(
        &current,
        &receipt.copy,
        bytes,
        &Unit::from_source(doc, target, id)?,
    )
}

pub(crate) fn check_source(
    project: &Project,
    copy: &CopyIntent,
    parent: Option<&str>,
    candidate: Option<&str>,
) -> Result<(), Diagnostic> {
    let source = Document::parse(&project.read(&copy.source)?)?;
    let target = native_unit(
        project,
        &copy.source,
        &source,
        &copy.id,
        &copy.guard,
        parent,
    )?;
    archive_namespace(project, &copy.source, &source, target, &copy.path)?;
    if hash(
        unit_span(&source, target)?
            .slice(&source.source)
            .expect("parsed extent")
            .as_bytes(),
    ) != copy.item_sha256
        || unit_ids(&source, target) != copy.ids
    {
        return Err(refused("archive source changed before publication"));
    }
    if matches!(target, Target::Section(_)) {
        let archived;
        let candidate = match candidate {
            Some(candidate) => candidate,
            None => {
                archived = project.read(&copy.path)?;
                &archived
            }
        };
        let doc = Document::parse(candidate)?;
        let unit = Unit::from_source(&source, target, &copy.id)?;
        let bytes = unit_span(&source, target)?
            .slice(&source.source)
            .expect("source extent");
        unit_matches(&doc, copy, bytes, &unit)?;
    }
    clean(project, &copy.source)
}

fn existing(
    project: &Project,
    namespace: &[String],
    unit: &Unit,
    intent: &CopyIntent,
    bytes: &str,
) -> Result<Option<(String, String)>, Diagnostic> {
    let (paths, errors) = schema::files(project, namespace);
    if let Some(e) = errors.into_iter().next() {
        return Err(e);
    }
    let mut found = false;
    for path in paths {
        let doc = Document::parse(&project.read(&path)?)?;
        if crate::citations::carries_any(&doc, &intent.ids) {
            if path != intent.path || found {
                return Err(refused("another archive already carries a copied stay"));
            }
            unit_matches(&doc, intent, bytes, unit)?;
            found = true;
        }
    }
    if !found {
        return Ok(None);
    }
    clean(project, &intent.path)?;
    let git = Git::new(project);
    let history = git
        .log(&[
            "--first-parent",
            "--max-count=4096",
            "--format=%H",
            "--",
            &intent.path,
        ])
        .map_err(|e| refused(e.to_string()))?;
    if !history.ok() {
        return Err(refused(history.failure("git log")));
    }
    for sha in String::from_utf8_lossy(&history.stdout).lines() {
        if let Ok(receipt) = evidence(&git, sha)
            && receipt.copy == *intent
        {
            let committed = Document::parse(&blob(&git, &format!("{sha}:{}", intent.path))?)?;
            unit_matches(&committed, intent, bytes, unit)?;
            return Ok(Some((receipt.op, sha.into())));
        }
    }
    Err(refused(
        "matching archive bytes lack verifiable committed copy provenance",
    ))
}

pub fn copy(project: &Project, request: CopyRequest) -> Response {
    if let Err(e) =
        require_canonical(&request.source).and_then(|_| require_canonical(&request.path))
    {
        return api::refused(e);
    }
    transaction::transact_with_source(
        project,
        &request.path,
        Options {
            kind: Kind::ArchiveCopy,
            preview: false,
            timeout: request.lock_timeout_ms,
            by: Attribution {
                message: request.message,
                session: request.session,
            },
            require_commit: true,
            pending: Pending::Settle,
        },
        Some(&request.source),
        |tx| {
            let source = Document::parse(&project.read(&request.source)?)?;
            let target = native_unit(
                project,
                &request.source,
                &source,
                &request.id,
                &request.guard,
                request.parent.as_deref(),
            )?;
            let namespace =
                archive_namespace(project, &request.source, &source, target, &request.path)?;
            clean(project, &request.source)?;
            let bytes = unit_span(&source, target)?
                .slice(&source.source)
                .expect("parsed extent");
            let intent = CopyIntent {
                source: request.source.clone(),
                id: request.id.clone(),
                guard: request.guard.clone(),
                path: request.path.clone(),
                item_sha256: hash(bytes.as_bytes()),
                ids: unit_ids(&source, target),
            };
            let unit = Unit::from_source(&source, target, &request.id)?;
            if let Some((op, sha)) = existing(project, &namespace, &unit, &intent, bytes)? {
                hook("archive_retry_checked")?;
                // Keep the ownership snapshot that matched committed provenance:
                // outside reference edits can switch both live owners together.
                check_source(
                    project,
                    &intent,
                    request.parent.as_deref(),
                    Some(&source.source),
                )?;
                if matches!(unit, Unit::Section { .. }) {
                    let current = Document::parse(&project.read(&intent.path)?)?;
                    unit_matches(&current, &intent, bytes, &unit)?;
                    clean(project, &intent.path)?;
                }
                return Ok(Plan::Unchanged(
                    json!({"status":"unchanged", "path":request.path, "copy_op":op, "commit":crate::commit::with_op(&op, json!({"status":"committed", "sha":sha, "copy_op":op}))}),
                ));
            }
            let before = tx.load()?;
            let Target::Section(section) = before.doc.resolve(&request.select)? else {
                return Err(refused("destination selector must name a section"));
            };
            let outcome = match target {
                Target::Section(s) => edit::archive_section(
                    &source,
                    s,
                    &request.id,
                    &before.doc,
                    section,
                    &request.destination_guard,
                )?,
                _ => edit::archive_item(
                    &source,
                    target,
                    &before.doc,
                    section,
                    &request.destination_guard,
                )?,
            };
            let mut draft = Draft::new(before, outcome.candidate, json!({"path":request.path}));
            draft.archive_copy = Some(intent);
            draft.archive_parent = request.parent;
            Ok(draft.ready())
        },
    )
}

#[cfg(test)]
mod compatibility_tests {
    use super::*;

    #[test]
    fn item_copy_intent_matches_b79930c_fixture() {
        let raw = include_str!("../tests/fixtures/archive_item_source.md");
        let doc = Document::parse(raw).unwrap();
        let i = doc
            .markers
            .iter()
            .position(|m| m.lexical.id.as_deref() == Some("Item0001"))
            .unwrap();
        let (fresh, _) = doc.marker_evidence(i).unwrap();
        let doc = Document::parse(&raw.replace("111111111111", &fresh[..12])).unwrap();
        let target = doc
            .resolve(&Selector::Id {
                id: "Item0001".into(),
            })
            .unwrap();
        let guard = doc.target_revision(target);
        native_item(&doc, "Item0001", &guard).unwrap();
        let bytes = unit_span(&doc, target).unwrap().slice(&doc.source).unwrap();
        let receipt = CopyReceipt {
            version: 1,
            op: "fixed-copy-op".into(),
            copy: CopyIntent {
                source: "tracking/source.md".into(),
                id: "Item0001".into(),
                guard,
                path: "archive/month/closed.md".into(),
                item_sha256: hash(bytes.as_bytes()),
                ids: unit_ids(&doc, target),
            },
        };
        assert_eq!(
            serde_json::to_vec(&receipt).unwrap(),
            include_bytes!("../tests/fixtures/archive_item_derived_receipt_b79930c.json")
        );
    }
}
