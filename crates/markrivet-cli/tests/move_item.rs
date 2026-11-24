mod common;
#[path = "common/move_fixture.rs"]
mod fixture;
use fixture::*;
use markrivet_core::{Document, Selector, document::Target};
use serde_json::json;

fn unit(text: &str) -> String {
    let doc = Document::parse(text).unwrap();
    let target = doc.resolve(&Selector::Id { id: ID.into() }).unwrap();
    doc.target_span(target).0.slice(text).unwrap().into()
}

fn assert_layout(shape: &str, crlf: bool) {
    let s = MoveStore::new(shape, crlf);
    let before_a = s.read(A);
    let before_b = s.read(B);
    let request = s.request();
    let out = s.ok("move", request.clone());
    assert_eq!(out["status"], "applied", "{out}");
    assert_eq!(out["commit"]["status"], "committed", "{out}");
    let expected_a = MoveStore::literal(shape, "expected-source", crlf);
    let mut expected_b = MoveStore::literal(shape, "expected-destination", crlf);
    let after_b = s.read(B);
    let doc = Document::parse(&after_b).unwrap();
    let target = doc.resolve(&Selector::Id { id: ID.into() }).unwrap();
    let Target::Item { list, .. } = target else {
        panic!("moved id must own item")
    };
    let list_ids = doc.stay_ids(&doc.blocks[doc.lists[list].block].markers);
    if shape == "empty" {
        assert_eq!(list_ids.len(), 1);
        expected_b = expected_b.replace("freshlis", &list_ids[0]);
        let old = Document::parse(&before_a).unwrap();
        assert!(
            old.resolve(&Selector::Id {
                id: list_ids[0].clone()
            })
            .is_err()
        );
    } else {
        assert_eq!(list_ids, ["destlist"]);
    }
    assert_eq!(s.read(A), expected_a);
    assert_eq!(after_b, expected_b);
    assert_eq!(
        doc.lists[list].items.len(),
        if shape == "empty" {
            1
        } else if shape == "loose" {
            3
        } else {
            2
        }
    );
    assert_eq!(doc.lists[list].tight, shape != "loose");
    assert_eq!(doc.target_ids(target), IDS);
    assert_eq!(unit(&before_a), unit(&after_b));
    let source_doc = Document::parse(&s.read(A)).unwrap();
    for id in IDS {
        assert!(source_doc.resolve(&Selector::Id { id: id.into() }).is_err());
    }
    let (_, gone) = s.run("get", json!({"path":A,"select":{"kind":"id","id":ID}}), &[]);
    // `get` keeps its existing selector contract; move replay uses not_found.
    assert_eq!(gone["error"]["code"], "missing_selector");
    s.ok("get", json!({"path":B,"select":{"kind":"id","id":ID}}));
    let sha = out["commit"]["sha"].as_str().unwrap();
    let changed = s.git(&["diff-tree", "-r", "--no-commit-id", "--name-only", sha]);
    assert_eq!(changed.lines().collect::<Vec<_>>(), [A, B]);
    let message = s.git(&["log", "-1", "--format=%B", sha]);
    assert_eq!(message.matches("Rivet-Op:").count(), 1);
    assert_eq!(message.matches("Rivet-Move:").count(), 1);
    let receipt = markrivet_core::relocate::MoveReceipt::decode(
        message
            .lines()
            .find_map(|l| l.strip_prefix("Rivet-Move: "))
            .unwrap(),
    )
    .unwrap();
    assert_eq!(receipt.ids, IDS);
    assert_eq!(receipt.source.path, A);
    assert_eq!(receipt.destination.path, B);
    assert_eq!(
        unit(&s.git(&["show", &format!("{sha}^:{A}")])),
        unit(&s.git(&["show", &format!("{sha}:{B}")]))
    );
    for path in [A, B] {
        assert_eq!(s.stamp(path)["status"], "unchanged");
    }
    assert!(s.records().is_empty());
    assert!(!s.root.join(".git/index.lock").exists());
    let before = s.snapshot();
    let replay = s.ok("move", request);
    assert_eq!(replay["status"], "unchanged");
    for key in ["sha", "op"] {
        assert_eq!(replay["commit"][key], out["commit"][key]);
    }
    assert_eq!(replay["current"], "matches");
    assert_eq!(s.snapshot(), before);
    assert!(before_b.contains("Preserved suffix."));
}

#[test]
fn item_move_is_one_commit_of_two_paths() {
    assert_layout("tight", false);
}
#[test]
fn layout_tight() {
    assert_layout("tight", true);
}
#[test]
fn layout_loose() {
    for crlf in [false, true] {
        assert_layout("loose", crlf);
    }
}
#[test]
fn layout_empty_keeps_item_and_child_stays() {
    for crlf in [false, true] {
        assert_layout("empty", crlf);
    }
}

#[test]
fn dirty_endpoints_refuse_without_publication() {
    for path in [A, B] {
        let s = MoveStore::new("tight", false);
        let request = s.request();
        s.put(path, &format!("{}\nHand edit.\n", s.read(path)));
        s.refuses(request, "commit_refused", None);
    }
}

#[test]
fn stale_native_and_typed_guards_refuse() {
    for typed in [false, true] {
        let s = MoveStore::new("tight", false);
        let request = s.request();
        if typed {
            let mut config = s.config();
            config["collections"]["actions"]["exclude"] = json!(["Plans"]);
            config["collections"]["actions"]["subsections"] = json!(true);
            s.set_config(config);
            s.commit();
        } else {
            s.ok("set",json!({"path":A,"select":{"kind":"id","id":ID},"guard":request["guard"],"content":"[ ] **Payload.** Edited."}));
        }
        s.refuses(request, "guard_conflict", None);
    }
}

#[test]
fn namespaces_policy_paths_and_item_only_fields_refuse() {
    for (field, value, code, reason) in [
        (
            "destination",
            json!("outside.md"),
            "move_refused",
            Some("destination_not_live"),
        ),
        (
            "source",
            json!("outside.md"),
            "move_refused",
            Some("source_not_live"),
        ),
        (
            "destination",
            json!("archive/B.md"),
            "move_refused",
            Some("destination_archive"),
        ),
        (
            "source",
            json!("archive/A.md"),
            "move_refused",
            Some("source_archive"),
        ),
        ("destination", json!(A), "invalid_request", None),
        ("source", json!("./tracking/A.md"), "path_rejected", None),
        (
            "destination",
            json!("./tracking/B.md"),
            "path_rejected",
            None,
        ),
        ("source_parent", json!("title001"), "invalid_request", None),
        (
            "destination_parent",
            json!("title001"),
            "invalid_request",
            None,
        ),
        ("before", json!("dest0001"), "invalid_request", None),
        (
            "message",
            json!("Rivet-Move: forged"),
            "invalid_request",
            None,
        ),
    ] {
        let s = MoveStore::new("tight", false);
        let mut request = s.request();
        let mut config = s.config();
        config["collections"]["actions"]["archive"] = json!(["archive/*.md"]);
        s.set_config(config);
        s.commit();
        // Keep the new binding-aware guard unless the source path is deliberately invalid.
        request["typed_guard"] = s.request()["typed_guard"].clone();
        request[field] = value;
        s.refuses(request, code, reason);
    }
    for policy in [
        json!({"commit":"off"}),
        json!({"commit":"per-write","on_dirty":"adopt"}),
    ] {
        let s = MoveStore::new("tight", false);
        let request = s.request();
        let mut config = s.config();
        config["git"] = policy;
        s.set_config(config);
        s.commit();
        s.refuses(request, "commit_refused", None);
    }
}

#[test]
fn destination_carrying_any_child_identity_refuses() {
    for carrier in [
        "<!-- stay:child001 -->\n",
        "## Conflict {#child002}\n<!-- stay:other222 -->\n",
        "Trailing <!-- stay:child002 -->\n",
    ] {
        let s = MoveStore::new("tight", false);
        s.put(B, &format!("{}\n{carrier}", s.read(B)));
        s.commit();
        s.refuses(s.request(), "move_refused", Some("destination_carries_id"));
    }
}

#[test]
fn newline_mismatch_and_sole_item_refuse() {
    let s = MoveStore::new("tight", false);
    s.put(B, &s.read(B).replace('\n', "\r\n"));
    s.commit();
    s.refuses(s.request(), "move_refused", Some("newline_mismatch"));
    let s = MoveStore::new("tight", false);
    let text = s
        .read(A)
        .lines()
        .filter(|l| !l.contains("keep0001") && !l.contains("other001"))
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    s.put(A, &text);
    s.stamp(A);
    s.commit();
    let out = s.refuses(s.request(), "move_refused", Some("sole_item"));
    assert!(out["error"]["message"].as_str().unwrap().contains("101 E1"));
}

#[test]
fn native_marker_evidence_and_outside_claims_refuse() {
    for kind in ["drift", "outside", "unaddressed"] {
        let s = MoveStore::new("tight", false);
        let text = match kind {
            "drift" => s.read(A).replace("22a1636f7e2e", "000000000000"),
            "outside" => format!(
                "{}\n## Duplicate {{#child001}}\n<!-- stay:other222 -->\n",
                s.read(A)
            ),
            _ => s.read(A).replace("Context.", "Context. <!-- stay -->"),
        };
        s.put(A, &text);
        s.commit();
        s.refuses(s.request(), "move_refused", Some("not_native"));
    }
}

#[test]
fn source_and_destination_membership_exclusions_refuse() {
    for kind in [
        "source-excluded",
        "under-excluded",
        "under-ambiguous",
        "unbound",
        "overlap",
    ] {
        let s = MoveStore::new("tight", false);
        let mut config = s.config();
        config["collections"]["actions"]["subsections"] = json!(true);
        config["collections"]["actions"]["exclude"] = json!(["Plans"]);
        let mut request = s.request();
        match kind {
            "source-excluded" => s.put(
                A,
                &s.read(A)
                    .replace("- [ ] **Payload.", "### Plans\n\n- [ ] **Payload."),
            ),
            "under-excluded" => {
                s.put(B, &s.read(B).replace("## Log", "### Plans\n\n## Log"));
                request["under"] = json!("Plans");
            }
            "under-ambiguous" => {
                s.put(
                    B,
                    &s.read(B)
                        .replace("## Log", "### Batch\n\n### Batch\n\n## Log"),
                );
                request["under"] = json!("Batch");
            }
            "unbound" => {
                config["collections"]["actions"]["files"] = json!([A]);
                config["section_archives"] =
                    json!({"history":{"files":[B],"archive":["history/*.md"]}});
            }
            _ => {
                config["collections"]["extra"] = config["collections"]["actions"].clone();
                config["collections"]["extra"]["files"] = json!([A]);
            }
        }
        s.set_config(config);
        s.commit();
        s.stamp(A);
        s.stamp(B);
        s.refresh_fixture(A);
        s.refresh_fixture(B);
        s.commit();
        if kind != "source-excluded" {
            request["typed_guard"] = s.request()["typed_guard"].clone();
        }
        let (code, reason) = match kind {
            "under-ambiguous" => ("ambiguous_selector", None),
            "overlap" => ("move_refused", Some("membership_changed")),
            _ => ("collection_conflict", None),
        };
        s.refuses(request, code, reason);
    }
}

#[test]
fn list_shape_changes_refuse() {
    for kind in [
        "loose-into-tight",
        "bullet",
        "source-tightens",
        "plain-list",
    ] {
        let s = MoveStore::new(
            if kind == "loose-into-tight" || kind == "source-tightens" {
                "loose"
            } else {
                "tight"
            },
            false,
        );
        match kind {
            "loose-into-tight" => {
                s.put(B, &MoveStore::literal("tight", "destination", false));
                s.put(
                    A,
                    &s.read(A)
                        .replace("Context.", "Context.\n\n  Second paragraph."),
                );
            }
            "bullet" => s.put(B, &s.read(B).replace("- [ ]", "* [ ]")),
            "source-tightens" => {
                let text = s.read(A).replace("\n\n- [x] **Other.", "\n- [x] **Other.");
                s.put(A, &text);
            }
            _ => s.put(
                B,
                &MoveStore::literal("empty", "destination", false)
                    .replace("## Log", "- Plain neighbour.\n\n## Log"),
            ),
        }
        s.commit();
        s.stamp(A);
        if kind != "plain-list" {
            s.stamp(B);
        }
        s.commit();
        s.refresh_fixture(A);
        s.refresh_fixture(B);
        s.commit();
        s.refuses(s.request(), "move_refused", Some("list_shape"));
    }
}

#[test]
fn reference_definitions_in_the_unit_refuse_and_code_examples_move() {
    for (body, definition) in [
        ("  [ref]: /target\n", true),
        ("  [a\\]b]: /target\n", true),
        ("  [multi\n  line]: /target\n", true),
        ("  [<https://example.invalid>]: /target\n", true),
        ("  ```\n  [ref]: /example\n  ```\n", false),
        ("      [ref]: /example\n", false),
        (
            "  See <https://example.invalid> and `[label]:` as text.\n",
            false,
        ),
    ] {
        let s = MoveStore::new("loose", false);
        let text = s
            .read(A)
            .replace("\n\n- [x] **Keep.", &format!("\n\n{body}\n- [x] **Keep."));
        s.put(A, &text);
        s.refresh_fixture(A);
        s.commit();
        s.stamp(A);
        s.commit();
        let request = s.request();
        if definition {
            s.refuses(request, "move_refused", Some("reference_definition"));
        } else {
            s.ok("move", request);
        }
    }
}

#[test]
fn archived_ids_refuse_after_real_collection_and_section_history_copies() {
    for section in [false, true] {
        let s = MoveStore::new("tight", false);
        let mut config = s.config();
        config["collections"]["actions"]["archive"] = json!(["archive/*.md"]);
        config["section_archives"] =
            json!({"history":{"files":["tracking/*.md"],"archive":["history/*.md"]}});
        s.set_config(config);
        s.commit();
        for dir in ["archive", "history"] {
            std::fs::create_dir(s.root.join(dir)).unwrap();
        }
        let path = if section {
            "history/closed.md"
        } else {
            "archive/closed.md"
        };
        s.ok(
            "create",
            json!({"path":path,"content":"# Archive\n\n## Closed\n","stamp":true}),
        );
        let (source, id, parent) = if section {
            s.put("tracking/C.md","# Other\n<!-- stay:ctitle01 -->\n\n## Log\n<!-- stay:clog0001 -->\n\n### History entry\n<!-- stay:history1 -->\n<!-- stay:child001 -->\n\nEarlier prose.\n");
            s.commit();
            s.stamp("tracking/C.md");
            s.commit();
            ("tracking/C.md", "history1", Some("clog0001"))
        } else {
            (A, ID, None)
        };
        let select = json!({"kind":"heading_path","path":["Closed"]});
        let destination = s.ok("get", json!({"path":path,"select":select}));
        let mut copy = json!({"source":source,"id":id,"guard":common::item_guard(&s.root,source,id),"path":path,"select":select,"destination_guard":destination["revision"]});
        if let Some(parent) = parent {
            copy["parent"] = json!(parent);
        }
        let copied = s.ok("archive-copy", copy);
        assert_eq!(copied["commit"]["status"], "committed");
        assert!(s.read(path).contains(if section { "child001" } else { ID }));
        s.refuses(s.request(), "move_refused", Some("archived_id"));
    }
}

#[test]
fn replay_current_state_and_entire_retry_key() {
    let s = MoveStore::new("tight", false);
    let request = s.request();
    let landed = s.ok("move", request.clone());
    let original = s.read(B);
    for (text, current) in [
        (format!("{original}\nDirty neighbour.\n"), "matches"),
        (original.replace("Context.", "Edited context."), "changed"),
        (MoveStore::literal("tight", "destination", false), "absent"),
    ] {
        s.put(B, &text);
        let before = s.snapshot();
        let out = s.ok("move", request.clone());
        assert_eq!(out["status"], "unchanged");
        assert_eq!(out["commit"]["op"], landed["commit"]["op"]);
        assert_eq!(out["commit"]["sha"], landed["commit"]["sha"]);
        assert_eq!(out["current"], current);
        assert_eq!(s.snapshot(), before);
    }
    s.put(B, &original);
    for (field, value) in [
        ("source", json!("tracking/TRACKING_WIDGETS.md")),
        ("destination", json!("tracking/TRACKING_DOCS.md")),
        ("id", json!("absent01")),
        ("guard", json!("other")),
        ("typed_guard", json!("other")),
        ("collection", json!("other")),
        ("under", json!("Action")),
    ] {
        let mut retry = request.clone();
        retry[field] = value;
        s.refuses(retry, "not_found", None);
    }
    let mut retry = request;
    retry["message"] = json!("Different attribution");
    retry["session"] = json!("other session");
    s.ok("move", retry);
}

#[test]
fn replay_without_receipt_is_not_found() {
    let s = MoveStore::new("tight", false);
    let mut request = s.request();
    request["id"] = json!("absent01");
    s.refuses(request, "not_found", None);
}

#[test]
fn missing_birth_time_refuses_without_side_effects() {
    let s = MoveStore::new("tight", false);
    let before = s.snapshot();
    let (code, out) = s.run(
        "move",
        s.request(),
        &[("RIVET_TEST_FAULT", "move_fence_birth".into())],
    );
    assert_eq!(code, 2, "{out}");
    assert_eq!(out["error"]["code"], "move_refused", "{out}");
    assert_eq!(out["error"]["move_reason"], "fence_unsupported", "{out}");
    assert_eq!(s.snapshot(), before);
}

#[test]
fn missing_no_replace_rename_refuses_before_publication_without_side_effects() {
    let s = MoveStore::new("tight", false);
    let before = s.snapshot();
    let (code, out) = s.run(
        "move",
        s.request(),
        &[("RIVET_TEST_FAULT", "move_noreplace_rename=EINVAL".into())],
    );
    assert_eq!(code, 2, "{out}");
    assert_eq!(out["error"]["code"], "move_refused", "{out}");
    assert_eq!(out["error"]["move_reason"], "fence_unsupported", "{out}");
    assert!(out.get("pending").is_none(), "{out}");
    assert_eq!(s.snapshot(), before);
    let fences = std::fs::read_dir(s.root.join(".git"))
        .unwrap()
        .filter(|e| {
            let name = e.as_ref().unwrap().file_name();
            name.to_string_lossy().starts_with("rivet-fence-")
        })
        .count();
    assert_eq!(fences, 0);
}

#[test]
fn missing_no_replace_rename_leaves_the_move_pending_with_its_staging_entry() {
    // The backstop: no-replace rename is proved before publication, but the
    // fence's own rename is then refused as unsupported, after the entry is
    // created, so the move ends pending and leaves the entry for the operator.
    let s = MoveStore::new("tight", false);
    let (a, b, head, index, receipts, _, _) = s.snapshot();
    let (code, out) = s.run(
        "move",
        s.request(),
        &[("RIVET_TEST_FAULT", "move_fence_noreplace".into())],
    );
    assert_eq!(code, 2, "{out}");
    assert_eq!(out["error"]["code"], "commit_refused", "{out}");
    let (a2, b2, head2, index2, receipts2, records, locked) = s.snapshot();
    assert_eq!(
        (a2, b2, head2, index2, receipts2),
        (a, b, head, index, receipts)
    );
    assert!(!locked);
    assert_eq!(records.len(), 1, "{records:?}");
    let op = records[0]["op"].as_str().unwrap();
    assert_eq!(out["pending"], op, "{out}");
    let message = out["error"]["message"].as_str().unwrap();
    assert!(
        message.contains("no-replace rename is unsupported"),
        "{message}"
    );
    assert!(message.contains(&format!("rivet-fence-{op}")), "{message}");
    let entry = s.root.join(format!(".git/rivet-fence-{op}"));
    assert_eq!(std::fs::read(entry).unwrap(), b"");
}

#[test]
fn placement_uses_the_selected_members_last_collection_list() {
    for under in [None, Some("Batch")] {
        let s = MoveStore::new("tight", false);
        let mut config = s.config();
        config["collections"]["actions"]["subsections"] = json!(true);
        s.set_config(config);
        let b = s.read(B).replace(
            "## Log",
            "### Batch\n\n- [x] **Batch item.** Kept. <!-- stay:batch001 subhash=sha256:000000000000 -->\n\n## Log",
        );
        s.put(B, &b);
        s.refresh_fixture(B);
        s.commit();
        s.stamp(B);
        s.commit();
        let mut request = s.request();
        if let Some(under) = under {
            request["under"] = json!(under);
        }
        s.ok("move", request);
        let doc = Document::parse(&s.read(B)).unwrap();
        let moved = doc.resolve(&Selector::Id { id: ID.into() }).unwrap();
        let Target::Item { list, .. } = moved else {
            panic!("item")
        };
        let owner = doc.section_of(doc.lists[list].items[0].span.start).unwrap();
        assert_eq!(
            doc.sections[owner].path.last().unwrap(),
            under.unwrap_or("Action")
        );
        assert_eq!(doc.lists[list].items.len(), 2);
        let old = doc
            .resolve(&Selector::Id {
                id: if under.is_some() {
                    "batch001"
                } else {
                    "dest0001"
                }
                .into(),
            })
            .unwrap();
        assert!(matches!(old,Target::Item{list:l,..} if l==list));
    }
}

#[test]
fn foreign_nested_stay_refuses() {
    let s = MoveStore::new("loose", false);
    s.put(
        A,
        &s.read(A).replace(
            "\n\n- [x] **Keep.",
            "\n  - Nested <!-- stay:foreign1 -->\n\n- [x] **Keep.",
        ),
    );
    s.refresh_fixture(A);
    s.commit();
    s.refuses(s.request(), "move_refused", Some("not_native"));
}

#[test]
fn malformed_last_child_evidence_refuses_import() {
    for attrs in [
        "subhash=invalid",
        "subhash=\"sha256:22a1636f7e2e\"",
        "subhash=sha256:22a1636f7e2e future=value",
        "subhash=sha256:22a1636f7e2e subhash=invalid",
    ] {
        let s = MoveStore::new("tight", false);
        let source = s.read(A).replace(
            "stay:child002 subhash=sha256:22a1636f7e2e",
            &format!("stay:child002 {attrs}"),
        );
        s.put(A, &source);
        s.commit();
        s.refuses(s.request(), "move_refused", Some("not_native"));
        let request = s.request();
        let before = s.snapshot();
        let (code,out)=s.run("archive-copy",json!({"source":A,"id":ID,"guard":request["guard"],"path":"archive/closed.md","select":{"kind":"heading_path","path":["Closed"]},"destination_guard":"unused"}),&[]);
        assert_eq!(code, 2, "{out}");
        assert_eq!(out["error"]["code"], "archive_copy_refused", "{out}");
        assert!(
            out["error"]["message"]
                .as_str()
                .unwrap()
                .contains("valid child evidence"),
            "{out}"
        );
        assert_eq!(s.snapshot(), before);
    }
}

#[test]
fn malformed_or_duplicate_move_trailers_in_history_refuse_explicitly() {
    for kind in ["duplicate", "spelling", "encoding"] {
        let s = MoveStore::new("tight", false);
        let request = s.request();
        s.ok("move", request.clone());
        let message = s.git(&["log", "-1", "--format=%B"]);
        let trailer = message
            .lines()
            .find(|l| l.starts_with("Rivet-Move: "))
            .unwrap();
        let message = match kind {
            "duplicate" => format!("{message}\n{trailer}\n"),
            "spelling" => message.replace("Rivet-Move:", "rivet-move:"),
            _ => message.replace(trailer, "Rivet-Move: invalid*base64"),
        };
        s.put(".test-home/amended-message", &message);
        s.git(&[
            "commit",
            "--amend",
            "--no-verify",
            "-q",
            "-F",
            ".test-home/amended-message",
        ]);
        s.refuses(request, "invalid_request", None);
    }
}
