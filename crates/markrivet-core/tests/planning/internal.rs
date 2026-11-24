//! Corrupt the builder's output to test the planning gate independently.
use super::*;
use serde_json::json;

const SOURCE: &str = "# A\n\n- [ ] first <!-- stay:first subhash=sha256:000000000000 -->\n- [ ] second <!-- stay:second subhash=sha256:000000000000 -->\n\n# B\n\n- [ ] third <!-- stay:third subhash=sha256:000000000000 -->\n- [ ] fourth <!-- stay:fourth subhash=sha256:000000000000 -->\n";

fn bound(section: &str) -> schema::Bound {
    schema::bind_bytes("tasks", schema::CollectionConfig {
        schema: "tasks@1".into(), files: vec!["**/*.md".into()], section: section.into(),
        level: Some(1), subsections: false, exclude: vec![], archive: vec![],
    }, br#"{"id":"tasks","version":1,"item":"checkbox","fields":{"done":{"type":"boolean","from":"checkbox"},"text":{"type":"string","from":"text"}}}"#).unwrap()
}

fn tick(doc: &Document) -> Change {
    Planner::new(doc).set_checkbox(0, 0, true).unwrap()
}

fn rewrite(doc: &Document) -> Change {
    let select = Selector::Id { id: "first".into() };
    Planner::new(doc)
        .structural(&Op::Set {
            collection: None,
            typed_guard: None,
            guard: doc.target_revision(doc.resolve(&select).unwrap()),
            select,
            part: None,
            content: json!("[x] changed"),
            allow_drop: false,
        })
        .unwrap()
}

fn typed_update(doc: &Document, bound: &schema::Bound) -> Change {
    schema::plan_update(
        &mut Planner::new(doc),
        bound,
        &schema::ItemLocator::Id("first".into()),
        &schema::item_revision(doc, bound, 0, 0),
        GuardScope::Action,
        &BTreeMap::from([("done".into(), json!(true))]),
    )
    .unwrap()
}

fn typed_insert(doc: &Document, bound: &schema::Bound) -> Change {
    let inst = schema::instance(doc, bound).unwrap().unwrap();
    schema::plan_insert(
        &mut Planner::new(doc),
        bound,
        Position::End,
        None,
        &schema::collection_revision(doc, bound, &inst),
        &BTreeMap::from([
            ("done".into(), json!(false)),
            ("text".into(), json!("new item")),
        ]),
    )
    .unwrap()
}

#[test]
fn reshaping_a_list_excludes_other_changes_to_it() {
    let doc = Document::parse(SOURCE).unwrap();
    let binding = bound("A");
    let delete = || {
        let select = Selector::Id {
            id: "second".into(),
        };
        Planner::new(&doc)
            .structural(&Op::Delete {
                list: None,
                guard: doc.target_revision(doc.resolve(&select).unwrap()),
                select,
                collection: None,
            })
            .unwrap()
    };
    for reshape in ["typed insert", "delete"] {
        for other in [
            "checkbox",
            "rewrite",
            "typed update",
            "typed insert",
            "delete",
        ] {
            for reverse in [false, true] {
                let x = if reshape == "delete" {
                    delete()
                } else {
                    typed_insert(&doc, &binding)
                };
                let y = match other {
                    "checkbox" => tick(&doc),
                    "rewrite" => rewrite(&doc),
                    "typed update" => typed_update(&doc, &binding),
                    "typed insert" => typed_insert(&doc, &binding),
                    "delete" => delete(),
                    _ => unreachable!(),
                };
                let changes = if reverse { vec![y, x] } else { vec![x, y] };
                let error = plan(&doc, changes).unwrap_err();
                assert_eq!(
                    error.code,
                    ErrorCode::InvalidRequest,
                    "{reshape}, {other}, {reverse}"
                );
            }
        }
    }
}

#[test]
fn disjoint_item_edits_and_reshapes_of_different_lists_combine() {
    let doc = Document::parse(SOURCE).unwrap();
    let mut planner = Planner::new(&doc);
    let out = plan(
        &doc,
        vec![
            planner.set_checkbox(0, 0, true).unwrap(),
            planner.set_checkbox(0, 1, true).unwrap(),
        ],
    )
    .unwrap();
    let expected =
        SOURCE
            .replacen("[ ] first", "[x] first", 1)
            .replacen("[ ] second", "[x] second", 1);
    // Only the two checkbox bytes and their stored evidence may change.
    let undigest = |s: &str| {
        s.lines()
            .map(|line| line.split(" subhash=").next().unwrap())
            .collect::<Vec<_>>()
            .join("\n")
    };
    assert_eq!(undigest(&out.candidate), undigest(&expected));

    for reverse in [false, true] {
        let insert = typed_insert(&doc, &bound("A"));
        let tick = Planner::new(&doc).set_checkbox(1, 0, true).unwrap();
        let changes = if reverse {
            vec![tick, insert]
        } else {
            vec![insert, tick]
        };
        let out = plan(&doc, changes).unwrap();
        let after = Document::parse(&out.candidate).unwrap();
        assert_eq!(after.lists[0].items.len(), 3);
        assert!(after.lists[1].items[0].checkbox.unwrap().checked);
        for id in ["first", "second", "third", "fourth"] {
            let select = Selector::Id { id: id.into() };
            let Target::Item { list, item } = doc.resolve(&select).unwrap() else {
                panic!("item")
            };
            let Target::Item {
                list: new_list,
                item: new_item,
            } = after.resolve(&select).unwrap()
            else {
                panic!("item")
            };
            assert_eq!((list, item), (new_list, new_item), "{id}");
            if id != "third" {
                let before_span = doc.lists[list].items[item].span;
                let after_span = after.lists[new_list].items[new_item].span;
                assert_eq!(
                    &SOURCE[before_span.start..before_span.end],
                    &out.candidate[after_span.start..after_span.end]
                );
            }
        }
    }
}

#[test]
fn planning_gate_requires_guard_and_expectation_even_for_noops() {
    let doc = Document::parse(SOURCE).unwrap();
    let mut no_guard = tick(&doc);
    no_guard.guard = None;
    no_guard.splices.clear();
    assert!(
        plan(&doc, vec![no_guard])
            .unwrap_err()
            .message
            .contains("missing its guard")
    );
    let mut no_expectation = tick(&doc);
    no_expectation.expectation = None;
    no_expectation.splices.clear();
    assert!(
        plan(&doc, vec![no_expectation])
            .unwrap_err()
            .message
            .contains("missing its required expectation")
    );
}

#[test]
fn planning_gate_rechecks_native_item_and_collection_revisions() {
    let doc = Document::parse(SOURCE).unwrap();
    let changed = Document::parse(&SOURCE.replace("first <!--", "other <!--")).unwrap();
    let binding = bound("A");
    let select = Selector::HeadingPath {
        path: vec!["A".into()],
    };
    let native = Planner::new(&doc)
        .structural(&Op::Set {
            collection: None,
            typed_guard: None,
            guard: doc.target_revision(doc.resolve(&select).unwrap()),
            select,
            part: Some(SectionPart::Body),
            content: json!("\nreplacement\n\n"),
            allow_drop: true,
        })
        .unwrap();
    let update = schema::plan_update(
        &mut Planner::new(&doc),
        &binding,
        &schema::ItemLocator::Id("first".into()),
        &schema::item_revision(&doc, &binding, 0, 0),
        GuardScope::Action,
        &BTreeMap::from([("text".into(), json!("replacement"))]),
    )
    .unwrap();
    for change in [native, update, typed_insert(&doc, &binding)] {
        assert_eq!(
            plan(&changed, vec![change]).unwrap_err().code,
            ErrorCode::GuardConflict
        );
    }
}

#[test]
fn planning_gate_stamp_allows_only_the_minted_markers() {
    let doc = Document::parse("# A\n\nbody\n").unwrap();
    let mut change = Planner::new(&doc)
        .structural(&Op::Stamp {
            collection: None,
            checkbox_only: false,
            collection_guard: None,
            select: Selector::Document,
            guard: doc.revision(),
        })
        .unwrap();
    change.splices[0].text.push_str("\n\nextra\n");
    let error = plan(&doc, vec![change]).unwrap_err();
    assert_eq!(error.code, ErrorCode::InvalidRequest);
    assert!(error.message.contains("mint placements"));
}

#[test]
fn planning_gate_adoption_uses_the_repair_blockers() {
    let doc = Document::parse("- [ ] first <!-- stay:bare foo=bar -->\n- [ ] second\n").unwrap();
    let mut planner = Planner::new(&doc);
    let mut change = planner.adopt_stay("bare").unwrap();
    let target = adoption_target(&doc, "bare").unwrap().0;
    change.guard = Some(Guard::new(
        GuardSubject::Native(target),
        &doc.target_revision(target),
    ));
    let error = plan(&doc, vec![change]).unwrap_err();
    assert_eq!(error.code, ErrorCode::UnsupportedSyntax);
    assert!(error.message.contains("only a bare"));
}

#[test]
fn planning_gate_binds_native_guard_to_its_source_unit() {
    let doc = Document::parse(SOURCE).unwrap();
    let mut change = tick(&doc);
    let other = Target::Item { list: 0, item: 1 };
    change.guard = Some(Guard::new(
        GuardSubject::Native(other),
        &doc.target_revision(other),
    ));
    let error = plan(&doc, vec![change]).unwrap_err();
    assert_eq!(error.code, ErrorCode::InvalidRequest);
    assert!(error.message.contains("guard subject"));
}

#[test]
fn planning_gate_binds_typed_guard_to_its_item_and_collection() {
    let doc = Document::parse(SOURCE).unwrap();
    let a = bound("A");
    let b = bound("B");
    let mut update = typed_update(&doc, &a);
    update.guard = Some(Guard::new(
        GuardSubject::Item {
            bound: b.clone(),
            locator: schema::ItemLocator::Id("third".into()),
        },
        &schema::item_revision(&doc, &b, 1, 0),
    ));
    assert!(
        plan(&doc, vec![update])
            .unwrap_err()
            .message
            .contains("guard subject")
    );
    let mut insert = typed_insert(&doc, &b);
    let inst = schema::instance(&doc, &a).unwrap().unwrap();
    insert.guard = Some(Guard::new(
        GuardSubject::Collection(a.clone()),
        &schema::collection_revision(&doc, &a, &inst),
    ));
    assert!(
        plan(&doc, vec![insert])
            .unwrap_err()
            .message
            .contains("guard subject")
    );
}

#[test]
fn planning_gate_rejects_a_typed_item_outside_its_binding() {
    let doc = Document::parse(SOURCE).unwrap();
    let a = bound("A");
    let b = bound("B");
    let mut change = typed_insert(&doc, &b);
    change.guard = Some(Guard::new(
        GuardSubject::Item {
            bound: a.clone(),
            locator: schema::ItemLocator::Id("third".into()),
        },
        &schema::item_revision(&doc, &a, 1, 0),
    ));
    assert!(
        plan(&doc, vec![change])
            .unwrap_err()
            .message
            .contains("not an item of collection")
    );
}

#[test]
fn planning_gate_does_not_accept_a_native_guard_for_typed_fields() {
    let doc = Document::parse(SOURCE).unwrap();
    let mut change = typed_update(&doc, &bound("A"));
    let item = Target::Item { list: 0, item: 0 };
    change.guard = Some(Guard::new(
        GuardSubject::Native(item),
        &doc.target_revision(item),
    ));
    assert!(
        plan(&doc, vec![change])
            .unwrap_err()
            .message
            .contains("guard subject")
    );
}

#[test]
fn planning_gate_binds_field_readback_to_the_guarded_schema() {
    let doc = Document::parse(SOURCE).unwrap();
    let mut change = typed_update(&doc, &bound("A"));
    let Some(Expectation::Fields { fields, .. }) = &mut change.expectation else {
        panic!()
    };
    fields.bound.digest = "different-schema-bytes".into();
    assert!(
        plan(&doc, vec![change])
            .unwrap_err()
            .message
            .contains("guard subject")
    );
}

#[test]
fn planning_gate_blocks_the_document_even_without_changes() {
    let doc = Document::parse("# A\n\ntext\n{/* stay:mdx */}\n").unwrap();
    assert!(doc.write_blocker.is_some());
    assert_eq!(plan(&doc, vec![]).unwrap_err(), doc.write_blocker.unwrap());
}

#[test]
fn planning_gate_confines_splices_even_in_unparsed_whitespace() {
    let doc = Document::parse(SOURCE).unwrap();
    let mut change = tick(&doc);
    let gap = SOURCE.find("\n\n# B").unwrap() + 1;
    change.splices.push(Splice {
        span: Span {
            start: gap,
            end: gap,
        },
        text: " \n".into(),
    });
    let error = plan(&doc, vec![change]).unwrap_err();
    assert_eq!(error.code, ErrorCode::InvalidRequest);
    assert!(error.message.contains("edit units"));
}

#[test]
fn planning_gate_rejects_out_of_bounds_and_split_utf8_splices() {
    let doc = Document::parse(&SOURCE.replace("first", "firšt")).unwrap();
    let middle = doc.source.find('š').unwrap() + 1;
    for span in [
        Span {
            start: middle,
            end: middle,
        },
        Span {
            start: 0,
            end: usize::MAX,
        },
        Span { start: 9, end: 8 },
    ] {
        let mut change = tick(&doc);
        change.splices[0].span = span;
        let error = plan(&doc, vec![change]).unwrap_err();
        assert_eq!(error.code, ErrorCode::InvalidRequest);
        assert!(error.message.contains("source bounds"));
    }
}

#[test]
fn planning_gate_rejects_missing_and_overflowing_region_splices() {
    let doc = Document::parse(SOURCE).unwrap();
    for mode in ["foreign", "overflow"] {
        let mut change = rewrite(&doc);
        let Some(Expectation::Item {
            region: ItemRegion::Spliced { splice, offset, .. },
            ..
        }) = &mut change.expectation
        else {
            panic!()
        };
        if mode == "foreign" {
            *splice = doc.lists[0].items[1].span;
        } else {
            *offset = usize::MAX;
        }
        let error = plan(&doc, vec![change]).unwrap_err();
        assert_eq!(error.code, ErrorCode::InvalidRequest);
        assert!(error.message.contains(if mode == "foreign" {
            "another change's splice"
        } else {
            "exceeds its splice"
        }));
    }
}

#[test]
fn planning_gate_rejects_bad_original_units_and_mapped_tails() {
    let doc = Document::parse(SOURCE).unwrap();
    let mut missing = tick(&doc);
    let Some(Expectation::Checkbox { list, .. }) = &mut missing.expectation else {
        panic!()
    };
    *list = usize::MAX;
    assert!(
        plan(&doc, vec![missing])
            .unwrap_err()
            .message
            .contains("missing source unit")
    );
    let mut tail = tick(&doc);
    let Some(Expectation::Checkbox { region, .. }) = &mut tail.expectation else {
        panic!()
    };
    *region = ItemRegion::Mapped {
        list: 0,
        span: doc.lists[0].items[0].span,
        tail: usize::MAX,
    };
    assert!(
        plan(&doc, vec![tail])
            .unwrap_err()
            .message
            .contains("tail exceeds")
    );
}

#[test]
fn planning_gate_checks_candidate_extents_before_mapping() {
    let doc = Document::parse("# A\n\nbody\n").unwrap();
    let select = Selector::HeadingPath {
        path: vec!["A".into()],
    };
    let mut change = Planner::new(&doc)
        .structural(&Op::Append {
            guard: doc.target_revision(doc.resolve(&select).unwrap()),
            select,
            content: "new paragraph".into(),
        })
        .unwrap();
    let Some(Expectation::Append { lead, len, .. }) = &mut change.expectation else {
        panic!()
    };
    *lead = usize::MAX;
    *len = usize::MAX;
    let error = plan(&doc, vec![change]).unwrap_err();
    assert_eq!(error.code, ErrorCode::InvalidRequest);
    assert!(error.message.contains("candidate extents"));
}

#[test]
fn planning_gate_checks_a_new_lists_whole_shape() {
    let doc = Document::parse("# A\n\nplain\n").unwrap();
    let mut change = typed_insert(&doc, &bound("A"));
    change.splices[0].text.push_str("\n\nextra paragraph\n");
    let error = plan(&doc, vec![change]).unwrap_err();
    assert_eq!(error.code, ErrorCode::InvalidItem);
    assert!(
        error
            .message
            .contains("new list must read back as one item")
    );
}

#[test]
fn planning_gate_requires_an_exact_empty_delete_replacement() {
    let doc = Document::parse("# A\n\nbody\n\n# B\n\noutside\n").unwrap();
    for mode in ["replace", "missing", "partial"] {
        let select = Selector::HeadingPath {
            path: vec!["A".into()],
        };
        let mut change = Planner::new(&doc)
            .structural(&Op::Delete {
                list: None,
                collection: None,
                guard: doc.target_revision(doc.resolve(&select).unwrap()),
                select,
            })
            .unwrap();
        match mode {
            "replace" => change.splices[0].text = "replacement prose\n\n".into(),
            "missing" => change.splices.clear(),
            "partial" => change.splices[0].span.end = doc.sections[0].heading.end,
            _ => unreachable!(),
        }
        let error = plan(&doc, vec![change]).unwrap_err();
        assert_eq!(error.code, ErrorCode::InvalidRequest);
        assert!(error.message.contains("one empty replacement"));
    }
}

#[test]
fn planning_gate_refuses_projectless_delete_preconditions_before_resolution() {
    let doc = Document::parse(SOURCE).unwrap();
    for select in [
        Selector::Id {
            id: "missing".into(),
        },
        Selector::Document,
    ] {
        let error = Planner::new(&doc)
            .structural(&Op::Delete {
                list: None,
                select,
                guard: "not a revision".into(),
                collection: Some("tasks".into()),
            })
            .err()
            .expect("project context must be required before resolving the target or guard");
        assert_eq!(error.code, ErrorCode::InvalidRequest);
        assert_eq!(
            error.message,
            "a delete collection precondition needs a project schema context"
        );
    }
}

#[test]
fn planning_gate_rechecks_resolved_delete_membership_independently_of_the_builder() {
    let doc = Document::parse(SOURCE).unwrap();
    let select = Selector::Id { id: "first".into() };
    let op = Op::Delete {
        list: None,
        guard: doc.target_revision(doc.resolve(&select).unwrap()),
        select,
        collection: Some("tasks".into()),
    };
    let mut change = Planner::new(&doc)
        .delete_in_collection(&op, &bound("A"))
        .unwrap();
    // Keep the source, native guard and exact deletion splice valid. Only the
    // carried binding changes, so no other gate can hide a missing membership check.
    let Some(Expectation::Delete { collection, .. }) = &mut change.expectation else {
        panic!("missing delete expectation");
    };
    *collection = Some(Box::new(bound("B")));
    assert_eq!(
        plan(&doc, vec![change]).unwrap_err().code,
        ErrorCode::CollectionConflict
    );
}

#[test]
fn planning_gate_runs_typed_expectations_after_application() {
    let doc = Document::parse("# A\n\nparagraph\n").unwrap();
    let select = Selector::HeadingPath {
        path: vec!["A".into()],
    };
    let mut change = Planner::new(&doc)
        .structural(&Op::ReplaceInSection {
            guard: doc.target_revision(doc.resolve(&select).unwrap()),
            select,
            old: "paragraph".into(),
            new: "text".into(),
            top_level: false,
        })
        .unwrap();
    change.splices[0].text = "# surprise".into();
    let error = plan(&doc, vec![change]).unwrap_err();
    assert_eq!(error.code, ErrorCode::InvalidContent);
    assert!(error.message.contains("preserve exactly one paragraph"));
}

#[test]
fn planning_gate_checks_nested_shape_and_requires_empty_paragraph_removal() {
    for (source, remove) in [
        ("# A\n\n- paragraph\n", false),
        ("# A\n\nparagraph\n", true),
    ] {
        let doc = Document::parse(source).unwrap();
        let select = Selector::HeadingPath {
            path: vec!["A".into()],
        };
        let guard = doc.target_revision(doc.resolve(&select).unwrap());
        let op = if remove {
            Op::RemoveParagraphInSection {
                select,
                guard,
                old: "paragraph".into(),
            }
        } else {
            Op::ReplaceInSection {
                select,
                guard,
                old: "paragraph".into(),
                new: "text".into(),
                top_level: false,
            }
        };
        let mut change = Planner::new(&doc).structural(&op).unwrap();
        change.splices[0].text = "\n# surprise".into();
        assert_eq!(
            plan(&doc, vec![change]).unwrap_err().code,
            ErrorCode::InvalidContent
        );
    }
}

#[test]
fn planning_gate_refuses_a_splice_extended_into_a_continuation_prefix() {
    let source = "# A\n\n> first\n> continuation\n";
    let doc = Document::parse(source).unwrap();
    let select = Selector::HeadingPath {
        path: vec!["A".into()],
    };
    let op = Op::ReplaceInSection {
        guard: doc.target_revision(doc.resolve(&select).unwrap()),
        select,
        old: "continuation".into(),
        new: "landed".into(),
        top_level: false,
    };
    let mut change = Planner::new(&doc).structural(&op).unwrap();
    change.splices[0].span.start -= 2;
    assert_eq!(
        plan(&doc, vec![change]).unwrap_err().code,
        ErrorCode::UnsupportedSyntax
    );
}

#[test]
fn planning_gate_derives_marker_policy_and_carry_allowances() {
    let source = "- [ ] first <!-- stay:first subhash=sha256:000000000000 -->\n  - child <!-- stay:foreign subhash=sha256:1 -->\n- [ ] second <!-- stay:second subhash=sha256:000000000000 -->\n";
    let doc = Document::parse(source).unwrap();
    // The low-level builder has not performed the planning gate.
    let mut change = Planner::new(&doc).set_item(0, 0, "[x] replaced").unwrap();
    let item = Target::Item { list: 0, item: 0 };
    change.guard = Some(Guard::new(
        GuardSubject::Native(item),
        &doc.target_revision(item),
    ));
    let error = plan(&doc, vec![change]).unwrap_err();
    assert_eq!(error.code, ErrorCode::UnsupportedSyntax);
    assert!(error.message.contains("owned by another unit"));
    let doc = Document::parse(SOURCE).unwrap();
    let mut change = rewrite(&doc);
    // Supply the genuine carry too, so a mutant that trusts this list reaches
    // the injected marker instead of refusing the genuine one first.
    change.carried.push("first".into());
    change.carried.push("injected".into());
    change.splices[0].text.push_str(" <!-- stay:injected -->");
    // Keep the descriptor honest so provenance, rather than region bounds, judges it.
    let Some(Expectation::Item {
        region: ItemRegion::Spliced { len, .. },
        ..
    }) = &mut change.expectation
    else {
        panic!()
    };
    *len = change.splices[0].text.len();
    let error = plan(&doc, vec![change]).unwrap_err();
    assert_eq!(error.code, ErrorCode::InvalidContent);
    assert!(error.message.contains("supplied content"));
}

#[test]
fn planning_gate_reports_lexical_losses_across_the_whole_batch() {
    let source = "# A\n\n- p\n  - c <!-- stay:shared subhash=sha256:1 -->\n\n# B\n\n- q\n  - d <!-- stay:shared subhash=sha256:1 -->\n";
    let doc = Document::parse(source).unwrap();
    let mut planner = Planner::new(&doc);
    let changes = ["A", "B"]
        .into_iter()
        .map(|name| {
            let select = Selector::HeadingPath {
                path: vec![name.into()],
            };
            planner
                .structural(&Op::Set {
                    collection: None,
                    typed_guard: None,
                    guard: doc.target_revision(doc.resolve(&select).unwrap()),
                    select,
                    part: Some(SectionPart::Body),
                    content: json!("\nplain\n\n"),
                    allow_drop: true,
                })
                .unwrap()
        })
        .collect();
    assert!(
        plan(&doc, changes)
            .unwrap_err()
            .message
            .contains("without being declared dropped")
    );
}

#[test]
fn planning_gate_rechecks_both_bound_body_guards_and_schema() {
    let doc = Document::parse(SOURCE).unwrap();
    let bound = bound("A");
    let op = Op::Set {
        select: Selector::Id { id: "first".into() },
        part: None,
        guard: doc.target_revision(Target::Item { list: 0, item: 0 }),
        collection: Some("tasks".into()),
        typed_guard: Some(schema::item_revision(&doc, &bound, 0, 0)),
        content: json!("[x] changed"),
        allow_drop: false,
    };
    for native in [false, true] {
        let mut change =
            schema::plan_bound_set(&mut Planner::new(&doc), std::slice::from_ref(&bound), &op)
                .unwrap();
        let guard = change.guard.as_mut().unwrap();
        if native {
            let GuardSubject::BoundBody { native, .. } = &mut guard.subject else {
                panic!()
            };
            *native = "rivet:v1:item:00000000000000000000000000000000".into();
        } else {
            guard.supplied = "rivet:v1:action:00000000000000000000000000000000".into();
        }
        assert_eq!(
            plan(&doc, vec![change]).unwrap_err().code,
            ErrorCode::GuardConflict
        );
    }
    let mut change =
        schema::plan_bound_set(&mut Planner::new(&doc), std::slice::from_ref(&bound), &op).unwrap();
    let Some(Expectation::Fields { fields, .. }) = &mut change.expectation else {
        panic!()
    };
    fields.bound.binding = "different binding".into();
    assert_eq!(
        plan(&doc, vec![change]).unwrap_err().code,
        ErrorCode::InvalidRequest
    );
}

#[test]
fn planning_gate_scoped_stamp_excludes_headings_and_prose_and_rechecks_guards() {
    let doc = Document::parse("# A\n\n- [ ] task\n- prose\n- [ ] tail\n").unwrap();
    let bound = bound("A");
    let op = Op::Stamp {
        select: Selector::Collections,
        guard: doc.revision(),
        collection: Some("tasks".into()),
        checkbox_only: true,
        collection_guard: Some(schema::collection_revision(
            &doc,
            &bound,
            &schema::instance(&doc, &bound).unwrap().unwrap(),
        )),
    };
    for native in [false, true] {
        let mut change =
            schema::plan_stamp(&mut Planner::new(&doc), std::slice::from_ref(&bound), &op).unwrap();
        let guard = change.guard.as_mut().unwrap();
        if native {
            let GuardSubject::CheckboxStamp { native, .. } = &mut guard.subject else {
                panic!()
            };
            *native = "rivet:v1:document:00000000000000000000000000000000".into();
        } else {
            guard.supplied = "rivet:v1:collection:00000000000000000000000000000000".into();
        }
        assert_eq!(
            plan(&doc, vec![change]).unwrap_err().code,
            ErrorCode::GuardConflict
        );
    }
    for (at, kind, start, text) in [
        (
            doc.blocks[doc.sections[0].block].span.end,
            "section",
            doc.sections[0].heading.start,
            format!(
                "\n{}",
                markers::format_plain("ForbiddenHeading", "hash", PLACEHOLDER)
            ),
        ),
        (
            doc.lists[0].items[1].carrier.unwrap(),
            "item",
            doc.lists[0].items[1].span.start,
            format!(
                " {}",
                markers::format_plain("ForbiddenProse", "subhash", PLACEHOLDER)
            ),
        ),
    ] {
        let mut change =
            schema::plan_stamp(&mut Planner::new(&doc), std::slice::from_ref(&bound), &op).unwrap();
        let id = if kind == "section" {
            "ForbiddenHeading"
        } else {
            "ForbiddenProse"
        };
        // A valid declaration and exact marker text pass mint-placement
        // validation if scope confinement is removed. Only scope must refuse.
        change.minted.push((id.into(), kind, start));
        change.splices.push(Splice {
            span: Span { start: at, end: at },
            text,
        });
        let error = plan(&doc, vec![change]).unwrap_err();
        assert_eq!(error.code, ErrorCode::InvalidRequest);
        assert!(
            error.message.contains("outside its guarded operation"),
            "{error:?}"
        );
    }
}
