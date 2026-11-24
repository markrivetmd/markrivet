//! Literal text search over one document's body, answering with the unit
//! that holds each match. Matching runs on the source, not the rendered page:
//! Markdown syntax is literal text, a run of whitespace (a soft line break and
//! the indentation after it included) matches one space, a blank line ends
//! what a match can span, and stay markers are not text.
use crate::document::{Attach, Document, Target};
use crate::{Diagnostic, ErrorCode, Selector};

/// Characters of excerpt context kept on each side of a match.
const CONTEXT: usize = 40;

/// Matches in one unit: the innermost one a stay id resolves to, or else the
/// innermost section (addressed by heading path) or the document.
pub struct Hit {
    pub target: Target,
    pub count: usize,
    /// Byte offset of the first match.
    pub at: usize,
    pub excerpt: String,
    /// Why the unit's heading path does not select it, when it does not.
    pub unresolved: Option<Diagnostic>,
}

/// The searchable text. `chars` holds one entry per kept source character
/// with its byte offset in `at`; a whitespace run is one space, a blank line
/// `\n`. `folded` is what matching compares, each entry naming the `chars`
/// index it came from, since a case fold can expand one character to several.
struct Hay {
    chars: Vec<char>,
    at: Vec<usize>,
    folded: Vec<char>,
    from: Vec<usize>,
}

/// Unicode lowercase, expansions included, with final sigma read as sigma.
fn fold(c: char, match_case: bool, out: &mut impl FnMut(char)) {
    if match_case {
        return out(c);
    }
    for l in c.to_lowercase() {
        out(if l == 'ς' { 'σ' } else { l });
    }
}

/// The needle as the haystack spells it: trimmed, whitespace runs collapsed.
pub fn needle(text: &str, match_case: bool) -> Result<Vec<char>, Diagnostic> {
    let words: Vec<&str> = text.split_whitespace().collect();
    if words.is_empty() {
        return Err(Diagnostic::new(
            ErrorCode::InvalidRequest,
            "find needs non-blank text",
        ));
    }
    let mut folded = vec![];
    for c in words.join(" ").chars() {
        fold(c, match_case, &mut |f| folded.push(f));
    }
    Ok(folded)
}

impl Hay {
    fn push(&mut self, c: char, at: usize, match_case: bool) {
        let k = self.chars.len();
        self.chars.push(c);
        self.at.push(at);
        let (folded, from) = (&mut self.folded, &mut self.from);
        fold(c, match_case, &mut |f| {
            folded.push(f);
            from.push(k);
        });
    }
}

fn haystack(doc: &Document, match_case: bool) -> Hay {
    let mut hay = Hay {
        chars: vec![],
        at: vec![],
        folded: vec![],
        from: vec![],
    };
    let mut markers: Vec<_> = doc
        .markers
        .iter()
        .filter(|m| m.attach != Attach::Malformed)
        .map(|m| m.lexical.span)
        .collect();
    markers.sort_by_key(|s| s.start);
    let mut markers = markers.into_iter().peekable();
    // A pending whitespace run: where it started, whether it holds a blank
    // line, and whether the current line has been blank since its newline.
    // A marker is not text and not whitespace: it neither starts a run nor
    // leaves its line blank.
    let mut run: Option<usize> = None;
    let mut blank = false;
    let mut line_blank = false;
    let mut pos = doc.body_start;
    let source = &doc.source;
    while pos < source.len() {
        if let Some(m) = markers.next_if(|m| m.start <= pos) {
            if m.end > pos {
                line_blank = false;
                pos = m.end;
            }
            continue;
        }
        let c = source[pos..].chars().next().expect("in bounds");
        if c.is_whitespace() {
            run.get_or_insert(pos);
            if c == '\n' {
                blank |= line_blank;
                line_blank = true;
            }
        } else {
            if let Some(start) = run.take() {
                // Leading whitespace in the body is not a separator.
                if !hay.chars.is_empty() {
                    hay.push(if blank { '\n' } else { ' ' }, start, true);
                }
                blank = false;
            }
            line_blank = false;
            hay.push(c, pos, match_case);
        }
        pos += c.len_utf8();
    }
    hay
}

/// Every non-overlapping match of `needle` in the document, grouped by unit
/// in order of each unit's first match.
pub fn search(doc: &Document, needle: &[char], match_case: bool) -> Vec<Hit> {
    let hay = haystack(doc, match_case);
    let mut hits: Vec<Hit> = vec![];
    let n = needle.len();
    let mut i = 0;
    while n > 0 && i + n <= hay.folded.len() {
        if hay.folded[i..i + n] != *needle {
            i += 1;
            continue;
        }
        let (first, last) = (hay.from[i], hay.from[i + n - 1]);
        let start = hay.at[first];
        let end = hay.at[last] + hay.chars[last].len_utf8();
        let target = unit(doc, start, end);
        match hits.iter_mut().find(|h| h.target == target) {
            Some(hit) => hit.count += 1,
            None => hits.push(Hit {
                target,
                count: 1,
                at: start,
                excerpt: excerpt(doc, &hay, first, last + 1, target),
                unresolved: match target {
                    Target::Item { list, item } => doc.address_item(list, item).refusal().cloned(),
                    Target::Section(s) if !addressed(doc, target) => {
                        let path = doc.sections[s].path.clone();
                        match doc.resolve(&Selector::HeadingPath { path }) {
                            Ok(t) if t == target => None,
                            Ok(_) => Some(Diagnostic::new(
                                ErrorCode::AmbiguousSelector,
                                "the section's heading path selects another section",
                            )),
                            Err(e) => Some(e),
                        }
                    }
                    _ => None,
                },
            }),
        }
        i += n;
    }
    hits
}

/// The first of the unit's stay ids that selects exactly this unit, so a
/// duplicated id is passed over for another the unit carries, or else for
/// the unit around it.
pub fn address(doc: &Document, target: Target) -> Option<String> {
    doc.target_ids(target).into_iter().find(|id| {
        doc.resolve(&Selector::Id { id: id.clone() })
            .is_ok_and(|t| t == target)
    })
}

fn addressed(doc: &Document, target: Target) -> bool {
    address(doc, target).is_some()
}

/// The direct list item holding all of `start..end`, stamped or not; else
/// the innermost block holding it that a stay id addresses, else the
/// innermost section holding it, else the document.
fn unit(doc: &Document, start: usize, end: usize) -> Target {
    let holds = |s: usize, e: usize| s <= start && end <= e;
    if let Some(b) = doc
        .blocks
        .iter()
        .position(|b| b.heading.is_none() && holds(b.span.start, b.region_end))
    {
        if let Some(l) = doc.blocks[b].list
            && let Some(item) = doc.lists[l]
                .items
                .iter()
                .position(|it| holds(it.span.start, it.span.end))
        {
            return Target::Item { list: l, item };
        }
        if addressed(doc, Target::Block(b)) {
            return Target::Block(b);
        }
    }
    let innermost = doc
        .sections
        .iter()
        .enumerate()
        .filter(|(_, s)| holds(s.whole.start, s.whole.end))
        .map(|(i, _)| i)
        .next_back();
    let mut section = innermost;
    while let Some(s) = section {
        if addressed(doc, Target::Section(s)) {
            return Target::Section(s);
        }
        section = doc.sections[s].parent;
    }
    innermost.map_or(Target::Document, Target::Section)
}

/// One line of context around the characters `first..end`, kept inside the
/// unit and inside the paragraph the match is in.
fn excerpt(doc: &Document, hay: &Hay, first: usize, end: usize, target: Target) -> String {
    let (span, _) = doc.target_span(target);
    let inside = |k: usize| span.contains(hay.at[k]) && hay.chars[k] != '\n';
    let mut from = first;
    while from > 0 && first - from < CONTEXT && inside(from - 1) {
        from -= 1;
    }
    let mut to = end;
    while to < hay.chars.len() && to - end < CONTEXT && inside(to) {
        to += 1;
    }
    let mut text: String = hay.chars[from..to].iter().collect();
    text = text.trim().to_string();
    if from > 0 && inside(from - 1) {
        text.insert(0, '…');
    }
    if to < hay.chars.len() && inside(to) {
        text.push('…');
    }
    text
}
