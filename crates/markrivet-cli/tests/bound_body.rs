mod common;
use common::*;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

fn fixture() -> (PathBuf, Value) {
    let root = project("bound-body");
    let baseline: Value =
        serde_json::from_str(include_str!("fixtures/guards_before_binding.json")).unwrap();
    std::fs::write(
        root.join(".rivet/project.json"),
        baseline["config"].to_string(),
    )
    .unwrap();
    std::fs::write(
        root.join(".rivet/schemas/task.json"),
        baseline["schema_bytes"].as_str().unwrap(),
    )
    .unwrap();
    std::fs::write(root.join("T.md"), baseline["source"].as_str().unwrap()).unwrap();
    (root, baseline)
}
fn get(root: &Path) -> Value {
    let (code, got) = rivet(
        root,
        "get",
        json!({"path":"T.md","select":{"kind":"id","id":"Item0001"}}),
    );
    assert_eq!(code, 0, "{got}");
    got
}
fn set(root: &Path) -> Value {
    let got = get(root);
    json!({"path":"T.md","select":{"kind":"id","id":"Item0001"},"guard":got["revision"],"collection":"tasks","typed_guard":got["typed"]["tasks"]["revision"],"content":"[x] **One.** Changed."})
}
fn scoped(root: &Path) -> Value {
    let (code, map) = rivet(root, "map", json!({"path":"T.md"}));
    assert_eq!(code, 0, "{map}");
    let collection = map["collections"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "tasks")
        .unwrap();
    json!({"path":"T.md","guard":map["revision"],"collection":"tasks","checkbox_only":true,"collection_guard":collection["revision"]})
}
fn refuse(root: &Path, command: &str, request: Value, error: &str) {
    let before = read(root, "T.md");
    let (code, out) = rivet(root, command, request);
    assert_eq!(code, 2, "{out}");
    assert_eq!(out["error"]["code"], error, "{out}");
    assert_eq!(read(root, "T.md"), before);
}
fn change_config(root: &Path, key: &str, value: Value) {
    let path = root.join(".rivet/project.json");
    let mut config: Value = serde_json::from_str(&read(root, ".rivet/project.json")).unwrap();
    config["collections"]["tasks"][key] = value;
    std::fs::write(path, config.to_string()).unwrap();
}
#[test]
fn native_guards_are_byte_equal_and_old_typed_guards_refuse() {
    let (root, before) = fixture();
    for row in before["native"].as_array().unwrap() {
        let (code, got) = rivet(&root, "get", json!({"path":"T.md","select":row["select"]}));
        assert_eq!(code, 0, "{got}");
        assert_eq!(got["revision"], row["revision"]);
    }
    let got = get(&root);
    assert_ne!(got["typed"]["tasks"]["revision"], before["typed_item"]);
    refuse(
        &root,
        "update-item",
        json!({"path":"T.md","collection":"tasks","id":"Item0001","guard":before["typed_item"],"set":{"done":true}}),
        "guard_conflict",
    );
    refuse(
        &root,
        "insert-item",
        json!({"path":"T.md","collection":"tasks","guard":before["typed_collection"],"position":"end","fields":{"title":"New"}}),
        "guard_conflict",
    );
}
#[test]
fn every_binding_field_conflicts_for_both_typed_revisions() {
    for descendants in [false, true] {
        for (key, value) in [
            ("level", Value::Null),
            ("files", json!(["*.md"])),
            ("section", json!("Renamed")),
            ("subsections", json!(!descendants)),
            ("exclude", json!(["Ignored"])),
            ("archive", json!(["archive/*.md"])),
        ] {
            if key == "exclude" && !descendants {
                continue;
            }
            let (root, _) = fixture();
            change_config(&root, "subsections", json!(descendants));
            let item = get(&root);
            let write = set(&root);
            let stamp = scoped(&root);
            change_config(&root, key, value);
            if key == "section" {
                std::fs::write(
                    root.join("T.md"),
                    read(&root, "T.md").replace("## Work", "## Renamed"),
                )
                .unwrap();
            }
            let current = get(&root);
            assert_eq!(current["revision"], item["revision"], "{key}");
            assert_ne!(
                current["typed"]["tasks"]["revision"], item["typed"]["tasks"]["revision"],
                "{key}"
            );
            refuse(&root, "set", write, "guard_conflict");
            // Section rename changes document bytes too; the remaining probes
            // isolate the collection fingerprint from the native document guard.
            refuse(&root, "stamp", stamp, "guard_conflict");
        }
    }
}
#[test]
fn binding_sets_and_explicit_defaults_are_canonical() {
    let (root, _) = fixture();
    change_config(&root, "files", json!(["T.md", "other/*.md"]));
    change_config(
        &root,
        "archive",
        json!(["archive/a/*.md", "archive/b/*.md"]),
    );
    change_config(&root, "subsections", json!(true));
    change_config(&root, "exclude", json!(["Ignored", "Plans"]));
    let item = get(&root);
    let original = scoped(&root);
    change_config(&root, "files", json!(["other/*.md", "T.md", "T.md"]));
    change_config(
        &root,
        "archive",
        json!(["archive/b/*.md", "archive/a/*.md", "archive/b/*.md"]),
    );
    change_config(&root, "exclude", json!(["Plans", "Ignored", "Plans"]));
    assert_eq!(get(&root)["typed"], item["typed"]);
    assert_eq!(
        scoped(&root)["collection_guard"],
        original["collection_guard"]
    );
    let (root, _) = fixture();
    change_config(&root, "level", Value::Null);
    let item = get(&root);
    let original = scoped(&root);
    let mut config: Value = serde_json::from_str(&read(&root, ".rivet/project.json")).unwrap();
    config["collections"]["tasks"]
        .as_object_mut()
        .unwrap()
        .remove("level");
    config["collections"]["tasks"]["subsections"] = json!(false);
    config["collections"]["tasks"]["exclude"] = json!([]);
    config["collections"]["tasks"]["archive"] = json!([]);
    std::fs::write(root.join(".rivet/project.json"), config.to_string()).unwrap();
    assert_eq!(get(&root)["typed"], item["typed"]);
    assert_eq!(
        scoped(&root)["collection_guard"],
        original["collection_guard"]
    );
}
#[test]
fn bound_set_preserves_outside_bytes_and_attached_stays() {
    let (root, _) = fixture();
    stamp(&root, "T.md");
    let before = read(&root, "T.md");
    let request = set(&root);
    let (code, out) = rivet(&root, "set", request);
    assert_eq!(code, 0, "{out}");
    let after = read(&root, "T.md");
    let old = markrivet_core::Document::parse(&before).unwrap();
    let new = markrivet_core::Document::parse(&after).unwrap();
    let select = markrivet_core::Selector::Id {
        id: "Item0001".into(),
    };
    let a = old.target_span(old.resolve(&select).unwrap()).0;
    let b = new.target_span(new.resolve(&select).unwrap()).0;
    // Exact outside bytes, allowing only the evidence digest values that the
    // normal verifier refreshes on ancestors of the edited item.
    let mask = |source: &str| {
        let mut text = source.to_string();
        let mut offset = 0;
        while let Some(start) = text[offset..].find("hash=sha256:") {
            let start = offset + start + "hash=sha256:".len();
            let len = text[start..]
                .bytes()
                .take_while(u8::is_ascii_hexdigit)
                .count();
            text.replace_range(start..start + len, &"0".repeat(len));
            offset = start + len;
        }
        text
    };
    assert_eq!(mask(&before[..a.start]), mask(&after[..b.start]));
    assert_eq!(mask(&before[a.end..]), mask(&after[b.end..]));
    for (id, owners) in &old.ids {
        assert_eq!(new.ids.get(id), Some(owners), "{id}");
    }
    let got = get(&root);
    assert!(got["content"].as_str().unwrap().contains("Changed."));
    assert_eq!(got["typed"]["tasks"]["fields"]["done"], true);
}
#[test]
fn bound_set_rejects_partial_preconditions_targets_and_invalid_bodies() {
    let (root, _) = fixture();
    for key in ["collection", "typed_guard"] {
        let mut request = set(&root);
        request.as_object_mut().unwrap().remove(key);
        refuse(&root, "set", request, "invalid_request");
    }
    for (key, value) in [
        ("part", json!("body")),
        ("content", json!(3)),
        ("select", json!({"kind":"document"})),
    ] {
        let mut request = set(&root);
        request[key] = value;
        refuse(&root, "set", request, "invalid_request");
    }
    for content in ["[ ] No bold title.", "**One.** no checkbox"] {
        let mut request = set(&root);
        request["content"] = json!(content);
        refuse(&root, "set", request, "invalid_item");
    }
    let mut request = set(&root);
    request["typed_guard"] = json!("rivet:v1:action:00000000000000000000000000000000");
    refuse(&root, "set", request, "guard_conflict");
    let mut request = set(&root);
    request["guard"] = json!("rivet:v1:item:00000000000000000000000000000000");
    refuse(&root, "set", request, "guard_conflict");
    let request = set(&root);
    std::fs::write(
        root.join("T.md"),
        read(&root, "T.md").replace("## Work", "## Elsewhere"),
    )
    .unwrap();
    refuse(&root, "set", request, "collection_conflict");
}
#[test]
fn schema_changes_refuse_bound_set_and_scoped_stamp() {
    let (root, _) = fixture();
    let write = set(&root);
    let stamp = scoped(&root);
    let path = root.join(".rivet/schemas/task.json");
    std::fs::write(
        path,
        format!("{}\n", read(&root, ".rivet/schemas/task.json")),
    )
    .unwrap();
    refuse(&root, "set", write, "guard_conflict");
    refuse(&root, "stamp", stamp, "guard_conflict");
}
#[test]
fn scoped_stamp_preserves_audit_prose_and_existing_attachments() {
    let (root, _) = fixture();
    let audit = (0..23)
        .map(|n| {
            if n < 18 {
                format!("- Audit {n}. <!-- stay:Audit{n:03} subhash=sha256:111111111111 -->\n")
            } else {
                format!("- Audit {n}.\n")
            }
        })
        .collect::<String>();
    let mixed = "- [ ] **New.**\n- Ordinary prose.\n- [x] **Kept.** <!-- stay:Kept0001 subhash=sha256:111111111111 -->\n";
    let source =
        format!("# Synthetic\n\n## Work\n\n{mixed}\nAudit:\n\n{audit}\n## Log\n\n- [ ] Outside.\n");
    std::fs::write(root.join("T.md"), &source).unwrap();
    let request = scoped(&root);
    let (code, out) = rivet(&root, "stamp", request);
    assert_eq!(code, 0, "{out}");
    let after = read(&root, "T.md");
    assert!(after.starts_with("# Synthetic\n\n## Work\n\n"));
    assert!(after.contains("- Ordinary prose.\n"));
    assert!(after.ends_with("\n## Log\n\n- [ ] Outside.\n"));
    // Existing audit hashes are already synthetic drift; no audit owner changes,
    // so even their original digest text must survive exactly.
    assert!(after.contains(&audit));
    let minted = out["ops"][0]["minted"].as_array().unwrap();
    assert_eq!(
        minted.iter().filter(|m| m["kind"] == "item").count(),
        1,
        "{out}"
    );
    assert_eq!(
        minted.iter().filter(|m| m["kind"] == "list").count(),
        1,
        "{out}"
    );
    assert!(minted.iter().all(|m| m["kind"] != "section"));
    let (code, kept) = rivet(
        &root,
        "get",
        json!({"path":"T.md","select":{"kind":"id","id":"Kept0001"}}),
    );
    assert_eq!(code, 0, "{kept}");
    assert!(
        kept["revision"]
            .as_str()
            .unwrap()
            .starts_with("rivet:v1:item:")
    );
    let (code, out) = rivet(&root, "stamp", scoped(&root));
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["status"], "unchanged");
    assert_eq!(read(&root, "T.md"), after);
}
#[test]
fn scoped_stamp_rejects_partial_scope_foreign_collections_and_unsupported_carriers() {
    let (root, _) = fixture();
    for key in ["collection", "collection_guard", "checkbox_only"] {
        let mut request = scoped(&root);
        request.as_object_mut().unwrap().remove(key);
        refuse(&root, "stamp", request, "invalid_request");
    }
    let mut request = scoped(&root);
    request["select"] = json!({"kind":"document"});
    refuse(&root, "stamp", request, "invalid_request");
    let mut request = scoped(&root);
    request["collection"] = json!("missing");
    refuse(&root, "stamp", request, "collection_conflict");
    std::fs::write(
        root.join("T.md"),
        "## Work\n\n- [ ] Bad < carrier.\n- [ ] **Good.**\n",
    )
    .unwrap();
    refuse(&root, "stamp", scoped(&root), "stamp_refused");
}

#[test]
fn scoped_stamp_obeys_descendant_exclusions_and_other_collection_ownership() {
    let (root, _) = fixture();
    let mut config: Value = serde_json::from_str(&read(&root, ".rivet/project.json")).unwrap();
    config["collections"]["tasks"]["subsections"] = json!(true);
    config["collections"]["tasks"]["exclude"] = json!(["Excluded"]);
    config["collections"]["other"] =
        json!({"schema":"task@1","files":["T.md"],"section":"Other","level":2});
    std::fs::write(root.join(".rivet/project.json"), config.to_string()).unwrap();
    let source = "## Work\n\n- [ ] Legacy invalid title.\n\n### Batch\n\n- [x] **Child.**\n- Prose.\n\n### Excluded\n\n- [ ] **Hidden.**\n\n## Other\n\n- [ ] **Other.**\n";
    std::fs::write(root.join("T.md"), source).unwrap();
    let (code, out) = rivet(&root, "stamp", scoped(&root));
    assert_eq!(code, 0, "{out}");
    let after = read(&root, "T.md");
    assert!(after.contains("- Prose.\n"));
    assert_eq!(
        after.split("### Excluded").nth(1),
        source.split("### Excluded").nth(1)
    );
    assert_eq!(
        out["ops"][0]["minted"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|m| m["kind"] == "item")
            .count(),
        2,
        "{out}"
    );
    assert!(!after.contains("hash=sha256:") || after.contains("subhash=sha256:"));
}

#[test]
fn schema_key_change_conflicts_even_when_membership_is_unchanged() {
    let (root, _) = fixture();
    let request = set(&root);
    let stamp = scoped(&root);
    let mut schema: Value = serde_json::from_str(&read(&root, ".rivet/schemas/task.json")).unwrap();
    schema["version"] = json!(2);
    std::fs::write(root.join(".rivet/schemas/task.json"), schema.to_string()).unwrap();
    change_config(&root, "schema", json!("task@2"));
    refuse(&root, "set", request, "guard_conflict");
    refuse(&root, "stamp", stamp, "guard_conflict");
}

#[test]
fn bound_set_shares_native_item_normalization_for_wrapped_crlf_titles() {
    for document_newline in ["\n", "\r\n"] {
        for content_newline in ["\n", "\r\n"] {
            for ending in ["", "\n", "\r\n"] {
                let mut results = vec![];
                for bound in [false, true] {
                    let (root, _) = fixture();
                    let source = read(&root, "T.md").replace('\n', document_newline);
                    std::fs::write(root.join("T.md"), source).unwrap();
                    let mut request = set(&root);
                    request["content"] =
                        json!(format!("[x] **Line{content_newline}Two.** Body.{ending}"));
                    if !bound {
                        request.as_object_mut().unwrap().remove("collection");
                        request.as_object_mut().unwrap().remove("typed_guard");
                    }
                    let (code, out) = rivet(&root, "set", request);
                    assert_eq!(
                        code, 0,
                        "bound={bound}, doc={document_newline:?}, input={content_newline:?}, ending={ending:?}: {out}"
                    );
                    results.push(read(&root, "T.md"));
                }
                assert_eq!(
                    results[0], results[1],
                    "native and bound set must normalize identically"
                );
            }
        }
    }
}
