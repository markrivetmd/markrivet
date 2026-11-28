//! Typed collections: a flat schema bound by version to the checkbox items of
//! one named section in matching files, and optionally of its subsections. Field edits compile to a whole-item
//! rewrite through the structural planner and are re-extracted and compared
//! before anything is written.
use crate::document::{Document, Target, dedent};
use crate::edit::{Change, Planner, Splice};
use crate::fs::Project;
use crate::markers;
use crate::{Diagnostic, ErrorCode, GuardScope, Position, Span, revision};
use comrak::nodes::{AstNode, NodeValue};
use comrak::{Arena, parse_document};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prune: Option<crate::prune_snapshot::PruneConfig>,
    #[serde(default)]
    pub collections: BTreeMap<String, CollectionConfig>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub section_archives: BTreeMap<String, SectionArchiveConfig>,
    /// Per-write commits; absent means off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git: Option<crate::commit::GitConfig>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CollectionConfig {
    /// `id@version` of a schema in `.rivet/schemas/<id>.json`.
    pub schema: String,
    /// Globs relative to the project root: `*`, `?` and whole-segment `**`.
    pub files: Vec<String>,
    /// Exact title of the section holding the items.
    pub section: String,
    #[serde(default)]
    pub level: Option<u8>,
    /// Take the bound section's descendant sections as members too, so a
    /// queue kept under dated `###` batches is one collection.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub subsections: bool,
    /// Exact titles of descendant sections left out, each with its subtree.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exclude: Vec<String>,
    /// Globs of files that hold the collection's pruned items: read only to
    /// resolve `query`'s `ready` waits and cited stay ids. The confined
    /// archive-copy path also lists and writes them; collection writes do not.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub archive: Vec<String>,
}

/// Live section sources and their disjoint history destinations.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SectionArchiveConfig {
    pub files: Vec<String>,
    pub archive: Vec<String>,
    /// Headings whose direct plain bullets may be archived as items into this
    /// binding's history namespace. Absent means sections only.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "items_object"
    )]
    pub items: Option<ItemSources>,
}

/// `items`, when present, is an object: `null` is not an absent key, and a
/// positional array is not the documented shape.
fn items_object<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<ItemSources>, D::Error> {
    use serde::de::Error;
    match Value::deserialize(d)? {
        v @ Value::Object(_) => serde_json::from_value(v)
            .map(Some)
            .map_err(D::Error::custom),
        other => Err(D::Error::custom(format!(
            "items must be an object, found {other}"
        ))),
    }
}

/// Item sources outside collections: exact heading paths under the file's
/// unique level-1 title, each component one heading level deeper.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ItemSources {
    pub headings: Vec<Vec<String>>,
}

impl ProjectConfig {
    pub(crate) fn live_patterns(&self) -> Vec<String> {
        self.collections
            .values()
            .flat_map(|c| &c.files)
            .chain(self.section_archives.values().flat_map(|c| &c.files))
            .cloned()
            .collect()
    }

    pub(crate) fn archive_patterns(&self) -> Vec<String> {
        self.collections
            .values()
            .flat_map(|c| &c.archive)
            .chain(self.section_archives.values().flat_map(|c| &c.archive))
            .cloned()
            .collect()
    }

    pub(crate) fn validate_section_archives(&self) -> Result<(), Diagnostic> {
        let live = self.live_patterns();
        let item_archives: Vec<_> = self.collections.values().flat_map(|c| &c.archive).collect();
        for (name, binding) in &self.section_archives {
            for (field, patterns) in [("files", &binding.files), ("archive", &binding.archive)] {
                validate_file_patterns(patterns).map_err(|mut d| {
                    d.message = format!("section_archives {name:?} {field}: {}", d.message);
                    d
                })?;
            }
            if let Some(items) = &binding.items {
                let refuse = |detail: &str| {
                    Err(invalid_schema(format!(
                        "section_archives {name:?} items.headings: {detail}"
                    )))
                };
                if items.headings.is_empty() {
                    return refuse("must name at least one heading path");
                }
                for (i, path) in items.headings.iter().enumerate() {
                    if path.is_empty() {
                        return refuse("a heading path must name at least one heading");
                    }
                    if path.iter().any(String::is_empty) {
                        return refuse("a heading path component must not be empty");
                    }
                    if items.headings[..i].contains(path) {
                        return refuse(&format!("duplicate heading path {path:?}"));
                    }
                }
            }
            for archive in &binding.archive {
                for other in live.iter().chain(item_archives.iter().copied()) {
                    if globs_intersect(archive, other) {
                        return Err(invalid_schema(format!(
                            "section_archives {name:?} archive glob {archive:?} overlaps live or collection archive glob {other:?}"
                        )));
                    }
                }
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FieldType {
    String,
    Boolean,
    Number,
    Date,
    Enum,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    /// The task checkbox.
    Checkbox,
    /// A leading code span matching one of `tags` (`prefix:*` or exact).
    Tag,
    /// The leading `**strong**` title.
    Title,
    /// A trailing `(YYYY-MM-DD)` inside the title.
    TitleDate,
    /// A line `**Label**: value` in the item's first paragraph; with
    /// `multiline`, its continuation lines up to the next declared label too.
    Label,
    /// The first paragraph's lines outside the title and every declared
    /// label: before the first label, and after each one-line label up to
    /// the next. A `**Lead-in**:` line naming no declared label is text.
    Text,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FieldSpec {
    #[serde(rename = "type")]
    pub kind: FieldType,
    pub from: Source,
    #[serde(default)]
    pub required: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub values: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// A label whose value runs on across its continuation lines, up to the
    /// next declared label line or the end of the paragraph. Without it a
    /// label is one line, and the lines after it are text.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub multiline: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ItemShape {
    Checkbox,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Schema {
    pub id: String,
    pub version: u32,
    pub item: ItemShape,
    pub fields: BTreeMap<String, FieldSpec>,
}

#[derive(Debug, Clone)]
pub struct Bound {
    pub name: String,
    pub config: CollectionConfig,
    pub schema: Schema,
    pub key: String,
    pub digest: String,
    /// Canonical, versioned identity of every binding setting.
    pub binding: String,
}

fn invalid_schema(message: impl Into<String>) -> Diagnostic {
    Diagnostic::new(ErrorCode::InvalidSchema, message)
}

pub fn load_config(project: &Project) -> Result<ProjectConfig, Diagnostic> {
    match project.read_config(&["project.json"])? {
        Some(bytes) => parse_config(&bytes),
        None => Ok(ProjectConfig::default()),
    }
}

fn parse_config(bytes: &[u8]) -> Result<ProjectConfig, Diagnostic> {
    let config: ProjectConfig = serde_json::from_slice(bytes)
        .map_err(|e| invalid_schema(format!(".rivet/project.json: {e}")))?;
    config.validate_section_archives()?;
    Ok(config)
}

/// Resolve a collection and its schema, checking the binding's version.
pub(crate) fn bound_paths(
    project: &Project,
    config: &ProjectConfig,
    paths: &[&str],
) -> Result<Vec<Vec<Bound>>, Diagnostic> {
    for path in paths {
        validate_path_bindings(config, path)?;
    }
    let mut schemas = BTreeMap::<String, Vec<u8>>::new();
    let mut bounds = Vec::new();
    for (name, collection) in &config.collections {
        if !paths
            .iter()
            .any(|p| matches_file_patterns(&collection.files, p))
        {
            continue;
        }
        let id = schema_id(name, collection)?;
        if !schemas.contains_key(id) {
            let bytes = project
                .read_config(&["schemas", &format!("{id}.json")])?
                .ok_or_else(|| {
                    invalid_schema(format!(
                        "schema {id}: .rivet/schemas/{id}.json does not exist"
                    ))
                })?;
            schemas.insert(id.to_string(), bytes);
        }
        bounds.push(bind_bytes(name, collection.clone(), &schemas[id])?);
    }
    Ok(paths
        .iter()
        .map(|path| {
            bounds
                .iter()
                .filter(|b| matches_file_patterns(&b.config.files, path))
                .cloned()
                .collect()
        })
        .collect())
}

/// Resolve a collection and its schema, checking the binding's version.
pub fn bind(project: &Project, name: &str) -> Result<Bound, Diagnostic> {
    bind_with(name, |parts| project.read_config(parts))
}

pub(crate) fn bind_with(
    name: &str,
    mut read: impl FnMut(&[&str]) -> Result<Option<Vec<u8>>, Diagnostic>,
) -> Result<Bound, Diagnostic> {
    let config: ProjectConfig = match read(&["project.json"])? {
        Some(bytes) => parse_config(&bytes)?,
        None => ProjectConfig::default(),
    };
    let collection =
        config.collections.get(name).cloned().ok_or_else(|| {
            invalid_schema(format!("no collection {name:?} in .rivet/project.json"))
        })?;
    let id = schema_id(name, &collection)?;
    let bytes = read(&["schemas", &format!("{id}.json")])?.ok_or_else(|| {
        invalid_schema(format!(
            "schema {id}: .rivet/schemas/{id}.json does not exist"
        ))
    })?;
    bind_bytes(name, collection, &bytes)
}

/// Validate the binding before using its schema name as a confined config path.
pub(crate) fn schema_id<'a>(
    name: &str,
    collection: &'a CollectionConfig,
) -> Result<&'a str, Diagnostic> {
    validate_binding_paths(name, collection)?;
    if !collection.exclude.is_empty() && !collection.subsections {
        return Err(invalid_schema(format!(
            "collection {name}: exclude needs subsections: true"
        )));
    }
    let (id, _version) = collection
        .schema
        .split_once('@')
        .and_then(|(id, v)| Some((id, v.parse::<u32>().ok()?)))
        .ok_or_else(|| invalid_schema(format!("collection {name}: schema must be id@version")))?;
    if !id
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
        || id.is_empty()
    {
        return Err(invalid_schema(format!(
            "schema id {id:?} is not a plain name"
        )));
    }
    Ok(id)
}

/// Resolve a binding against immutable schema bytes already read by a scanner.
pub(crate) fn bind_bytes(
    name: &str,
    collection: CollectionConfig,
    bytes: &[u8],
) -> Result<Bound, Diagnostic> {
    let id = schema_id(name, &collection)?;
    let (_, version) = collection
        .schema
        .split_once('@')
        .expect("validated binding");
    let version: u32 = version.parse().expect("validated version");
    let schema: Schema =
        serde_json::from_slice(bytes).map_err(|e| invalid_schema(format!("schema {id}: {e}")))?;
    if schema.id != id || schema.version != version {
        return Err(invalid_schema(format!(
            "collection {name} binds {id}@{version} but the file declares {}@{}",
            schema.id, schema.version
        )));
    }
    validate_schema(&schema)?;
    let digest = format!("{:x}", Sha256::digest(bytes));
    let binding = binding_fingerprint(&collection);
    Ok(Bound {
        name: name.to_string(),
        key: collection.schema.clone(),
        config: collection,
        schema,
        digest,
        binding,
    })
}

/// Sets have no ordering or multiplicity semantics; defaults are explicit.
fn binding_fingerprint(config: &CollectionConfig) -> String {
    let bytes = serde_json::to_vec(&(
        "rivet-binding-v1",
        &config.schema,
        config.files.iter().collect::<BTreeSet<_>>(),
        &config.section,
        config.level,
        config.subsections,
        config.exclude.iter().collect::<BTreeSet<_>>(),
        config.archive.iter().collect::<BTreeSet<_>>(),
    ))
    .expect("binding json");
    format!("{:x}", Sha256::digest(bytes))
}

fn validate_schema(schema: &Schema) -> Result<(), Diagnostic> {
    let mut singles = BTreeSet::new();
    let mut labels = BTreeSet::new();
    for (name, f) in &schema.fields {
        if name == "id" {
            return Err(invalid_schema(
                "field name id is reserved for the item's stay",
            ));
        }
        let ok = match f.from {
            Source::Checkbox => f.kind == FieldType::Boolean,
            Source::Tag => {
                matches!(f.kind, FieldType::String | FieldType::Enum) && !f.tags.is_empty()
            }
            Source::Title | Source::Text => f.kind == FieldType::String,
            Source::TitleDate => f.kind == FieldType::Date,
            Source::Label => f
                .label
                .as_ref()
                .is_some_and(|l| !l.is_empty() && !l.contains(['*', '\n', ':'])),
        };
        if !ok {
            return Err(invalid_schema(format!(
                "field {name}: type and source do not fit"
            )));
        }
        if f.multiline && f.from != Source::Label {
            return Err(invalid_schema(format!(
                "field {name}: multiline is for labels; text already spans lines"
            )));
        }
        if (f.kind == FieldType::Enum) == f.values.is_empty() {
            return Err(invalid_schema(format!(
                "field {name}: enum needs values, others must not have them"
            )));
        }
        if f.from == Source::Label {
            if !labels.insert(f.label.clone().unwrap_or_default()) {
                return Err(invalid_schema(format!("field {name}: label used twice")));
            }
        } else if !singles.insert(format!("{:?}", f.from)) {
            return Err(invalid_schema(format!(
                "field {name}: source {:?} used twice",
                f.from
            )));
        }
        if let Some(d) = &f.default {
            check_value(f, d).map_err(|m| invalid_schema(format!("field {name} default: {m}")))?;
        }
    }
    if !singles.contains("Checkbox") {
        return Err(invalid_schema(
            "a checkbox collection needs a checkbox field",
        ));
    }
    Ok(())
}

/// Calendar-validated `YYYY-MM-DD`.
pub fn valid_date(s: &str) -> bool {
    let b = s.as_bytes();
    if b.len() != 10 || b[4] != b'-' || b[7] != b'-' {
        return false;
    }
    let num = |r: std::ops::Range<usize>| {
        let part = &s[r];
        part.bytes()
            .all(|c| c.is_ascii_digit())
            .then(|| part.parse::<u32>().ok())
            .flatten()
    };
    let (Some(y), Some(m), Some(d)) = (num(0..4), num(5..7), num(8..10)) else {
        return false;
    };
    let leap = y % 4 == 0 && (y % 100 != 0 || y % 400 == 0);
    let days = match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => return false,
    };
    (1..=days).contains(&d)
}

fn tag_matches(tags: &[String], value: &str) -> bool {
    tags.iter().any(|t| match t.strip_suffix('*') {
        Some(prefix) => value.starts_with(prefix) && value.len() > prefix.len(),
        None => t == value,
    })
}

pub(crate) fn check_value(f: &FieldSpec, v: &Value) -> Result<(), String> {
    match (f.kind, v) {
        (FieldType::String, Value::String(s))
            if f.from == Source::Tag && !tag_matches(&f.tags, s) =>
        {
            Err(format!("{s:?} matches none of {:?}", f.tags))
        }
        (FieldType::String, Value::String(_))
        | (FieldType::Boolean, Value::Bool(_))
        | (FieldType::Number, Value::Number(_)) => Ok(()),
        (FieldType::Date, Value::String(s)) if valid_date(s) => Ok(()),
        (FieldType::Enum, Value::String(s)) if f.values.contains(s) => Ok(()),
        (FieldType::Date, _) => Err("expected a calendar date YYYY-MM-DD".into()),
        (FieldType::Enum, _) => Err(format!("expected one of {:?}", f.values)),
        (kind, _) => Err(format!("expected {kind:?}").to_lowercase()),
    }
}

/// Field spans found in the canonical rendering `- ` + content.
#[derive(Debug, Default, Clone)]
struct Found {
    checked: Option<bool>,
    symbol: Option<Span>,
    paragraph: Option<Span>,
    tag: Option<(String, Span)>,
    title: Option<(String, Span)>,
    labels: BTreeMap<String, LabelAt>,
    /// The text before the first declared label line: the head's text.
    text: Option<(String, Span)>,
    /// Text after one-line labels, each run up to the next label line.
    tail: Vec<Tail>,
    /// Indentation the text's continuation lines render at: the common
    /// indentation extraction strips from them.
    indent: String,
    /// Indentation a new label line is placed at.
    label_indent: String,
}

impl Found {
    /// The text field: the head's text and every tail run, in order.
    fn joined_text(&self) -> String {
        let head = self.text.as_ref().map(|(t, _)| t.as_str()).unwrap_or("");
        std::iter::once(head)
            .chain(self.tail.iter().map(|t| t.value.as_str()))
            .filter(|t| !t.is_empty())
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// One declared label as it sits in the paragraph.
#[derive(Debug, Clone)]
struct LabelAt {
    value: String,
    /// The value's bytes, continuation lines included for a multiline label.
    span: Span,
    /// The label's lines, from the start of its own to the end of its last.
    line: Span,
    /// Indentation the value's continuation lines render at.
    indent: String,
}

/// A run of text lines after a one-line label.
#[derive(Debug, Clone)]
struct Tail {
    value: String,
    /// From its first character to its last.
    span: Span,
    /// The run with the line break in front of it, which removing it takes.
    whole: Span,
    indent: String,
}

/// Lines `span` covers, each with its common indentation removed and its
/// trailing whitespace trimmed: (value, first character, last character,
/// common indentation). None when the lines are all blank.
fn block_lines(text: &str, span: Span) -> Option<(String, usize, usize, usize)> {
    let raw = &text[span.start..span.end];
    if raw.trim().is_empty() {
        return None;
    }
    let depth = |l: &str| l.len() - l.trim_start_matches(' ').len();
    let common = raw
        .split('\n')
        .filter(|l| !l.trim().is_empty())
        .map(depth)
        .min()
        .unwrap_or(0);
    let value = raw
        .split('\n')
        .map(|l| l.get(common..).unwrap_or("").trim_end())
        .collect::<Vec<_>>()
        .join("\n")
        .trim_matches('\n')
        .to_string();
    let start = span.start + (raw.len() - raw.trim_start().len());
    let end = span.start + raw.trim_end().len();
    Some((value, start, end, common))
}

fn canonical(content: &str) -> String {
    let mut out = String::from("- ");
    for (n, line) in content.split('\n').enumerate() {
        if n > 0 {
            out.push('\n');
            if !line.is_empty() {
                out.push_str("  ");
            }
        }
        out.push_str(line);
    }
    out
}

fn decanonical(text: &str) -> String {
    let mut lines = text.split('\n');
    let first = lines.next().unwrap_or("").strip_prefix("- ").unwrap_or("");
    let mut out = String::from(first);
    for line in lines {
        out.push('\n');
        out.push_str(dedent(line, 2));
    }
    out
}

fn text_of<'a>(node: &'a AstNode<'a>) -> String {
    node.descendants()
        .filter_map(|n| match &n.data.borrow().value {
            NodeValue::Text(t) => Some(t.to_string()),
            NodeValue::Code(c) => Some(c.literal.clone()),
            _ => None,
        })
        .collect()
}

fn find(text: &str, schema: &Schema) -> Found {
    let arena = Arena::new();
    let options = crate::markdown::options(crate::markdown::ParseMode::Document);
    let root = parse_document(&arena, text, &options);
    let mut starts = vec![0];
    starts.extend(text.match_indices('\n').map(|(i, _)| i + 1));
    let at = |line: usize, col: usize| starts[line - 1] + col - 1;
    let span_of = |n: &'_ AstNode<'_>| {
        let p = n.data.borrow().sourcepos;
        Span {
            start: at(p.start.line, p.start.column),
            end: at(p.end.line, p.end.column) + 1,
        }
    };
    let mut found = Found::default();
    let Some(item) = root.first_child().and_then(|l| l.first_child()) else {
        return found;
    };
    if let NodeValue::TaskItem(task) = item.data.borrow().value {
        found.checked = Some(task.symbol.is_some());
        let s = task.symbol_sourcepos.start;
        let start = at(s.line, s.column);
        found.symbol = Some(Span {
            start,
            end: start + task.symbol.map_or(1, char::len_utf8),
        });
    } else {
        return found;
    }
    let Some(paragraph) = item
        .children()
        .find(|c| matches!(c.data.borrow().value, NodeValue::Paragraph))
    else {
        return found;
    };
    // An item whose first line carries no text (a label-only action, say) makes
    // comrak report the paragraph starting at the space after `]` rather than
    // after it, which would address the separator itself as content. Content
    // begins two bytes past the checkbox symbol, so never start before that.
    let para = span_of(paragraph);
    let para = Span {
        start: found
            .symbol
            .map_or(para.start, |s| para.start.max(s.end + 2)),
        end: para.end,
    };
    found.paragraph = Some(para);
    let spec = |source: Source| schema.fields.values().find(|f| f.from == source);
    let mut inline = paragraph.children().peekable();
    let mut cursor = para.start;
    // Consume head separators in one place, before and after an optional tag.
    // The following strong run then has the same leading position regardless
    // of how many authored spaces precede it.
    while let Some(first) = inline.peek().copied() {
        match &first.data.borrow().value {
            NodeValue::Text(t) if t.trim().is_empty() => {
                cursor = span_of(first).end;
                inline.next();
            }
            NodeValue::Code(code)
                if found.tag.is_none()
                    && spec(Source::Tag).is_some_and(|s| tag_matches(&s.tags, &code.literal))
                    && first.next_sibling().is_some_and(
                        |n| matches!(&n.data.borrow().value, NodeValue::Text(t) if t.starts_with(' ')),
                    ) =>
            {
                let span = span_of(first);
                found.tag = Some((code.literal.clone(), span));
                cursor = span.end + 1;
                inline.next();
            }
            _ => break,
        }
    }
    // Decide label ownership before claiming the leading strong run as a title.
    // Use the same predicate for discovery below, including the first line
    // after its checkbox and optional tag.
    let declared: BTreeSet<&str> = schema
        .fields
        .values()
        .filter(|f| f.from == Source::Label)
        .filter_map(|f| f.label.as_deref())
        .collect();
    let leading_start = cursor;
    let leading_inline = found
        .symbol
        .is_some_and(|s| !text[s.end..leading_start].contains('\n'));
    let is_label = |strong| {
        let start = span_of(strong).start;
        let line_start = starts[starts.partition_point(|x| *x <= start) - 1];
        declared.contains(text_of(strong).as_str())
            && strong.next_sibling().is_some_and(
                |n| matches!(&n.data.borrow().value, NodeValue::Text(t) if t.starts_with(':')),
            )
            && (start == leading_start || text[line_start..start].trim().is_empty())
    };
    if let Some(node) = inline.peek().copied()
        && matches!(node.data.borrow().value, NodeValue::Strong)
        && !is_label(node)
    {
        let outer = span_of(node);
        let inner = Span {
            start: outer.start + 2,
            end: outer.end - 2,
        };
        found.title = Some((text[inner.start..inner.end].to_string(), inner));
        cursor = outer.end;
    }
    // Only a label the schema declares is a label. Prose opens lines with a
    // bold lead-in (`**Progress 2026-09-14**: ...`) far more often than it
    // declares a field, and reading every one as a label would end the text at
    // the first of them: the text would come back short, and a text write
    // carrying one would re-read as something other than what was written.
    // Every declared label line, in order: (name, line start, value start,
    // end of the label's own line). A name seen twice is still a boundary;
    // only its first line is the field.
    let mut label_lines: Vec<(String, usize, usize, usize)> = vec![];
    for strong in paragraph
        .children()
        .filter(|n| matches!(n.data.borrow().value, NodeValue::Strong))
    {
        let name = text_of(strong);
        if !is_label(strong) {
            continue;
        }
        let s = span_of(strong);
        let physical_start = starts[starts.partition_point(|x| *x <= s.start) - 1];
        // An inline label owns its content, never the checkbox or tag.
        let line_start = if leading_inline && s.start == leading_start {
            leading_start
        } else {
            physical_start
        };
        let line_end = text[s.end..]
            .find('\n')
            .map_or(para.end, |i| s.end + i)
            .min(para.end);
        let rest_start = s.end + 1;
        let value_start = rest_start
            + (text[rest_start..line_end].len() - text[rest_start..line_end].trim_start().len());
        label_lines.push((name, line_start, value_start, line_end));
    }
    let first_label_line = label_lines.first().map(|l| l.1);
    let multiline: BTreeSet<&str> = schema
        .fields
        .values()
        .filter(|f| f.from == Source::Label && f.multiline)
        .filter_map(|f| f.label.as_deref())
        .collect();
    for (i, (name, line_start, value_start, line_end)) in label_lines.iter().enumerate() {
        let (line_start, value_start, line_end) = (*line_start, *value_start, *line_end);
        // What follows the label's own line, up to the line break in front
        // of the next label line or the end of the paragraph.
        let next = label_lines
            .get(i + 1)
            .map_or(para.end, |n| n.1.saturating_sub(1));
        let follow = Span {
            start: (line_end + 1).min(next.max(line_end)),
            end: next.max(line_end),
        };
        let on_line = text[value_start..line_end].trim_end();
        let label_indent = line_start..line_start + {
            let l = &text[line_start..line_end];
            l.len() - l.trim_start_matches(' ').len()
        };
        let continued = if multiline.contains(name.as_str()) && line_end < para.end {
            block_lines(text, follow)
        } else {
            None
        };
        let at = match continued {
            Some((more, _, end, common)) => LabelAt {
                value: if on_line.is_empty() {
                    more
                } else {
                    format!("{on_line}\n{more}")
                },
                span: Span {
                    start: value_start,
                    end,
                },
                line: Span {
                    start: line_start,
                    end: follow.end,
                },
                indent: " ".repeat(common),
            },
            None => {
                if line_end < para.end
                    && let Some((value, start, end, common)) = block_lines(text, follow)
                {
                    found.tail.push(Tail {
                        value,
                        span: Span { start, end },
                        whole: Span {
                            start: line_end,
                            end,
                        },
                        indent: " ".repeat(common),
                    });
                }
                LabelAt {
                    value: on_line.to_string(),
                    span: Span {
                        start: value_start,
                        end: value_start + on_line.len(),
                    },
                    line: Span {
                        start: line_start,
                        end: line_end,
                    },
                    indent: if leading_inline && line_start == leading_start {
                        " ".repeat(6)
                    } else {
                        text[label_indent].to_string()
                    },
                }
            }
        };
        found.labels.entry(name.clone()).or_insert(at);
    }
    let text_end = first_label_line.map_or(para.end, |l| l.saturating_sub(1).max(cursor));
    let raw = &text[cursor.min(text_end)..text_end];
    let mut lines = raw.trim().split('\n');
    let head = lines.next().unwrap_or("").trim().to_string();
    let rest: Vec<&str> = lines.collect();
    let depth = |l: &str| l.len() - l.trim_start_matches(' ').len();
    let continued = rest.iter().any(|l| !l.trim().is_empty());
    let common = rest
        .iter()
        .filter(|l| !l.trim().is_empty())
        .map(|l| depth(l))
        .min()
        .unwrap_or(0);
    // A head that starts a line of its own (an untitled item whose first
    // line holds only the checkbox) sets the depth when nothing follows it.
    // Measured from the file text, not the span: comrak can start the
    // paragraph after that line break, so the span need not contain it.
    let head_at = cursor.min(text_end) + raw.len() - raw.trim_start().len();
    let head_depth = text[..head_at].rfind('\n').and_then(|nl| {
        let below_checkbox = found.symbol.is_some_and(|s| nl >= s.end);
        let gap = &text[nl + 1..head_at];
        (below_checkbox && !head.is_empty() && gap.bytes().all(|b| b == b' ')).then_some(gap.len())
    });
    let mut value = head;
    for l in rest {
        value.push('\n');
        value.push_str(l.get(common..).unwrap_or("").trim_end());
    }
    found.text = Some((
        value,
        Span {
            start: cursor.min(text_end),
            end: text_end,
        },
    ));
    // Text and labels are indented separately. Extraction strips exactly the
    // text's common indentation, so text renders at exactly that: anything
    // else moves every continuation line on a write that re-renders the text.
    // A new label goes where the item's labels already are. Each falls back
    // on the other, then on the six spaces an inserted item uses.
    let label_depth = found
        .labels
        .values()
        .filter(|l| {
            let start = text[..l.line.start].rfind('\n').map_or(0, |i| i + 1);
            text[start..l.line.start].trim().is_empty()
        })
        .map(|l| depth(&text[l.line.start..l.line.end]))
        .min();
    let text_depth = if continued { Some(common) } else { head_depth };
    found.indent = " ".repeat(text_depth.or(label_depth).unwrap_or(6));
    found.label_indent = " ".repeat(label_depth.or(text_depth).unwrap_or(6));
    found
}

/// A requested title that ends in `(YYYY-MM-DD)` would re-read with that date
/// as the date field and the title without it, so it is refused by name rather
/// than written and found not to read back.
fn title_with_date(schema: &Schema, title: &str) -> Result<(), Diagnostic> {
    let dated = schema
        .fields
        .iter()
        .find(|(_, f)| f.from == Source::TitleDate);
    if let (Some((name, _)), (_, Some((date, _)))) = (dated, title_split(title)) {
        return Err(Diagnostic::new(
            ErrorCode::InvalidItem,
            format!(
                "title {title:?} ends in ({date}), which reads back as the `{name}` field \
                 and not as part of the title; pass `{name}: {date}` with the title before it"
            ),
        ));
    }
    Ok(())
}

/// The fields that differ between what a write asked for and what its result
/// reads back as, by name, for a refusal that has to say which.
fn field_changes(got: &Map<String, Value>, expect: &Map<String, Value>) -> String {
    let names: BTreeSet<&String> = got.keys().chain(expect.keys()).collect();
    names
        .into_iter()
        .filter(|n| got.get(*n) != expect.get(*n))
        .map(|n| match (expect.get(n), got.get(n)) {
            (Some(e), Some(g)) => format!("{n} re-reads as {g} rather than {e}"),
            (Some(e), None) => format!("{n} does not re-read at all (expected {e})"),
            (None, Some(g)) => format!("{n} re-reads as {g}, which was not asked for"),
            (None, None) => unreachable!("a name comes from one of the maps"),
        })
        .collect::<Vec<_>>()
        .join("; ")
}

fn title_split(title: &str) -> (String, Option<(String, usize)>) {
    // `Title text (YYYY-MM-DD)` -> ("Title text", date at byte offset).
    if let Some(open) = title.rfind(" (") {
        let tail = &title[open + 2..];
        if let Some(date) = tail.strip_suffix(')')
            && valid_date(date)
        {
            return (
                title[..open].to_string(),
                Some((date.to_string(), open + 2)),
            );
        }
    }
    (title.to_string(), None)
}

/// Authored field inputs before defaults and validation, using the shared discovery rules.
pub fn discover(
    content: &str,
    schema: &Schema,
) -> Result<BTreeMap<String, Option<Value>>, Vec<String>> {
    let found = find(&canonical(content), schema);
    let Some(checked) = found.checked else {
        return Err(vec!["not a checkbox item".into()]);
    };
    let (title, date) = found
        .title
        .as_ref()
        .map(|(t, _)| title_split(t))
        .map_or((None, None), |(t, d)| (Some(t), d));
    let has_date_field = schema.fields.values().any(|f| f.from == Source::TitleDate);
    let mut out = BTreeMap::new();
    for (name, f) in &schema.fields {
        let raw: Option<Value> = match f.from {
            Source::Checkbox => Some(Value::Bool(checked)),
            Source::Tag => found.tag.as_ref().map(|(t, _)| Value::String(t.clone())),
            Source::Title => {
                let t = if has_date_field {
                    title.clone()
                } else {
                    found.title.as_ref().map(|(t, _)| t.clone())
                };
                t.filter(|t| !t.is_empty()).map(Value::String)
            }
            Source::TitleDate => date.as_ref().map(|(d, _)| Value::String(d.clone())),
            Source::Label => found
                .labels
                .get(f.label.as_deref().unwrap_or(""))
                .map(|l| label_value(f.kind, &l.value)),
            Source::Text => Some(found.joined_text())
                .filter(|t| !t.is_empty())
                .map(Value::String),
        };
        out.insert(name.clone(), raw);
    }
    Ok(out)
}

/// Strict extraction remains all-or-nothing, including its original errors.
pub fn extract(content: &str, schema: &Schema) -> Result<Map<String, Value>, Vec<String>> {
    let raw = discover(content, schema)?;
    let mut out = Map::new();
    let mut errors = vec![];
    for (name, f) in &schema.fields {
        match raw[name].clone().or_else(|| f.default.clone()) {
            Some(v) => match check_value(f, &v) {
                Ok(()) => {
                    out.insert(name.clone(), v);
                }
                Err(m) => errors.push(format!("{name}: {m}")),
            },
            None if f.required => errors.push(format!("{name}: required field missing")),
            None => {}
        }
    }
    if errors.is_empty() {
        Ok(out)
    } else {
        Err(errors)
    }
}

fn label_value(kind: FieldType, raw: &str) -> Value {
    match kind {
        FieldType::Number => serde_json::from_str::<Value>(raw)
            .ok()
            .filter(Value::is_number)
            .unwrap_or_else(|| Value::String(raw.to_string())),
        FieldType::Boolean => match raw {
            "true" => Value::Bool(true),
            "false" => Value::Bool(false),
            _ => Value::String(raw.to_string()),
        },
        _ => Value::String(raw.to_string()),
    }
}

fn scalar_text(f: &FieldSpec, v: &Value) -> Result<String, Diagnostic> {
    let bad = |m: &str| Diagnostic::new(ErrorCode::InvalidItem, m.to_string());
    let s = match v {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        _ => return Err(bad("field values are scalars")),
    };
    if s.contains(['\n', '\r']) && f.from != Source::Text && !f.multiline {
        return Err(bad("only text fields and multiline labels may span lines"));
    }
    if s != s.trim() {
        return Err(bad("field values must not have surrounding whitespace"));
    }
    // A multiline label's value is prose like text is, and the re-read check
    // refuses one whose lines would read back as something else.
    if (f.from == Source::Title || (f.from == Source::Label && !f.multiline)) && s.contains("**") {
        return Err(bad("titles and one-line labels must not contain **"));
    }
    if f.from == Source::Tag && s.contains('`') {
        return Err(bad("tags must not contain backticks"));
    }
    Ok(s)
}

/// A field value laid out across the lines it spans, continuations indented the
/// way the item already indents them.
fn indented(value: &str, indent: &str) -> String {
    let mut out = String::new();
    for (n, line) in value.split('\n').enumerate() {
        if n > 0 {
            out.push('\n');
            if !line.is_empty() {
                out.push_str(indent);
            }
        }
        out.push_str(line);
    }
    out
}

/// A tag needs another inline node after its separating space to be a tag.
fn check_tag_context(tag: bool, following: bool) -> Result<(), Diagnostic> {
    if tag && !following {
        return Err(Diagnostic::new(
            ErrorCode::InvalidItem,
            "a tag needs a title, text or label on the same line to read back as one",
        ));
    }
    Ok(())
}

/// A head part owns its spelling and the separator preceding it. Source spans
/// stay in order even when a neighboring part is inserted or removed.
struct HeadPart {
    source: Option<Span>,
    separator: Option<Span>,
    value: HeadBytes,
}

enum HeadBytes {
    Source(Span),
    Rendered(String),
}

/// The ordered tag/title parts, followed by the gap before the item's text.
struct ItemHead {
    span: Span,
    parts: Vec<HeadPart>,
    gap: Span,
}

impl ItemHead {
    fn pieces(&self, paragraph_start: usize, changed: bool) -> Vec<HeadBytes> {
        let mut pieces = Vec::new();
        for (i, part) in self.parts.iter().enumerate() {
            let previous_end = if i == 0 {
                Some(self.span.start)
            } else {
                self.parts[i - 1].source.map(|s| s.end)
            };
            match part.separator {
                Some(s) if Some(s.start) == previous_end && (i > 0 || !changed) => {
                    pieces.push(HeadBytes::Source(s));
                }
                _ if i > 0 || self.span.start != paragraph_start => {
                    pieces.push(HeadBytes::Rendered(" ".into()));
                }
                _ => {}
            }
            pieces.push(match &part.value {
                HeadBytes::Source(s) => HeadBytes::Source(*s),
                HeadBytes::Rendered(s) => HeadBytes::Rendered(s.clone()),
            });
        }
        pieces
    }

    /// Only gaps between retained source ranges become splices. An untouched
    /// part is never rendered, even when its neighbors change.
    fn plan(&self, text: &str, pieces: Vec<HeadBytes>, splices: &mut Vec<Splice>) {
        let mut at = self.span.start;
        let mut replacement = String::new();
        for piece in pieces.into_iter().chain([HeadBytes::Source(Span {
            start: self.span.end,
            end: self.span.end,
        })]) {
            match piece {
                HeadBytes::Rendered(s) => replacement.push_str(&s),
                HeadBytes::Source(s) => {
                    if text[at..s.start] != replacement {
                        splices.push(Splice {
                            span: Span {
                                start: at,
                                end: s.start,
                            },
                            text: std::mem::take(&mut replacement),
                        });
                    } else {
                        replacement.clear();
                    }
                    at = s.end;
                }
            }
        }
    }
}

/// Apply requested field values to canonical text.
///
/// The tag, title and embedded date are resolved together as ordered head
/// parts. Splices cover only the gaps between retained source ranges, so
/// untouched parts keep their exact bytes. Text and labels own separate spans.
fn edit_fields(
    text: &str,
    schema: &Schema,
    set: &BTreeMap<String, Value>,
) -> Result<String, Diagnostic> {
    let bad = |m: String| Diagnostic::new(ErrorCode::InvalidItem, m);
    let found = find(text, schema);
    let mut splices: Vec<Splice> = vec![];
    let para = found
        .paragraph
        .ok_or_else(|| bad("item has no paragraph to edit".into()))?;
    let field = |source: Source| schema.fields.iter().find(|(_, f)| f.from == source);
    let requested = |source: Source| field(source).and_then(|(n, _)| set.get(n));

    // Validate every requested value once, in one place, before anything is
    // planned. A field that is required cannot be removed.
    for (name, value) in set {
        let f = schema
            .fields
            .get(name)
            .ok_or_else(|| bad(format!("{name} is not a field of {}", schema.id)))?;
        if !value.is_null() {
            check_value(f, value).map_err(|m| bad(format!("{name}: {m}")))?;
        } else if f.required {
            return Err(bad(format!("{name} is required")));
        }
    }

    // The date lives inside the title, so read them as one and resolve both
    // against the request before deciding what the line should say.
    let dated = schema.fields.values().any(|f| f.from == Source::TitleDate);
    let (was_title, was_date) = match &found.title {
        Some((t, _)) if dated => {
            let (title, date) = title_split(t);
            (Some(title), date.map(|(d, _)| d))
        }
        Some((t, _)) => (Some(t.clone()), None),
        None => (None, None),
    };
    let resolve = |source: Source, current: Option<String>| -> Result<Option<String>, Diagnostic> {
        match (requested(source), field(source)) {
            (Some(Value::Null), _) => Ok(None),
            // Extraction exposes the title's original Markdown, including wraps.
            // Keeping that value is not a request to render new title syntax.
            (Some(Value::String(v)), _)
                if source == Source::Title && current.as_ref() == Some(v) =>
            {
                Ok(current)
            }
            (Some(v), Some((_, f))) => Ok(Some(scalar_text(f, v)?)),
            _ => Ok(current),
        }
    };
    let title = resolve(Source::Title, was_title.clone())?;
    if let (Some(t), Some(v)) = (&title, requested(Source::Title))
        && !v.is_null()
    {
        title_with_date(schema, t)?;
    }
    // A title on its way out takes the date inside it, whether or not the
    // request mentioned the date.
    let date = match &title {
        Some(_) => resolve(Source::TitleDate, was_date.clone())?,
        None => None,
    };
    if title.as_ref().is_some_and(String::is_empty) {
        return Err(bad("title must not be empty".into()));
    }
    if title.is_none() && requested(Source::TitleDate).is_some_and(|v| !v.is_null()) {
        return Err(bad("a date lives in the title; set the title first".into()));
    }
    let tag = match (requested(Source::Tag), field(Source::Tag)) {
        (Some(Value::Null), _) => None,
        (Some(v), Some((_, f))) if f.default.as_ref() == Some(v) => {
            let _ = scalar_text(f, v)?;
            None
        }
        (Some(v), Some((_, f))) => Some(scalar_text(f, v)?),
        _ => found.tag.as_ref().map(|(t, _)| t.clone()),
    };

    // The head runs from just past `]` to the text's first real character.
    // Its parts retain their source spans: a title written `__T__` stays
    // `__T__`, and a tag keeps whatever backticks spelled it.
    // Normally the head reaches back to just past `]`. It must not when
    // something else of the item's own lives in between: an item can open with
    // a nested list and carry its paragraph after it, and that list is not the
    // head's to rewrite.
    let head_start = match found.symbol.map(|s| s.end + 1) {
        Some(after) if after <= para.start && text[after..para.start].trim().is_empty() => after,
        _ => para.start,
    };
    let (text_value, text_span) = found
        .text
        .clone()
        .ok_or_else(|| bad("item has no text position".into()))?;
    // Every kind of whitespace, the line break included: text on a
    // continuation line is reached by crossing one, and the bytes crossed are
    // kept as the gap rather than overwritten by the text that follows them.
    let mut content_start = text_span.start;
    while content_start < text_span.end
        && matches!(
            text.as_bytes().get(content_start),
            Some(b' ' | b'\t' | b'\n' | b'\r')
        )
    {
        content_start += 1;
    }
    let content_start = content_start.max(head_start);
    // Whatever separates the last head element from the text, verbatim. Two
    // trailing spaces are a hard line break and a newline puts the text on its
    // own line, so this is never rebuilt from a guess about what it meant.
    let gap_start = match (&found.title, &found.tag) {
        (Some((_, s)), _) => s.end + 2,
        (None, Some((_, s))) => s.end,
        (None, None) => head_start,
    }
    .min(content_start);
    let gap = &text[gap_start..content_start];
    let had_text = !text_value.is_empty();
    // What the request asks the text field to hold, when that is not what it
    // already holds: the head's text and every tail run, read as one value.
    let joined = found.joined_text();
    let asked = match requested(Source::Text) {
        Some(Value::Null) => Some(String::new()),
        Some(v) => Some(scalar_text(
            field(Source::Text)
                .expect("a text request names a text field")
                .1,
            v,
        )?),
        None => None,
    }
    .filter(|t| *t != joined);
    // The text is written where it starts: in the head, unless the head holds
    // none and a run after a one-line label does. Either way every other run
    // goes, so the item re-reads as exactly the value written.
    let to_tail = asked.is_some() && !had_text && !found.tail.is_empty();
    // The head's own text, as the head edit below lays it out.
    let body = match &asked {
        Some(t) if !to_tail => t.clone(),
        _ => text_value.clone(),
    };
    // A field can be *named* in the request without actually moving: setting
    // an actor to the default it already reads as, or a title to the text it
    // already holds, resolves to what was already there. Rebuilding the head
    // for a request that changes nothing would rewrite spacing no field
    // named, so this is a real change, not merely a mention.
    let rebuild =
        tag != found.tag.as_ref().map(|(t, _)| t.clone()) || title != was_title || date != was_date;
    // A tag with nothing beside it on the line reads back as an ordinary code
    // span, not a tag, the same rule `render_new` applies on insert.
    let inline_label = schema.fields.iter().any(|(name, f)| {
        f.from == Source::Label
            && set.get(name).is_none_or(|v| !v.is_null())
            && found
                .labels
                .get(f.label.as_deref().unwrap_or(""))
                .is_some_and(|at| !text[head_start..at.line.start].contains('\n'))
    });
    check_tag_context(
        tag.is_some(),
        title.is_some() || !body.is_empty() || inline_label,
    )?;
    // Text with no span of its own has nowhere to be written, so the head
    // carries it. That also keeps it off `para.end`, which a new label claims.
    let carry = !body.is_empty() && text_span.start == text_span.end;
    // The end of the head's span, so no other edit reaches back into it.
    let mut head_taken = 0usize;

    if rebuild || carry {
        let mut head = ItemHead {
            span: Span {
                start: head_start,
                end: content_start,
            },
            parts: Vec::new(),
            gap: Span {
                start: gap_start,
                end: content_start,
            },
        };
        let tag_span = found.tag.as_ref().map(|(_, s)| *s);
        let title_span = found.title.as_ref().map(|(_, s)| Span {
            start: s.start - 2,
            end: s.end + 2,
        });
        if let Some(t) = &tag {
            head.parts.push(HeadPart {
                source: tag_span,
                separator: tag_span.map(|s| Span {
                    start: head_start,
                    end: s.start,
                }),
                value: match &found.tag {
                    Some((old, s)) if t == old => HeadBytes::Source(*s),
                    _ => HeadBytes::Rendered(format!("`{t}`")),
                },
            });
        }
        if let Some(t) = &title {
            head.parts.push(HeadPart {
                source: title_span,
                separator: title_span.map(|s| Span {
                    start: tag_span.map_or(head_start, |tag| tag.end),
                    end: s.start,
                }),
                value: match &found.title {
                    Some(_) if title == was_title && date == was_date => {
                        HeadBytes::Source(title_span.expect("existing title span"))
                    }
                    existing => {
                        let (open, close) = match existing {
                            Some((_, s)) => (&text[s.start - 2..s.start], &text[s.end..s.end + 2]),
                            None => ("**", "**"),
                        };
                        HeadBytes::Rendered(match &date {
                            Some(d) => format!("{open}{t} ({d}){close}"),
                            None => format!("{open}{t}{close}"),
                        })
                    }
                },
            });
        }
        let mut pieces = head.pieces(para.start, rebuild);
        if carry {
            // Inline gaps precede new text. A structural line break follows it.
            if gap.contains('\n') {
                pieces.push(HeadBytes::Rendered(format!(
                    " {}",
                    indented(&body, &found.indent)
                )));
                pieces.push(HeadBytes::Source(head.gap));
            } else {
                pieces.push(if gap.is_empty() {
                    HeadBytes::Rendered(" ".into())
                } else {
                    HeadBytes::Source(head.gap)
                });
                pieces.push(HeadBytes::Rendered(indented(&body, &found.indent)));
            }
        } else if !body.is_empty() || !had_text {
            if gap_start > head_start || gap.contains('\n') {
                pieces.push(HeadBytes::Source(head.gap));
            } else if !body.is_empty() {
                pieces.push(HeadBytes::Rendered(" ".into()));
            }
        }
        // Content on the checkbox's line needs a separator even after removal
        // of its last part.
        if !body.is_empty()
            && pieces.iter().all(|p| match p {
                HeadBytes::Source(s) => s.start == s.end,
                HeadBytes::Rendered(s) => s.is_empty(),
            })
        {
            pieces.push(HeadBytes::Rendered(" ".into()));
        }
        if inline_label {
            if title.is_some() || !body.is_empty() {
                pieces.push(HeadBytes::Rendered(format!("\n{}", found.label_indent)));
            } else if tag.is_some() && (gap.is_empty() || gap_start == head_start) {
                pieces.push(HeadBytes::Rendered(" ".into()));
            }
        }
        head_taken = content_start;
        head.plan(text, pieces, &mut splices);
    }

    // Missing labels all land at the paragraph's end. Collect their lines in
    // field-name order as one insertion; separate insertions at that point
    // would correctly look like overlapping splices to the general checker.
    let mut new_labels = String::new();
    for (name, value) in set {
        let f = &schema.fields[name];
        match f.from {
            // Resolved into the head above.
            Source::Tag | Source::Title | Source::TitleDate => {}
            Source::Checkbox if value.is_null() => {
                return Err(bad(format!("{name}: a checkbox cannot be removed")));
            }
            Source::Checkbox => {
                let symbol = found.symbol.expect("checkbox items have a symbol");
                splices.push(Splice {
                    span: symbol,
                    text: if value == &Value::Bool(true) {
                        "x"
                    } else {
                        " "
                    }
                    .into(),
                });
            }
            Source::Label => {
                let label = f.label.clone().unwrap_or_default();
                match (found.labels.get(&label), value.is_null()) {
                    // The value already there, which is not a change: a multiline
                    // value re-rendered would drop what extraction trims from its
                    // lines, a hard break's two trailing spaces among them.
                    (Some(at), false) if scalar_text(f, value)? == at.value => {}
                    (Some(at), false) => splices.push(Splice {
                        span: at.span,
                        text: indented(&scalar_text(f, value)?, &at.indent),
                    }),
                    (Some(LabelAt { line, .. }), true) => splices.push(Splice {
                        // The newline in front of the label goes with it, but
                        // on an item whose first line is empty that newline is
                        // the head's, and taking it here would collide.
                        span: Span {
                            start: line.start.saturating_sub(1).max(head_taken),
                            end: line.end,
                        },
                        text: String::new(),
                    }),
                    (None, false) => new_labels.push_str(&format!(
                        "\n{}**{label}**: {}",
                        found.label_indent,
                        indented(&scalar_text(f, value)?, &found.label_indent)
                    )),
                    (None, true) => {}
                }
            }
            // Carried into the head, which had nowhere else to put it.
            // Requesting the value already there, byte-extracted differences
            // included, is not a change: the source may hold whitespace
            // (a non-breaking space, say) that extraction trims but the
            // head/gap byte-walk below does not recognize as skippable, and
            // that mismatch is exactly what would otherwise let a genuine
            // no-op reach into and delete it.
            Source::Text if carry || asked.is_none() => {}
            Source::Text if to_tail => {
                let (first, rest) = found.tail.split_first().expect("to_tail has a run");
                let new = asked.as_deref().unwrap_or_default();
                splices.push(match new.is_empty() {
                    true => Splice {
                        span: first.whole,
                        text: String::new(),
                    },
                    false => Splice {
                        span: first.span,
                        text: indented(new, &first.indent),
                    },
                });
                splices.extend(rest.iter().map(|t| Splice {
                    span: t.whole,
                    text: String::new(),
                }));
            }
            Source::Text if body == text_value => {
                splices.extend(found.tail.iter().map(|t| Splice {
                    span: t.whole,
                    text: String::new(),
                }));
            }
            Source::Text => {
                splices.extend(found.tail.iter().map(|t| Splice {
                    span: t.whole,
                    text: String::new(),
                }));
                // The separator in front of the text belongs to the head or to
                // the source, never to this splice, so it starts at the first
                // real character and leaves everything before it alone. That
                // rule only holds while real text is actually being removed
                // with no head rebuild in flight: the arm above has already
                // ruled out a no-op reaching here, so `body.is_empty()` now
                // means a genuine removal. When text does leave, this splice
                // takes the whole run back to `gap_start`, the point right
                // after the title, the tag, or `]` itself, or its leading
                // line break is left dangling in front of whatever comes next
                // (a label landing just past `para.end` would otherwise fall
                // outside this item's own paragraph).
                let start = if body.is_empty() && !rebuild {
                    gap_start
                } else {
                    content_start
                };
                let mut replacement = indented(&body, &found.indent);
                // A title followed immediately by punctuation is valid, but
                // replacing that punctuation with a word would join the bold
                // delimiters to the word and stop the title reading as bold.
                // The requested text owns that punctuation; give its new value
                // a separator without respelling the untouched title or gap.
                if !body.is_empty() && title.is_some() && gap.is_empty() {
                    replacement.insert(0, ' ');
                }
                splices.push(Splice {
                    span: Span {
                        start,
                        end: text_span.end.max(content_start),
                    },
                    text: replacement,
                });
            }
        }
    }
    if !new_labels.is_empty() {
        splices.push(Splice {
            span: Span {
                start: para.end,
                end: para.end,
            },
            text: new_labels,
        });
    }
    splices.sort_by_key(|s| (s.span.start, s.span.end));
    for pair in splices.windows(2) {
        if pair[0].span.overlaps(pair[1].span) || pair[0].span.end > pair[1].span.start {
            return Err(bad(
                "requested fields overlap in the source; set them separately".into(),
            ));
        }
    }
    let mut out = String::new();
    let mut at = 0;
    for s in &splices {
        out.push_str(&text[at..s.span.start]);
        out.push_str(&s.text);
        at = s.span.end;
    }
    out.push_str(&text[at..]);
    Ok(out)
}

/// Canonical content for a new item built from typed fields.
fn render_new(schema: &Schema, fields: &BTreeMap<String, Value>) -> Result<String, Diagnostic> {
    let bad = |m: String| Diagnostic::new(ErrorCode::InvalidItem, m);
    for name in fields.keys() {
        if !schema.fields.contains_key(name) {
            return Err(bad(format!("{name} is not a field of {}", schema.id)));
        }
    }
    let get = |source: Source| {
        schema
            .fields
            .iter()
            .find(|(_, f)| f.from == source)
            .and_then(|(n, f)| fields.get(n).filter(|v| !v.is_null()).map(|v| (f, v)))
    };
    let mut first = String::new();
    let done = get(Source::Checkbox).is_some_and(|(_, v)| v == &Value::Bool(true));
    first.push_str(if done { "[x] " } else { "[ ] " });
    if let Some((f, v)) = get(Source::Tag)
        && f.default.as_ref() != Some(v)
    {
        // A tag is only a tag when text follows it on the line; alone it reads
        // back as an ordinary code span and the actor is lost. Say that here
        // rather than letting the re-read comparison refuse it as a mismatch.
        check_tag_context(
            true,
            get(Source::Title).is_some() || get(Source::Text).is_some(),
        )?;
        first.push_str(&format!("`{}` ", scalar_text(f, v)?));
    }
    let title = get(Source::Title)
        .map(|(f, v)| scalar_text(f, v))
        .transpose()?;
    if let Some(t) = &title {
        title_with_date(schema, t)?;
    }
    let date = get(Source::TitleDate)
        .map(|(f, v)| scalar_text(f, v))
        .transpose()?;
    match (title, date) {
        (Some(t), Some(d)) => first.push_str(&format!("**{t} ({d})**")),
        (Some(t), None) => first.push_str(&format!("**{t}**")),
        (None, Some(_)) => return Err(bad("a date needs a title".into())),
        (None, None) => {}
    }
    if let Some((f, v)) = get(Source::Text) {
        let t = scalar_text(f, v)?;
        let mut lines = t.split('\n');
        // The same separator rule the update path uses: a `**title**` ends
        // flush against the text, while the checkbox and a tag already leave
        // the space behind them.
        if !first.ends_with(' ') {
            first.push(' ');
        }
        first.push_str(lines.next().unwrap_or(""));
        for line in lines {
            first.push('\n');
            if !line.is_empty() {
                first.push_str("    ");
            }
            first.push_str(line);
        }
    }
    for (name, f) in schema
        .fields
        .iter()
        .filter(|(_, f)| f.from == Source::Label)
    {
        if let Some(v) = fields.get(name).filter(|v| !v.is_null()) {
            let label = f.label.as_deref().unwrap_or("");
            let value = indented(&scalar_text(f, v)?, "    ");
            first.push_str(&format!("\n    **{label}**: {value}"));
        }
    }
    Ok(first.trim_end().to_string())
}

/// One collection instance inside a document.
pub struct Instance {
    /// The section the binding names.
    pub section: usize,
    /// Member sections in document order: the bound section, then, when the
    /// binding takes subsections, every descendant outside an excluded subtree.
    pub members: Vec<usize>,
    /// (list, item) pairs of checkbox items in the members' own bodies.
    pub items: Vec<(usize, usize)>,
}

/// A section's own body: from under its heading to its first child section.
fn own_body(doc: &Document, section: usize) -> Span {
    let s = &doc.sections[section];
    Span {
        start: s.body.start,
        end: s.own_end,
    }
}

impl Instance {
    pub(crate) fn named_member(
        &self,
        doc: &Document,
        title: &str,
    ) -> Result<Option<usize>, Diagnostic> {
        let named: Vec<_> = self
            .members
            .iter()
            .copied()
            .filter(|&m| doc.sections[m].path.last().is_some_and(|t| t == title))
            .collect();
        match named.as_slice() {
            [] => Ok(None),
            [one] => Ok(Some(*one)),
            _ => Err(Diagnostic::new(
                ErrorCode::AmbiguousSelector,
                format!("{} member sections are titled {title:?}", named.len()),
            )),
        }
    }

    pub(crate) fn member_lists(&self, doc: &Document, member: usize) -> BTreeSet<usize> {
        self.items
            .iter()
            .map(|(l, _)| *l)
            .filter(|&l| self.member_of(doc, l) == Some(member))
            .collect()
    }

    /// The member section whose own body holds a list.
    fn member_of(&self, doc: &Document, list: usize) -> Option<usize> {
        let at = doc.blocks[doc.lists[list].block].span.start;
        self.members
            .iter()
            .copied()
            .find(|&m| own_body(doc, m).contains(at))
    }

    /// Titles from below the bound section down to the member holding a
    /// list; empty for the bound section's own body.
    pub fn subsection(&self, doc: &Document, list: usize) -> Vec<String> {
        let depth = doc.sections[self.section].path.len();
        self.member_of(doc, list)
            .map(|m| doc.sections[m].path[depth..].to_vec())
            .unwrap_or_default()
    }

    /// The members' own bodies, the scope a collection stamp covers.
    pub fn spans(&self, doc: &Document) -> Vec<Span> {
        self.members.iter().map(|&m| own_body(doc, m)).collect()
    }
}

pub fn instance(doc: &Document, bound: &Bound) -> Result<Option<Instance>, Diagnostic> {
    let matches: Vec<usize> = doc
        .sections
        .iter()
        .enumerate()
        .filter(|(_, s)| {
            doc.blocks[s.block]
                .heading
                .as_ref()
                .is_some_and(|h| h.title_text == bound.config.section)
                && bound.config.level.is_none_or(|l| l == s.level)
        })
        .map(|(i, _)| i)
        .collect();
    let section = match matches.as_slice() {
        [] => return Ok(None),
        [one] => *one,
        _ => {
            return Err(Diagnostic::new(
                ErrorCode::AmbiguousSelector,
                format!(
                    "{} sections are titled {:?}",
                    matches.len(),
                    bound.config.section
                ),
            ));
        }
    };
    let mut members = vec![section];
    if bound.config.subsections {
        // Sections are in document order, so the descendants follow the
        // bound section directly, each after its parent.
        let whole = doc.sections[section].whole;
        let mut dropped = BTreeSet::new();
        for (d, s) in doc
            .sections
            .iter()
            .enumerate()
            .skip(section + 1)
            .take_while(|(_, s)| whole.contains(s.whole.start))
        {
            let title = s.path.last().map(String::as_str).unwrap_or("");
            if s.parent.is_some_and(|p| dropped.contains(&p))
                || bound.config.exclude.iter().any(|x| x == title)
            {
                dropped.insert(d);
            } else {
                members.push(d);
            }
        }
    }
    let spans: Vec<Span> = members.iter().map(|&m| own_body(doc, m)).collect();
    let items = doc
        .lists
        .iter()
        .enumerate()
        .filter(|(_, l)| {
            let at = doc.blocks[l.block].span.start;
            spans.iter().any(|s| s.contains(at))
        })
        .flat_map(|(li, l)| {
            l.items
                .iter()
                .enumerate()
                .filter(|(_, it)| it.checkbox.is_some())
                .map(move |(ii, _)| (li, ii))
        })
        .collect();
    Ok(Some(Instance {
        section,
        members,
        items,
    }))
}

pub fn item_revision(doc: &Document, bound: &Bound, list: usize, item: usize) -> String {
    let it = &doc.lists[list].items[item];
    let id = doc
        .stay_ids(&it.markers)
        .first()
        .cloned()
        .unwrap_or_default();
    revision(
        "action",
        &id,
        &doc.source.as_bytes()[it.span.start..it.span.end],
        &[
            bound.name.as_bytes(),
            bound.binding.as_bytes(),
            bound.digest.as_bytes(),
        ],
    )
}

/// Covers each member's heading and own body, excluding nonmember content.
pub fn collection_revision(doc: &Document, bound: &Bound, inst: &Instance) -> String {
    let bytes = doc.source.as_bytes();
    let mut framed = vec![];
    let content = if !bound.config.subsections {
        let s = &doc.sections[inst.section];
        &bytes[s.whole.start..s.own_end]
    } else {
        for &m in &inst.members {
            let s = &doc.sections[m];
            framed.extend_from_slice(&((s.own_end - s.whole.start) as u64).to_be_bytes());
            framed.extend_from_slice(&bytes[s.whole.start..s.own_end]);
        }
        &framed
    };
    revision(
        "collection",
        &bound.name,
        content,
        &[bound.binding.as_bytes(), bound.digest.as_bytes()],
    )
}

/// How a typed write names its item: by a stay id, or by a position handle
/// whose item is found again in the candidate by its mapped extent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ItemLocator {
    Id(String),
    Position(crate::Selector),
}

impl ItemLocator {
    pub(crate) fn selector(&self) -> crate::Selector {
        match self {
            ItemLocator::Id(id) => crate::Selector::Id { id: id.clone() },
            ItemLocator::Position(select) => select.clone(),
        }
    }

    pub(crate) fn is_position(&self) -> bool {
        matches!(self, ItemLocator::Position(_))
    }

    fn describe(&self) -> String {
        match self {
            ItemLocator::Id(id) => id.clone(),
            ItemLocator::Position(_) => "the item at this position".to_string(),
        }
    }
}

/// Typed readback is data, with the original update/insert comparison rules.
pub(crate) struct FieldExpectation {
    pub bound: Bound,
    pub locator: ItemLocator,
    /// For a position locator: where the written item lands in the candidate.
    pub region: Option<crate::edit::ItemRegion>,
    pub expect: Map<String, Value>,
    pub insert: bool,
}

impl FieldExpectation {
    pub(crate) fn binding_matches(&self, bound: &Bound) -> bool {
        (
            self.bound.name.as_str(),
            self.bound.binding.as_str(),
            self.bound.digest.as_str(),
        ) == (
            bound.name.as_str(),
            bound.binding.as_str(),
            bound.digest.as_str(),
        )
    }

    pub(crate) fn check(self, c: &Document, map: &crate::edit::Mapper) -> Result<(), Diagnostic> {
        let Self {
            bound,
            locator,
            region,
            expect,
            insert,
        } = self;
        let schema = bound.schema;
        if insert {
            let Ok(Target::Item { list, item }) = c.resolve(&locator.selector()) else {
                return Err(Diagnostic::new(
                    ErrorCode::InvalidItem,
                    "new item did not attach",
                ));
            };
            let got = extract(&c.item_content(list, item), &schema)
                .map_err(|e| Diagnostic::new(ErrorCode::InvalidItem, e.join("; ")))?;
            if got != expect {
                return Err(Diagnostic::new(
                    ErrorCode::InvalidItem,
                    format!(
                        "new item would not read back as asked: {}",
                        field_changes(&got, &expect)
                    ),
                ));
            }
            Ok(())
        } else {
            let located = match (&locator, region) {
                (ItemLocator::Position(_), Some(region)) => {
                    crate::edit::candidate_item(c, map, &region)
                }
                (ItemLocator::Position(_), None) => None,
                (ItemLocator::Id(_), _) => match c.resolve(&locator.selector()) {
                    Ok(Target::Item { list, item }) => Some((list, item)),
                    _ => None,
                },
            };
            let Some((list, item)) = located else {
                return Err(Diagnostic::new(
                    ErrorCode::InvalidItem,
                    if locator.is_position() {
                        "the written item does not re-read as one list item"
                    } else {
                        "item lost its stay"
                    },
                ));
            };
            let got = extract(&c.item_content(list, item), &schema)
                .map_err(|e| Diagnostic::new(ErrorCode::InvalidItem, e.join("; ")))?;
            // A stay the text happens to carry (a list's, say) has its digest
            // refreshed by any write, which no request asks for and none can
            // prevent, so digests are left out of the comparison.
            let blank = |m: &Map<String, Value>| {
                crate::edit::blank_digests(&Value::Object(m.clone()).to_string())
            };
            if blank(&got) != blank(&expect) {
                let unblank = |s: String| match serde_json::from_str(&s) {
                    Ok(Value::Object(m)) => m,
                    _ => Map::new(),
                };
                return Err(Diagnostic::new(
                    ErrorCode::InvalidItem,
                    format!(
                        "the item would not read back as asked: {}",
                        field_changes(&unblank(blank(&got)), &unblank(blank(&expect)))
                    ),
                ));
            }
            Ok(())
        }
    }
}

/// The collection item a typed write names, resolved the way `get` resolves
/// a selector, and checked for membership in the bound collection.
pub(crate) fn guarded_item(
    doc: &Document,
    bound: &Bound,
    locator: &ItemLocator,
) -> Result<(usize, usize), Diagnostic> {
    let inst = instance(doc, bound)?.ok_or_else(|| {
        Diagnostic::new(
            ErrorCode::MissingSelector,
            "this file has no such collection",
        )
    })?;
    let target = doc.resolve(&locator.selector())?;
    let named = locator.describe();
    let Target::Item { list, item } = target else {
        return Err(Diagnostic::new(
            ErrorCode::MissingSelector,
            format!("{named} is not an item"),
        ));
    };
    if !inst.items.contains(&(list, item)) {
        return Err(Diagnostic::new(
            ErrorCode::MissingSelector,
            format!("{named} is not an item of collection {}", bound.name),
        ));
    }
    Ok((list, item))
}

/// Native body replacement with original native and binding-aware typed guards.
pub fn plan_bound_set(
    planner: &mut Planner,
    bindings: &[Bound],
    op: &crate::edit::Op,
) -> Result<Change, Diagnostic> {
    use crate::edit::{Expectation, Guard, GuardSubject, Op};
    let Op::Set {
        select,
        part,
        guard,
        content,
        collection: Some(name),
        typed_guard: Some(typed),
        ..
    } = op
    else {
        return Err(Diagnostic::new(
            ErrorCode::InvalidRequest,
            "set requires collection and typed_guard together",
        ));
    };
    if part.is_some() || !content.is_string() {
        return Err(Diagnostic::new(
            ErrorCode::InvalidRequest,
            "bound set requires item string content and no part",
        ));
    }
    let bound = precondition_binding(bindings, name)?;
    let doc = planner.doc;
    let Target::Item { list, item } = doc.resolve(select)? else {
        return Err(Diagnostic::new(
            ErrorCode::InvalidRequest,
            "bound set targets one list item",
        ));
    };
    let locator = match select {
        crate::Selector::Position { .. } => ItemLocator::Position(select.clone()),
        _ => ItemLocator::Id(
            doc.stay_ids(&doc.lists[list].items[item].markers)
                .first()
                .cloned()
                .ok_or_else(|| {
                    Diagnostic::new(ErrorCode::MissingSelector, "bound set needs an item stay")
                })?,
        ),
    };
    guarded_item(doc, bound, &locator)
        .map_err(|e| Diagnostic::new(ErrorCode::CollectionConflict, e.message))?;
    let intent_guard = Guard::new(
        GuardSubject::BoundBody {
            bound: bound.clone(),
            locator: locator.clone(),
            native: guard.clone(),
        },
        typed,
    );
    intent_guard.check(doc)?;
    let text = content.as_str().expect("checked string");
    let mut change = planner.set_item(list, item, text)?;
    let Some(Expectation::Item { expect, .. }) = change.expectation.as_ref() else {
        unreachable!("set_item supplies normalized item content");
    };
    let expect = extract(expect, &bound.schema)
        .map_err(|e| Diagnostic::new(ErrorCode::InvalidItem, e.join("; ")))?;
    let region = change
        .expectation
        .as_ref()
        .and_then(Expectation::item_region);
    if locator.is_position() {
        change.follow = Some(crate::edit::Follow::Placed {
            region: region.expect("an item write has a region"),
            typed: None,
        });
    }
    change.expectation = Some(Expectation::Fields {
        inner: Box::new(change.expectation.take().expect("item intent")),
        fields: Box::new(FieldExpectation {
            bound: bound.clone(),
            region: region.filter(|_| locator.is_position()),
            locator,
            expect,
            insert: false,
        }),
    });
    change.guard = Some(intent_guard);
    change.preflight(doc)?;
    Ok(change)
}

fn precondition_binding<'a>(bindings: &'a [Bound], name: &str) -> Result<&'a Bound, Diagnostic> {
    bindings.iter().find(|b| b.name == name).ok_or_else(|| {
        Diagnostic::new(
            ErrorCode::CollectionConflict,
            format!("this file is not in collection {name:?}"),
        )
    })
}

/// Explicit checkbox-only collection stamping, or the unchanged native modes.
pub fn plan_stamp(
    planner: &mut Planner,
    bindings: &[Bound],
    op: &crate::edit::Op,
) -> Result<Change, Diagnostic> {
    use crate::edit::{Guard, GuardSubject, Op};
    let Op::Stamp {
        select,
        guard,
        collection,
        checkbox_only,
        collection_guard,
    } = op
    else {
        return Err(Diagnostic::new(ErrorCode::InvalidRequest, "expected stamp"));
    };
    if collection.is_none() && !checkbox_only && collection_guard.is_none() {
        return if matches!(select, crate::Selector::Collections) {
            plan_stamp_collections(planner, bindings, guard)
        } else {
            planner.structural(op)
        };
    }
    let (Some(name), true, Some(typed)) = (collection, checkbox_only, collection_guard) else {
        return Err(Diagnostic::new(
            ErrorCode::InvalidRequest,
            "stamp requires collection, checkbox_only true and collection_guard together",
        ));
    };
    if !matches!(select, crate::Selector::Collections) {
        return Err(Diagnostic::new(
            ErrorCode::InvalidRequest,
            "checkbox-only stamp requires select collections",
        ));
    }
    let bound = precondition_binding(bindings, name)?;
    let doc = planner.doc;
    let inst = instance(doc, bound)?
        .ok_or_else(|| Diagnostic::new(ErrorCode::CollectionConflict, "collection is absent"))?;
    let intent_guard = Guard::new(
        GuardSubject::CheckboxStamp {
            bound: bound.clone(),
            native: guard.clone(),
        },
        typed,
    );
    intent_guard.check(doc)?;
    let mut change = planner.stamp_selected(Target::Document, Some(&inst.spans(doc)), true)?;
    if let Some(d) = change.refused.first() {
        return Err(d.clone());
    }
    change.guard = Some(intent_guard);
    let bound = bound.clone();
    change.collection = Some(Box::new(move |c| {
        instance(c, &bound)
            .ok()
            .flatten()
            .map(|i| collection_revision(c, &bound, &i))
    }));
    change.preflight(doc)?;
    Ok(change)
}

/// Plan a typed field update as a whole-item rewrite.
pub(crate) fn plan_update(
    planner: &mut Planner,
    bound: &Bound,
    locator: &ItemLocator,
    guard: &str,
    scope: GuardScope,
    set: &BTreeMap<String, Value>,
) -> Result<Change, Diagnostic> {
    if scope == GuardScope::Field {
        return Err(Diagnostic::new(
            ErrorCode::UnsupportedSyntax,
            "field guard scope is reserved; use the whole-action guard",
        ));
    }
    let doc = planner.doc;
    let (list, item) = guarded_item(doc, bound, locator)?;
    let intent_guard = crate::edit::Guard::new(
        crate::edit::GuardSubject::Item {
            bound: bound.clone(),
            locator: locator.clone(),
        },
        guard,
    );
    intent_guard.check(doc)?;
    let content = doc.item_content(list, item);
    let before = extract(&content, &bound.schema).map_err(|e| {
        Diagnostic::new(
            ErrorCode::InvalidItem,
            format!("item is invalid before the edit: {}", e.join("; ")),
        )
    })?;
    let text = edit_fields(&canonical(&content), &bound.schema, set)?;
    let new_content = decanonical(&text);
    let mut expect = before.clone();
    for (k, v) in set {
        match (v, bound.schema.fields[k].default.clone()) {
            (Value::Null, Some(d)) => {
                expect.insert(k.clone(), d);
            }
            (Value::Null, None) => {
                expect.remove(k);
            }
            _ => {
                expect.insert(k.clone(), v.clone());
            }
        }
    }
    // A date lives inside the title envelope, so removing the title removes it
    // whether or not the request said so. Expecting it to survive would refuse
    // the write for doing exactly what it was asked to do.
    let removes_title = bound
        .schema
        .fields
        .iter()
        .any(|(n, f)| f.from == Source::Title && set.get(n).is_some_and(Value::is_null));
    if removes_title {
        for (n, f) in &bound.schema.fields {
            if f.from == Source::TitleDate {
                match f.default.clone() {
                    Some(d) => expect.insert(n.clone(), d),
                    None => expect.remove(n),
                };
            }
        }
    }
    // A request naming only the checkbox rewrites one byte rather than the
    // item, so an item whose body carries a block's stay can still be ticked.
    let checkbox_only = set
        .iter()
        .map(|(k, v)| (bound.schema.fields[k].from, v))
        .collect::<Vec<_>>();
    let mut change = match checkbox_only.as_slice() {
        [(Source::Checkbox, Value::Bool(checked))] => planner.set_checkbox(list, item, *checked)?,
        _ => planner.set_item(list, item, &new_content)?,
    };
    let region = change
        .expectation
        .as_ref()
        .and_then(crate::edit::Expectation::item_region)
        .filter(|_| locator.is_position());
    follow_typed(&mut change, bound, locator, region);
    change.expectation = Some(crate::edit::Expectation::Fields {
        inner: Box::new(change.expectation.take().expect("item intent")),
        fields: Box::new(FieldExpectation {
            bound: bound.clone(),
            locator: locator.clone(),
            region,
            expect,
            insert: false,
        }),
    });
    change.guard = Some(intent_guard);
    change.preflight(doc)?;
    Ok(change)
}

/// Plan a typed insertion, stamping the new item. It goes into the member
/// section `under` names, else the first member (`start`) or the last (`end`).
pub fn plan_insert(
    planner: &mut Planner,
    bound: &Bound,
    position: Position,
    under: Option<&str>,
    guard: &str,
    fields: &BTreeMap<String, Value>,
) -> Result<Change, Diagnostic> {
    let doc = planner.doc;
    let inst = instance(doc, bound)?.ok_or_else(|| {
        Diagnostic::new(
            ErrorCode::MissingSelector,
            "this file has no such collection",
        )
    })?;
    let intent_guard =
        crate::edit::Guard::new(crate::edit::GuardSubject::Collection(bound.clone()), guard);
    intent_guard.check(doc)?;
    let member = match under {
        None => match position {
            Position::Start => inst.section,
            Position::End => *inst.members.last().expect("the bound section is a member"),
        },
        Some(title) => inst.named_member(doc, title)?.ok_or_else(|| {
            Diagnostic::new(
                ErrorCode::MissingSelector,
                format!(
                    "collection {} has no member section titled {title:?}",
                    bound.name
                ),
            )
        })?,
    };
    let content = render_new(&bound.schema, fields)?;
    let mut expect = extract(&content, &bound.schema)
        .map_err(|e| Diagnostic::new(ErrorCode::InvalidItem, e.join("; ")))?;
    for (k, v) in fields {
        if !v.is_null() && expect.get(k) != Some(v) {
            expect.insert(k.clone(), v.clone());
        }
    }
    let lists = inst.member_lists(doc, member);
    let target = match position {
        Position::Start => lists.first(),
        Position::End => lists.last(),
    };
    let mut change = match target {
        Some(&list) => {
            planner.insert_item(list, position, &content.replace('\n', doc.newline), true)?
        }
        None => new_list(planner, member, &content)?,
    };
    let id = change
        .minted
        .iter()
        .find(|(_, k, _)| *k == "item")
        .map(|(id, _, _)| id.clone())
        .expect("typed inserts stamp the item");
    let locator = ItemLocator::Id(id);
    follow_typed(&mut change, bound, &locator, None);
    change.expectation = Some(crate::edit::Expectation::Fields {
        inner: Box::new(change.expectation.take().expect("item intent")),
        fields: Box::new(FieldExpectation {
            bound: bound.clone(),
            locator,
            region: None,
            expect,
            insert: true,
        }),
    });
    change.guard = Some(intent_guard);
    change.preflight(doc)?;
    Ok(change)
}

/// Report the typed guards a follow-up write needs: the item's action
/// revision and the collection revision, computed on the written candidate.
/// A positional write finds its item by the extent it was written to, so
/// the returned handle names where the item is now, not the request's place.
fn follow_typed(
    change: &mut Change,
    bound: &Bound,
    locator: &ItemLocator,
    region: Option<crate::edit::ItemRegion>,
) {
    let collection_bound = bound.clone();
    change.collection = Some(Box::new(move |c| {
        instance(c, &collection_bound)
            .ok()
            .flatten()
            .map(|inst| collection_revision(c, &collection_bound, &inst))
    }));
    let id = match locator {
        ItemLocator::Position(_) => {
            change.follow = Some(crate::edit::Follow::Placed {
                region: region.expect("a positional write has a region"),
                typed: Some(Box::new(bound.clone())),
            });
            return;
        }
        ItemLocator::Id(id) => id,
    };
    let item_bound = bound.clone();
    let key = id.to_string();
    change.follow = Some(crate::edit::Follow::Custom(
        key.clone(),
        Box::new(
            move |c| match c.resolve(&crate::Selector::Id { id: key.clone() }) {
                Ok(Target::Item { list, item }) => Some(item_revision(c, &item_bound, list, item)),
                _ => None,
            },
        ),
    ));
}

/// Start a member section's first list at the end of its own body.
fn new_list(planner: &mut Planner, section: usize, content: &str) -> Result<Change, Diagnostic> {
    let doc = planner.doc;
    let nl = doc.newline;
    let rendered = canonical(content).replace('\n', nl);
    let at_marker = {
        let (count, carrier) = crate::markdown::standalone_item(&canonical(content));
        if count != 1 {
            return Err(Diagnostic::new(
                ErrorCode::InvalidItem,
                "fields render as more than one item",
            ));
        }
        let carrier = carrier.ok_or_else(|| {
            Diagnostic::new(ErrorCode::StampRefused, "no paragraph to carry a stay")
        })?;
        let lf = canonical(content);
        // The same §3.4 scan the rest of the engine uses, so a code span that
        // v1.8 neutralises is accepted here too. No blocker: this text is
        // rendered from the caller's fields and is nowhere in the document.
        if let Some(r) = crate::edit::carrier_refusal(&lf[..carrier], &[]) {
            return Err(Diagnostic::new(ErrorCode::StampRefused, r.reason));
        }
        lf[..carrier].matches('\n').count() * (nl.len() - 1) + carrier
    };
    let item_id = planner.mint();
    let list_id = planner.mint();
    let raw = format!(
        "{} {}{}",
        &rendered[..at_marker],
        markers::format_plain(&item_id, "subhash", "000000000000"),
        &rendered[at_marker..]
    );
    let (splice, _) = crate::edit::new_list_frame(doc, section, &raw, &list_id);
    let at = splice.span.start;
    let mut change = Change {
        splices: vec![splice],
        ..Change::default()
    };
    change.expectation = Some(crate::edit::Expectation::NewList { section });
    change.minted = vec![(item_id.clone(), "item", at), (list_id, "list", at)];
    change.follow = Some(crate::edit::Follow::Id(item_id));
    Ok(change)
}

/// A comparison in a query. Values are never coerced between types.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Clause {
    pub field: String,
    pub op: Cmp,
    #[serde(default)]
    pub value: Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Cmp {
    Eq,
    Ne,
    In,
    Lt,
    Lte,
    Gt,
    Gte,
    Exists,
    Missing,
    Contains,
}

pub fn check_clause(schema: &Schema, clause: &Clause) -> Result<(), Diagnostic> {
    let bad = |m: String| Diagnostic::new(ErrorCode::InvalidRequest, m);
    let kind = if clause.field == "id" {
        FieldType::String
    } else {
        schema
            .fields
            .get(&clause.field)
            .ok_or_else(|| bad(format!("{} is not a field of {}", clause.field, schema.id)))?
            .kind
    };
    let values = schema
        .fields
        .get(&clause.field)
        .map(|f| f.values.clone())
        .unwrap_or_default();
    let fits = |v: &Value| match kind {
        FieldType::String => v.is_string(),
        FieldType::Date => v.as_str().is_some_and(valid_date),
        FieldType::Enum => v.as_str().is_some_and(|s| values.iter().any(|x| x == s)),
        FieldType::Boolean => v.is_boolean(),
        FieldType::Number => v.as_number().is_some_and(comparable),
    };
    let ok = match clause.op {
        Cmp::Exists | Cmp::Missing => clause.value.is_null(),
        Cmp::In => clause.value.as_array().is_some_and(|a| a.iter().all(fits)),
        Cmp::Contains => kind == FieldType::String && clause.value.is_string(),
        Cmp::Lt | Cmp::Lte | Cmp::Gt | Cmp::Gte => {
            kind != FieldType::Boolean && fits(&clause.value)
        }
        Cmp::Eq | Cmp::Ne => fits(&clause.value),
    };
    if ok {
        Ok(())
    } else {
        Err(bad(format!(
            "clause on {} does not fit its {kind:?} type",
            clause.field
        )
        .to_lowercase()))
    }
}

fn order(a: &Value, b: &Value) -> Option<std::cmp::Ordering> {
    match (a, b) {
        (Value::String(x), Value::String(y)) => Some(x.cmp(y)),
        (Value::Number(x), Value::Number(y)) => Some(cmp_decimal(&x.to_string(), &y.to_string())?),
        _ => None,
    }
}

/// A JSON number as (negative, significant digits, exponent of the last
/// digit). Exponents are bounded so nothing downstream can overflow; a number
/// outside the bound is not comparable.
pub(crate) fn decimal(text: &str) -> Option<(bool, String, i128)> {
    const BOUND: i128 = 1 << 62;
    let (negative, rest) = match text.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, text),
    };
    let (mantissa, exponent) = match rest.find(['e', 'E']) {
        Some(i) => (&rest[..i], rest[i + 1..].parse::<i128>().ok()?),
        None => (rest, 0),
    };
    let (int, frac) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    if !(int.bytes().chain(frac.bytes()).all(|b| b.is_ascii_digit())) || int.is_empty() {
        return None;
    }
    let digits = format!("{int}{frac}");
    let trimmed = digits.trim_start_matches('0');
    let significant = trimmed.trim_end_matches('0');
    let exp = exponent
        .checked_sub(frac.len() as i128)?
        .checked_add((trimmed.len() - significant.len()) as i128)?;
    if exp.unsigned_abs() > BOUND as u128 {
        return None;
    }
    Some((
        negative && !significant.is_empty(),
        significant.to_string(),
        exp,
    ))
}

/// Whether a JSON number can be ordered exactly by `cmp_decimal`.
pub fn comparable(n: &serde_json::Number) -> bool {
    decimal(&n.to_string()).is_some()
}

/// Exact ordering of two JSON numbers, with no conversion to floating point.
pub(crate) fn cmp_decimal(a: &str, b: &str) -> Option<std::cmp::Ordering> {
    use std::cmp::Ordering::*;
    let (an, ad, ae) = decimal(a)?;
    let (bn, bd, be) = decimal(b)?;
    let magnitude = || {
        if ad.is_empty() || bd.is_empty() {
            return ad.len().cmp(&bd.len());
        }
        // Position of the most significant digit decides first.
        let (ta, tb) = (ad.len() as i128 + ae, bd.len() as i128 + be);
        ta.cmp(&tb).then_with(|| {
            let width = ad.len().max(bd.len());
            format!("{ad:0<width$}").cmp(&format!("{bd:0<width$}"))
        })
    };
    Some(match (an, bn) {
        (false, true) => Greater,
        (true, false) => Less,
        (false, false) => magnitude(),
        (true, true) => magnitude().reverse(),
    })
}

pub fn matches(fields: &Map<String, Value>, id: Option<&str>, clause: &Clause) -> bool {
    let value = if clause.field == "id" {
        id.map(|i| Value::String(i.into()))
    } else {
        fields.get(&clause.field).cloned()
    };
    use std::cmp::Ordering::*;
    match (clause.op, value) {
        (Cmp::Exists, v) => v.is_some(),
        (Cmp::Missing, v) => v.is_none(),
        (_, None) => false,
        (Cmp::Eq, Some(v)) => v == clause.value,
        (Cmp::Ne, Some(v)) => v != clause.value,
        (Cmp::In, Some(v)) => clause.value.as_array().is_some_and(|a| a.contains(&v)),
        (Cmp::Contains, Some(v)) => v
            .as_str()
            .zip(clause.value.as_str())
            .is_some_and(|(h, n)| h.contains(n)),
        (Cmp::Lt, Some(v)) => order(&v, &clause.value) == Some(Less),
        (Cmp::Lte, Some(v)) => matches!(order(&v, &clause.value), Some(Less | Equal)),
        (Cmp::Gt, Some(v)) => order(&v, &clause.value) == Some(Greater),
        (Cmp::Gte, Some(v)) => matches!(order(&v, &clause.value), Some(Greater | Equal)),
    }
}

/// One path segment against one glob segment, bytewise, so native names that
/// are not UTF-8 can be tested without a lossy conversion.
pub(crate) fn glob_segment(p: &[u8], s: &[u8]) -> bool {
    match (p.first(), s.first()) {
        (None, None) => true,
        (Some(b'*'), _) => glob_segment(&p[1..], s) || (!s.is_empty() && glob_segment(p, &s[1..])),
        (Some(b'?'), Some(_)) => glob_segment(&p[1..], &s[1..]),
        (Some(a), Some(b)) if a == b => glob_segment(&p[1..], &s[1..]),
        _ => false,
    }
}

/// Glob over `/`-separated relative paths: `**` spans whole segments.
pub fn glob(pattern: &str, path: &str) -> bool {
    use glob_segment as seg;
    fn walk(p: &[&str], s: &[&str]) -> bool {
        match (p.first(), s.first()) {
            (None, None) => true,
            (Some(&"**"), _) => walk(&p[1..], s) || (!s.is_empty() && walk(p, &s[1..])),
            (Some(a), Some(b)) => seg(a.as_bytes(), b.as_bytes()) && walk(&p[1..], &s[1..]),
            _ => false,
        }
    }
    let p: Vec<&str> = pattern.split('/').collect();
    let s: Vec<&str> = path.split('/').collect();
    walk(&p, &s)
}

/// Intersection of the same segment grammar as `glob`, without enumerating files.
/// Product automata terminate even when both sides have wildcard self-loops.
fn globs_intersect(a: &str, b: &str) -> bool {
    fn segments(a: &[u8], b: &[u8]) -> bool {
        let mut todo = vec![(0, 0)];
        let mut seen = BTreeSet::new();
        while let Some((i, j)) = todo.pop() {
            if !seen.insert((i, j)) {
                continue;
            }
            if i == a.len() && j == b.len() {
                return true;
            }
            if a.get(i) == Some(&b'*') {
                todo.push((i + 1, j));
            }
            if b.get(j) == Some(&b'*') {
                todo.push((i, j + 1));
            }
            if let (Some(x), Some(y)) = (a.get(i), b.get(j))
                && (x == y || matches!(x, b'*' | b'?') || matches!(y, b'*' | b'?'))
            {
                todo.push((i + usize::from(*x != b'*'), j + usize::from(*y != b'*')));
            }
        }
        false
    }
    let a: Vec<_> = a.split('/').collect();
    let b: Vec<_> = b.split('/').collect();
    let mut todo = vec![(0, 0)];
    let mut seen = BTreeSet::new();
    while let Some((i, j)) = todo.pop() {
        if !seen.insert((i, j)) {
            continue;
        }
        if i == a.len() && j == b.len() {
            return true;
        }
        if a.get(i) == Some(&"**") {
            todo.push((i + 1, j));
        }
        if b.get(j) == Some(&"**") {
            todo.push((i, j + 1));
        }
        if let (Some(x), Some(y)) = (a.get(i), b.get(j))
            && (*x == "**" || *y == "**" || segments(x.as_bytes(), y.as_bytes()))
        {
            todo.push((i + usize::from(*x != "**"), j + usize::from(*y != "**")));
        }
    }
    false
}

/// Shared file-binding predicate; traversal policy remains the walker's concern.
pub fn matches_file_patterns(patterns: &[String], path: &str) -> bool {
    patterns.iter().any(|pattern| glob(pattern, path))
}

/// Literal bindings cannot opt a path into the walker's excluded namespace.
/// Only '*' and '?' are metacharacters in Rivet's glob grammar.
pub(crate) fn validate_file_patterns(patterns: &[String]) -> Result<(), Diagnostic> {
    for pattern in patterns {
        if pattern.contains(['*', '?']) {
            continue;
        }
        if let Some(component) = pattern
            .split('/')
            .find(|c| crate::fs::skipped_component(c).is_some())
        {
            return Err(invalid_schema(format!(
                "literal binding {pattern:?} selects traversal-skipped component {component:?}"
            ))
            .with_path(pattern));
        }
    }
    Ok(())
}

/// Validate both namespaces even when only the live collection is requested.
pub(crate) fn validate_binding_paths(
    name: &str,
    collection: &CollectionConfig,
) -> Result<(), Diagnostic> {
    for (field, patterns) in [
        ("files", &collection.files),
        ("archive", &collection.archive),
    ] {
        validate_file_patterns(patterns).map_err(|mut d| {
            d.message = format!("collection {name:?} {field}: {}", d.message);
            d
        })?;
    }
    Ok(())
}

/// Check direct paths against both namespaces without loading a schema.
pub(crate) fn validate_path_bindings(config: &ProjectConfig, path: &str) -> Result<(), Diagnostic> {
    for (name, collection) in &config.collections {
        if matches_file_patterns(&collection.files, path)
            || matches_file_patterns(&collection.archive, path)
        {
            validate_binding_paths(name, collection)?;
        }
    }
    Ok(())
}

/// Markdown documents matching any glob, sorted, with diagnostics for relevant
/// traversal gaps and policy exclusions.
pub fn files(project: &Project, globs: &[String]) -> (Vec<String>, Vec<Diagnostic>) {
    let (files, mut diagnostics) =
        project.documents(globs, None, &crate::fs::WalkFaults::default());
    if let Err(d) = validate_file_patterns(globs) {
        diagnostics.push(d);
    }
    (files, diagnostics)
}

/// Stamp under `Selector::Collections`: every section in the document, and
/// list items only inside the member sections of its bound collections. A `<` in a
/// prose list is then not a refusal a caller has to read past, and a file does
/// not fill with ids for bullets nothing addresses. Guarded by the document.
pub fn plan_stamp_collections(
    planner: &mut Planner,
    bindings: &[Bound],
    guard: &str,
) -> Result<Change, Diagnostic> {
    let doc = planner.doc;
    let intent_guard =
        crate::edit::Guard::new(crate::edit::GuardSubject::Native(Target::Document), guard);
    intent_guard.check(doc)?;
    let mut spans = vec![];
    for bound in bindings {
        if let Some(inst) = instance(doc, bound)? {
            spans.extend(inst.spans(doc));
        }
    }
    let mut change = planner.stamp_scoped(Target::Document, Some(&spans))?;
    change.guard = Some(intent_guard);
    change.preflight(doc)?;
    Ok(change)
}

#[cfg(test)]
mod pinned_tests {
    use super::*;

    const PROJECT: &str = include_str!("../../../examples/tracker/.rivet/project.json");
    const SCHEMA: &str =
        include_str!("../../../examples/tracker/.rivet/schemas/tracker-action.json");
    const WIDGETS: &str = include_str!("../../../examples/tracker/tracking/TRACKING_WIDGETS.md");

    /// Checkbox guards a caller already holds must not move when the binding
    /// grows new shapes: the fingerprint tuple, an `action` and a
    /// `collection` revision from the example tracker, as literals.
    #[test]
    fn checkbox_binding_and_revisions_are_pinned() {
        let config: ProjectConfig = serde_json::from_str(PROJECT).unwrap();
        let bound = bind_bytes(
            "actions",
            config.collections["actions"].clone(),
            SCHEMA.as_bytes(),
        )
        .unwrap();
        assert_eq!(
            bound.binding,
            "fc197f3469b5ac20460e2d0fa9bf02b6ba27f40a9fa9c96f6a1d7e610fcc55c0"
        );
        let doc = Document::parse(WIDGETS).unwrap();
        let inst = instance(&doc, &bound).unwrap().unwrap();
        let (list, item) = inst.items[0];
        assert_eq!(
            item_revision(&doc, &bound, list, item),
            "rivet:v1:action:fdb009a755d9721389ae7f62002ce876"
        );
        assert_eq!(
            collection_revision(&doc, &bound, &inst),
            "rivet:v1:collection:37611eeca9c98d8682550333d321476c"
        );
    }
}
