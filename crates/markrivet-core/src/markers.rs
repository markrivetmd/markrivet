//! markstay marker adapter. The `markstay` crate owns the §4 grammar; this
//! module adds the byte offsets and attribute spans the crate keeps private,
//! and cross-checks every marker against the crate's own reading.
use crate::{Diagnostic, ErrorCode, Span};

/// One lexical marker in the original source. Whether it is live (a real
/// HTML comment rather than code or attribute text) is decided by the
/// document model, which owns the parser.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lexical {
    pub span: Span,
    pub id: Option<String>,
    pub hash: Option<String>,
    pub subhash: Option<String>,
    pub has_subhash: bool,
    pub malformed: bool,
    pub mdx: bool,
    /// Only an id plus valid, unquoted, unduplicated `hash`/`subhash` digests
    /// (§3.4 plain-text state). Only plain markers may have evidence refreshed.
    pub plain: bool,
    /// Raw-relative value spans of the digest attributes, for evidence refresh.
    pub hash_value: Option<Span>,
    pub subhash_value: Option<Span>,
}

pub(crate) fn line_starts(source: &str) -> Vec<usize> {
    let mut starts = vec![0];
    starts.extend(source.match_indices('\n').map(|(i, _)| i + 1));
    starts
}

/// Scan a complete document. Multi-line markers, overlapping markers and any
/// disagreement with the markstay crate are reported instead of guessed.
pub fn scan(source: &str) -> Result<Vec<Lexical>, Diagnostic> {
    let starts = line_starts(source);
    let mut found: Vec<Lexical> = vec![];
    let mut cursor: Option<(usize, usize)> = None;
    for marker in markstay::find_markers(source, 0) {
        let fail = |message: &str| Diagnostic::new(ErrorCode::UnsupportedSyntax, message);
        if marker.raw.contains(['\n', '\r']) {
            return Err(fail("multi-line stay markers are unsupported").with_line(marker.line));
        }
        let Some(&line_start) = starts.get(marker.line - 1) else {
            return Err(fail("marker line is outside the source"));
        };
        let line_end = starts
            .get(marker.line)
            .map_or(source.len(), |next| next - 1);
        let from = match cursor {
            Some((line, previous)) if line == marker.line => previous + 1,
            _ => line_start,
        };
        let Some(relative) = source[from..line_end].find(&marker.raw) else {
            return Err(fail("could not locate marker bytes").with_line(marker.line));
        };
        let start = from + relative;
        cursor = Some((marker.line, start));
        let span = Span {
            start,
            end: start + marker.raw.len(),
        };
        if found
            .last()
            .is_some_and(|previous| previous.span.end > span.start)
        {
            return Err(fail("overlapping stay markers are unsupported").with_line(marker.line));
        }
        let mdx = marker.syntax == markstay::Syntax::Mdx;
        let attrs = if mdx || marker.malformed {
            None
        } else {
            parse_attributes(&marker.raw)
        };
        let mut lexical = Lexical {
            span,
            id: marker.id.clone(),
            hash: marker.hash.clone(),
            subhash: marker.subhash.clone(),
            has_subhash: marker.has_subhash,
            malformed: marker.malformed,
            mdx,
            plain: false,
            hash_value: None,
            subhash_value: None,
        };
        if let Some(attrs) = attrs {
            if attrs.id != marker.id.clone().unwrap_or_default() {
                return Err(fail("marker id disagrees with markstay").with_line(marker.line));
            }
            let digest = |key: &str| {
                attrs
                    .items
                    .iter()
                    .filter(|a| a.key == key)
                    .collect::<Vec<_>>()
            };
            let hashes = digest("hash");
            let subhashes = digest("subhash");
            lexical.plain = attrs
                .items
                .iter()
                .all(|a| matches!(a.key.as_str(), "hash" | "subhash") && a.digest.is_some())
                && hashes.len() <= 1
                && subhashes.len() <= 1;
            if hashes.len() == 1 && hashes[0].digest.is_some() {
                lexical.hash_value = Some(hashes[0].value);
            }
            if subhashes.len() == 1 && subhashes[0].digest.is_some() {
                lexical.subhash_value = Some(subhashes[0].value);
            }
            if lexical.plain
                && (hashes.first().and_then(|a| a.digest.clone()) != marker.hash
                    || subhashes.first().and_then(|a| a.digest.clone()) != marker.subhash)
            {
                return Err(fail("marker digest disagrees with markstay").with_line(marker.line));
            }
        } else if !mdx && !marker.malformed {
            return Err(fail("marker attributes disagree with markstay").with_line(marker.line));
        }
        found.push(lexical);
    }
    Ok(found)
}

struct Attribute {
    key: String,
    /// Raw-relative extent of the hex digits after `sha256:`.
    value: Span,
    digest: Option<String>,
}

struct Attributes {
    id: String,
    items: Vec<Attribute>,
}

/// §4 HTML form on one line: `<!--` WSP* `stay:` id (WSP+ key=value)* WSP* `-->`.
fn parse_attributes(raw: &str) -> Option<Attributes> {
    let b = raw.as_bytes();
    let body_end = raw.len().checked_sub(3)?;
    if !raw.starts_with("<!--") || !raw.ends_with("-->") {
        return None;
    }
    let mut pos = 4;
    while pos < body_end && matches!(b[pos], b' ' | b'\t') {
        pos += 1;
    }
    if !raw[pos..].starts_with("stay:") {
        return None;
    }
    pos += 5;
    let id_start = pos;
    while pos < body_end && is_id_byte(b[pos]) {
        pos += 1;
    }
    if pos == id_start {
        return None;
    }
    let id = raw[id_start..pos].to_string();
    let mut items = vec![];
    loop {
        let separator = pos;
        while pos < body_end && matches!(b[pos], b' ' | b'\t') {
            pos += 1;
        }
        if pos == body_end {
            break;
        }
        if pos == separator || !b[pos].is_ascii_alphabetic() {
            return None;
        }
        let key_start = pos;
        while pos < body_end && is_id_byte(b[pos]) {
            pos += 1;
        }
        let key = raw[key_start..pos].to_string();
        if b.get(pos) != Some(&b'=') {
            return None;
        }
        pos += 1;
        if pos >= body_end {
            return None;
        }
        if b[pos] == b'"' {
            pos += 1;
            while pos < body_end && b[pos] != b'"' {
                pos += if b[pos] == b'\\' { 2 } else { 1 };
            }
            if pos >= body_end {
                return None;
            }
            pos += 1;
            items.push(Attribute {
                key,
                value: Span {
                    start: pos,
                    end: pos,
                },
                digest: None,
            });
        } else {
            let value_start = pos;
            while pos < body_end && !matches!(b[pos], b' ' | b'\t') {
                pos += 1;
            }
            let value = &raw[value_start..pos];
            let digest = value
                .strip_prefix("sha256:")
                .filter(|hex| !hex.is_empty() && hex.bytes().all(|c| c.is_ascii_hexdigit()))
                .map(str::to_ascii_lowercase);
            items.push(Attribute {
                key,
                value: Span {
                    start: value_start + 7,
                    end: pos,
                },
                digest,
            });
        }
    }
    Some(Attributes { id, items })
}

fn is_id_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-')
}

/// Rewrite one digest value in a plain marker, keeping the stored precision
/// and every other byte. The markstay crate re-reads the result.
pub fn refresh_digest(raw: &str, value: Span, full_hex: &str) -> Option<String> {
    let precision = value.end - value.start;
    let fresh = full_hex.get(..precision)?;
    let mut out = String::with_capacity(raw.len());
    out.push_str(&raw[..value.start]);
    out.push_str(fresh);
    out.push_str(&raw[value.end..]);
    let reread = markstay::find_markers(&out, 0);
    let original = markstay::find_markers(raw, 0);
    match (reread.as_slice(), original.as_slice()) {
        ([new], [old]) if new.id == old.id && new.has_subhash == old.has_subhash => Some(out),
        _ => None,
    }
}

/// What every HTML-form marker opens with (§4).
pub(crate) const OPENING: &str = "<!-- stay:";

/// Whether `text` holds a marker opening, whole marker or not.
pub(crate) fn contains_opening(text: &str) -> bool {
    text.contains(OPENING)
}

/// Byte offset of every marker opening in `text`, whole marker or not.
pub(crate) fn openings(text: &str) -> impl Iterator<Item = usize> + '_ {
    text.match_indices(OPENING).map(|(i, _)| i)
}

/// A bare marker carrying only an id: the form `adopt_stay` takes, and the
/// one a rendered page reduces every marker to.
pub(crate) fn format_bare(id: &str) -> String {
    format!("{OPENING}{id} -->")
}

/// A new plain marker carrying only an id and one digest (§3.4, §5.5).
pub fn format_plain(id: &str, key: &str, hex: &str) -> String {
    format!("<!-- stay:{id} {key}=sha256:{hex} -->")
}

/// Mask plain markers position for position (§3.4): every byte becomes a
/// space except line endings, so a writer's own markers never refuse a carrier.
pub fn mask_plain(text: &str, markers: &[Span]) -> String {
    let mut bytes = text.as_bytes().to_vec();
    for span in markers {
        for b in &mut bytes[span.start..span.end] {
            if !matches!(*b, b'\n' | b'\r') {
                *b = b' ';
            }
        }
    }
    String::from_utf8(bytes).expect("masking replaces whole markers with ASCII")
}
