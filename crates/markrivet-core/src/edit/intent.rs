//! Required, data-only operation expectations and their source permissions.
use super::*;
use crate::schema;

#[cfg(test)]
#[path = "../../tests/planning/internal.rs"]
mod tests;

pub(crate) enum GuardSubject {
    Native(Target),
    Item {
        bound: schema::Bound,
        locator: schema::ItemLocator,
    },
    Collection(schema::Bound),
    BoundBody {
        bound: schema::Bound,
        locator: schema::ItemLocator,
        native: String,
    },
    CheckboxStamp {
        bound: schema::Bound,
        native: String,
    },
}

pub(crate) struct Guard {
    subject: GuardSubject,
    supplied: String,
    /// The target was named by a position handle: a conflict then carries no
    /// `current`, which may belong to another item, and may carry `relocated`.
    positional: bool,
}

fn invalid(message: &str) -> Diagnostic {
    err(ErrorCode::InvalidRequest, message)
}

pub(super) fn contains(outer: Span, inner: Span) -> bool {
    outer.start <= inner.start && inner.start <= inner.end && inner.end <= outer.end
}

pub(super) fn preserve_foreign(
    doc: &Document,
    span: Span,
    own: &[usize],
    message: &str,
    located: bool,
) -> Result<(), Diagnostic> {
    if let Some(&m) = foreign_markers(doc, span, own).first() {
        let d = err(ErrorCode::UnsupportedSyntax, message);
        return Err(if located {
            d.with_span(doc.markers[m].lexical.span)
        } else {
            d
        });
    }
    Ok(())
}

pub(super) fn delete_span(doc: &Document, target: Target) -> Result<Span, Diagnostic> {
    target_exists(doc, target)?;
    let span = match target {
        Target::Item { list, item } => {
            let l = &doc.lists[list];
            if l.items.len() == 1 {
                return Err(invalid(
                    "cannot delete the only item; delete the list instead",
                ));
            }
            if item + 1 < l.items.len() {
                Span {
                    start: l.items[item].span.start,
                    end: l.items[item + 1].span.start,
                }
            } else {
                Span {
                    start: l.items[item - 1].span.end,
                    end: l.items[item].span.end,
                }
            }
        }
        Target::Section(s) => doc.sections[s].whole,
        Target::Block(b) => Span {
            start: doc.blocks[b].span.start,
            end: doc
                .blocks
                .get(b + 1)
                .map_or(doc.source.len(), |b| b.span.start),
        },
        _ => return Err(invalid("delete targets an item, section or block")),
    };
    let (guarded, _) = doc.target_span(target);
    // Item separators can precede the guard; block separators can follow it.
    // Reference definitions have no block of their own, so never infer that
    // bytes outside the guard are separators merely from the next AST node.
    for gap in [
        (span.start, guarded.start.min(span.end)),
        (guarded.end.max(span.start), span.end),
    ] {
        if gap.0 < gap.1 {
            let gap = Span {
                start: gap.0,
                end: gap.1,
            };
            if !gap
                .slice(&doc.source)
                .expect("delete gap")
                .bytes()
                .all(|b| matches!(b, b' ' | b'\t' | b'\r' | b'\n' | b'\x0b' | b'\x0c'))
            {
                return Err(err(
                    ErrorCode::UnsupportedSyntax,
                    "delete would remove non-whitespace bytes outside the target guard",
                )
                .with_span(gap));
            }
        }
    }
    Ok(span)
}

/// Resolve the explicit list authority identically during planning and recheck.
/// Without it, sole-item deletion retains the ordinary refusal.
pub(super) fn guarded_delete_span(
    doc: &Document,
    target: Target,
    list_guard: Option<&ListGuard>,
) -> Result<Span, Diagnostic> {
    let Some(list_guard) = list_guard else {
        return delete_span(doc, target);
    };
    target_exists(doc, target)?;
    let Target::Item { list, item } = target else {
        return Err(invalid("list authority requires an item target"));
    };
    let l = &doc.lists[list];
    let it = &l.items[item];
    preserve_foreign(
        doc,
        it.span,
        &it.markers,
        "the item holds a marker owned by another unit; delete refused",
        true,
    )?;
    if l.items.len() != 1 {
        return Err(invalid("list authority requires exactly one item"));
    }
    let block = Target::Block(l.block);
    check_guard(doc, block, &list_guard.guard)?;
    let guarded = doc.target_span(block).0;
    for m in &doc.markers {
        if contains(guarded, m.lexical.span)
            && !contains(it.span, m.lexical.span)
            && m.attach != Attach::Block(l.block)
        {
            let id = m.lexical.id.as_deref().unwrap_or("<malformed>");
            let line = doc.file_line(m.lexical.span.start);
            return Err(err(
                ErrorCode::ListTrailerRefused,
                format!("list trailer marker {id} at line {line} does not attach to this list"),
            )
            .with_span(m.lexical.span)
            .with_line(line)
            .with_id(id));
        }
    }
    delete_span(doc, block)
}

/// The source membership precondition is independent of the native byte guard.
pub(super) fn delete_collection(
    doc: &Document,
    bound: &schema::Bound,
    target: Target,
) -> Result<(), Diagnostic> {
    let conflict = || {
        Diagnostic::new(
            ErrorCode::CollectionConflict,
            format!(
                "the delete target is not an item of collection {:?}",
                bound.name
            ),
        )
    };
    let inst = schema::instance(doc, bound)?.ok_or_else(conflict)?;
    if !matches!(target, Target::Item { list, item } if inst.items.contains(&(list, item))) {
        return Err(conflict());
    }
    Ok(())
}

impl Change {
    /// Used for early, contextual refusals and independently at the planning gate.
    pub(crate) fn preflight(&mut self, doc: &Document) -> Result<(), Diagnostic> {
        check_write_blocker(doc)?;
        let guard = self
            .guard
            .as_ref()
            .ok_or_else(|| invalid("change is missing its guard intent"))?;
        guard.check(doc)?;
        let intent = self
            .expectation
            .take()
            .ok_or_else(|| invalid("change is missing its required expectation"))?;
        let authorization = guard.authorizes(doc, &intent);
        let result = (|| {
            authorization?;
            let allowed = intent.allowed(doc)?;
            for splice in &self.splices {
                let span = splice.span;
                if span.start > span.end
                    || span.end > doc.source.len()
                    || !doc.source.is_char_boundary(span.start)
                    || !doc.source.is_char_boundary(span.end)
                {
                    return Err(invalid(
                        "splice is outside source bounds or splits a UTF-8 character",
                    ));
                }
                if !allowed.iter().any(|&unit| contains(unit, span)) {
                    return Err(invalid(
                        "splice lies outside its guarded operation's edit units",
                    ));
                }
            }
            intent.validate(doc, &self.splices)?;
            intent.declare(doc, self)?;
            for region in &self.regions {
                let list = doc
                    .lists
                    .get(region.list())
                    .ok_or_else(|| invalid("item region names a missing list"))?;
                match *region {
                    ItemRegion::Spliced {
                        splice,
                        offset,
                        len,
                        ..
                    } => {
                        let text = &self
                            .splices
                            .iter()
                            .find(|s| s.span == splice)
                            .ok_or_else(|| invalid("item region names another change's splice"))?
                            .text;
                        if offset
                            .checked_add(len)
                            .is_none_or(|end| end > text.len() || !text.is_char_boundary(end))
                            || !text.is_char_boundary(offset)
                        {
                            return Err(invalid("item region exceeds its splice text"));
                        }
                        let b = &doc.blocks[list.block];
                        if !contains(b.span, splice) {
                            return Err(invalid("item region lies outside its source list"));
                        }
                    }
                    ItemRegion::Mapped { span, .. } | ItemRegion::InPlace { span, .. } => {
                        if !list.items.iter().any(|i| i.span == span) {
                            return Err(invalid("mapped region does not name an original item"));
                        }
                        if let ItemRegion::Mapped { tail, .. } = *region {
                            let inserted = self
                                .splices
                                .iter()
                                .filter(|s| s.span.is_empty() && s.span.start == span.end)
                                .map(|s| s.text.len())
                                .sum::<usize>();
                            if tail > inserted {
                                return Err(invalid("mapped item tail exceeds its insertion"));
                            }
                        } else if self
                            .splices
                            .iter()
                            .any(|s| contains(span, s.span) && s.span.len() != s.text.len())
                        {
                            return Err(invalid("in-place item region changes byte length"));
                        }
                    }
                }
            }
            Ok(())
        })();
        self.expectation = Some(intent);
        result
    }
}

fn target_exists(doc: &Document, target: Target) -> Result<(), Diagnostic> {
    let exists = match target {
        Target::Document => true,
        Target::Block(b) => doc.blocks.get(b).is_some(),
        Target::Section(s) => doc.sections.get(s).is_some(),
        Target::Item { list, item } => doc
            .lists
            .get(list)
            .and_then(|l| l.items.get(item))
            .is_some(),
        Target::FrontmatterKey(k) => doc.scalars().get(k).is_some(),
    };
    exists
        .then_some(())
        .ok_or_else(|| invalid("intent names a missing source unit"))
}

impl Guard {
    pub(crate) fn new(subject: GuardSubject, supplied: &str) -> Self {
        let positional = matches!(
            &subject,
            GuardSubject::Item { locator, .. } | GuardSubject::BoundBody { locator, .. }
                if locator.is_position()
        );
        Self {
            subject,
            supplied: supplied.to_string(),
            positional,
        }
    }

    /// Mark a native guard as taken through a position handle.
    pub(crate) fn at_position(mut self) -> Self {
        self.positional = true;
        self
    }

    pub(crate) fn check(&self, doc: &Document) -> Result<(), Diagnostic> {
        super::check_write_blocker(doc)?;
        match self.check_revisions(doc) {
            Err(mut e) if self.positional && e.code == ErrorCode::GuardConflict => {
                e.current = None;
                e.message = if e.message == STALE {
                    "the item at this position changed since the guard was read (positional handle; re-read)"
                        .to_string()
                } else {
                    format!("{} (positional handle; re-read)", e.message)
                };
                e.relocated = self.relocated(doc);
                Err(e)
            }
            other => other,
        }
    }

    /// The one direct item in this read that still has every guarded
    /// revision, by its position handle; none when no item or more than one
    /// has them, or when its handle would not resolve back to it.
    fn relocated(&self, doc: &Document) -> Option<crate::Relocated> {
        let member = |bound: &schema::Bound| {
            schema::instance(doc, bound)
                .ok()
                .flatten()
                .map(|inst| inst.items)
                .unwrap_or_default()
        };
        let holds: Box<dyn Fn(usize, usize) -> bool + '_> = match &self.subject {
            GuardSubject::Native(_) => Box::new(|l, i| {
                doc.target_revision(Target::Item { list: l, item: i }) == self.supplied
            }),
            GuardSubject::Item { bound, .. } => {
                let items = member(bound);
                Box::new(move |l, i| {
                    items.contains(&(l, i))
                        && schema::item_revision(doc, bound, l, i) == self.supplied
                })
            }
            GuardSubject::BoundBody { bound, native, .. } => {
                let items = member(bound);
                Box::new(move |l, i| {
                    items.contains(&(l, i))
                        && schema::item_revision(doc, bound, l, i) == self.supplied
                        && doc.target_revision(Target::Item { list: l, item: i }) == *native
                })
            }
            GuardSubject::Collection(_) | GuardSubject::CheckboxStamp { .. } => return None,
        };
        let holds = &holds;
        let mut found = doc.lists.iter().enumerate().flat_map(|(l, list)| {
            (0..list.items.len())
                .filter(move |&i| holds(l, i))
                .map(move |i| Target::Item { list: l, item: i })
        });
        let (Some(target), None) = (found.next(), found.next()) else {
            return None;
        };
        let select = doc.position_of(target)?;
        (doc.resolve(&select).ok() == Some(target)).then_some(crate::Relocated { select })
    }

    fn check_revisions(&self, doc: &Document) -> Result<(), Diagnostic> {
        let current = match &self.subject {
            GuardSubject::Native(target) => {
                target_exists(doc, *target)?;
                doc.target_revision(*target)
            }
            GuardSubject::Item { bound, locator }
            | GuardSubject::BoundBody { bound, locator, .. } => {
                let (list, item) = schema::guarded_item(doc, bound, locator)?;
                if let GuardSubject::BoundBody { native, .. } = &self.subject {
                    check_revision(native, doc.target_revision(Target::Item { list, item }))?;
                }
                schema::item_revision(doc, bound, list, item)
            }
            GuardSubject::Collection(bound) | GuardSubject::CheckboxStamp { bound, .. } => {
                let inst =
                    schema::instance(doc, bound)?.ok_or_else(|| invalid("collection is absent"))?;
                if let GuardSubject::CheckboxStamp { native, .. } = &self.subject {
                    check_revision(native, doc.revision())?;
                }
                schema::collection_revision(doc, bound, &inst)
            }
        };
        check_revision(&self.supplied, current)
    }

    fn authorizes(&self, doc: &Document, expectation: &Expectation) -> Result<(), Diagnostic> {
        let target = expectation.target(doc)?;
        let covers = match &self.subject {
            GuardSubject::Native(guarded) => {
                !matches!(
                    expectation,
                    Expectation::Fields { .. }
                        | Expectation::Stamp {
                            checkbox_only: true,
                            ..
                        }
                ) && target == *guarded
            }
            GuardSubject::Item { bound, locator }
            | GuardSubject::BoundBody { bound, locator, .. } => {
                let (list, item) = schema::guarded_item(doc, bound, locator)?;
                matches!(expectation, Expectation::Fields {fields,..} if !fields.insert && fields.locator==*locator && fields.binding_matches(bound))
                    && target == Target::Item { list, item }
            }
            GuardSubject::Collection(bound) => {
                let inst =
                    schema::instance(doc, bound)?.ok_or_else(|| invalid("collection is absent"))?;
                matches!(expectation, Expectation::Fields { fields, .. } if fields.insert && fields.binding_matches(bound))
                    && match target {
                        Target::Section(s) => inst.members.contains(&s),
                        Target::Block(b) => doc.blocks[b]
                            .list
                            .is_some_and(|list| inst.items.iter().any(|&(l, _)| l == list)),
                        _ => false,
                    }
            }
            GuardSubject::CheckboxStamp { bound, .. } => {
                let inst =
                    schema::instance(doc, bound)?.ok_or_else(|| invalid("collection is absent"))?;
                matches!(expectation, Expectation::Stamp { scope: Target::Document, item_scopes: Some(spans), checkbox_only: true }
                    if *spans == inst.spans(doc))
            }
        };
        covers
            .then_some(())
            .ok_or_else(|| invalid("guard subject does not authorize this operation's source unit"))
    }
}

const STALE: &str = "target changed since the guard was read";

pub(super) fn check_revision(guard: &str, current: String) -> Result<(), Diagnostic> {
    if guard == current {
        return Ok(());
    }
    let expected_kind = revision_kind(&current).expect("computed revision");
    let message = match revision_kind(guard) {
        None => "guard is not a rivet revision".to_string(),
        Some(kind) if kind != expected_kind => {
            format!("guard is a {kind} revision; this target needs a {expected_kind} revision")
        }
        Some(_) => STALE.to_string(),
    };
    let mut d = err(ErrorCode::GuardConflict, message);
    d.current = Some(current);
    Err(d)
}

pub(crate) enum Expectation {
    Checkbox {
        list: usize,
        item: usize,
        region: ItemRegion,
        expect: String,
        ids: Vec<String>,
    },
    Title {
        s: usize,
        start: usize,
        level: u8,
        text: String,
        ids: Vec<String>,
    },
    Body {
        s: usize,
        start: usize,
        level: u8,
        ids: Vec<String>,
        region_span: Span,
        len: usize,
        allow_drop: bool,
    },
    Paragraph {
        s: usize,
        b: usize,
        paragraph_span: Span,
        match_span: Span,
        remove: bool,
    },
    Block {
        b: usize,
        replaced: Span,
        len: usize,
        ids: Vec<String>,
        allow_drop: bool,
    },
    Frontmatter {
        k: usize,
        field: Option<String>,
        key: String,
        expect: Value,
        others: Vec<(String, Value, String)>,
    },
    DeletedKey {
        k: usize,
        key: String,
        others: Vec<(String, Value, String)>,
    },
    InsertedSection {
        s: usize,
        at: usize,
        lead: usize,
        level: u8,
        title: String,
        parent_start: usize,
        parent_ids: Vec<String>,
        ids: Vec<String>,
    },
    Append {
        s: usize,
        at: usize,
        lead: usize,
        len: usize,
        heading: usize,
        level: u8,
        kinds: Vec<&'static str>,
    },
    NewKey {
        key: String,
        expect: Value,
        others: Vec<(String, Value, String)>,
    },
    /// A new key line carrying the caller's exact YAML token.
    AddKeyYaml {
        key: String,
        raw: String,
        expect: Value,
        others: Vec<(String, Value, String)>,
    },
    /// An existing key's value token replaced with the caller's exact one.
    SetKeyYaml {
        k: usize,
        key: String,
        raw: String,
        replace_line: bool,
        expect: Value,
        others: Vec<(String, Value, String)>,
    },
    /// One flat mapping appended to a key's sequence, or the key added
    /// holding it (`k` is `None`).
    AppendKeyItem {
        k: Option<usize>,
        key: String,
        item: serde_json::Map<String, Value>,
        expect: Value,
        others: Vec<(String, Value, String)>,
    },
    Rejoin {
        s: usize,
        list: usize,
        list_start: usize,
        items: usize,
        before: String,
        codes: Vec<String>,
    },
    Adoption {
        list: usize,
        item: usize,
        want: String,
        item_start: usize,
    },
    Item {
        list: usize,
        item: usize,
        region: ItemRegion,
        expect: String,
        ids: Vec<String>,
    },
    InsertedItem {
        list: usize,
        position: Position,
        region: ItemRegion,
        expect: String,
        ids: Vec<String>,
    },
    Delete {
        target: Target,
        span: Span,
        list: Option<ListGuard>,
        collection: Option<Box<schema::Bound>>,
    },
    Stamp {
        scope: Target,
        item_scopes: Option<Vec<Span>>,
        checkbox_only: bool,
    },
    NewList {
        section: usize,
    },
    Move(Box<super::move_intent::MoveIntent>),
    Refresh(Box<super::refresh::RefreshIntent>),
    Archive {
        unit: crate::archive::Unit,
        section: usize,
        at: usize,
        text: String,
        ids: Vec<String>,
    },
    Fields {
        inner: Box<Expectation>,
        fields: Box<schema::FieldExpectation>,
    },
}

impl Expectation {
    /// Where a whole-item write or a tick lands in the candidate.
    pub(crate) fn item_region(&self) -> Option<ItemRegion> {
        match self {
            Self::Item { region, .. } | Self::Checkbox { region, .. } => Some(*region),
            Self::Fields { inner, .. } => inner.item_region(),
            _ => None,
        }
    }

    /// Check descriptor coordinates before any candidate offset arithmetic.
    fn validate(&self, doc: &Document, splices: &[Splice]) -> Result<(), Diagnostic> {
        if let Self::Move(m) = self {
            return m.validate(doc, splices);
        }
        if let Self::Refresh(r) = self {
            return r.validate(doc, splices);
        }
        let text = |span: Span| {
            splices
                .iter()
                .find(|s| s.span == span)
                .map(|s| s.text.as_str())
                .ok_or_else(|| invalid("intent names a missing splice"))
        };
        let fits = |text: &str, offset: usize, len: usize| {
            offset
                .checked_add(len)
                .is_some_and(|end| end <= text.len() && text.is_char_boundary(end))
                && text.is_char_boundary(offset)
        };
        let point = |at| Span { start: at, end: at };
        let valid = match self {
            Self::Title {
                s, start, level, ..
            } => *start == doc.sections[*s].heading.start && *level == doc.sections[*s].level,
            Self::Body {
                s,
                start,
                level,
                region_span,
                len,
                ..
            } => {
                *start == doc.sections[*s].heading.start
                    && *level == doc.sections[*s].level
                    && *region_span == doc.sections[*s].body
                    && text(*region_span)?.len() == *len
            }
            Self::Paragraph {
                s,
                b,
                paragraph_span,
                match_span,
                remove,
            } => {
                let block = &doc.blocks[*b];
                if block.kind != "paragraph" {
                    let lines = markdown::paragraph_source_spans(doc, *paragraph_span).ok_or_else(
                        || {
                            err(
                                ErrorCode::UnsupportedSyntax,
                                "cannot determine the paragraph's source lines",
                            )
                        },
                    )?;
                    if !lines.iter().any(|&line| contains(line, *match_span))
                        || splices
                            .iter()
                            .any(|splice| !lines.iter().any(|&line| contains(line, splice.span)))
                    {
                        return Err(err(
                            ErrorCode::UnsupportedSyntax,
                            "the match overlaps a list or quote container prefix",
                        ));
                    }
                }
                doc.section_of(paragraph_span.start) == Some(*s)
                    && if block.kind == "paragraph" {
                        *paragraph_span == block.span
                    } else {
                        !remove
                            && matches!(block.kind, "list" | "block_quote")
                            && contains(block.span, *paragraph_span)
                            && markdown::block_layout(doc)
                                .iter()
                                .any(|p| p.kind == "paragraph" && p.span == *paragraph_span)
                    }
            }
            Self::Block {
                b, replaced, len, ..
            } => *replaced == doc.blocks[*b].span && text(*replaced)?.len() == *len,
            Self::InsertedSection {
                s,
                at,
                lead,
                parent_start,
                parent_ids,
                ..
            } => {
                *parent_start == doc.sections[*s].heading.start
                    && *parent_ids == doc.section_ids(*s)
                    && fits(text(point(*at))?, *lead, 0)
            }
            Self::Append {
                s,
                at,
                lead,
                len,
                heading,
                level,
                ..
            } => {
                *at == doc.sections[*s].own_end
                    && *heading == doc.sections[*s].heading.start
                    && *level == doc.sections[*s].level
                    && fits(text(point(*at))?, *lead, *len)
            }
            Self::Rejoin {
                list,
                list_start,
                items,
                ..
            } => {
                *list_start == doc.blocks[doc.lists[*list].block].span.start
                    && *items == doc.lists[*list].items.len()
            }
            Self::Adoption {
                list,
                item,
                item_start,
                ..
            } => *item_start == doc.lists[*list].items[*item].span.start,
            Self::Stamp { scope, .. } => matches!(scope, Target::Section(_) | Target::Document),
            Self::Archive { ids, .. } => !ids.is_empty() && splices.len() == 1,
            Self::Delete { span, .. } => {
                return if splices.len() == 1
                    && splices[0].span == *span
                    && splices[0].text.is_empty()
                {
                    Ok(())
                } else {
                    Err(invalid(
                        "delete intent requires one empty replacement of its entire source unit",
                    ))
                };
            }
            Self::DeletedKey { k, .. } => {
                return if splices.len() == 1
                    && splices[0].span == frontmatter_key_lines(doc, *k)
                    && splices[0].text.is_empty()
                {
                    Ok(())
                } else {
                    Err(invalid(
                        "delete_key requires an empty replacement of its entire field",
                    ))
                };
            }
            Self::AddKeyYaml { .. } | Self::SetKeyYaml { .. } => {
                let write = self.yaml_write(doc)?;
                return if let [one] = splices
                    && one.span == write.splice.span
                    && one.text == write.splice.text
                {
                    Ok(())
                } else {
                    Err(invalid(
                        "a raw YAML key write makes exactly the one splice its request derives",
                    ))
                };
            }
            Self::AppendKeyItem { .. } => {
                let write = self.append_write(doc)?;
                return if let [one] = splices
                    && one.span == write.splice.span
                    && one.text == write.splice.text
                {
                    Ok(())
                } else {
                    Err(invalid(
                        "append_key_item makes exactly the one splice its request derives",
                    ))
                };
            }
            Self::Fields { inner, .. } => return inner.validate(doc, splices),
            _ => true,
        };
        valid.then_some(()).ok_or_else(|| {
            invalid("intent has inconsistent source coordinates or candidate extents")
        })
    }

    fn target(&self, doc: &Document) -> Result<Target, Diagnostic> {
        let target = match self {
            Self::Item { list, item, .. } | Self::Checkbox { list, item, .. } => Target::Item {
                list: *list,
                item: *item,
            },
            Self::Title { s, .. }
            | Self::Body { s, .. }
            | Self::Paragraph { s, .. }
            | Self::InsertedSection { s, .. }
            | Self::Append { s, .. }
            | Self::Rejoin { s, .. } => Target::Section(*s),
            Self::Block { b, .. } => Target::Block(*b),
            Self::Frontmatter { k, .. }
            | Self::DeletedKey { k, .. }
            | Self::SetKeyYaml { k, .. } => Target::FrontmatterKey(*k),
            Self::NewKey { .. } | Self::AddKeyYaml { .. } => Target::Document,
            Self::AppendKeyItem { k, .. } => k.map_or(Target::Document, Target::FrontmatterKey),
            Self::InsertedItem { list, .. } | Self::Adoption { list, .. } => {
                let l = doc
                    .lists
                    .get(*list)
                    .ok_or_else(|| invalid("intent names a missing list"))?;
                Target::Block(l.block)
            }
            Self::Delete { target, .. } | Self::Stamp { scope: target, .. } => *target,
            Self::NewList { section } | Self::Archive { section, .. } => Target::Section(*section),
            Self::Move(m) => return m.target(doc),
            Self::Refresh(r) => return r.target(doc),
            Self::Fields { inner, .. } => return inner.target(doc),
        };
        target_exists(doc, target)?;
        Ok(target)
    }

    fn allowed(&self, doc: &Document) -> Result<Vec<Span>, Diagnostic> {
        self.target(doc)?;
        let point = |at| Span { start: at, end: at };
        Ok(match self {
            Self::Item { list, item, .. } => vec![doc.lists[*list].items[*item].span],
            Self::Checkbox { list, item, .. } => vec![
                doc.lists[*list].items[*item]
                    .checkbox
                    .as_ref()
                    .ok_or_else(|| invalid("checkbox intent has no checkbox"))?
                    .symbol,
            ],
            Self::Title { s, .. } => vec![
                doc.blocks[doc.sections[*s].block]
                    .heading
                    .as_ref()
                    .and_then(|h| h.title)
                    .ok_or_else(|| invalid("title has no editable span"))?,
            ],
            Self::Body { s, .. } => vec![doc.sections[*s].body],
            Self::Paragraph {
                b, paragraph_span, ..
            } => {
                target_exists(doc, Target::Block(*b))?;
                vec![*paragraph_span]
            }
            Self::Block { b, .. } => vec![doc.blocks[*b].span],
            Self::Frontmatter { k, field, .. } => {
                let parent = &doc.scalars()[*k];
                if let Some(field) = field {
                    let scalar =
                        yaml::mapping_field(&doc.source[parent.line.start..parent.line.end], field)
                            .ok_or_else(|| invalid("mapping field intent has no scalar span"))?;
                    vec![Span {
                        start: parent.line.start + scalar.span.start,
                        end: parent.line.start + scalar.span.end,
                    }]
                } else {
                    vec![parent.span]
                }
            }
            Self::DeletedKey { k, .. } => vec![frontmatter_key_lines(doc, *k)],
            Self::AddKeyYaml { .. } | Self::SetKeyYaml { .. } => {
                vec![self.yaml_write(doc)?.splice.span]
            }
            Self::AppendKeyItem { .. } => vec![self.append_write(doc)?.splice.span],
            Self::NewKey { .. } => vec![point(
                doc.frontmatter
                    .payload
                    .ok_or_else(|| invalid("frontmatter is absent"))?
                    .end,
            )],
            Self::InsertedItem { list, position, .. } => {
                let l = &doc.lists[*list];
                let at = match position {
                    Position::Start => l.items.first(),
                    Position::End => l.items.last(),
                }
                .ok_or_else(|| invalid("insert intent names an empty list"))?;
                vec![
                    point(match position {
                        Position::Start => at.span.start,
                        Position::End => at.span.end,
                    }),
                    point(doc.blocks[l.block].span.end),
                ]
            }
            Self::InsertedSection { s, at, .. } => {
                let sec = &doc.sections[*s];
                if *at != sec.own_end && *at != sec.whole.end {
                    return Err(invalid("section insertion is outside its parent boundary"));
                }
                vec![point(*at)]
            }
            Self::Append { s, .. } => vec![point(doc.sections[*s].own_end)],
            Self::Rejoin { s, list, .. } => {
                let l = doc
                    .lists
                    .get(*list)
                    .ok_or_else(|| invalid("rejoin intent names a missing list"))?;
                let span = doc.blocks[l.block].span;
                if !contains(doc.sections[*s].whole, span) {
                    return Err(invalid("rejoin list is outside its section"));
                }
                vec![span]
            }
            Self::Adoption {
                list, item, want, ..
            } => {
                target_exists(
                    doc,
                    Target::Item {
                        list: *list,
                        item: *item,
                    },
                )?;
                vec![
                    doc.markers
                        .iter()
                        .find(|m| m.lexical.id.as_deref() == Some(want))
                        .ok_or_else(|| invalid("adoption marker is absent"))?
                        .lexical
                        .span,
                ]
            }
            Self::Delete {
                target,
                span,
                collection,
                list,
            } => {
                if let Some(bound) = collection {
                    delete_collection(doc, bound, *target)?;
                }
                if *span != guarded_delete_span(doc, *target, list.as_ref())? {
                    return Err(invalid("deletion span differs from its source unit"));
                }
                vec![*span]
            }
            Self::Stamp {
                scope,
                item_scopes,
                checkbox_only,
            } => {
                let scope = doc.target_span(*scope).0;
                let mut points = vec![];
                for sec in &doc.sections {
                    let b = &doc.blocks[sec.block];
                    if !checkbox_only && scope.contains(sec.whole.start) {
                        points.push(point(b.span.end));
                    }
                }
                for list in &doc.lists {
                    let b = &doc.blocks[list.block];
                    if !scope.contains(b.span.start)
                        || item_scopes
                            .as_ref()
                            .is_some_and(|spans| !spans.iter().any(|s| s.contains(b.span.start)))
                    {
                        continue;
                    }
                    let eligible: Vec<_> = list
                        .items
                        .iter()
                        .filter(|it| !checkbox_only || it.checkbox.is_some())
                        .collect();
                    if !eligible.is_empty() {
                        points.push(point(b.span.end));
                        points.extend(eligible.iter().filter_map(|it| it.carrier.map(point)));
                    }
                }
                points
            }
            Self::NewList { section } => vec![point(doc.sections[*section].own_end)],
            Self::Archive {
                unit, section, at, ..
            } => {
                if matches!(unit, crate::archive::Unit::Section { .. }) {
                    let edge = doc.sections[*section].whole.end;
                    if *at != edge {
                        return Err(invalid("archive insertion is outside its section boundary"));
                    }
                    return Ok(vec![point(edge)]);
                }
                let Self::Archive { text, .. } = self else {
                    unreachable!()
                };
                let (_, edge) = super::archive_item_slot(doc, *section, text);
                if *at != edge {
                    return Err(invalid("archive insertion is outside its section boundary"));
                }
                vec![point(edge)]
            }
            Self::Move(m) => return m.allowed(doc),
            Self::Refresh(r) => return r.allowed(doc),
            Self::Fields { inner, .. } => return inner.allowed(doc),
        })
    }

    /// Source declarations are derived from the required operation shape.
    fn declare(&self, doc: &Document, change: &mut Change) -> Result<(), Diagnostic> {
        if let Self::Fields { inner, .. } = self {
            return inner.declare(doc, change);
        }
        change.blocks.clear();
        change.lists.clear();
        change.reshaped.clear();
        change.items.clear();
        change.regions.clear();
        change.frontmatter = false;
        change.carried.clear();
        change.dropped.clear();
        change.adopted.clear();
        change.imported.clear();
        let mut own = vec![];
        match self {
            Self::Item {
                list, item, region, ..
            }
            | Self::Checkbox {
                list, item, region, ..
            } => {
                change.lists.insert(*list);
                change.items.insert((*list, *item));
                change.regions.push(*region);
                if matches!(self, Self::Item { .. }) {
                    rewrite_prerequisite(doc, *list, *item)?;
                    own = doc.lists[*list].items[*item].markers.clone();
                }
            }
            Self::Title { s, .. } => {
                let b = doc.sections[*s].block;
                let span = doc.blocks[b]
                    .heading
                    .as_ref()
                    .and_then(|h| h.title)
                    .expect("editable title");
                preserve_foreign(
                    doc,
                    span,
                    &doc.blocks[b].markers,
                    "the title holds a marker owned by another unit; edit refused",
                    true,
                )?;
                change.blocks.insert(b);
            }
            Self::Body { s, allow_drop, .. } => {
                let span = doc.sections[*s].body;
                change.dropped = ids_in(doc, span);
                if !change.dropped.is_empty() && !allow_drop {
                    return Err(err(
                        ErrorCode::InvalidContent,
                        format!(
                            "the body holds stays {:?}; pass allow_drop to remove them",
                            change.dropped
                        ),
                    ));
                }
                change.blocks.extend(doc.blocks_in(span));
            }
            Self::Paragraph {
                b,
                paragraph_span,
                remove,
                ..
            } => {
                let block = &doc.blocks[*b];
                let region = if block.kind == "paragraph" {
                    Span {
                        start: block.span.start,
                        end: block.region_end,
                    }
                } else {
                    *paragraph_span
                };
                let mut own = block.markers.clone();
                if let Some(list) = block.list {
                    for item in &doc.lists[list].items {
                        if contains(item.span, *paragraph_span) {
                            own.extend(&item.markers);
                        }
                    }
                }
                preserve_foreign(
                    doc,
                    region,
                    &own,
                    "the paragraph holds a live stay Rivet does not own (including row subhash stays)",
                    false,
                )?;
                if *remove && live_markers_in(doc, region).next().is_some() {
                    return Err(err(
                        ErrorCode::InvalidContent,
                        "paragraph removal would orphan or re-home a stay",
                    ));
                }
                if !change.splices.is_empty() {
                    change.blocks.insert(*b);
                }
            }
            Self::Block { b, allow_drop, .. } => {
                let block = &doc.blocks[*b];
                let foreign = foreign_markers(doc, block.span, &block.markers);
                change.dropped = foreign
                    .iter()
                    .filter_map(|&m| doc.markers[m].lexical.id.clone())
                    .collect();
                if !foreign.is_empty() && !allow_drop {
                    return Err(err(
                        ErrorCode::InvalidContent,
                        format!(
                            "the block holds other stays {:?}; pass allow_drop to remove them",
                            change.dropped
                        ),
                    ));
                }
                own = block.markers.clone();
                change.blocks.insert(*b);
            }
            Self::Frontmatter { .. }
            | Self::NewKey { .. }
            | Self::DeletedKey { .. }
            | Self::AddKeyYaml { .. }
            | Self::SetKeyYaml { .. }
            | Self::AppendKeyItem { .. } => change.frontmatter = true,
            Self::InsertedItem { list, region, .. } => {
                change.reshaped.insert(*list);
                change.regions.push(*region);
            }
            Self::Rejoin { list, .. } => {
                change.blocks.insert(doc.lists[*list].block);
            }
            Self::Adoption {
                list, item, want, ..
            } => {
                let (_, l, i, m) = adoption_target(doc, want)?;
                if (l, i) != (*list, *item) {
                    return Err(invalid("adoption intent names a different item"));
                }
                let carriers = Carriers::new(doc);
                if let Some(blocker) =
                    adoption_blockers(&carriers, l, want, &doc.markers[m].lexical, &[])
                        .into_iter()
                        .next()
                {
                    return Err(blocker.diagnostic);
                }
                change.lists.insert(*list);
                change.adopted.push(want.clone());
                change.items.insert((*list, *item));
                change.regions.push(ItemRegion::Mapped {
                    list: *list,
                    span: doc.lists[*list].items[*item].span,
                    tail: 0,
                });
                own.extend(
                    doc.markers
                        .iter()
                        .position(|m| m.lexical.id.as_deref() == Some(want)),
                );
            }
            Self::Delete {
                target,
                span,
                list: list_guard,
                ..
            } => match target {
                Target::Item { list, item } => {
                    let it = &doc.lists[*list].items[*item];
                    preserve_foreign(
                        doc,
                        it.span,
                        &it.markers,
                        "the item holds a marker owned by another unit; delete refused",
                        true,
                    )?;
                    change.dropped = if list_guard.is_some() {
                        change.blocks.insert(doc.lists[*list].block);
                        ids_in(doc, *span)
                    } else {
                        doc.stay_ids(&it.markers)
                    };
                    change.reshaped.insert(*list);
                    change.items.insert((*list, *item));
                }
                Target::Block(b) => {
                    change.blocks.insert(*b);
                    change.dropped = ids_in(doc, *span);
                }
                Target::Section(_) => {
                    change.blocks.extend(doc.blocks_in(*span));
                    change.dropped = ids_in(doc, *span);
                }
                _ => return Err(invalid("unsupported deletion intent")),
            },
            Self::Stamp { .. } => {
                let mut placements: BTreeMap<usize, String> = BTreeMap::new();
                for (id, kind, start) in &change.minted {
                    if *kind == "section" {
                        let block = doc
                            .blocks
                            .iter()
                            .find(|b| b.span.start == *start && b.heading.is_some())
                            .ok_or_else(|| invalid("minted section has no source heading"))?;
                        placements
                            .entry(block.span.end)
                            .or_default()
                            .push_str(&format!(
                                "{}{}",
                                doc.newline,
                                markers::format_plain(id, "hash", PLACEHOLDER)
                            ));
                        continue;
                    }
                    if *kind == "list" {
                        let block = doc
                            .blocks
                            .iter()
                            .find(|b| b.span.start == *start && b.list.is_some())
                            .ok_or_else(|| invalid("minted list has no source list"))?;
                        placements
                            .entry(block.span.end)
                            .or_default()
                            .push_str(&format!(
                                "{}{}",
                                doc.newline,
                                markers::format_plain(id, "hash", PLACEHOLDER)
                            ));
                        continue;
                    }
                    if *kind != "item" {
                        return Err(invalid("stamp mint has an unsupported kind"));
                    }
                    let (li, ii) = doc
                        .lists
                        .iter()
                        .enumerate()
                        .find_map(|(li, l)| {
                            l.items
                                .iter()
                                .position(|i| i.span.start == *start)
                                .map(|ii| (li, ii))
                        })
                        .ok_or_else(|| invalid("minted item has no source item"))?;
                    let item = &doc.lists[li].items[ii];
                    let at = item
                        .carrier
                        .ok_or_else(|| invalid("minted item has no carrier"))?;
                    let marker = format!(" {}", markers::format_plain(id, "subhash", PLACEHOLDER));
                    let tail = if at == item.span.end { marker.len() } else { 0 };
                    placements.entry(at).or_default().push_str(&marker);
                    change.items.insert((li, ii));
                    change.lists.insert(li);
                    change.regions.push(ItemRegion::Mapped {
                        list: li,
                        span: item.span,
                        tail,
                    });
                }
                if change.splices.len() != placements.len()
                    || change.splices.iter().any(|s| {
                        !s.span.is_empty() || placements.get(&s.span.start) != Some(&s.text)
                    })
                {
                    return Err(invalid(
                        "stamp splice differs from its declared mint placements",
                    ));
                }
            }
            Self::Archive {
                unit, section, ids, ..
            } => {
                if let crate::archive::Unit::Section { markers, .. } = unit {
                    change.imported = markers.iter().map(|m| (m.id.clone(), m.kind)).collect();
                    return Ok(());
                }
                change.imported = ids.iter().cloned().map(|id| (id, "item")).collect();
                let Self::Archive { text, .. } = self else {
                    unreachable!()
                };
                if let (Some(list), _) = super::archive_item_slot(doc, *section, text) {
                    change.reshaped.insert(list);
                    let splice = change
                        .splices
                        .first()
                        .ok_or_else(|| invalid("archive intent has no splice"))?;
                    let offset = splice
                        .text
                        .find(text)
                        .ok_or_else(|| invalid("archive text is absent from its splice"))?;
                    change.regions.push(ItemRegion::Spliced {
                        list,
                        splice: splice.span,
                        offset,
                        len: text.len(),
                    });
                }
            }
            Self::Move(m) => return m.declare(doc, change),
            Self::Refresh(r) => return r.declare(doc, change),
            Self::InsertedSection { .. } | Self::Append { .. } | Self::NewList { .. } => {}
            Self::Fields { .. } => unreachable!(),
        }
        change.carried = own
            .into_iter()
            .filter(|&m| {
                change
                    .splices
                    .iter()
                    .any(|s| contains(s.span, doc.markers[m].lexical.span))
            })
            .filter_map(|m| doc.markers[m].lexical.id.clone())
            .collect();
        Ok(())
    }

    /// The raw YAML write's one splice, derived again from the preimage and
    /// the request, never taken from the planner's change.
    fn yaml_write(&self, doc: &Document) -> Result<YamlWrite, Diagnostic> {
        let (write, expect) = match self {
            Self::AddKeyYaml {
                key, raw, expect, ..
            } => (yaml_add(doc, key, raw)?, expect),
            Self::SetKeyYaml {
                k,
                key,
                raw,
                replace_line,
                expect,
                ..
            } => {
                target_exists(doc, Target::FrontmatterKey(*k))?;
                if doc.scalars()[*k].key != *key {
                    return Err(invalid("raw YAML set intent names another key"));
                }
                (yaml_set(doc, *k, raw, *replace_line)?, expect)
            }
            _ => return Err(invalid("not a raw YAML key write")),
        };
        if write.expect != *expect {
            return Err(invalid(
                "raw YAML intent's decoded value disagrees with its token",
            ));
        }
        Ok(write)
    }

    /// The append's one splice, derived again from the preimage and the
    /// request, never taken from the planner's change.
    fn append_write(&self, doc: &Document) -> Result<AppendWrite, Diagnostic> {
        let Self::AppendKeyItem {
            k,
            key,
            item,
            expect,
            ..
        } = self
        else {
            return Err(invalid("not an append_key_item"));
        };
        if let Some(k) = k {
            target_exists(doc, Target::FrontmatterKey(*k))?;
        }
        let write = yaml_append(doc, key, item)?;
        if write.k != *k || write.expect != *expect {
            return Err(invalid(
                "append_key_item intent disagrees with the sequence its request derives",
            ));
        }
        Ok(write)
    }

    /// The candidate's whole frontmatter is the preimage's with exactly the
    /// derived splice applied, the key reads back as its sequence with the
    /// item appended, and every other field keeps its value and source lines.
    fn check_append(self, doc: &Document, c: &Document) -> Result<(), Diagnostic> {
        let write = self.append_write(doc)?;
        let Self::AppendKeyItem { key, others, .. } = self else {
            unreachable!("checked by append_write");
        };
        spliced_frontmatter(doc, c, &write.splice, "append_key_item")?;
        let fields = c.frontmatter.fields.as_ref().map_err(Clone::clone)?;
        if fields.len() != others.len() + 1
            || fields
                .iter()
                .find(|s| s.key == key)
                .is_none_or(|s| s.value != write.expect)
        {
            return Err(err(
                ErrorCode::InvalidContent,
                "the appended frontmatter sequence did not read back",
            ));
        }
        preserve_frontmatter(c, &others, false)
    }

    /// The candidate's whole frontmatter is the preimage's with exactly the
    /// derived splice applied, and the key re-reads with the caller's token
    /// and value; every other field keeps its value and source lines.
    fn check_yaml(self, doc: &Document, c: &Document) -> Result<(), Diagnostic> {
        let write = self.yaml_write(doc)?;
        let (Self::AddKeyYaml { key, others, .. } | Self::SetKeyYaml { key, others, .. }) = self
        else {
            unreachable!("checked by yaml_write");
        };
        let fail = |m: &str| err(ErrorCode::InvalidContent, m);
        spliced_frontmatter(doc, c, &write.splice, "the raw YAML write")?;
        let fields = c.frontmatter.fields.as_ref().map_err(Clone::clone)?;
        let field = fields
            .iter()
            .find(|s| s.key == key)
            .ok_or_else(|| fail("the written frontmatter key did not read back"))?;
        let text = &c.source[field.line.start..field.line.end];
        let line = text.split('\n').next().unwrap_or(text);
        let line = line.strip_suffix('\r').unwrap_or(line);
        let read = yaml::key_line_token(line);
        if line != write.first_line
            || fields.len() != others.len() + 1
            || field.value != write.expect
            || read.is_none_or(|r| {
                r.value != write.expect
                    || r.token.start != write.token_start
                    || line[r.token.start..r.token.end] != write.token
            })
        {
            return Err(fail(
                "the written frontmatter key did not read back as its exact token",
            ));
        }
        preserve_frontmatter(c, &others, false)
    }

    pub(super) fn check(
        self,
        doc: &Document,
        c: &Document,
        map: &Mapper,
    ) -> Result<(), Diagnostic> {
        match self {
            yaml @ (Self::AddKeyYaml { .. } | Self::SetKeyYaml { .. }) => yaml.check_yaml(doc, c),
            append @ Self::AppendKeyItem { .. } => append.check_append(doc, c),
            Self::Checkbox {
                region,
                expect,
                ids,
                ..
            } => {
                let span = region.resolve(map);
                let (list, index) = c
                    .lists
                    .iter()
                    .enumerate()
                    .find_map(|(l, x)| x.items.iter().position(|i| i.span == span).map(|i| (l, i)))
                    .ok_or_else(|| {
                        err(
                            ErrorCode::InvalidContent,
                            "the ticked item does not re-read as one list item",
                        )
                    })?;
                let got = c.item_content(list, index);
                if blank_digests(&got) != blank_digests(&expect) {
                    return Err(err(
                        ErrorCode::InvalidContent,
                        format!("the tick changed more than its checkbox: got {got:?}"),
                    ));
                }
                if c.stay_ids(&c.lists[list].items[index].markers) != ids {
                    return Err(err(
                        ErrorCode::InvalidContent,
                        "the item's stay did not stay attached",
                    ));
                }
                Ok(())
            }
            Self::Title {
                start,
                level,
                text,
                ids,
                ..
            } => {
                let got = preimage_section(c, map, start, "title no longer parses as the heading")?;
                let h = c.blocks[got.1.block].heading.as_ref().expect("heading");
                if h.level != level || h.title_text != text || c.section_ids(got.0) != ids {
                    return Err(err(
                        ErrorCode::InvalidContent,
                        format!("title re-reads as {:?} at level {}", h.title_text, h.level),
                    ));
                }
                Ok(())
            }
            Self::Body {
                start,
                level,
                ids,
                region_span,
                len,
                ..
            } => {
                let (i, sec) = preimage_section(c, map, start, "the section heading moved")?;
                if sec.level != level || c.section_ids(i) != ids {
                    return Err(err(
                        ErrorCode::InvalidContent,
                        "the section heading changed",
                    ));
                }
                let new_start = map.splice_start(region_span);
                let inside = Span {
                    start: new_start,
                    end: new_start + len,
                };
                if c.sections
                    .iter()
                    .any(|x| inside.contains(c.blocks[x.block].span.start) && x.level <= level)
                {
                    return Err(err(
                        ErrorCode::InvalidContent,
                        "body content contains a heading at or above the section's level",
                    ));
                }
                Ok(())
            }
            Self::Paragraph {
                s,
                b,
                paragraph_span,
                remove,
                ..
            } => {
                let block = &doc.blocks[b];
                let map_span = |span: Span| Span {
                    start: map.map(span.start, true),
                    end: map.map(span.end, false),
                };
                preimage_section(
                    c,
                    map,
                    doc.sections[s].heading.start,
                    "the section heading moved",
                )?;
                if remove {
                    if !map_span(paragraph_span).is_empty() {
                        return Err(err(
                            ErrorCode::InvalidContent,
                            "paragraph removal must leave no replacement bytes",
                        ));
                    }
                    return Ok(());
                }
                // The containing block must keep its mapped extent and section.
                // Compare nested block kinds and parents: inline edits can change
                // a paragraph's leading whitespace without changing its shape.
                let current_span = map_span(block.span);
                if !c
                    .blocks
                    .iter()
                    .any(|p| p.span == current_span && p.kind == block.kind)
                    || c.section_of(current_span.start).is_none_or(|i| {
                        c.sections[i].heading.start != map.map(doc.sections[s].heading.start, true)
                    })
                {
                    return Err(err(
                        ErrorCode::InvalidContent,
                        "replacement must preserve exactly one paragraph and its containing block extent",
                    ));
                }
                let layout = |d: &Document, region: Span| {
                    let all = markdown::block_layout(d);
                    let indexes: Vec<usize> = all
                        .iter()
                        .enumerate()
                        .filter(|(_, p)| contains(region, p.span))
                        .map(|(i, _)| i)
                        .collect();
                    let positions: BTreeMap<usize, usize> = indexes
                        .iter()
                        .enumerate()
                        .map(|(position, &index)| (index, position))
                        .collect();
                    indexes
                        .iter()
                        .map(|&i| {
                            let p = &all[i];
                            (
                                p.kind,
                                p.parent.and_then(|parent| positions.get(&parent).copied()),
                            )
                        })
                        .collect::<Vec<_>>()
                };
                if layout(doc, block.span) != layout(c, current_span) {
                    return Err(err(
                        ErrorCode::InvalidContent,
                        "replacement must preserve exactly one paragraph and the container's block shape",
                    ));
                }
                // Block and item stays are checked by their source positions and
                // mapped owner extents, including markers on following marker-only lines.
                let region = Span {
                    start: block.span.start,
                    end: block.region_end,
                };
                let old: Vec<_> = live_markers_in(doc, region).collect();
                let new: Vec<_> = live_markers_in(c, map_span(region)).collect();
                if old.len() != new.len()
                    || old.iter().zip(new).any(|(old, new)| {
                        if map_span(old.lexical.span) != new.lexical.span {
                            return true;
                        }
                        match (old.attach, new.attach) {
                            (Attach::Block(a), Attach::Block(b)) => {
                                map_span(doc.blocks[a].span) != c.blocks[b].span
                            }
                            (
                                Attach::Item { list: a, item: x },
                                Attach::Item { list: b, item: y },
                            ) => map_span(doc.lists[a].items[x].span) != c.lists[b].items[y].span,
                            (Attach::Unaddressed, Attach::Unaddressed)
                            | (Attach::Orphan, Attach::Orphan)
                            | (Attach::Malformed, Attach::Malformed) => false,
                            _ => true,
                        }
                    })
                {
                    return Err(err(
                        ErrorCode::InvalidContent,
                        "the paragraph's stays would change attachment",
                    ));
                }
                Ok(())
            }
            Self::Block {
                replaced, len, ids, ..
            } => {
                let from = map.splice_start(replaced);
                let region = Span {
                    start: from,
                    end: from + len,
                };
                let inside: Vec<usize> = c.blocks_in(region).collect();
                match inside.as_slice() {
                    [one] if c.stay_ids(&c.blocks[*one].markers) == ids => Ok(()),
                    [_] => Err(err(
                        ErrorCode::InvalidContent,
                        "the block's stays did not stay attached",
                    )),
                    _ => Err(err(
                        ErrorCode::InvalidContent,
                        format!("content parses as {} blocks, not one", inside.len()),
                    )),
                }
            }
            Self::Frontmatter {
                key,
                expect,
                others,
                ..
            } => {
                let fields = c.frontmatter.fields.as_ref().map_err(Clone::clone)?;
                let got = fields.iter().find(|s| s.key == key);
                if got.map(|s| &s.value) != Some(&expect) {
                    return Err(err(
                        ErrorCode::InvalidContent,
                        "frontmatter value did not round-trip",
                    ));
                }
                preserve_frontmatter(c, &others, false)?;
                Ok(())
            }
            Self::DeletedKey { key, others, .. } => {
                if c.scalars().iter().any(|s| s.key == key) {
                    return Err(invalid("deleted frontmatter key survived"));
                }
                preserve_frontmatter(c, &others, true)?;
                Ok(())
            }
            Self::InsertedSection {
                at,
                lead,
                level,
                title,
                parent_start,
                parent_ids,
                ids,
                ..
            } => {
                let pos = map.map(at, false) + lead;
                let (i, sec) = section_at(c, pos, "inserted section did not parse in place")?;
                let parent = sec.parent.map(|p| c.blocks[c.sections[p].block].span.start);
                let h = c.blocks[sec.block].heading.as_ref().expect("heading");
                if sec.level != level
                    || h.title_text != title
                    || parent != Some(map.map(parent_start, true))
                    || c.section_ids(sec.parent.expect("parent")) != parent_ids
                    || c.section_ids(i) != ids
                {
                    return Err(err(
                        ErrorCode::InvalidContent,
                        "inserted section re-reads differently",
                    ));
                }
                Ok(())
            }
            Self::Append {
                at,
                lead,
                len,
                heading,
                level,
                kinds,
                ..
            } => {
                let start = map.map(at, false) + lead;
                let region = Span {
                    start,
                    end: start + len,
                };
                let home = map.map(heading, true);
                let found: Vec<usize> = c.blocks_in(region).collect();
                let shape: Vec<&'static str> = found.iter().map(|b| c.blocks[*b].kind).collect();
                if shape != kinds || found.iter().any(|b| c.blocks[*b].span.end > region.end) {
                    return Err(err(
                        ErrorCode::InvalidContent,
                        "the appended content does not stand as its own blocks in place",
                    ));
                }
                let owner = c.section_of(start).map(|i| &c.sections[i]);
                if owner.is_none_or(|o| {
                    c.blocks[o.block].span.start != home
                        || o.level != level
                        || o.own_end < region.end
                }) {
                    return Err(err(
                        ErrorCode::InvalidContent,
                        "the appended content does not land in the section's own body",
                    ));
                }
                Ok(())
            }
            Self::NewKey {
                key,
                expect,
                others,
            } => {
                let fields = c.frontmatter.fields.as_ref().map_err(Clone::clone)?;
                let got = fields.iter().find(|s| s.key == key);
                if got.is_none_or(|s| s.value != expect || !s.writable)
                    || fields.len() != others.len() + 1
                {
                    return Err(err(
                        ErrorCode::InvalidContent,
                        "the new frontmatter key did not read back as written",
                    ));
                }
                preserve_frontmatter(c, &others, false)?;
                Ok(())
            }
            Self::Rejoin {
                list_start,
                items,
                before,
                codes,
                ..
            } => {
                let at = map.map(list_start, true);
                let same_shape = c
                    .lists
                    .iter()
                    .find(|l| c.blocks[l.block].span.start == at)
                    .is_some_and(|l| l.items.len() == items);
                if !same_shape || markdown::page(c) != before || markdown::code_texts(c) != codes {
                    return Err(err(
                        ErrorCode::InvalidContent,
                        "rejoining the span would change more than where a line breaks",
                    ));
                }
                Ok(())
            }
            Self::Adoption {
                want, item_start, ..
            } => {
                let at = map.map(item_start, true);
                match c.ids.get(&want).map(Vec::as_slice) {
                    Some([Target::Item { list, item }])
                        if c.lists[*list].items[*item].span.start == at =>
                    {
                        Ok(())
                    }
                    _ => Err(err(
                        ErrorCode::InvalidContent,
                        format!("stay {want} would not attach to its item"),
                    )),
                }
            }
            Self::Item {
                region,
                expect,
                ids,
                ..
            }
            | Self::InsertedItem {
                region,
                expect,
                ids,
                ..
            } => {
                let span = region.resolve(map);
                let (list, index) = c
                    .lists
                    .iter()
                    .enumerate()
                    .find_map(|(l, x)| x.items.iter().position(|i| i.span == span).map(|i| (l, i)))
                    .ok_or_else(|| {
                        err(
                            ErrorCode::InvalidContent,
                            "content does not re-read as exactly one list item",
                        )
                    })?;
                let got = c.item_content(list, index);
                if got != expect {
                    return Err(err(
                        ErrorCode::InvalidContent,
                        format!("content does not re-read as written: got {got:?}"),
                    ));
                }
                if c.stay_ids(&c.lists[list].items[index].markers) != ids {
                    return Err(err(
                        ErrorCode::InvalidContent,
                        "the item's stay did not stay attached",
                    ));
                }
                Ok(())
            }
            Self::Delete { .. } | Self::Stamp { .. } => Ok(()),
            Self::NewList { section } => check_new_list(doc, c, map, section),
            Self::Archive {
                unit,
                section,
                text,
                ids,
                ..
            } => {
                let id = match &unit {
                    crate::archive::Unit::Item => &ids[0],
                    crate::archive::Unit::Section { id, .. } => id,
                };
                let copied = c.resolve(&Selector::Id { id: id.clone() })?;
                if let crate::archive::Unit::Section { markers, .. } = &unit {
                    let home = map.map(doc.sections[section].heading.start, true);
                    let Target::Section(s) = copied else {
                        return Err(invalid("copied section did not retain its attachment"));
                    };
                    crate::archive::section_matches(c, s, &text, &ids, markers)?;
                    if c.sections[s]
                        .parent
                        .is_none_or(|p| c.sections[p].heading.start != home)
                    {
                        return Err(err(
                            ErrorCode::ArchiveCopyRefused,
                            "copied section does not retain its direct parent",
                        ));
                    }
                    return Ok(());
                }
                let home = map.map(doc.sections[section].heading.start, true);
                if !matches!(copied, Target::Item { .. })
                    || c.target_span(copied).0.slice(&c.source) != Some(text.as_str())
                    || c.target_ids(copied) != ids
                    || c.section_of(c.target_span(copied).0.start)
                        .is_none_or(|s| c.sections[s].heading.start != home)
                {
                    return Err(err(
                        ErrorCode::ArchiveCopyRefused,
                        "copied bytes or attachment would change in the archive layout",
                    ));
                }
                Ok(())
            }
            Self::Move(m) => m.check(doc, c, map),
            Self::Refresh(r) => r.check(doc, c, map),
            Self::Fields { inner, fields } => {
                inner.check(doc, c, map)?;
                fields.check(c, map)
            }
        }
    }
}

pub(super) fn check_new_list(
    doc: &Document,
    c: &Document,
    map: &Mapper,
    section: usize,
) -> Result<(), Diagnostic> {
    let at = doc.sections[section].own_end;
    let region = Span {
        start: map.map(at, false),
        end: map.map(at, true),
    };
    let blocks: Vec<usize> = c.blocks_in(region).collect();
    let valid = match blocks.as_slice() {
        [b] => {
            c.blocks[*b]
                .list
                .is_some_and(|l| c.lists[l].items.len() == 1)
                && c.section_of(c.blocks[*b].span.start).is_some_and(|s| {
                    c.sections[s].heading.start
                        == map.map(doc.sections[section].heading.start, true)
                })
        }
        _ => false,
    };
    valid.then_some(()).ok_or_else(|| {
        err(
            ErrorCode::InvalidItem,
            "new list must read back as one item in its member section",
        )
    })
}

fn section_at<'a>(
    c: &'a Document,
    pos: usize,
    message: &str,
) -> Result<(usize, &'a crate::document::Section), Diagnostic> {
    c.sections
        .iter()
        .enumerate()
        .find(|(_, s)| c.blocks[s.block].span.start == pos)
        .ok_or_else(|| err(ErrorCode::InvalidContent, message))
}

fn preimage_section<'a>(
    c: &'a Document,
    map: &Mapper,
    start: usize,
    message: &str,
) -> Result<(usize, &'a crate::document::Section), Diagnostic> {
    section_at(c, map.map(start, true), message)
}

/// Everything up to the candidate's frontmatter end is the preimage's with
/// exactly `splice` applied.
fn spliced_frontmatter(
    doc: &Document,
    c: &Document,
    splice: &Splice,
    what: &str,
) -> Result<(), Diagnostic> {
    let fm = doc
        .frontmatter
        .span
        .ok_or_else(|| invalid("frontmatter is absent"))?;
    let span = splice.span;
    let want = format!(
        "{}{}{}",
        &doc.source[fm.start..span.start],
        splice.text,
        &doc.source[span.end..fm.end]
    );
    let same = c.frontmatter.span.is_some_and(|s| {
        c.source[s.start..s.end] == want && c.source[..s.start] == doc.source[..fm.start]
    });
    if !same {
        return Err(err(
            ErrorCode::InvalidContent,
            format!("the frontmatter would change beyond {what}'s own splice"),
        ));
    }
    Ok(())
}

fn preserve_frontmatter(
    c: &Document,
    others: &[(String, Value, String)],
    allow_comment_extension: bool,
) -> Result<(), Diagnostic> {
    let fields = c.frontmatter.fields.as_ref().map_err(Clone::clone)?;
    for (k, v, line) in others {
        let s = fields.iter().find(|s| &s.key == k).ok_or_else(|| {
            err(
                ErrorCode::InvalidContent,
                "another frontmatter key disappeared",
            )
        })?;
        let rendered = &c.source[s.line.start..s.line.end];
        // Deleting the next key can leave its trailing comments after this
        // collection. Only the parser's extent grows: all original field
        // bytes and its decoded value must still be identical.
        let same_lines = rendered == line
            || (allow_comment_extension
                && rendered.strip_prefix(line).is_some_and(|tail| {
                    tail.starts_with(['\n', '\r'])
                        && tail.lines().all(|line| {
                            let line = line.trim_start_matches([' ', '\t']);
                            line.is_empty() || line.starts_with('#')
                        })
                }));
        if &s.value != v || !same_lines {
            return Err(err(
                ErrorCode::InvalidContent,
                "another frontmatter key changed",
            ));
        }
    }
    Ok(())
}
