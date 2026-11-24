use super::*;
use serde_json::Value;

fn corpus(name: &str) -> std::path::PathBuf {
    let root = project(name);
    for entry in std::fs::read_dir(root.join("tracking")).unwrap() {
        std::fs::remove_file(entry.unwrap().path()).unwrap();
    }
    std::fs::create_dir_all(root.join(".rivet/analytics")).unwrap();
    std::fs::create_dir_all(root.join(".rivet/views")).unwrap();
    put(
        &root,
        ".rivet/analytics/census.json",
        &json!({"version":1,"collection":"actions","fields":{
            "cluster":{"type":"enum","key":["cluster"],"values":["alpha","beta","empty"],"required":true},
            "status":{"type":"enum","key":["status"],"values":["active","parked"],"default":"active"},
            "note":{"type":"string","key":["note"],"nullable":true},
            "nested":{"type":"boolean","key":["info","enabled"]}
        }}),
    );
    write_doc(
        &root,
        "a.md",
        "---\ncluster: alpha\ninfo: {enabled: true}\n---\n## Action\n\n- [ ] `agent` **First**\n- [x] **Second**\n",
    );
    write_doc(
        &root,
        "b.md",
        "---\ncluster: beta\nstatus: parked\nnote: null\n---\n## Action\n\n- [ ] **Third**\n",
    );
    write_doc(
        &root,
        "empty.md",
        "---\ncluster: empty\n---\n## Action\n\nNothing queued.\n",
    );
    root
}
fn write_doc(root: &std::path::Path, name: &str, text: &str) {
    std::fs::write(root.join("tracking").join(name), text).unwrap();
}
fn put(root: &std::path::Path, path: &str, value: &Value) {
    std::fs::write(root.join(path), value.to_string()).unwrap();
}
fn query() -> Value {
    json!({"collection":"actions","analytics":"census"})
}
fn analyze(root: &std::path::Path, mut q: Value) -> Value {
    if q.get("collection").is_none() {
        q["collection"] = json!("actions");
        q["analytics"] = json!("census");
    }
    let (code, v) = rivet(root, "analyze", q);
    assert_eq!(code, 0, "{v}");
    v
}
#[test]
fn analytics_metadata_empty_documents() {
    let root = corpus("analytics-metadata");
    let items = analyze(&root, query());
    assert_eq!(items["total"], 3);
    assert_eq!(items["coverage"]["candidate_items"], 3);
    assert_eq!(items["coverage"]["addressable_items"], 0);
    let docs = analyze(&root, json!({"source":"documents"}));
    assert_eq!(docs["total"], 3);
    assert_eq!(docs["coverage"]["reads"], 3);
    assert_eq!(items["rows"][0]["fields"]["document.nested"]["value"], true);
    assert_eq!(
        items["rows"][0]["fields"]["document.status"]["origin"],
        "defaulted"
    );
    assert_eq!(docs["scope"]["analytics_version"], 1);
    let plain = analyze(&root, json!({"collection":"actions","source":"documents"}));
    assert!(plain["rows"][0]["fields"].get("document.cluster").is_none());
    let (code, _) = rivet(
        &root,
        "analyze",
        json!({"collection":"actions","group_by":["document.cluster"]}),
    );
    assert_eq!(code, 2);
    let (code, _) = rivet(
        &root,
        "analyze",
        json!({"collection":"actions","analytics":"missing"}),
    );
    assert_eq!(code, 2);
    let mut decl: Value =
        serde_json::from_str(&read(&root, ".rivet/analytics/census.json")).unwrap();
    decl["fields"]["path"] = json!({"type":"string","key":["path"]});
    put(&root, ".rivet/analytics/census.json", &decl);
    assert_eq!(rivet(&root, "analyze", query()).0, 2);
}
#[test]
fn analytics_defaults_provenance_legacy_equivalence() {
    let root = corpus("analytics-defaults");
    let clause = json!({"field":"actor","op":"eq","value":"human:owner"});
    let legacy = rivet(
        &root,
        "query",
        json!({"collection":"actions","where":[clause]}),
    )
    .1;
    let new = analyze(
        &root,
        json!({"where":[{"field":"item.actor","op":"eq","value":"human:owner"}]}),
    );
    assert_eq!(legacy["total"], new["total"]);
    assert_eq!(new["total"], 2);
    assert_eq!(
        new["rows"][0]["fields"]["item.actor"]["origin"],
        "defaulted"
    );
    let origins = analyze(
        &root,
        json!({"where":[{"field":"item.actor.origin","op":"eq","value":"authored"}],"fields":["item.actor.origin"]}),
    );
    assert_eq!(origins["total"], 1);
    assert_eq!(
        origins["rows"][0]["fields"]["item.actor.origin"]["value"],
        "authored"
    );
    let present = analyze(
        &root,
        json!({"where":[{"field":"item.actor","op":"exists"}]}),
    );
    assert_eq!(present["total"], 3);
    let groups = analyze(
        &root,
        json!({"group_by":["item.actor"],"measures":{"n":{"op":"count"}}}),
    );
    assert_eq!(groups["groups"][0]["base_rows"], 1);
    assert_eq!(groups["groups"][1]["base_rows"], 2);
}
#[test]
fn analytics_optional_required_null_and_default_errors() {
    let root = corpus("analytics-presence");
    let v = analyze(&root, query());
    assert_eq!(
        v["rows"][0]["fields"]["document.note"]["presence"],
        "missing"
    );
    assert_eq!(
        v["rows"][2]["fields"]["document.note"]["value"],
        Value::Null
    );
    assert!(
        v["rows"][2]["fields"]["document.note"]
            .get("value")
            .is_some()
    );
    write_doc(&root, "bad.md", "## Action\n\n- [ ] **Bad**\n");
    assert_eq!(rivet(&root, "analyze", query()).0, 2);
    let v = analyze(
        &root,
        json!({"mode":"diagnostic","where":[{"field":"document.cluster","op":"missing"}]}),
    );
    assert_eq!(v["total"], 1);
    let e = &v["rows"][0]["fields"]["document.cluster"];
    assert_eq!(e["presence"], "missing");
    assert_eq!(e["validity"], "invalid");
    let mut d: Value = serde_json::from_str(&read(&root, ".rivet/analytics/census.json")).unwrap();
    d["fields"]["status"]["default"] = json!("wrong");
    put(&root, ".rivet/analytics/census.json", &d);
    let (code, v) = rivet(
        &root,
        "analyze",
        json!({"collection":"actions","analytics":"census","mode":"diagnostic","paths":[]}),
    );
    assert_eq!(code, 2);
    assert!(v["error"]["message"].as_str().unwrap().contains("default"));
}
#[test]
fn analytics_invalid_sibling_unknown_and_strict_write() {
    let root = corpus("analytics-invalid-item");
    let mut s: Value =
        serde_json::from_str(&read(&root, ".rivet/schemas/tracker-action.json")).unwrap();
    s["fields"]["state"] =
        json!({"type":"enum","from":"label","label":"State","values":["ready","later"]});
    put(&root, ".rivet/schemas/tracker-action.json", &s);
    write_doc(
        &root,
        "bad.md",
        "---\ncluster: alpha\n---\n## Action\n\n- [ ] **Bad**\n  **State**: invalid\n",
    );
    let before = read(&root, "tracking/bad.md");
    assert_eq!(rivet(&root, "analyze", query()).0, 2);
    // A false filter cannot hide a candidate error in strict mode.
    assert_eq!(rivet(&root,"analyze",json!({"collection":"actions","analytics":"census","where":[{"field":"item.done","op":"eq","value":true}]})).0,2);
    let v = analyze(&root, json!({"mode":"diagnostic"}));
    assert_eq!(v["coverage"]["invalid_items"], 1);
    assert_eq!(v["total"], 4);
    let bad = v["rows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["path"] == "tracking/bad.md")
        .unwrap();
    assert_eq!(bad["fields"]["item.done"]["value"], false);
    assert_eq!(bad["fields"]["item.state"]["raw"], "invalid");
    let v = analyze(
        &root,
        json!({"mode":"diagnostic","where":[{"field":"item.state","op":"eq","value":"ready"}]}),
    );
    assert_eq!(v["total"], 0);
    assert_eq!(v["coverage"]["unknown_predicates"], 1);
    assert_eq!(v["coverage"]["nonmatching_rows"], 3);
    let v = analyze(
        &root,
        json!({"mode":"diagnostic","where":[{"field":"item.state","op":"eq","value":"ready"},{"field":"item.done","op":"eq","value":true}]}),
    );
    assert_eq!(v["coverage"]["unknown_predicates"], 0);
    assert_eq!(v["coverage"]["nonmatching_rows"], 4);
    let legacy = rivet(&root, "query", json!({"collection":"actions"})).1;
    assert_eq!(legacy["total"], 3);
    let guard =
        rivet(&root, "map", json!({"path":"tracking/bad.md"})).1["collections"][0]["revision"]
            .clone();
    assert!(guard.is_string());
    let (code, _) = rivet(
        &root,
        "insert-item",
        json!({"path":"tracking/bad.md","collection":"actions","guard":guard,"position":"end","fields":{"done":false,"title":"New","state":"invalid"}}),
    );
    assert_eq!(code, 2);
    assert_eq!(read(&root, "tracking/bad.md"), before);
    stamp(&root, "tracking/bad.md");
    let before = read(&root, "tracking/bad.md");
    let rows = analyze(
        &root,
        json!({"mode":"diagnostic","paths":["tracking/bad.md"]}),
    );
    let row = &rows["rows"][0];
    assert!(row["id"].is_string());
    let (code, v) = rivet(
        &root,
        "update-item",
        json!({"path":"tracking/bad.md","collection":"actions","id":row["id"],"guard":row["revision"],"set":{"done":true}}),
    );
    assert_eq!(code, 2, "{v}");
    assert_eq!(v["error"]["code"], "invalid_item");
    assert_eq!(read(&root, "tracking/bad.md"), before);
}
#[test]
fn analytics_frontmatter_recognition_and_unknown_presence() {
    let root = corpus("analytics-frontmatter");
    for text in [
        "---\ncluster: alpha\n\n---\n## Action\n\n- [ ] **Bad**\n",
        "\u{feff}---\ncluster: alpha\n## Action\n\n- [ ] **Bad**\n",
        "---\ncluster: &a alpha\n---\n## Action\n\n- [ ] **Bad**\n",
    ] {
        write_doc(&root, "bad.md", text);
        assert_eq!(rivet(&root, "analyze", query()).0, 2);
        let v = analyze(
            &root,
            json!({"mode":"diagnostic","paths":["tracking/bad.md"],"where":[{"field":"document.status","op":"exists"}]}),
        );
        assert_eq!(v["total"], 0);
        assert_eq!(v["coverage"]["unknown_predicates"], 1);
        let v = analyze(
            &root,
            json!({"mode":"diagnostic","paths":["tracking/bad.md"]}),
        );
        assert_eq!(
            v["rows"][0]["fields"]["document.status"]["presence"],
            "unknown"
        );
        assert_eq!(v["rows"][0]["fields"]["document.status"]["origin"], "none");
    }
    let mut decl: Value =
        serde_json::from_str(&read(&root, ".rivet/analytics/census.json")).unwrap();
    decl["fields"]["cluster"]["default"] = json!("empty");
    put(&root, ".rivet/analytics/census.json", &decl);
    for text in [
        "---\n\n## Action\n\n- [ ] **Rule**\n",
        "---\njust prose\n---\n## Action\n\n- [ ] **Rule**\n",
    ] {
        write_doc(&root, "bad.md", text);
        let v = analyze(&root, json!({"paths":["tracking/bad.md"]}));
        assert_eq!(
            v["rows"][0]["fields"]["document.cluster"]["origin"],
            "defaulted"
        );
    }
}
#[test]
fn analytics_missing_and_ambiguous_binding() {
    let root = corpus("analytics-binding");
    write_doc(&root, "missing.md", "---\ncluster: empty\n---\n## Notes\n");
    assert_eq!(rivet(&root, "analyze", query()).0, 2);
    let v = analyze(&root, json!({"missing_binding":"allow"}));
    assert_eq!(v["total"], 3);
    assert_eq!(v["coverage"]["missing_bindings"], 1);
    let v = analyze(
        &root,
        json!({"missing_binding":"allow","source":"documents"}),
    );
    assert_eq!(v["total"], 4);
    assert_eq!(v["coverage"]["missing_bindings"], 1);
    write_doc(
        &root,
        "ambiguous.md",
        "---\ncluster: empty\n---\n## Action\n\n- [ ] **One**\n\n## Action\n\n- [ ] **Two**\n",
    );
    assert_eq!(
        rivet(
            &root,
            "analyze",
            json!({"collection":"actions","analytics":"census","missing_binding":"allow"})
        )
        .0,
        2
    );
    let v = analyze(&root, json!({"mode":"diagnostic","source":"documents"}));
    assert_eq!(v["total"], 5);
    assert_eq!(v["coverage"]["ambiguous_bindings"], 1);
    let v = analyze(&root, json!({"mode":"diagnostic"}));
    assert_eq!(v["total"], 3);
}
#[test]
fn analytics_metadata_readonly_guard_isolation() {
    let root = corpus("analytics-guard");
    stamp(&root, "tracking/a.md");
    let (_, legacy) = rivet(
        &root,
        "query",
        json!({"collection":"actions","paths":["tracking/a.md"]}),
    );
    let row = &legacy["items"][0];
    let map = rivet(&root, "map", json!({"path":"tracking/a.md"})).1;
    let collection_guard = map["collections"][0]["revision"].clone();
    assert!(collection_guard.is_string());
    let mut decl: Value =
        serde_json::from_str(&read(&root, ".rivet/analytics/census.json")).unwrap();
    decl["fields"]["status"]["default"] = json!("parked");
    put(&root, ".rivet/analytics/census.json", &decl);
    put(
        &root,
        ".rivet/views/all.json",
        &json!({"version":1,"query":query()}),
    );
    let map2 = rivet(&root, "map", json!({"path":"tracking/a.md"})).1;
    assert_eq!(map2["collections"][0]["revision"], collection_guard);
    let (code, v) = rivet(
        &root,
        "update-item",
        json!({"path":"tracking/a.md","collection":"actions","id":row["id"],"guard":row["revision"],"set":{"done":true}}),
    );
    assert_eq!(code, 0, "{v}");
    let before = read(&root, "tracking/a.md");
    let (_, rows) = rivet(
        &root,
        "query",
        json!({"collection":"actions","paths":["tracking/a.md"]}),
    );
    let row = &rows["items"][0];
    let (code, _) = rivet(
        &root,
        "update-item",
        json!({"path":"tracking/a.md","collection":"actions","id":row["id"],"guard":row["revision"],"set":{"document.status":"active"}}),
    );
    assert_eq!(code, 2);
    assert_eq!(read(&root, "tracking/a.md"), before);
}
#[test]
fn analytics_duplicate_and_unstamped_identity() {
    let root = corpus("analytics-duplicate");
    stamp(&root, "tracking/a.md");
    let text = read(&root, "tracking/a.md");
    let lines: Vec<_> = text.lines().collect();
    let marker = lines
        .iter()
        .find(|l| l.contains("<!-- stay:") && l.contains("First"))
        .copied();
    // Copy a complete stamped item, retaining the same attached stay id.
    let first = lines.iter().position(|l| l.contains("**First**")).unwrap();
    let second = lines.iter().position(|l| l.contains("**Second**")).unwrap();
    let block = lines[first..second].join("\n");
    let _ = marker;
    write_doc(&root, "a.md", &format!("{text}\n{block}\n"));
    let v = analyze(&root, query());
    assert_eq!(v["total"], 4);
    assert!(v["coverage"]["duplicate_ids"].as_u64().unwrap() > 0);
    assert_eq!(v["coverage"]["addressable_items"], 1);
}
#[test]
fn analytics_native_filenames_and_exclusions() {
    use std::os::unix::ffi::OsStringExt;
    let root = corpus("analytics-native");
    let invalid = std::ffi::OsString::from_vec(b"bad\xff.md".to_vec());
    std::fs::write(root.join("tracking").join(invalid), "## Action\n").unwrap();
    std::fs::create_dir(
        root.join("tracking")
            .join(std::ffi::OsString::from_vec(b"dir\xfe".to_vec())),
    )
    .unwrap();
    write_doc(
        &root,
        "bad\u{fffd}.md",
        "---\ncluster: empty\n---\n## Action\n",
    );
    std::os::unix::fs::symlink(root.join("tracking/a.md"), root.join("tracking/link.md")).unwrap();
    std::fs::create_dir_all(root.join("tracking/target")).unwrap();
    std::fs::create_dir_all(root.join("tracking/node_modules")).unwrap();
    write_doc(&root, ".hidden.md", "## Action\n");
    let (code, refused) = rivet(
        &root,
        "analyze",
        json!({"collection":"actions","analytics":"census","source":"documents"}),
    );
    assert_eq!(code, 2, "{refused}");
    let v = analyze(&root, json!({"source":"documents","mode":"diagnostic"}));
    assert_eq!(v["total"], 4);
    let events = v["events"].as_array().unwrap();
    for reason in ["non_utf8", "symlink", "target", "node_modules", "hidden"] {
        assert!(
            events.iter().any(|e| e["reason"] == reason),
            "missing {reason}"
        );
    }
    assert!(
        events
            .iter()
            .any(|e| e["native_name_hex"] == "626164ff2e6d64")
    );
    assert!(
        events
            .iter()
            .any(|e| e["native_name_hex"] == "646972fe" && e["descendants_unknown"] == true)
    );
}
#[test]
fn analytics_group_counts_distinct_paging_and_top() {
    let root = corpus("analytics-groups");
    let q = json!({"group_by":["document.cluster"],"measures":{"actions":{"op":"count"},"documents":{"op":"count_distinct","field":"document.path"}},"limit":1});
    let v = analyze(&root, q.clone());
    assert_eq!(v["total"], 3);
    assert_eq!(v["total_groups"], 2);
    assert_eq!(v["omitted_groups"], 1);
    assert_eq!(v["groups"][0]["base_rows"], 2);
    assert_eq!(v["groups"][0]["measures"]["documents"]["count"], 1);
    let mut next = q.clone();
    next["offset"] = json!(1);
    let next = analyze(&root, next);
    assert_eq!(next["groups"][0]["base_rows"], 1);
    assert_eq!(
        v["returned_group_rows"].as_u64().unwrap() + next["returned_group_rows"].as_u64().unwrap(),
        3
    );
    let mut top = q;
    top["top"] = json!(1);
    let v = analyze(&root, top);
    assert_eq!(v["groups"][0]["base_rows"], 2);
    assert_eq!(v["total"], 3);
    let docs = analyze(
        &root,
        json!({"source":"documents","group_by":["document.cluster"],"measures":{"docs":{"op":"count"}}}),
    );
    assert_eq!(docs["total_groups"], 3);
    assert_eq!(docs["total"], 3);
}
#[test]
fn analytics_facet_state_partition_and_filter() {
    let root = corpus("analytics-facets");
    let mut decl: Value =
        serde_json::from_str(&read(&root, ".rivet/analytics/census.json")).unwrap();
    decl["fields"]["cluster"]["required"] = json!(false);
    decl["fields"]["cluster"]["nullable"] = json!(true);
    put(&root, ".rivet/analytics/census.json", &decl);
    for (name, head) in [
        ("missing.md", ""),
        ("null.md", "---\ncluster: null\n---\n"),
        ("invalid.md", "---\ncluster: missing\n---\n"),
        ("unknown.md", "---\ncluster: &a alpha\n---\n"),
    ] {
        write_doc(&root, name, &format!("{head}## Action\n\n- [ ] **Task**\n"));
    }
    let (code, duplicate) = rivet(
        &root,
        "analyze",
        json!({
            "collection":"actions", "analytics":"census", "mode":"diagnostic",
            "facets":["document.cluster","document.cluster"]
        }),
    );
    assert_eq!(code, 2);
    assert_eq!(duplicate["error"]["code"], "invalid_request");
    let v = analyze(
        &root,
        json!({"mode":"diagnostic","facets":["document.cluster"],"zero_enums":true}),
    );

    let distinct = analyze(
        &root,
        json!({"mode":"diagnostic","measures":{"keys":{"op":"count_distinct","field":"document.cluster"}}}),
    );
    assert_eq!(distinct["groups"][0]["measures"]["keys"]["count"], 2);
    assert_eq!(
        distinct["groups"][0]["measures"]["keys"]["excluded"],
        json!({"missing":1,"null":1,"invalid":1,"unsupported":1})
    );
    let buckets = v["facets"]["document.cluster"]["buckets"]
        .as_array()
        .unwrap();
    assert_eq!(
        buckets
            .iter()
            .map(|b| b["count"].as_u64().unwrap())
            .sum::<u64>(),
        7
    );
    for state in ["missing", "null", "invalid", "unsupported"] {
        assert_eq!(
            buckets.iter().find(|b| b["key"]["state"] == state).unwrap()["count"],
            1
        );
    }
    assert_eq!(
        buckets
            .iter()
            .find(|b| b["key"]["value"] == "empty")
            .unwrap()["count"],
        0
    );
    let v = analyze(
        &root,
        json!({"mode":"diagnostic","facets":["document.cluster"],"where":[{"field":"document.cluster","op":"eq","value":"alpha"}]}),
    );
    assert_eq!(v["total"], 2);
    assert_eq!(v["facets"]["document.cluster"]["buckets"][0]["count"], 2);
    assert_eq!(v["coverage"]["unknown_predicates"], 2);
}
#[test]
fn analytics_conditional_unknown_and_distinct_exclusions() {
    let root = corpus("analytics-condition");
    write_doc(
        &root,
        "bad.md",
        "---\ncluster: alpha\nstatus: wrong\n---\n## Action\n\n- [ ] **Bad**\n",
    );
    let v = analyze(
        &root,
        json!({"mode":"diagnostic","measures":{"n":{"op":"count"},"active":{"op":"count_where","where":[{"field":"document.status","op":"eq","value":"active"}]},"status":{"op":"count_distinct","field":"document.status"},"notes":{"op":"count_distinct","field":"document.note"}}}),
    );
    let m = &v["groups"][0]["measures"];
    assert_eq!(m["active"]["count"], 2);
    assert_eq!(m["active"]["false"], 1);
    assert_eq!(m["active"]["unknown"], 1);
    assert_eq!(m["status"]["count"], 2);
    assert_eq!(m["status"]["excluded"]["invalid"], 1);
    assert_eq!(m["notes"]["excluded"]["missing"], 3);
    assert_eq!(m["notes"]["excluded"]["null"], 1);
}
#[test]
fn analytics_declaration_selection_and_refusals() {
    let root = corpus("analytics-refusals");
    let d: Value = serde_json::from_str(&read(&root, ".rivet/analytics/census.json")).unwrap();
    put(&root, ".rivet/analytics/other.json", &d);
    let v = analyze(&root, query());
    assert_eq!(v["total"], 3);
    for q in [
        json!({"collection":"actions","group_by":["document.cluster"]}),
        json!({"collection":"actions","analytics":"census","group_by":["actor"]}),
        json!({"collection":"actions","analytics":"census","source":"documents","group_by":["item.actor"]}),
        json!({"collection":"actions","analytics":"census","facets":["document.path"]}),
        json!({"collection":"actions","analytics":"../census"}),
        json!({"collection":"actions","analytics":"census","limit":0}),
        json!({"collection":"actions","analytics":"census","paths":["../a.md"]}),
        json!({"collection":"actions","analytics":"census","where":[{"field":"item.done","op":"gt","value":false}]}),
    ] {
        assert_eq!(rivet(&root, "analyze", q).0, 2);
    }
    let mut mismatch = d;
    mismatch["collection"] = json!("other");
    put(&root, ".rivet/analytics/other.json", &mismatch);
    assert_eq!(
        rivet(
            &root,
            "analyze",
            json!({"collection":"actions","analytics":"other"})
        )
        .0,
        2
    );
}
#[test]
fn analytics_view_export_replay_and_live_changes() {
    let root = corpus("analytics-replay");
    let q = json!({"collection":"actions","analytics":"census","group_by":["document.cluster"],"measures":{"n":{"op":"count"},"docs":{"op":"count_distinct","field":"document.path"}}});
    put(
        &root,
        ".rivet/views/report.json",
        &json!({"version":1,"query":q}),
    );
    let (code, view) = rivet(&root, "run-view", json!({"view":"report"}));
    assert_eq!(code, 0, "{view}");
    let direct = analyze(&root, q.clone());
    assert_eq!(view["groups"], direct["groups"]);
    let (code, drill) = rivet(
        &root,
        "drilldown",
        json!({"query":q,"view":"report","fingerprint":view["fingerprint"],"group":view["groups"][0]["key"],"measure":"n"}),
    );
    assert_eq!(code, 0, "{drill}");
    assert_eq!(drill["partitions"]["included"].as_array().unwrap().len(), 2);
    let (code, export) = rivet(&root, "export", json!({"bundle":"frozen","view":"report"}));
    assert_eq!(code, 0, "{export}");
    let (_, replay) = rivet(&root, "replay", json!({"bundle":"frozen"}));
    assert_eq!(replay, export["report"]);
    write_doc(&root, "a.md", "changed after export\n");
    std::fs::remove_file(root.join(".rivet/project.json")).unwrap();
    std::fs::remove_file(root.join(".rivet/analytics/census.json")).unwrap();
    std::fs::remove_file(root.join(".rivet/views/report.json")).unwrap();
    let (code, replay) = rivet(&root, "replay", json!({"bundle":"frozen"}));
    assert_eq!(code, 0, "{replay}");
    assert_eq!(replay, export["report"]);
    assert_eq!(
        rivet(&root, "export", json!({"bundle":"../bad","query":q})).0,
        2
    );
}
#[test]
fn analytics_measure_drilldown_and_changed_data() {
    let root = corpus("analytics-drilldown");
    write_doc(
        &root,
        "bad.md",
        "---\ncluster: alpha\nstatus: wrong\n---\n## Action\n\n- [ ] **Bad**\n",
    );
    let q = json!({"collection":"actions","analytics":"census","mode":"diagnostic","group_by":["document.cluster"],"measures":{"n":{"op":"count"},"active":{"op":"count_where","where":[{"field":"document.status","op":"eq","value":"active"}]},"docs":{"op":"count_distinct","field":"document.path"}}});
    let report = analyze(&root, q.clone());
    let key = report["groups"][0]["key"].clone();
    let fp = report["fingerprint"].clone();
    for (measure, count) in [("n", 3), ("active", 2), ("docs", 2)] {
        let (code, v) = rivet(
            &root,
            "drilldown",
            json!({"query":q,"fingerprint":fp,"group":key,"measure":measure}),
        );
        assert_eq!(code, 0, "{v}");
        match measure {
            "n" => assert_eq!(v["partitions"]["included"].as_array().unwrap().len(), count),
            "active" => {
                assert_eq!(v["partitions"]["true"].as_array().unwrap().len(), count);
                assert_eq!(v["partitions"]["unknown"].as_array().unwrap().len(), 1)
            }
            _ => {
                assert_eq!(v["keys"].as_array().unwrap().len(), count);
                assert_eq!(
                    v["keys"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|k| k["rows"].as_array().unwrap().len())
                        .sum::<usize>(),
                    3
                )
            }
        }
    }
    let (code, v) = rivet(&root, "export", json!({"bundle":"frozen","query":q}));
    assert_eq!(code, 0, "{v}");
    assert_eq!(v["report"]["fingerprint"], fp);
    let mut d: Value = serde_json::from_str(&read(&root, ".rivet/analytics/census.json")).unwrap();
    d["fields"]["status"]["default"] = json!("parked");
    put(&root, ".rivet/analytics/census.json", &d);
    let (code, v) = rivet(
        &root,
        "drilldown",
        json!({"query":q,"fingerprint":fp,"group":key,"measure":"n"}),
    );
    assert_eq!(code, 2);
    assert_eq!(v["error"]["code"], "external_change");
    let (code, v) = rivet(
        &root,
        "drilldown",
        json!({"query":q,"fingerprint":fp,"group":key,"measure":"n","bundle":"frozen"}),
    );
    assert_eq!(code, 0, "{v}");
    assert_eq!(v["partitions"]["included"].as_array().unwrap().len(), 3);
}
#[test]
fn analytics_failed_file_replay_and_byte_preservation() {
    let root = corpus("analytics-failed-replay");
    std::fs::write(root.join("tracking/bad.md"), [0xff, 0xfe]).unwrap();
    let before = std::fs::read(root.join("tracking/bad.md")).unwrap();
    let q = json!({"collection":"actions","analytics":"census","mode":"diagnostic"});
    let (code, v) = rivet(&root, "export", json!({"bundle":"badbytes","query":q}));
    assert_eq!(code, 0, "{v}");
    assert_eq!(v["report"]["coverage"]["failed_files"], 1);
    assert_eq!(std::fs::read(root.join("tracking/bad.md")).unwrap(), before);
    std::fs::remove_file(root.join("tracking/bad.md")).unwrap();
    let (code, replay) = rivet(&root, "replay", json!({"bundle":"badbytes"}));
    assert_eq!(code, 0, "{replay}");
    assert_eq!(replay, v["report"]);
    write_doc(&root, "oversize.md", &"x".repeat(8 * 1024 * 1024 + 1));
    let (code, v) = rivet(&root, "export", json!({"bundle":"oversize","query":q}));
    assert_eq!(code, 0, "{v}");
    assert_eq!(v["report"]["coverage"]["unreadable_files"], 1);
    let (code, replay) = rivet(&root, "replay", json!({"bundle":"oversize"}));
    assert_eq!(code, 0, "{replay}");
    assert_eq!(replay, v["report"]);
    let frozen = read(&root, "tracking/a.md");
    assert_eq!(rivet(&root, "analyze", query()).0, 2);
    assert_eq!(read(&root, "tracking/a.md"), frozen);
}
#[test]
fn analytics_bundle_corruption_versions_and_confinement() {
    let root = corpus("analytics-corruption");
    let q = query();
    let (code, v) = rivet(&root, "export", json!({"bundle":"one","query":q}));
    assert_eq!(code, 0, "{v}");
    let original = read(&root, ".rivet/exports/one/manifest.json");
    let manifest: Value = serde_json::from_str(&original).unwrap();
    for field in ["version", "engine", "parser"] {
        let mut m = manifest.clone();
        m[field] = json!("wrong");
        put(&root, ".rivet/exports/one/manifest.json", &m);
        assert_eq!(rivet(&root, "replay", json!({"bundle":"one"})).0, 2);
    }
    let mut m = manifest.clone();
    m["outcomes"][0]["member"] = json!("sources/../../outside.bin");
    put(&root, ".rivet/exports/one/manifest.json", &m);
    let (code, v) = rivet(&root, "replay", json!({"bundle":"one"}));
    assert_eq!(code, 2);
    assert_eq!(v["error"]["code"], "path_rejected");
    let mut m = manifest.clone();
    m["outcomes"][0]["status"] = json!("parse_failed");
    put(&root, ".rivet/exports/one/manifest.json", &m);
    assert_eq!(rivet(&root, "replay", json!({"bundle":"one"})).0, 2);
    std::fs::write(root.join(".rivet/exports/one/manifest.json"), original).unwrap();
    std::fs::write(root.join(".rivet/exports/one/sources/0.bin"), "tampered").unwrap();
    assert_eq!(rivet(&root, "replay", json!({"bundle":"one"})).0, 2);
    assert_eq!(
        rivet(&root, "export", json!({"bundle":"one","query":q})).0,
        2
    );
    std::os::unix::fs::symlink(root.join("tracking"), root.join(".rivet/exports/linked")).unwrap();
    assert_eq!(
        rivet(&root, "export", json!({"bundle":"linked","query":q})).0,
        2
    );
    assert_eq!(rivet(&root, "replay", json!({"bundle":"linked"})).0, 2);
    assert!(!root.join("tracking/manifest.json").exists());
}

#[test]
fn analytics_capture_once_without_reread() {
    let root = corpus("analytics-single-read");
    let pause = Pause::new(&root, "analytics_read");
    let child = spawn(
        &root,
        "export",
        &json!({"bundle":"observed","query":query()}),
        &[pause.env()],
    );
    pause.wait();
    write_doc(&root, "a.md", "changed while scan was paused\n");
    pause.release();
    let (code, export) = finish(child);
    assert_eq!(code, 0, "{export}");
    assert_eq!(export["report"]["total"], 3);
    assert_eq!(export["report"]["coverage"]["reads"], 3);
    let (code, replay) = rivet(&root, "replay", json!({"bundle":"observed"}));
    assert_eq!(code, 0, "{replay}");
    assert_eq!(replay, export["report"]);
    assert_eq!(
        read(&root, "tracking/a.md"),
        "changed while scan was paused\n"
    );
}

#[test]
fn analytics_documented_example() {
    fn copy(from: &std::path::Path, to: &std::path::Path) {
        std::fs::create_dir_all(to).unwrap();
        for entry in std::fs::read_dir(from).unwrap() {
            let entry = entry.unwrap();
            let target = to.join(entry.file_name());
            if entry.file_type().unwrap().is_dir() {
                copy(&entry.path(), &target)
            } else {
                std::fs::copy(entry.path(), target).unwrap();
            }
        }
    }
    let root = project("analytics-doc-example");
    for e in std::fs::read_dir(root.join("tracking")).unwrap() {
        std::fs::remove_file(e.unwrap().path()).unwrap();
    }
    let source =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/analytics/project");
    copy(&source, &root);
    let v = analyze(
        &root,
        json!({"collection":"actions","analytics":"census","source":"documents"}),
    );
    assert_eq!(v["total"], 3);
    let q = json!({"collection":"actions","analytics":"census","where":[{"field":"item.done","op":"eq","value":false}],"group_by":["document.cluster"],"measures":{"actions":{"op":"count"},"trackers":{"op":"count_distinct","field":"document.path"}}});
    let v = analyze(&root, q);
    assert_eq!(v["groups"].as_array().unwrap().len(), 2);
    for group in v["groups"].as_array().unwrap() {
        assert_eq!(group["measures"]["actions"]["count"], 1);
        assert_eq!(group["measures"]["trackers"]["count"], 1);
    }
    let v = analyze(
        &root,
        json!({"collection":"actions","analytics":"census","source":"documents","facets":["document.cluster"],"zero_enums":true}),
    );
    assert_eq!(v["facets"]["document.cluster"]["buckets"][0]["count"], 1);
    assert_eq!(v["facets"]["document.cluster"]["buckets"][1]["count"], 2);
    assert_eq!(v["facets"]["document.cluster"]["buckets"][2]["count"], 0);
    let (code, view) = rivet(&root, "run-view", json!({"view":"open-actions"}));
    assert_eq!(code, 0, "{view}");
    assert_eq!(view["total"], 2);
    let (code, export) = rivet(
        &root,
        "export",
        json!({"bundle":"open-actions-capture","view":"open-actions"}),
    );
    assert_eq!(code, 0, "{export}");
    let (code, replay) = rivet(&root, "replay", json!({"bundle":"open-actions-capture"}));
    assert_eq!(code, 0, "{replay}");
    assert_eq!(replay, export["report"]);
}

#[test]
fn analytics_definitions_capture_once() {
    let root = corpus("analytics-config-capture");
    let pause = Pause::new(&root, "analytics_config");
    let child = spawn(
        &root,
        "export",
        &json!({"bundle":"definitions","query":query()}),
        &[pause.env()],
    );
    pause.wait();
    let mut schema: Value =
        serde_json::from_str(&read(&root, ".rivet/schemas/tracker-action.json")).unwrap();
    schema["fields"]["actor"]["default"] = json!("agent");
    put(&root, ".rivet/schemas/tracker-action.json", &schema);
    pause.release();
    let (code, export) = finish(child);
    assert_eq!(code, 0, "{export}");
    assert_eq!(
        export["report"]["rows"][1]["fields"]["item.actor"]["value"],
        "human:owner"
    );
    let (code, replay) = rivet(&root, "replay", json!({"bundle":"definitions"}));
    assert_eq!(code, 0, "{replay}");
    assert_eq!(replay, export["report"]);
}

#[test]
fn analytics_item_origin_namespace_collision() {
    let root = corpus("analytics-origin-collision");
    let mut schema: Value =
        serde_json::from_str(&read(&root, ".rivet/schemas/tracker-action.json")).unwrap();
    schema["fields"]["actor.origin"] = json!({"type":"string","from":"label","label":"Provenance"});
    put(&root, ".rivet/schemas/tracker-action.json", &schema);
    write_doc(
        &root,
        "a.md",
        "---\ncluster: alpha\n---\n## Action\n\n- [ ] `agent` **First**\n  **Provenance**: tagged\n",
    );
    let legacy=rivet(&root,"query",json!({"collection":"actions","where":[{"field":"actor.origin","op":"eq","value":"tagged"}]})).1;
    assert_eq!(legacy["total"], 1);
    let v = analyze(
        &root,
        json!({"where":[{"field":"item.actor.origin","op":"eq","value":"tagged"}]}),
    );
    assert_eq!(v["total"], legacy["total"]);
    assert_eq!(
        v["rows"][0]["fields"]["item.actor.origin"]["value"],
        "tagged"
    );
    let v = analyze(
        &root,
        json!({"where":[{"field":"item.actor.origin.origin","op":"eq","value":"authored"}],"fields":["item.actor.origin.origin"]}),
    );
    assert_eq!(v["total"], 1);
    assert_eq!(
        v["rows"][0]["fields"]["item.actor.origin.origin"]["value"],
        "authored"
    );
}

#[test]
fn analytics_replay_returns_verified_read() {
    let root = corpus("analytics-replay-verified");
    let (code, export) = rivet(
        &root,
        "export",
        json!({"bundle":"observed","query":query()}),
    );
    assert_eq!(code, 0, "{export}");
    let pause = Pause::new(&root, "replay_verified");
    let child = spawn(
        &root,
        "replay",
        &json!({"bundle":"observed"}),
        &[pause.env()],
    );
    pause.wait();
    put(
        &root,
        ".rivet/exports/observed/records.json",
        &json!({"report":{"total":999}}),
    );
    pause.release();
    let (code, report) = finish(child);
    assert_eq!(code, 0, "{report}");
    assert_eq!(report, export["report"]);
    // A later invocation sees the changed member and must refuse it.
    assert_eq!(rivet(&root, "replay", json!({"bundle":"observed"})).0, 2);
}
#[test]
fn analytics_filtered_diagnostics_are_bounded() {
    let root = corpus("analytics-diagnostic-budget");
    let mut decl: Value =
        serde_json::from_str(&read(&root, ".rivet/analytics/census.json")).unwrap();
    for (values, rows) in [(400, 230), (700, 1)] {
        decl["fields"]["cluster"]["values"] = json!(
            (0..values)
                .map(|i| format!("{i}{}", "x".repeat(100)))
                .collect::<Vec<_>>()
        );
        put(&root, ".rivet/analytics/census.json", &decl);
        write_doc(
            &root,
            "a.md",
            &format!(
                "---\ncluster: invalid\n---\n## Action\n\n{}",
                "- [ ] **Excluded**\n".repeat(rows)
            ),
        );
        let (code, v) = rivet(
            &root,
            "analyze",
            json!({
                "collection":"actions","analytics":"census","mode":"diagnostic",
                "where":[{"field":"item.done","op":"eq","value":true}],
                "measures":{"rows":{"op":"count"}}
            }),
        );
        assert_eq!(code, 2, "{v}");
        assert_eq!(v["error"]["code"], "limit_exceeded", "{v}");
    }
}

#[test]
fn analytics_strict_drilldown_changed_invalid_source() {
    let root = corpus("analytics-strict-drilldown");
    let q = json!({"collection":"actions","analytics":"census",
        "measures":{"rows":{"op":"count"}}});
    let report = analyze(&root, q.clone());
    write_doc(
        &root,
        "a.md",
        "---\ncluster: invalid\n---\n## Action\n\n- [ ] **Changed**\n",
    );
    let (code, v) = rivet(
        &root,
        "drilldown",
        json!({"query":q,
        "fingerprint":report["fingerprint"],"group":[],"measure":"rows"}),
    );
    assert_eq!(code, 2);
    assert_eq!(v["error"]["code"], "external_change");
    assert_eq!(
        rivet(&root, "analyze", q).1["error"]["code"],
        "invalid_item"
    );
}

#[test]
fn analytics_string_order_paging_facets_and_top_ties() {
    let root = corpus("analytics-string-order");
    let mut decl: Value =
        serde_json::from_str(&read(&root, ".rivet/analytics/census.json")).unwrap();
    decl["fields"]["cluster"]["values"] = json!(["A B", "A"]);
    put(&root, ".rivet/analytics/census.json", &decl);
    for (name, cluster) in [
        ("a.md", "A"),
        ("b.md", "A B"),
        ("empty.md", "A"),
        ("c.md", "A B"),
    ] {
        write_doc(
            &root,
            name,
            &format!("---\ncluster: {cluster}\n---\n## Action\n\n- [ ] **Task**\n"),
        );
    }
    let mut q = json!({"collection":"actions","analytics":"census","source":"documents",
        "group_by":["document.cluster"],"facets":["document.cluster"],
        "measures":{"rows":{"op":"count"}},"limit":1});
    let report = analyze(&root, q.clone());
    assert_eq!(report["groups"][0]["key"][0]["value"], "A");
    assert_eq!(
        report["facets"]["document.cluster"]["buckets"][0]["key"]["value"],
        "A"
    );
    assert_eq!(report["groups"][0]["base_rows"], 2);
    q["offset"] = json!(1);
    assert_eq!(
        analyze(&root, q.clone())["groups"][0]["key"][0]["value"],
        "A B"
    );
    q["offset"] = json!(0);
    q["top"] = json!(1);
    assert_eq!(analyze(&root, q)["groups"][0]["key"][0]["value"], "A");
    for (field, expected) in [
        ("document.cluster", vec![json!("A"), json!("A B")]),
        ("document.score", vec![json!(2), json!(10)]),
    ] {
        decl["fields"]["score"] = json!({"type":"number","key":["score"]});
        put(&root, ".rivet/analytics/census.json", &decl);
        for (name, cluster, score) in [
            ("a.md", "A", 10),
            ("b.md", "A B", 2),
            ("empty.md", "A", 10),
            ("c.md", "A B", 2),
        ] {
            write_doc(
                &root,
                name,
                &format!(
                    "---\ncluster: {cluster}\nscore: {score}\n---\n## Action\n\n- [ ] **Task**\n"
                ),
            );
        }
        let q = json!({"collection":"actions","analytics":"census","source":"documents",
            "measures":{"keys":{"op":"count_distinct","field":field}}});
        let report = analyze(&root, q.clone());
        let (code, drill) = rivet(
            &root,
            "drilldown",
            json!({"query":q,
            "fingerprint":report["fingerprint"],"group":[],"measure":"keys"}),
        );
        assert_eq!(code, 0, "{drill}");
        assert_eq!(
            drill["keys"]
                .as_array()
                .unwrap()
                .iter()
                .map(|k| k["key"]["value"].clone())
                .collect::<Vec<_>>(),
            expected
        );
    }
}
