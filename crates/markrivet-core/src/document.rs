//! Owned document model. Segmentation follows markstay §5.2: top-level
//! CommonMark nodes are blocks, a marker-only HTML block binds to the content
//! block before it, and direct items of a top-level list are child blocks
//! (§5.5). GFM tables are paragraphs here; row identity is declined.
use crate::markers::{self, Lexical};
use crate::yaml::{self, Frontmatter};
use crate::{Diagnostic, ErrorCode, Selector, Span, revision};
use comrak::nodes::{AstNode, ListType, NodeValue, Sourcepos};
use comrak::{Arena, parse_document};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Attach {
    Block(usize),
    Item {
        list: usize,
        item: usize,
    },
    Orphan,
    /// A `subhash` marker that addresses no child block. It is nobody's stay.
    Unaddressed,
    Malformed,
}

#[derive(Debug, Clone)]
pub struct Marker {
    pub lexical: Lexical,
    pub attach: Attach,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Heading {
    pub level: u8,
    pub setext: bool,
    /// Title extent for ATX headings, excluding a closing sequence and a
    /// trailing `{#id}` attribute. Setext titles are not editable.
    pub title: Option<Span>,
    pub title_text: String,
    pub legacy_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Block {
    pub kind: &'static str,
    /// Whole source lines, excluding the last line's terminator.
    pub span: Span,
    /// End of the last marker-only line bound to this block, or `span.end`.
    pub region_end: usize,
    pub first_line: usize,
    pub last_line: usize,
    /// Live block stays (no `subhash`) bound to this block.
    pub markers: Vec<usize>,
    pub heading: Option<Heading>,
    pub list: Option<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Checkbox {
    pub checked: bool,
    /// The single byte between the brackets.
    pub symbol: Span,
}

#[derive(Debug, Clone)]
pub struct Item {
    pub span: Span,
    pub first_line: usize,
    pub last_line: usize,
    /// Leading indentation, list marker and gap on the first line (§5.5).
    pub prefix: Span,
    /// Column width of `prefix`, counting a tab to the next 4-column stop.
    pub width: usize,
    pub checkbox: Option<Checkbox>,
    /// Owned child stays.
    pub markers: Vec<usize>,
    /// Where a new child marker goes: the end of the last direct paragraph.
    pub carrier: Option<usize>,
    /// Line ranges of nested items, which own no child stays.
    pub nested: Vec<(usize, usize)>,
}

#[derive(Debug, Clone)]
pub struct List {
    pub block: usize,
    pub ordered: bool,
    pub tight: bool,
    pub items: Vec<Item>,
}

#[derive(Debug, Clone)]
pub struct Section {
    pub block: usize,
    pub level: u8,
    /// Heading lines plus bound marker-only lines, excluding the terminator.
    pub heading: Span,
    pub body: Span,
    pub whole: Span,
    /// Start of the first child section, or `whole.end`.
    pub own_end: usize,
    pub parent: Option<usize>,
    pub path: Vec<String>,
}

/// What a stay id (or a selector) resolves to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    Section(usize),
    Block(usize),
    Item { list: usize, item: usize },
    FrontmatterKey(usize),
    Document,
}

/// How a response names an item ([`Document::address_item`]).
#[derive(Debug, Clone, PartialEq)]
pub enum Addressing {
    /// A stay id that selects exactly this item.
    Id(String),
    /// A position handle that resolves back to exactly this item.
    Position(Selector),
    /// Neither: the item's bytes repeat, or its heading path matches more
    /// than one section. Carries the refusal a position would get.
    Ambiguous(Box<Diagnostic>),
}

impl Addressing {
    pub fn label(&self) -> &'static str {
        match self {
            Addressing::Id(_) => "id",
            Addressing::Position(_) => "position",
            Addressing::Ambiguous(_) => "ambiguous",
        }
    }

    pub fn id(&self) -> Option<&str> {
        match self {
            Addressing::Id(id) => Some(id),
            _ => None,
        }
    }

    /// The selector `get` and the item writes take, or `null`.
    pub fn select(&self) -> serde_json::Value {
        match self {
            Addressing::Id(id) => serde_json::json!({"kind": "id", "id": id}),
            Addressing::Position(select) => serde_json::to_value(select).expect("selector json"),
            Addressing::Ambiguous(_) => serde_json::Value::Null,
        }
    }

    pub fn refusal(&self) -> Option<&Diagnostic> {
        match self {
            Addressing::Ambiguous(e) => Some(e),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Document {
    pub source: String,
    pub bom: usize,
    pub newline: &'static str,
    pub frontmatter: Frontmatter,
    pub body_start: usize,
    /// Byte offsets of every line start after `body_start`.
    pub lines: Vec<usize>,
    /// Byte offsets of all physical line starts, including frontmatter.
    file_lines: Vec<usize>,
    pub markers: Vec<Marker>,
    pub blocks: Vec<Block>,
    pub lists: Vec<List>,
    pub sections: Vec<Section>,
    pub ids: BTreeMap<String, Vec<Target>>,
    /// Ids whose section carries disagreeing stay and `{#id}` identities.
    pub conflicted: Vec<String>,
    pub diagnostics: Vec<Diagnostic>,
    /// Present when a write must be refused regardless of target.
    pub write_blocker: Option<Diagnostic>,
}

/// 0-based last line of a node. comrak ends a container that precedes a
/// blank line at column 0 of that blank line; the node really ends a line earlier.
/// Whether marker `m` lies wholly inside one of `spans`.
fn inside(spans: &[Span], m: &Lexical) -> bool {
    spans
        .iter()
        .any(|b| b.start <= m.span.start && m.span.end <= b.end)
}

/// The run of `sorted` whose starts lie in `[span.start, span.end)`.
///
/// `sorted` is a marker list in source order: ascending by start and pairwise
/// disjoint, which `markers::scan` guarantees by refusing a marker that starts
/// before its predecessor ends, and which every filter of its output keeps.
/// A marker is never empty, so every marker wholly inside `span` starts in
/// that range: the run holds all of them, in order, plus at most the ones
/// that start inside `span` and end past it, which callers still test for.
fn markers_within<T>(sorted: &[T], span: Span, start: impl Fn(&T) -> usize) -> &[T] {
    let lo = sorted.partition_point(|m| start(m) < span.start);
    let hi = lo + sorted[lo..].partition_point(|m| start(m) < span.end);
    &sorted[lo..hi]
}

/// Live markers are real HTML comments: an inline node that is exactly the
/// marker, or bytes inside an HTML block. MDX-form markers are never live.
/// Order is `lexical`'s.
fn live_markers(lexical: Vec<Lexical>, html_inline: &[Span], html_blocks: &[Span]) -> Vec<Lexical> {
    let inline: BTreeSet<(usize, usize)> = html_inline.iter().map(|s| (s.start, s.end)).collect();
    lexical
        .into_iter()
        .filter(|m| {
            !m.mdx && (inline.contains(&(m.span.start, m.span.end)) || inside(html_blocks, m))
        })
        .collect()
}

/// Whether `source[span]` is nothing but live markers and whitespace.
fn only_markers(source: &str, span: Span, live: &[Lexical]) -> bool {
    let mut text = source[span.start..span.end].to_string();
    for m in markers_within(live, span, |m| m.span.start)
        .iter()
        .rev()
        .filter(|m| span.start <= m.span.start && m.span.end <= span.end)
    {
        text.replace_range(m.span.start - span.start..m.span.end - span.start, "");
    }
    text.bytes()
        .all(|b| matches!(b, b' ' | b'\t' | b'\r' | b'\n' | b'\x0b' | b'\x0c'))
}

pub(crate) fn last_line(p: Sourcepos) -> usize {
    if p.end.column == 0 && p.end.line > p.start.line {
        p.end.line - 2
    } else {
        p.end.line - 1
    }
}

/// The last line a node's own content reaches, for the two kinds whose
/// content can hold a CommonMark blank line: an HTML block of types 1 to 5,
/// which runs to its terminator rather than to a blank line, and a fenced
/// code block. Both are cases where comrak shortens `sourcepos.end` to the
/// last non-blank line while keeping every line in `literal`, so the retained
/// content is the truth and the reported end is not.
///
/// An indented code block is not here: CommonMark §4.4 excludes its trailing
/// blank lines, so it owns none, and `indented_code_end` covers the case
/// where comrak reports it short.
fn content_end<'a>(node: &'a AstNode<'a>) -> Option<usize> {
    let data = node.data.borrow();
    let first = data.sourcepos.start.line - 1;
    // Never invent a line an empty literal does not hold: `- ~~~\n- next`
    // opens a fence with no body, and one invented line would take the
    // sibling item and any marker after it.
    let lines = |text: &String| text.lines().count();
    match &data.value {
        NodeValue::HtmlBlock(b) => Some(first + lines(&b.literal).saturating_sub(1)),
        // `literal` is the content between the fences, so add the opening one.
        NodeValue::CodeBlock(c) if c.fenced => Some(first + lines(&c.literal)),
        _ => None,
    }
}

/// The last content line of an indented code block. When the block is the
/// last thing in a list item and a blank line then closes the item, comrak
/// reports the block's end on its first line, one byte in, and the item
/// inherits that end, so every later line of the block falls outside the
/// item. `literal` still holds every content line and none of the trailing
/// blanks, so it is a floor the span cannot end above. Unlike `owned_end` it
/// claims no blank line, which leaves `extent` trimming them as before.
fn indented_code_end<'a>(node: &'a AstNode<'a>) -> Option<usize> {
    let data = node.data.borrow();
    match &data.value {
        NodeValue::CodeBlock(c) if !c.fenced => {
            Some(data.sourcepos.start.line - 1 + c.literal.lines().count().saturating_sub(1))
        }
        _ => None,
    }
}

/// The last line one of those nodes owns outright: its retained content or
/// its reported end, whichever reaches further. comrak can consume the
/// indentation of a final line that has no terminator, dropping it from
/// `literal` while still reporting it, so neither source alone is enough.
fn owned_end<'a>(node: &'a AstNode<'a>) -> Option<usize> {
    let reported = last_line(node.data.borrow().sourcepos);
    content_end(node).map(|end| end.max(reported))
}

/// The first and last source lines a node owns.
///
/// comrak ends a block that is followed by blank lines at `end.column == 0`
/// on a line past its content, and `last_line` backs over one such line. Two
/// or more leave the reported line blank, so a list item followed by a gap
/// and a list of the other type would report a span running over the gap.
///
/// So walk back over blank lines, with two limits. Blank is CommonMark's
/// blank line, spaces and tabs only, which keeps a non-breaking space inside
/// the paragraph it holds open. And the walk never passes a node whose own
/// retained content reaches further, whether that is this node or one of its
/// descendants: a fenced block or an HTML block keeps its own blank lines,
/// and a list item holding one keeps them with it.
fn extent<'a>(source: &str, lines: &[usize], node: &'a AstNode<'a>) -> (usize, usize) {
    let p = node.data.borrow().sourcepos;
    let first = p.start.line - 1;
    let cap = lines.len().saturating_sub(1);
    if let Some(end) = owned_end(node) {
        return (first, end.clamp(first, cap));
    }
    let floor = node
        .descendants()
        .skip(1)
        .filter_map(owned_end)
        .chain(node.descendants().filter_map(indented_code_end))
        .max()
        .unwrap_or(first)
        .clamp(first, cap);
    let mut last = last_line(p).max(floor);
    while last > floor && {
        let line = &source[lines[last]..lines.get(last + 1).map_or(source.len(), |n| *n)];
        line.bytes()
            .all(|b| matches!(b, b' ' | b'\t' | b'\r' | b'\n'))
    } {
        last -= 1;
    }
    (first, last)
}

struct Node {
    first: usize,
    last: usize,
    heading: Option<(u8, bool)>,
    marker_only: bool,
}

impl Document {
    pub fn parse(source: &str) -> Result<Document, Diagnostic> {
        if source.replace("\r\n", "").contains('\r') {
            return Err(Diagnostic::new(
                ErrorCode::UnsupportedSyntax,
                "lone CR line endings are unsupported",
            ));
        }
        let bom = if source.starts_with('\u{feff}') { 3 } else { 0 };
        let newline = match source.find('\n') {
            Some(i) if i > 0 && source.as_bytes()[i - 1] == b'\r' => "\r\n",
            _ => "\n",
        };
        let frontmatter = yaml::recognize(source, bom);
        let body_start = frontmatter.span.map_or(bom, |s| s.end);
        let body = &source[body_start..];
        let mut lines = vec![body_start];
        lines.extend(body.match_indices('\n').map(|(i, _)| body_start + i + 1));
        let mut doc = Document {
            source: source.to_string(),
            bom,
            newline,
            frontmatter,
            body_start,
            lines,
            file_lines: markers::line_starts(source),
            markers: vec![],
            blocks: vec![],
            lists: vec![],
            sections: vec![],
            ids: BTreeMap::new(),
            conflicted: vec![],
            diagnostics: vec![],
            write_blocker: None,
        };
        if let Err(error) = &doc.frontmatter.fields {
            doc.diagnostics.push(error.clone());
        }
        let lexical: Vec<Lexical> = markers::scan(source)?
            .into_iter()
            .filter(|m| m.span.start >= body_start)
            .collect();

        let arena = Arena::new();
        let options = crate::markdown::options(crate::markdown::ParseMode::Document);
        let root = parse_document(&arena, body, &options);
        let local = |line: usize, column: usize| -> usize { doc.lines[line - 1] + column - 1 };
        // Live markers are real HTML comments: an inline node that is exactly
        // the marker, or bytes inside an HTML block. Code and attributes are content.
        let mut html_blocks = vec![];
        let mut html_inline = vec![];
        let mut code = vec![];
        for node in root.descendants().skip(1) {
            let data = node.data.borrow();
            let p = data.sourcepos;
            let lines = || {
                let (first, last) = extent(&doc.source, &doc.lines, node);
                Span {
                    start: doc.lines[first],
                    end: doc.line_end(last),
                }
            };
            let exact = || Span {
                start: local(p.start.line, p.start.column),
                end: local(p.end.line, p.end.column) + 1,
            };
            match data.value {
                NodeValue::HtmlBlock(_) => html_blocks.push(lines()),
                NodeValue::HtmlInline(_) => html_inline.push(exact()),
                NodeValue::CodeBlock(_) => code.push(lines()),
                NodeValue::Code(_) => code.push(exact()),
                _ => {}
            }
        }
        // §3.2 asks readers to recognise MDX-form markers; Rivet targets
        // CommonMark files and refuses to write rather than misread one.
        if let Some(m) = lexical.iter().find(|m| m.mdx && !inside(&code, m)) {
            let d = Diagnostic::new(
                ErrorCode::UnsupportedSyntax,
                "MDX-form markers are unsupported",
            )
            .with_span(m.span);
            doc.write_blocker = Some(d.clone());
            doc.diagnostics.push(d);
        }
        let live = live_markers(lexical, &html_inline, &html_blocks);

        // Top-level nodes; a marker-only HTML block is a chunk, not a block.
        let mut nodes = vec![];
        for node in root.children() {
            let data = node.data.borrow();
            let (first, mut last) = extent(&doc.source, &doc.lines, node);
            // comrak can end a list before its own last item, when an item
            // carries indented content the list parse did not take (an
            // indented table after a paragraph, say). A list covers its items,
            // and never reaches the next top-level block.
            if matches!(data.value, NodeValue::List(_)) {
                let limit = node
                    .next_sibling()
                    .map_or(usize::MAX, |n| n.data.borrow().sourcepos.start.line - 2);
                for item in node.children() {
                    let (_, end) = extent(&doc.source, &doc.lines, item);
                    last = last.max(end.min(limit));
                }
            }
            let span = Span {
                start: doc.lines[first],
                end: doc.line_end(last),
            };
            let marker_only = matches!(data.value, NodeValue::HtmlBlock(_))
                && only_markers(&doc.source, span, &live);
            nodes.push(Node {
                first,
                last,
                heading: match data.value {
                    NodeValue::Heading(h) => Some((h.level, h.setext)),
                    _ => None,
                },
                marker_only,
            });
            if !marker_only {
                let block = doc.blocks.len();
                doc.blocks.push(Block {
                    kind: data.value.xml_node_name(),
                    span,
                    region_end: span.end,
                    first_line: first,
                    last_line: last,
                    markers: vec![],
                    heading: None,
                    list: None,
                });
                if let NodeValue::List(list) = data.value {
                    let list_index = doc.lists.len();
                    doc.blocks[block].list = Some(list_index);
                    let items = node.children().map(|item| doc.item(item)).collect();
                    doc.lists.push(List {
                        block,
                        ordered: list.list_type == ListType::Ordered,
                        tight: list.tight,
                        items,
                    });
                }
            }
        }

        // Attachment.
        let mut previous_block: Vec<Option<usize>> = vec![];
        let mut block_of_node: Vec<Option<usize>> = vec![];
        let mut counter = 0;
        let mut last = None;
        for node in &nodes {
            previous_block.push(last);
            if node.marker_only {
                block_of_node.push(None);
            } else {
                block_of_node.push(Some(counter));
                last = Some(counter);
                counter += 1;
            }
        }
        for lexical in live {
            let line = doc.line_of(lexical.span.start);
            let n = nodes
                .iter()
                .position(|node| node.first <= line && line <= node.last)
                .expect("a live marker lies inside a top-level node");
            let attach = if lexical.malformed {
                Attach::Malformed
            } else if nodes[n].marker_only {
                match (lexical.has_subhash, previous_block[n]) {
                    (true, _) => Attach::Unaddressed,
                    (false, Some(block)) => Attach::Block(block),
                    (false, None) => Attach::Orphan,
                }
            } else {
                let block = block_of_node[n].expect("content node has a block");
                if !lexical.has_subhash {
                    Attach::Block(block)
                } else {
                    doc.blocks[block]
                        .list
                        .and_then(|list| {
                            doc.lists[list]
                                .items
                                .iter()
                                .position(|item| {
                                    item.first_line <= line
                                        && line <= item.last_line
                                        && !item
                                            .nested
                                            .iter()
                                            .any(|(a, b)| *a <= line && line <= *b)
                                })
                                .map(|item| Attach::Item { list, item })
                        })
                        .unwrap_or(Attach::Unaddressed)
                }
            };
            let index = doc.markers.len();
            match attach {
                Attach::Block(b) => doc.blocks[b].markers.push(index),
                Attach::Item { list, item } => doc.lists[list].items[item].markers.push(index),
                Attach::Orphan => doc.diagnostics.push(
                    Diagnostic::new(ErrorCode::OrphanMarker, "marker has no preceding content block")
                        .with_span(lexical.span)
                        .with_line(line + 1),
                ),
                Attach::Unaddressed => doc.diagnostics.push(
                    Diagnostic::new(
                        ErrorCode::UnaddressedMarker,
                        "subhash marker addresses no supported child block (direct items of a top-level list only)",
                    )
                    .with_span(lexical.span)
                    .with_line(line + 1),
                ),
                Attach::Malformed => doc.diagnostics.push(
                    Diagnostic::new(ErrorCode::MalformedMarker, "stay marker has no id")
                        .with_span(lexical.span)
                        .with_line(line + 1),
                ),
            }
            doc.markers.push(Marker { lexical, attach });
        }
        // Regions: a block owns the marker-only lines that follow it.
        for (n, node) in nodes.iter().enumerate() {
            if node.marker_only
                && let Some(b) = previous_block[n]
            {
                doc.blocks[b].region_end = doc.line_end(node.last);
            }
        }
        for (n, node) in nodes.iter().enumerate() {
            if let (Some(b), Some((level, setext))) = (block_of_node[n], node.heading) {
                doc.blocks[b].heading = Some(doc.heading(b, level, setext));
            }
        }
        doc.build_sections();
        doc.index_ids();
        Ok(doc)
    }

    /// End of a body line, excluding its terminator.
    pub fn line_end(&self, line: usize) -> usize {
        let end = self
            .lines
            .get(line + 1)
            .map_or(self.source.len(), |next| next - 1);
        let end = end.max(self.lines[line]);
        if end > self.lines[line]
            && self.source.as_bytes()[end - 1] == b'\r'
            && end < self.source.len()
        {
            end - 1
        } else {
            end
        }
    }

    /// Start of the line after `pos`'s line, or the end of the source.
    pub fn after_line(&self, pos: usize) -> usize {
        self.source[pos..]
            .find('\n')
            .map_or(self.source.len(), |n| pos + n + 1)
    }

    /// 0-based body line containing `pos`.
    pub fn line_of(&self, pos: usize) -> usize {
        self.lines.partition_point(|start| *start <= pos) - 1
    }

    /// 1-based physical line for a byte offset, including offsets inside UTF-8 characters.
    pub fn file_line(&self, pos: usize) -> usize {
        assert!(
            pos <= self.source.len(),
            "file offset is outside the document"
        );
        self.file_lines.partition_point(|start| *start <= pos)
    }

    fn item<'a>(&self, node: &'a AstNode<'a>) -> Item {
        let data = node.data.borrow();
        let (first, last) = extent(&self.source, &self.lines, node);
        let line_start = self.lines[first];
        let text = &self.source[line_start..self.line_end(first)];
        let (prefix_len, width) = list_prefix(text).unwrap_or_else(|| lenient_prefix(text));
        let checkbox = match data.value {
            NodeValue::TaskItem(task) => {
                let s = task.symbol_sourcepos.start;
                let start = self.lines[s.line - 1] + s.column - 1;
                Some(Checkbox {
                    checked: task.symbol.is_some(),
                    symbol: Span {
                        start,
                        end: start + task.symbol.map_or(1, char::len_utf8),
                    },
                })
            }
            _ => None,
        };
        let carrier = node
            .children()
            .filter(|child| matches!(child.data.borrow().value, NodeValue::Paragraph))
            .last()
            .map(|paragraph| {
                let e = paragraph.data.borrow().sourcepos.end;
                let end = self.lines[e.line - 1] + e.column;
                let line_start = self.lines[e.line - 1];
                line_start
                    + self.source[line_start..end]
                        .trim_end_matches([' ', '\t', '\x0b', '\x0c'])
                        .len()
            });
        let nested = node
            .descendants()
            .skip(1)
            .filter(|n| {
                matches!(
                    n.data.borrow().value,
                    NodeValue::Item(_) | NodeValue::TaskItem(_)
                )
            })
            .map(|n| extent(&self.source, &self.lines, n))
            .collect();
        Item {
            span: Span {
                start: line_start,
                end: self.line_end(last),
            },
            first_line: first,
            last_line: last,
            prefix: Span {
                start: line_start,
                end: line_start + prefix_len,
            },
            width,
            checkbox,
            markers: vec![],
            carrier,
            nested,
        }
    }

    fn heading(&self, block: usize, level: u8, setext: bool) -> Heading {
        let span = self.blocks[block].span;
        let text = &self.source[span.start..span.end];
        if setext {
            let title_end = text.rfind('\n').unwrap_or(text.len());
            let title_text = text[..title_end]
                .trim_matches([' ', '\t', '\r', '\n'])
                .to_string();
            return Heading {
                level,
                setext,
                title: None,
                title_text,
                legacy_id: None,
            };
        }
        let (title, legacy_id) = atx_title(text);
        let title_span = Span {
            start: span.start + title.start,
            end: span.start + title.end,
        };
        Heading {
            level,
            setext,
            title: Some(title_span),
            title_text: text[title.start..title.end].to_string(),
            legacy_id,
        }
    }

    fn build_sections(&mut self) {
        let headings: Vec<(usize, u8)> = self
            .blocks
            .iter()
            .enumerate()
            .filter_map(|(i, b)| b.heading.as_ref().map(|h| (i, h.level)))
            .collect();
        let mut stack: Vec<usize> = vec![];
        for (n, &(block, level)) in headings.iter().enumerate() {
            let start = self.blocks[block].span.start;
            let end = headings[n + 1..]
                .iter()
                .find(|(_, l)| *l <= level)
                .map_or(self.source.len(), |(b, _)| self.blocks[*b].span.start);
            let own_end = headings
                .get(n + 1)
                .filter(|(b, _)| self.blocks[*b].span.start < end)
                .map_or(end, |(b, _)| self.blocks[*b].span.start);
            let heading_end = self.blocks[block].region_end;
            let body_start = self.after_line(heading_end).min(end.max(heading_end));
            while stack
                .last()
                .is_some_and(|s| self.sections[*s].level >= level)
            {
                stack.pop();
            }
            let parent = stack.last().copied();
            let mut path = parent.map_or(vec![], |p| self.sections[p].path.clone());
            path.push(
                self.blocks[block]
                    .heading
                    .as_ref()
                    .map(|h| h.title_text.clone())
                    .unwrap_or_default(),
            );
            stack.push(self.sections.len());
            self.sections.push(Section {
                block,
                level,
                heading: Span {
                    start,
                    end: heading_end,
                },
                body: Span {
                    start: body_start,
                    end,
                },
                whole: Span { start, end },
                own_end: own_end.max(body_start),
                parent,
                path,
            });
        }
    }

    fn index_ids(&mut self) {
        let mut ids: BTreeMap<String, Vec<Target>> = BTreeMap::new();
        let section_of_block: BTreeMap<usize, usize> = self
            .sections
            .iter()
            .enumerate()
            .map(|(i, s)| (s.block, i))
            .collect();
        for marker in &self.markers {
            let Some(id) = marker.lexical.id.clone() else {
                continue;
            };
            let target = match marker.attach {
                Attach::Block(b) => match section_of_block.get(&b) {
                    Some(s) => Target::Section(*s),
                    None => Target::Block(b),
                },
                Attach::Item { list, item } => Target::Item { list, item },
                // Orphan and unaddressed ids still collide with real ones.
                Attach::Orphan | Attach::Unaddressed => Target::Document,
                Attach::Malformed => continue,
            };
            ids.entry(id).or_default().push(target);
        }
        for (s, section) in self.sections.iter().enumerate() {
            let block = &self.blocks[section.block];
            let Some(legacy) = block.heading.as_ref().and_then(|h| h.legacy_id.clone()) else {
                continue;
            };
            let stays: Vec<String> = block
                .markers
                .iter()
                .filter_map(|m| self.markers[*m].lexical.id.clone())
                .collect();
            if stays.is_empty() {
                ids.entry(legacy).or_default().push(Target::Section(s));
            } else if !stays.contains(&legacy) {
                self.diagnostics.push(
                    Diagnostic::new(
                        ErrorCode::DuplicateId,
                        format!("heading {{#{legacy}}} disagrees with its stay marker"),
                    )
                    .with_id(legacy.clone())
                    .with_span(block.span),
                );
                self.conflicted.push(legacy);
                self.conflicted.extend(stays);
            }
        }
        for (id, targets) in &ids {
            if targets.len() > 1 {
                self.diagnostics.push(
                    Diagnostic::new(
                        ErrorCode::DuplicateId,
                        format!("stay id {id} is not unique"),
                    )
                    .with_id(id.clone()),
                );
            }
        }
        self.ids = ids;
    }

    pub fn revision(&self) -> String {
        revision("document", "", self.source.as_bytes(), &[])
    }

    /// Source with every live, well-formed marker inside `span` removed.
    pub fn strip_markers(&self, span: Span) -> String {
        let mut out = String::with_capacity(span.len());
        let mut at = span.start;
        for m in markers_within(&self.markers, span, |m| m.lexical.span.start) {
            let s = m.lexical.span;
            if m.attach == Attach::Malformed || s.start < span.start || s.end > span.end {
                continue;
            }
            out.push_str(&self.source[at..s.start]);
            at = s.end;
        }
        out.push_str(&self.source[at..span.end]);
        out
    }

    /// §8 digest of a block body: whole lines, markers removed, ASCII-trimmed
    /// as the markstay reference trims block content before hashing.
    pub fn block_hash(&self, block: usize) -> String {
        let body = self.strip_markers(self.blocks[block].span);
        markstay::body_hash(
            body.trim_matches([' ', '\t', '\n', '\r', '\x0c', '\x0b']),
            None,
        )
    }

    /// §5.5 child body of a direct list item.
    pub fn item_body(&self, list: usize, item: usize) -> String {
        child_body(&self.strip_markers(self.lists[list].items[item].span))
    }

    pub fn item_hash(&self, list: usize, item: usize) -> String {
        markstay::body_hash(&self.item_body(list, item), None)
    }

    /// The digest a marker's stored evidence should equal, if it binds anything.
    pub fn marker_evidence(&self, marker: usize) -> Option<(String, Option<String>)> {
        let m = &self.markers[marker];
        match m.attach {
            Attach::Block(b) => Some((self.block_hash(b), m.lexical.hash.clone())),
            Attach::Item { list, item } => {
                Some((self.item_hash(list, item), m.lexical.subhash.clone()))
            }
            _ => None,
        }
    }

    pub fn stay_ids(&self, markers: &[usize]) -> Vec<String> {
        markers
            .iter()
            .filter_map(|m| self.markers[*m].lexical.id.clone())
            .collect()
    }

    pub fn section_ids(&self, section: usize) -> Vec<String> {
        let block = &self.blocks[self.sections[section].block];
        let mut ids = self.stay_ids(&block.markers);
        if ids.is_empty() {
            ids.extend(block.heading.as_ref().and_then(|h| h.legacy_id.clone()));
        }
        ids
    }

    pub fn target_ids(&self, target: Target) -> Vec<String> {
        match target {
            Target::Section(s) => self.section_ids(s),
            Target::Block(b) => self.stay_ids(&self.blocks[b].markers),
            Target::Item { list, item } => self.stay_ids(&self.lists[list].items[item].markers),
            _ => vec![],
        }
    }

    /// Stable archive bytes: exclude only unowned, physically blank separator
    /// lines. Nonblank lines such as reference definitions survive without AST nodes.
    pub(crate) fn section_content_span(&self, section: usize) -> Result<Span, Diagnostic> {
        let whole = self.sections[section].whole;
        let refuse = |message| Diagnostic::new(ErrorCode::ArchiveCopyRefused, message);
        // This also rejects consumed EOF indentation absent from both Comrak
        // positions and retained literals. Never infer ownership for that byte.
        if !whole
            .slice(&self.source)
            .expect("section span")
            .ends_with('\n')
        {
            return Err(refuse(
                "section archive requires a terminated final physical line; EOF ownership is ambiguous",
            ));
        }
        let mut end = whole.end;
        while end > whole.start {
            let line_start = self.source[..end - 1].rfind('\n').map_or(0, |n| n + 1);
            let line = &self.source[line_start..end];
            let body = line
                .strip_suffix("\r\n")
                .or_else(|| line.strip_suffix('\n'))
                .unwrap_or(line);
            let owned = self
                .blocks
                .iter()
                .any(|b| b.span.start < end && b.region_end > line_start);
            if !body.bytes().all(|b| matches!(b, b' ' | b'\t')) || owned {
                break;
            }
            end = line_start;
        }
        let span = Span {
            start: whole.start,
            end,
        };
        // Ask the same parser whether the next physical heading is still content
        // of a terminal fence/HTML block. A closed control accepts it as a heading.
        let text = span.slice(&self.source).expect("content span");
        let probe = format!("{text}# Archive boundary probe\n");
        let arena = Arena::new();
        let root = parse_document(
            &arena,
            &probe,
            &crate::markdown::options(crate::markdown::ParseMode::Document),
        );
        let line = text.bytes().filter(|b| *b == b'\n').count() + 1;
        if !root.children().any(|n| {
            let d = n.data.borrow();
            matches!(d.value, NodeValue::Heading(_)) && d.sourcepos.start.line == line
        }) {
            return Err(refuse(
                "section archive ends in parser-owned fence or HTML content",
            ));
        }
        Ok(span)
    }

    /// The span a guard covers and the revision kind for a target.
    pub fn target_span(&self, target: Target) -> (Span, &'static str) {
        match target {
            Target::Section(s) => (self.sections[s].whole, "section"),
            Target::Block(b) => {
                let block = &self.blocks[b];
                (
                    Span {
                        start: block.span.start,
                        end: block.region_end,
                    },
                    if block.list.is_some() {
                        "list"
                    } else {
                        "block"
                    },
                )
            }
            Target::Item { list, item } => (self.lists[list].items[item].span, "item"),
            Target::FrontmatterKey(k) => (self.scalars()[k].line, "frontmatter"),
            Target::Document => (
                Span {
                    start: 0,
                    end: self.source.len(),
                },
                "document",
            ),
        }
    }

    pub fn target_revision(&self, target: Target) -> String {
        let (span, kind) = self.target_span(target);
        let identity = match target {
            Target::FrontmatterKey(k) => self.scalars()[k].key.clone(),
            _ => self.target_ids(target).first().cloned().unwrap_or_default(),
        };
        revision(
            kind,
            &identity,
            &self.source.as_bytes()[span.start..span.end],
            &[],
        )
    }

    pub fn scalars(&self) -> &[yaml::Scalar] {
        self.frontmatter.fields.as_deref().unwrap_or(&[])
    }

    pub fn resolve(&self, selector: &Selector) -> Result<Target, Diagnostic> {
        match selector {
            Selector::Document => Ok(Target::Document),
            Selector::Id { id } => {
                if self.conflicted.contains(id) {
                    return Err(Diagnostic::new(
                        ErrorCode::DuplicateId,
                        format!("id {id} names a heading whose identities disagree"),
                    )
                    .with_id(id.clone()));
                }
                match self.ids.get(id).map(Vec::as_slice) {
                    None | Some([]) => Err(Diagnostic::new(
                        ErrorCode::MissingSelector,
                        format!("no stay with id {id}"),
                    )
                    .with_id(id.clone())),
                    Some([Target::Document]) => Err(Diagnostic::new(
                        ErrorCode::MissingSelector,
                        format!("stay {id} is orphaned or addresses no supported block"),
                    )
                    .with_id(id.clone())),
                    Some([target]) => Ok(*target),
                    Some(_) => Err(Diagnostic::new(
                        ErrorCode::DuplicateId,
                        format!("stay id {id} is not unique"),
                    )
                    .with_id(id.clone())),
                }
            }
            Selector::HeadingPath { path } => {
                if path.is_empty() {
                    return Err(Diagnostic::new(
                        ErrorCode::InvalidRequest,
                        "empty heading path",
                    ));
                }
                let matches: Vec<usize> = self
                    .sections
                    .iter()
                    .enumerate()
                    .filter(|(_, s)| s.path.ends_with(path))
                    .map(|(i, _)| i)
                    .collect();
                match matches.as_slice() {
                    [] => Err(Diagnostic::new(
                        ErrorCode::MissingSelector,
                        format!("no section at heading path {path:?}"),
                    )),
                    [one] => Ok(Target::Section(*one)),
                    _ => Err(Diagnostic::new(
                        ErrorCode::AmbiguousSelector,
                        format!("{} sections match heading path {path:?}", matches.len()),
                    )),
                }
            }
            Selector::Position {
                heading_path,
                index,
            } => {
                let section = match heading_path {
                    None => None,
                    Some(path) => {
                        match self.resolve(&Selector::HeadingPath { path: path.clone() })? {
                            Target::Section(s) => Some(s),
                            _ => unreachable!("a heading path resolves to a section"),
                        }
                    }
                };
                let items = self.positional_items(section);
                let (list, item) = usize::try_from(*index)
                    .ok()
                    .and_then(|i| items.get(i))
                    .copied()
                    .ok_or_else(|| {
                        let place = match section {
                            Some(s) => format!("section {:?}", self.sections[s].path),
                            None => "the preamble".to_string(),
                        };
                        Diagnostic::new(
                            ErrorCode::MissingSelector,
                            format!(
                                "no item at index {index} in {place}, which has {} direct items",
                                items.len()
                            ),
                        )
                    })?;
                if let Some((l, i)) = self.repeated_item(list, item) {
                    let line = self.file_line(self.lists[l].items[i].span.start);
                    return Err(Diagnostic::new(
                        ErrorCode::AmbiguousSelector,
                        format!(
                            "the item's bytes repeat at line {line}; a position cannot tell the two apart, stamp the file to address it"
                        ),
                    )
                    .with_line(line));
                }
                Ok(Target::Item { list, item })
            }
            Selector::Collections => Err(Diagnostic::new(
                ErrorCode::InvalidRequest,
                "the collections scope is resolved against the project, not a document",
            )),
            Selector::FrontmatterKey { key } => {
                if let Err(e) = &self.frontmatter.fields {
                    return Err(e.clone());
                }
                self.scalars()
                    .iter()
                    .position(|s| &s.key == key)
                    .map(Target::FrontmatterKey)
                    .ok_or_else(|| {
                        Diagnostic::new(
                            ErrorCode::MissingSelector,
                            format!("no frontmatter key {key}"),
                        )
                    })
            }
        }
    }

    /// The direct items a position counts in a section's own body (or, for
    /// `None`, the preamble before the first heading): every item of every
    /// top-level list there, in document order, as one sequence.
    pub fn positional_items(&self, section: Option<usize>) -> Vec<(usize, usize)> {
        let mut lists: Vec<usize> = (0..self.lists.len())
            .filter(|&l| self.section_of(self.blocks[self.lists[l].block].span.start) == section)
            .collect();
        lists.sort_by_key(|&l| self.blocks[self.lists[l].block].span.start);
        lists
            .into_iter()
            .flat_map(|l| (0..self.lists[l].items.len()).map(move |i| (l, i)))
            .collect()
    }

    /// Another direct item anywhere in the document with exactly this item's
    /// span bytes. Two such items cannot be told apart by place and guard.
    pub fn repeated_item(&self, list: usize, item: usize) -> Option<(usize, usize)> {
        let span = self.lists[list].items[item].span;
        let bytes = &self.source.as_bytes()[span.start..span.end];
        self.lists.iter().enumerate().find_map(|(l, other)| {
            other.items.iter().enumerate().find_map(|(i, it)| {
                ((l, i) != (list, item)
                    && &self.source.as_bytes()[it.span.start..it.span.end] == bytes)
                    .then_some((l, i))
            })
        })
    }

    /// The position handle naming an item's place, with the section's full
    /// heading path. `None` for anything but a direct list item.
    pub fn position_of(&self, target: Target) -> Option<Selector> {
        let Target::Item { list, item } = target else {
            return None;
        };
        let section = self.section_of(self.blocks[self.lists[list].block].span.start);
        let index = self
            .positional_items(section)
            .iter()
            .position(|&p| p == (list, item))?;
        Some(Selector::Position {
            heading_path: section.map(|s| self.sections[s].path.clone()),
            index: index as u64,
        })
    }

    /// How every read and write response names an item: by a stay id that
    /// selects it, else by a position handle that resolves back to exactly
    /// this item, else not at all, with the refusal resolution would give.
    pub fn address_item(&self, list: usize, item: usize) -> Addressing {
        let target = Target::Item { list, item };
        if let Some(id) = crate::find::address(self, target) {
            return Addressing::Id(id);
        }
        let select = self
            .position_of(target)
            .expect("a direct item has a position");
        match self.resolve(&select) {
            Ok(t) if t == target => Addressing::Position(select),
            Ok(_) => Addressing::Ambiguous(Box::new(Diagnostic::new(
                ErrorCode::AmbiguousSelector,
                "the item's position selects another item",
            ))),
            Err(e) => Addressing::Ambiguous(Box::new(e)),
        }
    }

    /// Blocks whose span begins inside `span`.
    pub fn blocks_in(&self, span: Span) -> impl Iterator<Item = usize> + '_ {
        self.blocks
            .iter()
            .enumerate()
            .filter(move |(_, b)| span.contains(b.span.start))
            .map(|(i, _)| i)
    }

    /// Section containing a block, innermost first.
    pub fn section_of(&self, pos: usize) -> Option<usize> {
        self.sections
            .iter()
            .enumerate()
            .filter(|(_, s)| s.whole.start <= pos && pos < s.whole.end)
            .map(|(i, _)| i)
            .next_back()
    }

    /// Editable item text: first-line prefix and continuation indentation
    /// removed, own child markers (with the separator before them) removed.
    pub fn item_content(&self, list: usize, item: usize) -> String {
        let lines: Vec<String> = self
            .item_lines(list, item)
            .into_iter()
            .map(|(_, text)| text)
            .collect();
        lines.join("\n")
    }

    /// The lines `item_content` joins, each paired with the indentation removed
    /// from it. The first line and any continuation not indented by exactly the
    /// item's width pair with nothing removed: such a line keeps its own
    /// leading whitespace as text.
    pub fn item_lines(&self, list: usize, item: usize) -> Vec<(String, String)> {
        let it = &self.lists[list].items[item];
        let mut text = String::new();
        let mut at = it.prefix.end;
        for m in &it.markers {
            let span = self.markers[*m].lexical.span;
            let mut start = span.start;
            while start > at && matches!(self.source.as_bytes()[start - 1], b' ' | b'\t') {
                start -= 1;
            }
            text.push_str(&self.source[at..start]);
            at = span.end;
        }
        text.push_str(&self.source[at..it.span.end]);
        let text = text.replace("\r\n", "\n");
        let mut out: Vec<(String, String)> = text
            .split('\n')
            .enumerate()
            .map(|(n, line)| {
                let kept = if n == 0 { line } else { dedent(line, it.width) };
                let removed = &line[..line.len() - kept.len()];
                (removed.to_string(), kept.to_string())
            })
            .collect();
        if let Some((_, last)) = out.last_mut() {
            last.truncate(last.trim_end_matches([' ', '\t']).len());
        }
        out
    }
}

/// ATX title extent (relative) and optional trailing `{#id}`.
fn atx_title(text: &str) -> (Span, Option<String>) {
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() && bytes[i] == b' ' {
        i += 1;
    }
    while i < bytes.len() && bytes[i] == b'#' {
        i += 1;
    }
    let mut start = i;
    while start < bytes.len() && matches!(bytes[start], b' ' | b'\t') {
        start += 1;
    }
    let mut end = text.trim_end_matches([' ', '\t', '\r']).len().max(start);
    // A closing sequence needs whitespace before it (or to be the whole rest).
    let trimmed = &text[start..end];
    let without_hashes = trimmed.trim_end_matches('#');
    if without_hashes.len() < trimmed.len()
        && (without_hashes.is_empty() || without_hashes.ends_with([' ', '\t']))
    {
        end = start + without_hashes.trim_end_matches([' ', '\t']).len();
    }
    let mut legacy = None;
    let title = &text[start..end];
    if let Some(open) = title.rfind("{#") {
        let attr = &title[open..];
        let id = attr
            .strip_prefix("{#")
            .and_then(|a| a.strip_suffix('}'))
            .unwrap_or("");
        if !id.is_empty()
            && id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
            && (open == 0 || title[..open].ends_with([' ', '\t']))
        {
            legacy = Some(id.to_string());
            end = start + title[..open].trim_end_matches([' ', '\t']).len();
        }
    }
    (Span { start, end }, legacy)
}

/// §5.5 prefix as the markstay reference matches it: up to three spaces, a
/// bullet or 1-9 digits plus `.`/`)`, a whitespace gap, then a non-space.
/// Returns (bytes, columns); `None` when the reference regex would not match.
pub fn list_prefix(line: &str) -> Option<(usize, usize)> {
    let (gap, end) = marker_and_gap(line)?;
    let next = line[end..].chars().next()?;
    (end > gap && !next.is_whitespace()).then(|| (end, columns(&line[..end])))
}

/// Indentation, list marker and trailing whitespace, without the reference's
/// lookahead: used to re-render items whose first line may be empty.
fn lenient_prefix(line: &str) -> (usize, usize) {
    let (_, end) = marker_and_gap(line).unwrap_or((0, 0));
    (end, columns(&line[..end]))
}

/// The list marker's delimiter byte (`-`, `*`, `+`, `.` or `)`): two items
/// with different delimiters never share a list.
pub fn list_delimiter(line: &str) -> Option<u8> {
    let (gap, _) = marker_and_gap(line)?;
    Some(line.as_bytes()[gap - 1])
}

fn marker_and_gap(line: &str) -> Option<(usize, usize)> {
    let b = line.as_bytes();
    let mut i = 0;
    while i < 3 && b.get(i) == Some(&b' ') {
        i += 1;
    }
    let marker_start = i;
    if matches!(b.get(i), Some(b'*' | b'+' | b'-')) {
        i += 1;
    } else {
        while i < b.len() && b[i].is_ascii_digit() && i - marker_start < 9 {
            i += 1;
        }
        if i == marker_start || !matches!(b.get(i), Some(b'.' | b')')) {
            return None;
        }
        i += 1;
    }
    let gap = i;
    while matches!(b.get(i), Some(b' ' | b'\t')) {
        i += 1;
    }
    Some((gap, i))
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

/// Remove `width` columns of leading whitespace when the line carries exactly
/// that much at a character boundary (markstay's reference rule).
pub fn dedent(line: &str, width: usize) -> &str {
    let mut col = 0;
    for (cut, c) in line.char_indices() {
        if col >= width {
            return if col == width { &line[cut..] } else { line };
        }
        match c {
            ' ' => col += 1,
            '\t' => col += 4 - col % 4,
            _ => return line,
        }
    }
    if col == width { "" } else { line }
}

/// §5.5 child body, ported from the markstay Python reference: prefix and
/// continuation indentation removed, then trimmed. Markers are removed first.
pub fn child_body(stripped: &str) -> String {
    let text = stripped.replace("\r\n", "\n");
    let mut lines: Vec<&str> = text.split('\n').collect();
    let (prefix, width) = lines.first().and_then(|l| list_prefix(l)).unwrap_or((0, 0));
    let first = lines.first().map(|l| &l[prefix..]).unwrap_or("");
    let rest: Vec<&str> = lines.drain(1..).map(|l| dedent(l, width)).collect();
    let mut out = String::from(first);
    for line in rest {
        out.push('\n');
        out.push_str(line);
    }
    out.trim_matches([' ', '\t', '\n', '\r', '\x0c', '\x0b'])
        .to_string()
}

#[cfg(test)]
mod tests {
    //! The sorted marker lookups select exactly what
    //! the linear searches they replace selected. The linear versions are kept
    //! here verbatim as oracles.
    use super::*;

    fn strip_markers_oracle(doc: &Document, span: Span) -> String {
        let mut out = String::with_capacity(span.len());
        let mut at = span.start;
        for m in &doc.markers {
            let s = m.lexical.span;
            if m.attach == Attach::Malformed || s.start < span.start || s.end > span.end {
                continue;
            }
            out.push_str(&doc.source[at..s.start]);
            at = s.end;
        }
        out.push_str(&doc.source[at..span.end]);
        out
    }

    fn live_markers_oracle(
        lexical: Vec<Lexical>,
        html_inline: &[Span],
        html_blocks: &[Span],
    ) -> Vec<Lexical> {
        let inside = |spans: &[Span], m: &Lexical| {
            spans
                .iter()
                .any(|b| b.start <= m.span.start && m.span.end <= b.end)
        };
        lexical
            .into_iter()
            .filter(|m| !m.mdx && (html_inline.contains(&m.span) || inside(html_blocks, m)))
            .collect()
    }

    fn only_markers_oracle(source: &str, span: Span, live: &[Lexical]) -> bool {
        let mut text = source[span.start..span.end].to_string();
        for m in live
            .iter()
            .rev()
            .filter(|m| span.start <= m.span.start && m.span.end <= span.end)
        {
            text.replace_range(m.span.start - span.start..m.span.end - span.start, "");
        }
        text.bytes()
            .all(|b| matches!(b, b' ' | b'\t' | b'\r' | b'\n' | b'\x0b' | b'\x0c'))
    }

    /// xorshift64*, seeded per case; no dependency.
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
        }
        fn below(&mut self, n: usize) -> usize {
            (self.next() % n as u64) as usize
        }
    }

    const MALFORMED: &str = "<!-- stay:hash=sha256:111111111111 -->";

    /// One marker of a kind `Document` tells apart.
    fn marker(rng: &mut Rng) -> String {
        let id: String = (0..8)
            .map(|_| (b'a' + rng.below(26) as u8) as char)
            .collect();
        match rng.below(6) {
            0 => markers::format_plain(&id, "hash", "000000000000"),
            1 => markers::format_plain(&id, "subhash", "000000000000"),
            2 => format!("<!-- stay:{id} -->"),
            3 => format!("{{/* stay:{id} */}}"),
            _ => MALFORMED.to_string(),
        }
    }

    /// A document of paragraphs, headings, checkbox lists with nested items,
    /// code, HTML blocks and marker-only lines, carrying every marker kind,
    /// in LF or CRLF.
    fn document(seed: u64) -> String {
        let mut rng = Rng(seed.wrapping_mul(0xA076_1D64_78BD_642F) | 1);
        let mut out = String::new();
        for _ in 0..(2 + rng.below(12)) {
            let m = marker(&mut rng);
            let n = marker(&mut rng);
            match rng.below(9) {
                0 => out.push_str(&format!("## Heading {m}\n\n")),
                1 => out.push_str(&format!("A paragraph {m} and {n}\nsecond line.\n\n")),
                2 => out.push_str(&format!("A paragraph.\n{m}\n\n")),
                3 => out.push_str(&format!("{m}\n{n}\n\n")),
                4 => out.push_str(&format!("<div>\n{m}\ntext {n}\n</div>\n\n")),
                5 => out.push_str(&format!("```\ncode {m}\n```\n\n")),
                6 => out.push_str(&format!("Inline `{m}` code {n}.\n\n")),
                _ => {
                    for i in 0..(1 + rng.below(6)) {
                        let m = marker(&mut rng);
                        out.push_str(&format!("- [ ] **Item {i}** text {m}\n"));
                        if rng.below(4) == 0 {
                            let n = marker(&mut rng);
                            out.push_str(&format!("  - nested {n}\n"));
                        }
                    }
                    out.push('\n');
                }
            }
        }
        if rng.below(2) == 0 {
            out.replace('\n', "\r\n")
        } else {
            out
        }
    }

    /// A span between two char boundaries of `text`.
    fn any_span(rng: &mut Rng, text: &str) -> Span {
        let at = |rng: &mut Rng| {
            let mut i = rng.below(text.len() + 1);
            while !text.is_char_boundary(i) {
                i -= 1;
            }
            i
        };
        let (a, b) = (at(rng), at(rng));
        Span {
            start: a.min(b),
            end: a.max(b),
        }
    }

    #[test]
    fn sorted_marker_lookups_select_what_the_linear_searches_did() {
        let (mut docs, mut spans, mut malformed_spans, mut live_sets, mut only) = (0, 0, 0, 0, 0);
        for seed in 0..600u64 {
            let src = document(seed);
            let mut rng = Rng(seed | 1);

            // live_markers over the scanner's own output and adversarial spans.
            if let Ok(lexical) = markers::scan(&src) {
                let mut inline = vec![];
                let mut blocks = vec![];
                for m in &lexical {
                    match rng.below(5) {
                        0 => inline.push(m.span),
                        1 => inline.extend([m.span, m.span]),
                        2 => inline.push(Span {
                            start: m.span.start + 1,
                            end: m.span.end + 1,
                        }),
                        3 => blocks.push(Span {
                            start: m.span.start.saturating_sub(rng.below(3)),
                            end: m.span.end + rng.below(3),
                        }),
                        _ => {}
                    }
                }
                for _ in 0..rng.below(4) {
                    inline.push(any_span(&mut rng, &src));
                    blocks.push(any_span(&mut rng, &src));
                }
                assert_eq!(
                    live_markers(lexical.clone(), &inline, &blocks),
                    live_markers_oracle(lexical, &inline, &blocks),
                    "seed {seed}: live set\n{src}"
                );
                live_sets += 1;
            }

            let Ok(doc) = Document::parse(&src) else {
                continue;
            };
            docs += 1;
            // Precondition S.
            for pair in doc.markers.windows(2) {
                assert!(
                    pair[0].lexical.span.end <= pair[1].lexical.span.start,
                    "seed {seed}: markers out of order or overlapping"
                );
            }
            assert!(doc.markers.iter().all(|m| !m.lexical.span.is_empty()));

            let live: Vec<Lexical> = doc.markers.iter().map(|m| m.lexical.clone()).collect();
            let mut tested: Vec<Span> = doc.blocks.iter().map(|b| b.span).collect();
            tested.extend(
                doc.lists
                    .iter()
                    .flat_map(|l| l.items.iter().map(|i| i.span)),
            );
            for _ in 0..8 {
                tested.push(any_span(&mut rng, &src));
            }
            for span in tested {
                assert_eq!(
                    doc.strip_markers(span),
                    strip_markers_oracle(&doc, span),
                    "seed {seed}: strip_markers {span:?}\n{src}"
                );
                assert_eq!(
                    only_markers(&doc.source, span, &live),
                    only_markers_oracle(&doc.source, span, &live),
                    "seed {seed}: only_markers {span:?}\n{src}"
                );
                spans += 1;
                only += 1;
                if doc.markers.iter().any(|m| {
                    m.attach == Attach::Malformed
                        && span.start <= m.lexical.span.start
                        && m.lexical.span.end <= span.end
                }) {
                    malformed_spans += 1;
                }
            }
        }
        assert!(docs >= 300, "only {docs} generated documents parsed");
        assert!(
            malformed_spans > 0,
            "no tested span held a live malformed marker"
        );
        eprintln!(
            "sorted marker lookups: {docs} documents, {spans} strip_markers spans \
             ({malformed_spans} holding a malformed marker), {only} only_markers spans, \
             {live_sets} live sets, 0 mismatches"
        );
    }

    #[test]
    fn a_malformed_marker_before_a_well_formed_one_keeps_its_bytes() {
        let src = format!("A paragraph {MALFORMED} and <!-- stay:abcdefgh --> end.\n");
        let doc = Document::parse(&src).unwrap();
        assert!(
            doc.markers.iter().any(|m| m.attach == Attach::Malformed),
            "{:?}",
            doc.markers
        );
        let span = doc.blocks[0].span;
        let stripped = doc.strip_markers(span);
        assert_eq!(stripped, format!("A paragraph {MALFORMED} and  end."));
        assert_eq!(stripped, strip_markers_oracle(&doc, span));
    }
}
