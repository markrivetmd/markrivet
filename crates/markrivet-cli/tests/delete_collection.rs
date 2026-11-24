mod common;
use common::*;
use markrivet_core::{Document, Selector, document::Target};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

fn fixture() -> PathBuf {
    let root = project("delete-collection");
    let config_path = root.join(".rivet/project.json");
    let mut config: Value = serde_json::from_slice(&std::fs::read(&config_path).unwrap()).unwrap();
    config["collections"]["actions"]["subsections"] = json!(true);
    config["collections"]["actions"]["exclude"] = json!(["Plans"]);
    std::fs::write(config_path, serde_json::to_vec(&config).unwrap()).unwrap();
    std::fs::write(root.join(WIDGETS), "# Synthetic\r\n\r\nPrefix.\r\n\r\n## Action\r\n\r\n- [ ] Keep. <!-- stay:KeepOne1 subhash=sha256:111111111111 -->\r\n- [x] Remove. <!-- stay:DropOne1 subhash=sha256:111111111111 -->\r\n\r\n### Batch\r\n\r\n- [ ] Batch keep. <!-- stay:BatchKe1 subhash=sha256:111111111111 -->\r\n- [x] Batch remove. <!-- stay:BatchRm1 subhash=sha256:111111111111 -->\r\n\r\n### Plans\r\n\r\n#### Hidden\r\n\r\n- [ ] Hidden keep. <!-- stay:HideKe01 subhash=sha256:111111111111 -->\r\n- [x] Hidden remove. <!-- stay:HideRm01 subhash=sha256:111111111111 -->\r\n\r\n## Log\r\n\r\nUnrelated suffix.\r\n").unwrap();
    // Refresh synthetic evidence before taking the byte-preservation snapshot.
    stamp(&root, WIDGETS);
    root
}

fn blank_hashes(text: &str) -> String {
    let mut out = text.to_string();
    for (at, _) in text
        .match_indices("sha256:")
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
    {
        out.replace_range(at + 7..at + 19, "000000000000");
    }
    out
}

fn request(root: &Path, id: &str) -> Value {
    json!({"path":WIDGETS,"select":{"kind":"id","id":id},
        "guard":item_guard(root,WIDGETS,id),"collection":"actions"})
}

#[test]
fn delete_collection_accepts_own_body_and_member_batches() {
    for id in ["DropOne1", "BatchRm1"] {
        let root = fixture();
        let before = read(&root, WIDGETS);
        let doc = Document::parse(&before).unwrap();
        let select = Selector::Id { id: id.into() };
        let Target::Item { list, item } = doc.resolve(&select).unwrap() else {
            panic!()
        };
        let span = doc.lists[list].items[item].span;
        let (code, out) = rivet(&root, "delete", request(&root, id));
        assert_eq!(code, 0, "{out}");
        assert_eq!(out["ops"][0]["dropped"], json!([id]));
        let after = read(&root, WIDGETS);
        // Evidence hashes may refresh, but every other byte outside the item
        // must survive, including CRLF and the excluded subtree.
        let blank = blank_hashes;
        let mut expected = before.clone();
        expected.replace_range(doc.lists[list].items[item - 1].span.end..span.end, "");
        assert_eq!(blank(&after), blank(&expected));
        let remaining = Document::parse(&after).unwrap();
        for marker in &doc.markers {
            let Some(kept_id) = marker.lexical.id.as_ref() else {
                continue;
            };
            if kept_id != id {
                let kept = Selector::Id {
                    id: kept_id.clone(),
                };
                assert_eq!(
                    doc.resolve(&kept).unwrap(),
                    remaining.resolve(&kept).unwrap()
                );
            }
        }
    }
}

#[test]
fn delete_collection_refuses_nonmembers_and_missing_bindings_without_writing() {
    for case in [
        "excluded",
        "unknown",
        "unbound",
        "missing-section",
        "section",
    ] {
        let root = fixture();
        let mut input = request(
            &root,
            if case == "excluded" {
                "HideRm01"
            } else {
                "DropOne1"
            },
        );
        match case {
            "unknown" => input["collection"] = json!("unknown"),
            "unbound" => {
                let path = root.join(".rivet/project.json");
                let mut config: Value =
                    serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
                config["collections"]["actions"]["files"] = json!(["other/*.md"]);
                std::fs::write(path, serde_json::to_vec(&config).unwrap()).unwrap();
            }
            "missing-section" => {
                let source = read(&root, WIDGETS).replace("## Action", "## Elsewhere");
                std::fs::write(root.join(WIDGETS), source).unwrap();
            }
            "section" => {
                input["select"] = json!({"kind":"heading_path","path":["Action"]});
                let (_, got) = rivet(
                    &root,
                    "get",
                    json!({"path":WIDGETS,"select":input["select"]}),
                );
                input["guard"] = got["revision"].clone();
            }
            _ => {}
        }
        let before = read(&root, WIDGETS);
        for preview in [true, false] {
            input["preview"] = json!(preview);
            let (code, out) = rivet(&root, "delete", input.clone());
            assert_eq!(code, 2, "{case}: {out}");
            assert_eq!(out["error"]["code"], "collection_conflict", "{case}: {out}");
            assert_eq!(read(&root, WIDGETS), before, "{case}");
        }
    }
}

#[test]
fn delete_collection_batch_checks_the_preimage_and_preserves_legacy_delete() {
    let root = fixture();
    let mut input = request(&root, "HideRm01");
    let before = read(&root, WIDGETS);
    let mut op = input.as_object().unwrap().clone();
    op.remove("path");
    op.insert("op".into(), json!("delete"));
    let (code, out) = rivet(&root, "write", json!({"path":WIDGETS,"ops":[op]}));
    assert_eq!(code, 2, "{out}");
    assert_eq!(out["error"]["code"], "collection_conflict");
    assert_eq!(read(&root, WIDGETS), before);
    input.as_object_mut().unwrap().remove("collection");
    let (code, out) = rivet(&root, "delete", input);
    assert_eq!(code, 0, "{out}");
}

#[test]
fn delete_collection_refuses_an_item_moved_into_an_excluded_batch_at_the_same_revision() {
    let root = fixture();
    let input = request(&root, "DropOne1");
    let source = read(&root, WIDGETS);
    let line = source
        .split_inclusive("\r\n")
        .find(|line| line.contains("stay:DropOne1"))
        .unwrap();
    let moved = source.replacen(line, "", 1);
    let moved = moved.replacen("#### Hidden\r\n", &format!("#### Hidden\r\n\r\n{line}"), 1);
    std::fs::write(root.join(WIDGETS), &moved).unwrap();
    assert_eq!(item_guard(&root, WIDGETS, "DropOne1"), input["guard"]);
    let (code, out) = rivet(&root, "delete", input);
    assert_eq!(code, 2, "{out}");
    assert_eq!(out["error"]["code"], "collection_conflict");
    assert_eq!(read(&root, WIDGETS), moved);
}
