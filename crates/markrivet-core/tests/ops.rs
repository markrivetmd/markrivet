//! The section append, frontmatter key insert and the two repair ops: each
//! changes only its declared bytes, a refusal plans nothing, every stay
//! keeps its attachment, and CRLF stays CRLF.
use markrivet_core::document::Target;
use markrivet_core::edit::{self, Op, Outcome, Planner};
use markrivet_core::{Diagnostic, Document, ErrorCode, Selector};
use serde_json::{Value, json};
use std::collections::BTreeMap;

const DOC: &str = "---
status: active
owner: sam
---

# Widgets

## Action

- [ ] **Ship it.** See `the long
      code span` for the gate.
- [ ] `long
      span` opens the item.
- [ ] **Plain.** Nothing wrapped.

## Log

Newest first.

### 2026-09-01: opened

First entry.
";

fn run(src: &str, op: Op) -> Result<Outcome, Diagnostic> {
    let doc = Document::parse(src).unwrap();
    let mut planner = Planner::new(&doc);
    let change = planner.structural(&op)?;
    edit::plan(&doc, vec![change])
}

fn heading(path: &[&str]) -> Selector {
    Selector::HeadingPath {
        path: path.iter().map(|p| p.to_string()).collect(),
    }
}

fn guard(src: &str, selector: &Selector) -> String {
    let doc = Document::parse(src).unwrap();
    doc.target_revision(doc.resolve(selector).unwrap())
}

fn doc_guard(src: &str) -> String {
    Document::parse(src).unwrap().revision()
}

/// Every id with the kind of unit it names.
fn attachments(src: &str) -> BTreeMap<String, &'static str> {
    let doc = Document::parse(src).unwrap();
    doc.ids
        .iter()
        .map(|(id, t)| {
            let kind = match t.as_slice() {
                [Target::Section(_)] => "section",
                [Target::Item { .. }] => "item",
                [Target::Block(_)] => "block",
                _ => "other",
            };
            (id.clone(), kind)
        })
        .collect()
}

fn stamped(src: &str) -> String {
    run(
        src,
        Op::Stamp {
            collection: None,
            checkbox_only: false,
            collection_guard: None,
            select: Selector::Document,
            guard: doc_guard(src),
        },
    )
    .unwrap()
    .candidate
}

/// `out` is `src` with `inserted` added at `at` and nothing else changed.
fn only_inserted(src: &str, out: &str, at: usize, inserted: &str) {
    assert_eq!(&out[..at], &src[..at], "bytes before the insertion");
    assert_eq!(&out[at..at + inserted.len()], inserted);
    assert_eq!(&out[at + inserted.len()..], &src[at..], "bytes after it");
}

fn code(r: Result<Outcome, Diagnostic>) -> ErrorCode {
    r.map(|o| o.candidate).unwrap_err().code
}

// ---------------------------------------------------------------------------
// append

fn append(src: &str, path: &[&str], content: &str) -> Result<Outcome, Diagnostic> {
    let select = heading(path);
    let guard = guard(src, &select);
    run(
        src,
        Op::Append {
            select,
            guard,
            content: content.into(),
        },
    )
}

#[test]
fn append_adds_blocks_before_the_first_subsection_and_nothing_else() {
    let src = stamped(DOC);
    let out = append(&src, &["Log"], "A note.\n\n- one\n- two\n").unwrap();
    let at = src.find("### 2026-09-01").unwrap();
    only_inserted(&src, &out.candidate, at, "A note.\n\n- one\n- two\n\n");
    assert_eq!(attachments(&out.candidate), attachments(&src));
    let next = &out.ops[0].revision;
    assert_eq!(
        next.as_ref(),
        Some(&guard(&out.candidate, &heading(&["Log"])))
    );
    // At the end of a file with no final newline.
    let out = append("# T\n\nText", &["T"], "New.").unwrap();
    assert_eq!(out.candidate, "# T\n\nText\n\nNew.\n");
}

#[test]
fn append_refuses_what_would_not_stand_alone_in_the_own_body() {
    let src = stamped(DOC);
    for (content, want) in [
        ("### A heading\n\nText.", ErrorCode::InvalidContent),
        ("Setext\n===", ErrorCode::InvalidContent),
        ("---\nkey: value\n---", ErrorCode::InvalidContent),
        ("  \n\n", ErrorCode::InvalidContent),
        ("Text <!-- stay:abcdefgh -->", ErrorCode::InvalidContent),
    ] {
        assert_eq!(code(append(&src, &["Log"], content)), want, "{content:?}");
    }
    // A list after the action list would join it.
    assert_eq!(
        code(append(DOC, &["Action"], "- [ ] **Joins.** The list.")),
        ErrorCode::InvalidContent
    );
    let stale = run(
        &src,
        Op::Append {
            select: heading(&["Log"]),
            guard: guard(DOC, &heading(&["Log"])),
            content: "x".into(),
        },
    );
    assert_eq!(code(stale), ErrorCode::GuardConflict);
    let doc = Document::parse(&src).unwrap();
    let item = doc.stay_ids(&doc.lists[0].items[0].markers)[0].clone();
    let not_a_section = run(
        &src,
        Op::Append {
            select: Selector::Id { id: item.clone() },
            guard: guard(&src, &Selector::Id { id: item }),
            content: "x".into(),
        },
    );
    assert!(matches!(code(not_a_section), ErrorCode::InvalidRequest));
}

#[test]
fn append_keeps_crlf() {
    let src = DOC.replace('\n', "\r\n");
    let out = append(&src, &["Log"], "Line one\nline two").unwrap();
    assert!(!out.candidate.replace("\r\n", "").contains('\n'));
    let at = src.find("### 2026-09-01").unwrap();
    only_inserted(&src, &out.candidate, at, "Line one\r\nline two\r\n\r\n");
}

// ---------------------------------------------------------------------------
// add_key

fn add_key(src: &str, key: &str, value: Value) -> Result<Outcome, Diagnostic> {
    run(
        src,
        Op::AddKey {
            key: key.into(),
            value,
            guard: doc_guard(src),
        },
    )
}

#[test]
fn add_key_writes_one_line_before_the_closing_fence() {
    let at = DOC.find("---\n\n# Widgets").unwrap();
    for (value, line) in [
        (json!("2026-10-10"), "updated: 2026-10-10\n"),
        (json!("a: b"), "updated: 'a: b'\n"),
        (json!(3), "updated: 3\n"),
        (json!(true), "updated: true\n"),
        (json!(null), "updated: null\n"),
    ] {
        let out = add_key(DOC, "updated", value.clone()).unwrap();
        only_inserted(DOC, &out.candidate, at, line);
        let doc = Document::parse(&out.candidate).unwrap();
        let key = doc
            .resolve(&Selector::FrontmatterKey {
                key: "updated".into(),
            })
            .unwrap();
        let Target::FrontmatterKey(k) = key else {
            panic!()
        };
        assert_eq!(doc.scalars()[k].value, value);
        assert_eq!(
            out.ops[0].revision.as_deref(),
            Some(doc.target_revision(key).as_str())
        );
    }
    // After a block list, at column 0.
    let src = "---\naliases:\n  - a\n  - b\n---\n\n# T\n";
    let out = add_key(src, "owner", json!("sam")).unwrap();
    assert_eq!(
        out.candidate,
        "---\naliases:\n  - a\n  - b\nowner: sam\n---\n\n# T\n"
    );
    let crlf = DOC.replace('\n', "\r\n");
    let out = add_key(&crlf, "updated", json!("x")).unwrap();
    let at = crlf.find("---\r\n\r\n# Widgets").unwrap();
    only_inserted(&crlf, &out.candidate, at, "updated: x\r\n");
}

#[test]
fn add_key_refuses_an_existing_key_a_missing_frontmatter_and_odd_values() {
    assert_eq!(
        code(add_key(DOC, "owner", json!("x"))),
        ErrorCode::AlreadyExists
    );
    assert_eq!(
        code(add_key("# T\n", "owner", json!("x"))),
        ErrorCode::UnsupportedSyntax
    );
    for key in ["", "a b", "-a", "a:b", "ключ"] {
        assert_eq!(
            code(add_key(DOC, key, json!("x"))),
            ErrorCode::InvalidRequest,
            "{key:?}"
        );
    }
    for value in [json!({"a": 1}), json!(["a"]), json!("line\nbreak")] {
        assert_eq!(
            code(add_key(DOC, "k", value.clone())),
            ErrorCode::UnsupportedSyntax,
            "{value}"
        );
    }
    let stale = run(
        &stamped(DOC),
        Op::AddKey {
            key: "k".into(),
            value: json!("x"),
            guard: doc_guard(DOC),
        },
    );
    assert_eq!(code(stale), ErrorCode::GuardConflict);
}

// ---------------------------------------------------------------------------
// rejoin

fn rejoin(src: &str, line: usize) -> Result<Outcome, Diagnostic> {
    let select = heading(&["Action"]);
    let guard = guard(src, &select);
    run(
        src,
        Op::Rejoin {
            select,
            guard,
            line,
        },
    )
}

fn line_of(src: &str, needle: &str) -> usize {
    src[..src.find(needle).unwrap()].matches('\n').count() + 1
}

#[test]
fn rejoin_moves_the_break_in_front_of_the_span_or_joins_the_lines() {
    let src = stamped(DOC);
    // Text before the span: the break moves in front of it.
    let out = rejoin(&src, line_of(&src, "See `the long")).unwrap();
    let want = src.replace(
        "See `the long\n      code span` for the gate.",
        "See\n      `the long code span` for the gate.",
    );
    assert_eq!(undigested(&out.candidate), undigested(&want));
    assert_current(&out.candidate);
    assert_eq!(attachments(&out.candidate), attachments(&src));
    // Only the item marker before it: the two lines join.
    let out = rejoin(&src, line_of(&src, "- [ ] `long")).unwrap();
    let want = src.replace("- [ ] `long\n      span` opens", "- [ ] `long span` opens");
    assert_eq!(undigested(&out.candidate), undigested(&want));
    assert_current(&out.candidate);
    assert_eq!(attachments(&out.candidate), attachments(&src));
    // CRLF: the moved break keeps its terminator.
    let crlf = src.replace('\n', "\r\n");
    let out = rejoin(&crlf, line_of(&crlf, "See `the long")).unwrap();
    assert_eq!(undigested(&out.candidate), undigested(&want_crlf(&crlf)));
    // CRLF, joined: the CRLF between the lines becomes the one space.
    let out = rejoin(&crlf, line_of(&crlf, "- [ ] `long")).unwrap();
    assert!(!out.candidate.replace("\r\n", "").contains('\n'));
    assert!(out.candidate.contains("- [ ] `long span` opens"));
    assert_eq!(attachments(&out.candidate), attachments(&src));
    assert!(!out.candidate.replace("\r\n", "").contains('\n'));
}

fn assert_current(src: &str) {
    let doc = Document::parse(src).unwrap();
    for i in 0..doc.markers.len() {
        if let Some((fresh, Some(stored))) = doc.marker_evidence(i) {
            assert!(
                fresh.starts_with(&stored),
                "stale digest on {:?} in {src}",
                doc.markers[i].lexical.id
            );
        }
    }
}

/// Stored digests masked.
fn undigested(src: &str) -> String {
    let mut out = String::new();
    let mut rest = src;
    while let Some(i) = rest.find("sha256:") {
        out.push_str(&rest[..i + 7]);
        out.push_str("<digest>");
        rest = &rest[i + 7 + 12..];
    }
    out.push_str(rest);
    out
}

fn want_crlf(crlf: &str) -> String {
    crlf.replace(
        "See `the long\r\n      code span` for the gate.",
        "See\r\n      `the long code span` for the gate.",
    )
}

#[test]
fn rejoin_refuses_a_malformed_or_ambiguous_carrier() {
    let src = stamped(DOC);
    // The item's last line: the next line is not inside it.
    assert_eq!(
        code(rejoin(&src, line_of(&src, "- [ ] **Plain.**"))),
        ErrorCode::InvalidRequest
    );
    // Inside one item, but no span wraps from this line.
    let plain = "# T

## Action

- [ ] a `b` c
  d
";
    assert_eq!(code(rejoin(plain, 5)), ErrorCode::MissingSelector);
    // Not inside a list item.
    let para = "# T\n\n## Action\n\nSee `the long\ncode span` here.\n";
    assert_eq!(code(rejoin(para, 5)), ErrorCode::InvalidRequest);
    // Outside the selected section, and past the end.
    assert_eq!(code(rejoin(&src, 1)), ErrorCode::InvalidRequest);
    assert_eq!(code(rejoin(&src, 10_000)), ErrorCode::InvalidRequest);
    // A span over three lines is not one wrapped break.
    let three = "# T\n\n## Action\n\n- [ ] a `b\n  c\n  d` e\n";
    assert_eq!(code(rejoin(three, 5)), ErrorCode::MissingSelector);
    // A stay on the opening line after the span would move.
    // A backslash before the span would end the line as a hard break.
    let slash = "# T\n\n## Action\n\n- [ ] see \\ `long\n  span` here\n";
    assert_eq!(code(rejoin(slash, 5)), ErrorCode::InvalidContent);
    let after = "# T\n\n## Action\n\n- [ ] a `b <!-- stay:abcDEF12 -->\n  c` d\n";
    assert_eq!(code(rejoin(after, 5)), ErrorCode::UnsupportedSyntax);
    let stale = run(
        &src,
        Op::Rejoin {
            select: heading(&["Action"]),
            guard: guard(DOC, &heading(&["Action"])),
            line: line_of(&src, "See `the long"),
        },
    );
    assert_eq!(code(stale), ErrorCode::GuardConflict);
}

// ---------------------------------------------------------------------------
// adopt_stay

const HAND: &str = "# T\n\n## Action\n\n- [ ] **One.** Written by hand. <!-- stay:handID01 -->\n- [ ] **Two.** Plain.\n";

fn adopt(src: &str, id: &str) -> Result<Outcome, Diagnostic> {
    let guard = Document::parse(src)
        .ok()
        .and_then(|d| {
            d.resolve(&Selector::Id { id: id.into() })
                .ok()
                .map(|t| d.target_revision(t))
        })
        .unwrap_or_else(|| doc_guard(src));
    run(
        src,
        Op::AdoptStay {
            id: id.into(),
            guard,
        },
    )
}

#[test]
fn adopt_stay_turns_a_hand_written_marker_into_the_items_stay_in_place() {
    let before = attachments(HAND);
    assert_eq!(before.get("handID01"), Some(&"block"));
    let out = adopt(HAND, "handID01").unwrap();
    let at = HAND.find("<!-- stay:handID01 -->").unwrap();
    let end = at + "<!-- stay:handID01 -->".len();
    assert_eq!(&out.candidate[..at], &HAND[..at]);
    let tail = &HAND[end..];
    assert!(out.candidate.ends_with(tail));
    let marker = &out.candidate[at..out.candidate.len() - tail.len()];
    assert!(
        marker.starts_with("<!-- stay:handID01 subhash=sha256:"),
        "{marker}"
    );
    assert!(
        !marker.contains("000000000000"),
        "the digest is filled: {marker}"
    );
    assert_eq!(attachments(&out.candidate).get("handID01"), Some(&"item"));
    assert_eq!(out.ops[0].id.as_deref(), Some("handID01"));
    // Among stamped items, with a container marker: everything else stays.
    let src = stamped(DOC);
    let hand = src.replace(
        "- [ ] **Plain.**",
        "- [ ] **Hand.** Added later. <!-- stay:handID02 -->\n- [ ] **Plain.**",
    );
    assert_ne!(hand, src);
    let out = adopt(&hand, "handID02").unwrap();
    let mut want = attachments(&hand);
    assert_eq!(want.insert("handID02".into(), "item"), Some("block"));
    assert_eq!(attachments(&out.candidate), want);
    // The adopted digest is current; the container's stays as stale as the
    // hand insertion left it, since this write did not change the list body.
    let doc = Document::parse(&out.candidate).unwrap();
    let m = doc
        .markers
        .iter()
        .position(|m| m.lexical.id.as_deref() == Some("handID02"))
        .unwrap();
    let (fresh, stored) = doc.marker_evidence(m).unwrap();
    assert!(fresh.starts_with(&stored.unwrap()));
    // CRLF.
    let crlf = HAND.replace('\n', "\r\n");
    let out = adopt(&crlf, "handID01").unwrap();
    assert!(!out.candidate.replace("\r\n", "").contains('\n'));
    assert_eq!(attachments(&out.candidate).get("handID01"), Some(&"item"));
}

/// A lone hand-written stay already addresses its item for anyone citing it,
/// so `stamp` leaves the item to `adopt_stay` rather than minting a second id
/// beside it, open or closed, and reports the skip.
#[test]
fn stamp_leaves_an_item_with_a_lone_bare_stay_to_adopt_stay() {
    // A continuation line gives a second id somewhere to go besides the first line.
    let hand = HAND.replace(
        "<!-- stay:handID01 -->\n",
        "<!-- stay:handID01 -->\n  Continued.\n",
    );
    for tick in ["[ ]", "[x]"] {
        let src = hand.replace("- [ ] **One.**", &format!("- {tick} **One.**"));
        let out = run(
            &src,
            Op::Stamp {
                collection: None,
                checkbox_only: false,
                collection_guard: None,
                select: Selector::Document,
                guard: doc_guard(&src),
            },
        )
        .unwrap();
        // Item One, checkbox to the start of Two, is byte-identical.
        let span = |s: &str| {
            let a = s.find(&format!("- {tick} **One.**")).unwrap();
            let z = s.find("- [ ] **Two.**").unwrap();
            s[a..z].to_string()
        };
        assert_eq!(span(&out.candidate), span(&src), "{tick}");
        let two = out
            .candidate
            .lines()
            .find(|l| l.contains("**Two.**"))
            .unwrap();
        assert!(two.contains("subhash=sha256:"), "{tick}: {two}");
        let refused = &out.ops[0].refused;
        assert!(
            refused
                .iter()
                .any(|d| d.message.contains("handID01") && d.message.contains("adopt_stay")),
            "{tick}: {refused:?}"
        );
    }
}

/// Stamp then `adopt_stay` lands where a fresh stamp would: one id per item
/// and a container stay, which the skipped item's stay does not stand in for.
/// A skip that leaves nothing else to stamp writes nothing.
#[test]
fn stamp_skips_only_the_items_own_hand_written_stays() {
    let out = stamped(HAND);
    let ids = attachments(&out);
    assert_eq!(ids.get("handID01"), Some(&"block"));
    assert_eq!(ids.values().filter(|k| **k == "item").count(), 1, "{out}");
    assert_eq!(ids.values().filter(|k| **k == "block").count(), 2, "{out}");
    let adopted = adopt(&out, "handID01").unwrap().candidate;
    let ids = attachments(&adopted);
    assert_eq!(ids.get("handID01"), Some(&"item"));
    assert_eq!(ids.values().filter(|k| **k == "item").count(), 2);
    assert_eq!(ids.values().filter(|k| **k == "block").count(), 1);
    assert_eq!(stamped(&adopted), adopted, "nothing left to stamp");

    // Nothing else to stamp: byte-identical, the skip still reported.
    let alone = "- [ ] **One.** Hand. <!-- stay:handID01 -->\n";
    let out = run(
        alone,
        Op::Stamp {
            collection: None,
            checkbox_only: false,
            collection_guard: None,
            select: Selector::Document,
            guard: doc_guard(alone),
        },
    )
    .unwrap();
    assert_eq!(out.candidate, alone);
    assert_eq!(out.ops[0].refused.len(), 1);
    assert_eq!(out.ops[0].refused[0].id.as_deref(), Some("handID01"));

    // Two hand-written stays: skipped and both named. Neither stands in for
    // the container stay, which Two's stamp mints.
    let two = HAND.replace("-->\n", "--> <!-- stay:handID02 -->\n");
    let out = run(
        &two,
        Op::Stamp {
            collection: None,
            checkbox_only: false,
            collection_guard: None,
            select: Selector::Document,
            guard: doc_guard(&two),
        },
    )
    .unwrap();
    let one = out
        .candidate
        .lines()
        .find(|l| l.contains("**One.**"))
        .unwrap();
    assert!(one.ends_with("<!-- stay:handID02 -->"), "{one}");
    let refused = &out.ops[0].refused;
    assert!(
        refused
            .iter()
            .any(|d| d.message.contains("handID01, handID02") && d.id.is_none()),
        "{refused:?}"
    );
    assert_eq!(
        attachments(&out.candidate)
            .values()
            .filter(|k| **k == "block")
            .count(),
        3,
        "{}",
        out.candidate
    );

    // A shape adopt_stay refuses is still skipped, and the message says what
    // to repair first rather than sending the caller to a refusal.
    for (shape, src, want) in [
        (
            "attributes",
            HAND.replace("stay:handID01 -->", "stay:handID01 foo=bar -->"),
            "reduce it to",
        ),
        (
            "carried twice",
            format!("{HAND}- [ ] **Three.** <!-- stay:handID01 -->\n"),
            "drop the other marker carrying handID01 by hand, then adopt",
        ),
        (
            "carried twice, with attributes",
            format!(
                "{}- [ ] **Three.** <!-- stay:handID01 -->\n",
                HAND.replace("stay:handID01 -->", "stay:handID01 foo=bar -->")
            ),
            "reduce it to `<!-- stay:handID01 -->` and drop the other marker carrying handID01",
        ),
        (
            "carried again in a nested item",
            HAND.replace(
                "<!-- stay:handID01 -->\n",
                "<!-- stay:handID01 -->\n  - Nested. <!-- stay:handID01 -->\n",
            ),
            "drop the other marker carrying handID01 by hand, then adopt",
        ),
        (
            "heading id",
            HAND.replace("## Action", "## Action {#handID01}"),
            "rename the heading `{#handID01}` by hand",
        ),
        (
            "carrier",
            HAND.replace("Written by hand.", "Written a < b."),
            "clear its carrier (",
        ),
    ] {
        let out = run(
            &src,
            Op::Stamp {
                collection: None,
                checkbox_only: false,
                collection_guard: None,
                select: Selector::Document,
                guard: doc_guard(&src),
            },
        )
        .unwrap();
        let one = out
            .candidate
            .lines()
            .find(|l| l.contains("**One.**"))
            .unwrap();
        assert!(!one.contains("subhash"), "{shape}: {one}");
        let refused = &out.ops[0].refused;
        assert!(
            refused.iter().any(|d| d.message.contains(want)),
            "{shape}: {refused:?}"
        );
        assert!(adopt(&src, "handID01").is_err(), "{shape}");
    }

    // Two stays: what keeping each would need is named per id.
    let src = HAND.replace(
        "<!-- stay:handID01 -->",
        "<!-- stay:handID01 foo=bar --> <!-- stay:handID02 -->",
    );
    let out = run(
        &src,
        Op::Stamp {
            collection: None,
            checkbox_only: false,
            collection_guard: None,
            select: Selector::Document,
            guard: doc_guard(&src),
        },
    )
    .unwrap();
    let message = &out.ops[0].refused[0].message;
    assert!(
        message.contains("to keep handID01, first reduce it to"),
        "{message}"
    );
    assert!(!message.contains("to keep handID02"), "{message}");
    // Deleting a stay between two code spans joins their backtick runs, so
    // what was code becomes carrier text; masking it would hide that.
    let glued = "- [ ] `a < b`<!-- stay:dropID01 -->``c`` tail <!-- stay:keepID01 -->\n";
    let out = run(
        glued,
        Op::Stamp {
            collection: None,
            checkbox_only: false,
            collection_guard: None,
            select: Selector::Document,
            guard: doc_guard(glued),
        },
    )
    .unwrap();
    let message = &out.ops[0].refused[0].message;
    assert!(
        message.contains("to keep keepID01, first clear its carrier"),
        "{message}"
    );
    let dropped = glued.replace("<!-- stay:dropID01 -->", "");
    assert!(adopt(&dropped, "keepID01").is_err());

    // A digest-carrying stay, or a bare one in a nested item, is not the
    // item's own hand-written stay: the item is stamped as before.
    for (shape, from, to) in [
        (
            "hash",
            "stay:handID01 -->",
            "stay:handID01 hash=sha256:000000000000 -->",
        ),
        (
            "nested",
            "Written by hand. <!-- stay:handID01 -->\n",
            "Written by hand.\n  - Nested. <!-- stay:handID01 -->\n",
        ),
    ] {
        let src = HAND.replace(from, to);
        assert_ne!(src, HAND);
        let out = run(
            &src,
            Op::Stamp {
                collection: None,
                checkbox_only: false,
                collection_guard: None,
                select: Selector::Document,
                guard: doc_guard(&src),
            },
        )
        .unwrap();
        assert!(
            out.ops[0].refused.is_empty(),
            "{shape}: {:?}",
            out.ops[0].refused
        );
        assert_eq!(
            attachments(&out.candidate)
                .values()
                .filter(|k| **k == "item")
                .count(),
            2,
            "{shape}: {}",
            out.candidate
        );
    }
}

#[test]
fn adopt_stay_refuses_a_malformed_or_ambiguous_carrier() {
    // Missing, and carried twice.
    assert_eq!(code(adopt(HAND, "nothere1")), ErrorCode::MissingSelector);
    let twice = format!("{HAND}- [ ] **Three.** <!-- stay:handID01 -->\n");
    assert_eq!(
        code(adopt(&twice, "handID01")),
        ErrorCode::AmbiguousSelector
    );
    // A marker with attributes, and the list's own marker line.
    let hashed = HAND.replace(
        "stay:handID01 -->",
        "stay:handID01 hash=sha256:000000000000 -->",
    );
    let refused = adopt(&hashed, "handID01").map(|o| o.candidate).unwrap_err();
    assert_eq!(refused.code, ErrorCode::UnsupportedSyntax);
    assert_eq!(
        refused.message,
        "stay handID01 carries attributes; only a bare `<!-- stay:handID01 -->` is adopted"
    );
    let container = "# T\n\n## Action\n\n- [ ] **One.**\n- [ ] **Two.**\n<!-- stay:listID01 -->\n";
    assert_eq!(
        code(adopt(container, "listID01")),
        ErrorCode::InvalidRequest
    );
    // In a nested item, which owns no child stay.
    let nested = "---\na: b\n---\n\n# T\n\n## Action\n\n- [ ] **One.**\n  - nested <!-- stay:handID05 -->\n- [ ] **Two.**\n";
    assert_eq!(code(adopt(nested, "handID05")), ErrorCode::InvalidRequest);
    // Beside Rivet's own stay: a second address, left for a person.
    let src = stamped("# T\n\n## Action\n\n- [ ] **One.**\n- [ ] **Two.**\n");
    let both = src.replacen("**One.** <!--", "**One.** <!-- stay:handID03 --> <!--", 1);
    assert!(both.contains("handID03"));
    assert_eq!(code(adopt(&both, "handID03")), ErrorCode::UnsupportedSyntax);
    // Behind a `<` in the list (markstay §3.4).
    let blocked = "# T\n\n## Action\n\n- [ ] a <b>\n- [ ] **One.** <!-- stay:handID01 -->\n";
    assert_eq!(code(adopt(blocked, "handID01")), ErrorCode::StampRefused);
    // A stale guard.
    let stale = run(
        HAND,
        Op::AdoptStay {
            id: "handID01".into(),
            guard: doc_guard(HAND),
        },
    );
    assert_eq!(code(stale), ErrorCode::GuardConflict);
}

#[test]
fn rejoin_keeps_whitespace_inside_the_span_and_works_at_the_end_of_a_file() {
    for nl in ["\n", "\r\n"] {
        // Two spaces at the end of the opening line are the span's content.
        let src = format!(
            "# T{nl}{nl}## Action{nl}{nl}- [ ] See `a  {nl}  b` here.{nl}- [ ] `c  {nl}  d` e.{nl}"
        );
        let out = rejoin(&src, 5).unwrap();
        assert_eq!(
            out.candidate,
            src.replace(&format!("See `a  {nl}  b`"), &format!("See{nl}  `a   b`"))
        );
        let out = rejoin(&src, 7).unwrap();
        assert_eq!(
            out.candidate,
            src.replace(&format!("`c  {nl}  d`"), "`c   d`")
        );
        // No final newline after the closing line.
        let src = format!("# T{nl}{nl}## Action{nl}{nl}- [ ] See `long{nl}  span` here.");
        let out = rejoin(&src, 5).unwrap();
        assert_eq!(
            out.candidate,
            format!("# T{nl}{nl}## Action{nl}{nl}- [ ] See{nl}  `long span` here.")
        );
        assert_eq!(code(rejoin(&src, 6)), ErrorCode::InvalidRequest);
    }
}

#[test]
fn rejoin_accepts_a_parent_section_for_a_descendant_item() {
    let raw = "# Parent\n\n## Child\n\n- [ ] **One.** See `long\n      span` for the gate.\n- [ ] **Two.** Unchanged.\n\n# Peer\n\nUntouched.\n";
    for newline in ["\n", "\r\n"] {
        let src = stamped(&raw.replace('\n', newline));
        let line = line_of(&src, "See `long");
        let parent = heading(&["Parent"]);
        let out = run(
            &src,
            Op::Rejoin {
                guard: guard(&src, &parent),
                select: parent,
                line,
            },
        )
        .unwrap();
        let want = src.replace(
            &format!("See `long{newline}      span`"),
            &format!("See{newline}      `long span`"),
        );
        assert_eq!(undigested(&out.candidate), undigested(&want));
        assert_eq!(attachments(&out.candidate), attachments(&src));
        assert_current(&out.candidate);
        let peer = heading(&["Peer"]);
        let refused = run(
            &src,
            Op::Rejoin {
                guard: guard(&src, &peer),
                select: peer,
                line,
            },
        )
        .unwrap_err();
        assert_eq!(refused.code, ErrorCode::InvalidRequest);
    }
}
