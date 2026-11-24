//! Raw item placement and its independent candidate expectation.
use super::*;
use crate::{MoveReason, schema};

pub(crate) struct MoveIntent {
    bound: Option<schema::Bound>,
    section: Option<SectionDestination>,
    under: Option<String>,
    text: String,
    ids: Vec<String>,
    new_list: Option<String>,
}

struct SectionDestination {
    parent: String,
    before: Option<String>,
    level: u8,
    title: String,
    markers: Vec<crate::archive::SectionMarker>,
}

struct Placement {
    section: usize,
    list: Option<usize>,
    splice: Splice,
    offset: usize,
}

impl MoveIntent {
    fn member(&self, doc: &Document) -> Result<(usize, Option<usize>), Diagnostic> {
        let absent = || {
            Diagnostic::new(
                ErrorCode::CollectionConflict,
                "destination has no requested collection member",
            )
        };
        let inst = schema::instance(doc, self.bound.as_ref().expect("item binding"))?
            .ok_or_else(absent)?;
        let section = match &self.under {
            None => inst.section,
            Some(title) => inst.named_member(doc, title)?.ok_or_else(absent)?,
        };
        Ok((section, inst.member_lists(doc, section).last().copied()))
    }

    fn placement(&self, doc: &Document) -> Result<Placement, Diagnostic> {
        if let Some(section) = &self.section {
            return section.placement(doc, &self.text, &self.ids);
        }
        let (section, list) = self.member(doc)?;
        if self.ids.is_empty() || crate::citations::carries_any(doc, &self.ids) {
            return Err(err(
                ErrorCode::InvalidRequest,
                "move import identities are empty or already present",
            ));
        }
        let (splice, offset) = match (list, self.new_list.as_deref()) {
            (Some(list), None) => {
                let (at, gap) = item_edge(doc, list, Position::End);
                let offset = gap.len();
                (
                    Splice {
                        span: Span { start: at, end: at },
                        text: format!("{gap}{}", self.text),
                    },
                    offset,
                )
            }
            (None, Some(id))
                if !self.ids.iter().any(|x| x == id)
                    && !crate::citations::identity_claims(doc).any(|(x, _)| x == id) =>
            {
                new_list_frame(doc, section, &self.text, id)
            }
            _ => {
                return Err(err(
                    ErrorCode::InvalidRequest,
                    "move list mint does not match destination placement",
                ));
            }
        };
        Ok(Placement {
            section,
            list,
            splice,
            offset,
        })
    }

    pub(super) fn target(&self, doc: &Document) -> Result<Target, Diagnostic> {
        Ok(Target::Section(self.placement(doc)?.section))
    }

    pub(super) fn allowed(&self, doc: &Document) -> Result<Vec<Span>, Diagnostic> {
        Ok(vec![self.placement(doc)?.splice.span])
    }

    pub(super) fn validate(&self, doc: &Document, splices: &[Splice]) -> Result<(), Diagnostic> {
        let p = self.placement(doc)?;
        if splices.len() != 1
            || splices[0].span != p.splice.span
            || splices[0].text != p.splice.text
        {
            return Err(err(
                ErrorCode::InvalidRequest,
                "move splice differs from its computed placement",
            ));
        }
        Ok(())
    }

    pub(super) fn declare(&self, doc: &Document, change: &mut Change) -> Result<(), Diagnostic> {
        let p = self.placement(doc)?;
        change.imported = match &self.section {
            Some(section) => section
                .markers
                .iter()
                .map(|m| (m.id.clone(), m.kind))
                .collect(),
            None => self.ids.iter().cloned().map(|id| (id, "item")).collect(),
        };
        change.minted = self
            .new_list
            .iter()
            .cloned()
            .map(|id| (id, "list", p.splice.span.start))
            .collect();
        if let Some(list) = p.list {
            change.reshaped.insert(list);
            change.regions.push(ItemRegion::Spliced {
                list,
                splice: p.splice.span,
                offset: p.offset,
                len: self.text.len(),
            });
        }
        Ok(())
    }

    pub(super) fn check(
        &self,
        doc: &Document,
        c: &Document,
        map: &Mapper,
    ) -> Result<(), Diagnostic> {
        let p = self.placement(doc)?;
        if let Some(section) = &self.section {
            let target = c.resolve(&Selector::Id {
                id: self.ids[0].clone(),
            })?;
            let Target::Section(s) = target else {
                return Err(crate::relocate::refused(
                    MoveReason::UnitMatches,
                    "moved stay no longer owns a section",
                ));
            };
            crate::archive::section_matches(c, s, &self.text, &self.ids, &section.markers)
                .map_err(|e| crate::relocate::refused(MoveReason::UnitMatches, e.message))?;
            if c.sections[s].level != section.level
                || c.sections[s].parent.is_none_or(|parent| {
                    c.sections[parent].heading.start
                        != map.map(doc.sections[p.section].heading.start, true)
                })
            {
                return Err(crate::relocate::refused(
                    MoveReason::UnitMatches,
                    "moved section lost its level or destination parent",
                ));
            }
            return Ok(());
        }
        if p.list.is_none() {
            super::intent::check_new_list(doc, c, map, p.section)?;
        }
        let target = c.resolve(&Selector::Id {
            id: self.ids[0].clone(),
        })?;
        let Target::Item { list, .. } = target else {
            return Err(crate::relocate::refused(
                MoveReason::UnitMatches,
                "moved stay no longer owns an item",
            ));
        };
        let valid_list = match p.list {
            Some(old) => {
                c.blocks[c.lists[list].block].span.start
                    == map.map(doc.blocks[doc.lists[old].block].span.start, true)
            }
            None => self.new_list.as_ref().is_some_and(|id| {
                c.stay_ids(&c.blocks[c.lists[list].block].markers)
                    .contains(id)
            }),
        };
        if !valid_list
            || c.target_span(target).0.slice(&c.source) != Some(self.text.as_str())
            || c.target_ids(target) != self.ids
            || c.section_of(c.target_span(target).0.start).is_none_or(|s| {
                c.sections[s].heading.start != map.map(doc.sections[p.section].heading.start, true)
            })
        {
            return Err(crate::relocate::refused(
                MoveReason::UnitMatches,
                "moved bytes or marker owners differ in the destination",
            ));
        }
        Ok(())
    }
}

pub(crate) fn plan(
    source: &Document,
    target: Target,
    destination: &Document,
    bound: &schema::Bound,
    under: Option<&str>,
) -> Result<(Outcome, Option<String>), Diagnostic> {
    if !matches!(target, Target::Item { .. }) {
        return Err(err(
            ErrorCode::InvalidRequest,
            "move destination requires an item",
        ));
    }
    crate::native::markers(source, target)
        .map_err(|e| crate::relocate::refused(MoveReason::NotNative, e.message))?;
    let ids = source.target_ids(target);
    let text = source
        .target_span(target)
        .0
        .slice(&source.source)
        .expect("source item")
        .to_string();
    let mut intent = MoveIntent {
        bound: Some(bound.clone()),
        section: None,
        under: under.map(str::to_string),
        text,
        ids,
        new_list: None,
    };
    let mut planner = Planner::new(destination);
    planner
        .taken
        .extend(crate::citations::identity_claims(source).map(|(id, _)| id.to_string()));
    if intent.member(destination)?.1.is_none() {
        intent.new_list = Some(planner.mint());
    }
    let p = intent.placement(destination)?;
    let guard = Guard::new(
        GuardSubject::Native(Target::Section(p.section)),
        &destination.target_revision(Target::Section(p.section)),
    );
    let new_list = intent.new_list.clone();
    let change = Change {
        splices: vec![p.splice],
        guard: Some(guard),
        expectation: Some(Expectation::Move(Box::new(intent))),
        ..Change::default()
    };
    let outcome = super::plan(destination, vec![change]).map_err(|e| {
        if matches!(
            e.code,
            ErrorCode::CollectionConflict
                | ErrorCode::AmbiguousSelector
                | ErrorCode::GuardConflict
                | ErrorCode::LimitExceeded
                | ErrorCode::MoveRefused
        ) {
            e
        } else {
            crate::relocate::refused(MoveReason::ListShape, e.message)
        }
    })?;
    Ok((outcome, new_list))
}

impl SectionDestination {
    fn placement(
        &self,
        doc: &Document,
        text: &str,
        ids: &[String],
    ) -> Result<Placement, Diagnostic> {
        let refuse = crate::relocate::refused;
        let parent = doc.resolve(&Selector::Id {
            id: self.parent.clone(),
        })?;
        let Target::Section(parent) = parent else {
            return Err(refuse(
                MoveReason::NotNative,
                "destination parent must be a stamped section",
            ));
        };
        if !crate::native::heading_stay(doc, parent, &self.parent) {
            return Err(refuse(
                MoveReason::NotNative,
                "destination parent must own its heading stay",
            ));
        }
        if doc.sections[parent].level + 1 != self.level {
            return Err(refuse(
                MoveReason::ParentLevel,
                "destination parent must be exactly one heading level above the unit",
            ));
        }
        if doc
            .sections
            .iter()
            .any(|s| s.parent == Some(parent) && s.path.last() == Some(&self.title))
        {
            return Err(refuse(
                MoveReason::SiblingTitle,
                "destination already has a direct child with the unit title",
            ));
        }
        if ids.is_empty() || crate::citations::carries_any(doc, ids) {
            return Err(refuse(
                MoveReason::DestinationCarriesId,
                "destination already carries a unit identity",
            ));
        }
        let at = if let Some(id) = &self.before {
            let target = doc
                .resolve(&Selector::Id { id: id.clone() })
                .map_err(|e| err(ErrorCode::InvalidRequest, e.message))?;
            let Target::Section(child) = target else {
                return Err(err(
                    ErrorCode::InvalidRequest,
                    "before must name a direct child section",
                ));
            };
            if doc.sections[child].parent != Some(parent)
                || !crate::native::heading_stay(doc, child, id)
            {
                return Err(err(
                    ErrorCode::InvalidRequest,
                    "before must name a direct child of destination_parent",
                ));
            }
            doc.sections[child].heading.start
        } else {
            doc.sections[parent].whole.end
        };
        let prefix = &doc.source[..at];
        let nl = doc.newline;
        let mut inserted = String::new();
        if !prefix.ends_with('\n') {
            inserted.push_str(nl);
        }
        if !prefix.ends_with(&format!("{nl}{nl}")) {
            inserted.push_str(nl);
        }
        let offset = inserted.len();
        inserted.push_str(text);
        inserted.push_str(nl);
        Ok(Placement {
            section: parent,
            list: None,
            offset,
            splice: Splice {
                span: Span { start: at, end: at },
                text: inserted,
            },
        })
    }
}

pub(crate) fn plan_section(
    source: &Document,
    section: usize,
    destination: &Document,
    parent: &str,
    before: Option<&str>,
) -> Result<Outcome, Diagnostic> {
    let source_section = &source.sections[section];
    let intent = MoveIntent {
        bound: None,
        under: None,
        new_list: None,
        text: source
            .section_content_span(section)
            .map_err(|e| crate::relocate::refused(MoveReason::NotNative, e.message))?
            .slice(&source.source)
            .expect("section content")
            .to_string(),
        ids: ids_in(source, source_section.whole),
        section: Some(SectionDestination {
            parent: parent.into(),
            before: before.map(str::to_string),
            level: source_section.level,
            title: source_section.path.last().expect("section title").clone(),
            markers: crate::archive::section_markers(source, section)
                .map_err(|e| crate::relocate::refused(MoveReason::NotNative, e.message))?,
        }),
    };
    let p = intent.placement(destination)?;
    let guard = Guard::new(
        GuardSubject::Native(Target::Section(p.section)),
        &destination.target_revision(Target::Section(p.section)),
    );
    super::plan(
        destination,
        vec![Change {
            splices: vec![p.splice],
            guard: Some(guard),
            expectation: Some(Expectation::Move(Box::new(intent))),
            ..Change::default()
        }],
    )
}
