//! Stay-id citations in a document's text, resolved against every stay id the
//! project's collections and their archive files carry. Read-only.
//!
//! `item:<id>` and `section:<id>` are the checked forms. A bare id in prose is
//! not checked, since an eight-character token is as often a short sha or a
//! word, but a write that removes a stay still searches for it. A value
//! followed by `/` or `#` belongs to another scheme (a Loom note anchor, a
//! repository's pull request) and is no citation. Stay markers, frontmatter
//! and top-level code blocks are not text.
use crate::contract::Span;
use crate::document::{Attach, Document, Target};
use crate::fs::{Project, WalkEvent, WalkFaults};
use crate::schema::{self, ProjectConfig};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Form {
    Item,
    Section,
}

impl Form {
    fn prefix(self) -> &'static str {
        match self {
            Form::Item => "item:",
            Form::Section => "section:",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Citation {
    pub form: Form,
    pub id: String,
    pub span: Span,
}

fn id_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_')
}

/// A value that runs on into `/` or `#` is another scheme's.
fn foreign(src: &[u8], end: usize) -> bool {
    matches!(src.get(end), Some(b'/' | b'#'))
}

/// The body's text: after the frontmatter, outside stay markers and
/// top-level code blocks. A malformed marker is text, as `find` reads it.
fn text(doc: &Document) -> Vec<Span> {
    let mut cut: Vec<Span> = doc
        .markers
        .iter()
        .filter(|m| m.attach != Attach::Malformed)
        .map(|m| m.lexical.span)
        .chain(
            doc.blocks
                .iter()
                .filter(|b| b.kind == "code_block")
                .map(|b| b.span),
        )
        .collect();
    cut.sort_by_key(|s| s.start);
    let mut spans = vec![];
    let mut pos = doc.body_start;
    for s in cut {
        if s.start > pos {
            spans.push(Span {
                start: pos,
                end: s.start,
            });
        }
        pos = pos.max(s.end);
    }
    if pos < doc.source.len() {
        spans.push(Span {
            start: pos,
            end: doc.source.len(),
        });
    }
    spans
}

/// Every `item:<id>` and `section:<id>` in the document's text, in order.
pub fn citations(doc: &Document) -> Vec<Citation> {
    let src = doc.source.as_bytes();
    let mut found = vec![];
    for span in text(doc) {
        let text = &doc.source[span.start..span.end];
        for form in [Form::Item, Form::Section] {
            let prefix = form.prefix();
            for (i, _) in text.match_indices(prefix) {
                let at = span.start + i;
                if i > 0 && id_byte(src[at - 1]) {
                    continue;
                }
                let from = at + prefix.len();
                let len = src[from..span.end]
                    .iter()
                    .take_while(|b| id_byte(**b))
                    .count();
                if len == 0 || foreign(src, from + len) {
                    continue;
                }
                found.push(Citation {
                    form,
                    id: doc.source[from..from + len].to_string(),
                    span: Span {
                        start: at,
                        end: from + len,
                    },
                });
            }
        }
    }
    found.sort_by_key(|c| c.span.start);
    found
}

/// Byte offsets of every whole-token mention of one of `ids` in the text,
/// bare or prefixed, with the id mentioned. A token that runs on into `/` or
/// `#` is another scheme's, as for a citation.
pub fn mentions(doc: &Document, ids: &BTreeSet<String>) -> Vec<(String, usize)> {
    let src = doc.source.as_bytes();
    let mut found = vec![];
    for span in text(doc) {
        let mut i = span.start;
        while i < span.end {
            if !id_byte(src[i]) {
                i += 1;
                continue;
            }
            let len = src[i..span.end].iter().take_while(|b| id_byte(**b)).count();
            let token = &doc.source[i..i + len];
            if (i == span.start || !id_byte(src[i - 1]))
                && !foreign(src, i + len)
                && ids.contains(token)
            {
                found.push((token.to_string(), i));
            }
            i += len;
        }
    }
    found
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Item,
    Section,
    List,
    Block,
    /// An orphan or unaddressed stay: it names no unit but still collides.
    Loose,
}

#[derive(Debug, Clone)]
pub(crate) struct Carrier {
    pub path: String,
    pub line: usize,
    kind: Kind,
    pub archived: bool,
    pub checked: Option<bool>,
    pub item: Option<(usize, usize)>,
}

/// What a citation found that it should not have.
pub struct Problem {
    /// `citation_unresolved`, `citation_ambiguous` or `citation_kind`.
    pub code: &'static str,
    pub message: String,
}

/// Every stay id the scanned files carry, by id.
#[derive(Default)]
pub struct IdMap {
    carriers: BTreeMap<String, Vec<Carrier>>,
    /// Ids a heading's `{#id}` and stay disagree over, with each file.
    conflicted: BTreeMap<String, Vec<String>>,
}

impl IdMap {
    /// Register a file's stay ids; an archive file's copy of an id gives way
    /// to a live one.
    pub fn add(&mut self, path: &str, doc: &Document, archived: bool) {
        for id in &doc.conflicted {
            self.conflicted
                .entry(id.clone())
                .or_default()
                .push(path.to_string());
        }
        for (id, targets) in &doc.ids {
            let mut loose = doc
                .markers
                .iter()
                .filter(|m| {
                    m.lexical.id.as_ref() == Some(id)
                        && matches!(m.attach, Attach::Orphan | Attach::Unaddressed)
                })
                .map(|m| m.lexical.span.start);
            for &target in targets {
                let (pos, kind) = match target {
                    Target::Item { list, item } => {
                        (doc.lists[list].items[item].span.start, Kind::Item)
                    }
                    Target::Section(s) => {
                        (doc.blocks[doc.sections[s].block].span.start, Kind::Section)
                    }
                    Target::Block(b) if doc.blocks[b].list.is_some() => {
                        (doc.blocks[b].span.start, Kind::List)
                    }
                    Target::Block(b) => (doc.blocks[b].span.start, Kind::Block),
                    Target::Document => (loose.next().unwrap_or(doc.body_start), Kind::Loose),
                    Target::FrontmatterKey(_) => continue,
                };
                self.carriers.entry(id.clone()).or_default().push(Carrier {
                    path: path.to_string(),
                    line: doc.file_line(pos),
                    kind,
                    archived,
                    checked: match target {
                        Target::Item { list, item } => doc.lists[list].items[item]
                            .checkbox
                            .as_ref()
                            .map(|c| c.checked),
                        _ => None,
                    },
                    item: match target {
                        Target::Item { list, item } => Some((list, item)),
                        _ => None,
                    },
                });
            }
        }
    }

    /// Every id whose live (non-archive) carriers span two or more files,
    /// with each live carrier's path and line, in id order.
    pub fn live_in_several_files(&self) -> Vec<(String, Vec<(String, usize)>)> {
        self.carriers
            .iter()
            .filter_map(|(id, all)| {
                let live: Vec<_> = all
                    .iter()
                    .filter(|c| !c.archived)
                    .map(|c| (c.path.clone(), c.line))
                    .collect();
                let files: BTreeSet<&str> = live.iter().map(|(p, _)| p.as_str()).collect();
                (files.len() > 1).then(|| (id.clone(), live))
            })
            .collect()
    }

    pub fn carries(&self, id: &str) -> bool {
        self.carriers.contains_key(id) || self.conflicted.contains_key(id)
    }

    /// Select a unique carrier, preferring live carriers before checking its kind.
    pub(crate) fn resolve(&self, id: &str, prefix: &str) -> Result<&Carrier, Problem> {
        let problem = |code, message| Err(Problem { code, message });
        if let Some(paths) = self.conflicted.get(id) {
            return problem(
                "citation_ambiguous",
                format!(
                    "{prefix}{id} names an id a heading claims beside a different stay, in {}",
                    paths.join(", ")
                ),
            );
        }
        let all = self.carriers.get(id).map(Vec::as_slice).unwrap_or(&[]);
        let live: Vec<&Carrier> = all.iter().filter(|c| !c.archived).collect();
        let carriers = if live.is_empty() {
            all.iter().collect()
        } else {
            live
        };
        let carrier = match carriers.as_slice() {
            [] => {
                return problem(
                    "citation_unresolved",
                    format!(
                        "{prefix}{id} names a stay id no collection or archive file in this project carries; a citation into another project cannot be checked from here"
                    ),
                );
            }
            [one] => *one,
            many => {
                let at: Vec<String> = many
                    .iter()
                    .map(|c| format!("{}:{}", c.path, c.line))
                    .collect();
                return problem(
                    "citation_ambiguous",
                    format!(
                        "{prefix}{id} names a stay id carried more than once: {}",
                        at.join(", ")
                    ),
                );
            }
        };
        Ok(carrier)
    }

    /// Why a citation does not name exactly one unit of its kind.
    pub fn check(&self, citation: &Citation) -> Option<Problem> {
        let (id, prefix) = (&citation.id, citation.form.prefix());
        let problem = |code, message| Some(Problem { code, message });
        let carrier = match self.resolve(id, prefix) {
            Ok(carrier) => carrier,
            Err(problem) => return Some(problem),
        };
        let wanted = match citation.form {
            Form::Item => Kind::Item,
            Form::Section => Kind::Section,
        };
        if carrier.kind == wanted {
            return None;
        }
        let kind = match carrier.kind {
            Kind::Item => "an item",
            Kind::Section => "a section",
            Kind::List => "a list",
            Kind::Block => "a block",
            Kind::Loose => "a stay that addresses no unit",
        };
        problem(
            "citation_kind",
            format!(
                "{prefix}{id} names {}:{}, which is {kind}",
                carrier.path, carrier.line
            ),
        )
    }
}

/// Check physical identity claims, including orphaned and conflicting legacy stays.
pub(crate) fn identity_claims(doc: &Document) -> impl Iterator<Item = (&str, Span)> {
    doc.markers
        .iter()
        .filter(|m| m.attach != Attach::Malformed)
        .filter_map(|m| m.lexical.id.as_deref().map(|id| (id, m.lexical.span)))
        .chain(doc.blocks.iter().filter_map(|b| {
            b.heading
                .as_ref()
                .and_then(|h| h.legacy_id.as_deref())
                .map(|id| (id, b.span))
        }))
}

/// Check physical identity claims, including orphaned and conflicting legacy stays.
/// Importers must not mistake an ambiguous carrier for an absent id.
pub(crate) fn carries_any(doc: &Document, ids: &[String]) -> bool {
    let mut map = IdMap::default();
    map.add("", doc, false);
    ids.iter().any(|id| map.carries(id))
}

/// The files an id map covers, each with whether it is an archive file
/// (collection files first; a file a collection glob matches is never an
/// archive file), found by `doctor`'s walker over every collection and
/// archive glob. Also every raw traversal event for that union and literal-binding
/// configuration diagnostics, kept separate from enumeration observations.
pub fn scope(
    project: &Project,
    config: &ProjectConfig,
    faults: &WalkFaults,
) -> (Vec<(String, bool)>, Vec<WalkEvent>, Vec<crate::Diagnostic>) {
    let mut diagnostics: Vec<_> = config
        .collections
        .iter()
        .filter_map(|(name, c)| schema::validate_binding_paths(name, c).err())
        .collect();
    diagnostics.extend(config.validate_section_archives().err());
    let live: BTreeSet<_> = config.live_patterns().into_iter().collect();
    let archives: BTreeSet<_> = config.archive_patterns().into_iter().collect();
    let all: Vec<String> = live.union(&archives).cloned().collect();
    let (found, events) = project.observe_with(&all, None, faults);
    // A found file is live when some binding selects it, archived when an
    // archive glob matches it, and otherwise (a file only `exclude_files`
    // keeps out) not a carrier at all.
    let archive_patterns: Vec<_> = archives.into_iter().collect();
    let (live, archive): (Vec<String>, Vec<String>) = found
        .into_iter()
        .filter(|p| config.live_member(p) || schema::matches_file_patterns(&archive_patterns, p))
        .partition(|p| config.live_member(p));
    let mut files: Vec<(String, bool)> = live.into_iter().map(|p| (p, false)).collect();
    files.extend(archive.into_iter().map(|p| (p, true)));
    (files, events, diagnostics)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(source: &str) -> Vec<(Form, String)> {
        let doc = Document::parse(source).unwrap();
        citations(&doc)
            .into_iter()
            .map(|c| (c.form, c.id))
            .collect()
    }

    #[test]
    fn prefixed_ids_are_citations_and_other_schemes_are_not() {
        assert_eq!(
            ids("See item:Ab3_x-9Z, `section:SecSecSe` and **Waits**: item:WaitWait.\n"),
            [
                (Form::Item, "Ab3_x-9Z".into()),
                (Form::Section, "SecSecSe".into()),
                (Form::Item, "WaitWait".into())
            ]
        );
        // Another scheme's value, a word ending in the prefix, an empty value.
        assert!(
            ids("item:cd89cbcf/2/b6ce item:connect-test#162 subitem:Xx the `item:` form\n")
                .is_empty()
        );
    }

    #[test]
    fn markers_frontmatter_and_code_blocks_are_not_text() {
        let source = "---\nnote: item:FrontFro\n---\n\n## A\n<!-- stay:item0000 -->\n\n```\nitem:CodeCode\n```\n\n- [ ] one item:RealReal\n";
        assert_eq!(ids(source), [(Form::Item, "RealReal".into())]);
        // Only top-level code blocks are cut: one inside a list item is read.
        let nested = "- [ ] one\n\n  ```\n  item:NestedX\n  ```\n";
        assert_eq!(ids(nested), [(Form::Item, "NestedX".into())]);
    }

    #[test]
    fn mentions_are_whole_tokens_bare_or_prefixed() {
        let doc = Document::parse(
            "Gone: GoneGone, `GoneGone`, item:GoneGone; not GoneGoneX or x-GoneGone.\n",
        )
        .unwrap();
        let want: BTreeSet<String> = ["GoneGone".to_string()].into();
        assert_eq!(mentions(&doc, &want).len(), 3);
        // Another scheme's value is not a mention, bare or prefixed.
        let doc = Document::parse("item:GoneGone/2/note, item:GoneGone#162, GoneGone/x\n").unwrap();
        assert!(mentions(&doc, &want).is_empty());
    }

    fn check(source: &str, citation: &str) -> Option<&'static str> {
        let doc = Document::parse(source).unwrap();
        let mut map = IdMap::default();
        map.add("a.md", &doc, false);
        let cited = Document::parse(citation).unwrap();
        map.check(&citations(&cited)[0]).map(|p| p.code)
    }

    /// An orphan or unaddressed stay names no unit, but its id still collides
    /// with a real one, as a selector reads it.
    #[test]
    fn a_stay_that_addresses_nothing_still_carries_its_id() {
        let sub = "subhash=sha256:111111111111";
        let source = format!(
            "## A\n\n- [ ] one <!-- stay:SameSame {sub} -->\n  - nested <!-- stay:SameSame {sub} -->\n- [ ] two <!-- stay:TwoTwoTw {sub} -->\n  - nested <!-- stay:LooseLoo {sub} -->\n"
        );
        assert_eq!(check(&source, "item:TwoTwoTw\n"), None);
        assert_eq!(
            check(&source, "item:SameSame\n"),
            Some("citation_ambiguous")
        );
        assert_eq!(check(&source, "item:LooseLoo\n"), Some("citation_kind"));
    }

    /// A heading whose `{#id}` and stay disagree resolves under neither id.
    #[test]
    fn a_heading_with_disagreeing_ids_resolves_under_neither() {
        let source = "## A {#LegacyId}\n<!-- stay:StayStay -->\n\nbody\n";
        assert_eq!(
            check(source, "section:LegacyId\n"),
            Some("citation_ambiguous")
        );
        assert_eq!(
            check(source, "section:StayStay\n"),
            Some("citation_ambiguous")
        );
    }
}
