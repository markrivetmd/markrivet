//! Versioned, read-only analytics. Item write guards never include these definitions.
use crate::{
    Diagnostic, Document, ErrorCode,
    fs::Project,
    schema::{self, Bound, Clause, Cmp, FieldSpec, FieldType, Source},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

pub mod bundle;

pub const CONTRACT: u32 = 1;
pub const MAX_ROWS: usize = 100_000;
pub const MAX_GROUPS: usize = 10_000;

pub(crate) fn bad(message: impl Into<String>) -> Diagnostic {
    Diagnostic::new(ErrorCode::InvalidRequest, message)
}
pub(crate) fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
pub(crate) fn plain(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Metadata {
    #[serde(rename = "type")]
    pub kind: FieldType,
    pub key: Vec<String>,
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub nullable: bool,
    #[serde(default)]
    pub values: Vec<String>,
    // A map entry distinguishes an absent default from an explicit null default.
    #[serde(flatten)]
    pub options: BTreeMap<String, Value>,
}
impl Metadata {
    fn spec(&self) -> FieldSpec {
        FieldSpec {
            kind: self.kind,
            from: Source::Label,
            required: self.required,
            values: self.values.clone(),
            tags: vec![],
            label: None,
            multiline: false,
            default: None,
        }
    }
    fn validate(&self, v: &Value) -> Result<(), String> {
        if v.is_null() && self.nullable {
            Ok(())
        } else {
            schema::check_value(&self.spec(), v)
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Declaration {
    pub version: u32,
    pub collection: String,
    pub fields: BTreeMap<String, Metadata>,
}
impl Declaration {
    fn validate(&self, collection: &str) -> Result<(), Diagnostic> {
        if self.version != CONTRACT || self.collection != collection {
            return Err(bad("analytics version or collection mismatch"));
        }
        for (name, f) in &self.fields {
            if name == "path"
                || !plain(name)
                || f.key.is_empty()
                || f.key.iter().any(|k| k.is_empty())
                || f.options.keys().any(|k| k != "default")
            {
                return Err(bad(format!("invalid metadata declaration {name}")));
            }
            if (f.kind == FieldType::Enum) == f.values.is_empty()
                || f.values.iter().collect::<BTreeSet<_>>().len() != f.values.len()
            {
                return Err(bad(format!("invalid enum declaration {name}")));
            }
            if let Some(d) = f.options.get("default") {
                f.validate(d)
                    .map_err(|e| bad(format!("metadata default {name}: {e}")))?;
            }
        }
        Ok(())
    }
}
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RowSource {
    #[default]
    Items,
    Documents,
}
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    #[default]
    Strict,
    Diagnostic,
}
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MissingBinding {
    #[default]
    Refuse,
    Allow,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum Measure {
    Count,
    CountWhere {
        #[serde(rename = "where")]
        clauses: Vec<Clause>,
    },
    CountDistinct {
        field: String,
    },
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Query {
    pub collection: String,
    #[serde(default)]
    pub analytics: Option<String>,
    #[serde(default)]
    pub source: RowSource,
    #[serde(default)]
    pub mode: Mode,
    #[serde(default)]
    pub missing_binding: MissingBinding,
    #[serde(default, rename = "where")]
    pub clauses: Vec<Clause>,
    #[serde(default)]
    pub fields: Option<Vec<String>>,
    #[serde(default)]
    pub paths: Option<Vec<String>>,
    #[serde(default)]
    pub group_by: Vec<String>,
    #[serde(default)]
    pub measures: BTreeMap<String, Measure>,
    #[serde(default)]
    pub facets: Vec<String>,
    #[serde(default)]
    pub zero_enums: bool,
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub offset: usize,
    #[serde(default)]
    pub top: Option<usize>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct View {
    pub version: u32,
    pub query: Query,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Envelope {
    pub presence: String,
    pub origin: String,
    pub validity: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw: Option<Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<String>,
}
impl Envelope {
    fn effective(
        raw: Option<Value>,
        default: Option<Value>,
        required: bool,
        validate: impl Fn(&Value) -> Result<(), String>,
    ) -> Self {
        let origin = if raw.is_some() {
            "authored"
        } else if default.is_some() {
            "defaulted"
        } else {
            "none"
        };
        let value = raw.or(default);
        let errors = match &value {
            Some(v) => validate(v).err().into_iter().collect(),
            None if required => vec!["required field missing".into()],
            None => vec![],
        };
        let valid = errors.is_empty();
        Self {
            presence: if value.is_some() {
                "present"
            } else {
                "missing"
            }
            .into(),
            origin: origin.into(),
            validity: if valid { "valid" } else { "invalid" }.into(),
            value: if valid { value.clone() } else { None },
            raw: if valid { None } else { value },
            errors,
        }
    }
    fn unsupported() -> Self {
        Self {
            presence: "unknown".into(),
            origin: "none".into(),
            validity: "unsupported".into(),
            value: None,
            raw: None,
            errors: vec!["source unsupported".into()],
        }
    }
    fn authored(value: Value) -> Self {
        Self::effective(Some(value), None, false, |_| Ok(()))
    }
    pub fn bucket(&self) -> Value {
        match (self.presence.as_str(), self.validity.as_str(), &self.value) {
            ("unknown", "unsupported", None) => json!({"state":"unsupported"}),
            ("present" | "missing", "invalid", None) => json!({"state":"invalid"}),
            ("missing", "valid", None) => json!({"state":"missing"}),
            ("present", "valid", Some(Value::Null)) => json!({"state":"null"}),
            ("present", "valid", Some(v)) if v.is_string() || v.is_boolean() || v.is_number() => {
                json!({"state":"value","type":if v.is_string(){"string"}else if v.is_boolean(){"boolean"}else{"number"},"value":v})
            }
            _ => unreachable!("envelopes are constructed by validated extraction"),
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Row {
    pub path: String,
    pub line: usize,
    pub id: Option<String>,
    pub revision: String,
    pub binding: String,
    pub fields: BTreeMap<String, Envelope>,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Coverage {
    pub files_discovered: usize,
    pub files_read: usize,
    pub unreadable_files: usize,
    pub failed_files: usize,
    pub missing_bindings: usize,
    pub ambiguous_bindings: usize,
    pub candidate_items: usize,
    pub valid_items: usize,
    pub invalid_items: usize,
    pub candidate_rows: usize,
    pub valid_rows: usize,
    pub invalid_rows: usize,
    pub matching_items: usize,
    pub matching_rows: usize,
    pub nonmatching_rows: usize,
    pub unknown_predicates: usize,
    pub addressable_items: usize,
    pub duplicate_ids: usize,
    pub reads: usize,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Truth {
    True,
    False,
    Unknown,
}
pub fn predicate(fields: &BTreeMap<String, Envelope>, clauses: &[Clause]) -> Truth {
    let mut unknown = false;
    for c in clauses {
        let (name, origin) = if fields.contains_key(&c.field) {
            (c.field.as_str(), false)
        } else {
            c.field
                .strip_suffix(".origin")
                .map_or((c.field.as_str(), false), |s| (s, true))
        };
        let e = &fields[name];
        let truth = if origin {
            let map = serde_json::Map::from_iter([(c.field.clone(), json!(e.origin))]);
            if schema::matches(&map, None, c) {
                Truth::True
            } else {
                Truth::False
            }
        } else if matches!(c.op, Cmp::Exists | Cmp::Missing) {
            match e.presence.as_str() {
                "unknown" => Truth::Unknown,
                "present" => {
                    if c.op == Cmp::Exists {
                        Truth::True
                    } else {
                        Truth::False
                    }
                }
                _ => {
                    if c.op == Cmp::Missing {
                        Truth::True
                    } else {
                        Truth::False
                    }
                }
            }
        } else if e.validity != "valid" {
            Truth::Unknown
        } else {
            let map = serde_json::Map::from_iter(e.value.clone().map(|v| (c.field.clone(), v)));
            if schema::matches(&map, None, c) {
                Truth::True
            } else {
                Truth::False
            }
        };
        if truth == Truth::False {
            return Truth::False;
        }
        unknown |= truth == Truth::Unknown;
    }
    if unknown { Truth::Unknown } else { Truth::True }
}

pub(crate) struct Context {
    pub bound: Bound,
    pub declaration: Option<Declaration>,
    pub definitions: BTreeMap<String, Vec<u8>>,
    pub view: Option<String>,
}
impl Context {
    pub(crate) fn load(
        project: &Project,
        q: &Query,
        view: Option<(&str, Vec<u8>)>,
    ) -> Result<Self, Diagnostic> {
        let mut definitions = BTreeMap::new();
        let bound = schema::bind_with(&q.collection, |parts| {
            let bytes = project.read_config(parts)?;
            if let Some(bytes) = &bytes {
                definitions.insert(parts.join("/"), bytes.clone());
            }
            Ok(bytes)
        })?;
        crate::fs::hook("analytics_config")?;
        let declaration = if let Some(name) = &q.analytics {
            if !plain(name) {
                return Err(bad("analytics name must be plain"));
            }
            let bytes = project
                .read_config(&["analytics", &format!("{name}.json")])?
                .ok_or_else(|| bad("analytics declaration not found"))?;
            let d: Declaration = serde_json::from_slice(&bytes)
                .map_err(|e| bad(format!("analytics declaration: {e}")))?;
            d.validate(&q.collection)?;
            definitions.insert(format!("analytics/{name}.json"), bytes);
            Some(d)
        } else {
            None
        };
        let view_name = view.as_ref().map(|(n, _)| n.to_string());
        if let Some((name, bytes)) = view {
            definitions.insert(format!("views/{name}.json"), bytes);
        }
        let ctx = Self {
            bound,
            declaration,
            definitions,
            view: view_name,
        };
        ctx.check(q)?;
        Ok(ctx)
    }
    fn spec(&self, q: &Query, name: &str) -> Result<FieldSpec, Diagnostic> {
        let direct = name == "document.path"
            || name
                .strip_prefix("item.")
                .is_some_and(|n| n == "id" || self.bound.schema.fields.contains_key(n))
            || name.strip_prefix("document.").is_some_and(|n| {
                self.declaration
                    .as_ref()
                    .is_some_and(|d| d.fields.contains_key(n))
            });
        let (name, origin) = if direct {
            (name, false)
        } else {
            name.strip_suffix(".origin")
                .map_or((name, false), |s| (s, true))
        };
        let spec = if let Some(n) = name.strip_prefix("item.") {
            if q.source != RowSource::Items {
                return Err(bad("item fields need item rows"));
            }
            if n == "id" {
                string_spec()
            } else {
                self.bound
                    .schema
                    .fields
                    .get(n)
                    .cloned()
                    .ok_or_else(|| bad(format!("unknown field {name}")))?
            }
        } else if name == "document.path" {
            string_spec()
        } else if let Some(n) = name.strip_prefix("document.") {
            self.declaration
                .as_ref()
                .and_then(|d| d.fields.get(n))
                .map(Metadata::spec)
                .ok_or_else(|| bad(format!("undeclared field {name}")))?
        } else {
            return Err(bad(format!("analytics field needs a namespace: {name}")));
        };
        if origin {
            Ok(FieldSpec {
                kind: FieldType::Enum,
                values: vec!["authored".into(), "defaulted".into(), "none".into()],
                ..string_spec()
            })
        } else {
            Ok(spec)
        }
    }
    fn clause(&self, q: &Query, c: &Clause) -> Result<(), Diagnostic> {
        let f = self.spec(q, &c.field)?;
        // Nullable metadata admits explicit null equality; no coercion otherwise.
        let nullable = c
            .field
            .strip_prefix("document.")
            .and_then(|n| self.declaration.as_ref()?.fields.get(n))
            .is_some_and(|m| m.nullable);
        if nullable && c.value.is_null() && matches!(c.op, Cmp::Eq | Cmp::Ne) {
            return Ok(());
        }
        let s = schema::Schema {
            id: "analytics".into(),
            version: CONTRACT,
            item: schema::ItemShape::Checkbox,
            fields: BTreeMap::from([(c.field.clone(), f)]),
        };
        schema::check_clause(&s, c)
    }
    fn check(&self, q: &Query) -> Result<(), Diagnostic> {
        if q.group_by.len() > 8
            || q.measures.len() > 32
            || q.facets.len() > 32
            || q.clauses.len() > 64
            || q.fields.as_ref().is_some_and(|f| f.len() > 128)
        {
            return Err(bad("query dimension limit exceeded"));
        }
        for c in &q.clauses {
            self.clause(q, c)?
        }
        for f in q
            .group_by
            .iter()
            .chain(q.fields.iter().flatten())
            .chain(q.facets.iter())
        {
            self.spec(q, f)?;
        }
        if q.facets.iter().collect::<BTreeSet<_>>().len() != q.facets.len() {
            return Err(bad("duplicate facet fields"));
        }
        for f in &q.facets {
            if self.spec(q, f)?.kind != FieldType::Enum {
                return Err(bad("facets require declared enums"));
            }
        }
        for m in q.measures.values() {
            match m {
                Measure::Count => {}
                Measure::CountWhere { clauses } => {
                    if clauses.len() > 64 {
                        return Err(bad("too many conditional clauses"));
                    }
                    for c in clauses {
                        self.clause(q, c)?
                    }
                }
                Measure::CountDistinct { field } => {
                    self.spec(q, field)?;
                }
            }
        }
        if let Some(paths) = &q.paths {
            if paths.len() > 10_000 {
                return Err(bad("too many paths"));
            }
            for p in paths {
                if !safe_document_path(p)
                    || !self.bound.config.files.iter().any(|g| schema::glob(g, p))
                {
                    return Err(bad(format!("path outside collection: {p}")));
                }
            }
        }
        if q.limit == Some(0)
            || q.top == Some(0)
            || q.limit.is_some_and(|v| v > 1000)
            || q.top.is_some_and(|v| v > MAX_GROUPS)
        {
            return Err(bad("invalid page limit or top"));
        }
        Ok(())
    }
}
fn string_spec() -> FieldSpec {
    FieldSpec {
        kind: FieldType::String,
        from: Source::Label,
        required: false,
        values: vec![],
        tags: vec![],
        label: None,
        multiline: false,
        default: None,
    }
}
pub(crate) fn safe_document_path(p: &str) -> bool {
    !p.is_empty()
        && !p.contains('\0')
        && p.split('/').all(|s| !s.is_empty() && s != "." && s != "..")
        && matches!(
            std::path::Path::new(p).extension().and_then(|s| s.to_str()),
            Some("md" | "markdown")
        )
}

fn metadata(doc: &Document, ctx: &Context) -> (BTreeMap<String, Envelope>, Vec<String>) {
    let mut fields = BTreeMap::from([("document.path".into(), Envelope::authored(json!("")))]);
    let mut errors = vec![];
    let Some(d) = &ctx.declaration else {
        return (fields, errors);
    };
    let unsupported = doc.frontmatter.fields.is_err()
        || unrecognized(&doc.source, doc.frontmatter.span.is_some());
    if unsupported && !d.fields.is_empty() {
        errors.push("unsupported or unrecognized frontmatter".into());
    }
    for (name, f) in &d.fields {
        let e = if unsupported {
            Envelope::unsupported()
        } else {
            let mut raw = doc
                .scalars()
                .iter()
                .find(|s| s.key == f.key[0])
                .map(|s| s.value.clone());
            let mut shape_error = false;
            for key in &f.key[1..] {
                raw = match raw {
                    Some(Value::Object(m)) => m.get(key).cloned(),
                    None => None,
                    Some(v) => {
                        shape_error = true;
                        Some(v)
                    }
                };
                if shape_error {
                    break;
                }
            }
            if shape_error {
                Envelope::effective(raw, None, f.required, |_| {
                    Err("metadata path crosses a non-mapping value".into())
                })
            } else {
                Envelope::effective(raw, f.options.get("default").cloned(), f.required, |v| {
                    f.validate(v)
                })
            }
        };
        errors.extend(e.errors.iter().map(|e| format!("document.{name}: {e}")));
        fields.insert(format!("document.{name}"), e);
    }
    (fields, errors)
}
pub fn unrecognized(source: &str, recognized: bool) -> bool {
    if recognized {
        return false;
    }
    let source = source.strip_prefix('\u{feff}').unwrap_or(source);
    let mut lines = source.lines();
    if !lines
        .next()
        .is_some_and(|s| s.trim_end_matches([' ', '\t', '\r']) == "---")
    {
        return false;
    }
    for line in lines {
        let line = line.trim_end_matches('\r');
        if line.trim().is_empty() || matches!(line.trim_end_matches([' ', '\t']), "---" | "...") {
            return false;
        }
        if crate::yaml::yamlish(line) {
            return true;
        }
    }
    false
}
fn item_fields(content: &str, ctx: &Context) -> BTreeMap<String, Envelope> {
    let raw = schema::discover(content, &ctx.bound.schema);
    ctx.bound
        .schema
        .fields
        .iter()
        .map(|(name, f)| {
            let e = match &raw {
                Ok(r) => Envelope::effective(r[name].clone(), f.default.clone(), f.required, |v| {
                    schema::check_value(f, v)
                }),
                Err(_) => Envelope::unsupported(),
            };
            (format!("item.{name}"), e)
        })
        .collect()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Outcome {
    pub path: String,
    pub status: String,
    pub error: Option<Diagnostic>,
    pub source_hash: Option<String>,
    pub member: Option<String>,
}
/// Bound scan diagnostics before retaining them, including filtered-out rows.
#[derive(Default, Serialize)]
#[serde(transparent)]
pub(crate) struct Diagnostics {
    entries: Vec<Diagnostic>,
    #[serde(skip)]
    bytes: usize,
}
impl Diagnostics {
    fn push(&mut self, diagnostic: Diagnostic) -> Result<(), Diagnostic> {
        let bytes = serde_json::to_vec(&diagnostic).unwrap().len();
        if bytes > 64 * 1024
            || self.bytes + bytes > 8 * 1024 * 1024
            || self.entries.len() >= 100_000
        {
            return Err(Diagnostic::new(
                ErrorCode::LimitExceeded,
                "analytics diagnostic count, message or byte cap exceeded",
            ));
        }
        self.bytes += bytes;
        self.entries.push(diagnostic);
        Ok(())
    }
    fn iter(&self) -> std::slice::Iter<'_, Diagnostic> {
        self.entries.iter()
    }
}
pub(crate) struct Scan {
    pub coverage: Coverage,
    pub diagnostics: Diagnostics,
    pub events: Vec<crate::fs::WalkEvent>,
    pub outcomes: Vec<Outcome>,
    pub captures: Vec<(String, Vec<u8>)>,
    pub fingerprint: String,
    pub start: String,
    pub end: String,
}
fn now() -> String {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock after epoch")
        .as_millis()
        .to_string()
}

pub(crate) struct Collector {
    pub rows: Vec<Row>,
    pub groups: BTreeMap<String, Group>,
    pub facets: BTreeMap<String, BTreeMap<String, (Value, usize)>>,
    pub keep_rows: bool,
    pub state_bytes: usize,
}
#[derive(Default)]
pub(crate) struct Group {
    pub key: Vec<Value>,
    pub count: usize,
    pub measures: BTreeMap<String, Metric>,
}
#[derive(Default)]
pub(crate) struct Metric {
    pub yes: usize,
    pub no: usize,
    pub unknown: usize,
    pub keys: BTreeSet<String>,
    pub excluded: BTreeMap<String, usize>,
}
impl Collector {
    fn new(q: &Query, ctx: &Context, keep_rows: bool) -> Result<Self, Diagnostic> {
        let mut c = Self {
            rows: vec![],
            groups: BTreeMap::new(),
            facets: BTreeMap::new(),
            keep_rows,
            state_bytes: 0,
        };
        if q.group_by.is_empty() {
            c.groups.insert("[]".into(), Group::default());
        }
        for f in &q.facets {
            let mut buckets = BTreeMap::new();
            if q.zero_enums {
                for v in ctx.spec(q, f)?.values {
                    let b = Envelope::authored(json!(v)).bucket();
                    buckets.insert(b.to_string(), (b, 0));
                }
            }
            c.facets.insert(f.clone(), buckets);
        }
        if q.zero_enums && !q.group_by.is_empty() {
            let mut keys = vec![vec![]];
            for f in &q.group_by {
                let spec = ctx.spec(q, f)?;
                if spec.kind != FieldType::Enum {
                    return Err(bad("zero-count groups require enum dimensions"));
                }
                let mut next = vec![];
                let mut next_bytes = 0usize;
                for key in keys {
                    for value in &spec.values {
                        if next.len() >= MAX_GROUPS {
                            return Err(Diagnostic::new(
                                ErrorCode::LimitExceeded,
                                "too many zero enum groups",
                            ));
                        }
                        let mut k = key.clone();
                        k.push(Envelope::authored(json!(value)).bucket());
                        next_bytes += serde_json::to_vec(&k).unwrap().len() * 2;
                        if next_bytes > 32 * 1024 * 1024 {
                            return Err(Diagnostic::new(
                                ErrorCode::LimitExceeded,
                                "zero enum state cap exceeded",
                            ));
                        }
                        next.push(k);
                    }
                }
                keys = next;
            }
            for key in keys {
                c.groups.insert(
                    json!(key.iter().map(canonical_bucket).collect::<Vec<_>>()).to_string(),
                    Group {
                        key,
                        ..Group::default()
                    },
                );
            }
        }
        c.state_bytes = c
            .groups
            .iter()
            .map(|(token, g)| token.len() + serde_json::to_vec(&g.key).unwrap().len())
            .sum::<usize>()
            + c.facets
                .values()
                .flat_map(|b| {
                    b.iter()
                        .map(|(token, (value, _))| token.len() + value.to_string().len())
                })
                .sum::<usize>();
        if c.state_bytes > 32 * 1024 * 1024 {
            return Err(Diagnostic::new(
                ErrorCode::LimitExceeded,
                "enum state cap exceeded",
            ));
        }
        Ok(c)
    }
    fn consume(&mut self, row: Row, q: &Query) -> Result<(), Diagnostic> {
        for field in q
            .group_by
            .iter()
            .chain(q.measures.values().filter_map(|m| match m {
                Measure::CountDistinct { field } => Some(field),
                _ => None,
            }))
        {
            if selected(&row, field)
                .value
                .as_ref()
                .and_then(Value::as_number)
                .is_some_and(|n| !schema::comparable(n))
            {
                return Err(bad(
                    "group/distinct number exceeds the exact comparison range",
                ));
            }
        }
        let token_size = |v: &Value| v.to_string().len();
        let mut added = if self.keep_rows {
            serde_json::to_vec(&row).unwrap().len()
        } else {
            0
        };
        let group_key: Vec<_> = q
            .group_by
            .iter()
            .map(|f| selected(&row, f).bucket())
            .collect();
        let group_token =
            json!(group_key.iter().map(canonical_bucket).collect::<Vec<_>>()).to_string();
        let existing = self.groups.get(&group_token);
        if existing.is_none() {
            added += group_token.len() + token_size(&json!(group_key));
        }
        for (name, m) in &q.measures {
            if let Measure::CountDistinct { field } = m {
                let b = selected(&row, field).bucket();
                let token = canonical_bucket(&b);
                if b["state"] == "value"
                    && existing
                        .and_then(|g| g.measures.get(name))
                        .is_none_or(|m| !m.keys.contains(&token))
                {
                    added += token.len();
                }
            }
        }
        for field in &q.facets {
            let b = selected(&row, field).bucket();
            let token = b.to_string();
            if !self.facets[field].contains_key(&token) {
                added += token.len() * 2;
            }
        }
        self.state_bytes = self.state_bytes.checked_add(added).ok_or_else(|| {
            Diagnostic::new(ErrorCode::LimitExceeded, "analytics state cap exceeded")
        })?;
        if self.state_bytes > 32 * 1024 * 1024 {
            return Err(Diagnostic::new(
                ErrorCode::LimitExceeded,
                "analytics state cap exceeded",
            ));
        }

        let key = group_key;
        let token = group_token;
        if !self.groups.contains_key(&token) && self.groups.len() >= MAX_GROUPS {
            return Err(Diagnostic::new(ErrorCode::LimitExceeded, "too many groups"));
        }
        let group = self.groups.entry(token).or_insert_with(|| Group {
            key,
            ..Group::default()
        });
        group.count += 1;
        for (name, m) in &q.measures {
            let metric = group.measures.entry(name.clone()).or_default();
            match m {
                Measure::Count => metric.yes += 1,
                Measure::CountWhere { clauses } => match predicate(&row.fields, clauses) {
                    Truth::True => metric.yes += 1,
                    Truth::False => metric.no += 1,
                    Truth::Unknown => metric.unknown += 1,
                },
                Measure::CountDistinct { field } => {
                    let b = selected(&row, field).bucket();
                    if b["state"] == "value" {
                        metric.keys.insert(canonical_bucket(&b));
                        if metric.keys.len() > MAX_ROWS {
                            return Err(Diagnostic::new(
                                ErrorCode::LimitExceeded,
                                "too many distinct keys",
                            ));
                        }
                    } else {
                        *metric
                            .excluded
                            .entry(b["state"].as_str().unwrap().into())
                            .or_default() += 1;
                    }
                }
            }
        }
        for f in &q.facets {
            let b = selected(&row, f).bucket();
            self.facets
                .get_mut(f)
                .unwrap()
                .entry(b.to_string())
                .or_insert((b, 0))
                .1 += 1;
        }
        if self.keep_rows {
            if self.rows.len() >= MAX_ROWS {
                return Err(Diagnostic::new(
                    ErrorCode::LimitExceeded,
                    "too many retained rows",
                ));
            }
            self.rows.push(row)
        }
        Ok(())
    }
}
fn selected(row: &Row, field: &str) -> Envelope {
    if let Some(e) = row.fields.get(field) {
        e.clone()
    } else if let Some(f) = field.strip_suffix(".origin") {
        Envelope::authored(json!(row.fields[f].origin))
    } else {
        row.fields[field].clone()
    }
}

fn canonical_bucket(b: &Value) -> String {
    if let Some(n) = b["value"].as_number() {
        let text = n.to_string();
        let d = decimal_key(&text);
        json!(["number", d]).to_string()
    } else {
        b.to_string()
    }
}
fn decimal_key(s: &str) -> String {
    match schema::decimal(s) {
        Some((_, digits, _)) if digits.is_empty() => "0".into(),
        Some((negative, digits, exponent)) => {
            format!("{}{}e{}", if negative { "-" } else { "" }, digits, exponent)
        }
        None => format!("unsupported:{s}"),
    }
}

pub(crate) fn scan(
    project: &Project,
    q: &Query,
    ctx: &Context,
    collector: &mut Collector,
    capture: bool,
) -> Result<Scan, Diagnostic> {
    scan_for_report(project, q, ctx, collector, capture, None)
}
fn scan_for_report(
    project: &Project,
    q: &Query,
    ctx: &Context,
    collector: &mut Collector,
    capture: bool,
    expected_fingerprint: Option<&str>,
) -> Result<Scan, Diagnostic> {
    let start = now();
    let (files, events) = project.observe(&ctx.bound.config.files, q.paths.as_deref());
    let mut scan = Scan {
        coverage: Coverage {
            files_discovered: files.len(),
            ..Coverage::default()
        },
        diagnostics: Diagnostics::default(),
        events,
        outcomes: vec![],
        captures: vec![],
        fingerprint: String::new(),
        start,
        end: String::new(),
    };
    for path in files {
        if scan.outcomes.len() >= bundle::MAX_FILES {
            return Err(Diagnostic::new(
                ErrorCode::LimitExceeded,
                "too many source files",
            ));
        }
        scan.coverage.reads += 1;
        let bytes = project.read_bytes(&path).map_err(|e| e.with_path(&path));
        crate::fs::hook("analytics_read")?;
        let source_hash = bytes.as_ref().ok().map(|b| hash(b));
        let member = if capture && bytes.is_ok() {
            Some(format!("sources/{}.bin", scan.captures.len()))
        } else {
            None
        };
        let outcome = process(
            &path,
            bytes.as_deref().map_err(Clone::clone),
            q,
            ctx,
            collector,
            &mut scan.coverage,
            &mut scan.diagnostics,
        )?;
        scan.outcomes.push(Outcome {
            path,
            status: outcome.0,
            error: outcome.1,
            source_hash,
            member: member.clone(),
        });
        if let (Some(m), Ok(bytes)) = (member, bytes) {
            if scan.captures.iter().map(|(_, b)| b.len()).sum::<usize>() + bytes.len()
                > bundle::MAX_BYTES
            {
                return Err(Diagnostic::new(
                    ErrorCode::LimitExceeded,
                    "capture exceeds corpus cap",
                ));
            }
            scan.captures.push((m, bytes));
        }
    }
    scan.end = now();
    let validation = finish_scan(q, ctx, &mut scan);
    if !scan.fingerprint.is_empty()
        && expected_fingerprint.is_some_and(|expected| expected != scan.fingerprint)
    {
        return Err(Diagnostic::new(
            ErrorCode::ExternalChange,
            "report inputs changed; recompute the report",
        ));
    }
    validation?;
    Ok(scan)
}
pub(crate) fn process(
    path: &str,
    bytes: Result<&[u8], Diagnostic>,
    q: &Query,
    ctx: &Context,
    c: &mut Collector,
    coverage: &mut Coverage,
    diagnostics: &mut Diagnostics,
) -> Result<(String, Option<Diagnostic>), Diagnostic> {
    let bytes = match bytes {
        Ok(b) => {
            coverage.files_read += 1;
            b
        }
        Err(e) => {
            coverage.unreadable_files += 1;
            coverage.failed_files += 1;
            diagnostics.push(e.clone())?;
            return Ok(("read_failed".into(), Some(e)));
        }
    };
    let parsed = std::str::from_utf8(bytes)
        .map_err(|_| {
            Diagnostic::new(ErrorCode::UnsupportedSyntax, "file is not UTF-8").with_path(path)
        })
        .and_then(Document::parse);
    let doc = match parsed {
        Ok(d) => d,
        Err(e) => {
            let e = e.with_path(path);
            coverage.failed_files += 1;
            diagnostics.push(e.clone())?;
            return Ok(("parse_failed".into(), Some(e)));
        }
    };
    let (mut meta, metadata_errors) = metadata(&doc, ctx);
    meta.insert("document.path".into(), Envelope::authored(json!(path)));
    for e in metadata_errors {
        diagnostics.push(Diagnostic::new(ErrorCode::InvalidItem, e).with_path(path))?;
    }
    let instance = schema::instance(&doc, &ctx.bound);
    let (inst, binding) = match instance {
        Ok(Some(i)) => (Some(i), "bound"),
        Ok(None) => {
            coverage.missing_bindings += 1;
            diagnostics.push(
                Diagnostic::new(ErrorCode::MissingSelector, "missing collection binding")
                    .with_path(path),
            )?;
            (None, "missing")
        }
        Err(e) => {
            coverage.ambiguous_bindings += 1;
            diagnostics.push(e.with_path(path))?;
            (None, "ambiguous")
        }
    };
    // Duplicate identity is a diagnostic; each source location remains a distinct row.
    let ids: BTreeMap<String, usize> = doc
        .ids
        .iter()
        .map(|(id, targets)| (id.clone(), targets.len()))
        .collect();
    for (id, count) in &ids {
        if *count > 1 {
            coverage.duplicate_ids += 1;
            diagnostics.push(
                Diagnostic::new(ErrorCode::DuplicateId, "duplicate identity")
                    .with_path(path)
                    .with_id(id),
            )?;
        }
    }
    if q.source == RowSource::Documents {
        accept(
            Row {
                path: path.into(),
                line: 1,
                id: None,
                revision: doc.revision(),
                binding: binding.into(),
                fields: meta,
            },
            q,
            c,
            coverage,
            diagnostics,
        )?;
    } else if let Some(inst) = inst {
        for (list, item) in inst.items {
            coverage.candidate_items += 1;
            let it = &doc.lists[list].items[item];
            let id = doc.stay_ids(&it.markers).first().cloned();
            let mut fields = meta.clone();
            fields.extend(item_fields(&doc.item_content(list, item), ctx));
            fields.insert(
                "item.id".into(),
                Envelope::effective(id.as_ref().map(|s| json!(s)), None, false, |_| Ok(())),
            );
            let item_valid = fields
                .iter()
                .filter(|(k, _)| k.starts_with("item."))
                .all(|(_, e)| e.validity == "valid");
            if item_valid {
                coverage.valid_items += 1
            } else {
                coverage.invalid_items += 1
            }
            if id
                .as_ref()
                .is_some_and(|id| ids.get(id).is_some_and(|n| *n == 1))
            {
                coverage.addressable_items += 1
            }
            accept(
                Row {
                    path: path.into(),
                    line: doc.file_line(it.span.start),
                    id,
                    revision: schema::item_revision(&doc, &ctx.bound, list, item),
                    binding: binding.into(),
                    fields,
                },
                q,
                c,
                coverage,
                diagnostics,
            )?;
        }
    }
    Ok(("parsed".into(), None))
}
fn accept(
    row: Row,
    q: &Query,
    c: &mut Collector,
    coverage: &mut Coverage,
    diagnostics: &mut Diagnostics,
) -> Result<(), Diagnostic> {
    coverage.candidate_rows += 1;
    let invalid = row.fields.values().any(|e| e.validity != "valid");
    if invalid {
        coverage.invalid_rows += 1;
        for (name, e) in &row.fields {
            for error in &e.errors {
                diagnostics.push(
                    Diagnostic::new(ErrorCode::InvalidItem, format!("{name}: {error}"))
                        .with_path(&row.path)
                        .with_line(row.line),
                )?;
            }
        }
    } else {
        coverage.valid_rows += 1
    }
    match predicate(&row.fields, &q.clauses) {
        Truth::True => {
            coverage.matching_rows += 1;
            if q.source == RowSource::Items {
                coverage.matching_items += 1
            }
            c.consume(row, q)?
        }
        Truth::False => coverage.nonmatching_rows += 1,
        Truth::Unknown => coverage.unknown_predicates += 1,
    }
    Ok(())
}
pub(crate) fn finish_scan(q: &Query, ctx: &Context, scan: &mut Scan) -> Result<(), Diagnostic> {
    let outcomes: Vec<_> = scan
        .outcomes
        .iter()
        .map(|o| json!([o.path, o.status, o.error, o.source_hash]))
        .collect();
    if scan.events.iter().any(|e| {
        e.error
            .as_ref()
            .is_some_and(|d| d.code == ErrorCode::LimitExceeded)
    }) {
        return Err(Diagnostic::new(
            ErrorCode::LimitExceeded,
            "analytics scan metadata cap exceeded",
        ));
    }
    let token = json!([CONTRACT, q, ctx.definitions, outcomes, scan.events]);
    scan.fingerprint = hash(&serde_json::to_vec(&token).unwrap());
    let failed = scan.coverage.failed_files > 0
        || scan.coverage.invalid_rows > 0
        || scan
            .diagnostics
            .iter()
            .any(|d| d.code == ErrorCode::InvalidItem)
        || scan.coverage.ambiguous_bindings > 0
        || (q.missing_binding == MissingBinding::Refuse && scan.coverage.missing_bindings > 0)
        || scan
            .events
            .iter()
            .any(|e| e.is_gap(&ctx.bound.config.files));
    if failed && q.mode == Mode::Strict {
        let mut e = Diagnostic::new(
            ErrorCode::InvalidItem,
            "strict census refused: candidate validation or coverage failed",
        );
        e.current = Some(
            json!({"coverage":scan.coverage,"events":scan.events,"diagnostics":scan.diagnostics})
                .to_string(),
        );
        return Err(e);
    }
    Ok(())
}
fn bucket_order(a: &Value, b: &Value) -> std::cmp::Ordering {
    fn rank(b: &Value) -> u8 {
        match b["state"].as_str() {
            Some("missing") => 0,
            Some("null") => 1,
            Some("invalid") => 2,
            Some("unsupported") => 3,
            _ => match b["type"].as_str() {
                Some("boolean") => 4,
                Some("number") => 5,
                _ => 6,
            },
        }
    }
    rank(a).cmp(&rank(b)).then_with(|| {
        if let (Some(a), Some(b)) = (a["value"].as_number(), b["value"].as_number()) {
            schema::cmp_decimal(&a.to_string(), &b.to_string())
                .unwrap_or_else(|| a.to_string().cmp(&b.to_string()))
        } else if let (Some(a), Some(b)) = (a["value"].as_str(), b["value"].as_str()) {
            a.cmp(b)
        } else {
            a.to_string().cmp(&b.to_string())
        }
    })
}
fn key_order(a: &[Value], b: &[Value]) -> std::cmp::Ordering {
    a.iter()
        .zip(b)
        .map(|(a, b)| bucket_order(a, b))
        .find(|o| !o.is_eq())
        .unwrap_or_else(|| a.len().cmp(&b.len()))
}
pub(crate) fn render(q: &Query, ctx: &Context, scan: &Scan, c: &Collector) -> Value {
    let mut groups: Vec<_> = c.groups.values().collect();
    groups.sort_by(|a, b| key_order(&a.key, &b.key));
    if q.top.is_some() {
        groups.sort_by(|a, b| {
            b.count
                .cmp(&a.count)
                .then_with(|| key_order(&a.key, &b.key))
        });
    }
    let total_groups = groups.len();
    let available = q.top.unwrap_or(total_groups).min(total_groups);
    let limit = q.limit.unwrap_or(50);
    let selected:Vec<Value>=groups.into_iter().take(available).skip(q.offset).take(limit).map(|g|{
        let measures:BTreeMap<_,_>=q.measures.iter().map(|(name,m)|{
            let empty=Metric::default();let v=g.measures.get(name).unwrap_or(&empty);
            let value=match m {
                Measure::Count=>json!({"count":v.yes,"unit":"rows","base_rows":g.count}),
                Measure::CountWhere{..}=>json!({"count":v.yes,"false":v.no,"unknown":v.unknown,"base_rows":g.count,"unit":"rows"}),
                Measure::CountDistinct{..}=>{
                    let excluded:BTreeMap<_,_>=["missing","null","invalid","unsupported"].into_iter().map(|n|(n,v.excluded.get(n).copied().unwrap_or(0))).collect();
                    json!({"count":v.keys.len(),"excluded":excluded,"base_rows":g.count,"unit":"distinct_keys"})
                }
            };(name.clone(),value)
        }).collect();
        json!({"key":g.key,"base_rows":g.count,"measures":measures})
    }).collect();
    let mut rows: Vec<_> = if q.group_by.is_empty() && q.measures.is_empty() && q.facets.is_empty()
    {
        c.rows.iter().skip(q.offset).take(limit).cloned().collect()
    } else {
        vec![]
    };
    if let Some(fields) = &q.fields {
        for row in &mut rows {
            row.fields = fields
                .iter()
                .map(|f| (f.clone(), selected_field(row, f)))
                .collect();
        }
    }
    let facets:BTreeMap<_,_>=c.facets.iter().map(|(f,b)|{let mut b:Vec<_>=b.values().collect();b.sort_by(|a,b|bucket_order(&a.0,&b.0));(f,json!({"unit":"rows","denominator":scan.coverage.matching_rows,"buckets":b.into_iter().map(|(key,count)|json!({"key":key,"count":count})).collect::<Vec<_>>()}))}).collect();
    let scope = json!({
        "collection":q.collection,"source":q.source,"mode":q.mode,
        "missing_binding":q.missing_binding,"files":ctx.bound.config.files,"paths":q.paths,
        "section":ctx.bound.config.section,"level":ctx.bound.config.level,
        "subsections":ctx.bound.config.subsections,"exclude":ctx.bound.config.exclude,
        "traversal_exclusions":crate::fs::traversal_exclusions(),
        "schema":ctx.bound.key,"schema_digest":ctx.bound.digest,
        "binding_digest":hash(&ctx.definitions["project.json"]),
        "analytics":q.analytics,
        "analytics_digest":q.analytics.as_ref().map(|n|hash(&ctx.definitions[&format!("analytics/{n}.json")])),
        "view":ctx.view,
        "view_digest":ctx.view.as_ref().map(|n|hash(&ctx.definitions[&format!("views/{n}.json")])),
        "analytics_version":CONTRACT
    });
    json!({
        "ok":true,"status":"read","contract_version":CONTRACT,"consistency":"per_file",
        "fingerprint":scan.fingerprint,"coverage":scan.coverage,"diagnostics":scan.diagnostics,
        "events":scan.events,"scope":scope,
        "unit":if q.source==RowSource::Items{"items"}else{"documents"},
        "denominator":scan.coverage.candidate_rows,"total":scan.coverage.matching_rows,
        "rows":rows,"groups":selected,"total_groups":total_groups,
        "omitted_groups":total_groups-selected.len(),
        "returned_group_rows":selected.iter().map(|g|g["base_rows"].as_u64().unwrap()).sum::<u64>(),
        "facets":facets
    })
}
fn selected_field(row: &Row, f: &str) -> Envelope {
    selected(row, f)
}
pub fn execute(project: &Project, q: Query) -> Result<Value, Diagnostic> {
    run(project, q, None)
}
fn run(project: &Project, q: Query, view: Option<(&str, Vec<u8>)>) -> Result<Value, Diagnostic> {
    let ctx = Context::load(project, &q, view)?;
    let mut c = Collector::new(
        &q,
        &ctx,
        q.group_by.is_empty() && q.measures.is_empty() && q.facets.is_empty(),
    )?;
    let scan = scan(project, &q, &ctx, &mut c, false)?;
    Ok(render(&q, &ctx, &scan, &c))
}
pub(crate) fn load_view(project: &Project, name: &str) -> Result<(Query, Vec<u8>), Diagnostic> {
    if !plain(name) {
        return Err(bad("view name must be plain"));
    }
    let bytes = project
        .read_config(&["views", &format!("{name}.json")])?
        .ok_or_else(|| bad("view not found"))?;
    let v: View = serde_json::from_slice(&bytes).map_err(|e| bad(format!("view: {e}")))?;
    if v.version != CONTRACT {
        return Err(bad("unsupported view version"));
    }
    Ok((v.query, bytes))
}
pub fn view(project: &Project, name: &str) -> Result<Value, Diagnostic> {
    let (q, bytes) = load_view(project, name)?;
    run(project, q, Some((name, bytes)))
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Drilldown {
    pub query: Query,
    pub fingerprint: String,
    pub group: Vec<Value>,
    pub measure: String,
    #[serde(default)]
    pub bundle: Option<String>,
    #[serde(default)]
    pub view: Option<String>,
}
pub fn drilldown(project: &Project, request: Drilldown) -> Result<Value, Diagnostic> {
    let q = &request.query;
    let (ctx, scan, c) = if let Some(name) = &request.bundle {
        let (ctx, scan, c, _) = bundle::captured(project, name, Some(q))?;
        (ctx, scan, c)
    } else {
        let view = if let Some(name) = &request.view {
            let (view_query, bytes) = load_view(project, name)?;
            if serde_json::to_value(view_query).unwrap() != serde_json::to_value(q).unwrap() {
                return Err(bad("drilldown query differs from view"));
            }
            Some((name.as_str(), bytes))
        } else {
            None
        };
        let ctx = Context::load(project, q, view)?;
        let mut c = Collector::new(q, &ctx, true)?;
        let scan = scan_for_report(project, q, &ctx, &mut c, false, Some(&request.fingerprint))?;
        (ctx, scan, c)
    };
    if scan.fingerprint != request.fingerprint {
        return Err(Diagnostic::new(
            ErrorCode::ExternalChange,
            "report inputs changed; recompute the report",
        ));
    }
    if request.group.len() != q.group_by.len() {
        return Err(bad("group dimension mismatch"));
    }
    if !c.groups.contains_key(
        &json!(
            request
                .group
                .iter()
                .map(canonical_bucket)
                .collect::<Vec<_>>()
        )
        .to_string(),
    ) {
        return Err(bad("group not present in report"));
    }
    let measure = q
        .measures
        .get(&request.measure)
        .ok_or_else(|| bad("measure not in query"))?;
    let mut result = render(q, &ctx, &scan, &c);
    let rows: Vec<_> = c
        .rows
        .into_iter()
        .filter(|r| {
            q.group_by
                .iter()
                .map(|f| canonical_bucket(&selected(r, f).bucket()))
                .collect::<Vec<_>>()
                == request
                    .group
                    .iter()
                    .map(canonical_bucket)
                    .collect::<Vec<_>>()
        })
        .collect();
    let names: &[&str] = match measure {
        Measure::Count => &["included"],
        Measure::CountWhere { .. } => &["true", "false", "unknown"],
        Measure::CountDistinct { .. } => &["missing", "null", "invalid", "unsupported"],
    };
    let mut partitions: BTreeMap<String, Vec<Row>> =
        names.iter().map(|n| (n.to_string(), vec![])).collect();
    let mut keys = BTreeMap::<String, (Value, Vec<Row>)>::new();
    for row in rows {
        let partition = match measure {
            Measure::Count => "included".into(),
            Measure::CountWhere { clauses } => match predicate(&row.fields, clauses) {
                Truth::True => "true".into(),
                Truth::False => "false".into(),
                Truth::Unknown => "unknown".into(),
            },
            Measure::CountDistinct { field } => {
                let b = selected(&row, field).bucket();
                if b["state"] == "value" {
                    keys.entry(canonical_bucket(&b))
                        .or_insert((b, vec![]))
                        .1
                        .push(row);
                    continue;
                }
                b["state"].as_str().unwrap().into()
            }
        };
        partitions.entry(partition).or_default().push(row);
    }
    result["partitions"] = json!(partitions);
    let mut keys: Vec<_> = keys.into_values().collect();
    keys.sort_by(|a, b| bucket_order(&a.0, &b.0));
    result["keys"] = json!(
        keys.into_iter()
            .map(|(key, rows)| json!({"key":key,"rows":rows}))
            .collect::<Vec<_>>()
    );
    result["measure"] = json!(request.measure);
    result["group"] = json!(request.group);
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs::{WalkFaults, glob_reachable};
    fn project() -> Project {
        let root = crate::test_support::scratch("rivet-analytics-core");
        std::fs::create_dir_all(root.join("matching")).unwrap();
        std::fs::create_dir_all(root.join("unrelated")).unwrap();
        let project = Project::init(&root).unwrap();
        std::fs::write(root.join(".rivet/project.json"),r#"{"collections":{"actions":{"schema":"action@1","files":["matching/**/*.md"],"section":"Action"}}}"#).unwrap();
        std::fs::write(root.join(".rivet/schemas/action.json"),r#"{"id":"action","version":1,"item":"checkbox","fields":{"done":{"type":"boolean","from":"checkbox"},"state":{"type":"enum","from":"label","label":"State","values":["good","bad"]},"actor":{"type":"string","from":"tag","tags":["agent"],"default":"agent"}}}"#).unwrap();
        std::fs::write(
            root.join("matching/one.md"),
            "## Action\n\n- [ ] **First**\n  **State**: good\n",
        )
        .unwrap();
        project
    }
    #[test]
    fn analytics_walker_scope_and_injected_errors() {
        let p = project();
        let q = Query {
            collection: "actions".into(),
            ..Query::default()
        };
        let ctx = Context::load(&p, &q, None).unwrap();
        let globs = &ctx.bound.config.files;
        let fault = WalkFaults {
            directories: BTreeSet::from(["matching".into(), "unrelated".into()]),
            ..WalkFaults::default()
        };
        let (files, events) = p.observe_with(globs, None, &fault);
        assert!(files.is_empty());
        assert!(events.iter().any(|e| e.path.as_deref() == Some("matching")
            && e.error.is_some()
            && e.scope == "in_scope"));
        assert!(events.iter().any(|e| e.path.as_deref() == Some("unrelated")
            && e.reason == "glob_pruned"
            && e.error.is_none()));
        let mut s = Scan {
            coverage: Coverage::default(),
            diagnostics: Diagnostics::default(),
            events,
            outcomes: vec![],
            captures: vec![],
            fingerprint: String::new(),
            start: "1".into(),
            end: "2".into(),
        };
        assert!(finish_scan(&q, &ctx, &mut s).is_err());
        let q = Query {
            mode: Mode::Diagnostic,
            ..q
        };
        assert!(finish_scan(&q, &ctx, &mut s).is_ok());
        for fault in [
            WalkFaults {
                entries: BTreeSet::from(["matching".into()]),
                ..WalkFaults::default()
            },
            WalkFaults {
                types: BTreeSet::from(["matching/one.md".into()]),
                ..WalkFaults::default()
            },
        ] {
            let (_, events) = p.observe_with(globs, None, &fault);
            assert!(events.iter().any(|e| e.error.is_some()));
            s.events = events;
            let strict = Query {
                mode: Mode::Strict,
                ..q.clone()
            };
            assert!(finish_scan(&strict, &ctx, &mut s).is_err());
        }
        let outside = WalkFaults {
            types: BTreeSet::from(["unrelated".into()]),
            ..WalkFaults::default()
        };
        let (_, events) = p.observe_with(globs, None, &outside);
        s.events = events;
        let strict = Query {
            mode: Mode::Strict,
            ..q
        };
        assert!(finish_scan(&strict, &ctx, &mut s).is_ok());
        std::fs::write(p.root.join("matching/outside.md"), "ignored").unwrap();
        let paths = vec!["matching/one.md".into()];
        let fault = WalkFaults {
            types: BTreeSet::from(["matching/outside.md".into()]),
            ..WalkFaults::default()
        };
        let (files, events) = p.observe_with(globs, Some(&paths), &fault);
        assert_eq!(files, paths);
        assert!(
            events
                .iter()
                .any(|e| e.path.as_deref() == Some("matching/outside.md")
                    && e.error.is_some()
                    && e.scope == "out_of_scope")
        );
        s.events = events;
        assert!(finish_scan(&strict, &ctx, &mut s).is_ok());
        assert!(glob_reachable("matching/**/*.md", "matching/deep"));
        assert!(!glob_reachable("matching/*.md", "matching/deep"));
        assert!(!glob_reachable("matching/**/*.md", "unrelated"));
        std::fs::remove_dir_all(p.root).unwrap();
    }
    #[test]
    fn analytics_differential_extraction_and_presence_and() {
        let p = project();
        let q = Query {
            collection: "actions".into(),
            ..Query::default()
        };
        let ctx = Context::load(&p, &q, None).unwrap();
        for content in [
            "[ ] **First**\n**State**: good",
            "[x] `agent` **Second**\n**State**: bad",
            "[ ] **Third**",
        ] {
            let strict = schema::extract(content, &ctx.bound.schema).unwrap();
            let diagnostic = item_fields(content, &ctx);
            for (name, value) in strict {
                assert_eq!(diagnostic[&format!("item.{name}")].value, Some(value));
            }
        }
        let fields = item_fields("[ ] **Bad**\n**State**: invalid", &ctx);
        assert!(schema::extract("[ ] **Bad**\n**State**: invalid", &ctx.bound.schema).is_err());
        assert_eq!(fields["item.done"].value, Some(json!(false)));
        let clause = |field: &str, op: Cmp, value: Value| Clause {
            field: field.into(),
            op,
            value,
        };
        assert_eq!(
            predicate(&fields, &[clause("item.state", Cmp::Exists, Value::Null)]),
            Truth::True
        );
        assert_eq!(
            predicate(&fields, &[clause("item.state", Cmp::Eq, json!("good"))]),
            Truth::Unknown
        );
        assert_eq!(
            predicate(
                &fields,
                &[
                    clause("item.state", Cmp::Eq, json!("good")),
                    clause("item.done", Cmp::Eq, json!(true))
                ]
            ),
            Truth::False
        );
        let e = Envelope::unsupported();
        let fields = BTreeMap::from([("document.x".into(), e)]);
        assert_eq!(
            predicate(&fields, &[clause("document.x", Cmp::Missing, Value::Null)]),
            Truth::Unknown
        );
        let fields = BTreeMap::from([(
            "document.x".into(),
            Envelope::effective(None, None, false, |_| Ok(())),
        )]);
        assert_eq!(
            predicate(&fields, &[clause("document.x", Cmp::Ne, json!("x"))]),
            Truth::False
        );
        std::fs::remove_dir_all(p.root).unwrap();
    }
    #[test]
    fn analytics_numeric_typed_key_identity_and_sort() {
        let b = |v| Envelope::authored(v).bucket();
        assert_eq!(
            canonical_bucket(&b(serde_json::from_str("1.0").unwrap())),
            canonical_bucket(&b(json!(1)))
        );
        assert_eq!(
            canonical_bucket(&b(serde_json::from_str("-0.0").unwrap())),
            canonical_bucket(&b(json!(0)))
        );
        assert_ne!(
            canonical_bucket(&b(json!("1"))),
            canonical_bucket(&b(json!(1)))
        );
        assert_ne!(
            canonical_bucket(&Envelope::effective(None, None, false, |_| Ok(())).bucket()),
            canonical_bucket(&b(json!("missing")))
        );
        assert!(bucket_order(&b(json!(2)), &b(json!(10))).is_lt());
    }
    #[test]
    fn analytics_failed_outcome_and_event_replay() {
        let p = project();
        let q = Query {
            collection: "actions".into(),
            mode: Mode::Diagnostic,
            ..Query::default()
        };
        let ctx = Context::load(&p, &q, None).unwrap();
        let mut c = Collector::new(&q, &ctx, true).unwrap();
        let mut scan = scan(&p, &q, &ctx, &mut c, true).unwrap();
        let error =
            Diagnostic::new(ErrorCode::Io, "injected read failure").with_path("matching/lost.md");
        let result = process(
            "matching/lost.md",
            Err(error.clone()),
            &q,
            &ctx,
            &mut c,
            &mut scan.coverage,
            &mut scan.diagnostics,
        )
        .unwrap();
        assert_eq!(result.0, "read_failed");
        scan.coverage.files_discovered += 1;
        scan.coverage.reads += 1;
        scan.outcomes.push(Outcome {
            path: "matching/lost.md".into(),
            status: result.0,
            error: Some(error),
            source_hash: None,
            member: None,
        });
        scan.outcomes.sort_by(|a, b| a.path.cmp(&b.path));
        let (_, events) = p.observe_with(
            &ctx.bound.config.files,
            None,
            &WalkFaults {
                entries: BTreeSet::from(["matching".into()]),
                ..WalkFaults::default()
            },
        );
        scan.events = events;
        finish_scan(&q, &ctx, &mut scan).unwrap();
        let expected = render(&q, &ctx, &scan, &c);
        bundle::publish_scan(&p, "injected", q, ctx, scan, c).unwrap();
        let replay = bundle::replay(&p, "injected").unwrap();
        assert_eq!(replay, expected);
        assert_eq!(replay["coverage"]["unreadable_files"], 1);
        assert!(
            replay["events"]
                .as_array()
                .unwrap()
                .iter()
                .any(|e| e["error"].is_object())
        );
        std::fs::remove_dir_all(p.root).unwrap();
    }
}
