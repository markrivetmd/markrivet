mod common;
#[path = "common/move_fixture.rs"]
mod move_fixture;
use common::*;
use markrivet_core::{Document, Selector, document::Target};
use move_fixture::{A, B, MoveStore};
use serde_json::{Value, json};

fn section(source: &str, id: &str) -> (usize, markrivet_core::Span) {
    let doc = Document::parse(source).unwrap();
    let Target::Section(s) = doc.resolve(&Selector::Id { id: id.into() }).unwrap() else {
        panic!("not a section")
    };
    (s, doc.sections[s].whole)
}

fn moved(s: &MoveStore, before: bool) -> Value {
    let mut request = s.section_request();
    if before {
        request["before"] = json!("dchild01");
    }
    s.ok("move", request)
}

#[test]
fn section_move_is_one_commit_of_two_paths() {
    for crlf in [false, true] {
        for before in [false, true] {
            let s = MoveStore::section(crlf);
            let a = s.read(A);
            let out = moved(&s, before);
            assert_eq!(out["status"], "applied", "{out}");
            assert_eq!(
                s.read(A),
                MoveStore::literal("section", "source-after", crlf)
            );
            assert_eq!(
                s.read(B),
                MoveStore::literal(
                    "section",
                    if before {
                        "destination-before"
                    } else {
                        "destination-after"
                    },
                    crlf
                )
            );
            let cb = Document::parse(&s.read(B)).unwrap();
            let Target::Section(dest) = cb
                .resolve(&Selector::Id {
                    id: "section1".into(),
                })
                .unwrap()
            else {
                panic!()
            };
            assert_eq!(cb.sections[dest].level, 3);
            assert_eq!(
                cb.sections[dest].parent.map(|p| cb.section_ids(p)),
                Some(vec!["dparent1".into()])
            );
            let message = s.git(&["log", "-1", "--format=%B"]);
            assert_eq!(message.matches("Rivet-Move: ").count(), 1);
            assert_eq!(message.matches("Rivet-Op: ").count(), 1);
            let receipt = markrivet_core::relocate::MoveReceipt::decode(
                message
                    .lines()
                    .find_map(|l| l.strip_prefix("Rivet-Move: "))
                    .unwrap(),
            )
            .unwrap();
            assert_eq!(receipt.kind, "section");
            assert_eq!(receipt.source.parent.as_deref(), Some("sparent1"));
            assert_eq!(receipt.destination.parent.as_deref(), Some("dparent1"));
            assert_eq!(
                receipt.ids,
                [
                    "section1", "secalias", "payload1", "unitlist", "detail01", "unittabl"
                ]
            );
            assert_eq!(
                s.git(&["diff-tree", "-r", "--no-commit-id", "--name-only", "HEAD"])
                    .lines()
                    .collect::<Vec<_>>(),
                [A, B]
            );
            assert!(s.records().is_empty());
            assert!(!s.root.join(".git/index.lock").exists());
            let snapshot = s.snapshot();
            let replay = {
                let mut r = s.section_request_from(&a);
                if before {
                    r["before"] = json!("dchild01");
                }
                s.ok("move", r)
            };
            assert_eq!(replay["status"], "unchanged");
            assert_eq!(replay["commit"]["sha"], out["commit"]["sha"]);
            assert_eq!(replay["commit"]["op"], out["commit"]["op"]);
            assert_eq!(replay["current"], "matches");
            assert_eq!(snapshot, s.snapshot());
        }
    }
}

fn unit_request(s: &MoveStore, id: &str, parent: &str, destination: &str) -> Value {
    json!({"source":A,"destination":B,"id":id,"guard":item_guard(&s.root,A,id),
        "source_parent":parent,"destination_parent":destination})
}

#[test]
fn queue_batch_title_and_containing_sections_refuse() {
    let s = MoveStore::section(false);
    s.refuses(
        unit_request(&s, "saction1", "stitle01", "dtitle01"),
        "move_refused",
        Some("protected_unit"),
    );
    s.refuses(
        unit_request(&s, "stitle01", "stitle01", "dtitle01"),
        "move_refused",
        Some("protected_unit"),
    );
    let source = s
        .read(A)
        .replace("## Notes", "### Batch\n<!-- stay:batch001 -->\n\n## Notes");
    s.put(A, &source);
    s.commit();
    s.refuses(
        unit_request(&s, "batch001", "saction1", "daction1"),
        "move_refused",
        Some("protected_unit"),
    );
    let source = s.read(A).replace(
        "## Action",
        "## Container\n<!-- stay:contain1 -->\n\n### Action",
    );
    let mut config = s.config();
    config["collections"]["actions"]["level"] = json!(3);
    s.set_config(config);
    s.put(A, &source);
    s.commit();
    s.refuses(
        unit_request(&s, "contain1", "stitle01", "dtitle01"),
        "move_refused",
        Some("protected_unit"),
    );
}

#[test]
fn section_level_sibling_and_before_preconditions_refuse() {
    let s = MoveStore::section(false);
    let mut request = s.section_request();
    request["destination_parent"] = json!("dtitle01");
    s.refuses(request, "move_refused", Some("parent_level"));
    s.put(B, &s.read(B).replace("### Existing", "### Payload"));
    s.commit();
    s.refuses(s.section_request(), "move_refused", Some("sibling_title"));
    s.put(B, &s.read(B).replace("### Payload", "### Existing"));
    s.commit();
    for id in ["missing1", "dtitle01", "dlog0001", "ditem001"] {
        let mut r = s.section_request();
        r["before"] = json!(id);
        s.refuses(r, "invalid_request", None);
    }
    let mut r = s.section_request();
    r["source_parent"] = json!("stitle01");
    s.refuses(r, "move_refused", Some("not_native"));
    for key in ["collection", "typed_guard", "under"] {
        let mut r = s.section_request();
        r[key] = json!("actions");
        s.refuses(r, "invalid_request", None);
    }
}

#[test]
fn native_guard_children_references_and_archives_refuse_without_publication() {
    let s = MoveStore::section(false);
    let mut r = s.section_request();
    r["guard"] = json!("stale");
    s.refuses(r, "guard_conflict", None);
    s.put(
        B,
        &s.read(B).replace(
            "Destination history.",
            "Destination history.\n<!-- stay:detail01 -->",
        ),
    );
    s.commit();
    s.refuses(
        s.section_request(),
        "move_refused",
        Some("destination_carries_id"),
    );
    s.put(B, &MoveStore::literal("section", "destination", false));
    s.refresh_fixture(B);
    let original = s.read(A);
    s.put(
        A,
        &original.replace(
            "Payload paragraph.",
            "Payload paragraph.\n\n[ref]: https://example.invalid",
        ),
    );
    s.commit();
    s.refuses(
        s.section_request(),
        "move_refused",
        Some("reference_definition"),
    );
    s.put(
        A,
        &original.replace("Source history.", "Source history.\n<!-- stay:payload1 -->"),
    );
    s.commit();
    s.refuses(s.section_request(), "move_refused", Some("not_native"));
    s.put(
        A,
        &original.replace(
            "Payload paragraph.",
            "Payload paragraph. <!-- stay:badchild subhash=sha256:0 -->",
        ),
    );
    s.commit();
    s.refuses(s.section_request(), "move_refused", Some("not_native"));
    s.put(A, &original);
    let mut config = s.config();
    config["section_archives"] =
        json!({"history":{"files":["tracking/*.md"],"archive":["archive/history/*.md"]}});
    s.set_config(config);
    std::fs::create_dir_all(s.root.join("archive/history")).unwrap();
    s.put(
        "archive/history/old.md",
        "# History\n<!-- stay:detail01 -->\n",
    );
    s.commit();
    s.refuses(s.section_request(), "move_refused", Some("archived_id"));
}

#[test]
fn plain_section_under_action_and_binding_title_refuse() {
    let s = MoveStore::section(false);
    let mut r = s.section_request();
    r["destination_parent"] = json!("daction1");
    s.refuses(r, "move_refused", Some("membership_changed"));
    s.put(A, &s.read(A).replace("### Payload", "### Action"));
    s.commit();
    let mut config = s.config();
    config["collections"]["actions"]
        .as_object_mut()
        .unwrap()
        .remove("level");
    s.set_config(config);
    s.commit();
    // Source would itself be ambiguous; refusal must still leave every byte intact.
    s.refuses(s.section_request(), "ambiguous_selector", None);
}

#[test]
fn excluded_rule_candidates_and_descendants_remain_unbound() {
    for destination in ["daction1", "dparent1"] {
        let s = MoveStore::section(false);
        s.put(
            A,
            &s.read(A)
                .replace("### Payload", "### Rule candidates")
                .replace("## Notes", "## Action notes"),
        );
        // Put the excluded subtree directly under Action.
        let source = s.read(A).replace(
            "## Action notes\n<!-- stay:sparent1 -->\n\nUntouched source prefix.\n\n",
            "",
        );
        s.put(A, &source);
        let mut config = s.config();
        config["collections"]["actions"]["exclude"] = json!(["Rule candidates", "Plans"]);
        s.set_config(config);
        s.put(A,&s.read(A).replace("Payload paragraph.","- [ ] **Candidate.** Context. <!-- stay:cand0001 subhash=sha256:0 -->\n<!-- stay:candlist -->"));
        s.refresh_fixture(A);
        s.commit();
        let before = s.ok("query", json!({"collection":"actions","paths":[A]}));
        assert!(!before.to_string().contains("cand0001"));
        let mut r = unit_request(&s, "section1", "saction1", destination);
        r["before"] = Value::Null;
        s.ok("move", r);
        let after = s.ok("query", json!({"collection":"actions","paths":[B]}));
        assert!(!after.to_string().contains("cand0001"));
    }
}

#[test]
fn details_under_excluded_plans_cannot_join_a_batch() {
    let s = MoveStore::section(false);
    s.put(
        A,
        &s.read(A)
            .replace("## Notes", "### Plans")
            .replace("#### Detail", "##### Detail")
            .replace("### Payload", "#### Details"),
    );
    let mut config = s.config();
    config["collections"]["actions"]["exclude"] = json!(["Plans"]);
    s.put(B, &s.read(B).replace("## Notes", "### Batch"));
    s.set_config(config);
    s.commit();
    s.refuses(
        unit_request(&s, "section1", "sparent1", "dchild01"),
        "move_refused",
        Some("membership_changed"),
    );
}

#[test]
fn log_is_an_ordinary_section_and_before_keeps_it_last() {
    let s = MoveStore::section(false);
    let r = unit_request(&s, "slog0001", "stitle01", "dtitle01");
    s.refuses(r, "move_refused", Some("sibling_title"));
    s.put(B, &s.read(B).replace("## Log", "## History"));
    s.commit();
    let mut r = unit_request(&s, "slog0001", "stitle01", "dtitle01");
    r["before"] = json!("dlog0001");
    s.ok("move", r);
    assert!(s.read(B).find("## Log").unwrap() < s.read(B).find("## History").unwrap());
}

#[test]
fn section_replay_checks_every_precondition_and_current_state() {
    let s = MoveStore::section(false);
    let request = s.section_request();
    s.ok("move", request.clone());
    for key in ["guard", "source_parent", "destination_parent", "before"] {
        let mut r = request.clone();
        r[key] = json!("other001");
        s.refuses(r, "not_found", None);
    }
    s.put(
        B,
        &s.read(B).replace("Payload paragraph.", "Edited payload."),
    );
    let snapshot = s.snapshot();
    assert_eq!(s.ok("move", request.clone())["current"], "changed");
    assert_eq!(snapshot, s.snapshot());
    let b = s.read(B);
    let (_, span) = section(&b, "section1");
    s.put(B, &b.replacen(span.slice(&b).unwrap(), "", 1));
    assert_eq!(s.ok("move", request)["current"], "absent");
}

#[test]
fn heading_that_would_create_or_ambiguate_a_destination_binding_refuses() {
    for title in ["Payload", "Action"] {
        let s = MoveStore::section(false);
        let mut config = s.config();
        config["collections"]["extra"] = json!({
            "files":[B], "schema":"tracker-action@1", "section":title, "subsections":true
        });
        if title == "Action" {
            s.put(A, &s.read(A).replace("### Payload", "### Action"));
        }
        s.set_config(config);
        s.commit();
        s.refuses(
            s.section_request(),
            "move_refused",
            Some("membership_changed"),
        );
    }
}

#[test]
fn section_stamp_is_idempotent_after_move() {
    let s = MoveStore::section(false);
    s.stamp(A);
    s.stamp(B);
    let out = moved(&s, false);
    assert_eq!(out["status"], "applied");
    assert_eq!(s.stamp(A)["status"], "unchanged");
    assert_eq!(s.stamp(B)["status"], "unchanged");
}

#[test]
fn section_native_evidence_and_terminal_boundaries_refuse() {
    let s = MoveStore::section(false);
    let original = s.read(A);
    for source in [
        original.replace(
            "stay:payload1 -->",
            "stay:payload1 hash=sha256:000000000000 -->",
        ),
        original.replace(
            "Payload paragraph.",
            "Payload paragraph.\n<!-- stay:unowned1 subhash=sha256:000000000000 -->",
        ),
        original[..original.find("### Keep").unwrap()]
            .trim_end()
            .to_string(),
        format!(
            "{}\n```\nunclosed\n",
            &original[..original.find("### Keep").unwrap()]
        ),
    ] {
        s.put(A, &source);
        s.commit();
        s.refuses(s.section_request(), "move_refused", Some("not_native"));
    }
}

#[test]
fn before_refuses_a_grandchild_and_sections_reject_item_fields_on_replay() {
    let s = MoveStore::section(false);
    s.put(
        B,
        &s.read(B).replace(
            "Untouched destination child.",
            "Untouched destination child.\n\n#### Nested\n<!-- stay:nested01 -->\n\nNested body.",
        ),
    );
    s.commit();
    let mut r = s.section_request();
    r["before"] = json!("nested01");
    s.refuses(r, "invalid_request", None);
    let request = s.section_request();
    s.ok("move", request.clone());
    for key in ["collection", "typed_guard", "under"] {
        let mut r = request.clone();
        r[key] = json!("actions");
        s.refuses(r, "invalid_request", None);
    }
}

#[test]
fn section_replay_reports_changed_when_an_outside_reference_switches_the_marker_owner() {
    for change in [false, true] {
        let s = MoveStore::section(false);
        s.put(A,"# Source\n<!-- stay:stitle01 -->\n\n[ref]: /synthetic\n\n## Notes\n<!-- stay:sparent1 -->\n\n### Payload\n<!-- stay:section1 -->\n\nFirst [foo][ref](<!-- stay:Nested01 -->)\n\nSecond [bar][other](<!-- stay:Nested01 -->)\n");
        s.put(B,"# Destination\n<!-- stay:dtitle01 -->\n\n[ref]: /synthetic\n\n## Notes\n<!-- stay:dparent1 -->\n\n## Log\n<!-- stay:dlog0001 -->\n\nHistory.\n");
        s.commit();
        let request = s.section_request();
        s.ok("move", request.clone());
        let before = s.read(B);
        let after = before.replace(
            "[ref]: /synthetic",
            if change {
                "[other]: /synthetic"
            } else {
                "[ref]: /synthetic\n[unused]: /synthetic"
            },
        );
        let old = Document::parse(&before).unwrap();
        let new = Document::parse(&after).unwrap();
        let select = Selector::Id {
            id: "section1".into(),
        };
        assert_eq!(
            old.target_revision(old.resolve(&select).unwrap()),
            new.target_revision(new.resolve(&select).unwrap())
        );
        let stay = Selector::Id {
            id: "Nested01".into(),
        };
        let owner = new.target_span(new.resolve(&stay).unwrap()).0;
        assert!(
            owner
                .slice(&after)
                .unwrap()
                .starts_with(if change { "Second" } else { "First" })
        );
        s.put(B, &after);
        let snapshot = s.snapshot();
        assert_eq!(
            s.ok("move", request)["current"],
            if change { "changed" } else { "matches" }
        );
        assert_eq!(s.snapshot(), snapshot);
    }
}

#[test]
fn section_replay_proves_historical_parent_and_before_placement() {
    use base64::Engine;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    for field in ["source_parent", "destination_parent", "before"] {
        let s = MoveStore::section(false);
        let mut request = s.section_request();
        s.ok("move", request.clone());
        let message = s.git(&["log", "-1", "--format=%B"]);
        let trailer = message
            .lines()
            .find(|l| l.starts_with("Rivet-Move: "))
            .unwrap();
        let mut receipt = markrivet_core::relocate::MoveReceipt::decode(
            trailer.strip_prefix("Rivet-Move: ").unwrap(),
        )
        .unwrap();
        let value = match field {
            "source_parent" => {
                receipt.source.parent = Some("stitle01".into());
                "stitle01"
            }
            "destination_parent" => {
                receipt.destination.parent = Some("daction1".into());
                "daction1"
            }
            _ => {
                receipt.destination.before = Some("dchild01".into());
                "dchild01"
            }
        };
        request[field] = json!(value);
        let payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&receipt).unwrap());
        s.put(
            ".test-home/amended-message",
            &message.replace(trailer, &format!("Rivet-Move: {payload}")),
        );
        s.git(&[
            "commit",
            "--amend",
            "--no-verify",
            "-q",
            "-F",
            ".test-home/amended-message",
        ]);
        s.refuses(request, "not_found", None);
    }
}
