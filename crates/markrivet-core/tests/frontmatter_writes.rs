use markrivet_core::edit::{self, Op, Outcome, Planner};
use markrivet_core::{Diagnostic, Document, ErrorCode, Selector};
use serde_json::{Value, json};

fn run(source: &str, op: Op) -> Result<Outcome, Diagnostic> {
    let doc = Document::parse(source).unwrap();
    let change = Planner::new(&doc).structural(&op)?;
    edit::plan(&doc, vec![change])
}

fn guard(source: &str, key: &str) -> String {
    let doc = Document::parse(source).unwrap();
    doc.target_revision(
        doc.resolve(&Selector::FrontmatterKey { key: key.into() })
            .unwrap(),
    )
}

fn set(source: &str, field: &str, value: Value) -> Result<Outcome, Diagnostic> {
    run(
        source,
        Op::SetKeyField {
            key_path: vec!["generated".into(), field.into()],
            value,
            guard: guard(source, "generated"),
        },
    )
}

fn delete(source: &str, key: &str) -> Result<Outcome, Diagnostic> {
    run(
        source,
        Op::DeleteKey {
            key: key.into(),
            guard: guard(source, key),
        },
    )
}

fn stamped(source: &str) -> String {
    run(
        source,
        Op::Stamp {
            collection: None,
            checkbox_only: false,
            collection_guard: None,
            select: Selector::Document,
            guard: Document::parse(source).unwrap().revision(),
        },
    )
    .unwrap()
    .candidate
}

fn same_stays(before: &str, after: &str) {
    let before = Document::parse(before).unwrap();
    let after = Document::parse(after).unwrap();
    assert!(!before.ids.is_empty());
    assert_eq!(
        before.ids.keys().collect::<Vec<_>>(),
        after.ids.keys().collect::<Vec<_>>()
    );
    for (id, targets) in &before.ids {
        let old = targets[0];
        let new = after.resolve(&Selector::Id { id: id.clone() }).unwrap();
        assert_eq!(std::mem::discriminant(&old), std::mem::discriminant(&new));
        assert_eq!(
            before.target_revision(old),
            after.target_revision(new),
            "{id}"
        );
    }
}

#[test]
fn mapping_field_splices_only_its_scalar_in_flow_and_block_renderings() {
    for mapping in [
        "generated: {by: 'process:synthetic', at: 'old', keep: old} # tail",
        "'generated': # parent\n  by: 'process:synthetic' # who\n  'at': 'old'   # when\n  keep: old",
        "generated: {by: 'é😀',\n  at: 'old', # when\n  keep: old,}",
        "generated:\n    by: é😀\n    at: 'old'\n    keep: {nested: [a, b]}",
    ] {
        for crlf in [false, true] {
            let source = format!(
                "\u{feff}---\n# retained\n{mapping}\nstatus: active # neighbour\n...\n\n# Widgets\n\n- [ ] Ship synthetic widget.\n\n## Log\n\nKept."
            );
            let source = if crlf {
                source.replace('\n', "\r\n")
            } else {
                source
            };
            let source = stamped(&source);
            let out = set(&source, "at", json!("2026-11-02T12:00:00+13:00")).unwrap();
            assert_eq!(
                out.candidate,
                source.replacen("'old'", "'2026-11-02T12:00:00+13:00'", 1)
            );
            same_stays(&source, &out.candidate);
        }
    }
}

#[test]
fn mapping_scalar_rendering_round_trips_in_its_actual_yaml_context() {
    for original in ["old", "'old'", "\"old\""] {
        for value in [
            json!("a, b}"),
            json!("x: y"),
            json!(" # hash"),
            json!("it's"),
            json!("true"),
            json!("~"),
            json!(""),
            json!("é😀"),
            json!(true),
            json!(null),
            json!(23),
        ] {
            let source = format!(
                "---\ngenerated: {{by: 'é😀', at: {original}, other: [a, b]}} # kept\nstatus: active\n---\n# Widgets\n"
            );
            let out = set(&source, "at", value.clone()).unwrap();
            let doc = Document::parse(&out.candidate).unwrap();
            let field = doc.scalars().iter().find(|s| s.key == "generated").unwrap();
            assert_eq!(
                field.value,
                json!({"by": "é😀", "at": value, "other": ["a", "b"]})
            );
            assert!(
                out.candidate
                    .starts_with("---\ngenerated: {by: 'é😀', at: ")
            );
            assert!(
                out.candidate
                    .ends_with(", other: [a, b]} # kept\nstatus: active\n---\n# Widgets\n")
            );
        }
    }
}

#[test]
fn mapping_field_refuses_missing_empty_structured_and_nonmapping_targets() {
    for (mapping, field, value, code) in [
        (
            "generated: scalar",
            "at",
            json!("new"),
            ErrorCode::UnsupportedSyntax,
        ),
        (
            "generated: [a, b]",
            "at",
            json!("new"),
            ErrorCode::UnsupportedSyntax,
        ),
        (
            "generated: {}",
            "at",
            json!("new"),
            ErrorCode::MissingSelector,
        ),
        (
            "generated: {by: synthetic}",
            "at",
            json!("new"),
            ErrorCode::MissingSelector,
        ),
        (
            "generated:\n  at:",
            "at",
            json!("new"),
            ErrorCode::UnsupportedSyntax,
        ),
        (
            "generated: {at: [a]}",
            "at",
            json!("new"),
            ErrorCode::UnsupportedSyntax,
        ),
        (
            "generated: {at: {nested: old}}",
            "at",
            json!("new"),
            ErrorCode::UnsupportedSyntax,
        ),
        (
            "generated: {at: old}",
            "at",
            json!(["new"]),
            ErrorCode::UnsupportedSyntax,
        ),
        (
            "generated: {at: old}",
            "at",
            json!("new\nline"),
            ErrorCode::UnsupportedSyntax,
        ),
    ] {
        let source = format!("---\n{mapping}\nstatus: active\n---\n# Widgets\n");
        assert_eq!(set(&source, field, value).unwrap_err().code, code);
    }
}

#[test]
fn literal_key_path_segments_support_dots_and_quoted_unicode_keys() {
    let source = "---\n'gen.erated': {'日.at': 'old', keep: é}\nstatus: active\n---\n# Widgets\n";
    let out = run(
        source,
        Op::SetKeyField {
            key_path: vec!["gen.erated".into(), "日.at".into()],
            value: json!("new"),
            guard: guard(source, "gen.erated"),
        },
    )
    .unwrap();
    assert_eq!(out.candidate, source.replace("'old'", "'new'"));
    for key_path in [
        vec![],
        vec!["generated".into()],
        vec!["generated".into(), "at".into(), "deeper".into()],
    ] {
        assert_eq!(
            run(
                source,
                Op::SetKeyField {
                    key_path,
                    value: json!("new"),
                    guard: String::new()
                }
            )
            .unwrap_err()
            .code,
            ErrorCode::InvalidRequest
        );
    }
}

#[test]
fn delete_key_removes_its_whole_lines_and_keeps_other_bytes_and_stays() {
    for field in [
        "'retired': 'old' # own comment",
        "retired: {by: synthetic, at: 'old'} # own comment",
        "retired:\n  by: synthetic\n  at: 'old'",
        "retired:\n- a\n- b",
        "retired:",
    ] {
        for crlf in [false, true] {
            let source = format!(
                "---\n# before\n{field}\n# after\nstatus: active # kept\n---\n\n# Widgets\n\n- [ ] Synthetic action.\n"
            );
            let source = if crlf {
                source.replace('\n', "\r\n")
            } else {
                source
            };
            let source = stamped(&source);
            let out = delete(&source, "retired").unwrap();
            let field = if crlf {
                field.replace('\n', "\r\n")
            } else {
                field.into()
            };
            let newline = if crlf { "\r\n" } else { "\n" };
            assert_eq!(
                out.candidate,
                source.replacen(&format!("{field}{newline}"), "", 1)
            );
            same_stays(&source, &out.candidate);
        }
    }
    let source = "---\nstatus: active\nretired: old\n...\n# Widgets";
    assert_eq!(
        delete(source, "retired").unwrap().candidate,
        source.replace("retired: old\n", "")
    );
}

#[test]
fn deleting_the_last_key_and_using_another_keys_guard_are_refused() {
    assert_eq!(
        delete("---\nretired: old\n---\n# Widgets\n", "retired")
            .unwrap_err()
            .code,
        ErrorCode::UnsupportedSyntax
    );
    let source = "---\ngenerated: {at: old}\nretired: old\n---\n# Widgets\n";
    assert_eq!(
        run(
            source,
            Op::DeleteKey {
                key: "retired".into(),
                guard: guard(source, "generated"),
            }
        )
        .unwrap_err()
        .code,
        ErrorCode::GuardConflict
    );
    assert_eq!(
        run(
            source,
            Op::SetKeyField {
                key_path: vec!["generated".into(), "at".into()],
                value: json!("new"),
                guard: guard(source, "retired"),
            }
        )
        .unwrap_err()
        .code,
        ErrorCode::GuardConflict
    );
}

// ---------------------------------------------------------------------------
// add_key_yaml and set_key_yaml: the caller's exact token, scalar or flat list

/// Lead comment, quoted scalar with a comment, flow list, flow and block
/// mappings, a block list and a Unicode neighbour; the body has no final
/// newline.
const YAML_DOC: &str = "---\n# lead comment\nscope: 'old scope' # old tail\nlenses: [a, b]   # list tail\ngenerated: {by: 'process:synthetic', at: old}\nsources:\n  - one\n  - two\nowner: é # ünicode\n---\n\n# Widgets\n\n- [ ] Ship synthetic widget.\n\n## Log\n\nKept.";

fn doc_guard(source: &str) -> String {
    Document::parse(source).unwrap().revision()
}

fn add_yaml(source: &str, key: &str, yaml: &str) -> Result<Outcome, Diagnostic> {
    run(
        source,
        Op::AddKeyYaml {
            key: key.into(),
            yaml: yaml.into(),
            guard: doc_guard(source),
        },
    )
}

fn set_yaml(
    source: &str,
    key: &str,
    yaml: &str,
    replace_line: bool,
) -> Result<Outcome, Diagnostic> {
    run(
        source,
        Op::SetKeyYaml {
            key: key.into(),
            yaml: yaml.into(),
            guard: guard(source, key),
            replace_line,
        },
    )
}

fn value_of(source: &str, key: &str) -> Value {
    let doc = Document::parse(source).unwrap();
    doc.scalars()
        .iter()
        .find(|s| s.key == key)
        .unwrap_or_else(|| panic!("{key} missing from {source:?}"))
        .value
        .clone()
}

/// LF and CRLF, with and without a BOM, each stamped so stays can be checked.
fn yaml_variants(base: &str) -> Vec<(String, &'static str)> {
    let mut out = vec![];
    for bom in [false, true] {
        for nl in ["\n", "\r\n"] {
            let s = base.replace('\n', nl);
            let s = if bom { format!("\u{feff}{s}") } else { s };
            out.push((stamped(&s), nl));
        }
    }
    out
}

/// Accepted tokens and the JSON each must read back as. The expected value is
/// parsed from JSON text so arbitrary-precision numbers compare exactly.
const YAML_TOKENS: &[(&str, &str)] = &[
    ("[x, 'y z']", r#"["x", "y z"]"#),
    ("[]", "[]"),
    ("[ a ,b ]", r#"["a", "b"]"#),
    (
        "[a, 1, 1.5, true, null, 'q', \"d\", 2026-11-13,]",
        r#"["a", 1, 1.5, true, null, "q", "d", "2026-11-13"]"#,
    ),
    (
        "['a]b', \"c#d\", 'e, f', 'it''s', \"q\\\"r\", 'é😀']",
        r#"["a]b", "c#d", "e, f", "it's", "q\"r", "é😀"]"#,
    ),
    ("'it''s'", r#""it's""#),
    ("\"q\\\"r\"", r#""q\"r""#),
    ("plain text", r#""plain text""#),
    ("42", "42"),
    ("12345678901234567890.0001", "12345678901234567890.0001"),
    ("2026-11-13", r#""2026-11-13""#),
    ("''", r#""""#),
    ("\"\"", r#""""#),
    ("'ünï 😀 # not a comment'", r#""ünï 😀 # not a comment""#),
];

#[test]
fn yaml_add_and_set_copy_exact_tokens_as_arrays_and_scalars() {
    for (source, nl) in yaml_variants(YAML_DOC) {
        for (token, json_text) in YAML_TOKENS {
            let want: Value = serde_json::from_str(json_text).unwrap();
            for (key, old) in [("scope", "'old scope'"), ("lenses", "[a, b]")] {
                let out = set_yaml(&source, key, token, false).unwrap();
                assert_eq!(
                    out.candidate,
                    source.replacen(&format!("{key}: {old}"), &format!("{key}: {token}"), 1),
                    "{key} <- {token}"
                );
                assert_eq!(value_of(&out.candidate, key), want, "{token}");
                same_stays(&source, &out.candidate);
                assert_eq!(
                    out.ops[0].revision.as_deref(),
                    Some(guard(&out.candidate, key).as_str())
                );
            }
            let out = add_yaml(&source, "added", token).unwrap();
            let fence = format!("{nl}---{nl}{nl}# Widgets");
            assert_eq!(
                out.candidate,
                source.replacen(&fence, &format!("{nl}added: {token}{fence}"), 1),
                "added <- {token}"
            );
            assert_eq!(value_of(&out.candidate, "added"), want, "{token}");
            same_stays(&source, &out.candidate);
            assert_eq!(
                out.ops[0].revision.as_deref(),
                Some(guard(&out.candidate, "added").as_str())
            );
        }
    }
}

#[test]
fn yaml_scalar_and_list_swap_and_the_reader_reports_arrays_not_strings() {
    let source = "---\nscope: one\nlenses: [a, b]\n---\n# T\n";
    let out = set_yaml(source, "scope", "[one, two]", false).unwrap();
    assert_eq!(value_of(&out.candidate, "scope"), json!(["one", "two"]));
    let out = set_yaml(&out.candidate, "lenses", "'a, b'", false).unwrap();
    assert_eq!(
        out.candidate,
        "---\nscope: [one, two]\nlenses: 'a, b'\n---\n# T\n"
    );
    assert_eq!(value_of(&out.candidate, "lenses"), json!("a, b"));
    // The legacy capability flag is unchanged: lists stay read-only to `set`.
    let doc = Document::parse(&out.candidate).unwrap();
    let flags: Vec<(&str, bool)> = doc
        .scalars()
        .iter()
        .map(|s| (s.key.as_str(), s.writable))
        .collect();
    assert_eq!(flags, [("scope", false), ("lenses", true)]);
}

#[test]
fn yaml_comment_policies_preserve_or_remove_the_first_line_tail() {
    for nl in ["\n", "\r\n"] {
        let s = |t: &str| t.replace('\n', nl);
        // Same token: preserving is a no-op; removing changes once, then is.
        let source = s("---\nscope: 'new scope' # old tail\nnext: x\n---\n# T\n");
        assert_eq!(
            set_yaml(&source, "scope", "'new scope'", false)
                .unwrap()
                .candidate,
            source
        );
        let removed = set_yaml(&source, "scope", "'new scope'", true)
            .unwrap()
            .candidate;
        assert_eq!(removed, source.replace(" # old tail", ""));
        for replace_line in [true, false] {
            assert_eq!(
                set_yaml(&removed, "scope", "'new scope'", replace_line)
                    .unwrap()
                    .candidate,
                removed
            );
        }
        for (line, token, kept, cleared) in [
            // A quoted `#` belongs to the old token in either mode.
            ("title: 'a # b' # c", "'x'", "title: 'x' # c", "title: 'x'"),
            (
                "lenses: [a, b]\t# t",
                "[c]",
                "lenses: [c]\t# t",
                "lenses: [c]",
            ),
            (
                "lenses: [a, '#]']  # t ]",
                "x",
                "lenses: x  # t ]",
                "lenses: x",
            ),
            ("k: v   ", "w", "k: w   ", "k: w"),
            ("k: v", "[]", "k: []", "k: []"),
            (
                "'ключ😀': [é, 'ü]'] # ç",
                "['ß', 😀]",
                "'ключ😀': ['ß', 😀] # ç",
                "'ключ😀': ['ß', 😀]",
            ),
        ] {
            let source = s(&format!("---\n# lead\n{line}\nnext: x\n---\n# T\n"));
            let key = Document::parse(&source).unwrap().scalars()[0].key.clone();
            for (replace_line, want) in [(false, kept), (true, cleared)] {
                let out = set_yaml(&source, &key, token, replace_line).unwrap();
                assert_eq!(out.candidate, source.replacen(line, want, 1), "{line}");
            }
        }
        // Standalone comment lines the reader gave the field stay in both modes.
        let source =
            s("---\nlenses: [a, b] # tail\n  # continuation\n  # more\nnext: x\n---\n# T\n");
        for (replace_line, want) in [(false, "lenses: x # tail"), (true, "lenses: x")] {
            let out = set_yaml(&source, "lenses", "x", replace_line).unwrap();
            assert_eq!(
                out.candidate,
                source.replacen("lenses: [a, b] # tail", want, 1)
            );
            assert_eq!(value_of(&out.candidate, "next"), json!("x"));
        }
    }
}

#[test]
fn yaml_quote_equivalent_tokens_change_bytes_intentionally() {
    let source = "---\nscope: 'v'\n---\n# T\n";
    for token in ["\"v\"", "v"] {
        let out = set_yaml(source, "scope", token, false).unwrap();
        assert_eq!(out.candidate, source.replace("'v'", token));
        assert_eq!(value_of(&out.candidate, "scope"), json!("v"));
    }
}

#[test]
fn yaml_unsupported_tokens_targets_and_guards_are_refused() {
    let source = "---\nscope: 'v' # a\nlenses: [a, b]\n---\n# T\n";
    for token in [
        "",
        " a",
        "a ",
        "a # c",
        "[a] # c",
        "[a] x",
        "'a' b",
        "&x a",
        "*x",
        "!!str a",
        "{}",
        "{a: 1}",
        "[[a]]",
        "[{a: 1}]",
        "[a,,b]",
        "[,]",
        "[a",
        "'open",
        "a\nb",
        "a\r",
        "'a\tb'",
        "a\u{85}b",
        "a\u{2028}b",
        "|",
        ">",
        "- a",
        "a: b",
        "~",
        "0x10",
        "[0x10]",
        "[~]",
        "#c",
        "[a]#c",
    ] {
        for key in ["scope", "lenses"] {
            for replace_line in [false, true] {
                assert_eq!(
                    set_yaml(source, key, token, replace_line).unwrap_err().code,
                    ErrorCode::UnsupportedSyntax,
                    "{key} <- {token:?}"
                );
            }
        }
        assert_eq!(
            add_yaml(source, "added", token).unwrap_err().code,
            ErrorCode::UnsupportedSyntax,
            "added <- {token:?}"
        );
    }
    for field in [
        "target:",
        "target:\n  - a",
        "target:\n- a",
        "target: {a: 1}",
        "target: {}",
        "target: [[a]]",
        "target: [{a: 1}]",
        "target: [a,\n  b]",
        "target:\n  a: 1",
    ] {
        let source = format!("---\n{field}\nnext: x\n---\n# T\n");
        assert_eq!(
            set_yaml(&source, "target", "x", false).unwrap_err().code,
            ErrorCode::UnsupportedSyntax,
            "{field}"
        );
    }
    // A quoted key may hold characters the collection reader refuses
    // anywhere, so such a key keeps a scalar but cannot take a list.
    for odd in ['\u{feff}', '\u{fffe}', '\u{ffff}', '\u{86}'] {
        let source = format!("---\n'k{odd}': a\nnext: x\n---\n# T\n");
        let key = format!("k{odd}");
        assert_eq!(
            set_yaml(&source, &key, "[a]", false).unwrap_err().code,
            ErrorCode::UnsupportedSyntax,
            "{odd:?}"
        );
        let out = set_yaml(&source, &key, "'b'", false).unwrap();
        assert_eq!(out.candidate, source.replace(": a", ": 'b'"));
    }
    let missing = run(
        source,
        Op::SetKeyYaml {
            key: "absent".into(),
            yaml: "x".into(),
            guard: guard(source, "scope"),
            replace_line: false,
        },
    );
    assert_eq!(missing.unwrap_err().code, ErrorCode::MissingSelector);
    assert_eq!(
        add_yaml(source, "scope", "x").unwrap_err().code,
        ErrorCode::AlreadyExists
    );
    for key in ["", "a b", "-a", "a:b", "ключ"] {
        assert_eq!(
            add_yaml(source, key, "x").unwrap_err().code,
            ErrorCode::InvalidRequest,
            "{key:?}"
        );
    }
    assert_eq!(
        add_yaml("# T\n", "added", "x").unwrap_err().code,
        ErrorCode::UnsupportedSyntax
    );
    // A comment-only change to the key invalidates its guard, even for a
    // request that would otherwise change nothing.
    let changed = source.replace("# a", "# b");
    for (key, token) in [("scope", "'v'"), ("lenses", "[a, b]")] {
        let stale = run(
            &changed,
            Op::SetKeyYaml {
                key: key.into(),
                yaml: token.into(),
                guard: if key == "scope" {
                    guard(source, key)
                } else {
                    doc_guard(source)
                },
                replace_line: false,
            },
        );
        assert_eq!(stale.unwrap_err().code, ErrorCode::GuardConflict, "{key}");
    }
    let stale = run(
        &changed,
        Op::AddKeyYaml {
            key: "added".into(),
            yaml: "x".into(),
            guard: doc_guard(source),
        },
    );
    assert_eq!(stale.unwrap_err().code, ErrorCode::GuardConflict);
    let wrong = run(
        source,
        Op::AddKeyYaml {
            key: "added".into(),
            yaml: "x".into(),
            guard: guard(source, "scope"),
        },
    );
    assert_eq!(wrong.unwrap_err().code, ErrorCode::GuardConflict);
}

#[test]
fn yaml_legacy_writes_keep_refusing_lists() {
    let source = "---\nscope: v\nlenses: [a, b]\n---\n# T\n";
    let set = run(
        source,
        Op::Set {
            select: Selector::FrontmatterKey {
                key: "lenses".into(),
            },
            part: None,
            guard: guard(source, "lenses"),
            content: json!(["c"]),
            collection: None,
            typed_guard: None,
            allow_drop: false,
        },
    );
    assert_eq!(set.unwrap_err().code, ErrorCode::UnsupportedSyntax);
    let add = |key: &str, value: Value| {
        run(
            source,
            Op::AddKey {
                key: key.into(),
                value,
                guard: doc_guard(source),
            },
        )
    };
    assert_eq!(
        add("added", json!(["c"])).unwrap_err().code,
        ErrorCode::UnsupportedSyntax
    );
    assert_eq!(
        add("scope", json!("c")).unwrap_err().code,
        ErrorCode::AlreadyExists
    );
    // A bracket string is still quoted by the legacy scalar renderer.
    assert_eq!(
        add("added", json!("[c]")).unwrap().candidate,
        "---\nscope: v\nlenses: [a, b]\nadded: '[c]'\n---\n# T\n"
    );
}

#[test]
fn yaml_seeded_list_and_add_replays() {
    let source = stamped(YAML_DOC);
    let seeded = add_yaml(&source, "seeded", "[a, 'b c']").unwrap().candidate;
    // The original add replayed: its document guard is stale first.
    let replay = run(
        &seeded,
        Op::AddKeyYaml {
            key: "seeded".into(),
            yaml: "[a, 'b c']".into(),
            guard: doc_guard(&source),
        },
    );
    assert_eq!(replay.unwrap_err().code, ErrorCode::GuardConflict);
    // A fresh document guard reaches the existence check.
    assert_eq!(
        add_yaml(&seeded, "seeded", "[a, 'b c']").unwrap_err().code,
        ErrorCode::AlreadyExists
    );
    // The seeded list replays as a set under its fresh key guard: no change.
    for replace_line in [false, true] {
        let out = set_yaml(&seeded, "seeded", "[a, 'b c']", replace_line).unwrap();
        assert_eq!(out.candidate, seeded);
        assert_eq!(
            out.ops[0].revision.as_deref(),
            Some(guard(&seeded, "seeded").as_str())
        );
    }
    assert_eq!(value_of(&seeded, "seeded"), json!(["a", "b c"]));
}

/// Plan `op` on `source`, apply `forge` to its one splice, and return what
/// the planning gate says. The unforged change must land, as a control.
fn forged(source: &str, op: &Op, forge: impl Fn(&mut markrivet_core::edit::Splice)) -> Diagnostic {
    let doc = Document::parse(source).unwrap();
    let plan = || Planner::new(&doc).structural(op).unwrap();
    assert!(edit::plan(&doc, vec![plan()]).is_ok());
    let mut change = plan();
    assert_eq!(change.splices.len(), 1);
    forge(&mut change.splices[0]);
    edit::plan(&doc, vec![change]).unwrap_err()
}

const FORGE_SOURCE: &str = "---\nscope: 'v' # tail\nlenses: [a, b]\n---\n# T\n";

fn forge_ops(source: &str) -> [Op; 3] {
    [
        Op::SetKeyYaml {
            key: "scope".into(),
            yaml: "[x]".into(),
            guard: guard(source, "scope"),
            replace_line: false,
        },
        Op::SetKeyYaml {
            key: "scope".into(),
            yaml: "[x]".into(),
            guard: guard(source, "scope"),
            replace_line: true,
        },
        Op::AddKeyYaml {
            key: "added".into(),
            yaml: "[x]".into(),
            guard: doc_guard(source),
        },
    ]
}

/// Each forgery below re-reads as valid YAML with every decoded value as the
/// request intends, so only the intent gate's derived splice stops it.
#[test]
fn yaml_forged_extent_and_extra_bytes_are_rejected_by_the_intent_gate() {
    for nl in ["\n", "\r\n"] {
        let source = FORGE_SOURCE.replace('\n', nl);
        let tail = source.find(" # tail").unwrap();
        let line_end = tail + " # tail".len();
        let [keep, _, add] = forge_ops(&source);
        // A preserving set whose extent also takes the line's comment.
        let error = forged(&source, &keep, |s| s.span.end = line_end);
        assert_eq!(error.code, ErrorCode::InvalidRequest, "{}", error.message);
        // An add whose extent reaches back over the last field's line ending
        // and puts it back: identical bytes, wrong coordinates.
        let error = forged(&source, &add, |s| {
            s.span.start -= nl.len();
            s.text = format!("{nl}{}", &s.text[..s.text.len() - nl.len()]);
            s.text.push_str(nl);
        });
        assert_eq!(error.code, ErrorCode::InvalidRequest, "{}", error.message);
        for op in forge_ops(&source) {
            let error = forged(&source, &op, |s| s.text.push_str(" # injected"));
            assert_eq!(error.code, ErrorCode::InvalidRequest, "{}", error.message);
            assert!(error.message.contains("derives"), "{}", error.message);
        }
    }
}

#[test]
fn yaml_forged_injected_comment_line_is_rejected_for_set() {
    for nl in ["\n", "\r\n"] {
        let source = FORGE_SOURCE.replace('\n', nl);
        let [keep, clear, _] = forge_ops(&source);
        for op in [keep, clear] {
            let error = forged(&source, &op, |s| {
                s.text.push_str(&format!("{nl}# undeclared"))
            });
            assert_eq!(error.code, ErrorCode::InvalidRequest, "{}", error.message);
            assert!(error.message.contains("derives"), "{}", error.message);
        }
    }
}

#[test]
fn yaml_forged_injected_comment_line_is_rejected_for_add() {
    for nl in ["\n", "\r\n"] {
        let source = FORGE_SOURCE.replace('\n', nl);
        let [_, _, add] = forge_ops(&source);
        let error = forged(&source, &add, |s| {
            s.text.push_str(&format!("# undeclared{nl}"))
        });
        assert_eq!(error.code, ErrorCode::InvalidRequest, "{}", error.message);
        assert!(error.message.contains("derives"), "{}", error.message);
    }
}

// append_key_item

fn item(by: &str, at: &str) -> serde_json::Map<String, Value> {
    json!({"by": by, "at": at}).as_object().unwrap().clone()
}

fn append(source: &str, key: &str, item: Value, guard: String) -> Result<Outcome, Diagnostic> {
    run(
        source,
        Op::AppendKeyItem {
            key: key.into(),
            item: item.as_object().cloned().unwrap_or_default(),
            guard,
        },
    )
}

fn append_verified(source: &str, item: Value) -> Result<Outcome, Diagnostic> {
    append(source, "verified", item, guard(source, "verified"))
}

/// A synthetic tracker: the given frontmatter lines between two other keys,
/// and a stamped body, so the stays can be shown to stay attached.
fn tracker(lines: &str, nl: &str) -> String {
    stamped(
        &format!(
            "---\nentity: Synthetic # e\n{lines}\nstatus: active\n---\n\n# Widgets\n\n## Action\n\n- [ ] Synthetic action.\n"
        )
        .replace('\n', nl),
    )
}

const AT: &str = "2026-11-21T10:00:00+13:00";

#[test]
fn append_key_item_inserts_one_item_in_each_form_and_keeps_every_other_byte() {
    let new = json!({"by": "claude/b", "at": AT});
    // (existing lines, the same lines after the append)
    let cases = [
        // Flow, one item: `, ` before the new item, the comment kept after `]`.
        (
            "verified: [{by: 'claude/a', at: '2026-11-01T10:00:00+13:00'}] # c",
            "verified: [{by: 'claude/a', at: '2026-11-01T10:00:00+13:00'}, {by: 'claude/b', at: '2026-11-21T10:00:00+13:00'}] # c",
        ),
        // Flow, the last two items' separator repeated, double quotes kept.
        (
            "verified: [{by: a, at: \"1\"},{by: b, at: \"2\"}]",
            "verified: [{by: a, at: \"1\"},{by: b, at: \"2\"},{by: claude/b, at: \"2026-11-21T10:00:00+13:00\"}]",
        ),
        // Flow, an empty list: no previous style, so plain where it reads back.
        (
            "verified: []",
            "verified: [{at: 2026-11-21T10:00:00+13:00, by: claude/b}]",
        ),
        // Block entries of flow mappings, indented, before a trailing comment.
        (
            "verified:\n  - {by: 'claude/a', at: '2026-11-01T10:00:00+13:00'}\n  # trailing",
            "verified:\n  - {by: 'claude/a', at: '2026-11-01T10:00:00+13:00'}\n  - {by: 'claude/b', at: '2026-11-21T10:00:00+13:00'}\n  # trailing",
        ),
        // At the key's indentation, with wider `-` spacing and a comment
        // between entries.
        (
            "verified:\n-   {by: a, at: '1'}\n# between\n-   {by: b, at: '2'} # last",
            "verified:\n-   {by: a, at: '1'}\n# between\n-   {by: b, at: '2'} # last\n-   {by: claude/b, at: '2026-11-21T10:00:00+13:00'}",
        ),
        // Mixed entries: the last one's form is repeated, either way round.
        (
            "verified:\n  - {by: a, at: '1'}\n  - by: b\n    at: '2'",
            "verified:\n  - {by: a, at: '1'}\n  - by: b\n    at: '2'\n  - by: claude/b\n    at: '2026-11-21T10:00:00+13:00'",
        ),
        (
            "verified:\n  - by: a\n    at: '1'\n  - {by: b, at: '2'}",
            "verified:\n  - by: a\n    at: '1'\n  - {by: b, at: '2'}\n  - {by: claude/b, at: '2026-11-21T10:00:00+13:00'}",
        ),
        // Block mapping items: later fields under the first, the last item's
        // key order and quote style.
        (
            "verified:\n  - at: '2026-11-01T10:00:00+13:00' # x\n    by: \"claude/a\"",
            "verified:\n  - at: '2026-11-01T10:00:00+13:00' # x\n    by: \"claude/a\"\n  - at: '2026-11-21T10:00:00+13:00'\n    by: \"claude/b\"",
        ),
    ];
    for nl in ["\n", "\r\n"] {
        for (before, after) in cases {
            let source = tracker(before, nl);
            let want = source.replacen(&before.replace('\n', nl), &after.replace('\n', nl), 1);
            assert_ne!(want, source);
            let out =
                append_verified(&source, new.clone()).unwrap_or_else(|e| panic!("{before}: {e:?}"));
            assert_eq!(out.candidate, want, "{before}");
            same_stays(&source, &out.candidate);
            let mut expect = value_of(&source, "verified").as_array().unwrap().clone();
            expect.push(new.clone());
            assert_eq!(value_of(&out.candidate, "verified"), json!(expect));
            assert_eq!(out.ops[0].revision, Some(guard(&out.candidate, "verified")));
        }
    }
}

#[test]
fn append_key_item_quotes_values_by_set_key_field_rules() {
    // A previous quoted style is kept; plain values that would change type
    // or break the flow mapping are quoted; numbers and null stay JSON.
    let source = tracker("verified: [{by: 'a', note: x, n: 1}]", "\n");
    let out = append_verified(
        &source,
        json!({"by": "it's", "note": "yes", "n": 2, "z": "a, b: c", "e": null}),
    )
    .unwrap();
    assert!(
        out.candidate.contains(
            "verified: [{by: 'a', note: x, n: 1}, {by: 'it''s', note: 'yes', n: 2, e: null, z: 'a, b: c'}]"
        ),
        "{}",
        out.candidate
    );
    // `:` and `+` in a timestamp: plain in a block mapping item with no
    // previous style, and it reads back as the same text.
    let source = tracker("verified:\n- by: a\n  at: b", "\n");
    let out = append_verified(&source, json!({"by": "c", "at": AT})).unwrap();
    assert!(
        out.candidate.contains(&format!("- by: c\n  at: {AT}\n")),
        "{}",
        out.candidate
    );
    assert_eq!(value_of(&out.candidate, "verified")[1]["at"], json!(AT));
}

#[test]
fn append_key_item_keeps_the_previous_key_order_across_an_empty_value() {
    // A field with an empty value still holds its place in the order.
    for (lines, want) in [
        (
            "verified: [{z: , a: old}]",
            "verified: [{z: , a: old}, {z: new, a: new}]",
        ),
        (
            "verified:\n  - z:\n    a: old",
            "verified:\n  - z:\n    a: old\n  - z: new\n    a: new",
        ),
    ] {
        let source = tracker(lines, "\n");
        let out = append_verified(&source, json!({"z": "new", "a": "new"})).unwrap();
        assert_eq!(out.candidate, source.replacen(lines, want, 1), "{lines}");
    }
}

#[test]
fn append_key_item_adds_an_absent_key_under_the_document_guard() {
    for nl in ["\n", "\r\n"] {
        let source = tracker("generated:\n  by: x\n  at: y", nl);
        let out = append(
            &source,
            "verified",
            json!({"by": "claude/b", "at": AT}),
            doc_guard(&source),
        )
        .unwrap();
        let fence = format!("status: active{nl}---");
        let want = source.replacen(
            &fence,
            &format!("status: active{nl}verified: [{{at: {AT}, by: claude/b}}]{nl}---"),
            1,
        );
        assert_eq!(out.candidate, want);
        same_stays(&source, &out.candidate);
        assert_eq!(
            value_of(&out.candidate, "verified"),
            json!([{"by": "claude/b", "at": AT}])
        );
        assert_eq!(out.ops[0].revision, Some(guard(&out.candidate, "verified")));
        // The added key then takes appends of its own.
        let again = append_verified(&out.candidate, json!({"by": "c", "at": "d"})).unwrap();
        assert!(again.candidate.contains(&format!(
            "verified: [{{at: {AT}, by: claude/b}}, {{at: d, by: c}}]"
        )));
    }
}

#[test]
fn append_key_item_refuses_other_shapes_items_and_guards() {
    let good = json!({"by": "a", "at": "b"});
    // Target shapes the op does not append to.
    for lines in [
        "verified: scalar",
        "verified: 'quoted'",
        "verified: {by: a, at: b}",
        "verified:\n  by: a",
        "verified:",
        "verified: [a, b]",
        "verified:\n  - a",
        "verified: [{by: a}, b]",
        "verified: [{by: a, at: [1]}]",
        "verified:\n  - {by: a, at: {x: 1}}",
        "verified: [[{by: a}]]",
        "verified: [{by: a},\n  {by: b}]",
        "verified: [\n  {by: a}]",
        "verified:\n  - {by: a,\n     at: b}",
        "verified:\n  -\n    by: a",
        "verified:\n  - [{by: a}]",
        // An earlier entry the op would refuse as the last one.
        "verified:\n  -\n    by: a\n  - {by: b}",
        "verified:\n  -\t{by: a}\n  - {by: b}",
        "verified:\n  - {by: a,\n     at: b}\n  - {by: b}",
    ] {
        let source = tracker(lines, "\n");
        let error = append_verified(&source, good.clone()).unwrap_err();
        assert_eq!(
            error.code,
            ErrorCode::UnsupportedSyntax,
            "{lines}: {}",
            error.message
        );
    }
    // Items that are not a nonempty flat mapping of plain-keyed scalars.
    let source = tracker("verified: [{by: a, at: b}]", "\n");
    for bad in [
        json!({}),
        json!({"by": ["a"]}),
        json!({"by": {"x": 1}}),
        json!({"-by": "a"}),
        json!({"b y": "a"}),
        json!({"true": "a"}),
        json!({"null": "a"}),
        json!({"1": "a"}),
        json!({"": "a"}),
    ] {
        let error = append_verified(&source, bad.clone()).unwrap_err();
        assert_eq!(
            error.code,
            ErrorCode::InvalidRequest,
            "{bad}: {}",
            error.message
        );
    }
    // A scalar no single line holds, as set_key_field refuses it.
    for bad in [json!({"by": "a\nb"}), json!({"by": "a\u{7}"})] {
        let error = append_verified(&source, bad.clone()).unwrap_err();
        assert_eq!(
            error.code,
            ErrorCode::UnsupportedSyntax,
            "{bad}: {}",
            error.message
        );
    }
    // Guards: another key's, the document's for an existing key, a key's for
    // an absent one, and a stale one.
    let other = guard(&source, "status");
    for wrong in [other.clone(), doc_guard(&source)] {
        let error = append(&source, "verified", good.clone(), wrong).unwrap_err();
        assert_eq!(error.code, ErrorCode::GuardConflict, "{}", error.message);
    }
    let error = append(&source, "absent", good.clone(), other).unwrap_err();
    assert_eq!(error.code, ErrorCode::GuardConflict, "{}", error.message);
    let stale = guard(&source, "verified");
    let applied = append_verified(&source, good.clone()).unwrap().candidate;
    let error = append(&applied, "verified", good.clone(), stale).unwrap_err();
    assert_eq!(error.code, ErrorCode::GuardConflict, "{}", error.message);
    // An absent key under add_key's rules.
    let error = append(&source, "-bad", good.clone(), doc_guard(&source)).unwrap_err();
    assert_eq!(error.code, ErrorCode::InvalidRequest, "{}", error.message);
    let bare = "# No frontmatter\n";
    let error = append(bare, "verified", good.clone(), doc_guard(bare)).unwrap_err();
    assert_eq!(
        error.code,
        ErrorCode::UnsupportedSyntax,
        "{}",
        error.message
    );
    // Unsupported frontmatter anywhere refuses before any guard is read.
    let anchored = "---\nverified: [{by: a}]\nx: &a 1\n---\n# T\n";
    let error = append(anchored, "verified", good, doc_guard(anchored)).unwrap_err();
    assert_eq!(
        error.code,
        ErrorCode::UnsupportedSyntax,
        "{}",
        error.message
    );
}

fn append_forge_ops(source: &str) -> [Op; 3] {
    let at = |key: &str| Op::AppendKeyItem {
        key: key.into(),
        item: item("c", "d"),
        guard: if key == "added" {
            doc_guard(source)
        } else {
            guard(source, key)
        },
    };
    [at("flow"), at("block"), at("added")]
}

/// Each forgery re-reads as valid YAML holding the intended values, so only
/// the intent gate's derived splice stops it.
#[test]
fn append_key_item_forged_splices_are_rejected_by_the_intent_gate() {
    for nl in ["\n", "\r\n"] {
        let source =
            "---\nflow: [{by: a, at: b}] # tail\nblock:\n  - {by: a, at: b}\nz: 1\n---\n# T\n"
                .replace('\n', nl);
        for op in append_forge_ops(&source) {
            for forge in [
                (|s: &mut edit::Splice| s.text.push(' ')) as fn(&mut edit::Splice),
                |s| s.text.insert(0, ' '),
            ] {
                let error = forged(&source, &op, forge);
                assert_eq!(error.code, ErrorCode::InvalidRequest, "{}", error.message);
                assert!(error.message.contains("derives"), "{}", error.message);
            }
            // One byte earlier, the same text: refused either as outside the
            // derived site or as not the derived splice.
            let error = forged(&source, &op, |s| {
                s.span.start -= 1;
                s.span.end -= 1;
            });
            assert_eq!(error.code, ErrorCode::InvalidRequest, "{}", error.message);
        }
    }
}

// File-record writes: one change carrying every key edit, checked at the
// intent gate against the splices its request derives.

const RECORD_SCHEMA: &[u8] = br#"{"id": "rec", "version": 1, "item": "file", "fields": {
  "status": {"type": "string",    "from": "key", "key": "status"},
  "rank":   {"type": "number",    "from": "key", "key": "rank"},
  "tags":   {"type": "list",      "from": "key", "key": "tags"},
  "by":     {"type": "string",    "from": "key", "key": "generated", "field": "by"},
  "at":     {"type": "timestamp", "from": "key", "key": "generated", "field": "at"}}}"#;

fn record_binding() -> markrivet_core::schema::Bound {
    markrivet_core::schema::bind_bytes(
        "recs",
        markrivet_core::schema::CollectionConfig {
            schema: "rec@1".into(),
            files: vec!["**/*.md".into()],
            exclude_files: vec![],
            section: None,
            level: None,
            subsections: false,
            exclude: vec![],
            archive: vec![],
        },
        RECORD_SCHEMA,
    )
    .unwrap()
}

fn plan_record(doc: &Document, set: &Value) -> Result<edit::Change, Diagnostic> {
    let bound = record_binding();
    let guard = markrivet_core::schema::record_revision(doc, "r.md", &bound);
    let set = serde_json::from_value(set.clone()).unwrap();
    markrivet_core::schema::plan_record_update(
        &mut Planner::new(doc),
        &bound,
        "r.md",
        &guard,
        markrivet_core::GuardScope::Action,
        &set,
    )
}

const RECORD_SOURCE: &str = "---\nstatus: 'old' # note\nrank: 1\ngenerated: {by: agent, at: 2026-10-01T00:00:00Z}\ntags: [a]\n---\n# T\n\nBody.\n";

/// Plan the record write, let `forge` alter its splices, and return what the
/// planning gate says. The unforged change must land, as a control.
fn forged_record(
    source: &str,
    set: Value,
    forge: impl Fn(&str, &mut Vec<markrivet_core::edit::Splice>),
) -> Diagnostic {
    let doc = Document::parse(source).unwrap();
    assert!(edit::plan(&doc, vec![plan_record(&doc, &set).unwrap()]).is_ok());
    let mut change = plan_record(&doc, &set).unwrap();
    forge(source, &mut change.splices);
    edit::plan(&doc, vec![change]).unwrap_err()
}

fn span_of(source: &str, text: &str) -> markrivet_core::Span {
    let start = source.find(text).unwrap();
    markrivet_core::Span {
        start,
        end: start + text.len(),
    }
}

/// Each forgery re-reads with every requested value decoded as asked.
#[test]
fn a_forged_record_change_is_refused_by_the_intent_gate() {
    let set = json!({"status": "new", "at": "2026-12-01T00:00:00Z", "tags": ["a", "b"]});
    let token = |splices: &mut Vec<markrivet_core::edit::Splice>, old: &str| -> usize {
        let at = span_of(RECORD_SOURCE, old);
        splices.iter().position(|s| s.span == at).unwrap()
    };
    type Forge = Box<dyn Fn(&str, &mut Vec<markrivet_core::edit::Splice>)>;
    let forgeries: Vec<(&str, Forge)> = vec![
        // A comment changed beside the value.
        (
            "comment",
            Box::new(move |source, splices| {
                let i = token(splices, "'old'");
                splices[i].span = span_of(source, "'old' # note");
                splices[i].text = "'new' # changed".into();
            }),
        ),
        // Whitespace added after the value.
        (
            "whitespace",
            Box::new(move |_, splices| {
                let i = token(splices, "'old'");
                splices[i].text = "'new'  ".into();
            }),
        ),
        // An untouched mapping sibling requoted, decoding the same.
        (
            "sibling",
            Box::new(move |source, splices| {
                let i = token(splices, "2026-10-01T00:00:00Z");
                splices[i].span.start = span_of(source, "agent").start;
                splices[i].text = "\"agent\", at: 2026-12-01T00:00:00Z".into();
            }),
        ),
        // A body splice riding on the record change.
        (
            "body",
            Box::new(move |source, splices| {
                let at = span_of(source, "Body.").end;
                splices.push(markrivet_core::edit::Splice {
                    span: markrivet_core::Span { start: at, end: at },
                    text: " More.".into(),
                });
            }),
        ),
        // A splice extent moved: the flow list's closing bracket rewritten
        // from outside the token.
        (
            "extent",
            Box::new(move |source, splices| {
                let i = token(splices, "[a]");
                let span = span_of(source, "[a]\n");
                splices[i].span = span;
                splices[i].text = "[a, b]\n".into();
            }),
        ),
    ];
    for (name, forge) in forgeries {
        let error = forged_record(RECORD_SOURCE, set.clone(), forge);
        assert!(
            matches!(
                error.code,
                ErrorCode::InvalidRequest | ErrorCode::InvalidContent
            ),
            "{name}: {error:?}"
        );
    }
}

/// `'old'` -> `'new'` planned as the whole token, forged as `old` -> `new`
/// inside the quotes: the same bytes, inside the allowed span, so only the
/// exact comparison with the derived splices refuses it.
#[test]
fn a_byte_equivalent_record_forgery_is_refused_by_the_splice_comparison() {
    let error = forged_record(
        RECORD_SOURCE,
        json!({"status": "new"}),
        |source, splices| {
            let [one] = splices.as_mut_slice() else {
                panic!("{splices:?}")
            };
            let quoted = span_of(source, "'old'");
            assert_eq!(one.span, quoted);
            one.span = markrivet_core::Span {
                start: quoted.start + 1,
                end: quoted.end - 1,
            };
            one.text = "new".into();
        },
    );
    assert_eq!(error.code, ErrorCode::InvalidRequest, "{error:?}");
    assert!(
        error
            .message
            .contains("exactly the splices its request derives"),
        "{error:?}"
    );
}

/// The native key ops plan what they did before the renderer factoring.
#[test]
fn native_key_ops_plan_what_record_writes_share() {
    let source = "---\nstatus: 'old' # note\nrank: 1\ngenerated: {by: agent, at: 2026-10-01T00:00:00Z}\n---\n# T\n";
    let doc = Document::parse(source).unwrap();
    let record = plan_record(
        &doc,
        &json!({"status": "new", "at": "2026-12-01T00:00:00Z", "rank": null}),
    )
    .unwrap();
    let set_op = run(
        source,
        Op::Set {
            collection: None,
            typed_guard: None,
            select: Selector::FrontmatterKey {
                key: "status".into(),
            },
            guard: guard(source, "status"),
            part: None,
            content: json!("new"),
            allow_drop: false,
        },
    )
    .unwrap()
    .candidate;
    assert_eq!(set_op, source.replace("'old'", "'new'"));
    let field = set(source, "at", json!("2026-12-01T00:00:00Z"))
        .unwrap()
        .candidate;
    assert_eq!(field, source.replace("2026-10-01", "2026-12-01"));
    let removed = delete(source, "rank").unwrap().candidate;
    assert_eq!(removed, source.replace("rank: 1\n", ""));
    // The record write makes those same three edits in one change.
    let out = edit::plan(&doc, vec![record]).unwrap().candidate;
    assert_eq!(
        out,
        source
            .replace("'old'", "'new'")
            .replace("2026-10-01", "2026-12-01")
            .replace("rank: 1\n", "")
    );
}
