//! Planning single-file mutations. Every operation resolves against one
//! preimage, becomes byte splices, and the complete candidate is re-parsed and
//! verified before anything touches the filesystem: untouched blocks and items
//! must reappear byte-for-byte at their mapped positions with the same stays.
use crate::document::{Attach, Document, Marker, Target};
use crate::markdown;
use crate::markers;
use crate::{
    CarrierBlocker, Diagnostic, ErrorCode, GuardScope, Position, SectionPart, Selector, Span,
    revision_kind, yaml,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

pub const HASH_PRECISION: usize = 12;
const PLACEHOLDER: &str = "000000000000";

/// Explicit authority to remove a selected sole item's containing list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListGuard {
    pub guard: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum Op {
    /// Replace the selected value. Sections need `part`.
    Set {
        select: Selector,
        #[serde(default)]
        part: Option<SectionPart>,
        guard: String,
        content: Value,
        /// Require the original typed item revision and its exact collection.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        collection: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        typed_guard: Option<String>,
        /// Replacing a region that holds other stays drops them only when set.
        #[serde(default)]
        allow_drop: bool,
    },
    /// Insert a list item (target: a list's stay) or a child section (target:
    /// a section) at the start or end of the collection.
    Insert {
        select: Selector,
        position: Position,
        guard: String,
        content: String,
        #[serde(default)]
        stamp: bool,
    },
    Delete {
        select: Selector,
        guard: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        list: Option<ListGuard>,
        /// Require this item to belong to the named queue in the locked preimage.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        collection: Option<String>,
    },
    /// Append Markdown blocks to the end of a section's own body, before
    /// its first subsection. Headings are refused: `insert` adds sections.
    Append {
        select: Selector,
        guard: String,
        content: String,
    },
    /// Replace one exact, unique source substring in a section's own body.
    /// Both strings are single-line; the match must be inside a paragraph.
    ReplaceInSection {
        select: Selector,
        guard: String,
        old: String,
        new: String,
        /// Refuse a match in a list item or block quote, so a caller keeps
        /// those to their own typed writes.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        top_level: bool,
    },
    /// Remove one exact whole top-level paragraph, without dropping stays.
    RemoveParagraphInSection {
        select: Selector,
        guard: String,
        old: String,
    },
    /// Add a top-level frontmatter key the document does not have yet,
    /// after the last one, under the document guard.
    AddKey {
        key: String,
        value: Value,
        guard: String,
    },
    /// Set one existing immediate scalar field of a frontmatter mapping.
    /// The two key path segments are literal keys; the guard is the parent's.
    SetKeyField {
        key_path: Vec<String>,
        value: Value,
        guard: String,
    },
    /// Delete a top-level frontmatter key under that key's own guard.
    DeleteKey { key: String, guard: String },
    /// Add a top-level frontmatter key whose value is the caller's exact
    /// single-line YAML token (a scalar or a flat flow sequence of scalars),
    /// under the document guard.
    AddKeyYaml {
        key: String,
        yaml: String,
        guard: String,
    },
    /// Replace an existing single-line scalar or flat flow-sequence value
    /// with the caller's exact YAML token, under that key's own guard. The
    /// line's trailing comment is kept unless `replace_line` removes
    /// everything after the token on the key's first line.
    SetKeyYaml {
        key: String,
        yaml: String,
        guard: String,
        #[serde(default)]
        replace_line: bool,
    },
    /// Append one flat mapping of scalars to a top-level frontmatter sequence
    /// of such mappings, in the sequence's own flow or block form, under that
    /// key's guard; or, when the key is absent, add it as `key: [{...}]`
    /// after the last field under the document guard.
    AppendKeyItem {
        key: String,
        item: serde_json::Map<String, Value>,
        guard: String,
    },
    /// Repair: move a line break out of a code span that opens on `line`
    /// (1-based, in the selected section) and closes on the next line.
    /// Only where the break falls changes.
    Rejoin {
        select: Selector,
        guard: String,
        line: usize,
    },
    /// Repair: give a hand-written stay (`<!-- stay:<id> -->`, no digest)
    /// inside an unstamped list item Rivet's child-stay form, so the id
    /// becomes the item's. The guard is the list's, which that marker names.
    AdoptStay { id: String, guard: String },
    /// Repair: rewrite the named stays' stored digests to their units'
    /// current digests at the stored precision, and nothing else. Each id
    /// carries the stored digest the caller saw; the guard is the document's.
    Refresh {
        select: Vec<RefreshSelect>,
        guard: String,
    },
    /// Mint stays for unstamped headings and direct list items in scope.
    Stamp {
        /// Defaults to [`Selector::Collections`]: every section, and list
        /// items only where a bound collection holds them. `document` still
        /// stamps every list in the file.
        #[serde(default = "collections_selector")]
        select: Selector,
        guard: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        collection: Option<String>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        checkbox_only: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        collection_guard: Option<String>,
    },
    /// Typed whole-item field edits under an item-scoped guard. The item is
    /// named by exactly one of `id` and `select` (a position or id selector).
    UpdateItem {
        collection: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        select: Option<Selector>,
        guard: String,
        #[serde(default)]
        guard_scope: GuardScope,
        set: BTreeMap<String, Value>,
    },
    /// Typed insertion into a bound collection.
    InsertItem {
        collection: String,
        position: Position,
        /// Exact title of the member section to insert into.
        #[serde(default)]
        under: Option<String>,
        guard: String,
        fields: BTreeMap<String, Value>,
    },
}

fn collections_selector() -> Selector {
    Selector::Collections
}

impl Op {
    /// Refuse a position selector in every op field that does not take one
    /// (only `set` and `update_item` do), and an `update_item` that names
    /// its item other than by exactly one of an id and a position or id
    /// selector. Runs where a write request is validated, before anything else.
    pub fn check_selectors(&self) -> Result<(), Diagnostic> {
        match self {
            Op::Insert { select, .. } => select.require_not_position("insert.select"),
            Op::Delete { select, .. } => select.require_not_position("delete.select"),
            Op::Append { select, .. } => select.require_not_position("append.select"),
            Op::ReplaceInSection { select, .. } => {
                select.require_not_position("replace_in_section.select")
            }
            Op::RemoveParagraphInSection { select, .. } => {
                select.require_not_position("remove_paragraph_in_section.select")
            }
            Op::Rejoin { select, .. } => select.require_not_position("rejoin.select"),
            Op::Stamp { select, .. } => select.require_not_position("stamp.select"),
            Op::UpdateItem { id, select, .. } => match (id, select) {
                (Some(_), None) | (None, Some(Selector::Id { .. } | Selector::Position { .. })) => {
                    Ok(())
                }
                (None, Some(_)) => Err(err(
                    ErrorCode::InvalidRequest,
                    "update_item.select must be a position or id selector",
                )),
                _ => Err(err(
                    ErrorCode::InvalidRequest,
                    "update_item names its item by exactly one of id and select",
                )),
            },
            Op::Set { .. }
            | Op::AddKey { .. }
            | Op::SetKeyField { .. }
            | Op::DeleteKey { .. }
            | Op::AddKeyYaml { .. }
            | Op::SetKeyYaml { .. }
            | Op::AppendKeyItem { .. }
            | Op::AdoptStay { .. }
            | Op::Refresh { .. }
            | Op::InsertItem { .. } => Ok(()),
        }
    }
}

fn frontmatter_others(doc: &Document, except: Option<&str>) -> Vec<(String, Value, String)> {
    doc.scalars()
        .iter()
        .filter(|s| Some(s.key.as_str()) != except)
        .map(|s| {
            (
                s.key.clone(),
                s.value.clone(),
                doc.source[s.line.start..s.line.end].to_string(),
            )
        })
        .collect()
}

/// The field's lines and final terminator, leaving trailing standalone
/// comments in place even when the collection reader included them.
fn frontmatter_key_lines(doc: &Document, k: usize) -> Span {
    let line = doc.scalars()[k].line;
    let text = &doc.source[line.start..line.end];
    let mut end = line.end;
    for raw in text.split_inclusive('\n').rev() {
        if raw.trim_start_matches([' ', '\t']).starts_with('#') {
            end -= raw.len();
        } else {
            break;
        }
    }
    if !doc.source[..end].ends_with('\n') {
        let tail = &doc.source[end..];
        end += if tail.starts_with("\r\n") {
            2
        } else if tail.starts_with('\n') {
            1
        } else {
            0
        };
    }
    Span {
        start: line.start,
        end,
    }
}

/// Where a new top-level key goes (the payload, whose end it is inserted
/// at), under the rules every key addition shares: recognized, readable
/// frontmatter, a plain ASCII key name, and no key of that name yet.
pub(crate) fn new_key_site(doc: &Document, key: &str) -> Result<Span, Diagnostic> {
    let Some(payload) = doc.frontmatter.payload else {
        return Err(err(
            ErrorCode::UnsupportedSyntax,
            "the document has no frontmatter to add a key to",
        ));
    };
    let fields = doc.frontmatter.fields.as_ref().map_err(Clone::clone)?;
    check_key_name("frontmatter key", key)?;
    if fields.iter().any(|f| f.key == key) {
        return Err(err(
            ErrorCode::AlreadyExists,
            format!("frontmatter key {key} exists; set it with its own guard"),
        ));
    }
    Ok(payload)
}

/// The name rule every key Rivet adds shares, top-level or inside a new
/// sequence item.
fn check_key_name(what: &str, key: &str) -> Result<(), Diagnostic> {
    if key.is_empty()
        || !key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
        || key.starts_with(['-', '.'])
    {
        return Err(err(
            ErrorCode::InvalidRequest,
            format!(
                "{what} {key:?} must be ASCII letters, digits, `_`, `-` or `.`, not starting with `-` or `.`"
            ),
        ));
    }
    Ok(())
}

/// The one splice an `append_key_item` makes, derived from the preimage and
/// the request alone, so planning and the intent gate compute it the same way.
pub(crate) struct AppendWrite {
    pub splice: Splice,
    /// The key's index when it exists; `None` adds it.
    pub k: Option<usize>,
    /// The whole sequence the key must read back as.
    pub expect: Value,
}

pub(crate) fn yaml_append(
    doc: &Document,
    key: &str,
    item: &serde_json::Map<String, Value>,
) -> Result<AppendWrite, Diagnostic> {
    let invalid = |m: String| err(ErrorCode::InvalidRequest, m);
    if item.is_empty() {
        return Err(invalid("item must be a nonempty mapping".into()));
    }
    for (field, value) in item {
        check_key_name("item key", field)?;
        if !yaml::plain_key(field) {
            return Err(invalid(format!(
                "item key {field:?} would not read back as text written plain"
            )));
        }
        if value.is_array() || value.is_object() {
            return Err(invalid(format!(
                "item field {field:?} must be a string, number, boolean or null"
            )));
        }
    }
    let unrenderable = || {
        err(
            ErrorCode::UnsupportedSyntax,
            "value cannot be written as a single-line JSON-compatible YAML scalar",
        )
    };
    let fields = doc.frontmatter.fields.as_ref().map_err(Clone::clone)?;
    let Some(k) = fields.iter().position(|s| s.key == key) else {
        let payload = new_key_site(doc, key)?;
        let rendered = yaml::render_item(item, &[], false).ok_or_else(unrenderable)?;
        let write = AppendWrite {
            splice: Splice {
                span: Span {
                    start: payload.end,
                    end: payload.end,
                },
                text: format!("{key}: [{}]{}", flow_item(&rendered), doc.newline),
            },
            k: None,
            expect: Value::Array(vec![Value::Object(item.clone())]),
        };
        append_reads_back(doc, &write, key)?;
        return Ok(write);
    };
    let scalar = &fields[k];
    let shape = || {
        err(
            ErrorCode::UnsupportedSyntax,
            format!(
                "frontmatter key {key} is not a single-line flow or block sequence of flat \
                 mappings of scalars; append_key_item appends only to those"
            ),
        )
    };
    let Value::Array(items) = &scalar.value else {
        return Err(shape());
    };
    let flat = |v: &Value| {
        v.as_object()
            .is_some_and(|m| m.values().all(|v| !v.is_array() && !v.is_object()))
    };
    if !items.iter().all(flat) {
        return Err(shape());
    }
    let text = &doc.source[scalar.line.start..scalar.line.end];
    let tail = yaml::sequence_tail(text).ok_or_else(shape)?;
    let block = matches!(tail.form, yaml::SequenceForm::Block { mapping: true, .. });
    let rendered = yaml::render_item(item, &tail.fields, block).ok_or_else(unrenderable)?;
    let (at, text) = match &tail.form {
        yaml::SequenceForm::Flow { at, sep } => (
            scalar.line.start + at,
            format!("{sep}{}", flow_item(&rendered)),
        ),
        yaml::SequenceForm::Block { prefix, mapping } => {
            let body = if *mapping {
                let indent = format!("{}{}", doc.newline, " ".repeat(prefix.len()));
                rendered
                    .iter()
                    .map(|(k, v)| format!("{k}: {v}"))
                    .collect::<Vec<_>>()
                    .join(&indent)
            } else {
                flow_item(&rendered)
            };
            (
                frontmatter_key_lines(doc, k).end,
                format!("{prefix}{body}{}", doc.newline),
            )
        }
    };
    let mut expect = items.clone();
    expect.push(Value::Object(item.clone()));
    let write = AppendWrite {
        splice: Splice {
            span: Span { start: at, end: at },
            text,
        },
        k: Some(k),
        expect: Value::Array(expect),
    };
    append_reads_back(doc, &write, key)?;
    Ok(write)
}

fn flow_item(rendered: &[(String, String)]) -> String {
    let fields: Vec<String> = rendered.iter().map(|(k, v)| format!("{k}: {v}")).collect();
    format!("{{{}}}", fields.join(", "))
}

/// The candidate payload must read, through the unchanged frontmatter
/// reader, as the same fields in the same order with only `key`'s value
/// replaced (or appended). Refused as unsupported otherwise, before any
/// bytes change. Returns the candidate's fields.
fn payload_reads_back(
    doc: &Document,
    splice: &Splice,
    key: &str,
    k: Option<usize>,
    expect: &Value,
    refuse: impl Fn() -> Diagnostic,
) -> Result<(String, Vec<yaml::Scalar>), Diagnostic> {
    let payload = doc
        .frontmatter
        .payload
        .ok_or_else(|| err(ErrorCode::UnsupportedSyntax, "frontmatter is absent"))?;
    let span = splice.span;
    if span.start < payload.start || span.end > payload.end {
        return Err(refuse());
    }
    let candidate = format!(
        "{}{}{}",
        &doc.source[payload.start..span.start],
        splice.text,
        &doc.source[span.end..payload.end]
    );
    let fields = yaml::read_payload(&candidate, payload.start).map_err(|_| refuse())?;
    let mut want: Vec<(&str, &Value)> = doc
        .scalars()
        .iter()
        .map(|s| (s.key.as_str(), &s.value))
        .collect();
    match k {
        Some(k) => want[k].1 = expect,
        None => want.push((key, expect)),
    }
    let got: Vec<(&str, &Value)> = fields.iter().map(|s| (s.key.as_str(), &s.value)).collect();
    if got != want {
        return Err(refuse());
    }
    Ok((candidate, fields))
}

fn append_reads_back(doc: &Document, write: &AppendWrite, key: &str) -> Result<(), Diagnostic> {
    let refuse = || {
        err(
            ErrorCode::UnsupportedSyntax,
            format!(
                "frontmatter key {key} would not read back as its sequence with the item \
                 appended; the write is refused"
            ),
        )
    };
    payload_reads_back(doc, &write.splice, key, write.k, &write.expect, refuse).map(|_| ())
}

/// The one splice a raw YAML key write makes, derived from the preimage and
/// the request alone, so planning and the intent gate compute it the same way.
pub(crate) struct YamlWrite {
    pub splice: Splice,
    /// The decoded value the key must read back as.
    pub expect: Value,
    /// The key's whole first line after the write, without its terminator.
    pub first_line: String,
    /// Where the token starts in that line.
    pub token_start: usize,
    /// The caller's exact token.
    pub token: String,
}

fn unsupported_token(raw: &str) -> Diagnostic {
    err(
        ErrorCode::UnsupportedSyntax,
        format!(
            "yaml {raw:?} is not one single-line JSON-compatible YAML scalar or flat flow \
             sequence of them, with nothing around it"
        ),
    )
}

pub(crate) fn yaml_add(doc: &Document, key: &str, raw: &str) -> Result<YamlWrite, Diagnostic> {
    let payload = new_key_site(doc, key)?;
    let expect = yaml::raw_token(raw).ok_or_else(|| unsupported_token(raw))?;
    let first_line = format!("{key}: {raw}");
    let write = YamlWrite {
        splice: Splice {
            span: Span {
                start: payload.end,
                end: payload.end,
            },
            text: format!("{first_line}{}", doc.newline),
        },
        expect,
        token_start: key.len() + 2,
        token: raw.to_string(),
        first_line,
    };
    yaml_reads_back(doc, &write, key, None)?;
    Ok(write)
}

/// Replace the value token on key `k`'s first line. The rest of that line
/// stays unless `replace_line`; later lines the reader gave the field may
/// only be comments, and stay.
pub(crate) fn yaml_set(
    doc: &Document,
    k: usize,
    raw: &str,
    replace_line: bool,
) -> Result<YamlWrite, Diagnostic> {
    let scalar = doc
        .scalars()
        .get(k)
        .ok_or_else(|| err(ErrorCode::InvalidRequest, "intent names a missing key"))?;
    let shape = || {
        err(
            ErrorCode::UnsupportedSyntax,
            format!(
                "frontmatter key {} is not a single-line scalar or flat flow sequence; \
                 set_key_yaml replaces only those",
                scalar.key
            ),
        )
    };
    let field = &doc.source[scalar.line.start..scalar.line.end];
    let (first, rest) = field.split_once('\n').unwrap_or((field, ""));
    let first = first.strip_suffix('\r').unwrap_or(first);
    if !rest.split('\n').all(|l| {
        let l = l
            .strip_suffix('\r')
            .unwrap_or(l)
            .trim_start_matches([' ', '\t']);
        l.is_empty() || l.starts_with('#')
    }) {
        return Err(shape());
    }
    let old = yaml::key_line_token(first).ok_or_else(shape)?;
    if old.key != scalar.key || old.value != scalar.value {
        return Err(shape());
    }
    let expect = yaml::raw_token(raw).ok_or_else(|| unsupported_token(raw))?;
    let end = if replace_line {
        first.len()
    } else {
        old.token.end
    };
    let first_line = format!("{}{raw}{}", &first[..old.token.start], &first[end..]);
    let write = YamlWrite {
        splice: Splice {
            span: Span {
                start: scalar.line.start + old.token.start,
                end: scalar.line.start + end,
            },
            text: raw.to_string(),
        },
        expect,
        first_line,
        token_start: old.token.start,
        token: raw.to_string(),
    };
    yaml_reads_back(doc, &write, &scalar.key, Some(k))?;
    Ok(write)
}

/// The candidate payload must read, through the unchanged frontmatter
/// reader, as the same fields in the same order with only `key`'s value
/// replaced (or appended), and the key's first line must re-read with the
/// caller's token exactly where it was put. Refused as unsupported otherwise,
/// before any bytes change.
fn yaml_reads_back(
    doc: &Document,
    write: &YamlWrite,
    key: &str,
    k: Option<usize>,
) -> Result<(), Diagnostic> {
    let payload = doc
        .frontmatter
        .payload
        .ok_or_else(|| err(ErrorCode::UnsupportedSyntax, "frontmatter is absent"))?;
    let refuse = || {
        err(
            ErrorCode::UnsupportedSyntax,
            format!(
                "frontmatter key {key} would not read back as the given yaml token in place; \
                 the write is refused"
            ),
        )
    };
    let (candidate, fields) =
        payload_reads_back(doc, &write.splice, key, k, &write.expect, refuse)?;
    let field = fields.iter().find(|s| s.key == key).ok_or_else(refuse)?;
    let text = &candidate[field.line.start - payload.start..field.line.end - payload.start];
    let line = text.split('\n').next().unwrap_or(text);
    let line = line.strip_suffix('\r').unwrap_or(line);
    let read = yaml::key_line_token(line).ok_or_else(refuse)?;
    if line != write.first_line
        || read.value != write.expect
        || read.token.start != write.token_start
        || line[read.token.start..read.token.end] != write.token
    {
        return Err(refuse());
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Minted {
    pub id: String,
    /// `section`, `item` or `list`.
    pub kind: String,
    pub line: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Evidence {
    pub id: String,
    pub key: String,
    pub stored: String,
    pub refreshed: String,
    /// The stored digest was already stale before this write. Reported even
    /// when the write restores bytes that make the stored digest current.
    pub prior_drift: bool,
    /// Whether this write actually replaced the stored digest bytes.
    #[serde(skip)]
    pub rewritten: bool,
    /// Rewritten by a `refresh` op that named it, so not news to the caller.
    #[serde(skip)]
    pub requested: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct OpOutcome {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revision: Option<String>,
    /// For a write through a position handle: the selector that names the
    /// written item in the written file (`null` when none names it alone),
    /// computed from the candidate, never echoed from the request.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub select: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub addressing: Option<String>,
    /// For typed operations: the collection's revision after this write.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub collection_revision: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub minted: Vec<Minted>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub dropped: Vec<String>,
    /// Stamp targets refused by markstay §3.4 or lacking a carrier.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub refused: Vec<Diagnostic>,
    /// A `refresh` op's result for each stay it named.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub refreshed: Vec<Refreshed>,
}

#[derive(Debug, Clone)]
pub struct Outcome {
    pub candidate: String,
    pub ops: Vec<OpOutcome>,
    pub evidence: Vec<Evidence>,
}

/// A replacement of original bytes. Insertions have empty spans.
#[derive(Debug, Clone)]
pub struct Splice {
    pub span: Span,
    pub text: String,
}

mod carrier;
mod intent;
mod move_intent;
mod refresh;
pub(crate) use carrier::Carriers;
pub(crate) use intent::{Expectation, Guard, GuardSubject};
pub(crate) use move_intent::{plan as move_item, plan_section as move_section};
pub(crate) use refresh::unrefreshable;
pub use refresh::{RefreshSelect, Refreshed};

/// Computes a revision on the verified candidate, for guards returned to callers.
pub type Reviser = Box<dyn Fn(&Document) -> Option<String>>;

/// One operation's splices, the units it may change, and its postconditions.
#[derive(Default)]
pub struct Change {
    pub splices: Vec<Splice>,
    /// Blocks rewritten wholesale; not compared byte-for-byte.
    pub(crate) blocks: BTreeSet<usize>,
    /// Lists with individually rewritten items; other items are verified.
    pub(crate) lists: BTreeSet<usize>,
    /// Lists gaining or losing items. Exclusive to one operation per batch.
    pub(crate) reshaped: BTreeSet<usize>,
    pub(crate) items: BTreeSet<(usize, usize)>,
    /// Where new or rewritten items of existing lists land in the candidate.
    pub(crate) regions: Vec<ItemRegion>,
    pub(crate) frontmatter: bool,
    pub(crate) dropped: Vec<String>,
    /// Ids of existing markers this change copies into its own new text (an
    /// item's or a block's own stays moved past rewritten content). With
    /// `minted`, the only markers its splices may place.
    pub(crate) carried: Vec<String>,
    /// Only the confined archive copier supplies these proven source-owned stays.
    pub(crate) imported: Vec<(String, &'static str)>,
    pub(crate) minted: Vec<(String, &'static str, usize)>,
    /// Existing ids this change turns into an item's child stay (a
    /// hand-written marker adopted). Their digest is filled as a minted
    /// one's is, and they may change attachment kind, to `item` only.
    pub(crate) adopted: Vec<String>,
    pub(crate) refused: Vec<Diagnostic>,
    pub(crate) expectation: Option<Expectation>,
    pub(crate) guard: Option<Guard>,
    /// Where to find the target afterwards, for the returned revision.
    pub(crate) follow: Option<Follow>,
    /// For typed operations, the collection revision after the write.
    pub(crate) collection: Option<Reviser>,
    /// A `refresh` op's per-stay result, reported as planned.
    pub(crate) refreshed: Vec<Refreshed>,
}

/// A candidate item extent, stated in original coordinates.
#[derive(Debug, Clone, Copy)]
pub enum ItemRegion {
    /// `len` bytes at `offset` into the text of the splice replacing `splice`.
    Spliced {
        list: usize,
        splice: Span,
        offset: usize,
        len: usize,
    },
    /// An original item gaining `tail` inserted bytes at its very end (a
    /// stamped marker), with nothing else changed inside it.
    Mapped {
        list: usize,
        span: Span,
        tail: usize,
    },
    /// An original item changed only inside its own bytes, by same-length
    /// splices that move none of its markers (a ticked checkbox). Its stays
    /// are where they were, so §3.4 has no insertion to judge.
    InPlace { list: usize, span: Span },
}

impl ItemRegion {
    fn list(&self) -> usize {
        match self {
            ItemRegion::Spliced { list, .. }
            | ItemRegion::Mapped { list, .. }
            | ItemRegion::InPlace { list, .. } => *list,
        }
    }

    pub fn resolve(&self, map: &Mapper) -> Span {
        match *self {
            ItemRegion::Spliced {
                splice,
                offset,
                len,
                ..
            } => {
                let start = map.splice_start(splice) + offset;
                Span {
                    start,
                    end: start + len,
                }
            }
            ItemRegion::Mapped { span, tail, .. } => Span {
                start: map.map(span.start, true),
                end: map.map(span.end, false) + tail,
            },
            ItemRegion::InPlace { span, .. } => Span {
                start: map.map(span.start, true),
                end: map.map(span.end, false),
            },
        }
    }
}

pub enum Follow {
    Id(String),
    /// An unstamped section, found again by its heading's original offset.
    Section(usize),
    FrontmatterKey(String),
    /// The target's id plus how to compute the guard a follow-up write needs.
    Custom(String, Reviser),
    /// A write through a position handle: the item is found in the candidate
    /// by the extent it was written to, and reported with the handle and
    /// addressing a read of the candidate would give it. With a binding, the
    /// returned guard is its typed revision; otherwise the native one.
    Placed {
        region: ItemRegion,
        typed: Option<Box<crate::schema::Bound>>,
    },
}

/// The candidate item a write's region lands on, by exact extent.
pub(crate) fn candidate_item(
    c: &Document,
    map: &Mapper,
    region: &ItemRegion,
) -> Option<(usize, usize)> {
    let span = region.resolve(map);
    c.lists
        .iter()
        .enumerate()
        .find_map(|(l, x)| x.items.iter().position(|i| i.span == span).map(|i| (l, i)))
}

/// Maps original offsets to candidate offsets across sorted splices.
pub struct Mapper {
    splices: Vec<(Span, usize)>,
}

impl Mapper {
    fn new(splices: &[Splice]) -> Self {
        Mapper {
            splices: splices.iter().map(|s| (s.span, s.text.len())).collect(),
        }
    }

    /// Map an original offset. `after_inserts` places the point after any
    /// insertion at exactly `pos` (a unit start); otherwise before it (a unit end).
    pub fn map(&self, pos: usize, after_inserts: bool) -> usize {
        let mut out = pos as isize;
        for (span, len) in &self.splices {
            let counts = span.end < pos || (span.end == pos && (after_inserts || !span.is_empty()));
            if counts {
                out += *len as isize - span.len() as isize;
            }
        }
        out as usize
    }

    /// Candidate offset where a splice's own text begins. Insertions sort
    /// before a replacement at the same offset, so a replacement follows them.
    pub fn splice_start(&self, span: Span) -> usize {
        self.map(span.start, !span.is_empty())
    }
}

fn err(code: ErrorCode, message: impl Into<String>) -> Diagnostic {
    Diagnostic::new(code, message)
}

/// Random ids from the OS, unique within the document (§6).
pub fn mint_id(taken: &BTreeSet<String>) -> String {
    loop {
        let id = markstay::mint_id(
            markstay::DEFAULT_ID_LENGTH,
            markstay::DEFAULT_ALPHABET,
            |n| {
                let mut buf = vec![0u8; n];
                getrandom(&mut buf);
                buf
            },
        );
        if !taken.contains(&id) {
            return id;
        }
    }
}

fn getrandom(buf: &mut [u8]) {
    // Debug builds only: `RIVET_TEST_ID_SEED` makes minted ids reproducible
    // for benchmark corpora. Release builds always use the OS source.
    if cfg!(debug_assertions)
        && let Ok(seed) = std::env::var("RIVET_TEST_ID_SEED")
    {
        use sha2::{Digest, Sha256};
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        for chunk in buf.chunks_mut(32) {
            let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let digest = Sha256::digest(format!("{seed}:{n}"));
            chunk.copy_from_slice(&digest[..chunk.len()]);
        }
        return;
    }
    use std::io::Read;
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(buf))
        .expect("the OS random source is readable");
}

pub(crate) fn check_write_blocker(doc: &Document) -> Result<(), Diagnostic> {
    doc.write_blocker
        .as_ref()
        .map_or(Ok(()), |d| Err(d.clone()))
}

pub fn check_guard(doc: &Document, target: Target, guard: &str) -> Result<(), Diagnostic> {
    Guard::new(GuardSubject::Native(target), guard).check(doc)
}

/// Content supplied by a caller: LF or CRLF, never markers or lone CR.
///
/// Marker-shaped text wholly inside code (a code span, a fenced or indented
/// code block) is content, the way the document model reads it once written,
/// so an item that quotes a marker can still be edited. Anything else that
/// looks like one, whole or partial, is refused: the write would place or
/// garble an identity, and whole-candidate verification refusing it later
/// would say less about why.
fn clean_content(text: &str, newline: &str) -> Result<String, Diagnostic> {
    let lf = text.replace("\r\n", "\n");
    if lf.contains('\r') {
        return Err(err(ErrorCode::InvalidContent, "content contains a lone CR"));
    }
    if marker_outside_code(&lf) {
        return Err(err(
            ErrorCode::InvalidContent,
            "content must not contain stay markers outside code; Rivet places and preserves them",
        ));
    }
    Ok(lf.replace('\n', newline))
}

/// The content of a document Rivet creates: non-empty, LF or CRLF with no
/// lone CR, and no stay markers outside code (`stamp` mints them).
pub fn check_new_document(text: &str) -> Result<Document, Diagnostic> {
    if text.trim().is_empty() {
        return Err(err(ErrorCode::InvalidContent, "a document needs content"));
    }
    if text.replace("\r\n", "").contains('\r') {
        return Err(err(ErrorCode::InvalidContent, "content contains a lone CR"));
    }
    if marker_outside_code(text) {
        return Err(err(
            ErrorCode::InvalidContent,
            "content must not contain stay markers outside code; pass stamp to mint them",
        ));
    }
    let doc = Document::parse(text)?;
    check_write_blocker(&doc)?;
    Ok(doc)
}

/// Whether `text` carries a marker, or the opening of one, anywhere but
/// inside code. Liveness is the document model's own reading of the content,
/// so code blocks get the same extents a written file would; an opening that
/// no markstay reader would take for a marker is content inside a code span or
/// a code block's lines, and refused anywhere else.
fn marker_outside_code(text: &str) -> bool {
    let Ok(doc) = Document::parse(text) else {
        return true;
    };
    if !doc.markers.is_empty() || doc.write_blocker.is_some() {
        return true;
    }
    // Content is spliced into a document, never at its start, so what reads
    // as frontmatter here is Markdown there (a thematic break and a setext
    // heading) and a marker inside it would be live once written.
    if let Some(span) = doc.frontmatter.span
        && markers::contains_opening(&text[span.start..span.end])
    {
        return true;
    }
    let Ok(lexical) = markers::scan(text) else {
        return true;
    };
    let loose: Vec<usize> = markers::openings(text)
        .filter(|&i| !lexical.iter().any(|m| m.span.start <= i && i < m.span.end))
        .collect();
    if loose.is_empty() {
        return false;
    }
    let spans = markdown::code_extents(text);
    loose.iter().any(|&i| {
        !spans
            .iter()
            .any(|c| c.start <= i && i + markers::OPENING.len() <= c.end)
    })
}

/// Text before a span that is only indentation, or a list marker with an
/// optional task box: the span's lines join rather than break before it.
fn marker_only(head: &str) -> bool {
    let s = head.trim_start_matches([' ', '\t']);
    if s.is_empty() {
        return true;
    }
    let rest = match s.strip_prefix(['-', '*', '+']) {
        Some(r) => r,
        None => {
            let digits = s.bytes().take_while(u8::is_ascii_digit).count();
            if !(1..=9).contains(&digits) {
                return false;
            }
            match s[digits..].strip_prefix(['.', ')']) {
                Some(r) => r,
                None => return false,
            }
        }
    };
    if rest.is_empty() {
        return true;
    }
    let gap = rest.trim_start_matches([' ', '\t']);
    gap.len() < rest.len() && matches!(gap, "" | "[ ]" | "[x]" | "[X]")
}

fn trim_end_block(text: &str) -> &str {
    text.trim_end_matches([' ', '\t', '\r', '\n'])
}

/// First-line prefix plus continuation indentation of `width` columns, except
/// where `kept` supplies a line's indentation from the source it repeats.
fn render_item(
    prefix: &str,
    width: usize,
    content: &str,
    newline: &str,
    kept: &[Option<&str>],
) -> String {
    let mut out = String::from(prefix);
    for (n, line) in content.split(newline).enumerate() {
        if n > 0 {
            out.push_str(newline);
            match kept.get(n).copied().flatten() {
                Some(indent) => out.push_str(indent),
                None if !line.is_empty() => out.push_str(&" ".repeat(width)),
                None => {}
            }
        }
        out.push_str(line);
    }
    out
}

/// For each line of `new`, the indentation removed from the source
/// continuation line it repeats, so a line the write did not change is
/// rewritten with its own bytes rather than the item's width. Lines are paired
/// by longest common subsequence, which keeps an unchanged line between two
/// changed ones as well as a run at either end. The first line is never
/// paired: its indentation is the item's prefix.
fn kept_indentation<'a>(old: &'a [(String, String)], new: &[&str]) -> Vec<Option<&'a str>> {
    let mut kept = vec![None; new.len()];
    let (a, b) = (old.get(1..).unwrap_or(&[]), new.get(1..).unwrap_or(&[]));
    let mut table = vec![vec![0usize; b.len() + 1]; a.len() + 1];
    for i in (0..a.len()).rev() {
        for j in (0..b.len()).rev() {
            table[i][j] = if a[i].1 == b[j] {
                table[i + 1][j + 1] + 1
            } else {
                table[i + 1][j].max(table[i][j + 1])
            };
        }
    }
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        if a[i].1 == b[j] {
            kept[j + 1] = Some(a[i].0.as_str());
            i += 1;
            j += 1;
        } else if table[i + 1][j] >= table[i][j + 1] {
            i += 1;
        } else {
            j += 1;
        }
    }
    kept
}

/// Whether a rendered item parses on its own as one item holding `content`.
fn reads_back(rendered: &str, content: &str, newline: &str) -> bool {
    let Ok(doc) = Document::parse(rendered) else {
        return false;
    };
    let items: Vec<_> = doc
        .lists
        .iter()
        .enumerate()
        .flat_map(|(l, list)| (0..list.items.len()).map(move |i| (l, i)))
        .filter(|&(l, i)| doc.lists[l].items[i].prefix.start == 0)
        .collect();
    let [(list, item)] = items.as_slice() else {
        return false;
    };
    doc.lists[*list].items[*item].span.end == rendered.len()
        && doc.item_content(*list, *item) == content.replace(newline, "\n")
}

/// `(1-based line number, line, byte offset)`, splitting on CRLF, bare CR and
/// LF. `markstay::code_lines` normalises all three, so a scan that split on LF
/// alone would number its lines differently: a fence that function sees would
/// be invisible here, and the backticks either side of it would pair across it.
fn lines_with_offsets(text: &str) -> Vec<(usize, &str, usize)> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let (mut at, mut number, mut i) = (0usize, 1usize, 0usize);
    while i < bytes.len() {
        let width = match bytes[i] {
            b'\r' if bytes.get(i + 1) == Some(&b'\n') => 2,
            b'\r' | b'\n' => 1,
            _ => {
                i += 1;
                continue;
            }
        };
        out.push((number, &text[at..i], at));
        number += 1;
        i += width;
        at = i;
    }
    out.push((number, &text[at..], at));
    out
}

/// Closed inline code spans in `text`, as `[start, end)` byte spans over their
/// content (markstay §3.4, v1.8).
///
/// CommonMark pairs a backtick run with the next run of exactly the same length
/// (6.1). A run with no equal-length run after it opens nothing that closes
/// inside this text, so it yields no span and whatever follows stays visible.
///
/// `code` is §3.3's fenced-code line set. A backtick run on one of those lines
/// is a fence delimiter or fence content, never an inline-span delimiter, so it
/// is dropped before pairing; without that a fence's opening and closing rows
/// pair with each other and mask the block between them, hiding a live `<` that
/// §3.3 keeps as content. Only the backticks' ability to pair is dropped, never
/// the characters: §3.4 still sees every capturing character the fence holds.
///
/// **Pairing is per line, and it stops at the first line it cannot balance.**
/// Per line, because a carrier text is the *container's* prefix, which for a
/// list spans earlier items: two backticks in different blocks pair for a
/// lexical scan but not for a renderer. Stopping, because per-line pairing is
/// not a subset of CommonMark's: CommonMark pairs sequentially across a whole
/// paragraph, so one leftover run on an earlier line takes the next line's
/// first run as its closer and shifts every pairing after it. A line whose runs
/// do not pair evenly therefore ends the scan; spans already found on earlier
/// lines stand, because while nothing was left over the two scans agree.
fn code_spans(text: &str) -> Vec<Span> {
    let code = markstay::code_lines(text);
    let mut spans = Vec::new();
    for (number, line, offset) in lines_with_offsets(text) {
        if code.contains(&number) {
            continue;
        }
        let mut runs: Vec<Span> = Vec::new();
        let bytes = line.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == b'`' {
                let start = i;
                while i < bytes.len() && bytes[i] == b'`' {
                    i += 1;
                }
                runs.push(Span {
                    start: start + offset,
                    end: i + offset,
                });
            } else {
                i += 1;
            }
        }
        let mut found = Vec::new();
        let mut a = 0;
        while a < runs.len() {
            let width = runs[a].end - runs[a].start;
            let mut b = a + 1;
            while b < runs.len() && runs[b].end - runs[b].start != width {
                b += 1;
            }
            if b == runs.len() {
                // A run with no partner on its own line. CommonMark will close
                // it from a later line and re-pair everything after it, so this
                // line and every line below it are ambiguous.
                return spans;
            }
            found.push(Span {
                start: runs[a].end,
                end: runs[b].start,
            });
            a = b + 1;
        }
        spans.extend(found);
    }
    spans
}

/// `text` with the content of closed code spans replaced by spaces.
///
/// Masked rather than deleted so every byte keeps its offset. The backticks
/// themselves stay, since they are not characters this scan refuses on.
fn mask_code_spans(text: &str) -> String {
    let mut bytes = text.as_bytes().to_vec();
    for span in code_spans(text) {
        for b in &mut bytes[span.start..span.end] {
            *b = b' ';
        }
    }
    // Every replaced byte becomes an ASCII space, so a multi-byte character
    // inside a span becomes that many spaces: the offsets and the answer are
    // what this is for, not the masked text's own character count.
    String::from_utf8(bytes).expect("masking writes ASCII spaces only")
}

/// markstay §3.4 carrier check over a raw container prefix. Plain live markers
/// (relative spans) are masked first, then the content of closed inline code
/// spans (v1.8); `<` or `\` in what is left refuses.
///
/// Rivet mints only §5.5 child carriers, never §5.6's row carrier, so the
/// code-span mask applies at every carrier it writes. A row carrier keeps the
/// unmasked presence rule, because GFM splits cells before inline parsing and a
/// lexical backtick scan pairs across a `|` where a renderer does not.
/// The carrier text as §3.4 scans it: plain markers masked, then the content of
/// closed inline code spans. Exposed so the conformance vectors can check the
/// masked text against markstay's, not only the accept/refuse verdict.
pub fn mask_for_carrier(prefix: &str, plain: &[Span]) -> String {
    mask_code_spans(&markers::mask_plain(prefix, plain))
}

/// The first character of a carrier that §3.4 refuses, and where it sits.
#[derive(Debug, Clone, Copy)]
pub struct CarrierRefusal {
    pub reason: &'static str,
    /// Byte offset of the offending character within the scanned prefix.
    /// Masking replaces bytes one for one, so this is also its offset in the
    /// unmasked prefix.
    pub at: usize,
    pub character: char,
}

/// Reports the *first* offender rather than always preferring `<`, so the
/// position and the reason describe the same character. The verdict is
/// unaffected: either character refuses.
pub fn carrier_refusal(prefix: &str, plain: &[Span]) -> Option<CarrierRefusal> {
    let masked = mask_for_carrier(prefix, plain);
    // A byte scan is exact here: masking writes ASCII spaces, and neither
    // offender's byte can occur inside a multi-byte character.
    let at = masked.bytes().position(|b| b == b'<' || b == b'\\')?;
    let character = masked.as_bytes()[at] as char;
    Some(CarrierRefusal::new(at, character))
}

impl CarrierRefusal {
    /// The refusal for offender `character` at byte `at` of the scanned prefix.
    fn new(at: usize, character: char) -> Self {
        CarrierRefusal {
            reason: if character == '<' {
                "the list's source before this item carries '<' (markstay §3.4)"
            } else {
                "the list's source before this item carries a backslash (markstay §3.4)"
            },
            at,
            character,
        }
    }
}

pub(crate) fn plain_markers_in(doc: &Document, span: Span) -> Vec<Span> {
    doc.markers
        .iter()
        .filter(|m| {
            m.lexical.plain && span.start <= m.lexical.span.start && m.lexical.span.end <= span.end
        })
        .map(|m| Span {
            start: m.lexical.span.start - span.start,
            end: m.lexical.span.end - span.start,
        })
        .collect()
}

fn foreign_markers(doc: &Document, span: Span, own: &[usize]) -> Vec<usize> {
    doc.markers
        .iter()
        .enumerate()
        .filter(|(i, m)| {
            !own.contains(i) && span.start <= m.lexical.span.start && m.lexical.span.end <= span.end
        })
        .map(|(i, _)| i)
        .collect()
}

/// Source-only prerequisites of set_item; replacement validation stays in the planner.
pub(crate) fn rewrite_prerequisite(
    doc: &Document,
    list: usize,
    item: usize,
) -> Result<(), Diagnostic> {
    let it = &doc.lists[list].items[item];
    intent::preserve_foreign(
        doc,
        it.span,
        &it.markers,
        "the item holds a marker owned by another unit; edit refused",
        true,
    )?;
    for &m in &it.markers {
        if !doc.markers[m].lexical.plain {
            return Err(err(
                ErrorCode::UnsupportedSyntax,
                "the item's stay carries attributes beyond id and digest; moving it is refused",
            )
            .with_span(doc.markers[m].lexical.span));
        }
    }
    Ok(())
}

/// Exact existing stamp placement checks. Does not mint or move any marker.
/// `carriers` indexes the document being stamped.
pub(crate) fn stamp_item_carrier(
    carriers: &Carriers,
    list: usize,
    item: usize,
) -> Result<usize, Diagnostic> {
    let doc = carriers.doc();
    let it = &doc.lists[list].items[item];
    let at = it.carrier.ok_or_else(|| {
        err(
            ErrorCode::StampRefused,
            "item has no paragraph to carry a stay",
        )
        .with_line(doc.file_line(it.span.start))
    })?;
    current_carrier(carriers, list, at)
        .map_or(Ok(at), |d| Err(d.with_line(doc.file_line(it.span.start))))
}

/// A hand-written stay without a digest, typed into an unstamped item's own
/// content, reads as the list's but already addresses the item for whoever
/// cites it. Stamping beside it would give the item a second id, so stamp
/// leaves the item to `adopt_stay`, which keeps the id. Returns the skip and
/// every id it covers. Shared with doctor. `carriers` indexes the document.
pub(crate) fn hand_written_stay(
    carriers: &Carriers,
    list: usize,
    item: usize,
) -> Option<(Diagnostic, Vec<String>)> {
    let doc = carriers.doc();
    let it = &doc.lists[list].items[item];
    let stays: Vec<(&str, &markers::Lexical)> = doc.blocks[doc.lists[list].block]
        .markers
        .iter()
        .map(|&m| &doc.markers[m].lexical)
        .filter(|x| {
            let line = doc.line_of(x.span.start);
            x.hash.is_none()
                && it.span.start <= x.span.start
                && x.span.end <= it.span.end
                && !it.nested.iter().any(|&(a, z)| a <= line && line <= z)
        })
        .filter_map(|x| Some((x.id.as_deref()?, x)))
        .collect();
    let ids: Vec<String> = stays.iter().map(|(id, _)| id.to_string()).collect();
    let d = match stays.as_slice() {
        [] => return None,
        [(id, x)] => {
            let repairs: Vec<String> = adoption_blockers(carriers, list, id, x, &[])
                .into_iter()
                .map(|b| b.repair)
                .collect();
            let then = if repairs.is_empty() {
                String::new()
            } else {
                format!("{} by hand, then ", repairs.join(" and "))
            };
            err(
                ErrorCode::StampRefused,
                format!(
                    "item carries hand-written stay {id}; {then}adopt it with adopt_stay \
                     rather than stamping a second id"
                ),
            )
            .with_id(*id)
        }
        _ => {
            let mut message = format!(
                "item carries hand-written stays {}; keep one, drop the rest by hand, \
                 then adopt it with adopt_stay",
                ids.join(", ")
            );
            for (id, x) in &stays {
                let dropped: Vec<Span> = stays
                    .iter()
                    .filter(|(_, y)| y.span != x.span)
                    .map(|(_, y)| y.span)
                    .collect();
                let repairs: Vec<String> = adoption_blockers(carriers, list, id, x, &dropped)
                    .into_iter()
                    .map(|b| b.repair)
                    .collect();
                if !repairs.is_empty() {
                    message.push_str(&format!("; to keep {id}, first {}", repairs.join(" and ")));
                }
            }
            err(ErrorCode::StampRefused, message)
        }
    };
    Some((d.with_line(doc.file_line(it.span.start)), ids))
}

struct AdoptionBlocker {
    diagnostic: Diagnostic,
    repair: String,
}

fn adoption_others(doc: &Document, id: &str, x: &markers::Lexical, dropped: &[Span]) -> usize {
    doc.markers
        .iter()
        .filter(|m| {
            m.lexical.id.as_deref() == Some(id)
                && m.lexical.span != x.span
                && !dropped.contains(&m.lexical.span)
        })
        .count()
}

/// Locate the source unit before guard validation, preserving selector diagnostics.
fn adoption_target(doc: &Document, id: &str) -> Result<(Target, usize, usize, usize), Diagnostic> {
    let marker = doc
        .markers
        .iter()
        .position(|m| m.lexical.id.as_deref() == Some(id))
        .ok_or_else(|| {
            err(
                ErrorCode::MissingSelector,
                format!("0 markers carry {id}; adoption needs exactly one"),
            )
        })?;
    let m = &doc.markers[marker];
    let n = adoption_others(doc, id, &m.lexical, &[]);
    if n > 0 {
        return Err(err(
            ErrorCode::AmbiguousSelector,
            format!("{} markers carry {id}; adoption needs exactly one", n + 1),
        ));
    }
    let Attach::Block(b) = m.attach else {
        return Err(err(
            ErrorCode::InvalidRequest,
            format!("stay {id} is not a hand-written block stay"),
        ));
    };
    let list = doc.blocks[b].list.ok_or_else(|| {
        err(
            ErrorCode::InvalidRequest,
            format!("stay {id} is not inside a list"),
        )
    })?;
    let item = doc.lists[list]
        .items
        .iter()
        .position(|it| it.span.start <= m.lexical.span.start && m.lexical.span.end <= it.span.end)
        .ok_or_else(|| {
            err(
                ErrorCode::InvalidRequest,
                format!("stay {id} is the list's own marker, not inside an item"),
            )
        })?;
    Ok((Target::Block(b), list, item, marker))
}

/// What has to change by hand before `adopt_stay` takes stay `x` of this
/// item, once the item's other hand-written stays are gone: its attributes,
/// other carriers of its id (any other marker, wherever it attaches, or a
/// heading's `{#id}`), and §3.4 carrier text before it in the list.
fn adoption_blockers(
    carriers: &Carriers,
    list: usize,
    id: &str,
    x: &markers::Lexical,
    dropped: &[Span],
) -> Vec<AdoptionBlocker> {
    let doc = carriers.doc();
    let mut repairs = vec![];
    if !x.plain || x.hash.is_some() || x.has_subhash || x.malformed {
        repairs.push(AdoptionBlocker {
            repair: format!("reduce it to `{}`", markers::format_bare(id)),
            diagnostic: err(
                ErrorCode::UnsupportedSyntax,
                format!(
                    "stay {id} carries attributes; only a bare `{}` is adopted",
                    markers::format_bare(id)
                ),
            ),
        });
    }
    let elsewhere = adoption_others(doc, id, x, dropped);
    if elsewhere > 0 {
        repairs.push(AdoptionBlocker {
            repair: match elsewhere {
                1 => format!("drop the other marker carrying {id}"),
                n => format!("drop the other {n} markers carrying {id}"),
            },
            diagnostic: err(
                ErrorCode::AmbiguousSelector,
                format!(
                    "{} markers carry {id}; adoption needs exactly one",
                    elsewhere + 1
                ),
            ),
        });
    }
    if doc
        .blocks
        .iter()
        .any(|b| b.heading.as_ref().and_then(|h| h.legacy_id.as_deref()) == Some(id))
    {
        repairs.push(AdoptionBlocker {
            repair: format!("rename the heading `{{#{id}}}`"),
            diagnostic: err(
                ErrorCode::InvalidContent,
                format!("stay {id} is also a heading id; rename the heading before adoption"),
            ),
        });
    }
    if let Some(item) = doc.lists[list]
        .items
        .iter()
        .find(|it| it.span.start <= x.span.start && x.span.end <= it.span.end)
    {
        if !item.markers.is_empty() {
            repairs.push(AdoptionBlocker {
                repair: format!("drop {id}"),
                diagnostic: err(
                    ErrorCode::UnsupportedSyntax,
                    format!("the item already has Rivet's stay; drop {id} by hand"),
                ),
            });
        }
        if item
            .nested
            .iter()
            .any(|&(a, z)| a <= doc.line_of(x.span.start) && doc.line_of(x.span.start) <= z)
        {
            repairs.push(AdoptionBlocker {
                repair: format!("move {id} out of the nested item"),
                diagnostic: err(
                    ErrorCode::InvalidRequest,
                    format!("stay {id} sits in a nested item"),
                ),
            });
        }
    }
    // The carrier as it reads once the item's other hand-written stays are
    // deleted: removed, not masked, since deleting one can join the backtick
    // runs on either side of it. Plain markers left behind are masked.
    let start = doc.blocks[doc.lists[list].block].span.start;
    let prefix = Span {
        start,
        end: x.span.start,
    };
    let mut gone: Vec<Span> = dropped
        .iter()
        .copied()
        .filter(|d| prefix.start <= d.start && d.end <= prefix.end)
        .collect();
    gone.sort_by_key(|d| d.start);
    let shift = |at: usize| -> usize {
        gone.iter()
            .filter(|d| d.end <= at)
            .map(|d| d.end - d.start)
            .sum()
    };
    // With nothing cut out, the text is the list's own prefix and its plain
    // markers are `plain_markers_in`'s, which is exactly the list index's query.
    let refusal = if gone.is_empty() {
        carriers.refusal(list, prefix.end)
    } else {
        let mut text = String::new();
        let mut at = prefix.start;
        for d in &gone {
            text.push_str(&doc.source[at..d.start]);
            at = d.end;
        }
        text.push_str(&doc.source[at..prefix.end]);
        let masked: Vec<Span> = plain_markers_in(doc, prefix)
            .into_iter()
            .map(|m| Span {
                start: m.start + start,
                end: m.end + start,
            })
            .filter(|m| !gone.contains(m))
            .map(|m| Span {
                start: m.start - start - shift(m.start),
                end: m.end - start - shift(m.start),
            })
            .collect();
        carrier_refusal(&text, &masked)
    };
    if let Some(r) = refusal {
        repairs.push(AdoptionBlocker {
            repair:format!("clear its carrier ({})", r.reason),
            diagnostic:err(ErrorCode::StampRefused,format!("stay {id} would sit behind non-plain carrier text: {}; remove '<' or '\\' from the list before it",r.reason)).with_id(id),
        });
    }
    repairs
}

/// Located carrier refusal over original bytes, shared by stamping and doctor:
/// §3.4 over the list's raw source up to `end`, answered by the list's index.
pub(crate) fn current_carrier(carriers: &Carriers, list: usize, end: usize) -> Option<Diagnostic> {
    let doc = carriers.doc();
    let block = &doc.blocks[doc.lists[list].block];
    let prefix = Span {
        start: block.span.start,
        end,
    };
    let r = carriers.refusal(list, prefix.end)?;
    Some(
        err(ErrorCode::StampRefused, r.reason).with_blocker(CarrierBlocker::new(
            &doc.source,
            prefix.start + r.at,
            r.character,
            doc.file_line(block.span.start),
            doc.stay_ids(&block.markers).into_iter().next(),
        )),
    )
}

/// Every identity a region holds: the id of every marker in it, whatever it
/// attaches to, and the `{#id}` of any heading in it. Replacing the region
/// drops all of them. A marker Rivet does not address (a nested item's, a
/// table row's, a marker-only line carrying `subhash`) counts too: markstay's
/// lexical drop check sees its id leave the document, so the write has to
/// declare it as `set_block` does.
pub(crate) fn ids_in(doc: &Document, span: Span) -> Vec<String> {
    let inside = |m: &Marker| span.start <= m.lexical.span.start && m.lexical.span.end <= span.end;
    let mut ids: Vec<String> = Vec::new();
    for m in &doc.markers {
        let Some(id) = &m.lexical.id else { continue };
        if !inside(m) || ids.contains(id) {
            continue;
        }
        // An unaddressed id that another marker outside the region still
        // carries does not leave the document, so it is not a loss to declare.
        let addressed = matches!(m.attach, Attach::Block(_) | Attach::Item { .. });
        if !addressed
            && doc
                .markers
                .iter()
                .any(|o| !inside(o) && o.lexical.id.as_ref() == Some(id))
        {
            continue;
        }
        ids.push(id.clone());
    }
    for (i, section) in doc.sections.iter().enumerate() {
        if span.contains(doc.blocks[section.block].span.start) {
            for id in doc.section_ids(i) {
                if !ids.contains(&id) {
                    ids.push(id);
                }
            }
        }
    }
    ids
}

/// Separator between two adjacent items: a blank line when the source has one.
pub(crate) fn item_edge(doc: &Document, list: usize, position: Position) -> (usize, String) {
    let l = &doc.lists[list];
    let (at, gap) = match position {
        Position::Start => (l.items[0].span.start, 0),
        Position::End => (
            l.items.last().expect("parsed list").span.end,
            l.items.len().saturating_sub(2),
        ),
    };
    (
        at,
        if item_gap(doc, list, gap) {
            format!("{0}{0}", doc.newline)
        } else {
            doc.newline.to_string()
        },
    )
}

pub(crate) fn new_list_frame(
    doc: &Document,
    section: usize,
    raw: &str,
    list_id: &str,
) -> (Splice, usize) {
    let nl = doc.newline;
    let at = doc.sections[section].own_end;
    let before = &doc.source[..at];
    let mut text = String::new();
    if !before.ends_with('\n') {
        text.push_str(nl);
    }
    let last = before.strip_suffix('\n').unwrap_or(before);
    let last = last.strip_suffix('\r').unwrap_or(last);
    if !last[last.rfind('\n').map_or(0, |i| i + 1)..]
        .trim()
        .is_empty()
    {
        text.push_str(nl);
    }
    let offset = text.len();
    text.push_str(raw);
    text.push_str(nl);
    text.push_str(&markers::format_plain(list_id, "hash", PLACEHOLDER));
    text.push_str(nl);
    if at < doc.source.len() {
        text.push_str(nl);
    }
    (
        Splice {
            span: Span { start: at, end: at },
            text,
        },
        offset,
    )
}

/// Separator between two adjacent items: a blank line when the source has one.
fn item_gap(doc: &Document, list: usize, a: usize) -> bool {
    let items = &doc.lists[list].items;
    match (items.get(a), items.get(a + 1)) {
        (Some(x), Some(y)) => doc.source[x.span.end..y.span.start].matches('\n').count() > 1,
        _ => !doc.lists[list].tight && items.len() > 1,
    }
}

fn numbered_prefix(prefix: &str, number: u64) -> Option<String> {
    let indent = prefix.len() - prefix.trim_start_matches(' ').len();
    let rest = &prefix[indent..];
    let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
    if digits == 0 {
        return None;
    }
    let text = number.to_string();
    (text.len() <= 9).then(|| format!("{}{}{}", &prefix[..indent], text, &rest[digits..]))
}

fn prefix_number(prefix: &str) -> Option<u64> {
    prefix
        .trim_start_matches(' ')
        .bytes()
        .take_while(u8::is_ascii_digit)
        .try_fold(0u64, |n, d| {
            n.checked_mul(10)?.checked_add(u64::from(d - b'0'))
        })
}

fn columns(s: &str) -> usize {
    s.chars().fold(0, |col, c| {
        if c == '\t' {
            col + 4 - col % 4
        } else {
            col + 1
        }
    })
}

/// `text` with every `sha256:` digest value emptied, for comparing content a
/// write may have refreshed evidence in.
pub(crate) fn blank_digests(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(i) = rest.find("sha256:") {
        out.push_str(&rest[..i + 7]);
        rest = rest[i + 7..].trim_start_matches(|c: char| c.is_ascii_hexdigit());
    }
    out.push_str(rest);
    out
}

pub struct Planner<'a> {
    pub doc: &'a Document,
    pub taken: BTreeSet<String>,
}

impl<'a> Planner<'a> {
    pub fn new(doc: &'a Document) -> Self {
        let mut taken: BTreeSet<String> = doc.ids.keys().cloned().collect();
        taken.extend(doc.markers.iter().filter_map(|m| m.lexical.id.clone()));
        taken.extend(crate::citations::identity_claims(doc).map(|(id, _)| id.to_string()));
        Planner { doc, taken }
    }

    pub fn mint(&mut self) -> String {
        let id = mint_id(&self.taken);
        self.taken.insert(id.clone());
        id
    }

    pub fn structural(&mut self, op: &Op) -> Result<Change, Diagnostic> {
        let doc = self.doc;
        check_write_blocker(doc)?;
        let (target, supplied) = match op {
            Op::Set {
                collection: Some(_),
                ..
            }
            | Op::Set {
                typed_guard: Some(_),
                ..
            }
            | Op::Stamp {
                collection: Some(_),
                ..
            }
            | Op::Stamp {
                checkbox_only: true,
                ..
            }
            | Op::Stamp {
                collection_guard: Some(_),
                ..
            }
            | Op::Delete {
                collection: Some(_),
                ..
            } => {
                return Err(err(
                    ErrorCode::InvalidRequest,
                    if matches!(op, Op::Delete { .. }) {
                        "a delete collection precondition needs a project schema context"
                    } else {
                        "collection preconditions need a project schema context"
                    },
                ));
            }
            Op::Set { select, guard, .. }
            | Op::Insert { select, guard, .. }
            | Op::Delete { select, guard, .. }
            | Op::Append { select, guard, .. }
            | Op::ReplaceInSection { select, guard, .. }
            | Op::RemoveParagraphInSection { select, guard, .. }
            | Op::Rejoin { select, guard, .. }
            | Op::Stamp { select, guard, .. } => (doc.resolve(select)?, guard),
            Op::AddKey { guard, .. } | Op::AddKeyYaml { guard, .. } => (Target::Document, guard),
            Op::SetKeyYaml { key, guard, .. } => (
                doc.resolve(&Selector::FrontmatterKey { key: key.clone() })?,
                guard,
            ),
            Op::SetKeyField {
                key_path, guard, ..
            } => {
                let [key, _] = key_path.as_slice() else {
                    return Err(err(
                        ErrorCode::InvalidRequest,
                        "key_path must contain exactly two literal keys: parent and field",
                    ));
                };
                (
                    doc.resolve(&Selector::FrontmatterKey { key: key.clone() })?,
                    guard,
                )
            }
            Op::DeleteKey { key, guard } => (
                doc.resolve(&Selector::FrontmatterKey { key: key.clone() })?,
                guard,
            ),
            // The key's own guard when it exists, the document's when the
            // write adds it: a caller's guard of the other kind is stale.
            Op::AppendKeyItem { key, guard, .. } => {
                let fields = doc.frontmatter.fields.as_ref().map_err(Clone::clone)?;
                let target = fields
                    .iter()
                    .position(|s| &s.key == key)
                    .map_or(Target::Document, Target::FrontmatterKey);
                (target, guard)
            }
            Op::AdoptStay { id, guard } => (adoption_target(doc, id)?.0, guard),
            Op::Refresh { guard, .. } => (Target::Document, guard),
            _ => {
                return Err(err(
                    ErrorCode::InvalidRequest,
                    "typed operations need a project schema context",
                ));
            }
        };
        let positional = matches!(
            op,
            Op::Set {
                select: Selector::Position { .. },
                ..
            }
        );
        let mut intent_guard = Guard::new(GuardSubject::Native(target), supplied);
        if positional {
            intent_guard = intent_guard.at_position();
        }
        intent_guard.check(doc)?;
        let mut change = match op {
            Op::Set {
                select: _,
                part,
                guard: _,
                content,
                allow_drop,
                ..
            } => match (target, part) {
                (Target::Item { list, item }, None) => {
                    let text = content.as_str().ok_or_else(|| {
                        err(ErrorCode::InvalidRequest, "item content must be a string")
                    })?;
                    self.set_item(list, item, text)
                }
                (Target::Section(s), Some(SectionPart::Title)) => {
                    let text = content.as_str().ok_or_else(|| {
                        err(ErrorCode::InvalidRequest, "a title must be a string")
                    })?;
                    self.set_title(s, text)
                }
                (Target::Section(s), Some(SectionPart::Body)) => {
                    let text = content.as_str().ok_or_else(|| {
                        err(ErrorCode::InvalidRequest, "section body must be a string")
                    })?;
                    self.set_body(s, text, *allow_drop)
                }
                (Target::Section(_), None) => Err(err(
                    ErrorCode::InvalidRequest,
                    "setting a section needs part: title or body",
                )),
                (Target::Block(b), None) => {
                    let text = content.as_str().ok_or_else(|| {
                        err(ErrorCode::InvalidRequest, "block content must be a string")
                    })?;
                    self.set_block(b, text, *allow_drop)
                }
                (Target::FrontmatterKey(k), None) => self.set_frontmatter(k, content),
                (Target::Document, _) => Err(err(
                    ErrorCode::InvalidRequest,
                    "replacing a whole document is not an operation",
                )),
                (_, Some(_)) => Err(err(
                    ErrorCode::InvalidRequest,
                    "part applies only to sections",
                )),
            },
            Op::Insert {
                select: _,
                position,
                guard: _,
                content,
                stamp,
            } => match target {
                Target::Block(b) if doc.blocks[b].list.is_some() => {
                    let list = doc.blocks[b].list.expect("list");
                    let text = clean_content(content, doc.newline)?;
                    self.insert_item(list, *position, &text, *stamp)
                }
                Target::Section(s) => self.insert_section(s, *position, content, *stamp),
                _ => Err(err(
                    ErrorCode::InvalidRequest,
                    "insert targets a list (by its stay) or a section",
                )),
            },
            Op::Delete { list, .. } => self.delete(target, list.as_ref()),
            Op::Append {
                select: _,
                guard: _,
                content,
            } => match target {
                Target::Section(s) => self.append(s, content),
                _ => Err(err(ErrorCode::InvalidRequest, "append targets a section")),
            },
            Op::ReplaceInSection {
                select: _,
                guard: _,
                old,
                new,
                top_level,
            } => match target {
                Target::Section(s) => self.replace_in_section(s, old, new, false, *top_level),
                _ => Err(err(
                    ErrorCode::InvalidRequest,
                    "replace_in_section targets a section",
                )),
            },
            Op::RemoveParagraphInSection { old, .. } => match target {
                Target::Section(s) => self.replace_in_section(s, old, "", true, false),
                _ => Err(err(
                    ErrorCode::InvalidRequest,
                    "remove_paragraph_in_section targets a section",
                )),
            },
            Op::AddKey { key, value, .. } => self.add_key(key, value),
            Op::AddKeyYaml { key, yaml, .. } => self.add_key_yaml(key, yaml),
            Op::SetKeyYaml {
                yaml, replace_line, ..
            } => {
                let Target::FrontmatterKey(k) = target else {
                    unreachable!()
                };
                self.set_key_yaml(k, yaml, *replace_line)
            }
            Op::SetKeyField {
                key_path, value, ..
            } => {
                let Target::FrontmatterKey(k) = target else {
                    unreachable!()
                };
                self.set_key_field(k, &key_path[1], value)
            }
            Op::DeleteKey { .. } => {
                let Target::FrontmatterKey(k) = target else {
                    unreachable!()
                };
                self.delete_key(k)
            }
            Op::AppendKeyItem { key, item, .. } => self.append_key_item(key, item),
            Op::Rejoin {
                select: _,
                guard: _,
                line,
            } => match target {
                Target::Section(s) => self.rejoin(s, *line),
                _ => Err(err(ErrorCode::InvalidRequest, "rejoin targets a section")),
            },
            Op::AdoptStay { id, .. } => self.adopt_stay(id),
            Op::Refresh { select, .. } => refresh::plan(doc, select),
            Op::Stamp { .. } => self.stamp(target),
            Op::UpdateItem { .. } | Op::InsertItem { .. } => Err(err(
                ErrorCode::InvalidRequest,
                "typed operations need a project schema context",
            )),
        }?;
        if positional
            && let Some(region) = change
                .expectation
                .as_ref()
                .and_then(Expectation::item_region)
        {
            change.follow = Some(Follow::Placed {
                region,
                typed: None,
            });
        }
        change.guard = Some(intent_guard);
        change.preflight(doc)?;
        Ok(change)
    }

    /// Resolve a native delete with a project binding, carrying its collection
    /// membership into the independent planning gate on the same source doc.
    pub fn delete_in_collection(
        &mut self,
        op: &Op,
        bound: &crate::schema::Bound,
    ) -> Result<Change, Diagnostic> {
        let Op::Delete {
            select,
            guard,
            collection: Some(name),
            list,
        } = op
        else {
            return Err(err(
                ErrorCode::InvalidRequest,
                "a bound delete needs a collection precondition",
            ));
        };
        if *name != bound.name {
            return Err(err(
                ErrorCode::CollectionConflict,
                "delete collection and resolved binding differ",
            ));
        }
        intent::delete_collection(self.doc, bound, self.doc.resolve(select)?)?;
        let mut change = self.structural(&Op::Delete {
            select: select.clone(),
            guard: guard.clone(),
            list: list.clone(),
            collection: None,
        })?;
        let Some(Expectation::Delete { collection, .. }) = &mut change.expectation else {
            unreachable!("structural delete supplies a delete expectation");
        };
        *collection = Some(Box::new(bound.clone()));
        change.preflight(self.doc)?;
        Ok(change)
    }

    pub fn set_item(&mut self, list: usize, item: usize, text: &str) -> Result<Change, Diagnostic> {
        let doc = self.doc;
        let it = &doc.lists[list].items[item];
        let content = clean_content(text, doc.newline)?;
        let content = trim_end_block(&content).to_string();
        if content.is_empty() || content.starts_with([' ', '\t']) {
            return Err(err(
                ErrorCode::InvalidContent,
                "item content must be non-empty and must not start with whitespace",
            ));
        }
        let own: Vec<String> = it
            .markers
            .iter()
            .map(|&m| {
                let lexical = &doc.markers[m].lexical;
                doc.source[lexical.span.start..lexical.span.end].to_string()
            })
            .collect();
        let prefix = &doc.source[it.prefix.start..it.prefix.end];
        let old = doc.item_lines(list, item);
        let lines: Vec<&str> = content.split(doc.newline).collect();
        let kept = kept_indentation(&old, &lines);
        let mut rendered = render_item(prefix, it.width, &content, doc.newline, &kept);
        // A line repeated with nothing removed may be lazy, and laziness
        // depends on the line before it; where the write changed that, the
        // line would leave the item. Such lines go back to the width, which is
        // what the content means. A removed indentation is exactly the width,
        // so keeping it never changes the reading.
        if kept.iter().any(Option::is_some) && !reads_back(&rendered, &content, doc.newline) {
            let exact: Vec<_> = kept.iter().map(|k| k.filter(|i| !i.is_empty())).collect();
            rendered = render_item(prefix, it.width, &content, doc.newline, &exact);
        }
        let (count, carrier) = markdown::standalone_item(&rendered);
        if count != 1 {
            return Err(err(
                ErrorCode::InvalidContent,
                "content must render as exactly one list item",
            ));
        }
        if !own.is_empty() {
            let at = carrier.ok_or_else(|| {
                err(
                    ErrorCode::InvalidContent,
                    "content leaves no paragraph to carry the item's stay",
                )
            })?;
            rendered.insert_str(at, &format!(" {}", own.join(" ")));
        }
        let ids = doc.stay_ids(&it.markers);
        let expect = content.replace("\r\n", "\n");
        let region = ItemRegion::Spliced {
            list,
            splice: it.span,
            offset: 0,
            len: rendered.len(),
        };
        let mut change = Change {
            splices: vec![Splice {
                span: it.span,
                text: rendered,
            }],
            ..Change::default()
        };
        change.follow = ids.first().cloned().map(Follow::Id);
        change.expectation = Some(Expectation::Item {
            list,
            item,
            region,
            expect,
            ids,
        });
        Ok(change)
    }

    /// Tick or untick an item by rewriting the one byte between its brackets.
    /// Nothing else in the item is re-rendered, so an item whose body holds a
    /// stay owned by one of its blocks can be ticked, which a whole-item
    /// rewrite refuses because it would have to move that stay.
    pub fn set_checkbox(
        &mut self,
        list: usize,
        item: usize,
        checked: bool,
    ) -> Result<Change, Diagnostic> {
        let doc = self.doc;
        let it = &doc.lists[list].items[item];
        let Some(checkbox) = &it.checkbox else {
            return Err(err(ErrorCode::InvalidRequest, "the item has no checkbox"));
        };
        // Already in the requested state: keep the byte, so `X` is not respelt.
        let symbol = if checked == checkbox.checked {
            &doc.source[checkbox.symbol.start..checkbox.symbol.end]
        } else if checked {
            "x"
        } else {
            " "
        };
        let content = doc.item_content(list, item);
        let offset = checkbox.symbol.start - it.prefix.end;
        let expect = format!(
            "{}{symbol}{}",
            &content[..offset],
            &content[offset + checkbox.symbol.len()..]
        );
        let region = ItemRegion::InPlace {
            list,
            span: it.span,
        };
        let ids = doc.stay_ids(&it.markers);
        let mut change = Change {
            splices: vec![Splice {
                span: checkbox.symbol,
                text: symbol.to_string(),
            }],
            ..Change::default()
        };
        change.follow = ids.first().cloned().map(Follow::Id);
        // A stay inside the item that is not its own (the list's, or a block's)
        // keeps its bytes except the digest, which the write refreshes like any
        // other. So compare with digest values blanked on both sides; the stay
        // ids, and so every marker's place, are checked exactly.
        change.expectation = Some(Expectation::Checkbox {
            list,
            item,
            region,
            expect,
            ids,
        });
        change.guard = Some(Guard::new(
            GuardSubject::Native(Target::Item { list, item }),
            &doc.target_revision(Target::Item { list, item }),
        ));
        Ok(change)
    }

    fn set_title(&mut self, s: usize, text: &str) -> Result<Change, Diagnostic> {
        let doc = self.doc;
        let section = &doc.sections[s];
        let heading = doc.blocks[section.block].heading.as_ref().expect("heading");
        let Some(title) = heading.title else {
            return Err(err(
                ErrorCode::UnsupportedSyntax,
                "setext heading titles are not editable",
            ));
        };
        let text = clean_content(text, doc.newline)?;
        if text.contains('\n') || text.trim().is_empty() || text != text.trim_matches([' ', '\t']) {
            return Err(err(
                ErrorCode::InvalidContent,
                "a title is one non-empty line without surrounding whitespace",
            ));
        }
        let level = section.level;
        let start = doc.blocks[section.block].span.start;
        let ids = doc.section_ids(s);
        let mut change = Change {
            splices: vec![Splice {
                span: title,
                text: text.clone(),
            }],
            ..Change::default()
        };
        change.follow = Some(
            ids.first()
                .cloned()
                .map_or(Follow::Section(start), Follow::Id),
        );
        change.expectation = Some(Expectation::Title {
            s,
            start,
            level,
            text,
            ids,
        });
        Ok(change)
    }

    fn set_body(&mut self, s: usize, text: &str, allow_drop: bool) -> Result<Change, Diagnostic> {
        let doc = self.doc;
        let section = &doc.sections[s];
        let region = section.body;
        let mut content = clean_content(text, doc.newline)?;
        if region.end < doc.source.len() && !content.is_empty() && !content.ends_with('\n') {
            content.push_str(doc.newline);
        }
        let level = section.level;
        let start = doc.blocks[section.block].span.start;
        let ids = doc.section_ids(s);
        let region_span = region;
        let len = content.len();
        let mut change = Change {
            splices: vec![Splice {
                span: region,
                text: content,
            }],
            ..Change::default()
        };
        change.follow = Some(
            ids.first()
                .cloned()
                .map_or(Follow::Section(start), Follow::Id),
        );
        change.expectation = Some(Expectation::Body {
            s,
            start,
            level,
            ids,
            region_span,
            len,
            allow_drop,
        });
        Ok(change)
    }

    fn replace_in_section(
        &mut self,
        s: usize,
        old: &str,
        new: &str,
        remove: bool,
        top_level: bool,
    ) -> Result<Change, Diagnostic> {
        let doc = self.doc;
        if old.is_empty() || (!remove && (old.contains(['\r', '\n']) || new.contains(['\r', '\n'])))
        {
            return Err(err(
                ErrorCode::InvalidRequest,
                "old must be nonempty; replacement strings must each fit on one physical line",
            ));
        }
        let section = &doc.sections[s];
        let body = &doc.source[section.body.start..section.own_end];
        // Resume one UTF-8 character past the first start so overlaps count.
        let at = body.find(old).ok_or_else(|| {
            err(
                ErrorCode::MatchNotFound,
                "old does not occur in the section's own body",
            )
        })?;
        let next = at + old.chars().next().expect("nonempty old").len_utf8();
        if body[next..].contains(old) {
            return Err(err(
                ErrorCode::AmbiguousMatch,
                "old occurs more than once in the section's own body (overlaps count)",
            ));
        }
        let span = Span {
            start: section.body.start + at,
            end: section.body.start + at + old.len(),
        };
        if doc.markers.iter().any(|m| m.lexical.span.overlaps(span)) {
            return Err(err(
                ErrorCode::InvalidContent,
                "the match overlaps a live stay marker",
            ));
        }
        let (b, block) = doc
            .blocks
            .iter()
            .enumerate()
            .find(|(_, block)| intent::contains(block.span, span))
            .ok_or_else(|| {
                err(
                    ErrorCode::UnsupportedSyntax,
                    "the match must lie wholly inside one paragraph",
                )
            })?;
        let paragraph_span = if block.kind == "paragraph" {
            block.span
        } else if !remove && top_level && matches!(block.kind, "list" | "block_quote") {
            return Err(err(
                ErrorCode::UnsupportedSyntax,
                "top_level is set and the match lies in a list item or block quote",
            ));
        } else if !remove && matches!(block.kind, "list" | "block_quote") {
            markdown::block_layout(doc)
                .into_iter()
                .find(|p| p.kind == "paragraph" && intent::contains(p.span, span))
                .map(|p| p.span)
                .ok_or_else(|| {
                    err(
                        ErrorCode::UnsupportedSyntax,
                        "the match must lie wholly inside one paragraph of a list item or quote",
                    )
                })?
        } else {
            return Err(err(
                ErrorCode::UnsupportedSyntax,
                "the match must lie wholly inside one paragraph; removal requires a top-level paragraph",
            ));
        };
        if remove && span != paragraph_span {
            return Err(err(
                ErrorCode::InvalidContent,
                "old must equal the whole paragraph source, excluding its final line ending",
            ));
        }
        let mut change = Change {
            follow: Some(
                doc.section_ids(s)
                    .first()
                    .cloned()
                    .map_or(Follow::Section(section.heading.start), Follow::Id),
            ),
            expectation: Some(Expectation::Paragraph {
                s,
                b,
                paragraph_span,
                match_span: span,
                remove,
            }),
            ..Change::default()
        };
        if remove || old != new {
            change.splices.push(Splice {
                span,
                text: new.to_string(),
            });
        }
        Ok(change)
    }

    fn set_block(&mut self, b: usize, text: &str, allow_drop: bool) -> Result<Change, Diagnostic> {
        let doc = self.doc;
        let block = &doc.blocks[b];
        let mut content = trim_end_block(&clean_content(text, doc.newline)?).to_string();
        if content.is_empty() {
            return Err(err(
                ErrorCode::InvalidContent,
                "block content must not be empty; delete instead",
            ));
        }
        // Own stays found on the block's lines move to a marker-only line
        // after the new content: markstay §3.4 forbids moving a stay onto a
        // line that carries content outside the child carriers.
        let inline_markers: Vec<&markers::Lexical> = block
            .markers
            .iter()
            .map(|m| &doc.markers[*m].lexical)
            .filter(|l| l.span.end <= block.span.end)
            .collect();
        let inline: Vec<String> = inline_markers
            .iter()
            .map(|l| doc.source[l.span.start..l.span.end].to_string())
            .collect();
        if !inline.is_empty() {
            content.push_str(doc.newline);
            content.push_str(&inline.join(" "));
        }
        let ids = doc.stay_ids(&block.markers);
        let replaced = block.span;
        let len = content.len();
        let mut change = Change {
            splices: vec![Splice {
                span: block.span,
                text: content,
            }],
            ..Change::default()
        };
        change.follow = ids.first().cloned().map(Follow::Id);
        change.expectation = Some(Expectation::Block {
            b,
            replaced,
            len,
            ids,
            allow_drop,
        });
        Ok(change)
    }

    fn set_frontmatter(&mut self, k: usize, value: &Value) -> Result<Change, Diagnostic> {
        let doc = self.doc;
        let scalar = &doc.scalars()[k];
        if !scalar.writable {
            return Err(err(
                ErrorCode::UnsupportedSyntax,
                format!(
                    "frontmatter key {} is read-only: Rivet writes single-line scalars, \
                     not an empty value, a list or a mapping",
                    scalar.key
                ),
            ));
        }
        let line = &doc.source[scalar.line.start..scalar.line.end];
        let relative = Span {
            start: scalar.span.start - scalar.line.start,
            end: scalar.span.end - scalar.line.start,
        };
        let text = yaml::render_scalar(line, relative, &scalar.style, value).ok_or_else(|| {
            err(
                ErrorCode::UnsupportedSyntax,
                "value cannot be written as a single-line JSON-compatible YAML scalar",
            )
        })?;
        let key = scalar.key.clone();
        let others = frontmatter_others(doc, Some(&key));
        let expect = value.clone();
        let mut change = Change {
            splices: vec![Splice {
                span: scalar.span,
                text,
            }],
            ..Change::default()
        };
        change.follow = Some(Follow::FrontmatterKey(key.clone()));
        change.expectation = Some(Expectation::Frontmatter {
            field: None,
            k,
            key,
            expect,
            others,
        });
        Ok(change)
    }

    fn set_key_field(
        &mut self,
        k: usize,
        field: &str,
        value: &Value,
    ) -> Result<Change, Diagnostic> {
        let doc = self.doc;
        let parent = &doc.scalars()[k];
        let mut expect = parent.value.clone();
        let mapping = expect.as_object_mut().ok_or_else(|| {
            err(
                ErrorCode::UnsupportedSyntax,
                "set_key_field requires a mapping parent",
            )
        })?;
        let old = mapping.get_mut(field).ok_or_else(|| {
            err(
                ErrorCode::MissingSelector,
                format!("mapping field {field:?} does not exist"),
            )
        })?;
        *old = value.clone();
        let line = &doc.source[parent.line.start..parent.line.end];
        let scalar = yaml::mapping_field(line, field).ok_or_else(|| {
            err(
                ErrorCode::UnsupportedSyntax,
                "mapping field must be an existing nonempty single-line scalar",
            )
        })?;
        let text = yaml::render_mapping_field(line, &scalar, value, &expect).ok_or_else(|| {
            err(
                ErrorCode::UnsupportedSyntax,
                "value cannot be written as a single-line JSON-compatible YAML scalar",
            )
        })?;
        let others = frontmatter_others(doc, Some(&parent.key));
        Ok(Change {
            splices: vec![Splice {
                span: Span {
                    start: parent.line.start + scalar.span.start,
                    end: parent.line.start + scalar.span.end,
                },
                text,
            }],
            follow: Some(Follow::FrontmatterKey(parent.key.clone())),
            expectation: Some(Expectation::Frontmatter {
                k,
                key: parent.key.clone(),
                field: Some(field.to_string()),
                expect,
                others,
            }),
            ..Change::default()
        })
    }

    fn delete_key(&mut self, k: usize) -> Result<Change, Diagnostic> {
        let doc = self.doc;
        // Without a YAML-looking line the fences become Markdown, which can
        // change body ownership. Refuse rather than synthesize a placeholder.
        if doc.scalars().len() == 1 {
            return Err(err(
                ErrorCode::UnsupportedSyntax,
                "cannot delete the last frontmatter key; the fences would become Markdown",
            ));
        }
        let scalar = &doc.scalars()[k];
        let span = frontmatter_key_lines(doc, k);
        Ok(Change {
            splices: vec![Splice {
                span,
                text: String::new(),
            }],
            expectation: Some(Expectation::DeletedKey {
                k,
                key: scalar.key.clone(),
                others: frontmatter_others(doc, Some(&scalar.key)),
            }),
            ..Change::default()
        })
    }

    pub fn insert_item(
        &mut self,
        list: usize,
        position: Position,
        content: &str,
        stamp: bool,
    ) -> Result<Change, Diagnostic> {
        let doc = self.doc;
        let l = &doc.lists[list];
        let n = l.items.len();
        let content = trim_end_block(content).to_string();
        if content.is_empty() || content.starts_with([' ', '\t']) {
            return Err(err(
                ErrorCode::InvalidContent,
                "item content must be non-empty and must not start with whitespace",
            ));
        }
        let anchor = match position {
            Position::Start => &l.items[0],
            Position::End => &l.items[n - 1],
        };
        let base = &doc.source[anchor.prefix.start..anchor.prefix.end];
        let prefix = if l.ordered {
            let number = prefix_number(base).unwrap_or(1);
            let number = match position {
                Position::Start => number,
                Position::End => number.saturating_add(1),
            };
            numbered_prefix(base, number)
                .ok_or_else(|| err(ErrorCode::UnsupportedSyntax, "cannot number the new item"))?
        } else {
            base.to_string()
        };
        let width = columns(&prefix);
        let mut rendered = render_item(&prefix, width, &content, doc.newline, &[]);
        let (count, carrier) = markdown::standalone_item(&rendered);
        if count != 1 {
            return Err(err(
                ErrorCode::InvalidContent,
                "content must render as exactly one list item",
            ));
        }
        let (edge, gap) = item_edge(doc, list, position);
        let block = l.block;
        let mut minted_ids = vec![];
        let mut container = String::new();
        if stamp {
            let at = carrier.ok_or_else(|| {
                err(
                    ErrorCode::StampRefused,
                    "content leaves no paragraph to carry a stay",
                )
            })?;
            // §3.4: the container's raw source up to the marker, as written.
            // `existing` is that source's extent, which is also the boundary
            // between a file position and one in the caller's own text.
            let (before, plain, existing) = match position {
                Position::Start => (String::new(), vec![], None),
                Position::End => {
                    let span = Span {
                        start: doc.blocks[block].span.start,
                        end: edge,
                    };
                    (
                        format!("{}{}", &doc.source[span.start..span.end], gap),
                        plain_markers_in(doc, span),
                        Some(span),
                    )
                }
            };
            let text = format!("{before}{}", &rendered[..at]);
            if let Some(r) = carrier_refusal(&text, &plain) {
                let d = err(ErrorCode::StampRefused, r.reason);
                // An offender inside the existing source is a document position.
                // Past it lies `gap`, which is `doc.newline` and carries neither
                // offender, and then the caller's own item text, which is theirs
                // and not somewhere in the file.
                return Err(match existing {
                    Some(span) if r.at < span.len() => d.with_blocker(CarrierBlocker::new(
                        &doc.source,
                        span.start + r.at,
                        r.character,
                        doc.file_line(doc.blocks[block].span.start),
                        doc.stay_ids(&doc.blocks[block].markers).into_iter().next(),
                    )),
                    _ => d,
                });
            }
            let id = self.mint();
            rendered.insert_str(
                at,
                &format!(" {}", markers::format_plain(&id, "subhash", PLACEHOLDER)),
            );
            minted_ids.push((id, "item"));
            if doc.stay_ids(&doc.blocks[block].markers).is_empty() {
                let lid = self.mint();
                container = format!(
                    "{}{}",
                    doc.newline,
                    markers::format_plain(&lid, "hash", PLACEHOLDER)
                );
                minted_ids.push((lid, "list"));
            }
        }
        let list_start = doc.blocks[block].span.start;
        let mut change = Change::default();
        let region = match position {
            Position::Start => {
                let at = edge;
                ItemRegion::Spliced {
                    list,
                    splice: Span { start: at, end: at },
                    offset: 0,
                    len: rendered.len(),
                }
            }
            Position::End => {
                let at = edge;
                ItemRegion::Spliced {
                    list,
                    splice: Span { start: at, end: at },
                    offset: gap.len(),
                    len: rendered.len(),
                }
            }
        };
        match position {
            Position::Start => {
                change.splices.push(Splice {
                    span: Span {
                        start: edge,
                        end: edge,
                    },
                    text: format!("{rendered}{gap}"),
                });
                if !container.is_empty() {
                    let end = doc.blocks[block].span.end;
                    change.splices.push(Splice {
                        span: Span { start: end, end },
                        text: container,
                    });
                }
            }
            Position::End => {
                let end = edge;
                change.splices.push(Splice {
                    span: Span { start: end, end },
                    text: format!("{gap}{rendered}{container}"),
                });
            }
        }
        let item_ids: Vec<String> = minted_ids
            .iter()
            .filter(|(_, k)| *k == "item")
            .map(|(id, _)| id.clone())
            .collect();
        change.follow = item_ids.first().cloned().map(Follow::Id);
        change.minted = minted_ids
            .into_iter()
            .map(|(id, kind)| (id, kind, list_start))
            .collect();
        change.expectation = Some(Expectation::InsertedItem {
            list,
            position,
            region,
            expect: content.replace("\r\n", "\n"),
            ids: item_ids,
        });
        Ok(change)
    }

    fn insert_section(
        &mut self,
        s: usize,
        position: Position,
        content: &str,
        stamp: bool,
    ) -> Result<Change, Diagnostic> {
        let doc = self.doc;
        let section = &doc.sections[s];
        let level = section.level + 1;
        let body = trim_end_block(&clean_content(content, "\n")?).to_string();
        let parsed = Document::parse(&body)?;
        let first = parsed.blocks.first();
        let heading = first.and_then(|b| b.heading.clone());
        if first.is_none_or(|b| b.span.start != 0)
            || heading.as_ref().is_none_or(|h| h.level != level)
        {
            return Err(err(
                ErrorCode::InvalidContent,
                format!("content must start with a level-{level} heading"),
            ));
        }
        if parsed.sections.iter().skip(1).any(|x| x.level <= level) {
            return Err(err(
                ErrorCode::InvalidContent,
                format!("content may hold only one level-{level} section"),
            ));
        }
        let heading = heading.expect("heading");
        let mut lines: Vec<String> = body.split('\n').map(str::to_string).collect();
        let mut minted = vec![];
        if stamp {
            let id = self.mint();
            let heading_lines = parsed.blocks[0].last_line + 1;
            lines.insert(
                heading_lines,
                markers::format_plain(&id, "hash", PLACEHOLDER),
            );
            minted.push((id, "section", 0usize));
        }
        let at = match position {
            Position::Start => section.own_end,
            Position::End => section.whole.end,
        };
        let before = &doc.source[..at];
        let mut text = String::new();
        if !before.is_empty() && !before.ends_with('\n') {
            text.push_str(doc.newline);
        }
        let last = before.strip_suffix('\n').unwrap_or(before);
        let last = last.strip_suffix('\r').unwrap_or(last);
        let prev_line = &last[last.rfind('\n').map_or(0, |i| i + 1)..];
        if !before.is_empty() && !prev_line.trim().is_empty() {
            text.push_str(doc.newline);
        }
        let lead = text.len();
        text.push_str(&lines.join(doc.newline));
        text.push_str(doc.newline);
        if at < doc.source.len() {
            text.push_str(doc.newline);
        }
        let title = heading.title_text.clone();
        let parent_ids = doc.section_ids(s);
        let parent_start = doc.blocks[section.block].span.start;
        let mut change = Change {
            splices: vec![Splice {
                span: Span { start: at, end: at },
                text,
            }],
            ..Change::default()
        };
        let ids: Vec<String> = minted.iter().map(|(id, _, _)| id.clone()).collect();
        change.follow = ids.first().cloned().map(Follow::Id);
        change.minted = minted.into_iter().map(|(id, k, _)| (id, k, at)).collect();
        change.expectation = Some(Expectation::InsertedSection {
            s,
            at,
            lead,
            level,
            title,
            parent_start,
            parent_ids,
            ids,
        });
        Ok(change)
    }

    /// Blocks after the last one in a section's own body. The inserted text
    /// is separated by a blank line from what precedes it and from a heading
    /// that follows, so nothing before it can absorb it; verification refuses
    /// a result where it merged anyway (a list continuing a list).
    fn append(&mut self, s: usize, content: &str) -> Result<Change, Diagnostic> {
        let doc = self.doc;
        let section = &doc.sections[s];
        let body = clean_content(content, "\n")?;
        let body = trim_end_block(body.trim_start_matches(['\n', '\r'])).to_string();
        if body.trim().is_empty() {
            return Err(err(
                ErrorCode::InvalidContent,
                "append content must not be empty",
            ));
        }
        let parsed = Document::parse(&body)?;
        if !parsed.sections.is_empty() {
            return Err(err(
                ErrorCode::InvalidContent,
                "append content may not hold a heading; insert a child section instead",
            ));
        }
        if parsed.frontmatter.span.is_some() {
            return Err(err(
                ErrorCode::InvalidContent,
                "append content may not open with a `---` block: it would read as a heading in place",
            ));
        }
        let kinds: Vec<&'static str> = parsed.blocks.iter().map(|b| b.kind).collect();
        let at = section.own_end;
        let before = &doc.source[..at];
        let mut text = String::new();
        if !before.is_empty() && !before.ends_with('\n') {
            text.push_str(doc.newline);
        }
        let last = before.strip_suffix('\n').unwrap_or(before);
        let last = last.strip_suffix('\r').unwrap_or(last);
        let prev_line = &last[last.rfind('\n').map_or(0, |i| i + 1)..];
        if !before.is_empty() && !prev_line.trim().is_empty() {
            text.push_str(doc.newline);
        }
        let lead = text.len();
        text.push_str(&body.replace('\n', doc.newline));
        let len = text.len() - lead;
        text.push_str(doc.newline);
        if at < doc.source.len() {
            text.push_str(doc.newline);
        }
        let heading = doc.blocks[section.block].span.start;
        let ids = doc.section_ids(s);
        let level = section.level;
        let mut change = Change {
            splices: vec![Splice {
                span: Span { start: at, end: at },
                text,
            }],
            ..Change::default()
        };
        change.follow = Some(
            ids.first()
                .cloned()
                .map_or(Follow::Section(heading), Follow::Id),
        );
        change.expectation = Some(Expectation::Append {
            s,
            at,
            lead,
            len,
            heading,
            level,
            kinds,
        });
        Ok(change)
    }

    /// A new top-level frontmatter line, `key: value`, after the last field.
    fn add_key(&mut self, key: &str, value: &Value) -> Result<Change, Diagnostic> {
        let doc = self.doc;
        let payload = new_key_site(doc, key)?;
        let prefix = format!("{key}: ");
        let at = Span {
            start: prefix.len(),
            end: prefix.len(),
        };
        let value_text = yaml::render_scalar(&prefix, at, "Plain", value).ok_or_else(|| {
            err(
                ErrorCode::UnsupportedSyntax,
                "value cannot be written as a single-line JSON-compatible YAML scalar",
            )
        })?;
        let text = format!("{prefix}{value_text}{}", doc.newline);
        let others = frontmatter_others(doc, None);
        let key = key.to_string();
        let expect = value.clone();
        let mut change = Change {
            splices: vec![Splice {
                span: Span {
                    start: payload.end,
                    end: payload.end,
                },
                text,
            }],
            ..Change::default()
        };
        change.follow = Some(Follow::FrontmatterKey(key.clone()));
        change.expectation = Some(Expectation::NewKey {
            key,
            expect,
            others,
        });
        Ok(change)
    }

    /// A new top-level line, `key: TOKEN`, carrying the caller's exact token.
    fn add_key_yaml(&mut self, key: &str, raw: &str) -> Result<Change, Diagnostic> {
        let doc = self.doc;
        let write = yaml_add(doc, key, raw)?;
        Ok(Change {
            splices: vec![write.splice],
            follow: Some(Follow::FrontmatterKey(key.to_string())),
            expectation: Some(Expectation::AddKeyYaml {
                key: key.to_string(),
                raw: raw.to_string(),
                expect: write.expect,
                others: frontmatter_others(doc, None),
            }),
            ..Change::default()
        })
    }

    /// One flat mapping appended to a key's sequence, or the key added.
    fn append_key_item(
        &mut self,
        key: &str,
        item: &serde_json::Map<String, Value>,
    ) -> Result<Change, Diagnostic> {
        let doc = self.doc;
        let write = yaml_append(doc, key, item)?;
        Ok(Change {
            splices: vec![write.splice],
            follow: Some(Follow::FrontmatterKey(key.to_string())),
            expectation: Some(Expectation::AppendKeyItem {
                k: write.k,
                key: key.to_string(),
                item: item.clone(),
                expect: write.expect,
                others: frontmatter_others(doc, Some(key)),
            }),
            ..Change::default()
        })
    }

    /// Replace an existing key's value token with the caller's exact token.
    fn set_key_yaml(
        &mut self,
        k: usize,
        raw: &str,
        replace_line: bool,
    ) -> Result<Change, Diagnostic> {
        let doc = self.doc;
        let write = yaml_set(doc, k, raw, replace_line)?;
        let key = doc.scalars()[k].key.clone();
        Ok(Change {
            splices: vec![write.splice],
            follow: Some(Follow::FrontmatterKey(key.clone())),
            expectation: Some(Expectation::SetKeyYaml {
                k,
                others: frontmatter_others(doc, Some(&key)),
                key,
                raw: raw.to_string(),
                replace_line,
                expect: write.expect,
            }),
            ..Change::default()
        })
    }

    /// Move the line break out of the one code span that opens on `line`
    /// and closes on the next. Before the span on its opening line there is
    /// either text, and the break moves in front of the span, or only the
    /// item's marker (or nothing), and the two lines join. The rendered page
    /// is unchanged: the candidate's HTML equals the original's with runs of
    /// whitespace collapsed.
    fn rejoin(&mut self, s: usize, line: usize) -> Result<Change, Diagnostic> {
        let doc = self.doc;
        let section = &doc.sections[s];
        let starts = markers::line_starts(&doc.source);
        let malformed = |why: String| err(ErrorCode::InvalidRequest, why);
        if line == 0 || line >= starts.len() {
            return Err(malformed(format!(
                "line {line} and the line after it are not both in the document"
            )));
        }
        // Line ends exclude the terminator, `\r\n` or `\n`.
        let end_of = |n: usize| {
            let end = starts.get(n + 1).map_or(doc.source.len(), |s| s - 1);
            if end > starts[n] && doc.source.as_bytes()[end - 1] == b'\r' {
                end - 1
            } else {
                end
            }
        };
        let open = starts[line - 1];
        let next = starts[line];
        let open_end = end_of(line - 1);
        let next_end = end_of(line);
        if !intent::contains(
            section.whole,
            Span {
                start: open,
                end: next_end,
            },
        ) {
            return Err(malformed(format!(
                "lines {line} and {} are not inside the selected section",
                line + 1
            )));
        }
        // Both lines inside one direct item of a list.
        let home = doc.lists.iter().enumerate().find_map(|(l, list)| {
            list.items
                .iter()
                .position(|it| it.span.start <= open && next_end <= it.span.end)
                .map(|i| (l, i))
        });
        let Some((list, _)) = home else {
            return Err(malformed(format!(
                "lines {line} and {} are not both inside one list item",
                line + 1
            )));
        };
        let spans: Vec<usize> = markdown::wrapped_code_spans(&doc.source, line)
            .into_iter()
            .filter(|at| (open..open_end).contains(at))
            .collect();
        let [at] = spans.as_slice() else {
            return Err(err(
                ErrorCode::MissingSelector,
                format!(
                    "no code span opens on line {line} and closes on line {}",
                    line + 1
                ),
            ));
        };
        let opening = &doc.source[open..open_end];
        let col = at - open;
        if markers::contains_opening(&opening[col..]) {
            return Err(err(
                ErrorCode::UnsupportedSyntax,
                "a stay marker follows the span on its opening line; moving it is refused",
            ));
        }
        let second = &doc.source[next..next_end];
        let rest = second.trim_start_matches([' ', '\t']);
        let indent = &second[..second.len() - rest.len()];
        let head = opening[..col].trim_end_matches([' ', '\t']);
        // From the span's opening backtick to the line end, all of it inside
        // the span: trailing spaces are the span's content and stay.
        let tail = &opening[col..];
        let terminator = &doc.source[open_end..next];
        // Only the break and the next line's indentation are replaced, so the
        // span's text and anything after it on the next line (a stay) keep
        // their bytes: the break moves in front of the span, or becomes the
        // one space a line ending inside a code span already reads as.
        let (span, text) = if marker_only(head) {
            (
                Span {
                    start: open_end,
                    end: next + indent.len(),
                },
                " ".to_string(),
            )
        } else {
            (
                Span {
                    start: open + head.len(),
                    end: next + indent.len(),
                },
                format!("{terminator}{indent}{tail} "),
            )
        };
        let before = markdown::page(doc);
        let codes = markdown::code_texts(doc);
        let mut change = Change {
            splices: vec![Splice { span, text }],
            ..Change::default()
        };
        let ids = doc.section_ids(s);
        let heading = doc.blocks[section.block].span.start;
        change.follow = Some(
            ids.first()
                .cloned()
                .map_or(Follow::Section(heading), Follow::Id),
        );
        let items = doc.lists[list].items.len();
        let list_start = doc.blocks[doc.lists[list].block].span.start;
        change.expectation = Some(Expectation::Rejoin {
            s,
            list,
            list_start,
            items,
            before,
            codes,
        });
        Ok(change)
    }

    /// Rewrite a hand-written stay inside an unstamped list item, in place,
    /// as Rivet's child stay with the item's digest. The id and its position
    /// are kept, so a citation of it still resolves, now to the item.
    fn adopt_stay(&mut self, id: &str) -> Result<Change, Diagnostic> {
        let doc = self.doc;
        let (_, list, item, m) = adoption_target(doc, id)?;
        let lexical = &doc.markers[m].lexical;
        let it = &doc.lists[list].items[item];
        let text = markers::format_plain(id, "subhash", PLACEHOLDER);
        let mut change = Change {
            splices: vec![Splice {
                span: lexical.span,
                text,
            }],
            adopted: vec![id.to_string()],
            ..Change::default()
        };
        change.follow = Some(Follow::Id(id.to_string()));
        let want = id.to_string();
        let item_start = it.span.start;
        change.expectation = Some(Expectation::Adoption {
            list,
            item,
            want,
            item_start,
        });
        Ok(change)
    }

    fn delete(&mut self, target: Target, list: Option<&ListGuard>) -> Result<Change, Diagnostic> {
        let span = intent::guarded_delete_span(self.doc, target, list)?;
        Ok(Change {
            splices: vec![Splice {
                span,
                text: String::new(),
            }],
            expectation: Some(Expectation::Delete {
                target,
                span,
                list: list.cloned(),
                collection: None,
            }),
            ..Change::default()
        })
    }

    fn stamp(&mut self, scope: Target) -> Result<Change, Diagnostic> {
        self.stamp_scoped(scope, None)
    }

    /// Mint stays in `scope`. `items` narrows where list items may be
    /// stamped, leaving every section in scope addressable: a tracker needs
    /// an id on its Log heading, not on every bullet of its prose.
    pub fn stamp_scoped(
        &mut self,
        scope: Target,
        items: Option<&[Span]>,
    ) -> Result<Change, Diagnostic> {
        self.stamp_selected(scope, items, false)
    }

    pub(crate) fn stamp_selected(
        &mut self,
        scope: Target,
        items: Option<&[Span]>,
        checkbox_only: bool,
    ) -> Result<Change, Diagnostic> {
        let doc = self.doc;
        let carriers = Carriers::new(doc);
        let span = match scope {
            Target::Document => Span {
                start: doc.body_start,
                end: doc.source.len(),
            },
            Target::Section(s) => doc.sections[s].whole,
            _ => {
                return Err(err(
                    ErrorCode::InvalidRequest,
                    "stamp scope is the document or a section",
                ));
            }
        };
        let mut change = Change {
            expectation: Some(Expectation::Stamp {
                scope,
                item_scopes: items.map(<[Span]>::to_vec),
                checkbox_only,
            }),
            ..Change::default()
        };
        let mut inserts: BTreeMap<usize, String> = BTreeMap::new();
        for section in doc
            .sections
            .iter()
            .filter(|x| !checkbox_only && span.contains(x.whole.start))
        {
            let block = &doc.blocks[section.block];
            if !doc.stay_ids(&block.markers).is_empty() {
                continue;
            }
            // A heading's `{#id}` becomes its stay, so both identities agree.
            let id = match block.heading.as_ref().and_then(|h| h.legacy_id.clone()) {
                Some(id) if doc.ids.get(&id).is_some_and(|t| t.len() == 1) => id,
                Some(_) => {
                    change.refused.push(
                        err(ErrorCode::StampRefused, "heading {#id} is not unique")
                            .with_line(doc.file_line(block.span.start)),
                    );
                    continue;
                }
                None => self.mint(),
            };
            inserts
                .entry(block.span.end)
                .or_default()
                .push_str(&format!(
                    "{}{}",
                    doc.newline,
                    markers::format_plain(&id, "hash", PLACEHOLDER)
                ));
            change.minted.push((id, "section", block.span.start));
        }
        for (li, l) in doc.lists.iter().enumerate() {
            let block = &doc.blocks[l.block];
            let at = block.span.start;
            if !span.contains(at) || items.is_some_and(|s| !s.iter().any(|x| x.contains(at))) {
                continue;
            }
            let mut stamped_any = false;
            // A skipped item's hand-written stay reads as the list's until it is
            // adopted, so it does not count as the list's container stay.
            let mut adoptable: Vec<String> = vec![];
            for (index, it) in l.items.iter().enumerate() {
                if checkbox_only && it.checkbox.is_none() {
                    continue;
                }
                if !it.markers.is_empty() {
                    continue;
                }
                if let Some((d, ids)) = hand_written_stay(&carriers, li, index) {
                    adoptable.extend(ids);
                    change.refused.push(d);
                    continue;
                }
                let at = match stamp_item_carrier(&carriers, li, index) {
                    Ok(at) => at,
                    Err(d) => {
                        change.refused.push(d);
                        continue;
                    }
                };
                let id = self.mint();
                let marker = format!(" {}", markers::format_plain(&id, "subhash", PLACEHOLDER));
                inserts.entry(at).or_default().push_str(&marker);
                change.minted.push((id, "item", it.span.start));
                stamped_any = true;
            }
            if stamped_any
                && doc
                    .stay_ids(&block.markers)
                    .iter()
                    .all(|id| adoptable.contains(id))
            {
                let id = self.mint();
                inserts
                    .entry(block.span.end)
                    .or_default()
                    .push_str(&format!(
                        "{}{}",
                        doc.newline,
                        markers::format_plain(&id, "hash", PLACEHOLDER)
                    ));
                change.minted.push((id, "list", block.span.start));
            }
        }
        for (at, text) in inserts {
            change.splices.push(Splice {
                span: Span { start: at, end: at },
                text,
            });
        }
        Ok(change)
    }
}

fn apply(source: &str, splices: &[Splice]) -> String {
    let mut out = String::with_capacity(source.len());
    let mut at = 0;
    for s in splices {
        out.push_str(&source[at..s.span.start]);
        out.push_str(&s.text);
        at = s.span.end;
    }
    out.push_str(&source[at..]);
    out
}

/// Refresh stored digests whose unit body this write changed, and fill minted
/// placeholders. Returns length-preserving splices over `candidate`.
fn refresh(
    doc: &Document,
    candidate: &Document,
    minted: &BTreeSet<String>,
    map: &Mapper,
) -> Result<(Vec<Splice>, Vec<Evidence>), Diagnostic> {
    let mut original: BTreeMap<String, usize> = BTreeMap::new();
    let mut surviving = BTreeMap::new();
    for (i, m) in doc.markers.iter().enumerate() {
        let span = m.lexical.span;
        if !map.splices.iter().any(|(s, _)| s.overlaps(span)) {
            surviving.insert((map.map(span.start, true), map.map(span.end, false)), i);
        }
        if let Some(id) = &m.lexical.id
            && doc.ids.get(id).is_some_and(|t| t.len() == 1)
        {
            original.insert(id.clone(), i);
        }
    }
    let mut splices = vec![];
    let mut evidence = vec![];
    for (i, m) in candidate.markers.iter().enumerate() {
        let Some(id) = m.lexical.id.clone() else {
            continue;
        };
        let Some((fresh, stored)) = candidate.marker_evidence(i) else {
            continue;
        };
        let item = matches!(m.attach, Attach::Item { .. });
        let key = if item { "subhash" } else { "hash" };
        let is_new = minted.contains(&id);
        // Surviving spans identify each copy of a duplicate id separately.
        // A unique stay carried into replacement text can move within a splice,
        // where offsets no longer identify it, but its id still does.
        let before = surviving
            .get(&(m.lexical.span.start, m.lexical.span.end))
            .filter(|o| doc.markers[**o].lexical.id.as_ref() == Some(&id))
            .or_else(|| original.get(&id))
            .and_then(|o| doc.marker_evidence(*o));
        let changed = is_new || before.as_ref().is_none_or(|(full, _)| *full != fresh);
        if !changed {
            continue;
        }
        let Some(stored) = stored else {
            continue;
        };
        let prior_drift = before
            .as_ref()
            .is_some_and(|(full, _)| !full.starts_with(&stored));
        if fresh.starts_with(&stored) {
            // Restoring earlier bytes can make a stale stored digest current
            // again without rewriting it. The drift still predates this write.
            if prior_drift && !is_new {
                evidence.push(Evidence {
                    id: id.clone(),
                    key: key.to_string(),
                    stored: stored.clone(),
                    refreshed: stored,
                    prior_drift,
                    rewritten: false,
                    requested: false,
                });
            }
            continue;
        }
        let value = if item {
            m.lexical.subhash_value
        } else {
            m.lexical.hash_value
        };
        let raw = &candidate.source[m.lexical.span.start..m.lexical.span.end];
        let rewritten = value
            .and_then(|v| markers::refresh_digest(raw, v, &fresh))
            .ok_or_else(|| {
                err(
                    ErrorCode::UnsupportedSyntax,
                    format!("stay {id} needs its {key} refreshed but the marker cannot be rewritten safely"),
                )
                .with_id(id.clone())
            })?;
        if !is_new {
            evidence.push(Evidence {
                id: id.clone(),
                key: key.to_string(),
                stored: stored.clone(),
                refreshed: fresh[..stored.len()].to_string(),
                prior_drift,
                rewritten: true,
                requested: false,
            });
        }
        splices.push(Splice {
            span: m.lexical.span,
            text: rewritten,
        });
    }
    Ok((splices, evidence))
}

fn kind_of(target: Target) -> &'static str {
    match target {
        Target::Section(_) => "section",
        Target::Item { .. } => "item",
        Target::Block(_) => "block",
        _ => "other",
    }
}

fn live_markers_in(doc: &Document, region: Span) -> impl Iterator<Item = &Marker> {
    let first = doc
        .markers
        .partition_point(|m| m.lexical.span.start < region.start);
    doc.markers[first..]
        .iter()
        .take_while(move |m| m.lexical.span.start < region.end)
        .filter(move |m| m.lexical.span.end <= region.end)
}

/// Verify the candidate: everything outside the declared changes is intact,
/// stays keep their kind, no new marker faults appear, and each op's
/// postcondition holds.
fn verify(
    doc: &Document,
    c: &Document,
    changes: &mut [Change],
    map: &Mapper,
) -> Result<(), Diagnostic> {
    let invalid = |m: String| err(ErrorCode::InvalidContent, m);
    let rewritten: BTreeSet<usize> = changes
        .iter()
        .flat_map(|c| c.blocks.iter().copied())
        .collect();
    let partial: BTreeSet<usize> = changes
        .iter()
        .flat_map(|c| c.lists.iter().chain(c.reshaped.iter()).copied())
        .collect();
    let items: BTreeSet<(usize, usize)> = changes
        .iter()
        .flat_map(|c| c.items.iter().copied())
        .collect();
    let minted: BTreeMap<&String, &str> = changes
        .iter()
        .flat_map(|c| {
            c.minted
                .iter()
                .map(|(id, kind, _)| (id, *kind))
                .chain(c.imported.iter().map(|(id, kind)| (id, *kind)))
        })
        .collect();
    let old_ids = |ids: Vec<String>| -> Vec<String> {
        ids.into_iter()
            .filter(|id| !minted.contains_key(id))
            .collect()
    };
    // Reference definitions can change marker liveness in untouched source.
    // Compare positions as well as IDs: a newly exposed copy must not stand
    // in for a hidden marker. Changed units keep their operation-specific
    // checks; newly stamped stays are permitted by the batch's mint records.
    let markers_intact = |before: Span, after: Span| {
        live_markers_in(doc, before)
            .map(|m| Span {
                start: map.map(m.lexical.span.start, true),
                end: map.map(m.lexical.span.end, false),
            })
            .eq(live_markers_in(c, after)
                .filter(|m| {
                    m.lexical
                        .id
                        .as_ref()
                        .is_none_or(|id| !minted.contains_key(id))
                })
                .map(|m| m.lexical.span))
    };
    for (b, block) in doc.blocks.iter().enumerate() {
        if let Some(list) = block.list.filter(|l| partial.contains(l)) {
            // The candidate list holds exactly the untouched items, intact,
            // plus each declared new or rewritten item: no splits or merges.
            let mut expected: Vec<Span> = changes
                .iter()
                .flat_map(|ch| ch.regions.iter())
                .filter(|r| r.list() == list)
                .map(|r| r.resolve(map))
                .collect();
            for (i, item) in doc.lists[list].items.iter().enumerate() {
                if items.contains(&(list, i)) {
                    continue;
                }
                let span = Span {
                    start: map.map(item.span.start, true),
                    end: map.map(item.span.end, false),
                };
                let intact = c.lists.iter().flat_map(|l| l.items.iter()).any(|x| {
                    x.span == span
                        && c.source[span.start..span.end]
                            == doc.source[item.span.start..item.span.end]
                        && old_ids(c.stay_ids(&x.markers)) == doc.stay_ids(&item.markers)
                        && markers_intact(item.span, span)
                });
                if !intact {
                    return Err(invalid(format!(
                        "item at line {} would change",
                        doc.file_line(item.span.start)
                    )));
                }
                expected.push(span);
            }
            expected.sort_by_key(|s| s.start);
            let home = expected.first().and_then(|first| {
                c.lists
                    .iter()
                    .find(|l| l.items.iter().any(|i| i.span == *first))
            });
            let actual: Vec<Span> =
                home.map_or(vec![], |l| l.items.iter().map(|i| i.span).collect());
            if actual != expected {
                return Err(invalid(format!(
                    "the list at line {} would change shape ({} items instead of {})",
                    doc.file_line(block.span.start),
                    actual.len(),
                    expected.len()
                )));
            }
            continue;
        }
        if rewritten.contains(&b) {
            continue;
        }
        let start = map.map(block.span.start, true);
        let end = map.map(block.span.end, false);
        let intact = c
            .blocks
            .iter()
            .find(|x| x.span.start == start)
            .is_some_and(|x| {
                x.span.end == end
                    && x.kind == block.kind
                    && c.source[x.span.start..x.span.end]
                        == doc.source[block.span.start..block.span.end]
                    && old_ids(c.stay_ids(&x.markers)) == doc.stay_ids(&block.markers)
                    && markers_intact(
                        Span {
                            start: block.span.start,
                            end: block.region_end,
                        },
                        Span {
                            start,
                            end: map.map(block.region_end, false),
                        },
                    )
            });
        if !intact {
            return Err(invalid(format!(
                "the {} at line {} would change or re-parse differently",
                block.kind,
                doc.file_line(block.span.start)
            )));
        }
    }
    if !changes.iter().any(|ch| ch.frontmatter) {
        let before = doc.frontmatter.span.map(|s| &doc.source[s.start..s.end]);
        let after = c.frontmatter.span.map(|s| &c.source[s.start..s.end]);
        if before != after {
            return Err(invalid("frontmatter would change".into()));
        }
    }
    let count =
        |d: &Document, code: ErrorCode| d.diagnostics.iter().filter(|x| x.code == code).count();
    for code in [
        ErrorCode::OrphanMarker,
        ErrorCode::UnaddressedMarker,
        ErrorCode::MalformedMarker,
        ErrorCode::DuplicateId,
        ErrorCode::UnsupportedSyntax,
    ] {
        if count(c, code) > count(doc, code) {
            let d = c
                .diagnostics
                .iter()
                .find(|x| x.code == code)
                .expect("counted");
            return Err(invalid(format!(
                "the write would introduce a marker fault: {}",
                d.message
            )));
        }
    }
    let dropped: BTreeSet<&String> = changes.iter().flat_map(|c| c.dropped.iter()).collect();
    let adopted: BTreeSet<&String> = changes.iter().flat_map(|c| c.adopted.iter()).collect();
    for (id, targets) in &doc.ids {
        let [target] = targets.as_slice() else {
            continue;
        };
        if matches!(target, Target::Document) || dropped.contains(id) || doc.conflicted.contains(id)
        {
            continue;
        }
        let kind = if adopted.contains(id) {
            "item"
        } else {
            kind_of(*target)
        };
        match c.ids.get(id).map(Vec::as_slice) {
            Some([t]) if kind_of(*t) == kind => {}
            _ => return Err(invalid(format!("stay {id} would be lost or re-attached"))),
        }
    }
    for (id, kind) in &minted {
        let expected = if *kind == "list" { "block" } else { kind };
        match c.ids.get(*id).map(Vec::as_slice) {
            Some([t]) if kind_of(*t) == expected => {}
            _ => {
                return Err(invalid(format!(
                    "minted stay {id} did not attach as a {kind}"
                )));
            }
        }
    }
    for id in c.ids.keys() {
        if dropped.contains(id) {
            return Err(invalid(format!(
                "stay {id} is dropped by this write and may not name new content"
            )));
        }
        if !doc.ids.contains_key(id) && !minted.contains_key(id) {
            return Err(invalid(format!("unexpected stay {id} in the candidate")));
        }
    }
    carrier_checks(doc, c, changes, map)?;
    for change in changes {
        change
            .expectation
            .take()
            .expect("intent checked before application")
            .check(doc, c, map)?;
    }
    Ok(())
}

fn dropped_markers(doc: &Document, c: &Document, changes: &[Change]) -> Result<(), Diagnostic> {
    let dropped: BTreeSet<&String> = changes.iter().flat_map(|c| c.dropped.iter()).collect();
    let invalid = |m| err(ErrorCode::InvalidContent, m);
    // Lexical, as markstay's drop check is: an id no live marker carries any
    // more must be one the batch declared, whatever it was attached to. This
    // holds across ops, so two regions that each kept a duplicate's other copy
    // as the survivor cannot remove both copies undeclared. Markers only: a
    // heading's `{#id}` naming the same id is not a marker carrying it.
    let carried = |d: &Document| -> BTreeSet<String> {
        d.markers
            .iter()
            .filter_map(|m| m.lexical.id.clone())
            .collect()
    };
    let after = carried(c);
    for id in carried(doc) {
        if !after.contains(&id) && !dropped.contains(&id) {
            return Err(invalid(format!(
                "stay {id} would leave the document without being declared dropped"
            )));
        }
    }
    Ok(())
}

/// §3.4 carrier text for a child stay: its list's raw source up to the marker,
/// answered by the list's index in `carriers`, which indexes the document the
/// marker belongs to. No blocker position: this scans the candidate, a document
/// the refusal stops from ever becoming a file, so any offset into it names a
/// position the caller cannot open. The reason already says which character,
/// and the diagnostic carries the stay's own id.
fn carrier_state(carriers: &Carriers, list: usize, marker: usize) -> Option<&'static str> {
    let end = carriers.doc().markers[marker].lexical.span.start;
    Some(carriers.refusal(list, end)?.reason)
}

/// markstay §3.4 over the candidate. A child stay this write placed, moved or
/// minted must sit behind plain carrier text; moving a stay is an insertion.
/// A stay the write left in place may not lose a plain carrier because of it.
fn carrier_checks(
    doc: &Document,
    c: &Document,
    changes: &[Change],
    map: &Mapper,
) -> Result<(), Diagnostic> {
    let placed: Vec<Span> = changes
        .iter()
        .flat_map(|ch| ch.regions.iter())
        .filter(|r| !matches!(r, ItemRegion::InPlace { .. }))
        .map(|r| r.resolve(map))
        .collect();
    let minted: BTreeSet<&String> = changes
        .iter()
        .flat_map(|ch| {
            ch.minted
                .iter()
                .map(|(id, _, _)| id)
                .chain(ch.imported.iter().map(|(id, _)| id))
        })
        .collect();
    // One index per document: the candidate for every child stay, the original
    // only for the stays the candidate refuses.
    let candidate = Carriers::new(c);
    let original = Carriers::new(doc);
    for (l, list) in c.lists.iter().enumerate() {
        for item in &list.items {
            for &m in &item.markers {
                let id = c.markers[m].lexical.id.clone().unwrap_or_default();
                let Some(reason) = carrier_state(&candidate, l, m) else {
                    continue;
                };
                let strict = placed.contains(&item.span) || minted.contains(&id);
                let refuse = strict
                    || match doc.resolve(&Selector::Id { id: id.clone() }) {
                        Ok(Target::Item { list: dl, item: di }) => doc.lists[dl].items[di]
                            .markers
                            .iter()
                            .find(|dm| doc.markers[**dm].lexical.id.as_deref() == Some(id.as_str()))
                            .is_some_and(|dm| carrier_state(&original, dl, *dm).is_none()),
                        _ => false,
                    };
                if refuse {
                    return Err(err(
                        ErrorCode::StampRefused,
                        format!(
                            "stay {id} would sit behind non-plain carrier text: {reason}; \
                             remove '<' or '\\' from the list before it"
                        ),
                    )
                    .with_id(id));
                }
            }
        }
    }
    Ok(())
}

/// Reject batches whose operations share a unit. Individually rewritten items
/// of one list may share it; reshaping a list excludes every other operation on it.
fn conflicts(doc: &Document, changes: &[Change]) -> Result<(), Diagnostic> {
    let list_of = |b: &usize| doc.blocks[*b].list;
    let whole_lists =
        |c: &Change| -> BTreeSet<usize> { c.blocks.iter().filter_map(list_of).collect() };
    for (a, x) in changes.iter().enumerate() {
        for y in &changes[a + 1..] {
            let (wx, wy) = (whole_lists(x), whole_lists(y));
            let partial =
                |c: &Change| -> BTreeSet<usize> { c.lists.union(&c.reshaped).copied().collect() };
            let (px, py) = (partial(x), partial(y));
            let list_clash = !px.is_disjoint(&wy)
                || !py.is_disjoint(&wx)
                || !x.reshaped.is_disjoint(&py)
                || !y.reshaped.is_disjoint(&px);
            let block_clash = x.blocks.intersection(&y.blocks).next().is_some();
            let item_clash = x.items.intersection(&y.items).next().is_some();
            if list_clash || block_clash || item_clash || (x.frontmatter && y.frontmatter) {
                return Err(err(
                    ErrorCode::InvalidRequest,
                    "two operations touch the same unit; combine them or split the batch",
                ));
            }
        }
    }
    Ok(())
}

/// Plan a batch against one preimage and return a verified candidate.
/// Every marker inside text a splice wrote must be one Rivet put there: minted
/// by the batch, or an existing stay a change copied past its own rewrite.
/// Anything else came from supplied content that read as no marker on its own
/// and goes live where it lands (a tab stop shifting under an item's indent, a
/// leading BOM, a title read as a fence), and is refused even when its id
/// already exists, since it would otherwise stand in for a copy the write
/// removed.
fn provenance(
    candidate: &Document,
    changes: &[Change],
    splices: &[Splice],
    map: &Mapper,
) -> Result<(), Diagnostic> {
    let mut allowed: BTreeMap<&str, usize> = BTreeMap::new();
    for change in changes {
        for id in change
            .carried
            .iter()
            .chain(change.minted.iter().map(|(id, _, _)| id))
            .chain(change.imported.iter().map(|(id, _)| id))
        {
            *allowed.entry(id.as_str()).or_default() += 1;
        }
    }
    let written: Vec<Span> = splices
        .iter()
        .filter(|s| !s.text.is_empty())
        .map(|s| {
            let start = map.splice_start(s.span);
            Span {
                start,
                end: start + s.text.len(),
            }
        })
        .collect();
    for m in &candidate.markers {
        let span = m.lexical.span;
        if !written
            .iter()
            .any(|w| span.start < w.end && w.start < span.end)
        {
            continue;
        }
        let id = m.lexical.id.as_deref().unwrap_or("");
        match allowed.get_mut(id) {
            Some(n) if *n > 0 => *n -= 1,
            _ => {
                return Err(err(
                    ErrorCode::InvalidContent,
                    format!(
                        "the write would place a stay marker ({}) that came from the supplied content; \
                         Rivet places and preserves them",
                        if id.is_empty() { "no id" } else { id }
                    ),
                )
                .with_span(span));
            }
        }
    }
    Ok(())
}

pub fn plan(doc: &Document, mut changes: Vec<Change>) -> Result<Outcome, Diagnostic> {
    check_write_blocker(doc)?;
    for change in &mut changes {
        change.preflight(doc)?;
    }
    let mut all: Vec<(usize, Splice)> = changes
        .iter()
        .enumerate()
        .flat_map(|(i, c)| c.splices.iter().cloned().map(move |s| (i, s)))
        .collect();
    all.sort_by_key(|(_, s)| (s.span.start, s.span.end));
    for pair in all.windows(2) {
        if pair[0].1.span.overlaps(pair[1].1.span) || pair[0].1.span.end > pair[1].1.span.start {
            return Err(err(
                ErrorCode::InvalidRequest,
                "operations overlap; split them or target disjoint units",
            ));
        }
    }
    conflicts(doc, &changes)?;
    let splices: Vec<Splice> = all.into_iter().map(|(_, s)| s).collect();
    let map = Mapper::new(&splices);
    let first = apply(&doc.source, &splices);
    let parsed = Document::parse(&first).map_err(|d| {
        err(
            ErrorCode::InvalidContent,
            format!("candidate does not parse: {}", d.message),
        )
    })?;
    provenance(&parsed, &changes, &splices, &map)?;
    let fill: BTreeSet<String> = changes
        .iter()
        .flat_map(|c| {
            c.minted
                .iter()
                .map(|(id, _, _)| id.clone())
                .chain(c.adopted.iter().cloned())
        })
        .collect();
    let (refreshes, evidence) = refresh(doc, &parsed, &fill, &map)?;
    let candidate = apply(&first, &refreshes);
    let c = Document::parse(&candidate).map_err(|d| {
        err(
            ErrorCode::InvalidContent,
            format!("candidate does not parse: {}", d.message),
        )
    })?;
    dropped_markers(doc, &c, &changes)?;
    verify(doc, &c, &mut changes, &map)?;
    let ops = changes
        .iter()
        .map(|change| {
            let revision = match &change.follow {
                Some(Follow::Id(id)) => c
                    .resolve(&Selector::Id { id: id.clone() })
                    .ok()
                    .map(|t| c.target_revision(t)),
                Some(Follow::FrontmatterKey(key)) => c
                    .resolve(&Selector::FrontmatterKey { key: key.clone() })
                    .ok()
                    .map(|t| c.target_revision(t)),
                Some(Follow::Custom(_, revision)) => revision(&c),
                Some(Follow::Placed { region, typed }) => {
                    candidate_item(&c, &map, region).map(|(list, item)| match typed {
                        Some(bound) => crate::schema::item_revision(&c, bound, list, item),
                        None => c.target_revision(Target::Item { list, item }),
                    })
                }
                Some(Follow::Section(start)) => {
                    let pos = map.map(*start, true);
                    c.sections
                        .iter()
                        .position(|x| c.blocks[x.block].span.start == pos)
                        .map(|i| c.target_revision(Target::Section(i)))
                }
                None => None,
            };
            let placed = match &change.follow {
                Some(Follow::Placed { region, .. }) => {
                    candidate_item(&c, &map, region).map(|(l, i)| c.address_item(l, i))
                }
                _ => None,
            };
            OpOutcome {
                id: match &change.follow {
                    Some(Follow::Id(id) | Follow::Custom(id, _)) => Some(id.clone()),
                    Some(Follow::Placed { .. }) => {
                        placed.as_ref().and_then(|a| a.id()).map(str::to_string)
                    }
                    _ => None,
                },
                select: placed.as_ref().map(|a| a.select()),
                addressing: placed.as_ref().map(|a| a.label().to_string()),
                revision,
                collection_revision: change.collection.as_ref().and_then(|f| f(&c)),
                minted: change
                    .minted
                    .iter()
                    .map(|(id, kind, _)| Minted {
                        id: id.clone(),
                        kind: kind.to_string(),
                        line: c
                            .ids
                            .get(id)
                            .and_then(|t| t.first())
                            .map(|t| c.file_line(c.target_span(*t).0.start))
                            .unwrap_or(0),
                    })
                    .collect(),
                dropped: change.dropped.clone(),
                refused: change.refused.clone(),
                refreshed: change.refreshed.clone(),
            }
        })
        .collect();
    let mut evidence = evidence;
    evidence.extend(
        changes
            .iter()
            .flat_map(|c| c.refreshed.iter())
            .filter(|r| r.status == "refreshed")
            .map(|r| Evidence {
                id: r.id.clone(),
                key: r.key.clone(),
                stored: r.stored.clone(),
                refreshed: r.fresh.clone(),
                prior_drift: true,
                rewritten: true,
                requested: true,
            }),
    );
    Ok(Outcome {
        candidate,
        ops,
        evidence,
    })
}

/// Where an archived item lands in a section's own body: appended to the last
/// own-body block when that is a list the item joins (same marker delimiter),
/// otherwise as a new list at the end of the own body, before any child
/// section. Planning, the allowed edge and the reshaped-list declaration all
/// read this one rule.
pub(crate) fn archive_item_slot(
    doc: &Document,
    section: usize,
    text: &str,
) -> (Option<usize>, usize) {
    let sec = &doc.sections[section];
    let delimiter = |at: &str| at.lines().next().and_then(crate::document::list_delimiter);
    let list = doc
        .blocks
        .iter()
        .rfind(|b| b.span.start > sec.heading.start && b.span.start < sec.own_end)
        .and_then(|b| b.list)
        .filter(|l| {
            let first = doc.lists[*l].items[0].span.start;
            delimiter(&doc.source[first..]).is_some()
                && delimiter(&doc.source[first..]) == delimiter(text)
        });
    let at = list.map_or(sec.own_end, |l| {
        doc.lists[l].items.last().expect("parsed list").span.end
    });
    (list, at)
}

/// Copy only bytes already owned by a guarded source item. No caller text enters
/// the provenance allowance. Ordinary marker-bearing paste remains refused.
pub(crate) fn archive_item(
    source: &Document,
    target: Target,
    destination: &Document,
    section: usize,
    guard: &str,
) -> Result<Outcome, Diagnostic> {
    let Target::Item { list, item } = target else {
        return Err(err(
            ErrorCode::ArchiveCopyRefused,
            "archive source must be an item",
        ));
    };
    check_write_blocker(source)?;
    let intent_guard = Guard::new(GuardSubject::Native(Target::Section(section)), guard);
    intent_guard.check(destination)?;
    let it = &source.lists[list].items[item];
    let ids = source.stay_ids(&it.markers);
    if ids.is_empty() {
        return Err(err(
            ErrorCode::ArchiveCopyRefused,
            "archive source must have an item-owned stay",
        ));
    }
    if source.newline != destination.newline {
        return Err(err(
            ErrorCode::ArchiveCopyRefused,
            "archive copy requires the same newline style",
        ));
    }
    crate::native::markers(source, target)?;
    for id in &ids {
        if destination.ids.contains_key(id) {
            return Err(err(
                ErrorCode::ArchiveCopyRefused,
                format!("archive already carries {id}"),
            ));
        }
    }
    let text = it
        .span
        .slice(&source.source)
        .expect("parsed extent")
        .to_string();
    let (list, at) = archive_item_slot(destination, section, &text);
    let before = &destination.source[..at];
    let nl = destination.newline;
    let mut inserted = String::new();
    if !before.ends_with('\n') {
        inserted.push_str(nl);
    }
    if !before.ends_with(&format!("{nl}{nl}")) {
        inserted.push_str(nl);
    }
    inserted.push_str(&text);
    if list.is_none() {
        inserted.push_str(nl);
        if at < destination.source.len() {
            inserted.push_str(nl);
        }
    }
    let span = Span { start: at, end: at };
    let mut change = Change {
        splices: vec![Splice {
            span,
            text: inserted,
        }],
        ..Change::default()
    };
    change.guard = Some(intent_guard);
    change.expectation = Some(Expectation::Archive {
        unit: crate::archive::Unit::Item,
        section,
        at,
        text: text.clone(),
        ids: ids.clone(),
    });
    change.imported = ids.iter().cloned().map(|id| (id, "item")).collect();
    let outcome = plan(destination, vec![change])?;
    Ok(outcome)
}

/// Append an authenticated section subtree without changing any copied byte.
pub(crate) fn archive_section(
    source: &Document,
    source_section: usize,
    id: &str,
    destination: &Document,
    section: usize,
    guard: &str,
) -> Result<Outcome, Diagnostic> {
    let refuse = |message| err(ErrorCode::ArchiveCopyRefused, message);
    check_write_blocker(source)?;
    let intent_guard = Guard::new(GuardSubject::Native(Target::Section(section)), guard);
    intent_guard.check(destination)?;
    if source.sections[source_section].level != destination.sections[section].level + 1 {
        return Err(refuse(
            "archive section must be exactly one heading level below the destination",
        ));
    }
    if source.newline != destination.newline {
        return Err(refuse("archive copy requires the same newline style"));
    }
    let target = Target::Section(source_section);
    let ids = crate::archive::unit_ids(source, target);
    for id in &ids {
        if destination.ids.contains_key(id) {
            return Err(refuse("archive already carries a copied stay"));
        }
    }
    let content = source.section_content_span(source_section)?;
    let text = content
        .slice(&source.source)
        .expect("content extent")
        .to_string();
    // The unowned blank lines the content extent leaves out of the source unit.
    let tail = &source.source[content.end..source.sections[source_section].whole.end];
    let at = destination.sections[section].whole.end;
    let before = &destination.source[..at];
    let nl = destination.newline;
    let mut inserted = String::new();
    if !before.ends_with('\n') {
        inserted.push_str(nl);
    }
    if !before.ends_with(&format!("{nl}{nl}")) {
        inserted.push_str(nl);
    }
    inserted.push_str(&text);
    // The content already ends in its original terminator. The separator is
    // outside the extent and stays outside it when a later sibling is appended.
    // At end of file nothing follows, so the source's own trailing blank lines
    // end the unit there and its native revision equals the frozen source guard.
    if at < destination.source.len() {
        inserted.push_str(nl);
    } else {
        inserted.push_str(tail);
    }
    let mut change = Change {
        splices: vec![Splice {
            span: Span { start: at, end: at },
            text: inserted,
        }],
        ..Change::default()
    };
    change.guard = Some(intent_guard);
    change.expectation = Some(Expectation::Archive {
        unit: crate::archive::Unit::from_source(source, target, id)?,
        section,
        at,
        text,
        ids,
    });
    plan(destination, vec![change])
}
