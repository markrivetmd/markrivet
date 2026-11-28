//! Leading frontmatter recognition (markstay §5.3) and a deliberately narrow
//! top-level adapter. The scanner validates and decodes syntax; local lexical
//! boundaries preserve quotes and comments.
//!
//! Single-line scalars are writable. Empty values and collections of scalars
//! (flow or block, sequences or mappings, nested) are readable. A mapping's
//! immediate scalar fields can be rewritten through their exact source spans;
//! whole collections remain unavailable to `set`. The raw YAML writes copy a
//! caller's single-line scalar or flat flow-sequence token into one key line,
//! read through [`key_line_token`] rather than the read spans. An item append
//! locates its site in a sequence of flat mappings through [`sequence_tail`].
//! Anchors, aliases, tags, merges, block scalars, multiline scalars and duplicate keys at any depth
//! stay unsupported.
use crate::{Diagnostic, Span, contract::unsupported};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::BTreeSet;
use yaml_rust2::scanner::{Scanner, TScalarStyle, Token, TokenType};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Scalar {
    pub key: String,
    pub value: Value,
    /// Exact value bytes, quotes included.
    pub span: Span,
    /// The field's whole source lines, excluding the last terminator. A block
    /// list covers its key line and every item line.
    pub line: Span,
    pub style: String,
    /// A single-line scalar `set` can rewrite in place. Empty values and lists
    /// are read so a document is usable, and refused as `set` targets; the
    /// raw YAML writes decide their own eligibility through `key_line_token`.
    pub writable: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct MappingScalar {
    pub key: String,
    pub span: Span,
    pub style: String,
}

struct ParsedCollection {
    key: String,
    value: Value,
    span: Span,
    fields: Vec<MappingScalar>,
}

/// Locate an immediate, nonempty scalar field in a top-level mapping.
/// The same reader validates the entire collection and supplies the span.
pub(crate) fn mapping_field(text: &str, field: &str) -> Option<MappingScalar> {
    let parsed = parse_readonly(text)?;
    parsed.value.as_object()?;
    parsed.fields.into_iter().find(|s| s.key == field)
}

#[derive(Debug, Clone)]
pub struct Frontmatter {
    /// Opening delimiter through the closing delimiter's line terminator.
    pub span: Option<Span>,
    /// Payload lines strictly between the delimiters.
    pub payload: Option<Span>,
    pub fields: Result<Vec<Scalar>, Diagnostic>,
}

/// §5.3 recognition over LF-split lines (a CR before LF is ignored). The first
/// closing line fixes the span; if the payload is empty, holds a blank line, or
/// has no YAML-looking line, there is no frontmatter and nothing is rescanned.
/// A leading BOM is skipped, which markstay itself does not do.
pub fn recognize(source: &str, bom: usize) -> Frontmatter {
    let none = Frontmatter {
        span: None,
        payload: None,
        fields: Ok(vec![]),
    };
    let mut offset = bom;
    let mut lines = source[bom..].split_inclusive('\n').map(|raw| {
        let start = offset;
        offset += raw.len();
        (start, raw)
    });
    let Some((_, first)) = lines.next() else {
        return none;
    };
    if !is_fence(first, false) {
        return none;
    }
    let payload_start = bom + first.len();
    let mut payload_end = payload_start;
    for (start, raw) in lines {
        if is_fence(raw, true) {
            let payload = &source[payload_start..payload_end];
            let payload_lines: Vec<&str> = payload
                .split_inclusive('\n')
                .map(|l| l.trim_end_matches(['\n', '\r']))
                .collect();
            if payload_lines.is_empty()
                || payload_lines.iter().any(|l| l.bytes().all(ascii_ws))
                || !payload_lines.iter().any(|l| yamlish(l))
            {
                return none;
            }
            let span = Span {
                start: bom,
                end: start + raw.len(),
            };
            let payload = Span {
                start: payload_start,
                end: payload_end,
            };
            return Frontmatter {
                span: Some(span),
                payload: Some(payload),
                fields: scalars(&source[payload_start..payload_end], payload_start),
            };
        }
        payload_end = start + raw.len();
    }
    none
}

fn ascii_ws(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\x0b' | b'\x0c' | b'\r')
}

fn is_fence(raw: &str, closing: bool) -> bool {
    let line = raw.trim_end_matches('\n');
    let line = line.strip_suffix('\r').unwrap_or(line);
    let rest = line
        .strip_prefix("---")
        .or_else(|| closing.then(|| line.strip_prefix("...")).flatten());
    rest.is_some_and(|r| r.bytes().all(|b| matches!(b, b' ' | b'\t')))
}

/// A payload line that reads as YAML rather than prose (§5.3 condition 4).
pub(crate) fn yamlish(line: &str) -> bool {
    let rest = line.trim_start_matches([' ', '\t']);
    let token = |c: char| c > '\u{20}' && c != '\u{7f}';
    if let Some(after) = rest.strip_prefix('-') {
        let trimmed = after.trim_start_matches([' ', '\t']);
        if trimmed.len() < after.len() && trimmed.chars().next().is_some_and(token) {
            return true;
        }
    }
    let Some(first) = rest.chars().next() else {
        return false;
    };
    if !token(first) || first == ':' || first == '#' {
        return false;
    }
    rest.find(':').is_some_and(|i| {
        rest[i + 1..]
            .chars()
            .next()
            .is_none_or(|c| c == ' ' || c == '\t')
    })
}

/// One `key: TOKEN` line whose value is a single-line scalar or a flat flow
/// sequence of scalars, read for the raw YAML writes. `token` is the value's
/// exact bytes in the line, quotes and brackets included; anything after it
/// is horizontal whitespace and an optional comment.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct KeyLineToken {
    pub key: String,
    pub value: Value,
    pub token: Span,
}

/// Read one key line with the same scanner rules as the frontmatter reader:
/// scalars through `parse_line`, a flat flow sequence through `Reader`. The
/// line carries no terminator. Anything else (an empty value, a mapping, a
/// nested or multiline collection, a block form) is `None`.
pub(crate) fn key_line_token(line: &str) -> Option<KeyLineToken> {
    if line.contains(['\n', '\r']) {
        return None;
    }
    let token = match parse_line(line) {
        Some((key, value, token, _)) => KeyLineToken { key, value, token },
        None => flow_sequence_line(line)?,
    };
    trailing_comment(&line[token.token.end..]).then_some(token)
}

/// Only horizontal whitespace, then optionally a comment YAML recognises,
/// which needs whitespace before its `#`.
fn trailing_comment(suffix: &str) -> bool {
    let rest = suffix.trim_start_matches([' ', '\t']);
    rest.is_empty() || (rest.starts_with('#') && rest.len() < suffix.len())
}

/// `key: [scalar, ...]` on one line. The token's extent comes from the
/// scanner's own bracket tokens; their columns count characters, so each is
/// converted to a byte offset and checked against the source character.
fn flow_sequence_line(line: &str) -> Option<KeyLineToken> {
    if line.starts_with([' ', '\t']) || line.chars().any(invalid_unquoted) {
        return None;
    }
    let mut scanner = Scanner::new(line.chars());
    let mut tokens = vec![];
    while let Some(token) = scanner.next_token().ok()? {
        tokens.push(token);
    }
    let kinds: Vec<&TokenType> = tokens.iter().map(|t| &t.1).collect();
    let [
        TokenType::StreamStart(_),
        TokenType::BlockMappingStart,
        TokenType::Key,
        TokenType::Scalar(key_style, key),
        TokenType::Value,
        TokenType::FlowSequenceStart,
        ..,
    ] = kinds.as_slice()
    else {
        return None;
    };
    if !string_key(key_style, key) {
        return None;
    }
    let mut reader = Reader {
        tokens: &tokens,
        at: 5,
        lines: vec![(0, line)],
        fields: vec![],
    };
    let value = reader.node(0)?;
    let Value::Array(items) = &value else {
        return None;
    };
    if items.iter().any(|v| v.is_array() || v.is_object())
        || !matches!(
            &kinds[reader.at..],
            [TokenType::BlockEnd, TokenType::StreamEnd]
        )
    {
        return None;
    }
    let at = |mark: &yaml_rust2::scanner::Marker, want: char| {
        if mark.line() != 1 {
            return None;
        }
        let (i, c) = line.char_indices().nth(mark.col())?;
        (c == want).then_some(i)
    };
    let start = at(&tokens[5].0, '[')?;
    let end = at(&tokens[reader.at - 1].0, ']')? + 1;
    // Nothing but the separator's whitespace lies between the key and `[`.
    let head = line[..start].trim_end_matches([' ', '\t']);
    if !head.ends_with(':') || head.len() == start {
        return None;
    }
    Some(KeyLineToken {
        key: key.clone(),
        value,
        token: Span { start, end },
    })
}

/// A caller's raw YAML value token: one nonempty physical-line scalar or flat
/// flow sequence of scalars, with no surrounding whitespace, comment or
/// trailing bytes. It is read as the value of a synthetic `k: ` line by the
/// same rules as an existing key line, and must be consumed exactly.
pub(crate) fn raw_token(raw: &str) -> Option<Value> {
    if raw.is_empty()
        || raw.starts_with(char::is_whitespace)
        || raw.ends_with(char::is_whitespace)
        || raw
            .chars()
            .any(|c| c.is_control() || matches!(c, '\u{2028}' | '\u{2029}'))
    {
        return None;
    }
    let line = format!("k: {raw}");
    let read = key_line_token(&line)?;
    (read.token
        == Span {
            start: 3,
            end: line.len(),
        })
    .then_some(read.value)
}

/// Read a frontmatter payload exactly as `recognize` does, for checking a
/// candidate payload before it is planned.
pub(crate) fn read_payload(payload: &str, base: usize) -> Result<Vec<Scalar>, Diagnostic> {
    scalars(payload, base)
}

fn scalars(payload: &str, base: usize) -> Result<Vec<Scalar>, Diagnostic> {
    if payload
        .chars()
        .any(|c| c < ' ' && !matches!(c, '\n' | '\r' | '\t'))
    {
        return Err(unsupported(
            "raw control characters are unsupported in frontmatter",
            Some(Span {
                start: base,
                end: base + payload.len(),
            }),
        ));
    }
    // (start offset, raw line with terminator, line without it)
    let raws: Vec<(usize, &str, &str)> = {
        let mut offset = base;
        payload
            .split_inclusive('\n')
            .map(|raw| {
                let at = offset;
                offset += raw.len();
                (at, raw, raw.trim_end_matches(['\r', '\n']))
            })
            .collect()
    };
    let mut fields = vec![];
    let mut keys = BTreeSet::new();
    let mut i = 0;
    while i < raws.len() {
        let (offset, _, line) = raws[i];
        let whole = Span {
            start: offset,
            end: offset + line.len(),
        };
        let fail = move || {
            unsupported(
                "frontmatter supports only unique keys whose values are single-line \
                 JSON-compatible scalars, or lists and mappings of them",
                Some(whole),
            )
        };
        if line.trim_matches([' ', '\t']).is_empty()
            || line.trim_start_matches([' ', '\t']).starts_with('#')
        {
            if line.chars().any(invalid_unquoted) {
                return Err(fail());
            }
            i += 1;
            continue;
        }
        // A `key: scalar` line is a whole field, so nothing after it belongs
        // to it: an indented comment under one is a comment, not a value.
        // Only a key YAML left open takes continuation lines, and a
        // continuation is a block sequence entry or an indented line.
        let single = parse_line(line);
        let mut last = i;
        while single.is_none() && last + 1 < raws.len() {
            let next = raws[last + 1].2;
            let indent = &next[..next.len() - next.trim_start_matches([' ', '\t']).len()];
            // YAML forbids a tab in block indentation, and the scanner does
            // not catch one here, so refuse rather than read past it.
            if indent.contains('\t') {
                break;
            }
            let rest = next.trim_start_matches(' ');
            let entry = rest
                .strip_prefix('-')
                .is_some_and(|r| r.is_empty() || r.starts_with([' ', '\t']));
            // A comment between entries belongs to the field, but only once
            // the field has actually opened one.
            let comment = last > i && rest.starts_with('#');
            if entry || comment || next.starts_with(' ') {
                last += 1;
            } else {
                break;
            }
        }
        let field = Span {
            start: offset,
            end: raws[last].0 + raws[last].2.len(),
        };
        let text = &payload[field.start - base..field.end - base];
        let (key, value, span, style, writable) = match single {
            Some((k, v, s, y)) => (k, v, s, y, true),
            None => parse_readonly(text)
                .map(|p| (p.key, p.value, p.span, String::new(), false))
                .ok_or_else(fail)?,
        };
        // Named, because the fix is an edit to the file: which of the two
        // values stands is the author's call, not a reader's.
        if !keys.insert(key.clone()) {
            return Err(unsupported(
                format!("frontmatter key {key:?} appears more than once; remove one"),
                Some(field),
            ));
        }
        fields.push(Scalar {
            key,
            value,
            span: Span {
                start: field.start + span.start,
                end: field.start + span.end,
            },
            line: field,
            style,
            writable,
        });
        i = last + 1;
    }
    Ok(fields)
}

/// Read a collection or empty value, retaining immediate mapping scalar spans.
fn parse_readonly(text: &str) -> Option<ParsedCollection> {
    if text.starts_with([' ', '\t']) || text.chars().any(invalid_unquoted) {
        return None;
    }
    let mut scanner = Scanner::new(text.chars());
    let mut tokens = vec![];
    while let Some(token) = scanner.next_token().ok()? {
        tokens.push(token);
    }
    let kinds: Vec<&TokenType> = tokens.iter().map(|t| &t.1).collect();
    let [
        TokenType::StreamStart(_),
        TokenType::BlockMappingStart,
        TokenType::Key,
        TokenType::Scalar(key_style, key),
        TokenType::Value,
        ..,
    ] = kinds.as_slice()
    else {
        return None;
    };
    if !string_key(key_style, key) {
        return None;
    }
    let mut reader = Reader {
        tokens: &tokens,
        at: 5,
        lines: text
            .split('\n')
            .scan(0, |offset, line| {
                let entry = (*offset, line);
                *offset += line.len() + 1;
                Some(entry)
            })
            .collect(),
        fields: vec![],
    };
    let value = if reader.empty_value(false) {
        Value::Null
    } else {
        // A scalar value is a single line's, and `parse_line` already refused it.
        match reader.node(0)? {
            v @ (Value::Array(_) | Value::Object(_)) => v,
            _ => return None,
        }
    };
    if !matches!(
        &kinds[reader.at..],
        [TokenType::BlockEnd, TokenType::StreamEnd]
    ) {
        return None;
    }
    // The value starts after the separator, which a quoted key can contain.
    let first_line = reader.lines[0].1;
    let key_end = if first_line.starts_with(['\'', '"']) {
        quoted_end(first_line)?
    } else {
        0
    };
    let colon = key_end + first_line[key_end..].find(':')?;
    let after = colon + 1;
    let start = after + first_line[after..].len() - first_line[after..].trim_start().len();
    let span = if value.is_null() {
        // `key:` with nothing after it.
        Span {
            start,
            end: first_line.trim_end().len().max(start),
        }
    } else if tokens[5].0.line() == 1 {
        // A flow collection opening on the key's line. Its closing bracket can
        // sit on a later line, or a bracket inside a comment on this one, so
        // the extent is the whole field.
        Span {
            start,
            end: text.trim_end().len(),
        }
    } else {
        Span {
            start: (first_line.len() + 1).min(text.len()),
            end: text.len(),
        }
    };
    Some(ParsedCollection {
        key: key.clone(),
        value,
        span,
        fields: reader.fields,
    })
}

/// Deepest collection nesting read. Tracker frontmatter uses two levels; this
/// bounds recursion on hostile input rather than describing a real shape.
const MAX_DEPTH: usize = 16;

/// Recursive descent over scanner tokens, reading only what JSON can hold
/// without a choice: no anchors, aliases, tags, merge keys, complex keys,
/// block scalars, multiline scalars or duplicate keys at any depth.
struct Reader<'a> {
    tokens: &'a [Token],
    at: usize,
    lines: Vec<(usize, &'a str)>,
    fields: Vec<MappingScalar>,
}

impl Reader<'_> {
    fn peek(&self) -> Option<&TokenType> {
        self.tokens.get(self.at).map(|t| &t.1)
    }

    /// A value position with nothing in it, which YAML reads as null. After a
    /// key, a `-` opens a sequence at the key's indentation; only after a
    /// sequence entry does the next `-` mean this entry was empty.
    fn empty_value(&self, in_sequence: bool) -> bool {
        match self.peek() {
            Some(TokenType::BlockEntry) => in_sequence,
            Some(
                TokenType::Key
                | TokenType::BlockEnd
                | TokenType::FlowEntry
                | TokenType::FlowMappingEnd
                | TokenType::StreamEnd,
            ) => true,
            _ => false,
        }
    }

    fn expect(&mut self, want: &TokenType) -> Option<()> {
        (self.peek()? == want).then(|| self.at += 1)
    }

    fn node(&mut self, depth: usize) -> Option<Value> {
        if depth > MAX_DEPTH {
            return None;
        }
        match self.peek()?.clone() {
            TokenType::Scalar(style, decoded) => {
                self.single_line(&style)?;
                self.at += 1;
                decode(&style, &decoded)
            }
            TokenType::FlowSequenceStart => {
                self.at += 1;
                let mut values = vec![];
                loop {
                    if self.peek()? == &TokenType::FlowSequenceEnd {
                        self.at += 1;
                        return Some(Value::Array(values));
                    }
                    // Entry (`,` entry)* with an optional trailing `,`, so
                    // `[a,,b]` and `[,]` are refused rather than read short.
                    values.push(self.node(depth + 1)?);
                    match self.peek()? {
                        TokenType::FlowEntry => self.at += 1,
                        TokenType::FlowSequenceEnd => {}
                        _ => return None,
                    }
                }
            }
            TokenType::FlowMappingStart => {
                self.at += 1;
                let mut map = Map::new();
                loop {
                    if self.peek()? == &TokenType::FlowMappingEnd {
                        self.at += 1;
                        return Some(Value::Object(map));
                    }
                    self.entry(&mut map, depth)?;
                    match self.peek()? {
                        TokenType::FlowEntry => self.at += 1,
                        TokenType::FlowMappingEnd => {}
                        _ => return None,
                    }
                }
            }
            TokenType::BlockMappingStart => {
                self.at += 1;
                let mut map = Map::new();
                while self.peek()? != &TokenType::BlockEnd {
                    self.entry(&mut map, depth)?;
                }
                self.at += 1;
                Some(Value::Object(map))
            }
            TokenType::BlockSequenceStart => {
                self.at += 1;
                let values = self.entries(depth)?;
                self.expect(&TokenType::BlockEnd)?;
                Some(Value::Array(values))
            }
            // A sequence at its parent key's indentation has no start or end
            // token: it runs while entries do.
            TokenType::BlockEntry => Some(Value::Array(self.entries(depth)?)),
            _ => None,
        }
    }

    fn entries(&mut self, depth: usize) -> Option<Vec<Value>> {
        let mut values = vec![];
        while self.peek()? == &TokenType::BlockEntry {
            self.at += 1;
            values.push(if self.empty_value(true) {
                Value::Null
            } else {
                self.node(depth + 1)?
            });
        }
        Some(values)
    }

    /// `Key Scalar Value node?` into `map`, refusing a key it already holds.
    fn entry(&mut self, map: &mut Map<String, Value>, depth: usize) -> Option<()> {
        self.expect(&TokenType::Key)?;
        let TokenType::Scalar(style, key) = self.peek()?.clone() else {
            return None;
        };
        if !string_key(&style, &key) {
            return None;
        }
        self.single_line(&style)?;
        self.at += 1;
        self.expect(&TokenType::Value)?;
        let value = if self.empty_value(false) {
            Value::Null
        } else {
            if depth == 0
                && let TokenType::Scalar(style, _) = self.peek()?.clone()
            {
                self.fields.push(MappingScalar {
                    key: key.clone(),
                    span: self.scalar_span(&style)?,
                    style: format!("{style:?}"),
                });
            }
            self.node(depth + 1)?
        };
        map.insert(key, value).is_none().then_some(())
    }

    /// Validate a scalar against its source line, including quoted delimiters.
    fn single_line(&self, style: &TScalarStyle) -> Option<()> {
        self.scalar_span(style).map(|_| ())
    }

    /// Scanner columns count characters; all edit extents count UTF-8 bytes.
    fn scalar_span(&self, style: &TScalarStyle) -> Option<Span> {
        let marker = &self.tokens[self.at].0;
        let line_index = marker.line().checked_sub(1)?;
        let (line_start, line) = *self.lines.get(line_index)?;
        let line = line.strip_suffix('\r').unwrap_or(line);
        let from = line
            .char_indices()
            .nth(marker.col())
            .map_or(line.len(), |(i, _)| i);
        let rest = &line[from..];
        let TokenType::Scalar(_, decoded) = &self.tokens[self.at].1 else {
            return None;
        };
        let len = match style {
            TScalarStyle::Plain if rest.starts_with(decoded.as_str()) => decoded.len(),
            TScalarStyle::SingleQuoted | TScalarStyle::DoubleQuoted => quoted_end(rest)?,
            _ => return None,
        };
        let start = line_start + from;
        Some(Span {
            start,
            end: start + len,
        })
    }
}

/// A key YAML reads as a string, and not the merge key. A quoted key always
/// is. A plain one must decode as text on the same terms as a value: `32`,
/// `0x20`, `true`, `null` and `~` resolve to other types, so keys JSON would
/// hold apart (`0x20` and `32`) can be one duplicated key to YAML.
fn string_key(style: &TScalarStyle, key: &str) -> bool {
    match style {
        TScalarStyle::Plain => key != "<<" && matches!(plain_value(key), Some(Value::String(_))),
        TScalarStyle::SingleQuoted | TScalarStyle::DoubleQuoted => true,
        _ => false,
    }
}

/// A scanned scalar as JSON, on the same terms as a single-line value: quoted
/// text is a string, a plain token must be JSON-compatible, and a YAML-only
/// spelling of a keyword or number is refused rather than coerced.
fn decode(style: &TScalarStyle, decoded: &str) -> Option<Value> {
    match style {
        TScalarStyle::Literal | TScalarStyle::Folded => None,
        TScalarStyle::SingleQuoted | TScalarStyle::DoubleQuoted => {
            Some(Value::String(decoded.to_string()))
        }
        _ => plain_value(decoded),
    }
}

/// Parse one `key: scalar` line into (key, value, value extent, style).
fn parse_line(line: &str) -> Option<(String, Value, Span, String)> {
    if line.starts_with([' ', '\t']) {
        return None;
    }
    let mut scanner = Scanner::new(line.chars());
    let mut tokens = vec![];
    while let Some(token) = scanner.next_token().ok()? {
        tokens.push(token.1);
    }
    let [
        TokenType::StreamStart(_),
        TokenType::BlockMappingStart,
        TokenType::Key,
        TokenType::Scalar(key_style, key),
        TokenType::Value,
        TokenType::Scalar(style, decoded),
        TokenType::BlockEnd,
        TokenType::StreamEnd,
    ] = tokens.as_slice()
    else {
        return None;
    };
    if matches!(style, TScalarStyle::Literal | TScalarStyle::Folded) || !string_key(key_style, key)
    {
        return None;
    }
    // Locate the separator outside quoted keys; then scan the exact scalar.
    let key_end = if line.starts_with(['\'', '"']) {
        quoted_end(line)?
    } else {
        line.bytes()
            .enumerate()
            .find(|(i, b)| {
                *b == b':'
                    && line
                        .as_bytes()
                        .get(i + 1)
                        .is_none_or(|next| matches!(next, b' ' | b'\t'))
            })
            .map(|(i, _)| i)?
    };
    let colon = key_end + line[key_end..].find(':')?;
    let start = colon + 1 + line[colon + 1..].len()
        - line[colon + 1..].trim_start_matches([' ', '\t']).len();
    let tail = &line[start..];
    let quoted = matches!(
        style,
        TScalarStyle::SingleQuoted | TScalarStyle::DoubleQuoted
    );
    let len = if quoted {
        quoted_end(tail)?
    } else {
        tail.char_indices()
            .find(|(i, c)| *c == '#' && (*i == 0 || tail.as_bytes()[i - 1].is_ascii_whitespace()))
            .map_or(tail.len(), |(i, _)| i)
    };
    let value_end = start + tail[..len].trim_end_matches([' ', '\t']).len();
    // YAML's nb-char excludes BOM/non-printable characters. Quoted scalars
    // use nb-json instead, allowing every non-C0 Unicode character.
    for (i, c) in line.char_indices() {
        let in_quoted_key = line.starts_with(['\'', '"']) && i < key_end;
        let in_quoted_value = quoted && (start..value_end).contains(&i);
        if invalid_unquoted(c) && !in_quoted_key && !in_quoted_value {
            return None;
        }
    }
    let value = if quoted {
        Value::String(decoded.clone())
    } else {
        plain_value(decoded)?
    };
    Some((
        key.clone(),
        value,
        Span {
            start,
            end: value_end,
        },
        format!("{style:?}"),
    ))
}

/// A plain (unquoted) scalar as JSON. A token that starts like a number must
/// be a JSON number or nothing: YAML's own spellings (`0x20`, `01`, `1_0`,
/// `.nan`, `1:30`) are refused rather than coerced to a value JSON would read
/// differently. A token that starts that way but carries a character no YAML
/// number can hold is ordinary text, which is what `1Password` and
/// `../plans/PLAN_X.md` are.
fn plain_value(text: &str) -> Option<Value> {
    match text {
        d if crate::schema::valid_date(d) => Some(Value::String(d.to_string())),
        "null" | "true" | "false" => serde_json::from_str(text).ok(),
        "~" | "Null" | "NULL" | "True" | "TRUE" | "False" | "FALSE" => None,
        s if s.starts_with(|c: char| c.is_ascii_digit() || matches!(c, '+' | '-' | '.')) => {
            match serde_json::from_str::<Value>(s) {
                Ok(v @ Value::Number(_)) => Some(v),
                _ if yaml_only_number(s) => None,
                _ => Some(Value::String(s.to_string())),
            }
        }
        s => Some(Value::String(s.to_string())),
    }
}

/// Could any YAML version read this token as a number? A plain scalar that
/// starts like one is refused when so, and read as text when not, so nothing
/// is coerced in either direction.
///
/// **The test is deliberately one-sided.** Refusing a string costs a caller a
/// diagnostic; accepting a number as a writable string is unsafe, because a
/// later `set` of that same string renders it unquoted and YAML then reads a
/// number where Rivet wrote text. So the cases below are drawn wider than any
/// single YAML version: 1.0 separates digits with `,`, 1.1 and 1.2 disagree
/// about `0o`, about case and sign on `0X20` and `+.nan`, and about `_`
/// placement, and this refuses on any reading. A token that merely starts
/// with a digit is still text:
/// `1Password`, `6.9.6`, `../plans/X.md` and `2026-13-45` are strings.
fn yaml_only_number(s: &str) -> bool {
    let body = s.strip_prefix(['+', '-']).unwrap_or(s);
    let low = body.to_ascii_lowercase();
    if low == ".inf" || low == ".nan" {
        return true;
    }
    for (prefix, radix) in [("0x", 16), ("0o", 8), ("0b", 2)] {
        if let Some(rest) = low.strip_prefix(prefix) {
            return !rest.is_empty() && rest.chars().all(|c| c.is_digit(radix) || separator(c));
        }
    }
    // Sexagesimal: a decimal head, then `:` groups of at most two digits under
    // 60. YAML 1.1's float form lets the last group carry a fraction, so
    // `1:30.5` is 90.5 and must not read back as text.
    if let Some((head, rest)) = body.split_once(':') {
        let groups: Vec<&str> = rest.split(':').collect();
        let Some((last, rest)) = groups.split_last() else {
            return false;
        };
        let group = |g: &str| {
            (1..=2).contains(&g.len())
                && g.bytes().all(|b| b.is_ascii_digit())
                && g.parse::<u8>().is_ok_and(|v| v < 60)
        };
        let (last_int, last_frac) = last.split_once('.').unwrap_or((last, ""));
        return yaml_decimal(head)
            && rest.iter().all(|g| group(g))
            && group(last_int)
            && last_frac
                .chars()
                .all(|c| c.is_ascii_digit() || separator(c));
    }
    yaml_decimal(body)
}

/// A digit separator: `_` in YAML 1.1 and 1.2, `,` in YAML 1.0.
fn separator(c: char) -> bool {
    matches!(c, '_' | ',')
}

/// A YAML integer or float, allowing the separators and the bare leading
/// or trailing `.` that JSON refuses. A token that opens with `_` is not one:
/// YAML's integer starts at a digit.
fn yaml_decimal(t: &str) -> bool {
    let (mantissa, exponent) = match t.split_once(['e', 'E']) {
        Some((m, e)) => (m, Some(e)),
        None => (t, None),
    };
    if let Some(e) = exponent {
        let e = e.strip_prefix(['+', '-']).unwrap_or(e);
        if e.is_empty() || !e.chars().all(|c| c.is_ascii_digit()) {
            return false;
        }
    }
    if mantissa.starts_with(separator) {
        return false;
    }
    let part = |p: &str| p.chars().all(|c| c.is_ascii_digit() || separator(c));
    let (int, frac) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    part(int) && part(frac) && (int.chars().chain(frac.chars())).any(|c| c.is_ascii_digit())
}

fn invalid_unquoted(c: char) -> bool {
    matches!(c, '\u{7f}'..='\u{84}' | '\u{86}'..='\u{9f}' | '\u{feff}' | '\u{fffe}' | '\u{ffff}')
}

fn quoted_end(s: &str) -> Option<usize> {
    let quote = *s.as_bytes().first()?;
    let bytes = s.as_bytes();
    let mut i = 1;
    while i < bytes.len() {
        if quote == b'"' && bytes[i] == b'\\' {
            i += 2;
            continue;
        }
        if bytes[i] == quote {
            if quote == b'\'' && bytes.get(i + 1) == Some(&quote) {
                i += 2;
                continue;
            }
            return Some(i + 1);
        }
        i += 1;
    }
    None
}

/// Render a JSON scalar as a YAML value for `key`, keeping the old quoting
/// style when it still decodes to the same value. The result is re-parsed;
/// anything that does not round-trip exactly is refused.
pub fn render_scalar(key_line: &str, old: Span, style: &str, value: &Value) -> Option<String> {
    render_scalar_with(style, value, |text| {
        let line = format!("{}{}{}", &key_line[..old.start], text, &key_line[old.end..]);
        parse_line(&line).is_some_and(|(_, v, span, _)| {
            &v == value && span.start == old.start && span.end == old.start + text.len()
        })
    })
}

/// Render in the original mapping context: flow punctuation in a string must
/// be quoted even where the same text would be plain in a top-level scalar.
pub(crate) fn render_mapping_field(
    text: &str,
    field: &MappingScalar,
    value: &Value,
    expected: &Value,
) -> Option<String> {
    render_scalar_with(&field.style, value, |replacement| {
        let candidate = format!(
            "{}{}{}",
            &text[..field.span.start],
            replacement,
            &text[field.span.end..]
        );
        parse_readonly(&candidate).is_some_and(|p| {
            &p.value == expected
                && p.fields.iter().any(|s| {
                    s.key == field.key
                        && s.span.start == field.span.start
                        && s.span.end == field.span.start + replacement.len()
                })
        })
    })
}

/// A flat flow sequence of strings, `[a, b]`: each element rendered by the
/// candidates `set` tries (plain where it reads back inside the brackets,
/// else quoted), and the whole token read back through `raw_token`.
pub(crate) fn render_flow_list(items: &[Value]) -> Option<String> {
    let rendered = items
        .iter()
        .map(|v| {
            v.as_str()?;
            let one = Value::Array(vec![v.clone()]);
            render_scalar_with("Plain", v, |t| {
                raw_token(&format!("[{t}]")) == Some(one.clone())
            })
        })
        .collect::<Option<Vec<_>>>()?;
    let token = format!("[{}]", rendered.join(", "));
    (raw_token(&token) == Some(Value::Array(items.to_vec()))).then_some(token)
}

/// One field's source lines read as a mapping or sequence, by the reader's
/// own rules: the value it decodes to, or `None` when it does not read.
pub(crate) fn collection_value(text: &str) -> Option<Value> {
    parse_readonly(text).map(|p| p.value)
}

/// A plain key YAML reads as the same text, by the reader's own key rule.
pub(crate) fn plain_key(key: &str) -> bool {
    string_key(&TScalarStyle::Plain, key)
}

/// Where one more flat mapping goes in a top-level sequence field, read from
/// the field's own scanner tokens.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum SequenceForm {
    /// A flow sequence on the key's line: the item goes at byte `at` of the
    /// field text, after `sep` (empty when the sequence is empty).
    Flow { at: usize, sep: String },
    /// Block entries: the new item's first line opens with `prefix` (its
    /// indentation, `-` and the spaces after it). A block mapping item puts
    /// each later field on its own line, indented to the prefix's width.
    Block { prefix: String, mapping: bool },
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SequenceTail {
    pub form: SequenceForm,
    /// The last item's keys with their value's quote style, in source order.
    pub fields: Vec<(String, Option<String>)>,
}

/// Read a field holding a sequence of mappings for an append. `None` for
/// anything else the reader accepts: a scalar, a mapping, an empty value, a
/// flow sequence that leaves the key's line, or a block sequence any of whose
/// entries has a mapping that does not open on its `-` line, or a tab after
/// its `-`. The last entry's layout is the one an append repeats.
pub(crate) fn sequence_tail(text: &str) -> Option<SequenceTail> {
    let parsed = parse_readonly(text)?;
    let Value::Array(items) = &parsed.value else {
        return None;
    };
    if items.iter().any(|i| !i.is_object()) {
        return None;
    }
    let mut scanner = Scanner::new(text.chars());
    let mut tokens = vec![];
    while let Some(token) = scanner.next_token().ok()? {
        tokens.push(token);
    }
    // parse_readonly checked the stream opens with `key:` and closes with
    // the mapping's BlockEnd and StreamEnd.
    let value = &tokens[5..tokens.len().checked_sub(2)?];
    let mut lines = vec![];
    let mut offset = 0;
    for line in text.split('\n') {
        lines.push((offset, line.strip_suffix('\r').unwrap_or(line)));
        offset += line.len() + 1;
    }
    let at = |mark: &yaml_rust2::scanner::Marker, want: char| {
        let (start, line) = *lines.get(mark.line().checked_sub(1)?)?;
        let (i, c) = line.char_indices().nth(mark.col())?;
        (c == want).then_some(start + i)
    };
    fn kind(t: &Token) -> &TokenType {
        &t.1
    }
    match value.first().map(kind)? {
        TokenType::FlowSequenceStart => {
            let open = &value[0].0;
            let close = &value.last()?.0;
            if open.line() != 1 || close.line() != 1 {
                return None;
            }
            // Top-level items: (start, end) token indices of each `{...}`.
            let mut spans = vec![];
            let mut depth = 0usize;
            for (i, t) in value.iter().enumerate().skip(1).take(value.len() - 2) {
                match kind(t) {
                    TokenType::FlowMappingStart => {
                        if depth == 0 {
                            spans.push((i, i));
                        }
                        depth += 1;
                    }
                    TokenType::FlowMappingEnd => {
                        depth = depth.checked_sub(1)?;
                        if depth == 0 {
                            spans.last_mut()?.1 = i;
                        }
                    }
                    TokenType::FlowSequenceStart | TokenType::FlowSequenceEnd => return None,
                    TokenType::FlowEntry => {}
                    _ if depth == 0 => return None,
                    _ => {}
                }
            }
            let Some(&(start, end)) = spans.last() else {
                return Some(SequenceTail {
                    form: SequenceForm::Flow {
                        at: at(open, '[')? + 1,
                        sep: String::new(),
                    },
                    fields: vec![],
                });
            };
            let after = at(&value[end].0, '}')? + 1;
            let sep = match spans.len() {
                1 => ", ".to_string(),
                n => text[at(&value[spans[n - 2].1].0, '}')? + 1..at(&value[start].0, '{')?]
                    .to_string(),
            };
            Some(SequenceTail {
                form: SequenceForm::Flow { at: after, sep },
                fields: item_styles(&value[start + 1..end]),
            })
        }
        TokenType::BlockSequenceStart | TokenType::BlockEntry => {
            let inner = if kind(&value[0]) == &TokenType::BlockSequenceStart {
                if kind(value.last()?) != &TokenType::BlockEnd {
                    return None;
                }
                &value[1..value.len() - 1]
            } else {
                value
            };
            let mut entries = vec![];
            let mut depth = 0usize;
            for (i, t) in inner.iter().enumerate() {
                match kind(t) {
                    TokenType::BlockEntry if depth == 0 => entries.push(i),
                    TokenType::BlockMappingStart
                    | TokenType::BlockSequenceStart
                    | TokenType::FlowMappingStart
                    | TokenType::FlowSequenceStart => depth += 1,
                    TokenType::BlockEnd
                    | TokenType::FlowMappingEnd
                    | TokenType::FlowSequenceEnd => depth = depth.checked_sub(1)?,
                    _ if depth == 0 => return None,
                    _ => {}
                }
            }
            // Every entry must have a layout the new item could repeat; the
            // last one's is repeated. Returns (prefix, block mapping, item).
            let layout = |n: usize| -> Option<(&str, bool, &[Token])> {
                let entry = entries[n];
                let item = &inner[entry + 1..entries.get(n + 1).copied().unwrap_or(inner.len())];
                let dash = &inner[entry].0;
                let (mapping, first) = match item.first().map(kind)? {
                    TokenType::FlowMappingStart => {
                        let close = item
                            .iter()
                            .position(|t| kind(t) == &TokenType::FlowMappingEnd)?;
                        if item[close].0.line() != dash.line() {
                            return None;
                        }
                        (false, at(&item[0].0, '{')?)
                    }
                    TokenType::BlockMappingStart => {
                        let key = item.get(2)?;
                        let TokenType::Scalar(style, name) = kind(key) else {
                            return None;
                        };
                        let want = match style {
                            TScalarStyle::SingleQuoted => '\'',
                            TScalarStyle::DoubleQuoted => '"',
                            _ => name.chars().next()?,
                        };
                        if key.0.line() != dash.line() {
                            return None;
                        }
                        (true, at(&key.0, want)?)
                    }
                    _ => return None,
                };
                let line_start = lines.get(dash.line().checked_sub(1)?)?.0;
                let prefix = &text[line_start..first];
                let marker = prefix.trim_start_matches(' ');
                let gap = marker.strip_prefix('-')?;
                if gap.is_empty() || gap.bytes().any(|b| b != b' ') {
                    return None;
                }
                Some((prefix, mapping, item))
            };
            let mut last = None;
            for n in 0..entries.len() {
                last = Some(layout(n)?);
            }
            let (prefix, mapping, item) = last?;
            Some(SequenceTail {
                form: SequenceForm::Block {
                    prefix: prefix.to_string(),
                    mapping,
                },
                fields: item_styles(item),
            })
        }
        _ => None,
    }
}

/// Each `Key Scalar Value` run directly inside one mapping item's tokens, as
/// (key, value style), in source order. A field with an empty value has no
/// style but keeps its place.
fn item_styles(tokens: &[Token]) -> Vec<(String, Option<String>)> {
    tokens
        .windows(3)
        .enumerate()
        .filter_map(|(i, w)| match [&w[0].1, &w[1].1, &w[2].1] {
            [TokenType::Key, TokenType::Scalar(_, key), TokenType::Value] => {
                let style = match tokens.get(i + 3).map(|t| &t.1) {
                    Some(TokenType::Scalar(style, _)) => Some(format!("{style:?}")),
                    _ => None,
                };
                Some((key.clone(), style))
            }
            _ => None,
        })
        .collect()
}

/// Render each field of a new sequence item as `(key, value text)`, in the
/// order of `styles` (the previous item's keys) and then the item's own. Each
/// value is rendered by the same candidates and mapping-context check as
/// `set_key_field`, in a one-field mapping of the item's own form, keeping
/// the previous item's quote style for its key where that still reads back.
pub(crate) fn render_item(
    item: &Map<String, Value>,
    styles: &[(String, Option<String>)],
    block: bool,
) -> Option<Vec<(String, String)>> {
    let seen = |k: &String| styles.iter().any(|(s, _)| s == k);
    styles
        .iter()
        .map(|(k, _)| k)
        .filter(|k| item.contains_key(*k))
        .chain(item.keys().filter(|k| !seen(k)))
        .map(|key| {
            let value = &item[key];
            let text = if block {
                format!("k:\n  {key}: x")
            } else {
                format!("k: {{{key}: x}}")
            };
            let mut field = parse_readonly(&text)?
                .fields
                .into_iter()
                .find(|f| &f.key == key)?;
            if let Some((_, Some(style))) = styles.iter().find(|(k, _)| k == key) {
                field.style.clone_from(style);
            }
            let mut expected = Map::new();
            expected.insert(key.clone(), value.clone());
            let rendered = render_mapping_field(&text, &field, value, &Value::Object(expected))?;
            Some((key.clone(), rendered))
        })
        .collect()
}

fn render_scalar_with(
    style: &str,
    value: &Value,
    accepts: impl Fn(&str) -> bool,
) -> Option<String> {
    let candidates: Vec<String> = match value {
        Value::String(s) => {
            if s.chars().any(|c| {
                c.is_control() || matches!(c, '\u{2028}' | '\u{2029}' | '\u{85}' | '\u{feff}')
            }) {
                return None;
            }
            let single = format!("'{}'", s.replace('\'', "''"));
            let double = serde_json::to_string(s).ok()?;
            match style {
                "SingleQuoted" => vec![single, double],
                "DoubleQuoted" => vec![double, single],
                _ if s.is_empty()
                    || matches!(
                        s.to_ascii_lowercase().as_str(),
                        "y" | "n" | "yes" | "no" | "on" | "off" | "true" | "false" | "null" | "~"
                    ) =>
                {
                    vec![single, double]
                }
                _ => vec![s.clone(), single, double],
            }
        }
        Value::Bool(_) | Value::Null | Value::Number(_) => vec![value.to_string()],
        _ => return None,
    };
    candidates.into_iter().find(|text| accepts(text))
}
