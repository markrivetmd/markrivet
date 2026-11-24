use crate::document::Document;
use crate::markers;
use crate::{Diagnostic, Span, contract::unsupported, revision};
use comrak::{Arena, Options, nodes::NodeValue, parse_document};
use serde::{Deserialize, Serialize};

/// Inspection exposes GFM tables; the editing contract treats them as paragraphs.
#[derive(Clone, Copy)]
pub(crate) enum ParseMode {
    Document,
    Inspection,
}

/// Keep every parse used for planning, extraction and verification on the same dialect.
pub(crate) fn options(mode: ParseMode) -> Options<'static> {
    let mut options = Options::default();
    options.extension.tasklist = true;
    options.extension.table = matches!(mode, ParseMode::Inspection);
    options
}

/// Ask the pinned parser whether any candidate definition label resolves. This
/// also finds unused definitions, which have no paragraph extent in the AST.
pub(crate) fn has_reference_definition(source: &str) -> Result<bool, Diagnostic> {
    let bytes = source.as_bytes();
    let mut labels = std::collections::BTreeSet::new();
    let mut open = None;
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 1 < bytes.len() {
            i += 2;
            continue;
        }
        match bytes[i] {
            b'[' => open = Some(i + 1),
            b']' => {
                if let Some(start) = open
                    && bytes.get(i + 1) == Some(&b':')
                    && i - start <= 1000
                    && !source[start..i].trim().is_empty()
                {
                    labels.insert(source[start..i].to_string());
                    if labels.len() > 4096 {
                        return Err(Diagnostic::new(
                            crate::ErrorCode::LimitExceeded,
                            "reference definition scan exceeds 4096 candidate labels",
                        ));
                    }
                }
                open = None;
            }
            _ => {}
        }
        if open.is_some_and(|start| i.saturating_sub(start) > 1000) {
            open = None;
        }
        i += 1;
    }
    if labels.is_empty() {
        return Ok(false);
    }
    let mut text = format!("{source}\n\n");
    let mut line = source.bytes().filter(|b| *b == b'\n').count() + 3;
    let mut probes = std::collections::BTreeMap::new();
    for (index, label) in labels.into_iter().enumerate() {
        let name = format!("rivetMoveProbe{index}");
        probes.insert(line, name.clone());
        text.push_str(&format!("[{name}][{label}]\n\n"));
        line += label.bytes().filter(|b| *b == b'\n').count() + 2;
        if text.len() > 16 * 1024 * 1024 {
            return Err(Diagnostic::new(
                crate::ErrorCode::LimitExceeded,
                "reference definition scan exceeds 16 MiB",
            ));
        }
    }
    let arena = Arena::new();
    let root = parse_document(&arena, &text, &options(ParseMode::Document));
    for paragraph in root.children() {
        let data = paragraph.data.borrow();
        if !matches!(data.value, NodeValue::Paragraph) {
            continue;
        }
        let Some(name) = probes.get(&data.sourcepos.start.line) else {
            continue;
        };
        let children: Vec<_> = paragraph.children().collect();
        if let [link] = children.as_slice()
            && matches!(link.data.borrow().value, NodeValue::Link(_))
        {
            let content: Vec<_> = link.children().collect();
            if let [text] = content.as_slice()
                && matches!(&text.data.borrow().value,NodeValue::Text(t) if t.as_ref()==name)
            {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SourceBlock {
    pub kind: &'static str,
    pub span: Span,
    pub parent: Option<usize>,
}

/// The write dialect's block tree, including paragraphs inside lists and quotes.
/// Inline nodes are omitted so changing inline Markdown remains a source edit.
pub(crate) fn block_layout(doc: &Document) -> Vec<SourceBlock> {
    let arena = Arena::new();
    let root = parse_document(
        &arena,
        &doc.source[doc.body_start..],
        &options(ParseMode::Document),
    );
    let mut nodes = std::collections::HashMap::new();
    let mut blocks = vec![];
    for node in root.descendants().skip(1) {
        let data = node.data.borrow();
        if !data.value.block() {
            continue;
        }
        let p = data.sourcepos;
        let start = doc.lines[p.start.line - 1] + p.start.column - 1;
        let end = doc.lines[p.end.line - 1] + p.end.column;
        let parent = node
            .parent()
            .and_then(|parent| nodes.get(&std::ptr::from_ref(parent)).copied());
        nodes.insert(std::ptr::from_ref(node), blocks.len());
        blocks.push(SourceBlock {
            kind: data.value.xml_node_name(),
            span: Span { start, end },
            parent,
        });
    }
    blocks
}

#[derive(Default)]
struct PrefixCursor {
    byte: usize,
    column: usize,
    partial_tab: bool,
}

impl PrefixCursor {
    fn advance(&mut self, line: &[u8], mut columns: usize) {
        while columns > 0 && self.byte < line.len() {
            let width = if line[self.byte] == b'\t' {
                4 - self.column % 4
            } else {
                1
            };
            let taken = columns.min(width);
            self.column += taken;
            columns -= taken;
            self.partial_tab = taken < width;
            if self.partial_tab {
                break;
            }
            self.byte += 1;
        }
    }

    fn nonspace(&self, line: &[u8]) -> (usize, usize) {
        let (mut byte, mut column) = (self.byte, self.column);
        while let Some(ch) = line.get(byte) {
            match ch {
                b' ' => column += 1,
                b'\t' => column += 4 - column % 4,
                _ => break,
            }
            byte += 1;
        }
        (byte, column - self.column)
    }
}

/// Source segments belonging to a paragraph, excluding each physical line's
/// container prefixes. A paragraph extent alone includes those prefixes on
/// continuation lines. Item padding comes from comrak, including ordered lists
/// and tabs; a missing container prefix leaves a lazy continuation's bytes alone.
pub(crate) fn paragraph_source_spans(doc: &Document, paragraph: Span) -> Option<Vec<Span>> {
    let arena = Arena::new();
    let mut opts = options(ParseMode::Document);
    // Task expansion replaces Item's padding metadata. The CommonMark item
    // parse retains it; checkbox bytes stay outside the supplied paragraph span.
    opts.extension.tasklist = false;
    let root = parse_document(&arena, &doc.source[doc.body_start..], &opts);
    let node = root.descendants().find(|node| {
        let data = node.data.borrow();
        if !matches!(data.value, NodeValue::Paragraph) {
            return false;
        }
        let p = data.sourcepos;
        let start = doc.lines[p.start.line - 1] + p.start.column - 1;
        let end = doc.lines[p.end.line - 1] + p.end.column;
        start <= paragraph.start && end == paragraph.end
    })?;
    let p = node.data.borrow().sourcepos;
    let mut parents: Vec<_> = node.ancestors().skip(1).collect();
    parents.reverse();
    let mut spans = vec![];
    for line in p.start.line..=p.end.line {
        let start = doc.lines[line - 1];
        let end = doc.source[start..]
            .find(['\r', '\n'])
            .map_or(doc.source.len(), |n| start + n)
            .min(paragraph.end);
        let mut cursor = PrefixCursor::default();
        let bytes = &doc.source.as_bytes()[start..end];
        for parent in &parents {
            let data = parent.data.borrow();
            match &data.value {
                NodeValue::BlockQuote => {
                    let (nonspace, indent) = cursor.nonspace(bytes);
                    if indent > 3 || bytes.get(nonspace) != Some(&b'>') {
                        break;
                    }
                    cursor.advance(bytes, indent + 1);
                    if matches!(bytes.get(cursor.byte), Some(b' ' | b'\t')) {
                        cursor.advance(bytes, 1);
                    }
                }
                NodeValue::Item(item) => {
                    let width = item.marker_offset + item.padding;
                    if data.sourcepos.start.line != line && cursor.nonspace(bytes).1 < width {
                        break;
                    }
                    cursor.advance(bytes, width);
                }
                _ => {}
            }
        }
        let content_start = (start + cursor.byte + usize::from(cursor.partial_tab))
            .max(paragraph.start)
            .min(end);
        spans.push(Span {
            start: content_start,
            end,
        });
    }
    Some(spans)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Block {
    pub kind: String,
    /// Parser source extent; container prefixes inside this span are retained.
    pub span: Span,
    pub top_level: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Section {
    pub level: u8,
    pub heading: Span,
    pub body: Span,
    pub whole: Span,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Inspection {
    pub revision: String,
    pub blocks: Vec<Block>,
    pub sections: Vec<Section>,
    pub frontmatter: Vec<crate::yaml::Scalar>,
    pub diagnostics: Vec<Diagnostic>,
}

/// This spike reports source extents, not mutation-safe handles. No serializer
/// runs, and the caller retains the original bytes.
pub fn inspect(source: &str) -> Inspection {
    let mut result = Inspection {
        revision: revision("document", "", source.as_bytes(), &[]),
        blocks: vec![],
        sections: vec![],
        frontmatter: vec![],
        diagnostics: vec![],
    };
    if source.contains('\r') && source.replace("\r\n", "").contains('\r') {
        result
            .diagnostics
            .push(unsupported("lone CR line endings are unsupported", None));
        return result;
    }
    let bom = if source.starts_with('\u{feff}') { 3 } else { 0 };
    let frontmatter = crate::yaml::recognize(source, bom);
    match frontmatter.fields {
        Ok(fields) => result.frontmatter = fields,
        Err(error) => result.diagnostics.push(error),
    }
    let offset = frontmatter.span.map_or(bom, |s| s.end);
    let input = &source[offset..];
    let mut starts = vec![0];
    starts.extend(input.match_indices('\n').map(|(i, _)| i + 1));
    let arena = Arena::new();
    let options = options(ParseMode::Inspection);
    let root = parse_document(&arena, input, &options);
    for node in root.descendants().skip(1) {
        let data = node.data.borrow();
        if !matches!(
            data.value,
            NodeValue::Paragraph
                | NodeValue::Heading(_)
                | NodeValue::CodeBlock(_)
                | NodeValue::HtmlBlock(_)
                | NodeValue::BlockQuote
                | NodeValue::List(_)
                | NodeValue::Item(_)
                | NodeValue::TaskItem(_)
                | NodeValue::Table(_)
                | NodeValue::TableRow(_)
                | NodeValue::ThematicBreak
        ) {
            continue;
        }
        let p = data.sourcepos;
        let span = p
            .start
            .line
            .checked_sub(1)
            .and_then(|line| starts.get(line))
            .zip(p.end.line.checked_sub(1).and_then(|line| starts.get(line)))
            .and_then(|(a, b)| {
                Some(Span {
                    start: offset + a + p.start.column.checked_sub(1)?,
                    end: offset + b + p.end.column,
                })
            });
        let Some(mut span) = span.filter(|s| s.slice(source).is_some()) else {
            result.diagnostics.push(unsupported(
                format!("invalid {} parser extent", data.value.xml_node_name()),
                None,
            ));
            continue;
        };
        let top_level = node
            .parent()
            .is_some_and(|parent| std::ptr::eq(parent, root));
        if top_level {
            let line_start = offset + starts[p.start.line - 1];
            if source[line_start..span.start]
                .bytes()
                .all(|b| matches!(b, b' ' | b'\t'))
            {
                span.start = line_start;
            }
        }
        result.blocks.push(Block {
            kind: data.value.xml_node_name().into(),
            span,
            top_level,
        });
        if let NodeValue::Heading(ref h) = data.value
            && top_level
        {
            // Heading lines own their terminating newline; body starts after it.
            let end = source[span.end..]
                .find('\n')
                .map_or(source.len(), |n| span.end + n + 1);
            result.sections.push(Section {
                level: h.level,
                heading: span,
                body: Span {
                    start: end,
                    end: source.len(),
                },
                whole: Span {
                    start: span.start,
                    end: source.len(),
                },
            });
        }
    }
    for i in 0..result.sections.len() {
        if let Some(end) = result.sections[i + 1..]
            .iter()
            .find(|next| next.level <= result.sections[i].level)
            .map(|s| s.heading.start)
        {
            result.sections[i].body.end = end;
            result.sections[i].whole.end = end;
        }
    }
    result
}

/// Source extents of every code span and code block in `text`, as comrak
/// parses it: a span from its opening backtick to its closing one, a block
/// as whole lines.
pub(crate) fn code_extents(text: &str) -> Vec<Span> {
    let lines = markers::line_starts(text);
    let arena = Arena::new();
    let options = options(ParseMode::Document);
    let root = parse_document(&arena, text, &options);
    let at = |line: usize, column: usize| lines[line - 1] + column - 1;
    root.descendants()
        .filter_map(|n| {
            let data = n.data.borrow();
            let p = data.sourcepos;
            match data.value {
                NodeValue::Code(_) => Some(Span {
                    start: at(p.start.line, p.start.column),
                    end: at(p.end.line, p.end.column) + 1,
                }),
                // Whole lines, as comrak reports them. Where it reports a nested
                // indented block short, an opening past that end is refused,
                // which is the safe side of the mistake.
                NodeValue::CodeBlock(_) => Some(Span {
                    start: lines[p.start.line - 1],
                    end: lines.get(p.end.line).map_or(text.len(), |next| next - 1),
                }),
                _ => None,
            }
        })
        .collect()
}

/// Byte offsets of the opening backtick of each code span that starts on
/// 1-based `line` and ends on the line after it, as comrak parses the whole
/// source. A position that does not hold a backtick is left out rather than
/// trusted.
pub(crate) fn wrapped_code_spans(source: &str, line: usize) -> Vec<usize> {
    let lines = markers::line_starts(source);
    let arena = Arena::new();
    let options = options(ParseMode::Document);
    let root = parse_document(&arena, source, &options);
    root.descendants()
        .filter_map(|n| {
            let data = n.data.borrow();
            let p = data.sourcepos;
            if !matches!(data.value, NodeValue::Code(_))
                || p.start.line != line
                || p.end.line != line + 1
            {
                return None;
            }
            let at = lines[p.start.line - 1] + p.start.column - 1;
            (source.as_bytes().get(at) == Some(&b'`')).then_some(at)
        })
        .collect()
}

/// The page a document renders to, raw HTML kept and every run of
/// whitespace collapsed to one space: two sources that differ only in where
/// a line breaks inside a paragraph give the same string. Each stay marker
/// is reduced to its id, so a digest refreshed for the moved break does not
/// count as a change.
pub(crate) fn page(doc: &Document) -> String {
    let mut spans: Vec<(Span, Option<&String>)> = doc
        .markers
        .iter()
        .map(|m| (m.lexical.span, m.lexical.id.as_ref()))
        .collect();
    spans.sort_by_key(|(s, _)| s.start);
    let mut source = String::with_capacity(doc.source.len());
    let mut at = 0;
    for (span, id) in spans {
        source.push_str(&doc.source[at..span.start]);
        source.push_str(&markers::format_bare(id.map_or("", |i| i.as_str())));
        at = span.end;
    }
    source.push_str(&doc.source[at..]);
    let arena = Arena::new();
    let mut options = options(ParseMode::Document);
    options.render.r#unsafe = true;
    let root = parse_document(&arena, &source, &options);
    let mut html = String::new();
    comrak::format_html(root, &options, &mut html).expect("writing to a String");
    html.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The literal text of every code span, in order: whitespace inside a span
/// is content, which [`page`] collapses. comrak turns a line ending inside a
/// span into a space but keeps a CRLF's CR; a document holds no lone CR, so
/// dropping it gives the CommonMark reading.
pub(crate) fn code_texts(doc: &Document) -> Vec<String> {
    let arena = Arena::new();
    let options = options(ParseMode::Document);
    let root = parse_document(&arena, &doc.source, &options);
    root.descendants()
        .filter_map(|n| match &n.data.borrow().value {
            NodeValue::Code(code) => Some(code.literal.replace('\r', "")),
            _ => None,
        })
        .collect()
}

/// Parse a rendered item on its own and return (item count, carrier offset).
pub(crate) fn standalone_item(text: &str) -> (usize, Option<usize>) {
    let arena = Arena::new();
    let options = options(ParseMode::Document);
    let root = parse_document(&arena, text, &options);
    let mut starts = vec![0];
    starts.extend(text.match_indices('\n').map(|(i, _)| i + 1));
    let lists: Vec<_> = root.children().collect();
    let [list] = lists.as_slice() else {
        return (lists.len().max(2), None);
    };
    if !matches!(list.data.borrow().value, NodeValue::List(_)) {
        return (0, None);
    }
    let items: Vec<_> = list.children().collect();
    let carrier = items.first().and_then(|item| {
        item.children()
            .filter(|c| matches!(c.data.borrow().value, NodeValue::Paragraph))
            .last()
            .map(|p| {
                let e = p.data.borrow().sourcepos.end;
                let end = starts[e.line - 1] + e.column;
                starts[e.line - 1]
                    + text[starts[e.line - 1]..end]
                        .trim_end_matches([' ', '\t'])
                        .len()
            })
    });
    (items.len(), carrier)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inspection_keeps_tables_while_editing_uses_the_document_dialect() {
        let source = "| Key | Value |\n| --- | --- |\n| one | `two` |\n";
        let inspected = inspect(source);
        assert!(inspected.blocks.iter().any(|b| b.kind == "table"));
        let doc = Document::parse(source).unwrap();
        assert_eq!(doc.blocks.len(), 1);
        assert_eq!(doc.blocks[0].kind, "paragraph");
        assert!(!page(&doc).contains("<table"));
        assert_eq!(code_texts(&doc), ["two"]);
        let extent = code_extents(source);
        assert_eq!(extent.len(), 1);
        assert_eq!(&source[extent[0].start..extent[0].end], "`two`");
        let item = "- item\n\n  | Key | Value |\n  | --- | --- |\n  | one | two |";
        assert_eq!(standalone_item(item), (1, Some(item.len())));
    }
}
