//! Position selectors: which items a position counts, how a handle resolves,
//! and when it refuses. The CLI contract is in `markrivet-cli/tests/position.rs`.
use markrivet_core::document::{Addressing, Target};
use markrivet_core::{Document, ErrorCode, Selector};
use serde_json::json;

fn at(path: Option<&[&str]>, index: u64) -> Selector {
    Selector::Position {
        heading_path: path.map(|p| p.iter().map(|s| s.to_string()).collect()),
        index,
    }
}

/// The content of the item a position resolves to.
fn content(doc: &Document, select: &Selector) -> String {
    match doc.resolve(select) {
        Ok(Target::Item { list, item }) => doc.item_content(list, item),
        other => panic!("{select:?} resolved to {other:?}"),
    }
}

fn refusal(doc: &Document, select: &Selector) -> (ErrorCode, String) {
    let e = doc.resolve(select).expect_err("refused");
    (e.code, e.message)
}

/// Every direct item, in document order, with its handle resolving back to
/// it: the round trip every read relies on.
fn assert_round_trip(doc: &Document) {
    for (l, list) in doc.lists.iter().enumerate() {
        for i in 0..list.items.len() {
            let target = Target::Item { list: l, item: i };
            let select = doc.position_of(target).expect("a direct item has a place");
            if let Addressing::Position(handle) = doc.address_item(l, i) {
                assert_eq!(handle, select);
                assert_eq!(doc.resolve(&handle).unwrap(), target, "{handle:?}");
            }
        }
    }
}

const PLACES: &str = "\
- preamble one
- preamble two

# Places

Intro.

## Action

- [ ] first
- [ ] second
  - nested under second

Between the lists.

1. [ ] third

> - quoted, not a unit

### Sub

- [ ] sub item

## Notes

- alpha
";

#[test]
fn first_middle_and_last_across_two_lists_in_one_section() {
    let doc = Document::parse(PLACES).unwrap();
    let action = Some(&["Action"][..]);
    assert_eq!(content(&doc, &at(action, 0)), "[ ] first");
    assert_eq!(
        content(&doc, &at(action, 1)),
        "[ ] second\n- nested under second"
    );
    // The second list continues the count; there is no list ordinal.
    assert_eq!(content(&doc, &at(action, 2)), "[ ] third");
    // The subsection's item and the quoted item are not counted in Action.
    let (code, message) = refusal(&doc, &at(action, 3));
    assert_eq!(code, ErrorCode::MissingSelector, "{message}");
    assert!(message.contains("has 3 direct items"), "{message}");
    assert_eq!(
        content(&doc, &at(Some(&["Action", "Sub"]), 0)),
        "[ ] sub item"
    );
    assert_eq!(content(&doc, &at(Some(&["Places", "Notes"]), 0)), "alpha");
    // A section with no list of its own body has no items.
    assert_eq!(
        refusal(&doc, &at(Some(&["Places"]), 0)).0,
        ErrorCode::MissingSelector
    );
    assert_round_trip(&doc);
}

#[test]
fn the_preamble_and_a_document_without_headings() {
    let doc = Document::parse(PLACES).unwrap();
    assert_eq!(content(&doc, &at(None, 0)), "preamble one");
    assert_eq!(content(&doc, &at(None, 1)), "preamble two");
    assert_eq!(refusal(&doc, &at(None, 2)).0, ErrorCode::MissingSelector);
    // `null` and an absent `heading_path` both name the preamble.
    for raw in [
        json!({"kind": "position", "index": 1}),
        json!({"kind": "position", "heading_path": null, "index": 1}),
    ] {
        let select: Selector = serde_json::from_value(raw).unwrap();
        assert_eq!(select, at(None, 1));
    }
    let flat = Document::parse("- a\n\npara\n\n- b\n- c\n").unwrap();
    assert_eq!(content(&flat, &at(None, 0)), "a");
    assert_eq!(content(&flat, &at(None, 2)), "c");
    assert_eq!(
        flat.position_of(Target::Item { list: 1, item: 0 }),
        Some(at(None, 1))
    );
    assert_round_trip(&flat);
}

#[test]
fn handles_carry_the_full_heading_path_and_a_suffix_resolves() {
    let doc = Document::parse(PLACES).unwrap();
    let Target::Item { list, item } = doc.resolve(&at(Some(&["Sub"]), 0)).unwrap() else {
        panic!()
    };
    assert_eq!(
        doc.address_item(list, item),
        Addressing::Position(at(Some(&["Places", "Action", "Sub"]), 0))
    );
}

#[test]
fn setext_headings_crlf_and_bom() {
    let setext = "Title\n=====\n\n- a\n- b\n\nPart\n----\n\n- c\n";
    let doc = Document::parse(setext).unwrap();
    assert_eq!(content(&doc, &at(Some(&["Title"]), 1)), "b");
    assert_eq!(content(&doc, &at(Some(&["Title", "Part"]), 0)), "c");
    assert_round_trip(&doc);

    let crlf = "\u{feff}# T\r\n\r\n- one\r\n- two\r\n";
    let doc = Document::parse(crlf).unwrap();
    assert_eq!(content(&doc, &at(Some(&["T"]), 1)), "two");
    let Target::Item { list, item } = doc.resolve(&at(Some(&["T"]), 0)).unwrap() else {
        panic!()
    };
    let span = doc.lists[list].items[item].span;
    assert_eq!(&crlf[span.start..span.end], "- one");
    assert_round_trip(&doc);
}

#[test]
fn malformed_handles_are_refused() {
    let doc = Document::parse(PLACES).unwrap();
    assert_eq!(
        refusal(&doc, &at(Some(&[]), 0)).0,
        ErrorCode::InvalidRequest
    );
    assert_eq!(
        refusal(&doc, &at(Some(&["Nowhere"]), 0)).0,
        ErrorCode::MissingSelector
    );
    assert_eq!(
        refusal(&doc, &at(Some(&["Places"]), u64::MAX)).0,
        ErrorCode::MissingSelector
    );
    let twice = Document::parse("## Tasks\n\n- a\n\n## Tasks\n\n- b\n").unwrap();
    assert_eq!(
        refusal(&twice, &at(Some(&["Tasks"]), 0)).0,
        ErrorCode::AmbiguousSelector
    );
    // A negative or fractional index, or an unknown field, fails parsing.
    for raw in [
        json!({"kind": "position", "heading_path": ["A"], "index": -1}),
        json!({"kind": "position", "heading_path": ["A"], "index": 1.5}),
        json!({"kind": "position", "heading_path": ["A"]}),
        json!({"kind": "position", "heading_path": ["A"], "index": 0, "list": 1}),
    ] {
        assert!(
            serde_json::from_value::<Selector>(raw.clone()).is_err(),
            "{raw}"
        );
    }
}

/// An item whose bytes repeat anywhere in the document is refused by
/// position, naming the line of the other copy; its distinct neighbours are
/// not affected.
#[test]
fn repeated_item_bytes_are_ambiguous_document_wide() {
    let cases = [
        // same list
        (
            "## A\n\n- [ ] dup\n- [ ] other\n- [ ] dup\n",
            &["A"][..],
            0,
            5,
        ),
        // another list of the same section
        ("## A\n\n- [ ] dup\n\npara\n\n- [ ] dup\n", &["A"][..], 0, 7),
        // another section
        ("## A\n\n- [ ] dup\n\n## B\n\n- [ ] dup\n", &["A"][..], 0, 7),
        // and from the other side: the first copy is named
        ("## A\n\n- [ ] dup\n\n## B\n\n- [ ] dup\n", &["B"][..], 0, 3),
    ];
    for (source, path, index, other_line) in cases {
        let doc = Document::parse(source).unwrap();
        let e = doc.resolve(&at(Some(path), index)).unwrap_err();
        assert_eq!(e.code, ErrorCode::AmbiguousSelector, "{source}");
        assert_eq!(e.line, Some(other_line), "{source}");
        assert!(
            e.message.contains(&format!("line {other_line}")),
            "{}",
            e.message
        );
        assert_round_trip(&doc);
    }
    let doc = Document::parse(cases[0].0).unwrap();
    assert_eq!(content(&doc, &at(Some(&["A"]), 1)), "[ ] other");
    // The same text in a different list marker is different bytes.
    let doc = Document::parse("## A\n\n- [ ] x\n\n## B\n\n* [ ] x\n").unwrap();
    assert_eq!(content(&doc, &at(Some(&["B"]), 0)), "[ ] x");
}

/// An item is addressed by an id that selects it, else by a position
/// that resolves back to it, else it is ambiguous with the refusal.
#[test]
fn addressing_prefers_an_id_then_a_position_that_resolves_back() {
    let source = "## A\n\n- [ ] stamped <!-- stay:AbCdEfGh subhash=sha256:111111111111 -->\n- [ ] plain\n- [ ] dup\n\n## B\n\n- [ ] dup\n\n### Notes\n\n- one\n\n### Notes\n\n- two\n";
    let doc = Document::parse(source).unwrap();
    assert_eq!(doc.address_item(0, 0), Addressing::Id("AbCdEfGh".into()));
    assert_eq!(
        doc.address_item(0, 1),
        Addressing::Position(at(Some(&["A"]), 1))
    );
    for (list, item) in [(0, 2), (1, 0), (2, 0), (3, 0)] {
        let a = doc.address_item(list, item);
        assert_eq!(a.label(), "ambiguous", "{list}/{item}: {a:?}");
        assert_eq!(a.select(), json!(null));
        assert_eq!(
            a.refusal().map(|e| e.code),
            Some(ErrorCode::AmbiguousSelector)
        );
    }
    // A stamped item still resolves by position, to the same item.
    assert_eq!(
        doc.resolve(&at(Some(&["A"]), 0)).unwrap(),
        Target::Item { list: 0, item: 0 }
    );
    assert_round_trip(&doc);
}

/// Stamping adds a child stay to each item, so its bytes and revision
/// change; the content and the place do not.
#[test]
fn a_stamp_keeps_content_and_place_and_changes_the_guard() {
    let before = Document::parse("## A\n\n- [ ] one\n- [ ] two\n").unwrap();
    let after = Document::parse(
        "## A\n\n- [ ] one <!-- stay:AbCdEfGh subhash=sha256:111111111111 -->\n- [ ] two\n",
    )
    .unwrap();
    let select = at(Some(&["A"]), 0);
    let (b, a) = (
        before.resolve(&select).unwrap(),
        after.resolve(&select).unwrap(),
    );
    assert_eq!(content(&before, &select), content(&after, &select));
    assert_ne!(before.target_revision(b), after.target_revision(a));
    let other = at(Some(&["A"]), 1);
    assert_eq!(
        before.target_revision(before.resolve(&other).unwrap()),
        after.target_revision(after.resolve(&other).unwrap())
    );
}

/// Every `Selector`-typed field in the engine, by the type or variant
/// that declares it. A new one fails here until it is placed: accepting a
/// position (and tested) or refusing it through `require_not_position`.
#[test]
fn every_selector_field_is_accounted_for() {
    let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut found = vec![];
    let mut stack = vec![src.clone()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if path.extension().is_none_or(|e| e != "rs") {
                continue;
            }
            let text = std::fs::read_to_string(&path).unwrap();
            let lines: Vec<&str> = text.lines().collect();
            for (n, line) in lines.iter().enumerate() {
                let t = line.trim();
                let Some((field, ty)) = t.split_once(": ") else {
                    continue;
                };
                let ty = ty.trim_end_matches(',');
                let field = field.trim_start_matches("pub ");
                if !matches!(
                    ty,
                    "Selector" | "Option<Selector>" | "Vec<Selector>" | "crate::Selector"
                ) || !field.chars().all(|c| c.is_ascii_lowercase() || c == '_')
                {
                    continue;
                }
                let indent = line.len() - line.trim_start().len();
                let owner = lines[..n]
                    .iter()
                    .rev()
                    .find(|l| {
                        l.trim_end().ends_with('{') && l.len() - l.trim_start().len() < indent
                    })
                    .map(|l| {
                        l.trim()
                            .trim_end_matches('{')
                            .split_whitespace()
                            .last()
                            .unwrap()
                            .to_string()
                    })
                    .unwrap();
                let file = path.strip_prefix(&src).unwrap().display().to_string();
                found.push(format!("{file} {owner}.{field}"));
            }
        }
    }
    found.sort();
    let accepted = [
        "api.rs Get.select",
        "edit.rs Set.select",
        "edit.rs UpdateItem.select",
    ];
    let refused = [
        "archive.rs CopyRequest.select",
        "delete_receipt.rs Delete.select",
        "edit.rs Append.select",
        "edit.rs Delete.select",
        "edit.rs Insert.select",
        "edit.rs Rejoin.select",
        "edit.rs RemoveParagraphInSection.select",
        "edit.rs ReplaceInSection.select",
        "edit.rs Stamp.select",
    ];
    // Output only: a refusal's hint, never read from a request.
    let output = ["contract.rs Relocated.select"];
    let mut known: Vec<String> = accepted
        .iter()
        .chain(&refused)
        .chain(&output)
        .map(|s| s.to_string())
        .collect();
    known.sort();
    assert_eq!(found, known);
}
