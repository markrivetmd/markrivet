use markrivet_core::document::{Attach, Target};
use markrivet_core::edit::{self, Op, Outcome, Planner};
use markrivet_core::{Diagnostic, Document, ErrorCode, Selector};

const SOURCE: &str = "\u{feff}---\nstatus: active\n---\n\n# Example\n<!-- stay:document hash=sha256:000000000000 -->\n\n## State\n<!-- stay:state hash=sha256:000000000000 -->\n\nUnchanged prose.\n\n| Key | Value |\n| --- | --- |\n| First | waiting |\n| Second | ready |\n<!-- stay:table hash=sha256:000000000000 -->\n\n### Child\n\nDescendant only.\n\n## Plans\n\nSibling text.\n";

fn heading(name: &str) -> Selector {
    Selector::HeadingPath {
        path: vec![name.into()],
    }
}

fn revision(source: &str, select: &Selector) -> String {
    let doc = Document::parse(source).unwrap();
    doc.target_revision(doc.resolve(select).unwrap())
}

fn run(
    source: &str,
    select: Selector,
    guard: String,
    old: &str,
    new: &str,
) -> Result<Outcome, Diagnostic> {
    let doc = Document::parse(source).unwrap();
    let change = Planner::new(&doc).structural(&Op::ReplaceInSection {
        select,
        guard,
        old: old.into(),
        new: new.into(),
        top_level: false,
    })?;
    edit::plan(&doc, vec![change])
}

fn top_level(source: &str, old: &str, new: &str) -> Result<Outcome, Diagnostic> {
    let doc = Document::parse(source).unwrap();
    let select = heading("State");
    let change = Planner::new(&doc).structural(&Op::ReplaceInSection {
        guard: revision(source, &select),
        select,
        old: old.into(),
        new: new.into(),
        top_level: true,
    })?;
    edit::plan(&doc, vec![change])
}

#[test]
fn top_level_refuses_every_nested_match_the_default_accepts() {
    for (body, old) in [
        ("- item waiting", "waiting"),
        ("- [ ] item waiting", "waiting"),
        ("1. item waiting", "waiting"),
        ("> quoted waiting", "waiting"),
        ("> - quoted item waiting", "waiting"),
        ("- first\nlazy waiting", "waiting"),
        // Table-shaped lines are paragraph text to the write parser, so the lazy
        // line stays in the item although `inspect` ends the item before it.
        ("- [ ] first\n  | h | v |\n  |---|---|\nwaiting", "waiting"),
        ("> first\n> | h | v |\n> |---|---|\nwaiting", "waiting"),
    ] {
        let source = format!("# Doc\n\n## State\n\n{body}\n");
        replace(&source, old, "landed").unwrap_or_else(|e| panic!("{body:?}: {e:?}"));
        let refused = top_level(&source, old, "landed").expect_err(body);
        assert_eq!(refused.code, ErrorCode::UnsupportedSyntax, "{body:?}");
        assert!(
            refused.message.contains("top_level"),
            "{body:?}: {refused:?}"
        );
    }
}

#[test]
fn the_refusal_messages_without_the_flag_are_unchanged() {
    let want =
        "the match must lie wholly inside one paragraph; removal requires a top-level paragraph";
    let code = "# Doc\n\n## State\n\n```\nwaiting\n```\n";
    let refused = replace(code, "waiting", "x").expect_err("code block");
    assert_eq!(refused.message, want);
    let nested = "# Doc\n\n## State\n\n- waiting\n";
    let doc = Document::parse(nested).unwrap();
    let select = heading("State");
    let refused = Planner::new(&doc)
        .structural(&Op::RemoveParagraphInSection {
            guard: revision(nested, &select),
            select,
            old: "waiting".into(),
        })
        .err()
        .expect("nested removal");
    assert_eq!(refused.message, want);
}

#[test]
fn top_level_leaves_top_level_prose_and_every_earlier_answer() {
    for (body, old, new) in [
        ("Prose waiting.", "waiting", "landed"),
        (
            "| a | b |\n| --- | --- |\n| one | waiting |",
            "waiting",
            "landed",
        ),
        // Not a list to the write parser: an ordered list starting at 2 cannot
        // interrupt the paragraph the table-shaped lines belong to.
        ("| h | v |\n|---|---|\n2. waiting", "waiting", "landed"),
    ] {
        let source = format!("# Doc\n\n## State\n\n{body}\n");
        let plain = replace(&source, old, new).unwrap();
        let flagged = top_level(&source, old, new).unwrap_or_else(|e| panic!("{body:?}: {e:?}"));
        assert_eq!(flagged.candidate, plain.candidate, "{body:?}");
    }
    let source = "# Doc\n\n## State\n\nwaiting waiting\n\n- item\n";
    for (old, code) in [
        ("absent", ErrorCode::MatchNotFound),
        ("waiting", ErrorCode::AmbiguousMatch),
    ] {
        assert_eq!(top_level(source, old, "x").expect_err(old).code, code);
    }
    let stale = Op::ReplaceInSection {
        select: heading("State"),
        guard: "stale".into(),
        old: "item".into(),
        new: "x".into(),
        top_level: true,
    };
    let doc = Document::parse(source).unwrap();
    let refused = Planner::new(&doc).structural(&stale).err().expect("stale");
    assert_eq!(refused.code, ErrorCode::GuardConflict);
}

fn replace(source: &str, old: &str, new: &str) -> Result<Outcome, Diagnostic> {
    let select = heading("State");
    run(source, select.clone(), revision(source, &select), old, new)
}

#[test]
fn table_splice_changes_only_the_match_and_its_owned_hash() {
    for source in [SOURCE.to_string(), SOURCE.replace('\n', "\r\n")] {
        let out = replace(&source, "waiting", "finished ✓").unwrap();
        let before = Document::parse(&source).unwrap();
        let after = Document::parse(&out.candidate).unwrap();
        // Revert precisely the edited source span and the affected hash bytes.
        let mut restored = out.candidate.replace("finished ✓", "waiting");
        assert_eq!(out.evidence.len(), 1);
        assert_eq!(out.evidence[0].id, "table");
        restored = restored.replace(&out.evidence[0].refreshed, &out.evidence[0].stored);
        assert_eq!(restored, source);
        assert_eq!(out.ops[0].id.as_deref(), Some("state"));
        assert_eq!(
            out.ops[0].revision,
            Some(revision(&out.candidate, &heading("State")))
        );
        for marker in &before.markers {
            let id = &marker.lexical.id;
            let next = after.markers.iter().find(|m| &m.lexical.id == id).unwrap();
            assert_eq!(marker.attach, next.attach);
            if id.as_deref() != Some("table") {
                assert_eq!(
                    &source[marker.lexical.span.start..marker.lexical.span.end],
                    &out.candidate[next.lexical.span.start..next.lexical.span.end]
                );
            }
        }
        assert!(
            after
                .markers
                .iter()
                .all(|m| matches!(m.attach, Attach::Block(_)))
        );
        let no_op = replace(&source, "waiting", "waiting").unwrap();
        assert_eq!(
            no_op.candidate, source,
            "even pre-existing stale hashes stay intact"
        );
        assert!(no_op.evidence.is_empty());
    }
}

#[test]
fn exact_raw_occurrences_include_overlaps_code_and_markers() {
    for (body, old) in [
        ("aaa", "aa"),
        ("ééé", "éé"),
        ("same same", "same"),
        ("same\n\n```\nsame\n```", "same"),
        ("same <!-- stay:same -->", "same"),
    ] {
        let source = format!("# State\n\n{body}\n");
        assert_eq!(
            replace(&source, old, "new").unwrap_err().code,
            ErrorCode::AmbiguousMatch,
            "{body}"
        );
    }
    assert_eq!(
        replace(SOURCE, "Descendant only.", "new").unwrap_err().code,
        ErrorCode::MatchNotFound
    );
    assert_eq!(
        replace(SOURCE, "Child", "new").unwrap_err().code,
        ErrorCode::MatchNotFound
    );
    assert_eq!(
        replace(SOURCE, "State", "new").unwrap_err().code,
        ErrorCode::MatchNotFound
    );
    assert_eq!(
        replace(SOURCE, "missing", "new").unwrap_err().code,
        ErrorCode::MatchNotFound
    );
}

#[test]
fn invalid_inputs_and_nonparagraph_targets_are_explicitly_refused() {
    for (old, new) in [
        ("", "new"),
        ("waiting\n", "new"),
        ("waiting", "new\nline"),
        ("waiting\r", "new"),
        ("waiting", "new\rline"),
    ] {
        assert_eq!(
            replace(SOURCE, old, new).unwrap_err().code,
            ErrorCode::InvalidRequest
        );
    }
    for body in [
        "- list prefix",
        "```\ncode\n```",
        "    indented code",
        "> quoted",
        "<!-- other html -->",
    ] {
        let source = format!("# State\n\n{body}\n");
        assert_eq!(
            replace(
                &source,
                if body.starts_with("```") {
                    "code"
                } else {
                    body.lines().next().unwrap()
                },
                "new"
            )
            .unwrap_err()
            .code,
            ErrorCode::UnsupportedSyntax,
            "{body}"
        );
    }
    let select = Selector::Document;
    let error = run(
        SOURCE,
        select.clone(),
        revision(SOURCE, &select),
        "waiting",
        "new",
    )
    .unwrap_err();
    assert_eq!(error.code, ErrorCode::InvalidRequest);
    let select = Selector::Id { id: "table".into() };
    assert_eq!(
        run(
            SOURCE,
            select.clone(),
            revision(SOURCE, &select),
            "waiting",
            "new"
        )
        .unwrap_err()
        .code,
        ErrorCode::InvalidRequest
    );
}

#[test]
fn markers_cannot_be_edited_introduced_or_hidden() {
    for old in ["<!-- stay:table", "table hash=", "sha256:000000000000 -->"] {
        let error = replace(SOURCE, old, "new").unwrap_err();
        // The hash also appears on headings, which are outside the own body.
        assert_eq!(error.code, ErrorCode::InvalidContent, "{old}: {error:?}");
    }
    for body in ["text <!-- stay:owned -->", "text\n<!-- stay:owned -->"] {
        let source = format!("# State\n\n{body}\n");
        assert_eq!(
            replace(&source, "owned", "x").unwrap_err().code,
            ErrorCode::InvalidContent
        );
    }
    for (body, old, new) in [
        ("text", "text", "text <!-- stay:new -->"),
        ("text <!-- stay:new -x>", "-x>", "-->"),
        ("text `<!-- stay:quoted -->`", "`<!--", "<!--"),
        ("text <!-- stay:owned -->`", "text", "`text"),
        (
            "text `quote` <!-- stay:owned --> `<!-- stay:owned -->`",
            "`quote`",
            "`quote",
        ),
        ("text `quote` `<!-- stay:exposed -->`", "`quote`", "`quote"),
    ] {
        let source = format!("# State\n\n{body}\n");
        assert!(
            replace(&source, old, new).is_err(),
            "{body}: {old} -> {new}"
        );
    }
}

#[test]
fn reference_edits_preserve_live_marker_positions_in_untouched_units() {
    for marker in ["<!-- stay:owned -->", "<!-- stay:row subhash=sha256:1 -->"] {
        for location in ["", "### Child\n\n", "## Plans\n\n", "- "] {
            let source = format!(
                "# State\n\n[ref]: /url\nprose\n\n{location}[foo][ref]({marker}) [bar][other]({marker})\n"
            );
            let doc = Document::parse(&source).unwrap();
            assert_eq!(doc.markers.len(), 1);
            assert_eq!(
                replace(&source, "[ref]:", "[other]:").unwrap_err().code,
                ErrorCode::InvalidContent,
                "{source}"
            );
        }
    }

    // A different batch operation can edit one item of the affected list.
    // The untouched item still needs its original live marker, not its copy.
    let source = "# State\n\n[ref]: /url\nprose\n\n## Actions\n\n- [ ] [foo][ref](<!-- stay:first subhash=sha256:000000000000 -->) [bar][other](<!-- stay:first subhash=sha256:000000000000 -->)\n- [ ] two <!-- stay:second subhash=sha256:000000000000 -->\n";
    let doc = Document::parse(source).unwrap();
    let mut planner = Planner::new(&doc);
    let change = planner
        .structural(&Op::ReplaceInSection {
            select: heading("State"),
            guard: revision(source, &heading("State")),
            old: "[ref]:".into(),
            new: "[other]:".into(),
            top_level: false,
        })
        .unwrap();
    let tick = planner.set_checkbox(0, 1, true).unwrap();
    assert_eq!(
        edit::plan(&doc, vec![change, tick]).unwrap_err().code,
        ErrorCode::InvalidContent
    );

    // The same swap with the copy first and already holding the post-swap
    // digest: refresh changes no bytes and the copy's carrier is plain, so
    // only the untouched-item position check stands between it and a write.
    let source = "# State\n\n[ref]: /url\nprose\n\n## Actions\n\n- [ ] [foo][other](<!-- stay:first subhash=sha256:dc58fe2d794a -->) [bar][ref](<!-- stay:first subhash=sha256:000000000000 -->)\n- [ ] two <!-- stay:second subhash=sha256:000000000000 -->\n";
    let doc = Document::parse(source).unwrap();
    let mut planner = Planner::new(&doc);
    let change = planner
        .structural(&Op::ReplaceInSection {
            select: heading("State"),
            guard: revision(source, &heading("State")),
            old: "[ref]:".into(),
            new: "[other]:".into(),
            top_level: false,
        })
        .unwrap();
    let tick = planner.set_checkbox(0, 1, true).unwrap();
    let error = edit::plan(&doc, vec![change, tick]).unwrap_err();
    assert_eq!(error.code, ErrorCode::InvalidContent);
    assert!(error.message.contains("would change"), "{}", error.message);

    // Ticking the affected item itself edits it in place, which must not
    // let its exposed copy stand in for the marker the edit hid. The tick's
    // own check refuses it: item content omits live markers, so a different
    // marker going live changes the compared text.
    let source = "# State\n\n[ref]: /url\nprose\n\n## Actions\n\n- [ ] [foo][other](<!-- stay:first subhash=sha256:000000000000 -->) [bar][ref](<!-- stay:first subhash=sha256:000000000000 -->)\n- [ ] two <!-- stay:second subhash=sha256:000000000000 -->\n";
    let doc = Document::parse(source).unwrap();
    let mut planner = Planner::new(&doc);
    let change = planner
        .structural(&Op::ReplaceInSection {
            select: heading("State"),
            guard: revision(source, &heading("State")),
            old: "[ref]:".into(),
            new: "[other]:".into(),
            top_level: false,
        })
        .unwrap();
    let tick = planner.set_checkbox(0, 0, true).unwrap();
    assert_eq!(
        edit::plan(&doc, vec![change, tick]).unwrap_err().code,
        ErrorCode::InvalidContent
    );

    // Independent edits remain batchable, including a shifted untouched stay.
    let source = "# State\n\ntext\n\n## Actions\n\n- [ ] one <!-- stay:first subhash=sha256:000000000000 -->\n- [ ] two <!-- stay:second subhash=sha256:000000000000 -->\n";
    let doc = Document::parse(source).unwrap();
    let mut planner = Planner::new(&doc);
    let change = planner
        .structural(&Op::ReplaceInSection {
            select: heading("State"),
            guard: revision(source, &heading("State")),
            old: "text".into(),
            new: "longer text".into(),
            top_level: false,
        })
        .unwrap();
    let tick = planner.set_checkbox(0, 1, true).unwrap();
    let out = edit::plan(&doc, vec![change, tick]).unwrap();
    assert!(out.candidate.contains("longer text"));
    assert!(out.candidate.contains("- [x] two"));
    assert_eq!(Document::parse(&out.candidate).unwrap().markers.len(), 2);
}

#[test]
fn foreign_row_stay_uses_the_pinned_reference_vector() {
    let corpus: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/markstay/reference-children.json")).unwrap();
    let vector = corpus["vectors"]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["name"] == "table-rows-declined")
        .unwrap();
    let source = format!("# State\n\n{}", vector["doc"].as_str().unwrap());
    let doc = Document::parse(&source).unwrap();
    assert!(
        doc.markers
            .iter()
            .any(|m| m.lexical.id.as_deref() == Some("r1") && m.attach == Attach::Unaddressed)
    );
    let error = replace(&source, "| a | b", "| a | c").unwrap_err();
    assert_eq!(error.code, ErrorCode::UnsupportedSyntax);
    assert!(error.message.contains("does not own"));
    let source = "# State\n\ntext\n<!-- stay:foreign subhash=sha256:1 -->\n";
    assert_eq!(
        replace(source, "text", "new").unwrap_err().code,
        ErrorCode::UnsupportedSyntax
    );
}

#[test]
fn replacements_and_noops_use_the_surrounding_code_span_context() {
    let source = "# State\n\nA `kind` value.\n<!-- stay:paragraph -->\n";
    let out = replace(source, "kind", "<!-- stay:quoted -->").unwrap();
    assert_eq!(
        out.candidate,
        source.replace("kind", "<!-- stay:quoted -->")
    );
    assert_eq!(
        replace(
            &out.candidate,
            "<!-- stay:quoted -->",
            "<!-- stay:quoted -->"
        )
        .unwrap()
        .candidate,
        out.candidate
    );
    assert_eq!(
        replace(&out.candidate, "<!-- stay:quoted -->", "kind")
            .unwrap()
            .candidate,
        source
    );
}

#[test]
fn candidate_must_keep_one_paragraph_its_kind_and_attachment() {
    for (body, old, new) in [
        ("first\nmiddle\nlast", "middle", ""),
        ("text", "text", "# New"),
        ("text", "text", "- New"),
        ("text", "text", "---"),
        ("text", "text", ""),
        ("text\n<!-- stay:owned -->", "text", "<!-- comment -->"),
        ("text\n<!-- stay:owned -->", "text", "```"),
    ] {
        let source = format!("# State\n\n{body}\n");
        assert_eq!(
            replace(&source, old, new).unwrap_err().code,
            ErrorCode::InvalidContent,
            "{body}: {new}"
        );
    }
    let source = "# State\n\nkeep remove text\n";
    assert_eq!(
        replace(source, "remove ", "").unwrap().candidate,
        "# State\n\nkeep text\n"
    );
    let out = replace("# State\n\ntext\n", "text", "-literal").unwrap();
    assert_eq!(
        out.ops[0].revision,
        Some(revision(&out.candidate, &heading("State")))
    );
    assert!(
        Document::parse(&out.candidate)
            .unwrap()
            .resolve(&heading("State"))
            .is_ok_and(|t| matches!(t, Target::Section(_)))
    );
}

#[test]
fn guard_conflict_precedes_search_even_for_noop_missing_or_invalid_old() {
    let select = heading("State");
    let guard = revision(SOURCE, &select);
    let changed = SOURCE.replace("Descendant only.", "Changed child.");
    for (old, new) in [("waiting", "new"), ("missing", "missing"), ("", "new")] {
        let error = run(&changed, select.clone(), guard.clone(), old, new).unwrap_err();
        assert_eq!(error.code, ErrorCode::GuardConflict);
        assert_eq!(error.current, Some(revision(&changed, &select)));
    }
}

#[test]
fn nested_splices_preserve_every_other_byte_and_marker_owner() {
    for body in [
        "- waiting\n- other",
        "- [ ] waiting <!-- stay:item subhash=sha256:000000000000 -->\n- other",
        "- first\n\n  waiting\n\n- other\n<!-- stay:list hash=sha256:000000000000 -->",
        "- parent\n  - waiting\n  - other",
        "> waiting\n> continuation\n<!-- stay:quote hash=sha256:000000000000 -->",
        "> - waiting\n> - other",
        "- > waiting\n  > continuation",
        "- first\n  waiting",
        "- `waiting`",
        "- | Key | Value |\n  | --- | --- |\n  | One | waiting |",
    ] {
        for nl in ["\n", "\r\n"] {
            let source = format!("\u{feff}---\nstatus: active\n---\n\n# State\n<!-- stay:state -->\n\n{body}\n\nkept paragraph\n<!-- stay:kept -->\n\n## Child\n\nchild\n").replace('\n', nl);
            let out = replace(&source, "waiting", "finished ✓").unwrap();
            let mut restored = out.candidate.replace("finished ✓", "waiting");
            for evidence in &out.evidence {
                restored = restored.replace(&evidence.refreshed, &evidence.stored);
            }
            assert_eq!(restored, source, "{body}");
            let before = Document::parse(&source).unwrap();
            let after = Document::parse(&out.candidate).unwrap();
            for marker in &before.markers {
                let id = marker.lexical.id.as_ref().unwrap();
                assert_eq!(
                    before.resolve(&Selector::Id { id: id.clone() }).unwrap(),
                    after.resolve(&Selector::Id { id: id.clone() }).unwrap()
                );
            }
            assert_eq!(
                replace(&source, "waiting", "waiting").unwrap().candidate,
                source
            );
        }
    }
}

#[test]
fn inline_edits_can_change_leading_whitespace_without_changing_block_shape() {
    for body in ["waiting", "- waiting", "> waiting", "- first\n\n  waiting"] {
        let source = format!("# State\n\n{body}\n");
        let out = replace(&source, "waiting", " waiting").unwrap();
        assert_eq!(out.candidate, source.replace("waiting", " waiting"));
    }
}

#[test]
fn continuation_container_prefixes_are_not_paragraph_content_even_for_noops() {
    for (body, old) in [
        ("> first\n> continuation", "> continuation"),
        ("> > first\n> > continuation", "> continuation"),
        ("- first\n  continuation", "  continuation"),
        ("1. first\n   continuation", "   "),
        ("- > first\n  > continuation", "> continuation"),
        ("> - first\n>   continuation", ">   "),
        ("> `first\n> second` end", "> second"),
        ("> [first\n> ](/url) end", "> ]"),
        ("- [ ] first\n  continuation", "  continuation"),
        ("- first\n\tcontinuation", "\t"),
        ("> first\n>\tcontinuation", ">\t"),
    ] {
        for new in ["changed", old] {
            let source = format!("# State\n\n{body}\n");
            assert_eq!(
                replace(&source, old, new).unwrap_err().code,
                ErrorCode::UnsupportedSyntax,
                "{body:?}: {old:?} -> {new:?}"
            );
        }
    }
}

#[test]
fn nested_continuation_content_keeps_lazy_lines_inline_syntax_and_tabs() {
    for body in [
        "> first\n> continuation",
        "> first\ncontinuation",
        "> > first\n> continuation",
        "- first\n  continuation",
        "- first\ncontinuation",
        "- > first\n  > continuation",
        "> - first\n>   continuation",
        "> `first\n> continuation` end",
        "> [first\n> ](/continuation) end",
        "- [ ] first\n  continuation",
        "- first\n\tcontinuation",
        ">\tfirst\n>\tcontinuation",
        "  - first\n    continuation",
        "10. first\n    continuation",
        "> 10. first\n>     continuation",
        ">\t- first\n>\t  continuation",
        "-\tfirst\n\tcontinuation",
        "-\t> first\n\t> continuation",
        "- \t> first\n\t> continuation",
        "1. > - first\n   >   continuation",
    ] {
        let source = format!("# State\n\n{body}\n");
        assert_eq!(
            replace(&source, "continuation", "landed")
                .unwrap()
                .candidate,
            source.replace("continuation", "landed"),
            "{body:?}"
        );
    }
}

#[test]
fn nested_shape_changes_foreign_markers_and_nonunique_matches_refuse() {
    for (body, old, new, want) in [
        (
            "- first\n  waiting",
            "waiting",
            "- nested",
            ErrorCode::InvalidContent,
        ),
        (
            "> waiting",
            "waiting",
            "# heading",
            ErrorCode::InvalidContent,
        ),
        ("> waiting", "waiting", "", ErrorCode::InvalidContent),
        ("- waiting", "waiting", "---", ErrorCode::InvalidContent),
        (
            "- first\n\n  waiting\n\n- other",
            "waiting",
            "```",
            ErrorCode::InvalidContent,
        ),
        (
            "- waiting",
            "waiting",
            "<!-- stay:injected -->",
            ErrorCode::InvalidContent,
        ),
        (
            "> waiting <!-- stay:row subhash=sha256:1 -->",
            "waiting",
            "new",
            ErrorCode::UnsupportedSyntax,
        ),
        (
            "- parent\n  - waiting <!-- stay:nested subhash=sha256:1 -->",
            "waiting",
            "new",
            ErrorCode::UnsupportedSyntax,
        ),
        (
            "- waiting\n- waiting",
            "waiting",
            "new",
            ErrorCode::AmbiguousMatch,
        ),
        (
            "- waiting\n\n```\nwaiting\n```",
            "waiting",
            "new",
            ErrorCode::AmbiguousMatch,
        ),
        (
            "- ```\n  waiting\n  ```",
            "waiting",
            "new",
            ErrorCode::UnsupportedSyntax,
        ),
        (
            "- first\n  - second",
            "  - ",
            "  ",
            ErrorCode::UnsupportedSyntax,
        ),
        (
            "- first\n  continuation\n  last",
            "continuation",
            "",
            ErrorCode::InvalidContent,
        ),
        (
            "- `quote` <!-- stay:item subhash=sha256:1 --> `<!-- stay:item subhash=sha256:1 -->`",
            "`quote`",
            "`quote",
            ErrorCode::InvalidContent,
        ),
    ] {
        let source = format!("# State\n\n{body}\n\nkept\n<!-- stay:kept -->\n");
        assert_eq!(
            replace(&source, old, new).unwrap_err().code,
            want,
            "{body}: {old} -> {new}"
        );
    }
}

fn remove_paragraph(source: &str, old: &str) -> Result<Outcome, Diagnostic> {
    let doc = Document::parse(source).unwrap();
    let select = heading("State");
    let change = Planner::new(&doc).structural(&Op::RemoveParagraphInSection {
        guard: doc.target_revision(doc.resolve(&select).unwrap()),
        select,
        old: old.into(),
    })?;
    edit::plan(&doc, vec![change])
}

#[test]
fn whole_paragraph_removal_preserves_surrounding_bytes_and_stays() {
    for paragraph in [
        "Nothing queued.",
        "Last scanned: before\ncontinued residue",
        "  indented prose",
        "last without newline",
    ] {
        for nl in ["\n", "\r\n"] {
            let source =
                format!("# State\n<!-- stay:state -->\n\nkept\n<!-- stay:kept -->\n\n{paragraph}");
            let source = source.replace('\n', nl);
            let old = paragraph.replace('\n', nl);
            let out = remove_paragraph(&source, &old).unwrap();
            assert_eq!(out.candidate, source.replacen(&old, "", 1));
            let before = Document::parse(&source).unwrap();
            let after = Document::parse(&out.candidate).unwrap();
            for id in ["state", "kept"] {
                let select = Selector::Id { id: id.into() };
                assert_eq!(
                    before.resolve(&select).unwrap(),
                    after.resolve(&select).unwrap()
                );
            }
            assert_eq!(
                out.ops[0].revision,
                Some(revision(&out.candidate, &heading("State")))
            );
            assert!(out.evidence.is_empty());
        }
    }
    let source = "# State\n\nremove me\n\n## Child\n\nkept\n";
    assert_eq!(
        remove_paragraph(source, "remove me").unwrap().candidate,
        "# State\n\n\n\n## Child\n\nkept\n"
    );
}

#[test]
fn removal_refuses_partial_nested_ambiguous_and_attached_paragraphs() {
    for (body, old, want) in [
        ("text", "", ErrorCode::InvalidRequest),
        ("text", "missing", ErrorCode::MatchNotFound),
        ("text\n\ntext", "text", ErrorCode::AmbiguousMatch),
        ("whole text", "text", ErrorCode::InvalidContent),
        ("whole\ntext", "whole", ErrorCode::InvalidContent),
        (
            "text\n<!-- stay:owned -->",
            "text",
            ErrorCode::InvalidContent,
        ),
        (
            "previous\n\ntext\n<!-- stay:owned -->",
            "text",
            ErrorCode::InvalidContent,
        ),
        (
            "text <!-- stay:owned -->",
            "text <!-- stay:owned -->",
            ErrorCode::InvalidContent,
        ),
        (
            "text\n<!-- stay:row subhash=sha256:1 -->",
            "text",
            ErrorCode::UnsupportedSyntax,
        ),
        ("- text", "text", ErrorCode::UnsupportedSyntax),
        ("> text", "text", ErrorCode::UnsupportedSyntax),
        ("```\ntext\n```", "text", ErrorCode::UnsupportedSyntax),
    ] {
        let source = format!("# State\n\n{body}\n");
        assert_eq!(
            remove_paragraph(&source, old).unwrap_err().code,
            want,
            "{body}"
        );
    }
    assert_eq!(
        replace("# State\n\ntext\n", "text", "").unwrap_err().code,
        ErrorCode::InvalidContent
    );
}

#[test]
fn nested_attachment_and_digests_follow_pinned_markstay_vectors() {
    for (corpus, name, old, new) in [
        (
            include_str!("fixtures/markstay/reference-children.json"),
            "tight-list-with-nested-child",
            "one",
            "first",
        ),
        (
            include_str!("fixtures/markstay/reference-children.json"),
            "marker-mid-item-and-second-paragraph",
            "second paragraph",
            "later paragraph",
        ),
        (
            include_str!("fixtures/markstay/tree-parse52.json"),
            "blockquote-internal-blank-agrees",
            "line 2",
            "line two",
        ),
    ] {
        let corpus: serde_json::Value = serde_json::from_str(corpus).unwrap();
        let vector = corpus["vectors"]
            .as_array()
            .unwrap()
            .iter()
            .find(|v| v["name"] == name)
            .unwrap();
        let source = format!("# State\n\n{}", vector["doc"].as_str().unwrap());
        let before = Document::parse(&source).unwrap();
        let blocks = vector
            .get("blocks")
            .or_else(|| vector.get("treeBlocks"))
            .unwrap();
        for block in blocks.as_array().unwrap() {
            for id in block["ids"].as_array().unwrap() {
                let target = before
                    .resolve(&Selector::Id {
                        id: id.as_str().unwrap().into(),
                    })
                    .unwrap();
                let Target::Block(b) = target else {
                    panic!("reference block stay: {target:?}")
                };
                if let Some(hash) = block["hash"].as_str() {
                    assert_eq!(before.block_hash(b), hash, "{name}");
                }
                if let Some(children) = block["children"].as_array() {
                    for (item, child) in children.iter().enumerate() {
                        for id in child["ids"].as_array().unwrap() {
                            let select = Selector::Id {
                                id: id.as_str().unwrap().into(),
                            };
                            assert_eq!(
                                before.resolve(&select).unwrap(),
                                Target::Item {
                                    list: before.blocks[b].list.unwrap(),
                                    item
                                }
                            );
                        }
                    }
                }
            }
        }
        let out = replace(&source, old, new).unwrap();
        let after = Document::parse(&out.candidate).unwrap();
        for marker in &before.markers {
            let next = after
                .markers
                .iter()
                .find(|m| m.lexical.id == marker.lexical.id)
                .unwrap();
            assert_eq!(next.attach, marker.attach, "{name}");
        }
        let mut restored = out.candidate.replacen(new, old, 1);
        for evidence in &out.evidence {
            restored = restored.replace(&evidence.refreshed, &evidence.stored);
        }
        assert_eq!(restored, source, "{name}");
    }
}
