//! Evidence refresh: rewrite named stays' stored digests to their unit's
//! current digest, at the stored precision, and change no other byte.
//!
//! Selection is validated in full against the preimage before any digest is
//! spliced, and again, independently, by the intent at the planning gate. The
//! candidate must equal the preimage with only the named digest values
//! replaced, with every marker occurrence, owner and diagnostic unchanged.
use super::*;
use crate::native::{EvidenceBlock, evidence_block};

/// Longest stored digest a fresh SHA-256 hex digest can be a prefix of.
const FULL_HEX: usize = 64;

/// One stay named by a `refresh` op: its id and the stored digest the caller saw.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RefreshSelect {
    pub id: String,
    pub expect: String,
}

/// What `refresh` did, or found nothing to do, for one named stay.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Refreshed {
    pub id: String,
    /// `hash` for a block owner, `subhash` for an item.
    pub key: String,
    pub stored: String,
    /// The unit's current digest at the stored precision.
    pub fresh: String,
    /// `refreshed`, or `unchanged` when the stored digest was already current.
    pub status: String,
    /// `section`, `block`, `list` or `item`.
    pub owner: String,
    /// The owning unit's byte extent in the preimage.
    pub span: Span,
    /// The marker's 1-based file line.
    pub line: usize,
}

/// A validated selection: one live, addressed, plain marker per id.
struct Selected {
    marker: usize,
    key: &'static str,
    stored: String,
    /// Absolute extent of the stored hex digits.
    value: Span,
    /// The fresh digest at the stored precision.
    fresh: String,
    drifted: bool,
}

/// The digest key, value span (marker-relative) and stored value Rivet's
/// evidence binds for this marker's attachment.
fn digest_of(m: &Marker) -> (&'static str, Option<Span>, Option<&String>) {
    if matches!(m.attach, Attach::Item { .. }) {
        (
            "subhash",
            m.lexical.subhash_value,
            m.lexical.subhash.as_ref(),
        )
    } else {
        ("hash", m.lexical.hash_value, m.lexical.hash.as_ref())
    }
}

/// Why `refresh` cannot rewrite this live marker's evidence, if it cannot.
/// Shared by the op's selection and `doctor`'s drift findings.
pub(crate) fn unrefreshable(doc: &Document, marker: usize) -> Option<(ErrorCode, String)> {
    if let Some(d) = &doc.write_blocker {
        return Some((
            d.code,
            format!("the document refuses every write: {}", d.message),
        ));
    }
    let m = &doc.markers[marker];
    let Some(id) = m.lexical.id.as_deref() else {
        return Some((ErrorCode::MalformedMarker, "the marker has no id".into()));
    };
    // Every live claim counts: orphaned, unaddressed and HTML-block copies
    // collide with the addressed one, and so does a heading's `{#id}`.
    let carried = doc
        .markers
        .iter()
        .filter(|x| x.lexical.id.as_deref() == Some(id))
        .count();
    if carried != 1
        || doc.ids.get(id).is_none_or(|t| t.len() != 1)
        || doc.conflicted.iter().any(|c| c == id)
    {
        return Some((
            ErrorCode::DuplicateId,
            format!("stay {id} is carried more than once"),
        ));
    }
    if !matches!(m.attach, Attach::Block(_) | Attach::Item { .. }) {
        return Some((
            ErrorCode::UnaddressedMarker,
            format!("stay {id} addresses no unit, so it has no evidence to refresh"),
        ));
    }
    if !m.lexical.plain {
        return Some((
            ErrorCode::UnsupportedSyntax,
            format!("stay {id} has unsupported attributes; only id plus one plain digest each"),
        ));
    }
    let (key, value, stored) = digest_of(m);
    let (Some(value), Some(_)) = (value, stored) else {
        return Some((
            ErrorCode::InvalidRequest,
            format!("stay {id} stores no {key} digest; there is no evidence to refresh"),
        ));
    };
    if !(1..=FULL_HEX).contains(&value.len()) {
        return Some((
            ErrorCode::UnsupportedSyntax,
            format!(
                "stay {id} stores a {key} of {} hex digits; refresh supports 1 to {FULL_HEX}",
                value.len()
            ),
        ));
    }
    None
}

fn resolve(doc: &Document, select: &[RefreshSelect]) -> Result<Vec<Selected>, Diagnostic> {
    check_write_blocker(doc)?;
    if select.is_empty() {
        return Err(err(
            ErrorCode::InvalidRequest,
            "refresh selects at least one stay",
        ));
    }
    let mut seen = BTreeSet::new();
    let mut out = vec![];
    for s in select {
        if !seen.insert(s.id.as_str()) {
            return Err(err(
                ErrorCode::InvalidRequest,
                format!("refresh names stay {} more than once", s.id),
            )
            .with_id(s.id.clone()));
        }
        let Some(marker) = doc
            .markers
            .iter()
            .position(|m| m.lexical.id.as_deref() == Some(s.id.as_str()))
        else {
            return Err(err(
                ErrorCode::MissingSelector,
                format!("no live stay marker carries id {}", s.id),
            )
            .with_id(s.id.clone()));
        };
        let m = &doc.markers[marker];
        if let Some((code, message)) = unrefreshable(doc, marker) {
            return Err(err(code, message)
                .with_id(s.id.clone())
                .with_span(m.lexical.span));
        }
        let (key, value, stored) = digest_of(m);
        let (value, stored) = (value.expect("checked"), stored.expect("checked").clone());
        if s.expect != stored {
            return Err(err(
                ErrorCode::GuardConflict,
                format!(
                    "stay {} stores {key}={stored}, not the expected {}; preview again",
                    s.id, s.expect
                ),
            )
            .with_id(s.id.clone()));
        }
        let (full, _) = doc.marker_evidence(marker).expect("addressed marker");
        let drifted = matches!(
            evidence_block(doc, marker),
            Some(EvidenceBlock::Drift { .. })
        );
        let fresh = full[..value.len()].to_string();
        let start = m.lexical.span.start;
        out.push(Selected {
            marker,
            key,
            stored,
            value: Span {
                start: start + value.start,
                end: start + value.end,
            },
            fresh,
            drifted,
        });
    }
    // Every selection is valid; only now derive the rewrites. The markstay
    // re-read inside `refresh_digest` is a cross-check, never the validation.
    for s in out.iter().filter(|s| s.drifted) {
        let m = &doc.markers[s.marker];
        let raw = &doc.source[m.lexical.span.start..m.lexical.span.end];
        let relative = Span {
            start: s.value.start - m.lexical.span.start,
            end: s.value.end - m.lexical.span.start,
        };
        let (full, _) = doc.marker_evidence(s.marker).expect("addressed marker");
        let rewritten = markers::refresh_digest(raw, relative, &full);
        if rewritten.as_deref().is_none_or(|r| {
            r.len() != raw.len()
                || r[..relative.start] != raw[..relative.start]
                || r[relative.end..] != raw[relative.end..]
                || r[relative.start..relative.end] != s.fresh
        }) {
            let id = m.lexical.id.clone().unwrap_or_default();
            return Err(err(
                ErrorCode::UnsupportedSyntax,
                format!("stay {id} cannot have its {} rewritten safely", s.key),
            )
            .with_id(id));
        }
    }
    Ok(out)
}

fn owner(doc: &Document, id: &str) -> (&'static str, Span) {
    let target = doc.ids[id][0];
    let kind = match target {
        Target::Section(_) => "section",
        Target::Item { .. } => "item",
        Target::Block(b) if doc.blocks[b].list.is_some() => "list",
        _ => "block",
    };
    (kind, doc.target_span(target).0)
}

/// Plan a `refresh` op. The guard (the document revision) is checked by the
/// caller before this runs, so a stale guard refuses even a no-op.
pub(crate) fn plan(doc: &Document, select: &[RefreshSelect]) -> Result<Change, Diagnostic> {
    let selected = resolve(doc, select)?;
    let mut change = Change {
        splices: selected
            .iter()
            .filter(|s| s.drifted)
            .map(|s| Splice {
                span: s.value,
                text: s.fresh.clone(),
            })
            .collect(),
        ..Change::default()
    };
    change.refreshed = selected
        .iter()
        .map(|s| {
            let m = &doc.markers[s.marker];
            let id = m.lexical.id.clone().expect("selected by id");
            let (kind, span) = owner(doc, &id);
            Refreshed {
                id,
                key: s.key.to_string(),
                stored: s.stored.clone(),
                fresh: s.fresh.clone(),
                status: if s.drifted { "refreshed" } else { "unchanged" }.to_string(),
                owner: kind.to_string(),
                span,
                line: doc.file_line(m.lexical.span.start),
            }
        })
        .collect();
    change.expectation = Some(Expectation::Refresh(Box::new(RefreshIntent {
        select: select.to_vec(),
    })));
    Ok(change)
}

pub(crate) struct RefreshIntent {
    select: Vec<RefreshSelect>,
}

impl RefreshIntent {
    pub(super) fn target(&self, _doc: &Document) -> Result<Target, Diagnostic> {
        Ok(Target::Document)
    }

    pub(super) fn allowed(&self, doc: &Document) -> Result<Vec<Span>, Diagnostic> {
        Ok(resolve(doc, &self.select)?
            .into_iter()
            .filter(|s| s.drifted)
            .map(|s| s.value)
            .collect())
    }

    /// The change makes exactly the digest splices the request derives.
    pub(super) fn validate(&self, doc: &Document, splices: &[Splice]) -> Result<(), Diagnostic> {
        let mut want: Vec<(Span, String)> = resolve(doc, &self.select)?
            .into_iter()
            .filter(|s| s.drifted)
            .map(|s| (s.value, s.fresh))
            .collect();
        let mut got: Vec<(Span, String)> =
            splices.iter().map(|s| (s.span, s.text.clone())).collect();
        want.sort_by_key(|(s, _)| s.start);
        got.sort_by_key(|(s, _)| s.start);
        if want != got {
            return Err(err(
                ErrorCode::InvalidRequest,
                "a refresh makes exactly the digest splices its request derives",
            ));
        }
        Ok(())
    }

    /// The blocks holding a rewritten digest skip `verify`'s whole-block byte
    /// comparison; `check` compares the whole candidate instead.
    pub(super) fn declare(&self, doc: &Document, change: &mut Change) -> Result<(), Diagnostic> {
        for s in resolve(doc, &self.select)?
            .into_iter()
            .filter(|s| s.drifted)
        {
            let span = doc.markers[s.marker].lexical.span;
            change.blocks.extend(
                doc.blocks
                    .iter()
                    .enumerate()
                    .filter(|(_, b)| b.span.start < span.end && span.start < b.span.end)
                    .map(|(i, _)| i),
            );
            change
                .carried
                .extend(doc.markers[s.marker].lexical.id.clone());
        }
        Ok(())
    }

    pub(super) fn check(self, doc: &Document, c: &Document, _: &Mapper) -> Result<(), Diagnostic> {
        let fail = |m: &str| {
            Err(err(
                ErrorCode::InvalidContent,
                format!("refresh verification failed: {m}"),
            ))
        };
        let selected = resolve(doc, &self.select)?;
        let mut expected = doc.source.clone();
        for s in selected.iter().filter(|s| s.drifted) {
            expected.replace_range(s.value.start..s.value.end, &s.fresh);
        }
        if c.source != expected {
            return fail("a byte other than a named digest value would change");
        }
        let rewritten: BTreeMap<usize, &Selected> = selected
            .iter()
            .filter(|s| s.drifted)
            .map(|s| (s.marker, s))
            .collect();
        if doc.markers.len() != c.markers.len() {
            return fail("the live marker count would change");
        }
        for (i, (a, b)) in doc.markers.iter().zip(&c.markers).enumerate() {
            let (x, y) = (&a.lexical, &b.lexical);
            let same = x.span == y.span
                && x.id == y.id
                && a.attach == b.attach
                && x.plain == y.plain
                && x.has_subhash == y.has_subhash
                && x.malformed == y.malformed
                && x.mdx == y.mdx
                && x.hash_value == y.hash_value
                && x.subhash_value == y.subhash_value;
            let digests = match rewritten.get(&i) {
                Some(s) if s.key == "hash" => {
                    y.hash.as_deref() == Some(s.fresh.as_str()) && x.subhash == y.subhash
                }
                Some(s) => y.subhash.as_deref() == Some(s.fresh.as_str()) && x.hash == y.hash,
                None => x.hash == y.hash && x.subhash == y.subhash,
            };
            if !same || !digests {
                return fail("a marker occurrence, owner or unnamed digest would change");
            }
        }
        let lexical = |d: &Document| {
            markers::scan(&d.source)
                .map(|all| all.into_iter().map(|m| (m.span, m.id)).collect::<Vec<_>>())
        };
        if lexical(doc).ok() != lexical(c).ok() {
            return fail("a lexical marker would change");
        }
        // Declaring a block rewritten skips verify's whole per-block check
        // (extent, kind, stays, marker positions), not only its bytes, so the
        // structure is compared here explicitly.
        let shape = |d: &Document| {
            (
                d.blocks
                    .iter()
                    .map(|b| (b.span, b.kind))
                    .collect::<Vec<_>>(),
                d.lists
                    .iter()
                    .map(|l| l.items.iter().map(|i| i.span).collect::<Vec<_>>())
                    .collect::<Vec<_>>(),
            )
        };
        if shape(doc) != shape(c) {
            return fail("a block or list item extent or kind would change");
        }
        if doc.ids != c.ids || doc.conflicted != c.conflicted || doc.diagnostics != c.diagnostics {
            return fail("an identity or marker diagnostic would change");
        }
        if rewritten.keys().any(|&i| evidence_block(c, i).is_some()) {
            return fail("a refreshed digest would not be current");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DOC: &str = "# Head\n<!-- stay:hd000001 hash=sha256:000000000000 -->\n\nProse. <!-- stay:pa000001 hash=sha256:000000000000 -->\n";

    fn intent() -> RefreshIntent {
        RefreshIntent {
            select: vec![RefreshSelect {
                id: "hd000001".into(),
                expect: "000000000000".into(),
            }],
        }
    }

    fn check(candidate: &str) -> Result<(), Diagnostic> {
        let doc = Document::parse(DOC).unwrap();
        let c = Document::parse(candidate).unwrap();
        intent().check(&doc, &c, &Mapper::new(&[]))
    }

    /// The verifier, not the planner, refuses any candidate other than the
    /// preimage with exactly the named digest value replaced.
    #[test]
    fn only_the_named_digest_value_may_change() {
        let doc = Document::parse(DOC).unwrap();
        let change = plan(&doc, &intent().select).unwrap();
        let [splice] = change.splices.as_slice() else {
            panic!("one digest splice")
        };
        let fresh = splice.text.clone();
        let good = DOC.replacen("000000000000", &fresh, 1);
        check(&good).unwrap();
        for (what, bad) in [
            ("unrefreshed", DOC.to_string()),
            ("prose byte", good.replace("Prose.", "Prose!")),
            (
                "unnamed digest",
                good.replace(
                    "pa000001 hash=sha256:000000000000",
                    "pa000001 hash=sha256:000000000001",
                ),
            ),
            (
                "wrong value",
                DOC.replacen("000000000000", "123456789abc", 1),
            ),
        ] {
            let e = check(&bad).expect_err(what);
            assert_eq!(e.code, ErrorCode::InvalidContent, "{what}: {e:?}");
        }
    }

    /// Equal bytes but different parsed structure still refuse: the block
    /// check replaces the one `verify` skips for a declared-rewritten block.
    #[test]
    fn block_extent_and_kind_are_checked_independently_of_bytes() {
        let doc = Document::parse(DOC).unwrap();
        let fresh = plan(&doc, &intent().select).unwrap().splices[0]
            .text
            .clone();
        let good = DOC.replacen("000000000000", &fresh, 1);
        for what in ["end", "kind"] {
            let mut c = Document::parse(&good).unwrap();
            match what {
                "end" => c.blocks[0].span.end += 1,
                _ => c.blocks[0].kind = "paragraph",
            }
            let e = intent().check(&doc, &c, &Mapper::new(&[])).expect_err(what);
            assert_eq!(e.code, ErrorCode::InvalidContent, "{what}: {e:?}");
        }
    }
}
