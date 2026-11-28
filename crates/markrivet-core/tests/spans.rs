use markrivet_core::{ErrorCode, Request, inspect, revision};

#[test]
fn fixture_extents_are_exact_original_bytes() {
    let fixtures: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/spans.json")).unwrap();
    for f in fixtures.as_array().unwrap() {
        let source = f["source"].as_str().unwrap();
        let result = inspect(source);
        assert_eq!(
            result.blocks.len(),
            f["block_count"].as_u64().unwrap() as usize,
            "{}",
            f["name"]
        );
        assert!(
            result.diagnostics.is_empty(),
            "{}: {:?}",
            f["name"],
            result.diagnostics
        );
        for expected in f["blocks"].as_array().unwrap() {
            assert!(
                result
                    .blocks
                    .iter()
                    .any(|b| b.kind == expected["kind"].as_str().unwrap()
                        && b.span.slice(source) == expected["text"].as_str()),
                "{}: missing {expected}; got {:?}",
                f["name"],
                result
                    .blocks
                    .iter()
                    .map(|b| (&b.kind, b.span.slice(source)))
                    .collect::<Vec<_>>()
            );
        }
        for b in &result.blocks {
            let mut candidate = source.to_string();
            candidate.replace_range(b.span.start..b.span.end, "REPLACEMENT");
            assert_eq!(&candidate[..b.span.start], &source[..b.span.start]);
            assert_eq!(&candidate[b.span.start + 11..], &source[b.span.end..]);
        }
    }
}

#[test]
fn yaml_plain_colon_keys_and_malformed_input() {
    for value in ["baz", "'héllo'", "\"héllo\""] {
        let source = format!("---\ntitle: t\nfoo:bar: {value} # comment\n---\n");
        let r = inspect(&source);
        assert!(r.diagnostics.is_empty(), "{:?}", r.diagnostics);
        assert_eq!(r.frontmatter[1].key, "foo:bar");
        assert_eq!(r.frontmatter[1].span.slice(&source), Some(value));
    }
    for payload in [
        "x: 1\0junk",
        "\u{a0}# bogus\nx: 1",
        "\u{a0}\nx: 1",
        "x: raw\u{7f}",
    ] {
        let r = inspect(&format!("---\nvalid: true\n{payload}\n---\n"));
        assert!(!r.diagnostics.is_empty(), "accepted {payload:?}");
        assert!(r.frontmatter.is_empty());
    }
}

#[test]
fn sections_include_descendants_but_exclude_next_peer() {
    let source = "# A\r\n\r\none\r\n## B\r\nchild\r\n# C\r\nlast";
    let r = inspect(source);
    assert_eq!(r.sections.len(), 3);
    assert_eq!(
        r.sections[0].body.slice(source),
        Some("\r\none\r\n## B\r\nchild\r\n")
    );
    assert_eq!(r.sections[1].body.slice(source), Some("child\r\n"));
    assert_eq!(r.sections[2].body.slice(source), Some("last"));
    let nested = inspect("> # quoted\n\n- # listed\n\n# real\n");
    assert_eq!(nested.sections.len(), 1);
}

#[test]
fn yaml_scalars_preserve_lexical_envelope() {
    let source = "\u{feff}---\r\n# comment\r\n'naïve:key': 'héllo ''you'''  # keep\r\nflag: true\r\nbig: 123456789012345678901234567890\r\n---\r\n# body";
    let r = inspect(source);
    assert!(r.diagnostics.is_empty(), "{:?}", r.diagnostics);
    assert_eq!(r.frontmatter.len(), 3);
    let scalar = &r.frontmatter[0];
    assert_eq!(scalar.key, "naïve:key");
    assert_eq!(scalar.value, "héllo 'you'");
    assert_eq!(scalar.span.slice(source), Some("'héllo ''you'''"));
    assert_eq!(
        r.frontmatter[2].value.to_string(),
        "123456789012345678901234567890"
    );
    assert_eq!(r.sections[0].heading.slice(source), Some("# body"));
}

#[test]
fn unsupported_yaml_returns_no_partial_fields() {
    for payload in [
        "x: &a value",
        "x: *a",
        "<<: value",
        "x: !foo value",
        "x: 1\nx: 2",
        "x: |\n  text",
        // What is refused at the top level is refused at any depth.
        "x:\n  a: &y 1",
        "x:\n  a: *y",
        "x: [!foo a]",
        "x:\n  <<: {a: 1}",
        "x:\n  a: 1\n  a: 2",
        "x: {a: 1, a: 2}",
        "x:\n- {by: a, by: b}",
        "x:\n  a: |\n    text",
        "x:\n  a: 0x20",
        "x: [01]",
        // A scalar continuing onto the next line is folded by YAML into one
        // value with nothing to mark it, so it is refused, not read short.
        "x:\n  a: foo\n    bar",
        "x:\n  a: 'foo\n    bar'",
        "x:\n- foo\n  bar",
        // A plain key YAML resolves to something other than a string, at
        // the top level and nested: `0x20` and `32` are the same integer.
        "32: a",
        "true: a",
        "~: a",
        "0x20: a",
        "n: [a]\n1: b",
        "x: {0x20: first, 32: second}",
        "x:\n  null: a",
        "x:\n- 1: a",
        // A complex key, and a flow mapping key with no value.
        "x:\n  ? [a]\n  : b",
        "x: {a, b: c}",
        "x: .nan",
        "x: 0x20",
        "x: 'unterminated",
        "x: 01",
        "x: true\n---\ny: false",
    ] {
        let source = format!("---\nvalid: yes\n{payload}\n---\nbody");
        let r = inspect(&source);
        // A --- delimiter ends frontmatter; the rest is ordinary Markdown.
        if payload.contains("\n---\n") {
            continue;
        }
        assert!(!r.diagnostics.is_empty(), "accepted {payload}");
        assert!(r.frontmatter.is_empty());
    }
}

#[test]
fn revisions_frame_fields_and_use_source_bytes() {
    assert_ne!(
        revision("ab", "c", b"d", &[]),
        revision("a", "bc", b"d", &[])
    );
    assert_ne!(
        revision("row", "x", b"same", &[b"header A"]),
        revision("row", "x", b"same", &[b"header B"])
    );
    assert_ne!(inspect("a\n").revision, inspect("a\r\n").revision);
    assert_ne!(
        inspect("a <!-- stay:x hash=sha256:1234 -->").revision,
        inspect("b <!-- stay:x hash=sha256:1234 -->").revision
    );
}

#[test]
fn contract_is_owned_and_rejects_unknown_operations() {
    let r: Request = serde_json::from_str(r##"{"op":"inspect","source":"# hi"}"##).unwrap();
    assert!(serde_json::to_string(&r).unwrap().contains("inspect"));
    assert!(serde_json::from_str::<Request>(r#"{"op":"set","source":"oops"}"#).is_err());
    assert!(
        serde_json::from_str::<Request>(r#"{"op":"inspect","source":"","write":true}"#).is_err()
    );
}

#[test]
fn yaml_unicode_character_rules_depend_on_quoting() {
    for c in ['\u{feff}', '\u{fffe}', '\u{ffff}', '\u{7f}', '\u{90}'] {
        for line in [
            format!("x: a{c}b"),
            format!("a{c}b: v"),
            format!("x: 'ok' # {c}"),
            format!("# {c}"),
        ] {
            let r = inspect(&format!("---\nvalid: true\n{line}\n---\n"));
            assert!(!r.diagnostics.is_empty(), "accepted {line:?}");
            assert!(r.frontmatter.is_empty());
        }
        for quote in ['\'', '"'] {
            let source = format!("---\n{quote}a{c}b{quote}: {quote}c{c}d{quote}\n---\n");
            let r = inspect(&source);
            assert!(
                r.diagnostics.is_empty(),
                "rejected {source:?}: {:?}",
                r.diagnostics
            );
            assert_eq!(r.frontmatter[0].key, format!("a{c}b"));
            assert_eq!(r.frontmatter[0].value, format!("c{c}d"));
        }
    }
}

#[test]
fn frontmatter_recognition_follows_markstay_section_5_3() {
    // Not frontmatter: no YAML-looking line, a blank payload line, no closer,
    // or a colon inside the first key. Each stays ordinary Markdown.
    for source in [
        "---\nTitle\n---\n\nBody.\n",
        "---\n\nIntro.\n\n---\n",
        "---\ntitle: t\n",
        "---\nfoo:bar: baz\n---\n",
        "----\ntitle: t\n----\n",
    ] {
        let r = inspect(source);
        assert!(r.frontmatter.is_empty(), "{source:?}");
        assert!(r.diagnostics.is_empty(), "{source:?}: {:?}", r.diagnostics);
    }
    // Trailing spaces on the delimiters and a `...` closer are accepted.
    let r = inspect("--- \ntitle: t\n... \nBody.\n");
    assert_eq!(r.frontmatter.len(), 1);
    assert_eq!(r.sections.len(), 0);
}

/// Real trackers open with `aliases: []` and block lists of evidence, which
/// used to make every key in the file unreadable. They are read, reported as
/// not writable, and their bytes are never rewritten.
#[test]
fn empty_values_and_lists_of_scalars_are_read_but_not_writable() {
    use serde_json::json;
    for (payload, key, value) in [
        ("aliases: []", "aliases", json!([])),
        ("aliases: [a, 'b c', 2]", "aliases", json!(["a", "b c", 2])),
        ("verified:", "verified", json!(null)),
        ("verified: [a, b,]", "verified", json!(["a", "b"])),
        ("verified:\n- a\n- b", "verified", json!(["a", "b"])),
        (
            "verified:\n  - a\n  - 'b: c'",
            "verified",
            json!(["a", "b: c"]),
        ),
        ("verified: [  ]", "verified", json!([])),
    ] {
        let source = format!("---\nstatus: active\n{payload}\nowner: sam\n---\nbody\n");
        let r = inspect(&source);
        assert!(r.diagnostics.is_empty(), "{payload}: {:?}", r.diagnostics);
        let field = r
            .frontmatter
            .iter()
            .find(|f| f.key == key)
            .unwrap_or_else(|| panic!("{payload}: no key {key}"));
        assert_eq!(field.value, value, "{payload}");
        assert!(!field.writable, "{payload}: should not be writable");
        assert_eq!(
            field.line.slice(&source),
            Some(payload),
            "{payload}: field extent"
        );
        // The neighbouring single-line scalars stay readable and writable.
        for k in ["status", "owner"] {
            let n = r.frontmatter.iter().find(|f| f.key == k).unwrap();
            assert!(n.writable, "{payload}: {k} should stay writable");
        }
    }
}

/// Tracker frontmatter carries provenance as structured values: a `generated`
/// mapping, `verified` as a list of flow mappings, `sources` as a list of block
/// mappings at the key's indentation, and deeper shapes. They read as JSON,
/// are not writable, and leave their neighbours readable and writable.
#[test]
fn structured_frontmatter_is_read_but_not_writable() {
    use serde_json::json;
    for (payload, key, value) in [
        (
            "generated:\n  by: human:owner\n  at: '2026-09-01'",
            "generated",
            json!({"by": "human:owner", "at": "2026-09-01"}),
        ),
        (
            "verified:\n  - {by: 'process:refresh', at: '2026-08-14T09:12:00+12:00'}\n  - {by: human:owner, at: 2026-08-15}",
            "verified",
            json!([{"by": "process:refresh", "at": "2026-08-14T09:12:00+12:00"},
                   {"by": "human:owner", "at": "2026-08-15"}]),
        ),
        (
            "sources:\n- id: history\n  resource: 'merged PRs, #1 to #9'\n  usage_count: 478\n- id: notes\n  resource: a/b.md",
            "sources",
            json!([{"id": "history", "resource": "merged PRs, #1 to #9", "usage_count": 478},
                   {"id": "notes", "resource": "a/b.md"}]),
        ),
        (
            "clusters:\n  - name: one\n    members: [a, b]\n    notes:\n      - x\n      -\n  - name: two\n    members: []\n    empty:",
            "clusters",
            json!([{"name": "one", "members": ["a", "b"], "notes": ["x", null]},
                   {"name": "two", "members": [], "empty": null}]),
        ),
        (
            "generated:\n  by: x # who\n  # between\n  'at: when': \"2026\"",
            "generated",
            json!({"by": "x", "at: when": "2026"}),
        ),
        ("mixed:\n- a\n- b: c", "mixed", json!(["a", {"b": "c"}])),
        (
            "flow: {a: [1, 2], b: {c: d}}",
            "flow",
            json!({"a": [1, 2], "b": {"c": "d"}}),
        ),
        (
            "nested: [a, [b], {c: [d]}]",
            "nested",
            json!(["a", ["b"], {"c": ["d"]}]),
        ),
        // A quoted key is text whatever it spells, and so is a plain key only
        // a string can be.
        (
            "keys: {'32': a, \"true\": b, 1Password: c}",
            "keys",
            json!({"32": "a", "true": "b", "1Password": "c"}),
        ),
    ] {
        for eol in ["\n", "\r\n"] {
            let source = format!("---\nstatus: active\n{payload}\nowner: sam\n---\nbody\n")
                .replace('\n', eol);
            let r = inspect(&source);
            assert!(
                r.diagnostics.is_empty(),
                "{payload:?} {eol:?}: {:?}",
                r.diagnostics
            );
            let field = r
                .frontmatter
                .iter()
                .find(|f| f.key == key)
                .unwrap_or_else(|| panic!("{payload}: no key {key}"));
            assert_eq!(field.value, value, "{payload} {eol:?}");
            assert!(!field.writable, "{payload}: should not be writable");
            assert_eq!(
                field.line.slice(&source),
                Some(payload.replace('\n', eol).as_str()),
                "{payload} {eol:?}: field extent"
            );
            for k in ["status", "owner"] {
                let n = r.frontmatter.iter().find(|f| f.key == k).unwrap();
                assert!(n.writable, "{payload}: {k} should stay writable");
            }
        }
    }
    // Recursion is bounded: nesting past the reader's depth is refused rather
    // than read, and the refusal is an ordinary diagnostic, not a crash.
    let deep = format!("---\nx: {}a{}\n---\nbody\n", "[".repeat(64), "]".repeat(64));
    assert!(!inspect(&deep).diagnostics.is_empty());
}

/// A duplicated top-level key is the one frontmatter refusal whose fix is an
/// edit only the author can choose, so the diagnostic names the key and where.
#[test]
fn a_duplicate_key_is_named() {
    let source = "---\nrevised: 2026-01-01\nstatus: active\nrevised: 2026-02-01\n---\nbody\n";
    let r = inspect(source);
    assert!(r.frontmatter.is_empty());
    let d = &r.diagnostics[0];
    assert_eq!(d.code, ErrorCode::UnsupportedSyntax);
    assert!(d.message.contains("\"revised\""), "{}", d.message);
    assert_eq!(
        d.span.unwrap().slice(source),
        Some("revised: 2026-02-01"),
        "the span names the second occurrence"
    );
}

/// A plain scalar that starts like a number: taken as a JSON number when it
/// is one, refused when only YAML would read it as one, and otherwise text.
#[test]
fn plain_scalars_that_start_like_numbers() {
    use serde_json::json;
    for (payload, value) in [
        ("v: 1Password", json!("1Password")),
        ("v: 6.9.6", json!("6.9.6")),
        ("v: ../plans/PLAN_X.md", json!("../plans/PLAN_X.md")),
        ("v: 2026-09-06", json!("2026-09-06")),
        ("v: 2026-13-45", json!("2026-13-45")),
        // A sexagesimal group is at most two digits and under 60; anything
        // else, including an exponent or `_` in a group, is text.
        ("v: 1:99", json!("1:99")),
        ("v: 1:2e3", json!("1:2e3")),
        ("v: 1:2_0", json!("1:2_0")),
        // YAML's integer starts at a digit, so a leading `_` is text.
        ("v: +_1", json!("+_1")),
        ("v: 12", json!(12)),
        ("v: -1.5e3", serde_json::from_str("-1.5e3").unwrap()),
        ("v: [1Password, 6.9.6, 2]", json!(["1Password", "6.9.6", 2])),
    ] {
        let source = format!("---\n{payload}\n---\nbody\n");
        let r = inspect(&source);
        assert!(r.diagnostics.is_empty(), "{payload}: {:?}", r.diagnostics);
        assert_eq!(r.frontmatter[0].value, value, "{payload}");
    }
    for payload in [
        "v: 0x20",
        "v: 0o17",
        "v: 0b101",
        "v: 01",
        "v: 1_000",
        "v: 1:30",
        "v: .nan",
        "v: -.inf",
        "v: .5",
        "v: 1.",
        "v: +12",
        // A radix body is `[0-9a-fA-F_]+`, so `_` alone matches and resolves
        // to an integer whose construction fails. Refused, not read as text.
        "v: 0x_",
        "v: 0X20",
        "v: +.nan",
        "v: .iNf",
        // YAML 1.1 lets the last sexagesimal group carry a fraction, so
        // `1:30.5` is 90.5. Reading it back as text would let a later `set`
        // of that string write a number.
        "v: 1:30.5",
        "v: 1:30:15.25",
        // YAML 1.0 separates digits with `,`, so these are integers there.
        "v: +12,345",
        "v: 1,230.15",
        "v: 12,345",
        "v: 0b_",
        "v: 0o_",
        "v: [0x20]",
        "v: [1, .nan]",
    ] {
        let source = format!("---\n{payload}\n---\nbody\n");
        assert!(
            !inspect(&source).diagnostics.is_empty(),
            "accepted {payload}"
        );
    }
}

/// A calendar date is a YAML timestamp with no JSON type. It reads as its own
/// text and writes back unquoted, so `updated:` keeps the shape the file had.
#[test]
fn a_bare_date_reads_as_text_and_stays_writable() {
    let source = "---\nupdated: 2026-09-06\n---\nbody\n";
    let r = inspect(source);
    assert!(r.diagnostics.is_empty(), "{:?}", r.diagnostics);
    let f = &r.frontmatter[0];
    assert_eq!(f.value, serde_json::json!("2026-09-06"));
    assert!(f.writable, "a date is a single-line scalar");
    assert_eq!(f.span.slice(source), Some("2026-09-06"));
}

/// Only a key YAML leaves open takes continuation lines. An indented comment
/// after a scalar is a comment, not part of its value, and a `-label:` key is
/// a key rather than a sequence entry.
#[test]
fn a_scalar_line_is_a_whole_field() {
    for (name, source, keys) in [
        (
            "indented comment",
            "---\nstatus: active\n  # explanation\nowner: sam\n---\nbody\n",
            vec!["status", "owner"],
        ),
        (
            "a key that starts with a dash",
            "---\nstatus: active\n-label: value\n---\nbody\n",
            vec!["status", "-label"],
        ),
        (
            "a comment inside a block list",
            "---\nverified:\n  # why\n  - a\nowner: sam\n---\nbody\n",
            vec!["verified", "owner"],
        ),
    ] {
        let r = inspect(source);
        assert!(r.diagnostics.is_empty(), "{name}: {:?}", r.diagnostics);
        let got: Vec<&str> = r.frontmatter.iter().map(|f| f.key.as_str()).collect();
        assert_eq!(got, keys, "{name}");
    }
}

/// A flow sequence alternates scalar and `,`. The scanner tokenising a string
/// is not the grammar taking it, so a doubled or leading separator is
/// unsupported rather than silently dropped.
#[test]
fn a_malformed_flow_sequence_is_refused() {
    // `[a b]` is one plain scalar, which YAML allows, so it is not here.
    for payload in [
        "v: [a,,b]",
        "v: [,]",
        "v: [,a]",
        "v: [a, [b,,c]]",
        "v: {a: 1,, b: 2}",
        "v: [{a: 1},,]",
    ] {
        let source = format!("---\n{payload}\n---\nbody\n");
        assert!(
            !inspect(&source).diagnostics.is_empty(),
            "accepted {payload}"
        );
    }
}

/// The block and item spans a guard covers, over `Document`, because
/// `inspect` does not reach the helper these exercise.
///
/// A blank line is CommonMark's, spaces and tabs, so a line of other
/// whitespace is content and keeps its block open. A fenced block's blank
/// lines are its own content, and a list item holding one keeps them with it.
/// An indented code block excludes its trailing blanks, an HTML block ends at
/// the first one, and a gap between two blocks belongs to neither.
#[test]
fn a_span_ends_on_the_last_line_its_block_owns() {
    use markrivet_core::Document;
    for (name, src, blocks, items) in [
        (
            "a space-filled gap belongs to neither list",
            "- a\n \n \n1. b\n",
            vec!["- a", "1. b"],
            vec!["- a", "1. b"],
        ),
        (
            "an empty gap belongs to neither list",
            "- a\n- b\n\n\n1. x\n",
            vec!["- a\n- b", "1. x"],
            vec!["- a", "- b", "1. x"],
        ),
        (
            "a tab line inside a fence is its content",
            "~~~\ntext\n\t\n",
            vec!["~~~\ntext\n\t"],
            vec![],
        ),
        (
            "an empty line inside a fence is its content",
            "~~~\ntext\n\n",
            vec!["~~~\ntext\n"],
            vec![],
        ),
        (
            "an item keeps the blank lines its fenced child owns",
            "- ~~~\n  text\n  \t\n",
            vec!["- ~~~\n  text\n  \t"],
            vec!["- ~~~\n  text\n  \t"],
        ),
        (
            "indented code excludes its trailing blanks",
            "    code\n\n\n1. b\n",
            vec!["    code", "1. b"],
            vec!["1. b"],
        ),
        (
            // comrak reports an indented code block that ends an item before
            // a blank line as ending one byte into its first line.
            "indented code ending an item keeps every content line",
            "- a\n\n      code a\n      code b\n\n- b\n",
            vec!["- a\n\n      code a\n      code b\n\n- b"],
            vec!["- a\n\n      code a\n      code b", "- b"],
        ),
        (
            "indented code ending an item before a paragraph",
            "- a\n\n      code a\n\n      code b\n\n\nx\n",
            vec!["- a\n\n      code a\n\n      code b", "x"],
            vec!["- a\n\n      code a\n\n      code b"],
        ),
        (
            "indented code ending an item, CRLF",
            "- a\r\n\r\n      code a\r\n      code b\r\n\r\n- b\r\n",
            vec!["- a\r\n\r\n      code a\r\n      code b\r\n\r\n- b"],
            vec!["- a\r\n\r\n      code a\r\n      code b", "- b"],
        ),
        (
            "indented code ending an ordered item",
            "1. a\n\n       code a\n       code b\n\n2. b\n",
            vec!["1. a\n\n       code a\n       code b\n\n2. b"],
            vec!["1. a\n\n       code a\n       code b", "2. b"],
        ),
        (
            "an html block runs to its terminator, not to a blank line",
            "- <pre>\n  text\n  \t\n",
            vec!["- <pre>\n  text\n  \t"],
            vec!["- <pre>\n  text\n  \t"],
        ),
        (
            "an html block keeps the blank lines inside it",
            "<!-- a\n\nb\n-->\n\npara\n",
            vec!["<!-- a\n\nb\n-->", "para"],
            vec![],
        ),
        (
            "a fence comrak shortened keeps its content",
            "- ~~~\n  text\n\n",
            vec!["- ~~~\n  text\n"],
            vec!["- ~~~\n  text\n"],
        ),
        (
            // An empty literal must not invent a line, or the fence takes
            // the next item and anything after it.
            "an empty fence owns only its own line",
            "- ~~~\n- next\n",
            vec!["- ~~~\n- next"],
            vec!["- ~~~", "- next"],
        ),
        (
            // comrak consumes the indentation of a final line with no
            // terminator, dropping it from `literal` while still reporting
            // it, so the extent takes whichever reaches further.
            "a dedented final line is still owned",
            "- ~~~\n  text\n  ",
            vec!["- ~~~\n  text\n  "],
            vec!["- ~~~\n  text\n  "],
        ),
        (
            // Known limitation, see docs/parser-contract.md: comrak drops a
            // consumed final indentation from `sourcepos` and `literal`
            // alike, so no arithmetic over the two recovers it. The document
            // revision still covers the byte.
            "a consumed final indentation is not in the block span",
            "- <?\n ",
            vec!["- <?"],
            vec!["- <?"],
        ),
        (
            "a non-breaking space keeps its paragraph open",
            "text\n\u{a0}\n",
            vec!["text\n\u{a0}"],
            vec![],
        ),
    ] {
        let d = Document::parse(src).unwrap();
        let got_blocks: Vec<&str> = d
            .blocks
            .iter()
            .map(|b| &d.source[b.span.start..b.span.end])
            .collect();
        let got_items: Vec<&str> = d
            .lists
            .iter()
            .flat_map(|l| l.items.iter())
            .map(|i| &d.source[i.span.start..i.span.end])
            .collect();
        assert_eq!(got_blocks, blocks, "{name}: blocks");
        assert_eq!(got_items, items, "{name}: items");
    }
    // A guard moves when a line a fence owns changes.
    // The block's own guard moves when a line it owns changes, which a
    // whole-document revision cannot show.
    let block_rev = |src: &str| {
        let d = Document::parse(src).unwrap();
        d.blocks
            .iter()
            .map(|b| {
                revision(
                    "block",
                    "",
                    &d.source.as_bytes()[b.span.start..b.span.end],
                    &[],
                )
            })
            .collect::<Vec<_>>()
    };
    assert_ne!(block_rev("~~~\ntext\n\t\n"), block_rev("~~~\ntext\n    \n"));
    assert_ne!(
        block_rev("- ~~~\n  text\n\n"),
        block_rev("- ~~~\n  textA\n\n")
    );
    assert_ne!(block_rev("<pre>\ntext\n\n"), block_rev("<pre>\ntext\n\n\n"));
    assert_ne!(
        block_rev("- ~~~\n  text\n "),
        block_rev("- ~~~\n  text\n  ")
    );
    // An item's guard moves when a later line of its indented code changes.
    let item_rev = |src: &str| {
        let d = Document::parse(src).unwrap();
        let i = &d.lists[0].items[0];
        revision(
            "item",
            "",
            &d.source.as_bytes()[i.span.start..i.span.end],
            &[],
        )
    };
    assert_ne!(
        item_rev("- a\n\n      code a\n      code b\n\n- b\n"),
        item_rev("- a\n\n      code a\n      code B\n\n- b\n")
    );

    // An empty fence does not reach a marker on the next line, so that
    // marker addresses nothing rather than attaching to the item.
    let d = Document::parse("- ~~~\n<!-- stay:x subhash=sha256:0011223344556677 -->\n").unwrap();
    assert!(
        d.diagnostics
            .iter()
            .any(|x| x.code == ErrorCode::UnaddressedMarker),
        "expected an unaddressed marker: {:?}",
        d.diagnostics
    );
    assert!(
        d.lists[0].items[0].markers.is_empty(),
        "the marker must not attach to the item"
    );
}

/// A block sequence survives a comment between its entries, and a flow
/// sequence may close on a later line. A tab in block indentation is YAML the
/// scanner does not catch, so it is refused rather than read past.
#[test]
fn collections_span_the_lines_they_actually_own() {
    use serde_json::json;
    for (name, payload, value) in [
        (
            "comment between entries",
            "v:\n- a\n# explanation\n- b",
            json!(["a", "b"]),
        ),
        (
            "indented comment between entries",
            "v:\n  - a\n  # why\n  - b",
            json!(["a", "b"]),
        ),
        (
            "flow list over two lines",
            "v: [a,\n  b]",
            json!(["a", "b"]),
        ),
        (
            "a bracket inside a comment",
            "v: [a, b] # ]",
            json!(["a", "b"]),
        ),
    ] {
        let source = format!("---\n{payload}\nowner: sam\n---\nbody\n");
        let r = inspect(&source);
        assert!(r.diagnostics.is_empty(), "{name}: {:?}", r.diagnostics);
        let f = r.frontmatter.iter().find(|f| f.key == "v").expect(name);
        assert_eq!(f.value, value, "{name}");
        assert!(
            r.frontmatter.iter().any(|f| f.key == "owner"),
            "{name}: the next key is still its own field"
        );
    }
    assert!(
        !inspect("---\nv:\n  \t- a\n---\nbody\n")
            .diagnostics
            .is_empty(),
        "a tab in block indentation should be refused"
    );
}

/// A span that ends on a multi-byte character must not be reported by slicing
/// one byte back from its end.
///
/// `describe` asks for the line of `span.end - 1`, which is a continuation byte
/// whenever the last character is multi-byte, and `file_line` sliced `&str`
/// there. A list item ending in an emoji is the shape that found it: read-only
/// `map` panicked on a real 792-line document, reported as `internal`, which is
/// the class that means an invariant broke rather than a request being refused.
#[test]
fn a_span_ending_on_a_multibyte_character_reports_its_line() {
    for source in [
        "4. \u{2705}\n",
        "1. \u{2705}\n",
        "- \u{2705}\n",
        "- [ ] shipped \u{2705}\n",
        "# Head\n\n- one\n- two \u{2705}\n",
        "- \u{1f600}\u{1f600}\n",
        "- caf\u{e9}\n",
    ] {
        let doc = markrivet_core::Document::parse(source).expect("parses");
        for block in &doc.blocks {
            let end = doc.file_line(block.span.end.max(block.span.start + 1) - 1);
            assert!(
                end >= doc.file_line(block.span.start),
                "{source:?}: end line {end} precedes the start line"
            );
        }
    }
}

/// `file_line` counts line feeds, and it must count them the same whether or
/// not the position it is given sits inside a character.
#[test]
fn file_line_counts_over_bytes_not_characters() {
    let doc = markrivet_core::Document::parse("a\n\u{2705}b\nc\n").expect("parses");
    assert_eq!(doc.file_line(0), 1);
    assert_eq!(doc.file_line(2), 2, "first byte of the emoji is on line 2");
    assert_eq!(doc.file_line(3), 2, "inside the emoji is still line 2");
    assert_eq!(doc.file_line(4), 2, "still inside it");
    assert_eq!(doc.file_line(6), 2, "the byte after it");
    assert_eq!(doc.file_line(8), 3);
}

#[test]
fn extreme_numeric_exponent_is_refused_without_overflow() {
    use markrivet_core::schema::{
        self, Clause, Cmp, FieldSpec, FieldType, ItemShape, Schema, Source,
    };
    let value: serde_json::Value =
        serde_json::from_str("1e-170141183460469231731687303715884105728").unwrap();
    assert!(!schema::comparable(value.as_number().unwrap()));
    let field = FieldSpec {
        kind: FieldType::Number,
        from: Source::Label,
        required: false,
        values: vec![],
        tags: vec![],
        label: Some("Number".into()),
        multiline: false,
        key: None,
        field: None,
        default: None,
    };
    let schema = Schema {
        id: "numbers".into(),
        version: 1,
        item: ItemShape::Checkbox,
        fields: std::collections::BTreeMap::from([("number".into(), field)]),
    };
    assert!(
        schema::check_clause(
            &schema,
            &Clause {
                field: "number".into(),
                op: Cmp::Eq,
                value
            }
        )
        .is_err()
    );
}
