use markrivet_core::document::{Attach, Target};
use markrivet_core::edit::{self, Op, Outcome, Planner};
use markrivet_core::{
    BlockerOrigin, Diagnostic, Document, ErrorCode, Position, SectionPart, Selector,
};
use serde_json::json;

const TRACKER: &str = "---
status: active
owner: sam
---

# Widget tracker

> Scope paragraph.

## Action

- [ ] **Ship the widget. (2026-09-01)** Context a session needs,
      wrapped onto a second line.
      **Gate**: `make check` passes.

- [ ] `agent` **Refresh the fixtures.**
      **Gate**: fixture diff is empty.
  - a nested note

- [x] `human:sam` **Closed item.**
      **Gate**: reply on the thread.

## Log

Newest first.

### 2026-09-01: tracker opened

First entry.
";

fn run(src: &str, ops: &[Op]) -> Result<Outcome, Diagnostic> {
    let doc = Document::parse(src).unwrap();
    let mut planner = Planner::new(&doc);
    let changes = ops
        .iter()
        .map(|op| planner.structural(op))
        .collect::<Result<Vec<_>, _>>()?;
    edit::plan(&doc, changes)
}

fn stamp(src: &str) -> String {
    let doc = Document::parse(src).unwrap();
    let guard = doc.revision();
    run(
        src,
        &[Op::Stamp {
            collection: None,
            checkbox_only: false,
            collection_guard: None,
            select: Selector::Document,
            guard,
        }],
    )
    .unwrap()
    .candidate
}

fn guard(src: &str, selector: &Selector) -> String {
    let doc = Document::parse(src).unwrap();
    doc.target_revision(doc.resolve(selector).unwrap())
}

/// Every stored digest in the document matches its unit (no drift).
fn assert_evidence_current(src: &str) {
    let doc = Document::parse(src).unwrap();
    for (i, m) in doc.markers.iter().enumerate() {
        if let Some((fresh, Some(stored))) = doc.marker_evidence(i) {
            assert!(
                fresh.starts_with(&stored),
                "stale evidence on {:?}",
                m.lexical.id
            );
        }
    }
}

fn item_ids(src: &str) -> Vec<String> {
    let doc = Document::parse(src).unwrap();
    doc.lists
        .iter()
        .flat_map(|l| l.items.iter().flat_map(|i| doc.stay_ids(&i.markers)))
        .collect()
}

#[test]
fn stamp_mints_sections_items_and_container_once() {
    let stamped = stamp(TRACKER);
    let doc = Document::parse(&stamped).unwrap();
    assert!(doc.diagnostics.is_empty(), "{:?}", doc.diagnostics);
    assert_eq!(doc.sections.len(), 4);
    for s in 0..doc.sections.len() {
        assert_eq!(doc.section_ids(s).len(), 1);
    }
    assert_eq!(item_ids(&stamped).len(), 3);
    let list = &doc.lists[0];
    assert_eq!(doc.stay_ids(&doc.blocks[list.block].markers).len(), 1);
    assert_evidence_current(&stamped);
    // Only markers were added: removing them restores the original bytes.
    let mut restored = stamped.clone();
    for m in doc.markers.iter().rev() {
        let s = m.lexical.span;
        let line_start = restored[..s.start].rfind('\n').map_or(0, |i| i + 1);
        let line_end = restored[s.end..]
            .find('\n')
            .map_or(restored.len(), |i| s.end + i);
        if restored[line_start..s.start].trim().is_empty()
            && restored[s.end..line_end].trim().is_empty()
        {
            restored.replace_range(line_start - 1..line_end, "");
        } else {
            restored.replace_range(s.start - 1..s.end, "");
        }
    }
    assert_eq!(restored, TRACKER);
    // The nested note is not a direct item and gets nothing.
    assert!(!stamped.contains("a nested note <!--"));
    assert_eq!(stamp(&stamped), stamped, "stamping is idempotent");
}

#[test]
fn set_item_moves_stay_refreshes_evidence_and_preserves_neighbours() {
    let stamped = stamp(TRACKER);
    let ids = item_ids(&stamped);
    let select = Selector::Id { id: ids[0].clone() };
    let out = run(
        &stamped,
        &[Op::Set {
            collection: None,
            typed_guard: None,
            select: select.clone(),
            part: None,
            guard: guard(&stamped, &select),
            content: json!(
                "[x] **Ship the widget. (2026-09-01)** Shipped.\n    **Gate**: `make check` passes."
            ),
            allow_drop: false,
        }],
    )
    .unwrap();
    let doc = Document::parse(&out.candidate).unwrap();
    let Target::Item { list, item } = doc.resolve(&select).unwrap() else {
        panic!("item")
    };
    assert_eq!(item, 0);
    assert!(doc.lists[list].items[0].checkbox.unwrap().checked);
    assert_eq!(
        doc.item_content(list, 0),
        "[x] **Ship the widget. (2026-09-01)** Shipped.\n    **Gate**: `make check` passes."
    );
    assert_evidence_current(&out.candidate);
    assert_eq!(
        out.ops[0].revision.as_deref(),
        Some(doc.target_revision(Target::Item { list, item }).as_str())
    );
    // Items 2 and 3 are byte-identical; only the item and its list's digest changed.
    let before = Document::parse(&stamped).unwrap();
    for i in 1..3 {
        let a = before.lists[0].items[i].span;
        let b = doc.lists[0].items[i].span;
        assert_eq!(&stamped[a.start..a.end], &out.candidate[b.start..b.end]);
    }
    assert!(
        out.evidence
            .iter()
            .any(|e| e.key == "subhash" && !e.prior_drift)
    );
    assert!(
        out.evidence
            .iter()
            .any(|e| e.key == "hash" && !e.prior_drift)
    );
}

/// Source with its stay markers removed, for comparing the prose around them.
fn unmarked(src: &str) -> String {
    let mut out = String::new();
    for line in src.split_inclusive('\n') {
        let mut line = line.to_string();
        let marked = line.contains("<!-- stay:");
        while let Some(start) = line.find("<!-- stay:") {
            let end = start + line[start..].find("-->").unwrap() + 3;
            let start = line[..start].trim_end_matches(' ').len();
            line.replace_range(start..end, "");
        }
        if !(marked && line.trim().is_empty()) {
            out.push_str(&line);
        }
    }
    out
}

/// A raw item write keeps the source indentation of each line it repeats (a
/// tab, a lazy line, under CRLF) and gives a new line the item's width. A
/// repeated lazy line is kept at column 0 only where it still reads as part
/// of the item: after a blank line column 0 starts a new block, so the
/// caller's line is rendered at the width rather than refused, while the tab
/// beside it, which is exactly the width, is still kept.
#[test]
fn set_item_keeps_indentation_only_where_it_reads_the_same() {
    let src = "# T\r\n\r\n## Action\r\n\r\n-\t[ ] **A** Top\r\n\t**Gate**: tab\r\nlazy line\r\n\r\n-\t[ ] **B**\r\n\t**Owner**: tab\r\n**Gate**: lazy\r\n";
    let stamped = stamp(src);
    let ids = item_ids(&stamped);
    let cases = [
        (
            0,
            "[x] **A** Top\n**Gate**: tab\nlazy line\nnew line",
            "-\t[x] **A** Top\r\n\t**Gate**: tab\r\nlazy line\r\n    new line\r\n",
        ),
        (
            1,
            "[x] **B**\n**Owner**: tab\n\n**Gate**: lazy",
            "-\t[x] **B**\r\n\t**Owner**: tab\r\n\r\n    **Gate**: lazy\r\n",
        ),
    ];
    for (n, content, expected) in cases {
        let select = Selector::Id { id: ids[n].clone() };
        let out = run(
            &stamped,
            &[Op::Set {
                collection: None,
                typed_guard: None,
                select: select.clone(),
                part: None,
                guard: guard(&stamped, &select),
                content: json!(content),
                allow_drop: false,
            }],
        )
        .unwrap_or_else(|e| panic!("case {n}: {e:?}"));
        let doc = Document::parse(&out.candidate).unwrap();
        let Target::Item { list, item } = doc.resolve(&select).unwrap() else {
            panic!("case {n}: item")
        };
        let span = doc.lists[list].items[item].span;
        let written = unmarked(&out.candidate[span.start..]);
        assert!(
            written.starts_with(expected),
            "case {n}:\n{written:?}\nexpected prefix {expected:?}"
        );
        assert_evidence_current(&out.candidate);
    }
}

#[test]
fn stale_guard_conflicts_and_unrelated_edit_does_not() {
    let stamped = stamp(TRACKER);
    let ids = item_ids(&stamped);
    let first = Selector::Id { id: ids[0].clone() };
    let second = Selector::Id { id: ids[1].clone() };
    let g1 = guard(&stamped, &first);
    let g2 = guard(&stamped, &second);
    let edited = run(
        &stamped,
        &[Op::Set {
            collection: None,
            typed_guard: None,
            select: second.clone(),
            part: None,
            guard: g2.clone(),
            content: json!("[ ] `agent` **Refresh the fixtures again.**"),
            allow_drop: false,
        }],
    )
    .unwrap()
    .candidate;
    // The first item's guard still holds after an edit to its sibling.
    run(
        &edited,
        &[Op::Set {
            collection: None,
            typed_guard: None,
            select: first,
            part: None,
            guard: g1,
            content: json!("[x] **Ship the widget. (2026-09-01)** Done."),
            allow_drop: false,
        }],
    )
    .unwrap();
    // The second item's old guard is stale.
    let e = run(
        &edited,
        &[Op::Set {
            collection: None,
            typed_guard: None,
            select: second,
            part: None,
            guard: g2,
            content: json!("[x] `agent` **Refresh the fixtures.**"),
            allow_drop: false,
        }],
    )
    .unwrap_err();
    assert_eq!(e.code, ErrorCode::GuardConflict);
    // A section guard sees a descendant's change.
    let action = Selector::HeadingPath {
        path: vec!["Action".into()],
    };
    let before = guard(&stamped, &action);
    let e = run(
        &edited,
        &[Op::Set {
            collection: None,
            typed_guard: None,
            select: action,
            part: Some(SectionPart::Title),
            guard: before,
            content: json!("Actions"),
            allow_drop: false,
        }],
    )
    .unwrap_err();
    assert_eq!(e.code, ErrorCode::GuardConflict);
}

#[test]
fn content_rules_are_enforced() {
    let stamped = stamp(TRACKER);
    let ids = item_ids(&stamped);
    let select = Selector::Id { id: ids[0].clone() };
    let g = guard(&stamped, &select);
    let attempt = |content: &str| {
        run(
            &stamped,
            &[Op::Set {
                collection: None,
                typed_guard: None,
                select: select.clone(),
                part: None,
                guard: g.clone(),
                content: json!(content),
                allow_drop: false,
            }],
        )
    };
    assert_eq!(
        attempt("x <!-- stay:evil subhash=sha256:1 -->")
            .unwrap_err()
            .code,
        ErrorCode::InvalidContent
    );
    assert_eq!(
        attempt("   leading space").unwrap_err().code,
        ErrorCode::InvalidContent
    );
    assert_eq!(
        attempt("```\ncode only\n```").unwrap_err().code,
        ErrorCode::InvalidContent
    );
    // Moving a stay is an insertion (markstay §3.4): '<' or a backslash
    // anywhere before it refuses, by presence and not by meaning.
    for content in [
        "text with <textarea> open",
        "compare `a<b` and a < b",
        "a \\ path",
    ] {
        assert_eq!(
            attempt(content).unwrap_err().code,
            ErrorCode::StampRefused,
            "{content}"
        );
    }
    attempt("[ ] compare a and b").unwrap();
}

#[test]
fn a_write_may_not_break_a_later_stay_carrier() {
    let stamped = stamp(TRACKER);
    let doc = Document::parse(&stamped).unwrap();
    let list = Selector::Id {
        id: doc.stay_ids(&doc.blocks[doc.lists[0].block].markers)[0].clone(),
    };
    let insert = |position| Op::Insert {
        select: list.clone(),
        position,
        guard: guard(&stamped, &list),
        content: "[ ] compare a < b".into(),
        stamp: false,
    };
    // An unstamped item with '<' inserted before stamped ones would expose them.
    let e = run(&stamped, &[insert(Position::Start)]).unwrap_err();
    assert_eq!(e.code, ErrorCode::StampRefused);
    // Caught while checking the candidate, a document this refusal stops from
    // existing, so there is no position anyone could open.
    assert!(e.blocker.is_none());
    // Appended after them it exposes nothing and is allowed.
    run(&stamped, &[insert(Position::End)]).unwrap();
}

#[test]
fn dropped_ids_cannot_name_new_content() {
    let src = "# Parent\n\n- Old item <!-- stay:x subhash=sha256:1 -->\n";
    let parent = Selector::HeadingPath {
        path: vec!["Parent".into()],
    };
    let e = run(
        src,
        &[Op::Set {
            collection: None,
            typed_guard: None,
            select: parent.clone(),
            part: Some(SectionPart::Body),
            guard: guard(src, &parent),
            content: json!("\n## Replacement {#x}\n\nDifferent content.\n"),
            allow_drop: true,
        }],
    )
    .unwrap_err();
    assert_eq!(e.code, ErrorCode::InvalidContent);
    assert!(e.message.contains("dropped"), "{}", e.message);
}

#[test]
fn heading_attribute_fragments_parse_without_panicking() {
    for src in ["# {#\n", "# {#}\n", "# x {#\n", "# {#a\n", "## }{#\n"] {
        let doc = Document::parse(src).unwrap();
        let heading = doc.blocks[0].heading.as_ref().unwrap();
        assert!(heading.legacy_id.is_none(), "{src:?}");
    }
    let src = "# Title\n";
    let title = Selector::HeadingPath {
        path: vec!["Title".into()],
    };
    let out = run(
        src,
        &[Op::Set {
            collection: None,
            typed_guard: None,
            select: title.clone(),
            part: Some(SectionPart::Title),
            guard: guard(src, &title),
            content: json!("{#"),
            allow_drop: false,
        }],
    )
    .unwrap();
    assert_eq!(out.candidate, "# {#\n");
}

#[test]
fn insert_item_and_log_entry_in_one_batch() {
    let stamped = stamp(TRACKER);
    let doc = Document::parse(&stamped).unwrap();
    let list_id = doc.stay_ids(&doc.blocks[doc.lists[0].block].markers)[0].clone();
    let list = Selector::Id { id: list_id };
    let log = Selector::HeadingPath {
        path: vec!["Log".into()],
    };
    let out = run(
        &stamped,
        &[
            Op::Insert {
                select: list.clone(),
                position: Position::End,
                guard: guard(&stamped, &list),
                content: "[ ] **New action.**\n    **Gate**: it exists.".into(),
                stamp: true,
            },
            Op::Insert {
                select: log.clone(),
                position: Position::Start,
                guard: guard(&stamped, &log),
                content: "### 2026-09-11: added an action\n\nOne line.".into(),
                stamp: true,
            },
        ],
    )
    .unwrap();
    let doc = Document::parse(&out.candidate).unwrap();
    assert!(doc.diagnostics.is_empty(), "{:?}", doc.diagnostics);
    assert_eq!(doc.lists[0].items.len(), 4);
    assert_eq!(
        doc.item_content(0, 3),
        "[ ] **New action.**\n    **Gate**: it exists."
    );
    let new_item = &out.ops[0].minted[0];
    assert_eq!(new_item.kind, "item");
    assert!(matches!(
        doc.resolve(&Selector::Id {
            id: new_item.id.clone()
        }),
        Ok(Target::Item { item: 3, .. })
    ));
    let log_children: Vec<&str> = doc
        .sections
        .iter()
        .filter(|s| s.level == 3)
        .map(|s| {
            doc.blocks[s.block]
                .heading
                .as_ref()
                .unwrap()
                .title_text
                .as_str()
        })
        .collect();
    assert_eq!(
        log_children,
        ["2026-09-11: added an action", "2026-09-01: tracker opened"]
    );
    assert!(
        out.candidate
            .contains("Newest first.\n\n### 2026-09-11: added an action\n<!-- stay:")
    );
    assert!(out.candidate.contains("One line.\n\n### 2026-09-01"));
    // Loose list keeps a blank line between items.
    assert!(out.candidate.contains("reply on the thread. <!-- stay:"));
    assert_evidence_current(&out.candidate);
}

#[test]
fn list_reshape_refuses_an_item_edit_in_either_order() {
    let source = stamp(TRACKER);
    let doc = Document::parse(&source).unwrap();
    let list = Selector::Id {
        id: doc.stay_ids(&doc.blocks[doc.lists[0].block].markers)[0].clone(),
    };
    for position in [Position::Start, Position::End] {
        for reverse in [false, true] {
            let mut planner = Planner::new(&doc);
            let insert = planner
                .structural(&Op::Insert {
                    select: list.clone(),
                    position,
                    guard: guard(&source, &list),
                    content: "[ ] New action.".into(),
                    stamp: true,
                })
                .unwrap();
            let tick = planner.set_checkbox(0, 1, true).unwrap();
            let changes = if reverse {
                vec![tick, insert]
            } else {
                vec![insert, tick]
            };
            let error = edit::plan(&doc, changes).unwrap_err();
            assert_eq!(error.code, ErrorCode::InvalidRequest);
            assert!(error.message.contains("two operations touch the same unit"));
        }
    }
}

#[test]
fn overlapping_operations_are_rejected() {
    let stamped = stamp(TRACKER);
    let ids = item_ids(&stamped);
    let select = Selector::Id { id: ids[0].clone() };
    let g = guard(&stamped, &select);
    let op = Op::Set {
        collection: None,
        typed_guard: None,
        select,
        part: None,
        guard: g,
        content: json!("[x] done"),
        allow_drop: false,
    };
    let e = run(&stamped, &[op.clone(), op]).unwrap_err();
    assert_eq!(e.code, ErrorCode::InvalidRequest);
    let action = Selector::HeadingPath {
        path: vec!["Action".into()],
    };
    let e = run(
        &stamped,
        &[
            Op::Set {
                collection: None,
                typed_guard: None,
                select: Selector::Id { id: ids[1].clone() },
                part: None,
                guard: guard(&stamped, &Selector::Id { id: ids[1].clone() }),
                content: json!("[x] done"),
                allow_drop: false,
            },
            Op::Set {
                collection: None,
                typed_guard: None,
                select: action.clone(),
                part: Some(SectionPart::Body),
                guard: guard(&stamped, &action),
                content: json!("\nReplaced.\n\n"),
                allow_drop: true,
            },
        ],
    )
    .unwrap_err();
    assert_eq!(e.code, ErrorCode::InvalidRequest);
}

#[test]
fn section_body_needs_allow_drop_and_reports_dropped() {
    let stamped = stamp(TRACKER);
    let action = Selector::HeadingPath {
        path: vec!["Action".into()],
    };
    let g = guard(&stamped, &action);
    let op = |allow_drop| Op::Set {
        collection: None,
        typed_guard: None,
        select: action.clone(),
        part: Some(SectionPart::Body),
        guard: g.clone(),
        content: json!("\nNothing queued.\n\n"),
        allow_drop,
    };
    assert_eq!(
        run(&stamped, &[op(false)]).unwrap_err().code,
        ErrorCode::InvalidContent
    );
    let out = run(&stamped, &[op(true)]).unwrap();
    assert_eq!(out.ops[0].dropped.len(), 4);
    assert!(out.candidate.contains("## Action\n<!-- stay:"));
    assert!(out.candidate.contains("\n\nNothing queued.\n\n## Log"));
    // A body may not introduce a peer heading.
    let e = run(
        &stamped,
        &[Op::Set {
            collection: None,
            typed_guard: None,
            select: action,
            part: Some(SectionPart::Body),
            guard: g,
            content: json!("\n## Sneaky peer\n\n"),
            allow_drop: true,
        }],
    )
    .unwrap_err();
    assert_eq!(e.code, ErrorCode::InvalidContent);
}

/// A marker Rivet does not address still carries an id that markstay's
/// lexical drop check follows, so a body replacement that removes one has to
/// declare it like any other stay. Unstamped on purpose: stamping would give
/// the surrounding items and lists stays of their own, and the refusal would
/// be for those.
#[test]
fn section_body_needs_allow_drop_for_an_unaddressed_marker() {
    let cases = [
        (
            "nested item",
            "# H\n\n- parent\n  - child <!-- stay:n1 subhash=sha256:111111111111 -->\n",
            "n1",
            Attach::Unaddressed,
        ),
        (
            "table row",
            "# H\n\n| a | b |\n|---|---|\n| 1 | 2 <!-- stay:r1 subhash=sha256:111111111111 --> |\n",
            "r1",
            Attach::Unaddressed,
        ),
        (
            "marker-only subhash line",
            "# H\n\ntext\n<!-- stay:m1 subhash=sha256:111111111111 -->\n",
            "m1",
            Attach::Unaddressed,
        ),
    ];
    for (name, src, id, attach) in cases {
        let doc = Document::parse(src).unwrap();
        let [m] = doc.markers.as_slice() else {
            panic!("{name}: one marker expected, got {:?}", doc.markers);
        };
        assert_eq!(m.lexical.id.as_deref(), Some(id), "{name}");
        assert!(!m.lexical.malformed, "{name}");
        assert_eq!(m.attach, attach, "{name}");
        let h = Selector::HeadingPath {
            path: vec!["H".into()],
        };
        let op = |allow_drop| Op::Set {
            collection: None,
            typed_guard: None,
            select: h.clone(),
            part: Some(SectionPart::Body),
            guard: guard(src, &h),
            content: json!("\nplain text\n"),
            allow_drop,
        };
        let e = run(src, &[op(false)]).unwrap_err();
        assert_eq!(e.code, ErrorCode::InvalidContent, "{name}");
        assert!(e.message.contains(id), "{name}: {}", e.message);
        let out = run(src, &[op(true)]).unwrap();
        assert_eq!(out.ops[0].dropped, vec![id.to_string()], "{name}");
        assert_eq!(out.candidate, "# H\n\nplain text\n", "{name}");
    }
}

/// A delete is a declared drop, and its `dropped` names an unaddressed marker
/// it removes as well as the stays Rivet addresses.
#[test]
fn section_delete_reports_an_unaddressed_marker_it_removes() {
    let src = "# Keep\n\nkept\n\n# Gone\n\n- parent\n  - child <!-- stay:n1 subhash=sha256:111111111111 -->\n";
    let gone = Selector::HeadingPath {
        path: vec!["Gone".into()],
    };
    let out = run(
        src,
        &[Op::Delete {
            list: None,
            collection: None,
            select: gone.clone(),
            guard: guard(src, &gone),
        }],
    )
    .unwrap();
    assert_eq!(out.ops[0].dropped, vec!["n1".to_string()]);
    assert_eq!(out.candidate, "# Keep\n\nkept\n\n");
}

/// An unaddressed copy of an id that another marker outside the region still
/// carries does not leave the document, so removing it is no loss: neither a
/// body replacement nor a delete declares it, and the surviving copy keeps
/// naming its block.
#[test]
fn an_unaddressed_duplicate_that_survives_elsewhere_is_not_dropped() {
    let src = "# Keep\n\nkept\n<!-- stay:x -->\n\n# Gone\n\n- parent\n  - child <!-- stay:x subhash=sha256:111111111111 -->\n";
    let doc = Document::parse(src).unwrap();
    let attaches: Vec<_> = doc.markers.iter().map(|m| m.attach).collect();
    assert!(matches!(
        attaches.as_slice(),
        [Attach::Block(_), Attach::Unaddressed]
    ));
    let gone = Selector::HeadingPath {
        path: vec!["Gone".into()],
    };
    let out = run(
        src,
        &[Op::Set {
            collection: None,
            typed_guard: None,
            select: gone.clone(),
            part: Some(SectionPart::Body),
            guard: guard(src, &gone),
            content: json!("\nplain text\n"),
            allow_drop: false,
        }],
    )
    .unwrap();
    assert!(out.ops[0].dropped.is_empty());
    assert_eq!(
        out.candidate,
        "# Keep\n\nkept\n<!-- stay:x -->\n\n# Gone\n\nplain text\n"
    );
    let out = run(
        src,
        &[Op::Delete {
            list: None,
            collection: None,
            select: gone.clone(),
            guard: guard(src, &gone),
        }],
    )
    .unwrap();
    assert!(out.ops[0].dropped.is_empty());
    assert_eq!(out.candidate, "# Keep\n\nkept\n<!-- stay:x -->\n\n");
}

/// Each region alone leaves the other copy of a duplicated id as its
/// survivor, so neither declares it; together they remove every copy. The
/// batch is refused rather than losing the id undeclared.
#[test]
fn a_batch_removing_every_copy_of_an_unaddressed_id_is_refused() {
    let src = "# A\n\n- p\n  - c <!-- stay:x subhash=sha256:111111111111 -->\n\n# B\n\n- q\n  - d <!-- stay:x subhash=sha256:111111111111 -->\n";
    let doc = Document::parse(src).unwrap();
    assert!(
        doc.markers
            .iter()
            .all(|m| m.lexical.id.as_deref() == Some("x") && m.attach == Attach::Unaddressed)
    );
    assert_eq!(doc.markers.len(), 2);
    let section = |t: &str| Selector::HeadingPath {
        path: vec![t.into()],
    };
    let body_with = |t: &str, content: &str| Op::Set {
        collection: None,
        typed_guard: None,
        select: section(t),
        part: Some(SectionPart::Body),
        guard: guard(src, &section(t)),
        content: json!(content),
        allow_drop: true,
    };
    let body = |t: &str| body_with(t, "\nplain\n\n");
    let delete = |t: &str| Op::Delete {
        list: None,
        collection: None,
        select: section(t),
        guard: guard(src, &section(t)),
    };
    for (name, ops) in [
        ("bodies", vec![body("A"), body("B")]),
        ("deletes", vec![delete("A"), delete("B")]),
        // A heading attribute naming the id is not a marker carrying it.
        (
            "heading attribute",
            vec![body_with("A", "\n## Replacement {#x}\n\n"), body("B")],
        ),
    ] {
        let e = run(src, &ops).unwrap_err();
        assert_eq!(e.code, ErrorCode::InvalidContent, "{name}");
        assert!(
            e.message.contains("stay x would leave"),
            "{name}: {}",
            e.message
        );
    }
    // Either one alone keeps the other copy, so it applies.
    assert!(run(src, &[body("A")]).is_ok());
    assert!(run(src, &[delete("B")]).is_ok());
}

/// Content that opens like frontmatter is Markdown once spliced into a body,
/// so a marker inside it would go live: it is refused like any other supplied
/// marker, alone or as the survivor a batch would lean on.
#[test]
fn content_shaped_like_frontmatter_may_not_carry_a_marker() {
    let src = "# A\n\n- p\n  - c <!-- stay:x subhash=sha256:111111111111 -->\n\n# B\n\n- q\n  - d <!-- stay:x subhash=sha256:111111111111 -->\n";
    let fm = "---\nk: \"<!-- stay:x subhash=sha256:111111111111 -->\"\n---\n\n";
    let section = |t: &str| Selector::HeadingPath {
        path: vec![t.into()],
    };
    let body = |t: &str, content: &str| Op::Set {
        collection: None,
        typed_guard: None,
        select: section(t),
        part: Some(SectionPart::Body),
        guard: guard(src, &section(t)),
        content: json!(content),
        allow_drop: false,
    };
    for ops in [
        vec![body("A", fm)],
        vec![body("A", fm), body("B", "\nplain\n")],
    ] {
        let e = run(src, &ops).unwrap_err();
        assert_eq!(e.code, ErrorCode::InvalidContent);
        assert!(
            e.message.contains("must not contain stay markers"),
            "{}",
            e.message
        );
    }
    // The same frontmatter-shaped text without a marker is ordinary content.
    let plain = "---\nk: v\n---\n\n";
    assert!(run(src, &[body("A", plain)]).is_ok());
}

/// Supplied content that reads as no marker on its own can go live where it
/// lands. Each case removes both original copies of `x` and smuggles in a
/// live one to stand as their survivor; every one is refused, because a marker
/// inside written text must be minted or an existing stay the change moved.
#[test]
fn a_marker_that_goes_live_only_once_spliced_is_refused() {
    let src = "# A\n\n- p\n  - c <!-- stay:x subhash=sha256:111111111111 -->\n\n# B\n\n- q\n  - d <!-- stay:x subhash=sha256:111111111111 -->\n\n# Keep\n\n- p <!-- stay:i subhash=sha256:111111111111 -->\n";
    let section = |t: &str| Selector::HeadingPath {
        path: vec![t.into()],
    };
    let body = |t: &str, content: &str| Op::Set {
        collection: None,
        typed_guard: None,
        select: section(t),
        part: Some(SectionPart::Body),
        guard: guard(src, &section(t)),
        content: json!(content),
        allow_drop: false,
    };
    let title = |content: &str| Op::Set {
        collection: None,
        typed_guard: None,
        select: section("A"),
        part: Some(SectionPart::Title),
        guard: guard(src, &section("A")),
        content: json!(content),
        allow_drop: false,
    };
    let item = Selector::Id { id: "i".into() };
    let set_item = |content: &str| Op::Set {
        collection: None,
        typed_guard: None,
        select: item.clone(),
        part: None,
        guard: guard(src, &item),
        content: json!(content),
        allow_drop: false,
    };
    let m = "<!-- stay:x subhash=sha256:111111111111 -->";
    let cases = [
        (
            "frontmatter, wide marker spacing",
            vec![
                body(
                    "A",
                    "---\nk: \"<!--  stay:x subhash=sha256:111111111111 -->\"\n---\n\n",
                ),
                body("B", "\nplain\n\n"),
            ],
        ),
        (
            "leading BOM over indented code",
            vec![
                body("A", &format!("\u{feff}    {m}\n\n")),
                body("B", "\nplain\n\n"),
            ],
        ),
        (
            "title read as a fence",
            vec![
                title(&format!("~~~ {m}")),
                body("A", "\nplain\n\n"),
                body("B", "\nplain\n\n"),
            ],
        ),
        (
            "title read as a link definition",
            vec![
                title(&format!("[r]: /u \"{m}\"")),
                body("A", "\nplain\n\n"),
                body("B", "\nplain\n\n"),
            ],
        ),
        (
            "tab stop shifting under the item indent",
            vec![
                set_item(&format!("p\n\n\t- c {m}")),
                body("A", "\nplain\n\n"),
                body("B", "\nplain\n\n"),
            ],
        ),
    ];
    for (name, ops) in cases {
        let e = match run(src, &ops) {
            Ok(out) => panic!("{name}: applied as {:?}", out.candidate),
            Err(e) => e,
        };
        assert_eq!(e.code, ErrorCode::InvalidContent, "{name}");
        assert!(
            e.message
                .contains("stay marker (x) that came from the supplied content"),
            "{name}: {}",
            e.message
        );
    }
}

/// A title is replaced whole, so one holding a marker the heading does not
/// own is refused rather than losing it, and the file is left as it was.
#[test]
fn title_set_refuses_a_marker_the_heading_does_not_own() {
    let foreign = "# H <!-- stay:u subhash=sha256:111111111111 -->\n<!-- stay:h -->\n\nbody\n";
    let doc = Document::parse(foreign).unwrap();
    assert!(
        doc.markers
            .iter()
            .any(|m| m.lexical.id.as_deref() == Some("u") && m.attach == Attach::Unaddressed)
    );
    let h = Selector::Id { id: "h".into() };
    let e = run(
        foreign,
        &[Op::Set {
            collection: None,
            typed_guard: None,
            select: h.clone(),
            part: Some(SectionPart::Title),
            guard: guard(foreign, &h),
            content: json!("Renamed"),
            allow_drop: false,
        }],
    )
    .unwrap_err();
    assert_eq!(e.code, ErrorCode::UnsupportedSyntax);
    assert!(e.message.contains("title holds a marker"), "{}", e.message);
}

#[test]
fn frontmatter_and_title_edits_preserve_envelope() {
    let out = run(
        TRACKER,
        &[
            Op::Set {
                collection: None,
                typed_guard: None,
                select: Selector::FrontmatterKey {
                    key: "status".into(),
                },
                part: None,
                guard: guard(
                    TRACKER,
                    &Selector::FrontmatterKey {
                        key: "status".into(),
                    },
                ),
                content: json!("complete"),
                allow_drop: false,
            },
            Op::Set {
                collection: None,
                typed_guard: None,
                select: Selector::HeadingPath {
                    path: vec!["Widget tracker".into(), "Log".into()],
                },
                part: Some(SectionPart::Title),
                guard: guard(
                    TRACKER,
                    &Selector::HeadingPath {
                        path: vec!["Log".into()],
                    },
                ),
                content: json!("History"),
                allow_drop: false,
            },
        ],
    )
    .unwrap();
    assert_eq!(
        out.candidate,
        TRACKER
            .replace("status: active", "status: complete")
            .replace("## Log", "## History")
    );
    let e = run(
        TRACKER,
        &[Op::Set {
            collection: None,
            typed_guard: None,
            select: Selector::FrontmatterKey {
                key: "owner".into(),
            },
            part: None,
            guard: guard(
                TRACKER,
                &Selector::FrontmatterKey {
                    key: "owner".into(),
                },
            ),
            content: json!(["not", "scalar"]),
            allow_drop: false,
        }],
    )
    .unwrap_err();
    assert_eq!(e.code, ErrorCode::UnsupportedSyntax);
    // A numeric-looking string keeps its meaning by being quoted.
    let out = run(
        TRACKER,
        &[Op::Set {
            collection: None,
            typed_guard: None,
            select: Selector::FrontmatterKey {
                key: "owner".into(),
            },
            part: None,
            guard: guard(
                TRACKER,
                &Selector::FrontmatterKey {
                    key: "owner".into(),
                },
            ),
            content: json!("123"),
            allow_drop: false,
        }],
    )
    .unwrap();
    assert!(out.candidate.contains("owner: '123'\n"));
}

#[test]
fn yaml_11_boolean_and_null_strings_are_quoted_on_set_and_insert() {
    for word in [
        "y", "n", "yes", "no", "on", "off", "true", "false", "null", "~", "",
    ] {
        let mut spellings = std::collections::BTreeSet::from([word.to_string()]);
        // Cover every ASCII capitalization, including mixed case.
        for mask in 0..(1usize << word.len()) {
            spellings.insert(
                word.chars()
                    .enumerate()
                    .map(|(i, c)| {
                        if mask & (1 << i) == 0 {
                            c
                        } else {
                            c.to_ascii_uppercase()
                        }
                    })
                    .collect::<String>(),
            );
        }
        for value in spellings {
            for newline in ["\n", "\r\n"] {
                let source = stamp(&"---\nreviewed: pending # keep this comment\nother: untouched\n---\n\n## Body\n\n- [ ] Keep these bytes.\n".replace('\n', newline));
                let select = Selector::FrontmatterKey {
                    key: "reviewed".into(),
                };
                let out = run(
                    &source,
                    &[Op::Set {
                        collection: None,
                        typed_guard: None,
                        guard: guard(&source, &select),
                        select,
                        part: None,
                        content: json!(value),
                        allow_drop: false,
                    }],
                )
                .unwrap();
                assert_eq!(
                    out.candidate,
                    source.replace("reviewed: pending", &format!("reviewed: '{value}'"))
                );
                let doc = Document::parse(&out.candidate).unwrap();
                assert_eq!(
                    doc.scalars()
                        .iter()
                        .find(|s| s.key == "reviewed")
                        .unwrap()
                        .value,
                    json!(value)
                );
                let out = run(
                    &source,
                    &[Op::AddKey {
                        key: "added".into(),
                        value: json!(value),
                        guard: guard(&source, &Selector::Document),
                    }],
                )
                .unwrap();
                assert!(
                    out.candidate
                        .contains(&format!("added: '{value}'{newline}")),
                    "{:?}",
                    out.candidate
                );
                assert_eq!(
                    out.candidate
                        .replace(&format!("added: '{value}'{newline}"), ""),
                    source
                );
                assert_eq!(
                    Document::parse(&out.candidate)
                        .unwrap()
                        .scalars()
                        .iter()
                        .find(|s| s.key == "added")
                        .unwrap()
                        .value,
                    json!(value)
                );
            }
        }
    }
}

#[test]
fn carrier_refusal_blocks_minting_after_capturing_text() {
    let src = "- first has <b>html</b>\n- second\n";
    let out = run(
        src,
        &[Op::Stamp {
            collection: None,
            checkbox_only: false,
            collection_guard: None,
            select: Selector::Document,
            guard: Document::parse(src).unwrap().revision(),
        }],
    )
    .unwrap();
    assert_eq!(out.ops[0].minted.len(), 0);
    assert_eq!(out.ops[0].refused.len(), 2);
    assert_eq!(out.candidate, src);
    // A plain marker already in the prefix is masked and does not refuse.
    let src = "- one <!-- stay:a1 subhash=sha256:000000000000 -->\n- two\n";
    let out = run(
        src,
        &[Op::Stamp {
            collection: None,
            checkbox_only: false,
            collection_guard: None,
            select: Selector::Document,
            guard: Document::parse(src).unwrap().revision(),
        }],
    )
    .unwrap();
    assert_eq!(
        out.ops[0].minted.len(),
        2,
        "one item plus the missing container"
    );
}

fn preserves_untouched_duplicate_id_evidence(inline: bool) {
    for target_first in [false, true] {
        let separator = if inline { " " } else { "\n" };
        let untouched = format!(
            "Para one.{separator}<!-- stay:dupA1 hash=sha256:111111111111 -->\n\n\
                 Para two.{separator}<!-- stay:dupA1 hash=sha256:222222222222 -->\n\n"
        );
        let target =
            format!("Para three.{separator}<!-- stay:p3x hash=sha256:333333333333 -->\n\n");
        let src = if target_first {
            format!("{target}{untouched}")
        } else {
            format!("{untouched}{target}")
        };
        let select = Selector::Id { id: "p3x".into() };
        let out = run(
            &src,
            &[Op::Set {
                collection: None,
                typed_guard: None,
                select: select.clone(),
                part: None,
                guard: guard(&src, &select),
                content: json!("A longer replacement paragraph."),
                allow_drop: false,
            }],
        )
        .unwrap();
        if target_first {
            assert!(out.candidate.ends_with(&untouched));
        } else {
            assert!(out.candidate.starts_with(&untouched));
        }
        let before = Document::parse(&src).unwrap();
        let after = Document::parse(&out.candidate).unwrap();
        assert_eq!(before.markers.len(), 3);
        assert_eq!(after.markers.len(), before.markers.len());
        for (old, new) in before.markers.iter().zip(&after.markers) {
            assert_eq!(old.lexical.id, new.lexical.id);
            assert_eq!(old.attach, new.attach);
        }
        assert_eq!(out.evidence.len(), 1);
        let e = &out.evidence[0];
        assert_eq!(e.id, "p3x");
        assert_eq!(e.key, "hash");
        assert_eq!(e.stored, "333333333333");
        assert!(e.prior_drift);
        assert!(e.rewritten);
        let marker = after
            .markers
            .iter()
            .position(|m| m.lexical.id.as_deref() == Some("p3x"))
            .unwrap();
        let (fresh, stored) = after.marker_evidence(marker).unwrap();
        assert!(fresh.starts_with(&stored.unwrap()));
    }
}

#[test]
fn editing_a_block_preserves_untouched_duplicate_id_evidence_on_marker_lines() {
    preserves_untouched_duplicate_id_evidence(false);
}

#[test]
fn editing_a_block_preserves_untouched_duplicate_id_evidence_inline() {
    preserves_untouched_duplicate_id_evidence(true);
}

#[test]
fn prior_drift_is_reported_before_refresh() {
    let stamped = stamp(TRACKER);
    let ids = item_ids(&stamped);
    // A human edits the item without refreshing its digest.
    let drifted = stamped.replace("Context a session needs", "Context edited by hand");
    let select = Selector::Id { id: ids[0].clone() };
    let out = run(
        &drifted,
        &[Op::Set {
            collection: None,
            typed_guard: None,
            select: select.clone(),
            part: None,
            guard: guard(&drifted, &select),
            content: json!("[x] **Ship the widget. (2026-09-01)** Done."),
            allow_drop: false,
        }],
    )
    .unwrap();
    let e = out.evidence.iter().find(|e| e.key == "subhash").unwrap();
    assert!(e.prior_drift);
    assert_evidence_current(&out.candidate);
}

#[test]
fn delete_item_and_section() {
    let stamped = stamp(TRACKER);
    let ids = item_ids(&stamped);
    let select = Selector::Id { id: ids[1].clone() };
    let out = run(
        &stamped,
        &[Op::Delete {
            list: None,
            collection: None,
            select: select.clone(),
            guard: guard(&stamped, &select),
        }],
    )
    .unwrap();
    let doc = Document::parse(&out.candidate).unwrap();
    assert_eq!(doc.lists[0].items.len(), 2);
    assert_eq!(out.ops[0].dropped, vec![ids[1].clone()]);
    assert!(!out.candidate.contains("Refresh the fixtures"));
    assert_evidence_current(&out.candidate);
    let log = Selector::HeadingPath {
        path: vec!["2026-09-01: tracker opened".into()],
    };
    let out = run(
        &stamped,
        &[Op::Delete {
            list: None,
            collection: None,
            select: log.clone(),
            guard: guard(&stamped, &log),
        }],
    )
    .unwrap();
    assert!(out.candidate.ends_with("Newest first.\n\n"));
}

#[test]
fn delete_refuses_unguarded_reference_definitions_after_blocks() {
    for block in ["- first\n- second\n", "Paragraph.\n", "> Quotation.\n"] {
        for suffix in ["", "Uses [ref] here.\n"] {
            for newline in ["\n", "\r\n"] {
                let source = format!("# Page\n\n{block}<!-- stay:delete01 -->\n\n[ref]: https://example.com/x\n\n{suffix}")
                    .replace('\n', newline);
                let doc = Document::parse(&source).unwrap();
                let target = Target::Block(1);
                let select = Selector::Id {
                    id: "delete01".into(),
                };
                let error = run(
                    &source,
                    &[Op::Delete {
                        list: None,
                        collection: None,
                        select,
                        guard: doc.target_revision(target),
                    }],
                )
                .unwrap_err();
                assert_eq!(
                    error.code,
                    ErrorCode::UnsupportedSyntax,
                    "{source:?}: {error:?}"
                );
                let gap = error.span.expect("unguarded gap");
                assert!(gap.slice(&source).unwrap().contains("[ref]:"));
            }
        }
    }
}

#[test]
fn delete_whitespace_gaps_preserves_outside_bytes_and_stay_attachment() {
    for newline in ["\n", "\r\n"] {
        for last in [false, true] {
            let suffix = if last {
                ""
            } else {
                "Survivor.\n<!-- stay:survivor -->\n"
            };
            let source = format!(
                "# Page\n<!-- stay:page0001 -->\n\n- first <!-- stay:first001 subhash -->\n\n- second <!-- stay:second01 subhash -->\n<!-- stay:list0001 -->\n \t\n\n{suffix}"
            ).replace('\n', newline);
            let doc = Document::parse(&source).unwrap();
            let target = Target::Block(doc.lists[0].block);
            let start = doc.target_span(target).0.start;
            let end = if last {
                source.len()
            } else {
                source.find("Survivor.").unwrap()
            };
            let select = Selector::Id {
                id: "list0001".into(),
            };
            let out = run(
                &source,
                &[Op::Delete {
                    list: None,
                    collection: None,
                    select,
                    guard: doc.target_revision(target),
                }],
            )
            .unwrap();
            // Existing markers have no stored hashes, so refresh adds none.
            assert_eq!(
                out.candidate,
                format!("{}{}", &source[..start], &source[end..])
            );
            let after = Document::parse(&out.candidate).unwrap();
            for id in ["page0001"]
                .into_iter()
                .chain((!last).then_some("survivor"))
            {
                assert_eq!(
                    after.target_ids(after.resolve(&Selector::Id { id: id.into() }).unwrap()),
                    vec![id]
                );
            }
        }
    }
}

#[test]
fn delete_section_owns_reference_definitions_inside_its_guard() {
    let source = "# Page\n\n## Remove\n\n- first\n- second\n\n[ref]: https://example.com/x\n\n## Keep\n<!-- stay:keep0001 -->\n\nUntouched.\n";
    let select = Selector::HeadingPath {
        path: vec!["Remove".into()],
    };
    let out = run(
        source,
        &[Op::Delete {
            list: None,
            collection: None,
            select: select.clone(),
            guard: guard(source, &select),
        }],
    )
    .unwrap();
    assert_eq!(
        out.candidate,
        source.replace(
            "## Remove\n\n- first\n- second\n\n[ref]: https://example.com/x\n\n",
            ""
        )
    );
    let after = Document::parse(&out.candidate).unwrap();
    assert!(matches!(
        after.resolve(&Selector::Id {
            id: "keep0001".into()
        }),
        Ok(Target::Section(_))
    ));
}

#[test]
fn crlf_documents_keep_crlf() {
    let src = TRACKER.replace('\n', "\r\n");
    let stamped = stamp(&src);
    assert!(
        !stamped.replace("\r\n", "").contains('\n'),
        "only CRLF line endings"
    );
    let ids = item_ids(&stamped);
    let select = Selector::Id { id: ids[0].clone() };
    let out = run(
        &stamped,
        &[Op::Set {
            collection: None,
            typed_guard: None,
            select: select.clone(),
            part: None,
            guard: guard(&stamped, &select),
            content: json!("[x] **Done.**\n    **Gate**: ok."),
            allow_drop: false,
        }],
    )
    .unwrap();
    assert!(!out.candidate.replace("\r\n", "").contains('\n'));
    assert_evidence_current(&out.candidate);
}

#[test]
fn unrelated_markers_and_orphans_survive_writes() {
    let src = "<!-- stay:orphan -->\n\nIntro <!-- stay:p1 hash=sha256:abc -->\n\n- one <!-- stay:i1 subhash=sha256:1 -->\n  - nested <!-- stay:n1 subhash=sha256:2 -->\n- two <!-- stay:i2 subhash=sha256:3 -->\n";
    let doc = Document::parse(src).unwrap();
    assert!(doc.markers.iter().any(|m| m.attach == Attach::Orphan));
    let select = Selector::Id { id: "i2".into() };
    let out = run(
        src,
        &[Op::Set {
            collection: None,
            typed_guard: None,
            select: select.clone(),
            part: None,
            guard: guard(src, &select),
            content: json!("two, edited"),
            allow_drop: false,
        }],
    )
    .unwrap();
    assert!(out.candidate.starts_with("<!-- stay:orphan -->\n\nIntro <!-- stay:p1 hash=sha256:abc -->\n\n- one <!-- stay:i1 subhash=sha256:1 -->\n  - nested <!-- stay:n1 subhash=sha256:2 -->\n- two, edited <!-- stay:i2 subhash=sha256:"));
    // Editing the item that holds a nested child marker is refused rather than guessing.
    let select = Selector::Id { id: "i1".into() };
    let e = run(
        src,
        &[Op::Set {
            collection: None,
            typed_guard: None,
            select: select.clone(),
            part: None,
            guard: guard(src, &select),
            content: json!("one, edited"),
            allow_drop: false,
        }],
    )
    .unwrap_err();
    assert_eq!(e.code, ErrorCode::UnsupportedSyntax);
}

#[test]
fn heading_attribute_ids_count_as_dropped() {
    let src = "# Parent\n\n## Old {#child}\n\nOld body.\n";
    let parent = Selector::HeadingPath {
        path: vec!["Parent".into()],
    };
    let set = |allow_drop| Op::Set {
        collection: None,
        typed_guard: None,
        select: parent.clone(),
        part: Some(SectionPart::Body),
        guard: guard(src, &parent),
        content: json!("\n## Replacement {#child}\n\nNew body.\n"),
        allow_drop,
    };
    let e = run(src, &[set(false)]).unwrap_err();
    assert!(e.message.contains("child"), "{}", e.message);
    let e = run(src, &[set(true)]).unwrap_err();
    assert!(e.message.contains("dropped"), "{}", e.message);
}

#[test]
fn inline_block_stays_move_to_a_marker_line() {
    let src = "One <!-- stay:p hash=sha256:000000000000 -->\n";
    let p = Selector::Id { id: "p".into() };
    let out = run(
        src,
        &[Op::Set {
            collection: None,
            typed_guard: None,
            select: p.clone(),
            part: None,
            guard: guard(src, &p),
            content: json!("Two\nlines"),
            allow_drop: false,
        }],
    )
    .unwrap();
    assert!(
        out.candidate
            .starts_with("Two\nlines\n<!-- stay:p hash=sha256:")
    );
    assert_evidence_current(&out.candidate);
}

#[test]
fn unstamped_sections_still_return_a_next_guard() {
    let src = "# A\n\nBody.\n";
    let a = Selector::HeadingPath {
        path: vec!["A".into()],
    };
    let out = run(
        src,
        &[Op::Set {
            collection: None,
            typed_guard: None,
            select: a.clone(),
            part: Some(SectionPart::Title),
            guard: guard(src, &a),
            content: json!("B"),
            allow_drop: false,
        }],
    )
    .unwrap();
    let b = Selector::HeadingPath {
        path: vec!["B".into()],
    };
    assert_eq!(
        out.ops[0].revision.as_deref(),
        Some(guard(&out.candidate, &b).as_str())
    );
}

/// A list followed by two or more blank lines and a list of the other type:
/// comrak ends the last item on a blank line, which used to make `stamp`
/// refuse the whole document with `invalid_content`.
#[test]
fn a_blank_gap_before_a_list_of_the_other_type_does_not_block_stamping() {
    for (name, src) in [
        ("bullets then ordered", "- a\n- b\n\n\n1. x\n"),
        ("ordered then bullets", "1. a\n\n\n- b\n"),
        ("a wider gap", "- a\n- b\n\n\n\n\n1. x\n"),
        ("nested item at the edge", "- a\n  - deep\n\n\n1. x\n"),
    ] {
        let doc = Document::parse(src).unwrap();
        for list in &doc.lists {
            for item in &list.items {
                assert!(
                    !doc.source[item.span.start..item.span.end]
                        .lines()
                        .next_back()
                        .unwrap_or_default()
                        .trim()
                        .is_empty(),
                    "{name}: an item span ends on a blank line"
                );
            }
        }
        let out = stamp(src);
        let back = Document::parse(&out).unwrap();
        assert_eq!(
            back.lists.len(),
            doc.lists.len(),
            "{name}: stamping changed the list count"
        );
        for (before, after) in doc.lists.iter().zip(&back.lists) {
            assert_eq!(
                before.items.len(),
                after.items.len(),
                "{name}: stamping changed a list's item count"
            );
            assert_eq!(
                before.ordered, after.ordered,
                "{name}: stamping changed a list's kind"
            );
        }
    }
}

/// An item carrying indented content the list parse did not take (a table
/// after a paragraph) makes comrak end the list before its own last item.
/// The list block must still cover its items, or `stamp` refuses the document.
#[test]
fn a_list_block_covers_its_items_when_comrak_ends_it_early() {
    const TABLE: &str = "      | a | b |\n      |---|---|\n      | 1 | 2 |\n";
    for (name, src) in [
        (
            "table last",
            &format!("- [ ] one\n      para a\n\n{TABLE}\n### h\n"),
        ),
        (
            "table between paragraphs",
            &format!("- [ ] one\n      para a\n\n{TABLE}\n      para b\n\n### h\n"),
        ),
        (
            "no checkbox",
            &format!("- one\n      para a\n\n{TABLE}\n      para b\n\n### h\n"),
        ),
    ] {
        let doc = Document::parse(src).unwrap();
        for list in &doc.lists {
            let block = doc.blocks[list.block].span;
            for item in &list.items {
                assert!(
                    block.start <= item.span.start && item.span.end <= block.end,
                    "{name}: an item runs outside its list block"
                );
            }
        }
        let out = stamp(src);
        let back = Document::parse(&out).unwrap();
        assert_eq!(back.lists.len(), doc.lists.len(), "{name}: list count");
        for (before, after) in doc.lists.iter().zip(&back.lists) {
            assert_eq!(
                before.items.len(),
                after.items.len(),
                "{name}: item count changed"
            );
        }
    }
}

/// markstay v1.8: a closed inline code span is masked before §3.4's scan, so a
/// list whose earlier item quotes `` `<repo>` `` can still be stamped.
///
/// The write-safety claim is what matters here, not the count: every byte the
/// stamp did not mean to touch has to come back identical, because this clause
/// exists to let Rivet write where it previously refused, and a wrong mask
/// writes into documents it should have left alone.
#[test]
fn a_closed_code_span_no_longer_refuses_the_items_after_it() {
    let src = "- [ ] run `<repo>` first\n- [ ] then the second\n- [ ] and the third\n";
    let out = run(
        src,
        &[Op::Stamp {
            collection: None,
            checkbox_only: false,
            collection_guard: None,
            select: Selector::Document,
            guard: Document::parse(src).unwrap().revision(),
        }],
    )
    .unwrap();
    assert_eq!(
        out.ops[0].refused.len(),
        0,
        "v1.8 masks the span, so nothing refuses"
    );
    assert_eq!(out.ops[0].minted.len(), 4, "three items and their list");

    // Every byte outside the markers the stamp added is the original. Removing
    // each marker and the single separator it was inserted behind has to give
    // back `src` exactly: no normalising, no trimming, no forgiving a dropped
    // final newline, because each of those is a byte this clause was supposed
    // to protect. Stripped by hand rather than through the document model, so
    // the check does not lean on the code it is checking.
    let mut stripped = String::new();
    let mut rest = out.candidate.as_str();
    while let Some(at) = rest.find("<!-- stay:") {
        let (before, tail) = rest.split_at(at);
        let close = tail.find("-->").expect("a marker closes") + 3;
        let after = &tail[close..];
        if before.ends_with('\n') {
            // A container marker owns its own line. Exactly one newline goes
            // with it, the one that ends its line; the one before it ended the
            // real last line and belongs to the document.
            stripped.push_str(before);
            rest = after.strip_prefix('\n').unwrap_or(after);
        } else {
            // A child marker sits after one space at the end of a paragraph.
            stripped.push_str(before.strip_suffix(' ').unwrap_or(before));
            rest = after;
        }
    }
    stripped.push_str(rest);
    assert_eq!(stripped, src, "stamping moved a byte it did not own");
    assert!(
        out.candidate.contains("`<repo>`"),
        "the code span is still literal"
    );

    // The same list with the `<` outside a span still refuses, and refusing
    // leaves the document byte-identical.
    let unsafe_src = "- [ ] run <repo> first\n- [ ] then the second\n";
    let out = run(
        unsafe_src,
        &[Op::Stamp {
            collection: None,
            checkbox_only: false,
            collection_guard: None,
            select: Selector::Document,
            guard: Document::parse(unsafe_src).unwrap().revision(),
        }],
    )
    .unwrap();
    assert_eq!(out.ops[0].minted.len(), 0);
    assert_eq!(out.ops[0].refused.len(), 2);
    assert_eq!(
        out.candidate, unsafe_src,
        "a refused stamp changed the file"
    );

    // An unclosed run masks nothing, so the `<` after it still refuses.
    let unclosed = "- [ ] an unclosed ` span <repo>\n- [ ] second\n";
    let out = run(
        unclosed,
        &[Op::Stamp {
            collection: None,
            checkbox_only: false,
            collection_guard: None,
            select: Selector::Document,
            guard: Document::parse(unclosed).unwrap().revision(),
        }],
    )
    .unwrap();
    assert_eq!(out.ops[0].minted.len(), 0);
    assert_eq!(out.candidate, unclosed);
}

/// One `<` blocks every later item of its list, so a migration has to be able
/// to find the single source position behind a run of refusals rather than a
/// reason repeated N times. Every refusal in the cascade reports the same
/// `origin` and `offset`: that pair is the grouping key.
#[test]
fn one_blocker_is_named_by_every_refusal_it_cascades_into() {
    let src = "- first has <b>html</b>\n- second\n- third\n- fourth\n";
    let out = run(
        src,
        &[Op::Stamp {
            collection: None,
            checkbox_only: false,
            collection_guard: None,
            select: Selector::Document,
            guard: Document::parse(src).unwrap().revision(),
        }],
    )
    .unwrap();
    assert_eq!(out.candidate, src, "a refused stamp changes nothing");
    assert_eq!(out.ops[0].minted.len(), 0);
    assert_eq!(out.ops[0].refused.len(), 4, "every item of the list");

    let blockers: Vec<_> = out.ops[0]
        .refused
        .iter()
        .map(|d| d.blocker.clone().expect("a §3.4 refusal names its blocker"))
        .collect();
    let first = &blockers[0];
    assert_eq!(first.origin, BlockerOrigin::AsRead);
    assert_eq!(first.character, "<");
    assert_eq!(
        &src[first.offset..first.offset + 1],
        "<",
        "the offset addresses the offending byte in the file"
    );
    assert_eq!((first.line, first.column), (1, 13));
    assert_eq!(first.list_line, 1);
    for b in &blockers {
        assert_eq!(
            (b.origin, b.offset),
            (first.origin, first.offset),
            "one source position, so one grouping key for the whole cascade"
        );
    }
    // Each refusal still carries its own item's line, so a report can say which
    // actions that one position cost.
    let lines: Vec<_> = out.ops[0].refused.iter().map(|d| d.line.unwrap()).collect();
    assert_eq!(lines, vec![1, 2, 3, 4]);
}

/// The v1.8 clause: a `<` a closed code span has already neutralised is not a
/// refusal, so it mints and reports no blocker.
#[test]
fn a_closed_code_span_is_accepted_and_names_no_blocker() {
    let src = "- first has `<b>` inline\n- second\n";
    let out = run(
        src,
        &[Op::Stamp {
            collection: None,
            checkbox_only: false,
            collection_guard: None,
            select: Selector::Document,
            guard: Document::parse(src).unwrap().revision(),
        }],
    )
    .unwrap();
    assert!(out.ops[0].refused.is_empty());
    assert_eq!(
        out.ops[0].minted.len(),
        3,
        "two items plus the missing container"
    );
}

/// A span the line never closes masks nothing, so the `<` inside it still
/// refuses, and refusing leaves the file byte-identical.
#[test]
fn an_unclosed_code_span_refuses_without_mutation() {
    let src = "- first has `<b> unclosed\n- second\n";
    let out = run(
        src,
        &[Op::Stamp {
            collection: None,
            checkbox_only: false,
            collection_guard: None,
            select: Selector::Document,
            guard: Document::parse(src).unwrap().revision(),
        }],
    )
    .unwrap();
    assert_eq!(out.candidate, src);
    assert_eq!(out.ops[0].minted.len(), 0);
    assert_eq!(out.ops[0].refused.len(), 2);
    let b = out.ops[0].refused[0].blocker.clone().unwrap();
    assert_eq!(b.character, "<");
    assert_eq!(&src[b.offset..b.offset + 1], "<");
}

/// The reported reason and the reported position describe the same character,
/// so whichever offender comes first in the text is the one named. Both
/// orderings, because a single ordering only rejects one fixed precedence: with
/// the backslash case alone, an implementation preferring `\\` unconditionally
/// passes just as the old one preferring `<` failed.
#[test]
fn the_first_offender_is_the_one_reported() {
    for (src, character, word) in [
        ("- a back\\slash then <b>\n- second\n", "\\", "backslash"),
        ("- a <b> then back\\slash\n- second\n", "<", "'<'"),
        // Repeated, so `min(find('<'), rfind('\\'))` cannot masquerade as a
        // first-in-text scan: it would report the second backslash here.
        ("- a \\ b \\ c\n- second\n", "\\", "backslash"),
    ] {
        let out = run(
            src,
            &[Op::Stamp {
                collection: None,
                checkbox_only: false,
                collection_guard: None,
                select: Selector::Document,
                guard: Document::parse(src).unwrap().revision(),
            }],
        )
        .unwrap();
        let d = &out.ops[0].refused[0];
        let b = d.blocker.clone().unwrap();
        assert_eq!(b.character, character, "{src:?}");
        assert_eq!(&src[b.offset..b.offset + 1], character, "{src:?}");
        assert_eq!(
            b.offset,
            src.find(character).unwrap(),
            "the first occurrence in the text, not a fixed preference"
        );
        assert!(
            d.message.contains(word),
            "{src:?}: the message names the character the position points at"
        );
    }
}

/// `offset` is a byte and `column` is a character, so a multi-byte character
/// before the blocker separates them. A column counted over bytes would point
/// past the offender in any document with an accent or a symbol in it.
#[test]
fn a_column_is_characters_and_an_offset_is_bytes() {
    let src = "- naïve ★ then <b>\n";
    let out = run(
        src,
        &[Op::Stamp {
            collection: None,
            checkbox_only: false,
            collection_guard: None,
            select: Selector::Document,
            guard: Document::parse(src).unwrap().revision(),
        }],
    )
    .unwrap();
    let b = out.ops[0].refused[0].blocker.clone().unwrap();
    assert_eq!(&src[b.offset..b.offset + 1], "<");
    assert_eq!(b.column, src[..b.offset].chars().count() + 1);
    assert!(
        b.offset > b.column - 1,
        "ï costs one extra byte and ★ two, so the byte offset leads the column"
    );
}

/// The list's own stay is its stable identity, so a report can group by the
/// list as well as by the position. Without one it still has the list's line.
#[test]
fn a_blocker_carries_the_identity_of_the_list_it_sits_in() {
    let stamped = stamp("- first\n- second\n");
    let doc = Document::parse(&stamped).unwrap();
    let list_id = doc.stay_ids(&doc.blocks[doc.lists[0].block].markers)[0].clone();
    // A third item written by hand, after the two the list has stamped.
    let container = stamped.rfind("<!-- stay:").unwrap();
    let src = format!(
        "{}- third has <b>html</b>\n{}",
        &stamped[..container],
        &stamped[container..]
    );
    let out = run(
        &src,
        &[Op::Stamp {
            collection: None,
            checkbox_only: false,
            collection_guard: None,
            select: Selector::Document,
            guard: Document::parse(&src).unwrap().revision(),
        }],
    )
    .unwrap();
    assert_eq!(out.candidate, src);
    assert_eq!(out.ops[0].refused.len(), 1, "only the unstamped item");
    let b = out.ops[0].refused[0].blocker.clone().unwrap();
    assert_eq!(b.origin, BlockerOrigin::AsRead);
    assert_eq!(&src[b.offset..b.offset + 1], "<");
    assert_eq!((b.line, b.list_line), (3, 1));
    assert_eq!(b.list_id, Some(list_id));
}

/// Text the caller supplied is not in the document, so there is no position to
/// report and none is invented. An offset into the rendered Markdown would
/// address a string nobody sent: rendering has already added the list prefix,
/// the continuation indent and the document's own line endings.
#[test]
fn a_refusal_over_submitted_text_carries_no_blocker() {
    let stamped = stamp("- first\n- second\n");
    let doc = Document::parse(&stamped).unwrap();
    let list = Selector::Id {
        id: doc.stay_ids(&doc.blocks[doc.lists[0].block].markers)[0].clone(),
    };
    for position in [Position::End, Position::Start] {
        let e = run(
            &stamped,
            &[Op::Insert {
                select: list.clone(),
                position,
                guard: guard(&stamped, &list),
                content: "[ ] new item with <b>html</b>".into(),
                stamp: true,
            }],
        )
        .unwrap_err();
        assert_eq!(e.code, ErrorCode::StampRefused);
        assert!(e.message.contains("'<'"));
        assert!(
            e.blocker.is_none(),
            "{position:?}: the offending character is in the caller's own text"
        );
    }
}

/// A pass can mint and refuse at once, and minting shifts every byte after it.
/// The blocker addresses the document as read, which is the file a `preview`
/// leaves untouched and the file a migration edits.
#[test]
fn a_blocker_addresses_the_document_as_read_not_the_one_a_write_produces() {
    let src = "- ok\n- bad <b>\n";
    let out = run(
        src,
        &[Op::Stamp {
            collection: None,
            checkbox_only: false,
            collection_guard: None,
            select: Selector::Document,
            guard: Document::parse(src).unwrap().revision(),
        }],
    )
    .unwrap();
    assert_eq!(
        out.ops[0].minted.len(),
        2,
        "the clean item, plus the list's own container stay"
    );
    assert_eq!(out.ops[0].refused.len(), 1);
    let b = out.ops[0].refused[0].blocker.clone().unwrap();
    assert_eq!(b.origin, BlockerOrigin::AsRead);
    assert_eq!(b.offset, 11);
    assert_eq!(&src[b.offset..b.offset + 1], "<");
    // The same pass rewrote the file, and that byte is now inside the marker it
    // inserted. The offset is not stale, it is bound to the snapshot it names.
    assert_ne!(out.candidate, src);
    assert_ne!(&out.candidate[b.offset..b.offset + 1], "<");
}

/// Two things a list starting at byte zero cannot catch: an offset that is
/// really a list-relative one, and a masked code span being reported instead of
/// the live offender after it. The span holds a multi-byte character, so a mask
/// that widened or narrowed the text would move the answer.
#[test]
fn a_masked_span_is_skipped_and_a_list_need_not_start_at_byte_zero() {
    let src = "# Heading\n\nA paragraph first.\n\n- an `<é>` span\n- a raw < here\n- third\n";
    let out = run(
        src,
        &[Op::Stamp {
            collection: None,
            checkbox_only: false,
            collection_guard: None,
            select: Selector::Document,
            guard: Document::parse(src).unwrap().revision(),
        }],
    )
    .unwrap();
    assert_eq!(
        out.ops[0].refused.len(),
        2,
        "the item with the raw '<' and the one after it"
    );
    let b = out.ops[0].refused[0].blocker.clone().unwrap();
    assert_eq!(&src[b.offset..b.offset + 1], "<");
    assert_eq!(
        b.offset,
        src.find("raw <").unwrap() + 4,
        "the live '<', not the one the closed span neutralised"
    );
    assert!(
        b.offset > src.find("`<é>`").unwrap(),
        "past the masked span"
    );
    assert_eq!((b.line, b.list_line), (6, 5));
    let line_start = src.find("- a raw").unwrap();
    assert_eq!(b.column, src[line_start..b.offset].chars().count() + 1);
    // The first item's span was masked, so it stamped rather than refusing. The
    // heading mints too, so a nonempty `minted` proves nothing: look for the
    // stay on the item itself, in the candidate the write would produce.
    let after = Document::parse(&out.candidate).unwrap();
    let items = &after.lists[0].items;
    assert!(!items[0].markers.is_empty(), "the masked item stamped");
    assert!(
        items[1].markers.is_empty() && items[2].markers.is_empty(),
        "the two behind the live '<' did not"
    );
}

/// The insert path's own blocker, which nothing else pins: dropping
/// `with_blocker` there would satisfy every test that only asserts absence.
/// The list starts well past byte zero and carries a masked multi-byte code
/// span and live stay markers ahead of the offender, so a list-relative offset
/// or a mask of the wrong width lands somewhere else.
#[test]
fn an_insert_blocked_by_existing_source_reports_where() {
    let stamped = stamp("# Heading\n\nProse first.\n\n- an `<é>` span\n- second\n");
    let doc = Document::parse(&stamped).unwrap();
    let list_id = doc.stay_ids(&doc.blocks[doc.lists[0].block].markers)[0].clone();
    // A third item written by hand, carrying the character that blocks the list.
    let container = stamped.rfind("<!-- stay:").unwrap();
    let src = format!(
        "{}- third has <b>html</b>\n{}",
        &stamped[..container],
        &stamped[container..]
    );
    let list = Selector::Id {
        id: list_id.clone(),
    };
    let e = run(
        &src,
        &[Op::Insert {
            select: list.clone(),
            position: Position::End,
            guard: guard(&src, &list),
            content: "[ ] a clean new item".into(),
            stamp: true,
        }],
    )
    .unwrap_err();
    assert_eq!(e.code, ErrorCode::StampRefused);
    let b = e
        .blocker
        .expect("the offender is in the file, so it has a position");
    assert_eq!(b.origin, BlockerOrigin::AsRead);
    assert_eq!(b.character, "<");
    assert_eq!(
        b.offset,
        src.find("third has <").unwrap() + 10,
        "the live '<', not the one inside the closed span above it"
    );
    assert_eq!(&src[b.offset..b.offset + 1], "<");
    // Derived, not hardcoded: stamping inserts stay lines of its own, so the
    // literal numbers depend on a layout this test does not fix.
    let line_of = |at: usize| 1 + src[..at].matches('\n').count();
    assert_eq!(b.line, line_of(b.offset));
    assert_eq!(b.list_line, line_of(src.find("- an `<é>`").unwrap()));
    assert!(b.list_line < b.line, "the list opens before the offender");
    let line_start = src[..b.offset].rfind('\n').unwrap() + 1;
    assert_eq!(b.column, src[line_start..b.offset].chars().count() + 1);
    assert_eq!(b.list_id, Some(list_id));
}

#[test]
fn an_item_ending_in_indented_code_owns_every_line_of_it() {
    // comrak reports the code block ending one byte into its first line when a
    // blank line closes the item, which once cut `code b` out of the item.
    let src = "- [ ] **One.** Text.\n\n      code a\n      code b\n\n- [ ] **Two.** Text.\n";
    let stamped = stamp(src);
    let doc = Document::parse(&stamped).unwrap();
    assert!(doc.diagnostics.is_empty(), "{:?}", doc.diagnostics);
    let first = Selector::Id {
        id: item_ids(&stamped)[0].clone(),
    };
    let list = Selector::Id {
        id: doc.stay_ids(&doc.blocks[doc.lists[0].block].markers)[0].clone(),
    };

    // An append after the list is planned without claiming that item changed.
    let out = run(
        &stamped,
        &[Op::Insert {
            guard: guard(&stamped, &list),
            select: list,
            position: Position::End,
            content: "[ ] **Three.**".into(),
            stamp: true,
        }],
    )
    .unwrap();
    assert!(
        out.candidate
            .contains("      code a\n      code b\n\n- [ ] **Two.**")
    );
    assert_evidence_current(&out.candidate);

    // A change to the block's last line makes the item's old guard stale.
    let g = guard(&stamped, &first);
    let edited = stamped.replace("      code b", "      code B");
    let e = run(
        &edited,
        &[Op::Set {
            collection: None,
            typed_guard: None,
            select: first,
            part: None,
            guard: g,
            content: json!("[x] **One.** Text.\n\n    code a\n    code B"),
            allow_drop: false,
        }],
    )
    .unwrap_err();
    assert_eq!(e.code, ErrorCode::GuardConflict);
}

#[test]
fn every_op_refuses_a_reference_edit_that_swaps_a_live_stay_elsewhere() {
    // A plain `set` of the paragraph holding the definition would make the
    // quoted copy in another section live in place of the original stay.
    let src = "# Doc\n\n[ref]: /url\nprose\n<!-- stay:p hash=sha256:000000000000 -->\n\n## B\n\n[foo][ref](<!-- stay:owned -->) [bar][other](<!-- stay:owned -->)\n";
    let doc = Document::parse(src).unwrap();
    assert_eq!(doc.markers.len(), 2);
    let select = Selector::Id { id: "p".into() };
    let e = run(
        src,
        &[Op::Set {
            collection: None,
            typed_guard: None,
            select: select.clone(),
            part: None,
            guard: guard(src, &select),
            content: json!("[other]: /url\nprose"),
            allow_drop: false,
        }],
    )
    .unwrap_err();
    assert_eq!(e.code, ErrorCode::InvalidContent);
}

#[test]
fn delete_collection_precondition_requires_project_context() {
    let src = stamp(TRACKER);
    let select = Selector::Id {
        id: item_ids(&src)[0].clone(),
    };
    let error = run(
        &src,
        &[Op::Delete {
            list: None,
            select: select.clone(),
            guard: guard(&src, &select),
            collection: Some("actions".into()),
        }],
    )
    .unwrap_err();
    assert_eq!(error.code, ErrorCode::InvalidRequest);
}
