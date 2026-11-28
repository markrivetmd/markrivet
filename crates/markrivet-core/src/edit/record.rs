//! File-record writes: every key edit one `update_item` asks of a record,
//! derived from the preimage and the request alone. Planning and the intent
//! gate both call [`record_edits`], so the gate never trusts the planner's
//! change.
use super::intent::{preserve_frontmatter, spliced_frontmatter};
use super::*;
use crate::schema::{self, Bound, FieldSpec, FieldType};
use serde_json::Map;

/// What a record change is checked against: the request itself, so the gate
/// re-derives every splice, and the fields the record must read back as.
pub(crate) struct RecordExpectation {
    pub bound: Bound,
    pub path: String,
    pub set: BTreeMap<String, Value>,
    pub expect: Map<String, Value>,
}

/// The splices a record write makes and what its frontmatter reads as after.
pub(crate) struct RecordEdits {
    /// Sorted by preimage start; empty when every value is already current.
    pub splices: Vec<Splice>,
    /// The frontmatter's keys and values after the write, in order: the
    /// preimage's minus removed keys, then added keys by name.
    pub keys: Vec<(String, Value)>,
    /// Keys no edit touches, with their values and source lines.
    pub others: Vec<(String, Value, String)>,
    /// A key is removed, so a neighbour may gain its trailing comments.
    pub removes: bool,
}

/// One requested field, checked.
pub(crate) struct Want<'a> {
    pub name: &'a str,
    pub spec: &'a FieldSpec,
    pub value: &'a Value,
}

fn unsupported(message: String) -> Diagnostic {
    err(ErrorCode::UnsupportedSyntax, message)
}

/// The request checked in D7's order up to the no-op test: every field named
/// is one of the schema's, its source form can take the write whatever the
/// value, and each value fits its type. The frontmatter must exist and read.
pub(crate) fn record_request<'a>(
    doc: &Document,
    bound: &'a Bound,
    set: &'a BTreeMap<String, Value>,
) -> Result<Vec<Want<'a>>, Diagnostic> {
    let scalars = record_scalars(doc)?;
    let mut wants = vec![];
    for (name, value) in set {
        let spec = bound.schema.fields.get(name).ok_or_else(|| {
            err(
                ErrorCode::InvalidRequest,
                format!("{name} is not a field of {}", bound.schema.id),
            )
        })?;
        let key = spec.key.as_deref().expect("a file schema reads keys");
        let k = scalars.iter().position(|s| s.key == key);
        legal(doc, name, spec, k, value)?;
        wants.push(Want {
            name: name.as_str(),
            spec,
            value,
        });
    }
    for want in &wants {
        if want.value.is_null() {
            if want.spec.required {
                return Err(err(
                    ErrorCode::InvalidItem,
                    format!("{} is required", want.name),
                ));
            }
        } else {
            schema::check_value(want.spec, want.value)
                .map_err(|m| err(ErrorCode::InvalidItem, format!("{}: {m}", want.name)))?;
        }
    }
    Ok(wants)
}

/// The frontmatter's fields, or the refusal a record write gives when it has
/// none or they do not read.
pub(crate) fn record_scalars(doc: &Document) -> Result<&[yaml::Scalar], Diagnostic> {
    if doc.frontmatter.payload.is_none() {
        return Err(unsupported(
            "the file has no frontmatter; a record write edits frontmatter keys".into(),
        ));
    }
    match &doc.frontmatter.fields {
        Ok(fields) => Ok(fields),
        Err(d) => {
            let mut d = d.clone();
            d.code = ErrorCode::UnsupportedSyntax;
            d.message = format!("the record's frontmatter does not read: {}", d.message);
            Err(d)
        }
    }
}

/// Whether the field's source form can take a write of this kind, by the
/// rule the matching native op applies. Only `null` against a value changes
/// the kind of write; an identical value is still a write of its form.
fn legal(
    doc: &Document,
    name: &str,
    spec: &FieldSpec,
    k: Option<usize>,
    value: &Value,
) -> Result<(), Diagnostic> {
    let key = spec.key.as_deref().expect("a file schema reads keys");
    if spec.kind == FieldType::Entries {
        return Err(unsupported(format!(
            "field {name} holds entries, which a record write appends to rather than sets"
        )));
    }
    if let Some(field) = spec.field.as_deref() {
        if value.is_null() {
            return Err(unsupported(format!(
                "field {name} reads {key}.{field}; a mapping field is set, never removed"
            )));
        }
        let k = k.ok_or_else(|| {
            err(
                ErrorCode::MissingSelector,
                format!("frontmatter key {key} does not exist; a record write adds no mapping"),
            )
        })?;
        mapping_field_site(doc, k, field)?;
        return Ok(());
    }
    let (Some(k), false) = (k, value.is_null()) else {
        // An absent key is added; a present one with `null` is removed.
        return Ok(());
    };
    if spec.kind == FieldType::List {
        key_token_site(doc, k).map_err(|_| {
            unsupported(format!(
                "field {name}: frontmatter key {key} is not a single-line flow list; a record \
                 write replaces only those"
            ))
        })?;
        if !doc.scalars()[k].value.is_array() {
            return Err(unsupported(format!(
                "field {name}: frontmatter key {key} does not hold a list"
            )));
        }
        return Ok(());
    }
    writable_scalar(doc, k).map(|_| ())
}

/// The value a field holds now, before defaults: `Null` when absent.
fn current(doc: &Document, spec: &FieldSpec) -> Value {
    let key = spec.key.as_deref().expect("a file schema reads keys");
    let Some(s) = doc.scalars().iter().find(|s| s.key == key) else {
        return Value::Null;
    };
    match spec.field.as_deref() {
        None => s.value.clone(),
        Some(field) => s.value.get(field).cloned().unwrap_or(Value::Null),
    }
}

/// Every splice the request makes, derived from the preimage (D7). Each
/// touched key yields one edit inside its own extent; added keys share one
/// insertion at the payload end, by key name; a field whose value is current
/// is left alone.
pub(crate) fn record_edits(
    doc: &Document,
    bound: &Bound,
    set: &BTreeMap<String, Value>,
) -> Result<RecordEdits, Diagnostic> {
    let wants = record_request(doc, bound, set)?;
    let scalars = record_scalars(doc)?;
    let payload = doc.frontmatter.payload.expect("checked by record_scalars");
    let mut by_key: BTreeMap<&str, Vec<&Want>> = BTreeMap::new();
    for want in &wants {
        if *want.value != current(doc, want.spec) {
            let key = want.spec.key.as_deref().expect("a file schema reads keys");
            by_key.entry(key).or_default().push(want);
        }
    }
    let mut splices = vec![];
    let mut touched: BTreeMap<&str, Value> = BTreeMap::new();
    let mut removed = BTreeSet::new();
    let mut added: Vec<(&str, String, Value)> = vec![];
    for (key, group) in by_key {
        let k = scalars.iter().position(|s| s.key == key);
        if group[0].spec.field.is_some() {
            let k = k.expect("checked by legal");
            let parent = &scalars[k];
            let mut combined = parent.value.clone();
            let mut tokens = vec![];
            for want in &group {
                let field = want.spec.field.as_deref().expect("a field group");
                let (splice, _) = mapping_field_token(doc, k, field, want.value)?;
                combined[field] = want.value.clone();
                tokens.push(splice);
            }
            tokens.sort_by_key(|s| s.span.start);
            let lines = parent.line;
            let mut text = String::new();
            let mut at = lines.start;
            for t in &tokens {
                text.push_str(&doc.source[at..t.span.start]);
                text.push_str(&t.text);
                at = t.span.end;
            }
            text.push_str(&doc.source[at..lines.end]);
            if yaml::collection_value(&text).as_ref() != Some(&combined) {
                return Err(unsupported(format!(
                    "the fields of frontmatter key {key} would not read back together; the \
                     write is refused"
                )));
            }
            splices.extend(tokens);
            touched.insert(key, combined);
            continue;
        }
        let [want] = group.as_slice() else {
            unreachable!("schema validation lets one field read a key whole")
        };
        match (k, want.value) {
            (Some(k), Value::Null) => {
                splices.push(Splice {
                    span: frontmatter_key_lines(doc, k),
                    text: String::new(),
                });
                removed.insert(key);
            }
            (Some(k), Value::Array(items)) => {
                let raw = yaml::render_flow_list(items).ok_or_else(unrenderable)?;
                let write = yaml_set(doc, k, &raw, false)?;
                splices.push(write.splice);
                touched.insert(key, write.expect);
            }
            (Some(k), value) => {
                splices.push(scalar_token(doc, k, value)?);
                touched.insert(key, value.clone());
            }
            (None, Value::Array(items)) => {
                let raw = yaml::render_flow_list(items).ok_or_else(unrenderable)?;
                let write = yaml_add(doc, key, &raw)?;
                added.push((key, write.splice.text, write.expect));
            }
            (None, value) => {
                let (_, text) = new_key_line(doc, key, value)?;
                added.push((key, text, value.clone()));
            }
        }
    }
    keys_remain(scalars.len() - removed.len() + added.len())?;
    if !added.is_empty() {
        // Added keys share one insertion at the payload end. A removal that
        // ends there, then the insertion, then a removal that starts there
        // become one replacement whose text is the insertion.
        let at = payload.end;
        let mut span = Span { start: at, end: at };
        let removal = |s: &Splice| s.text.is_empty() && !s.span.is_empty();
        if let Some(i) = splices.iter().position(|s| removal(s) && s.span.end == at) {
            span.start = splices.remove(i).span.start;
        }
        if let Some(i) = splices
            .iter()
            .position(|s| removal(s) && s.span.start == at)
        {
            span.end = splices.remove(i).span.end;
        }
        splices.push(Splice {
            span,
            text: added.iter().map(|(_, text, _)| text.as_str()).collect(),
        });
    }
    splices.sort_by_key(|s| (s.span.start, s.span.end));
    if splices.windows(2).any(|w| w[0].span.end > w[1].span.start) {
        return Err(err(
            ErrorCode::InvalidRequest,
            "the record's key edits overlap; the write is refused",
        ));
    }
    let mut keys: Vec<(String, Value)> = scalars
        .iter()
        .filter(|s| !removed.contains(s.key.as_str()))
        .map(|s| {
            let value = touched.get(s.key.as_str()).unwrap_or(&s.value);
            (s.key.clone(), value.clone())
        })
        .collect();
    keys.extend(added.iter().map(|(k, _, v)| (k.to_string(), v.clone())));
    let others = frontmatter_others(doc, None)
        .into_iter()
        .filter(|(k, _, _)| !touched.contains_key(k.as_str()) && !removed.contains(k.as_str()))
        .collect();
    let edits = RecordEdits {
        splices,
        keys,
        others,
        removes: !removed.is_empty(),
    };
    edits.reads_back(doc, payload)?;
    Ok(edits)
}

impl RecordEdits {
    /// The candidate payload reads, through the unchanged frontmatter reader,
    /// as exactly the expected keys and values; refused before any byte
    /// changes otherwise.
    fn reads_back(&self, doc: &Document, payload: Span) -> Result<(), Diagnostic> {
        let mut candidate = String::new();
        let mut at = payload.start;
        for s in &self.splices {
            candidate.push_str(&doc.source[at..s.span.start]);
            candidate.push_str(&s.text);
            at = s.span.end;
        }
        candidate.push_str(&doc.source[at..payload.end]);
        let refuse = || {
            unsupported(
                "the record's frontmatter would not read back as written; the write is refused"
                    .into(),
            )
        };
        let fields = yaml::read_payload(&candidate, payload.start).map_err(|_| refuse())?;
        let got: Vec<(String, Value)> = fields.into_iter().map(|s| (s.key, s.value)).collect();
        (got == self.keys).then_some(()).ok_or_else(refuse)
    }
}

impl RecordExpectation {
    pub(crate) fn edits(&self, doc: &Document) -> Result<RecordEdits, Diagnostic> {
        record_edits(doc, &self.bound, &self.set)
    }

    /// The change's splices are exactly the derived ones, spans and texts,
    /// before any candidate is built. This is what refuses a byte-equivalent
    /// forgery that containment and the candidate comparison would accept.
    pub(crate) fn validate(&self, doc: &Document, splices: &[Splice]) -> Result<(), Diagnostic> {
        let edits = self.edits(doc)?;
        let same = splices.len() == edits.splices.len()
            && splices
                .iter()
                .zip(&edits.splices)
                .all(|(a, b)| a.span == b.span && a.text == b.text);
        same.then_some(()).ok_or_else(|| {
            err(
                ErrorCode::InvalidRequest,
                "a record write makes exactly the splices its request derives",
            )
        })
    }

    pub(crate) fn allowed(&self, doc: &Document) -> Result<Vec<Span>, Diagnostic> {
        Ok(self.edits(doc)?.splices.iter().map(|s| s.span).collect())
    }

    /// The candidate keeps every byte before the frontmatter, its frontmatter
    /// is the preimage's with exactly the derived splices applied, its keys
    /// read back in the derived order and values, every untouched key keeps
    /// its value and source lines, and the record re-reads as expected. The
    /// body is the batch's other expectations' business.
    pub(crate) fn check(self, doc: &Document, c: &Document) -> Result<(), Diagnostic> {
        let edits = self.edits(doc)?;
        spliced_frontmatter(doc, c, &edits.splices, "the record write")?;
        let fields = c.frontmatter.fields.as_ref().map_err(Clone::clone)?;
        let got: Vec<(String, Value)> = fields
            .iter()
            .map(|s| (s.key.clone(), s.value.clone()))
            .collect();
        if got != edits.keys {
            return Err(err(
                ErrorCode::InvalidContent,
                "the record's frontmatter did not read back as written",
            ));
        }
        preserve_frontmatter(c, &edits.others, edits.removes)?;
        let read = schema::record(c, &self.bound).map_err(|e| e.diagnostic())?;
        if read != self.expect {
            return Err(err(
                ErrorCode::InvalidItem,
                "the record would not read back as asked",
            ));
        }
        Ok(())
    }
}
