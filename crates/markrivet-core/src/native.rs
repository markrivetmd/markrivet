//! Source identity rules shared by the confined importers.
use crate::document::{Attach, Target};
use crate::{Diagnostic, Document, ErrorCode, Selector, Span};
use std::collections::BTreeSet;

fn refused(message: impl Into<String>) -> Diagnostic {
    Diagnostic::new(ErrorCode::ArchiveCopyRefused, message)
}

fn inside(outer: Span, inner: Span) -> bool {
    outer.start <= inner.start && inner.end <= outer.end
}

pub(crate) fn heading_stay(doc: &Document, section: usize, id: &str) -> bool {
    doc.stay_ids(&doc.blocks[doc.sections[section].block].markers)
        .iter()
        .any(|s| s == id)
}

pub(crate) fn preconditions(
    doc: &Document,
    target: Target,
    id: &str,
    parent: Option<&str>,
) -> Result<(), Diagnostic> {
    let Target::Section(section) = target else {
        return if parent.is_none() {
            Ok(())
        } else {
            Err(refused(
                "parent precondition is only valid for a section archive",
            ))
        };
    };
    if !heading_stay(doc, section, id) {
        return Err(refused(
            "section archive requires its own heading stay; stamp it first",
        ));
    }
    let parent = parent.ok_or_else(|| refused("section archive requires a parent precondition"))?;
    let p = doc.resolve(&Selector::Id { id: parent.into() })?;
    if !matches!(p,Target::Section(p) if heading_stay(doc,p,parent) && doc.sections[section].parent==Some(p))
    {
        return Err(refused(
            "section archive parent precondition does not name the stamped direct parent",
        ));
    }
    Ok(())
}

/// Why a live marker's evidence blocks an archive or a move. Drift and
/// refreshability are separate facts: this says only that the stored evidence
/// is unusable, never whether a `refresh` write can fix it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum EvidenceBlock {
    /// An item stay without supported attributes or a stored `subhash`.
    InvalidChild,
    /// The stored digest is not a prefix of the unit's current digest.
    Drift { fresh: String, stored: String },
}

/// The one evidence classification archive, move, `doctor` and `refresh` share.
pub(crate) fn evidence_block(doc: &Document, marker: usize) -> Option<EvidenceBlock> {
    let m = &doc.markers[marker];
    let evidence = doc.marker_evidence(marker);
    if matches!(m.attach, Attach::Item { .. })
        && (!m.lexical.plain || evidence.as_ref().is_none_or(|(_, stored)| stored.is_none()))
    {
        return Some(EvidenceBlock::InvalidChild);
    }
    match evidence {
        Some((fresh, Some(stored))) if !fresh.starts_with(&stored) => {
            Some(EvidenceBlock::Drift { fresh, stored })
        }
        _ => None,
    }
}

pub(crate) fn markers(doc: &Document, target: Target) -> Result<(), Diagnostic> {
    let span = doc.target_span(target).0;
    let outside: BTreeSet<&str> = crate::citations::identity_claims(doc)
        .filter(|(_, s)| !inside(span, *s))
        .map(|(id, _)| id)
        .collect();
    if matches!(target, Target::Item { .. }) && doc.target_ids(target).is_empty() {
        return Err(refused("archive source must have an item-owned stay"));
    }
    for (i, m) in doc
        .markers
        .iter()
        .enumerate()
        .filter(|(_, m)| inside(span, m.lexical.span))
    {
        let id = m.lexical.id.as_deref().unwrap_or("<unidentified>");
        let owned = match (target, m.attach) {
            (Target::Item { list, item }, Attach::Item { list: l, item: i }) => {
                list == l && item == i
            }
            (Target::Section(_), Attach::Block(b)) => inside(span, doc.blocks[b].span),
            (Target::Section(_), Attach::Item { list, item }) => {
                inside(span, doc.lists[list].items[item].span)
            }
            _ => false,
        };
        if !owned {
            return Err(refused(format!("source unit has unaddressed stay {id}")));
        }
        if outside.contains(id) {
            return Err(refused(format!(
                "source stay {id} is also carried outside the unit"
            )));
        }
        match evidence_block(doc, i) {
            Some(EvidenceBlock::InvalidChild) => {
                return Err(refused(format!(
                    "source stay {id} requires supported attributes and valid child evidence"
                )));
            }
            Some(EvidenceBlock::Drift { .. }) => {
                return Err(refused(format!(
                    "source stay {id} evidence has drifted; review the change, then refresh it with a separate `refresh` write"
                )));
            }
            None => {}
        }
    }
    if let Target::Section(_) = target {
        for (s, section) in doc
            .sections
            .iter()
            .enumerate()
            .filter(|(_, s)| span.contains(s.whole.start))
        {
            if doc.blocks[section.block]
                .heading
                .as_ref()
                .is_some_and(|h| h.legacy_id.is_some())
                && doc.blocks[section.block].markers.is_empty()
            {
                return Err(refused(format!(
                    "section archive has legacy-only heading stay {}",
                    doc.section_ids(s).join(",")
                )));
            }
        }
    }
    Ok(())
}
