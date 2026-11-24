//! Readiness for `query`: each open item's wait field resolved across every
//! file of its collection and the collection's archive files. Read-only.
//!
//! A wait value is one word with an optional trailing `(note)`. `none` is no
//! wait, `item:<id>` waits on the unit carrying that stay id, and anything else
//! is a condition Rivet cannot see close. Nothing is ready by default: an item
//! with no wait label is `unlabelled`, and a wait that cannot be resolved is
//! reported, never guessed.
use crate::citations::Carrier;
use crate::document::Document;
use crate::schema::{self, Bound, FieldType, Schema};
use crate::{Diagnostic, ErrorCode};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadyRequest {
    /// The schema field holding the wait, a string field.
    pub field: String,
    /// Which classes to list; every class is counted. Default `["ready"]`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub include: Option<Vec<Readiness>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Readiness {
    Ready,
    Waiting,
    Condition,
    Unlabelled,
    Unresolved,
}

const CLASSES: [Readiness; 5] = [
    Readiness::Ready,
    Readiness::Waiting,
    Readiness::Condition,
    Readiness::Unlabelled,
    Readiness::Unresolved,
];

impl ReadyRequest {
    pub fn check(&self, schema: &Schema) -> Result<(), Diagnostic> {
        match schema.fields.get(&self.field) {
            // A default would read an absent label as written, and an item
            // nobody labelled could come back ready.
            Some(spec) if spec.default.is_some() => Err(Diagnostic::new(
                ErrorCode::InvalidRequest,
                format!(
                    "ready.field {} declares a default, so an absent wait cannot be told apart",
                    self.field
                ),
            )),
            Some(spec) if spec.kind == FieldType::String => Ok(()),
            Some(_) => Err(Diagnostic::new(
                ErrorCode::InvalidRequest,
                format!("ready.field {} is not a string field", self.field),
            )),
            None => Err(Diagnostic::new(
                ErrorCode::InvalidRequest,
                format!("ready.field {} is not a field of {}", self.field, schema.id),
            )),
        }
    }

    pub fn lists(&self, class: Readiness) -> bool {
        match &self.include {
            None => class == Readiness::Ready,
            Some(include) => include.contains(&class),
        }
    }
}

pub fn counts() -> BTreeMap<Readiness, usize> {
    CLASSES.iter().map(|&c| (c, 0)).collect()
}

/// A wait value read without the project: what it asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Wait {
    Unlabelled,
    None,
    Item(String),
    Malformed,
    Condition,
}

fn parse(value: Option<&str>) -> Wait {
    let Some(value) = value.map(str::trim).filter(|v| !v.is_empty()) else {
        return Wait::Unlabelled;
    };
    let bare = without_note(value);
    if bare == "none" {
        Wait::None
    } else if let Some(id) = bare.strip_prefix("item:") {
        if !id.is_empty()
            && id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
        {
            Wait::Item(id.to_string())
        } else {
            Wait::Malformed
        }
    } else {
        Wait::Condition
    }
}

/// The value before one trailing `(note)`, or the whole value. The note's
/// parentheses must balance, so `none (a) (b)` stays whole.
fn without_note(value: &str) -> &str {
    let Some((head, rest)) = value.split_once(char::is_whitespace) else {
        return value;
    };
    let rest = rest.trim_start();
    let Some(inner) = rest.strip_prefix('(').and_then(|r| r.strip_suffix(')')) else {
        return value;
    };
    let mut depth = 0i32;
    for c in inner.chars() {
        depth += match c {
            '(' => 1,
            ')' => -1,
            _ => 0,
        };
        if depth < 0 {
            return value;
        }
    }
    if depth == 0 && !inner.trim().is_empty() {
        head
    } else {
        value
    }
}

/// An open or closed task item, or any other unit, by the file it is in.
type Key = (String, usize, usize);

/// One open member item's verdict, as `query` reports it under `wait`.
#[derive(Debug, Clone)]
pub struct Verdict {
    pub readiness: Readiness,
    value: Option<String>,
    on: Option<Carrier>,
    on_id: Option<String>,
    reason: Option<&'static str>,
    message: Option<String>,
}

impl Verdict {
    pub fn to_json(&self) -> Value {
        let mut v = json!({"readiness": self.readiness, "value": self.value});
        if let (Some(c), Some(id)) = (&self.on, &self.on_id) {
            v["on"] = json!({
                "id": id,
                "path": c.path,
                "line": c.line,
                "done": c.checked,
                "archived": c.archived,
            });
        }
        if let Some(reason) = self.reason {
            v["reason"] = json!(reason);
        }
        v
    }

    /// The `unresolved_wait` diagnostic for an unresolved verdict.
    pub fn diagnostic(&self, path: &str, line: usize, id: Option<&str>) -> Option<Diagnostic> {
        let message = self.message.as_ref()?;
        let mut d = Diagnostic::new(ErrorCode::UnresolvedWait, message.clone())
            .with_path(path)
            .with_line(line);
        d.id = id.map(str::to_string);
        Some(d)
    }
}

/// Every stay id in the scanned files, and the wait of every member item.
#[derive(Default)]
pub struct Resolver {
    ids: crate::citations::IdMap,
    /// Open member items and what they wait for, in scan order.
    open: Vec<(Key, Option<String>, Wait)>,
}

impl Resolver {
    /// Register a file's stay ids. A collection file (`bound` given) also
    /// registers its open member items' waits; an archive file only carries.
    pub fn add(&mut self, path: &str, doc: &Document, bound: Option<(&Bound, &str)>) {
        self.ids.add(path, doc, bound.is_none());
        let Some((bound, field)) = bound else { return };
        let Ok(Some(inst)) = schema::instance(doc, bound) else {
            return;
        };
        for &(list, item) in &inst.items {
            if doc.lists[list].items[item]
                .checkbox
                .as_ref()
                .is_some_and(|c| c.checked)
            {
                continue;
            }
            // An item that does not extract gets no verdict and leads nowhere;
            // `query` reports it as `invalid_item` where the file is listed.
            let Ok(values) = schema::extract(&doc.item_content(list, item), &bound.schema) else {
                continue;
            };
            let value = values
                .get(field)
                .and_then(Value::as_str)
                .map(str::to_string);
            let wait = parse(value.as_deref());
            self.open
                .push(((path.to_string(), list, item), value, wait));
        }
    }

    /// The verdict for every open member item.
    pub fn finish(self) -> BTreeMap<Key, Verdict> {
        let mut verdicts = BTreeMap::new();
        let mut next: BTreeMap<Key, Key> = BTreeMap::new();
        for (key, value, wait) in &self.open {
            let verdict =
                |readiness, reason: Option<&'static str>, message: Option<String>| Verdict {
                    readiness,
                    value: value.clone(),
                    on: None,
                    on_id: None,
                    reason,
                    message,
                };
            let v = match wait {
                Wait::Unlabelled => verdict(Readiness::Unlabelled, None, None),
                Wait::None => verdict(Readiness::Ready, None, None),
                Wait::Condition => verdict(Readiness::Condition, None, None),
                Wait::Malformed => verdict(
                    Readiness::Unresolved,
                    Some("malformed"),
                    Some(format!(
                        "wait {:?} starts item: but is not item:<id>",
                        value.as_deref().unwrap_or("")
                    )),
                ),
                Wait::Item(id) => self.resolve(id, value, key, &mut next),
            };
            verdicts.insert(key.clone(), v);
        }
        // Walked before any verdict changes, so every member of a loop is named,
        // and an item that only leads into someone else's loop stays waiting.
        let looped: Vec<Key> = next
            .keys()
            .filter(|start| {
                let mut seen = BTreeSet::new();
                let mut at = *start;
                while let Some(n) = next.get(at) {
                    if n == *start {
                        return true;
                    }
                    if !seen.insert(n) {
                        return false;
                    }
                    at = n;
                }
                false
            })
            .cloned()
            .collect();
        for key in looped {
            let v = verdicts.get_mut(&key).expect("looped items have verdicts");
            v.readiness = Readiness::Unresolved;
            v.reason = Some("cycle");
            v.message = Some(format!(
                "wait item:{} leads back to this item through open items",
                v.on_id.as_deref().unwrap_or("")
            ));
        }
        verdicts
    }

    fn resolve(
        &self,
        id: &str,
        value: &Option<String>,
        key: &Key,
        next: &mut BTreeMap<Key, Key>,
    ) -> Verdict {
        let unresolved = |reason, message: String| Verdict {
            readiness: Readiness::Unresolved,
            value: value.clone(),
            on: None,
            on_id: Some(id.to_string()),
            reason: Some(reason),
            message: Some(message),
        };
        let carrier = match self.ids.resolve(id, "wait item:") {
            Ok(carrier) => carrier,
            Err(problem) => {
                return unresolved(
                    if problem.code == "citation_unresolved" {
                        "dangling"
                    } else {
                        "ambiguous"
                    },
                    problem.message,
                );
            }
        };
        let Some(checked) = carrier.checked else {
            return unresolved(
                "not_an_item",
                format!(
                    "wait item:{id} names {}:{}, which is not a task item",
                    carrier.path, carrier.line
                ),
            );
        };
        if !checked && let Some((list, item)) = carrier.item {
            next.insert(key.clone(), (carrier.path.clone(), list, item));
        }
        Verdict {
            readiness: if checked {
                Readiness::Ready
            } else {
                Readiness::Waiting
            },
            value: value.clone(),
            on: Some(carrier.clone()),
            on_id: Some(id.to_string()),
            reason: None,
            message: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_parse_into_one_wait_with_an_optional_note() {
        assert_eq!(parse(None), Wait::Unlabelled);
        assert_eq!(parse(Some("  ")), Wait::Unlabelled);
        assert_eq!(parse(Some("none")), Wait::None);
        assert_eq!(parse(Some("none (decided 2026-01-02)")), Wait::None);
        assert_eq!(parse(Some("item:Ab3_x-9Z")), Wait::Item("Ab3_x-9Z".into()));
        assert_eq!(
            parse(Some("item:Ab3_x-9Z (after the (first) review)")),
            Wait::Item("Ab3_x-9Z".into())
        );
        assert_eq!(parse(Some("item:")), Wait::Malformed);
        assert_eq!(parse(Some("item:repo#162 merge")), Wait::Malformed);
        assert_eq!(parse(Some("item:Ab3 (a) (b)")), Wait::Malformed);
        assert_eq!(parse(Some("human:sam")), Wait::Condition);
        assert_eq!(parse(Some("2026-01-02")), Wait::Condition);
        assert_eq!(parse(Some("the vendor's reply")), Wait::Condition);
        assert_eq!(parse(Some("none (a) (b)")), Wait::Condition);
    }

    #[test]
    fn readiness_and_citations_share_live_archive_identity_selection() {
        use crate::citations::{Citation, Form};
        use crate::contract::Span;
        let task = |checked: &str| {
            format!(
                "## Tasks\n\n- [{checked}] target <!-- stay:Target01 subhash=sha256:111111111111 -->\n"
            )
        };
        for (live, archives, expected, citation_problem) in [
            (Some(task(" ")), vec![task("x")], Readiness::Waiting, None),
            (Some(task("x")), vec![task(" ")], Readiness::Ready, None),
            (
                Some(task("-")),
                vec![task("x")],
                Readiness::Unresolved,
                None,
            ),
            (
                Some("## Target\n<!-- stay:Target01 -->\n".into()),
                vec![task("x")],
                Readiness::Unresolved,
                Some("citation_kind"),
            ),
            (None, vec![task("x")], Readiness::Ready, None),
            (
                None,
                vec![task("x"), task("x")],
                Readiness::Unresolved,
                Some("citation_ambiguous"),
            ),
        ] {
            let mut resolver = Resolver::default();
            let has_live = live.is_some();
            if let Some(source) = live {
                resolver
                    .ids
                    .add("live.md", &Document::parse(&source).unwrap(), false);
            }
            for (i, source) in archives.iter().enumerate() {
                resolver.ids.add(
                    &format!("archive/{i}.md"),
                    &Document::parse(source).unwrap(),
                    true,
                );
            }
            let citation = Citation {
                id: "Target01".into(),
                form: Form::Item,
                span: Span { start: 0, end: 13 },
            };
            assert_eq!(
                resolver.ids.check(&citation).map(|p| p.code),
                citation_problem
            );
            let verdict = resolver.resolve(
                "Target01",
                &Some("item:Target01".into()),
                &("wait.md".into(), 0, 0),
                &mut BTreeMap::new(),
            );
            assert_eq!(verdict.readiness, expected);
            // Kind and task state are checked after live precedence.
            if let Some(carrier) = verdict.on {
                assert_eq!(carrier.archived, !has_live);
            }
        }
    }
}
