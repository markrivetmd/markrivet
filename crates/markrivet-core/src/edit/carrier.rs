//! markstay §3.4 carrier verdicts for every child stay of a list, from one
//! forward pass over the list.
//!
//! [`super::carrier_refusal`] is the definition: the list's raw source from
//! its first byte up to a query offset `e`, plain markers masked, then the
//! content of closed inline code spans masked, refuses on the first `<` or
//! `\` left. Called once per child stay it rescans the list from byte zero
//! each time, so a write over a list of `n` items costs `n` times the list.
//! [`ListCarrier`] computes the same answer for every `e` from one pass.
//!
//! Why the answer is the same, step by step (`S` is the list's source, `M`
//! the list with every plain marker masked, and `k` the number of lines
//! that end before `e`):
//!
//! * **Plain masking is prefix-stable.** Masking writes byte for byte inside
//!   each marker span. When no plain marker straddles `e`, every marker either
//!   ends at or before `e` (masked in both texts) or starts at or after it
//!   (touches no byte before `e`), so masking `S[..e]` gives `M[..e]`. A query
//!   where a marker straddles `e` is answered by the definition itself.
//! * **Complete lines are the same lines.** Lines split on CRLF, lone CR and
//!   LF. Every terminator that starts before `e` is a terminator of `M[..e]`
//!   (a CRLF cut between its two bytes becomes a lone CR there), so lines
//!   `1..=k` have the same numbers and bytes in both, and line `k + 1` of the
//!   prefix is the partial line `M[ls..e]`.
//! * **Fence state is a top-down scan.** `markstay::fence_state` decides line
//!   `n` from lines `1..=n` alone, and an unclosed fence runs on with no
//!   lookahead. So lines `1..=k` are fence lines in the prefix exactly when
//!   they are in `M`. The partial line enters with the state after line `k`:
//!   a fence still open makes it a fence line whatever it holds, and otherwise
//!   it is one exactly when it opens a fence, which `fence_state` of the
//!   partial line alone answers. This is the one place a truncated line can
//!   differ from the whole line (an info string cut before a backtick turns a
//!   non-opener into an opener), so it is computed from the partial line.
//! * **Backtick pairing is per line and stops at the first unbalanced line.**
//!   A line's spans depend on its own bytes, whether it is a fence line and
//!   whether an earlier line stopped the scan, so lines `1..=k` pair alike in
//!   both, and the partial line is paired from its own bytes.
//! * **The first offender.** Bytes before the partial line are lines `1..=k`
//!   and their terminators, which are never offenders, so the earliest
//!   offender there is the answer when there is one, and otherwise the
//!   answer is in the partial line or there is none.
//!
//! The differential tests below hold every query equal to the definition, on
//! the markstay carrier vectors and on generated text, at every prefix.

use super::{CarrierRefusal, carrier_refusal, lines_with_offsets, plain_markers_in};
use crate::Span;
use crate::document::Document;
use crate::markers;
use std::cell::OnceCell;
use std::collections::BTreeSet;

/// One line of the masked list: its content is `[start, end)`; its terminator,
/// if any, follows.
#[derive(Debug, Clone, Copy)]
struct Line {
    start: usize,
    end: usize,
}

/// The §3.4 verdict for every prefix of one list, from one pass over it.
#[derive(Debug)]
pub(crate) struct ListCarrier<'s> {
    /// The list's raw source, kept for queries the index does not answer.
    source: &'s str,
    /// Plain markers relative to the list, sorted by start.
    plain: Vec<Span>,
    /// `max_end[i]`: the largest `end` among `plain[..i]`.
    max_end: Vec<usize>,
    /// The list with its plain markers masked.
    masked: String,
    /// 1-based line numbers a fence is still open after (markstay §3.3).
    open_after: BTreeSet<usize>,
    lines: Vec<Line>,
    /// The first line whose backtick runs do not pair, which ends the scan.
    stop: Option<usize>,
    /// `first[k]`: the first offender in lines `1..=k` after both masks.
    first: Vec<Option<(usize, char)>>,
    /// Queries the index handed to the definition (a plain marker straddled
    /// the query offset). Read by the tests.
    #[cfg(test)]
    defined: std::cell::Cell<usize>,
}

/// The closed inline code spans on one line that is not a fence line, as
/// `[start, end)` content offsets within the line, or `None` when a backtick
/// run on it has no partner, which stops the scan. The per-line step of
/// [`super::code_spans`], which is its definition.
fn pair_line(line: &[u8]) -> Option<Vec<(usize, usize)>> {
    let mut runs: Vec<(usize, usize)> = Vec::new();
    let mut i = 0;
    while i < line.len() {
        if line[i] == b'`' {
            let start = i;
            while i < line.len() && line[i] == b'`' {
                i += 1;
            }
            runs.push((start, i));
        } else {
            i += 1;
        }
    }
    let mut found = Vec::new();
    let mut a = 0;
    while a < runs.len() {
        let width = runs[a].1 - runs[a].0;
        let mut b = a + 1;
        while b < runs.len() && runs[b].1 - runs[b].0 != width {
            b += 1;
        }
        if b == runs.len() {
            return None;
        }
        found.push((runs[a].1, runs[b].0));
        a = b + 1;
    }
    Some(found)
}

/// The first `<` or `\` in `line` with `spans` blanked, as an offset into
/// `line`. Masking writes ASCII spaces and neither offender's byte occurs
/// inside a multi-byte character, so a byte scan is exact.
fn first_offender(line: &[u8], spans: &[(usize, usize)]) -> Option<(usize, char)> {
    let mut masked = spans.iter().peekable();
    for (at, &b) in line.iter().enumerate() {
        while masked.peek().is_some_and(|&&(_, end)| end <= at) {
            masked.next();
        }
        if masked.peek().is_some_and(|&&(start, _)| start <= at) {
            continue;
        }
        if b == b'<' || b == b'\\' {
            return Some((at, b as char));
        }
    }
    None
}

impl<'s> ListCarrier<'s> {
    /// Index a list's raw source. `plain` holds its plain markers, relative to
    /// the list's first byte.
    pub(crate) fn new(source: &'s str, mut plain: Vec<Span>) -> Self {
        plain.sort_by_key(|m| (m.start, m.end));
        let mut max_end = Vec::with_capacity(plain.len() + 1);
        max_end.push(0);
        for m in &plain {
            let last = *max_end.last().expect("seeded");
            max_end.push(last.max(m.end));
        }
        let masked = markers::mask_plain(source, &plain);
        let fence = markstay::fence_state(&masked);
        let lines: Vec<Line> = lines_with_offsets(&masked)
            .into_iter()
            .map(|(_, text, start)| Line {
                start,
                end: start + text.len(),
            })
            .collect();
        let bytes = masked.as_bytes();
        let mut stop = None;
        let mut first = Vec::with_capacity(lines.len() + 1);
        first.push(None);
        for (index, line) in lines.iter().enumerate() {
            let number = index + 1;
            let text = &bytes[line.start..line.end];
            let spans = if stop.is_some() || fence.inside.contains(&number) {
                Vec::new()
            } else {
                match pair_line(text) {
                    Some(spans) => spans,
                    None => {
                        stop = Some(number);
                        Vec::new()
                    }
                }
            };
            let previous: Option<(usize, char)> = *first.last().expect("seeded");
            first.push(
                previous
                    .or_else(|| first_offender(text, &spans).map(|(at, c)| (line.start + at, c))),
            );
        }
        ListCarrier {
            source,
            plain,
            max_end,
            masked,
            open_after: fence.open_after,
            lines,
            stop,
            first,
            #[cfg(test)]
            defined: std::cell::Cell::new(0),
        }
    }

    /// [`carrier_refusal`] over the list's first `e` bytes, with the plain
    /// markers that end by `e` masked.
    pub(crate) fn query(&self, e: usize) -> Option<CarrierRefusal> {
        let before = self.plain.partition_point(|m| m.start < e);
        if self.max_end[before] > e {
            // A plain marker straddles `e`: masking the prefix is not a prefix
            // of masking the list, so ask the definition.
            #[cfg(test)]
            self.defined.set(self.defined.get() + 1);
            let plain: Vec<Span> = self.plain.iter().copied().filter(|m| m.end <= e).collect();
            return carrier_refusal(&self.source[..e], &plain);
        }
        // The last line starting at or before `e`.
        let index = self.lines.partition_point(|l| l.start <= e) - 1;
        let line = self.lines[index];
        // `k` complete lines, then the partial line `[ls, e)`. Past the line's
        // content means inside its terminator: a CRLF cut between its bytes,
        // whose CR completes the line in the prefix.
        let (k, ls) = if e <= line.end {
            (index, line.start)
        } else {
            (index + 1, e)
        };
        if let Some((at, character)) = self.first[k] {
            return Some(CarrierRefusal::new(at, character));
        }
        let partial = &self.masked.as_bytes()[ls..e];
        let spans = if self.stop.is_some_and(|n| n <= k) {
            Vec::new()
        } else {
            let fenced = (k >= 1 && self.open_after.contains(&k))
                || markstay::fence_state(&self.masked[ls..e])
                    .inside
                    .contains(&1);
            if fenced {
                Vec::new()
            } else {
                pair_line(partial).unwrap_or_default()
            }
        };
        let (at, character) = first_offender(partial, &spans)?;
        Some(CarrierRefusal::new(ls + at, character))
    }
}

/// Lazily built [`ListCarrier`]s for one document, one per list on its first
/// query. Holds no state a write could stale: it borrows the document it
/// indexes.
pub(crate) struct Carriers<'d> {
    doc: &'d Document,
    lists: Vec<OnceCell<ListCarrier<'d>>>,
}

impl<'d> Carriers<'d> {
    pub(crate) fn new(doc: &'d Document) -> Self {
        Carriers {
            doc,
            lists: (0..doc.lists.len()).map(|_| OnceCell::new()).collect(),
        }
    }

    /// The document this indexes.
    pub(crate) fn doc(&self) -> &'d Document {
        self.doc
    }

    fn list(&self, list: usize) -> &ListCarrier<'d> {
        self.lists[list].get_or_init(|| {
            let span = self.doc.blocks[self.doc.lists[list].block].span;
            ListCarrier::new(
                &self.doc.source[span.start..span.end],
                plain_markers_in(self.doc, span),
            )
        })
    }

    /// §3.4 over `list`'s raw source from its first byte to absolute offset
    /// `end`, with the plain markers inside that prefix masked: the answer
    /// `carrier_refusal` gives for that prefix. `at` is relative to the list.
    pub(crate) fn refusal(&self, list: usize, end: usize) -> Option<CarrierRefusal> {
        let span = self.doc.blocks[self.doc.lists[list].block].span;
        if end < span.start || end > span.end {
            // Not a prefix of the list: the definition, as the callers had it.
            let prefix = Span {
                start: span.start,
                end,
            };
            return carrier_refusal(
                &self.doc.source[prefix.start..prefix.end],
                &plain_markers_in(self.doc, prefix),
            );
        }
        self.list(list).query(end - span.start)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    /// Comparable form of a verdict: every field, not just accept or refuse.
    fn key(r: Option<CarrierRefusal>) -> Option<(usize, char, &'static str)> {
        r.map(|r| (r.at, r.character, r.reason))
    }

    /// The definition at `e`: the prefix, with the plain markers that end by
    /// `e` masked.
    fn defined(text: &str, plain: &[Span], e: usize) -> Option<CarrierRefusal> {
        let plain_e: Vec<Span> = plain.iter().copied().filter(|m| m.end <= e).collect();
        carrier_refusal(&text[..e], &plain_e)
    }

    /// Every char-boundary prefix of `text` through the index and through the
    /// definition. Returns the number of queries.
    fn every_prefix(name: &str, text: &str, plain: &[Span]) -> usize {
        let index = ListCarrier::new(text, plain.to_vec());
        let mut queries = 0;
        for e in (0..=text.len()).filter(|&e| text.is_char_boundary(e)) {
            assert_eq!(
                key(index.query(e)),
                key(defined(text, plain, e)),
                "{name}: prefix {e} of {text:?} (plain {plain:?})"
            );
            queries += 1;
        }
        queries
    }

    #[test]
    fn every_prefix_of_every_markstay_carrier_vector() {
        let corpus: Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/markstay/carrier-state.json"
        ))
        .unwrap();
        let (mut vectors, mut queries) = (0, 0);
        for vector in corpus["vectors"].as_array().unwrap() {
            let name = vector["name"].as_str().unwrap();
            let text = vector["carrier_text"].as_str().unwrap();
            queries += every_prefix(name, text, &[]);
            // The whole text as a list ending at its last byte.
            assert_eq!(
                ListCarrier::new(text, vec![]).query(text.len()).is_none(),
                vector["accepts"].as_bool().unwrap(),
                "{name}: verdict against markstay's"
            );
            vectors += 1;
        }
        assert!(vectors >= 20, "only {vectors} carrier vectors loaded");
        eprintln!("carrier vectors: {vectors} vectors, {queries} prefixes, 0 mismatches");
    }

    /// A deterministic generator: xorshift64*, seeded per case.
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

    /// Pieces chosen to reach every step of the argument: backtick runs of
    /// each width, fences of both kinds with 0-4 leading spaces, closers equal
    /// to, longer and shorter than their opener, an info string holding a
    /// backtick, tabs, offenders, list markers, multi-byte characters and
    /// every line ending.
    const PIECES: &[&str] = &[
        "`", "``", "```", "````", "~~~", "~~~~", " ```", "   ```", "    ```", "```rust", "```a`b",
        "~~~ x`y", "\t```", "<", "\\", "a", "b c", " ", "\t", "- ", "1. ", "* ", "é", "日", "\n",
        "\n", "\n", "\r\n", "\r\n", "\r", "`<`", "`` ` ``", "<b>", "x\\y",
    ];

    /// A plain marker's text, which carries a `<` the mask must hide.
    fn marker(rng: &mut Rng) -> String {
        let id: String = (0..8)
            .map(|_| (b'a' + rng.below(26) as u8) as char)
            .collect();
        markers::format_plain(&id, "subhash", "000000000000")
    }

    fn generate(seed: u64) -> (String, Vec<Span>) {
        let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
        let mut text = String::new();
        let mut plain = Vec::new();
        for _ in 0..(4 + rng.below(28)) {
            match rng.below(12) {
                0 => {
                    let m = marker(&mut rng);
                    plain.push(Span {
                        start: text.len(),
                        end: text.len() + m.len(),
                    });
                    text.push_str(&m);
                }
                1 => {
                    // A span over arbitrary text, line breaks included: the mask
                    // keeps CR and LF, so the lines must survive it.
                    let start = text.len();
                    for _ in 0..(1 + rng.below(4)) {
                        text.push_str(PIECES[rng.below(PIECES.len())]);
                    }
                    plain.push(Span {
                        start,
                        end: text.len(),
                    });
                }
                _ => text.push_str(PIECES[rng.below(PIECES.len())]),
            }
        }
        (text, plain)
    }

    #[test]
    fn every_prefix_of_generated_text() {
        let (mut cases, mut queries) = (0, 0);
        for seed in 0..2_000u64 {
            let (text, plain) = generate(seed);
            queries += every_prefix(&format!("seed {seed}"), &text, &plain);
            cases += 1;
        }
        eprintln!("generated: {cases} texts, {queries} prefixes, 0 mismatches");
    }

    #[test]
    fn named_edges() {
        let cases: &[(&str, &str)] = &[
            ("empty", ""),
            ("opener only once truncated", "- a\n```a`b <\n"),
            ("opener lost below three backticks", "- a\n``x <\n"),
            ("unclosed fence before", "```\n- a <\n- b\n"),
            ("closed fence then offender", "```\n<\n```\n- a \\ b\n"),
            ("closer longer than opener", "```\nx\n`````\n- `<` a\n"),
            ("closer shorter stays open", "````\nx\n```\n- a <\n"),
            ("unbalanced line stops the scan", "- a ` b\n- `<` c\n"),
            ("balanced partial hides", "- a\n- `<` b `c`"),
            ("unbalanced partial exposes", "- a\n- `<` b ``c"),
            ("crlf", "- a\r\n- `<`\r\n- b <\r\n"),
            ("lone cr", "- a\r- `<`\r- b\r"),
            ("mixed endings", "- a\r\n- b\n- c\r- `x` <"),
            ("no final newline", "- a `<` b"),
            ("tilde fence", "~~~\n<\n~~~\n- ok\n"),
            ("indented four is no fence", "    ```\n- `<`\n    ```\n"),
            ("backslash after fence", "```\n```\n\\"),
        ];
        let mut queries = 0;
        for (name, text) in cases {
            queries += every_prefix(name, text, &[]);
        }
        // A cut between CR and LF answers for the complete line before it.
        let text = "- `<`\r\n- b <";
        let index = ListCarrier::new(text, vec![]);
        assert_eq!(key(index.query(6)), key(defined(text, &[], 6)));
        // A plain marker carrying `<`, masked, then queried inside itself.
        let m = markers::format_plain("abcdefgh", "subhash", "000000000000");
        let text = format!("- a {m} b\n- c");
        let plain = [Span {
            start: 4,
            end: 4 + m.len(),
        }];
        queries += every_prefix("plain marker", &text, &plain);
        let index = ListCarrier::new(&text, plain.to_vec());
        index.query(text.len());
        assert_eq!(
            index.defined.get(),
            0,
            "a query past the marker uses the index"
        );
        index.query(6);
        assert_eq!(
            index.defined.get(),
            1,
            "a query inside it asks the definition"
        );
        eprintln!("named edges: {} cases, {queries} prefixes", cases.len() + 1);
    }

    /// Today's `carrier_state` body, verbatim: the oracle for a document.
    fn carrier_state_oracle(d: &Document, list: usize, end: usize) -> Option<CarrierRefusal> {
        let start = d.blocks[d.lists[list].block].span.start;
        let prefix = Span { start, end };
        carrier_refusal(
            &d.source[prefix.start..prefix.end],
            &plain_markers_in(d, prefix),
        )
    }

    /// A tracker-like document: lists of stamped and unstamped items with
    /// offenders before, between and after them, nested lists, inline spans
    /// and fences, in LF or CRLF.
    fn document(seed: u64, crlf: bool) -> String {
        let mut rng = Rng(seed.wrapping_mul(0xD1B5_4A32_D192_ED03) | 1);
        let mut out = String::from("---\nstatus: active\n---\n\n# Doc\n\n");
        for section in 0..(1 + rng.below(3)) {
            out.push_str(&format!("## Section {section}\n\n"));
            let items = 1 + rng.below(25);
            for i in 0..items {
                let check = if rng.below(2) == 0 { "[ ] " } else { "" };
                let body = match rng.below(9) {
                    0 => "has a raw < here".to_string(),
                    1 => "has `<code>` only".to_string(),
                    2 => "has a \\ backslash".to_string(),
                    3 => "has `` a ` b `` span".to_string(),
                    4 => "unbalanced ` tick".to_string(),
                    5 => "two\n  lines `<`".to_string(),
                    _ => format!("plain item {i}"),
                };
                out.push_str(&format!("- {check}{body}"));
                if rng.below(3) > 0 {
                    let id: String = (0..8)
                        .map(|_| (b'a' + rng.below(26) as u8) as char)
                        .collect();
                    out.push(' ');
                    out.push_str(&markers::format_plain(&id, "subhash", "000000000000"));
                }
                out.push('\n');
                match rng.below(12) {
                    0 => out.push_str("  - nested <\n"),
                    1 => out.push_str("\n  ```\n  a < b\n  ```\n"),
                    _ => {}
                }
            }
            out.push('\n');
        }
        if crlf { out.replace('\n', "\r\n") } else { out }
    }

    #[test]
    fn every_child_stay_and_carrier_in_generated_documents() {
        let (mut docs, mut stays, mut carriers) = (0, 0, 0);
        for seed in 0..300u64 {
            for crlf in [false, true] {
                let src = document(seed, crlf);
                let Ok(doc) = Document::parse(&src) else {
                    continue;
                };
                let index = Carriers::new(&doc);
                for (l, list) in doc.lists.iter().enumerate() {
                    for item in &list.items {
                        for &m in &item.markers {
                            let end = doc.markers[m].lexical.span.start;
                            assert_eq!(
                                key(index.refusal(l, end)),
                                key(carrier_state_oracle(&doc, l, end)),
                                "seed {seed} crlf {crlf}: list {l} marker at {end}\n{src}"
                            );
                            stays += 1;
                        }
                        if let Some(end) = item.carrier {
                            assert_eq!(
                                key(index.refusal(l, end)),
                                key(carrier_state_oracle(&doc, l, end)),
                                "seed {seed} crlf {crlf}: list {l} carrier at {end}\n{src}"
                            );
                            carriers += 1;
                        }
                    }
                }
                let defined: usize = index
                    .lists
                    .iter()
                    .filter_map(OnceCell::get)
                    .map(|c| c.defined.get())
                    .sum();
                assert_eq!(defined, 0, "seed {seed}: a real query asked the definition");
                docs += 1;
            }
        }
        assert!(docs > 500, "only {docs} generated documents parsed");
        assert!(
            stays > 1000 && carriers > 1000,
            "{stays} stays, {carriers} carriers"
        );
        eprintln!(
            "documents: {docs}, {stays} child stays, {carriers} carriers, 0 mismatches, 0 definition queries"
        );
    }

    /// Adoption carriers, through the index and through today's
    /// `adoption_blockers` body kept verbatim below, and the `adopt_stay`
    /// declare path against that oracle's first blocker.
    mod adoption {
        use super::Rng;
        use crate::document::Document;
        use crate::edit::{
            AdoptionBlocker, Carriers, Op, Planner, adoption_blockers, adoption_others,
            carrier_refusal, err, plain_markers_in, plan,
        };
        use crate::{ErrorCode, Selector, Span, markers};

        /// Today's `adoption_blockers`, verbatim but for its name: the oracle.
        fn adoption_blockers_oracle(
            doc: &Document,
            list: usize,
            id: &str,
            x: &markers::Lexical,
            dropped: &[Span],
        ) -> Vec<AdoptionBlocker> {
            let mut repairs = vec![];
            if !x.plain || x.hash.is_some() || x.has_subhash || x.malformed {
                repairs.push(AdoptionBlocker {
                    repair: format!("reduce it to `{}`", markers::format_bare(id)),
                    diagnostic: err(
                        ErrorCode::UnsupportedSyntax,
                        format!(
                            "stay {id} carries attributes; only a bare `{}` is adopted",
                            markers::format_bare(id)
                        ),
                    ),
                });
            }
            let elsewhere = adoption_others(doc, id, x, dropped);
            if elsewhere > 0 {
                repairs.push(AdoptionBlocker {
                    repair: match elsewhere {
                        1 => format!("drop the other marker carrying {id}"),
                        n => format!("drop the other {n} markers carrying {id}"),
                    },
                    diagnostic: err(
                        ErrorCode::AmbiguousSelector,
                        format!(
                            "{} markers carry {id}; adoption needs exactly one",
                            elsewhere + 1
                        ),
                    ),
                });
            }
            if doc
                .blocks
                .iter()
                .any(|b| b.heading.as_ref().and_then(|h| h.legacy_id.as_deref()) == Some(id))
            {
                repairs.push(AdoptionBlocker {
                    repair: format!("rename the heading `{{#{id}}}`"),
                    diagnostic: err(
                        ErrorCode::InvalidContent,
                        format!(
                            "stay {id} is also a heading id; rename the heading before adoption"
                        ),
                    ),
                });
            }
            if let Some(item) = doc.lists[list]
                .items
                .iter()
                .find(|it| it.span.start <= x.span.start && x.span.end <= it.span.end)
            {
                if !item.markers.is_empty() {
                    repairs.push(AdoptionBlocker {
                        repair: format!("drop {id}"),
                        diagnostic: err(
                            ErrorCode::UnsupportedSyntax,
                            format!("the item already has Rivet's stay; drop {id} by hand"),
                        ),
                    });
                }
                if item
                    .nested
                    .iter()
                    .any(|&(a, z)| a <= doc.line_of(x.span.start) && doc.line_of(x.span.start) <= z)
                {
                    repairs.push(AdoptionBlocker {
                        repair: format!("move {id} out of the nested item"),
                        diagnostic: err(
                            ErrorCode::InvalidRequest,
                            format!("stay {id} sits in a nested item"),
                        ),
                    });
                }
            }
            // The carrier as it reads once the item's other hand-written stays are
            // deleted: removed, not masked, since deleting one can join the backtick
            // runs on either side of it. Plain markers left behind are masked.
            let start = doc.blocks[doc.lists[list].block].span.start;
            let prefix = Span {
                start,
                end: x.span.start,
            };
            let mut gone: Vec<Span> = dropped
                .iter()
                .copied()
                .filter(|d| prefix.start <= d.start && d.end <= prefix.end)
                .collect();
            gone.sort_by_key(|d| d.start);
            let shift = |at: usize| -> usize {
                gone.iter()
                    .filter(|d| d.end <= at)
                    .map(|d| d.end - d.start)
                    .sum()
            };
            let mut text = String::new();
            let mut at = prefix.start;
            for d in &gone {
                text.push_str(&doc.source[at..d.start]);
                at = d.end;
            }
            text.push_str(&doc.source[at..prefix.end]);
            let masked: Vec<Span> = plain_markers_in(doc, prefix)
                .into_iter()
                .map(|m| Span {
                    start: m.start + start,
                    end: m.end + start,
                })
                .filter(|m| !gone.contains(m))
                .map(|m| Span {
                    start: m.start - start - shift(m.start),
                    end: m.end - start - shift(m.start),
                })
                .collect();
            if let Some(r) = carrier_refusal(&text, &masked) {
                repairs.push(AdoptionBlocker {
                repair:format!("clear its carrier ({})", r.reason),
                diagnostic:err(ErrorCode::StampRefused,format!("stay {id} would sit behind non-plain carrier text: {}; remove '<' or '\\' from the list before it",r.reason)).with_id(id),
            });
            }
            repairs
        }

        fn repairs(blockers: Vec<AdoptionBlocker>) -> Vec<(ErrorCode, String, String)> {
            blockers
                .into_iter()
                .map(|b| (b.diagnostic.code, b.diagnostic.message, b.repair))
                .collect()
        }

        /// A list whose unstamped items carry zero, one or two bare
        /// hand-written stays, with offenders before, between and after them.
        fn document(seed: u64, crlf: bool) -> String {
            let mut rng = Rng(seed.wrapping_mul(0x94D0_49BB_1331_11EB) | 1);
            let mut out = String::from("# Doc\n\n## Action\n\n");
            for i in 0..(2 + rng.below(30)) {
                let body = match rng.below(8) {
                    0 => "raw < here",
                    1 => "`<code>` only",
                    2 => "a \\ slash",
                    3 => "unbalanced ` tick",
                    _ => "plain",
                };
                out.push_str(&format!("- [ ] **Item {i}.** {body}"));
                for _ in 0..[0, 0, 1, 1, 1, 2][rng.below(6)] {
                    let id: String = (0..8)
                        .map(|_| (b'a' + rng.below(26) as u8) as char)
                        .collect();
                    out.push(' ');
                    out.push_str(&markers::format_bare(&id));
                }
                out.push('\n');
            }
            if crlf { out.replace('\n', "\r\n") } else { out }
        }

        #[test]
        fn hand_written_stays_match_the_verbatim_oracle() {
            let (mut docs, mut stays, mut declared, mut refused, mut carrier) = (0, 0, 0, 0, 0);
            for seed in 0..300u64 {
                for crlf in [false, true] {
                    let src = document(seed, crlf);
                    let Ok(doc) = Document::parse(&src) else {
                        continue;
                    };
                    let carriers = Carriers::new(&doc);
                    for (l, list) in doc.lists.iter().enumerate() {
                        for item in &list.items {
                            let hand: Vec<&markers::Lexical> = doc.blocks[list.block]
                                .markers
                                .iter()
                                .map(|&m| &doc.markers[m].lexical)
                                .filter(|x| {
                                    x.hash.is_none()
                                        && item.span.start <= x.span.start
                                        && x.span.end <= item.span.end
                                })
                                .collect();
                            for x in &hand {
                                let Some(id) = x.id.as_deref() else { continue };
                                let dropped: Vec<Span> = hand
                                    .iter()
                                    .filter(|y| y.span != x.span)
                                    .map(|y| y.span)
                                    .collect();
                                for cut in [&[][..], &dropped[..]] {
                                    assert_eq!(
                                        repairs(adoption_blockers(&carriers, l, id, x, cut)),
                                        repairs(adoption_blockers_oracle(&doc, l, id, x, cut)),
                                        "seed {seed} crlf {crlf}: stay {id} cut {cut:?}\n{src}"
                                    );
                                    stays += 1;
                                }
                                if hand.len() != 1 {
                                    continue;
                                }
                                // The declare path refuses with the oracle's first
                                // blocker, or not on the carrier at all.
                                let guard = doc.target_revision(
                                    doc.resolve(&Selector::Id { id: id.into() }).unwrap(),
                                );
                                let op = Op::AdoptStay {
                                    id: id.into(),
                                    guard,
                                };
                                let result = Planner::new(&doc)
                                    .structural(&op)
                                    .and_then(|change| plan(&doc, vec![change]));
                                let first = adoption_blockers_oracle(&doc, l, id, x, &[])
                                    .into_iter()
                                    .next()
                                    .map(|b| (b.diagnostic.code, b.diagnostic.message));
                                match (first, result) {
                                    (Some(want), Err(d)) => {
                                        assert_eq!((d.code, d.message), want, "seed {seed}\n{src}");
                                        refused += 1;
                                        carrier +=
                                            usize::from(want.1.contains("non-plain carrier"));
                                    }
                                    (Some(want), Ok(_)) => {
                                        panic!("seed {seed}: adopted past {want:?}\n{src}")
                                    }
                                    (None, Err(d)) => assert!(
                                        !d.message.contains("non-plain carrier"),
                                        "seed {seed}: {d:?}\n{src}"
                                    ),
                                    (None, Ok(_)) => {}
                                }
                                declared += 1;
                            }
                        }
                    }
                    docs += 1;
                }
            }
            assert!(
                docs > 500 && stays > 1000 && declared > 500 && refused > 100 && carrier > 100,
                "{docs} {stays} {declared} {refused}"
            );
            eprintln!(
                "adoption: {docs} documents, {stays} stay checks, {declared} adopt_stay plans ({refused} refused, {carrier} on the carrier), 0 mismatches"
            );
        }
    }
}
