mod common;
use common::*;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

const RECORD: &str = "records/sample.md";
fn fixture(name: &str, body: &str) -> PathBuf {
    let root = project(name);
    std::fs::create_dir(root.join("records")).unwrap();
    put(&root, RECORD, body);
    put_json(
        &root,
        ".rivet/project.json",
        json!({"collections":{"tasks":{"schema":"task@1","files":["records/*.md"],"section":"Tasks","level":2}}}),
    );
    put_json(
        &root,
        ".rivet/schemas/task.json",
        json!({"id":"task","version":1,"item":"checkbox","fields":{"checked":{"type":"boolean","from":"checkbox"},"title":{"type":"string","from":"title"},"text":{"type":"string","from":"text"}}}),
    );
    root
}
fn put(root: &Path, path: &str, text: &str) {
    std::fs::write(root.join(path), text).unwrap();
}
fn put_json(root: &Path, path: &str, value: Value) {
    put(root, path, &value.to_string());
}
fn config(root: &Path) -> Value {
    serde_json::from_str(&read(root, ".rivet/project.json")).unwrap()
}
fn schema(root: &Path) -> Value {
    serde_json::from_str(&read(root, ".rivet/schemas/task.json")).unwrap()
}
// Complete entry, mode, symlink and byte manifest, including .rivet and every directory.
fn manifest(root: &Path) -> BTreeMap<String, (u32, Vec<u8>)> {
    fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<String, (u32, Vec<u8>)>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            let meta = path.symlink_metadata().unwrap();
            let key = path
                .strip_prefix(root)
                .unwrap()
                .to_str()
                .unwrap()
                .to_string();
            let bytes = if meta.file_type().is_symlink() {
                std::fs::read_link(&path)
                    .unwrap()
                    .as_os_str()
                    .as_encoded_bytes()
                    .to_vec()
            } else if meta.is_file() {
                std::fs::read(&path).unwrap()
            } else {
                vec![]
            };
            out.insert(key, (meta.permissions().mode(), bytes));
            if meta.is_dir() {
                walk(root, &path, out);
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(root, root, &mut out);
    out
}
fn doctor(root: &Path, request: Value) -> (i32, Value) {
    let before = manifest(root);
    let out = rivet(root, "doctor", request);
    assert_eq!(
        manifest(root),
        before,
        "doctor changed project entries, modes or bytes"
    );
    out
}
fn has(out: &Value, code: &str) -> bool {
    out["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .any(|d| d["code"] == code)
}
fn diagnosed(root: &Path, exit: i32, code: &str) -> Value {
    let (rc, out) = doctor(root, json!({}));
    assert_eq!(rc, exit, "{out}");
    assert!(has(&out, code), "{out}");
    out
}

#[test]
fn healthy_complete_and_missing_ids_strict_are_read_only() {
    let root = fixture(
        "doctor-healthy",
        "## Tasks\n\n- [ ] **First**\n- [x] **Second**\n",
    );
    let out = diagnosed(&root, 0, "missing_id");
    assert_eq!(out["health"], "warning");
    assert_eq!(out["summary"]["items"], 2);
    assert_eq!(out["summary"]["valid"], 2);
    assert_eq!(out["summary"]["invalid"], 0);
    assert_eq!(out["items"][1]["checked"], true);
    assert_eq!(doctor(&root, json!({"strict":true})).0, 4);
    stamp(&root, RECORD);
    let (code, out) = doctor(&root, json!({"collection":"tasks"}));
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["health"], "healthy");
    assert_eq!(out["complete"], true);
    assert_eq!(out["report_version"], 1);
    assert_eq!(out["consistency"], "per_file");
    assert_eq!(out["summary"]["files_selected"], 1);
    assert_eq!(out["summary"]["files_parsed"], 1);
    assert_eq!(out["unique_items"], 2);
    assert_eq!(out["counts"], "exact");
    assert_eq!(out["items"][0]["readiness"]["typed_update"], "not_checked");
    assert_eq!(
        doctor(&root, json!({"strict":true})).0,
        0,
        "informational unchecked updates do not fail strict"
    );
    assert!(
        out["collections"][0]["schema_fingerprint"]
            .as_str()
            .unwrap()
            .starts_with("sha256:")
    );
}

#[test]
fn request_refusals_and_requested_missing_path_have_distinct_exits() {
    let root = fixture("doctor-request", "## Tasks\n");
    for request in [
        json!({"collection":"unknown"}),
        json!({"paths":[]}),
        json!({"fix":true}),
        json!({"paths":["elsewhere/a.md"]}),
        json!({"strict":"yes"}),
        json!({"paths":["records/sample.txt"]}),
        json!({"paths":["records/./sample.md"]}),
        json!({"paths":["records//sample.md"]}),
        json!({"paths":["records/../sample.md"]}),
    ] {
        assert_eq!(doctor(&root, request).0, 1);
    }
    let (code, out) = doctor(&root, json!({"paths":["records/missing.md", RECORD]}));
    assert_eq!(code, 2, "{out}");
    assert_eq!(out["status"], "incomplete");
    assert_eq!(out["ok"], false);
    assert_eq!(out["health"], "unknown");
    assert_eq!(out["summary"]["files_requested"], 2);
    assert_eq!(out["summary"]["files_failed"], 1);
    assert_eq!(out["summary"]["files_parsed"], 1);
    assert_eq!(out["files"].as_array().unwrap().len(), 2);
    assert!(has(&out, "file_read_or_parse"));
}

#[test]
fn unusable_configuration_preserves_partial_evidence() {
    for fault in [
        "missing-config",
        "malformed-config",
        "empty-config",
        "missing-schema",
        "malformed-schema",
        "wrong-version",
        "unsupported-shape",
        "invalid-fields",
    ] {
        let root = fixture(fault, "## Tasks\n\n- [ ] **One**\n");
        match fault {
            "missing-config" => std::fs::remove_file(root.join(".rivet/project.json")).unwrap(),
            "malformed-config" => put(&root, ".rivet/project.json", "{"),
            "empty-config" => put(&root, ".rivet/project.json", "{}"),
            "missing-schema" => {
                std::fs::remove_file(root.join(".rivet/schemas/task.json")).unwrap()
            }
            "malformed-schema" => put(&root, ".rivet/schemas/task.json", "{"),
            other => {
                let mut s = schema(&root);
                match other {
                    "wrong-version" => s["version"] = json!(2),
                    "unsupported-shape" => s["item"] = json!("table"),
                    "invalid-fields" => s["fields"]["checked"]["type"] = json!("string"),
                    _ => unreachable!(),
                }
                put_json(&root, ".rivet/schemas/task.json", s);
            }
        }
        let (code, out) = doctor(&root, json!({}));
        assert_eq!(code, 2, "{fault}: {out}");
        assert_eq!(out["health"], "unknown");
        assert!(!out["diagnostics"].as_array().unwrap().is_empty());
    }
}

#[test]
fn missing_ambiguous_and_empty_headings_and_zero_matches_are_distinct() {
    for (body, code) in [
        ("## Other\n", "missing_heading"),
        ("## Tasks\n\n## Tasks\n", "ambiguous_heading"),
    ] {
        let root = fixture(code, body);
        diagnosed(&root, 4, code);
    }
    let root = fixture("doctor-empty", "## Tasks\n");
    assert_eq!(doctor(&root, json!({})).1["health"], "healthy");
    let mut c = config(&root);
    c["collections"]["tasks"]["files"] = json!(["absent/*.md"]);
    put_json(&root, ".rivet/project.json", c);
    let out = diagnosed(&root, 4, "zero_files");
    assert_eq!(out["summary"]["files_discovered"], 0);
    assert_eq!(out["summary"]["items"], 0);
}

#[test]
fn invalid_rows_remain_visible_with_addressing_defects() {
    let root = fixture(
        "doctor-invalid",
        "## Tasks\n\n- [ ] no required title\n- [x] `human:wrong` **Bad date (2026-02-30)**\n",
    );
    let mut s = schema(&root);
    s["fields"]["title"]["required"] = json!(true);
    s["fields"]["date"] = json!({"type":"date","from":"title_date","required":true});
    s["fields"]["actor"] = json!({"type":"enum","from":"tag","tags":["human:*"],"values":["human:owner"],"required":true});
    put_json(&root, ".rivet/schemas/task.json", s);
    let out = diagnosed(&root, 4, "invalid_item");
    assert_eq!(out["summary"]["items"], 2);
    assert_eq!(out["summary"]["invalid"], 2);
    assert_eq!(out["summary"]["missing_ids"], 2);
    assert_eq!(out["items"].as_array().unwrap().len(), 2);
    let messages = out["diagnostics"].to_string();
    assert!(messages.contains("required field missing"));
    assert!(messages.contains("date"));
    assert!(messages.contains("actor"));
}

#[test]
fn subsection_exclusions_and_overlapping_unstamped_memberships_reconcile() {
    let root = fixture(
        "doctor-membership",
        "## Tasks\n\n- [ ] **Root**\n\n### Batch\n\n- [x] **Child**\n\n### Ignore\n\n- [ ] **Excluded**\n\n#### Deep\n\n- [ ] **Also excluded**\n",
    );
    let out = diagnosed(&root, 0, "unselected_subsection");
    assert_eq!(out["summary"]["items"], 1);
    assert_eq!(out["summary"]["unselected_items"], 3);
    let mut c = config(&root);
    c["collections"]["tasks"]["subsections"] = json!(true);
    c["collections"]["tasks"]["exclude"] = json!(["Ignore"]);
    c["collections"]["overlap"] = c["collections"]["tasks"].clone();
    put_json(&root, ".rivet/project.json", c);
    let out = diagnosed(&root, 0, "excluded_subtree");
    assert_eq!(out["summary"]["items"], 4);
    assert_eq!(out["unique_items"], 2);
    assert_eq!(out["summary"]["files_selected"], 1);
    assert_eq!(out["summary"]["files_parsed"], 1);
    assert_eq!(out["summary"]["excluded_items"], 4);
    assert_eq!(out["collections"][0]["coverage"]["items"], 2);
    assert_eq!(out["files"][0]["memberships"].as_array().unwrap().len(), 2);
    assert_eq!(
        doctor(&root, json!({"collection":"tasks"})).1["summary"]["items"],
        2
    );
}

#[test]
fn duplicate_plain_and_foreign_markers_follow_native_semantics() {
    let root = fixture(
        "doctor-plain",
        "## Tasks\n\n- [ ] **One** <!-- stay:plainA01 subhash=sha256:111111111111 -->\n",
    );
    let (code, out) = doctor(&root, json!({}));
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["items"][0]["addressing"], "resolvable");
    // A plain marker is never rejected; its stale digest is only a warning.
    assert_eq!(out["health"], "warning");
    let codes: Vec<_> = out["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| (d["code"].clone(), d["id"].clone()))
        .collect();
    assert_eq!(codes, [(json!("evidence_drift"), json!("plainA01"))]);
    put(
        &root,
        RECORD,
        "## Tasks\n\n- [ ] **One** <!-- stay:dup00001 subhash=sha256:111111111111 -->\n- [x] **Two** <!-- stay:dup00001 subhash=sha256:111111111111 -->\n",
    );
    let out = diagnosed(&root, 4, "duplicate_id");
    assert_eq!(out["summary"]["unresolvable_ids"], 2);
    put(
        &root,
        RECORD,
        "## Tasks\n\n- [ ] **One**\n\n  | a | b |\n  |---|---|\n  | 1 | 2 |\n\n  Closing paragraph. <!-- stay:innerP01 -->\n  More text.\n",
    );
    stamp(&root, RECORD);
    let out = diagnosed(&root, 4, "rewrite_blocked");
    assert_eq!(out["items"][0]["readiness"]["checkbox"], "clear");
    assert_eq!(
        out["items"][0]["readiness"]["rewrite_prerequisites"],
        "blocked"
    );
}

#[test]
fn carrier_matrix_matches_real_stamp_previews_and_original_coordinates() {
    for (name, text, blocked) in [
        ("less", "a < b", true),
        ("slash", r"a \ b", true),
        ("code", "`a < b`", false),
        ("wrapped", "`a <\n  b`", true),
        ("fenced", "text\n\n  ```\n  a < b\n  ```", true),
    ] {
        let root = fixture(
            name,
            &format!(
                "## Tasks\n\n- [ ] **One** {text}\n- [ ] **Two**\n\nOther prose.\n\n- [ ] **Independent**\n"
            ),
        );
        let (code, out) = doctor(&root, json!({}));
        assert_eq!(code, if blocked { 4 } else { 0 }, "{name}: {out}");
        let (_, map) = rivet(&root, "map", json!({"path":RECORD}));
        let (_, preview) = rivet(
            &root,
            "stamp",
            json!({"path":RECORD,"guard":map["revision"],"preview":true}),
        );
        let refused: BTreeSet<u64> = preview["ops"][0]["refused"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|r| r["line"].as_u64().unwrap())
            .collect();
        let diagnosed: BTreeSet<u64> = out["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|d| d["code"] == "stamp_blocked")
            .map(|d| d["line"].as_u64().unwrap())
            .collect();
        assert_eq!(!refused.is_empty(), blocked, "{name}: {preview}");
        assert_eq!(diagnosed, refused, "{name}: {out}");
        assert_eq!(
            out["items"].as_array().unwrap().last().unwrap()["readiness"]["stamp"],
            "clear"
        );
        if blocked {
            let group = &out["carrier_groups"][0];
            assert_eq!(group["affected_items"], refused.len(), "{name}");
            let d = out["diagnostics"]
                .as_array()
                .unwrap()
                .iter()
                .find(|d| d["code"] == "stamp_blocked")
                .unwrap();
            let b = &d["blocker"];
            assert!(d["column"].is_null(), "{name}: {d}");
            assert_eq!(b["origin"], "as_read");
            assert!(d["revision"].is_string());
            let source = read(&root, RECORD);
            let at = b["offset"].as_u64().unwrap() as usize;
            assert!(source[at..].starts_with(b["character"].as_str().unwrap()));
        }
    }
}

#[test]
fn an_item_with_its_own_hand_written_stay_points_at_adopt_stay_as_stamp_does() {
    let root = fixture(
        "doctor-hand",
        "## Tasks\n\n- [ ] **One** by hand <!-- stay:handID01 -->\n- [ ] **Two**\n",
    );
    // The hand-written stay reads as the list's, so a rewrite of One is
    // blocked; both findings point at the adoption.
    let out = diagnosed(&root, 4, "missing_id");
    let blocked = out["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["code"] == "rewrite_blocked")
        .unwrap();
    assert!(
        blocked["next_action"]
            .as_str()
            .unwrap()
            .contains("adopt_stay"),
        "{blocked}"
    );
    let missing: Vec<&Value> = out["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|d| d["code"] == "missing_id")
        .collect();
    assert_eq!(missing.len(), 2, "{out}");
    let one = missing.iter().find(|d| d["line"] == 3).unwrap();
    assert!(
        one["message"].as_str().unwrap().contains("handID01"),
        "{one}"
    );
    assert!(
        one["next_action"].as_str().unwrap().contains("adopt_stay"),
        "{one}"
    );
    let two = missing.iter().find(|d| d["line"] == 4).unwrap();
    assert!(
        !two["next_action"].as_str().unwrap().contains("adopt_stay"),
        "{two}"
    );
    let ready = &out["items"][0]["readiness"];
    assert_eq!(ready["stamp"], "not_applicable", "{ready}");
    assert_eq!(ready["current_carrier"], "not_applicable", "{ready}");
    assert_eq!(out["items"][1]["readiness"]["stamp"], "clear");
    // The stamp preview skips the same item, on the same line.
    let (_, map) = rivet(&root, "map", json!({"path":RECORD}));
    let (code, preview) = rivet(
        &root,
        "stamp",
        json!({"path":RECORD,"guard":map["revision"],"preview":true}),
    );
    assert_eq!(code, 0, "{preview}");
    let refused = preview["ops"][0]["refused"].as_array().unwrap();
    assert_eq!(refused.len(), 1, "{preview}");
    assert_eq!(refused[0]["line"], 3);
    assert_eq!(refused[0]["id"], "handID01");
    // A real stamp with nothing but the skip to do leaves the file's bytes alone.
    let alone = "records/alone.md";
    let text = "- [ ] **One** by hand <!-- stay:handID01 -->\n";
    put(&root, alone, text);
    let (_, map) = rivet(&root, "map", json!({"path":alone}));
    let (code, out) = rivet(
        &root,
        "stamp",
        json!({"path":alone,"select":{"kind":"document"},"guard":map["revision"]}),
    );
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["ops"][0]["refused"][0]["id"], "handID01", "{out}");
    assert_eq!(read(&root, alone), text);
}

#[test]
fn stamped_carrier_and_changing_checkbox_and_text_previews_are_separate() {
    let root = fixture(
        "doctor-tick",
        "## Tasks\n\n- [ ] **One** plain\n- [ ] **Two**\n",
    );
    stamp(&root, RECORD);
    put(
        &root,
        RECORD,
        &read(&root, RECORD).replace("plain", "a < b"),
    );
    let out = diagnosed(&root, 4, "carrier_blocked");
    assert_eq!(out["items"][1]["readiness"]["checkbox"], "clear");
    assert_eq!(out["items"][1]["readiness"]["stamp"], "not_applicable");
    let (_, q) = rivet(&root, "query", json!({"collection":"tasks"}));
    let second = &q["items"][1];
    for (set, want) in [(json!({"checked":true}), 0), (json!({"text":"Changed"}), 2)] {
        let before = manifest(&root);
        let (code, res) = rivet(
            &root,
            "update-item",
            json!({"path":RECORD,"collection":"tasks","id":second["id"],"guard":second["revision"],"set":set,"preview":true}),
        );
        assert_eq!(code, want, "{res}");
        assert_eq!(manifest(&root), before);
    }
}

#[test]
fn file_confinement_failures_are_accounted_without_writes() {
    use std::os::unix::fs::symlink;
    let root = fixture("doctor-confined", "## Tasks\n");
    let outside = fixture("doctor-outside", "## Tasks\n");
    symlink(outside.join(RECORD), root.join("records/link.md")).unwrap();
    {
        let path = "records/link.md";
        let mut c = config(&root);
        c["collections"]["tasks"]["files"] = json!(["records/**"]);
        put_json(&root, ".rivet/project.json", c);
        let (code, out) = doctor(&root, json!({"paths":[path]}));
        assert_eq!(code, 2, "{out}");
        assert_eq!(out["summary"]["files_failed"], 1);
    }
    std::fs::write(root.join(RECORD), [0xff, 0xfe]).unwrap();
    diagnosed(&root, 2, "file_read_or_parse");
}

#[test]
fn document_wide_mdx_blocker_outside_membership_blocks_every_write() {
    let root = fixture(
        "doctor-mdx",
        "## Tasks\n\n- [ ] **One**\n\n## Outside\n\n{/* stay:mdx00001 */}\n",
    );
    let out = diagnosed(&root, 4, "unsupported_syntax");
    assert_eq!(
        out["items"][0]["readiness"]["document_prerequisite"],
        "blocked"
    );
    assert_eq!(out["items"][0]["readiness"]["checkbox"], "blocked");
}

#[test]
fn more_than_query_page_is_fully_scanned_and_fault_at_end_is_visible() {
    let mut body = "## Tasks\n\n".to_string();
    for n in 0..1100 {
        body.push_str(&format!(
            "- [ ] **Item {n}** <!-- stay:D{n:07} subhash=sha256:111111111111 -->\n"
        ));
    }
    body.push_str("- [x] **Final** no id\n");
    let root = fixture("doctor-many", &body);
    let out = diagnosed(&root, 0, "missing_id");
    assert_eq!(out["summary"]["items"], 1101);
    assert_eq!(out["summary"]["valid"], 1101);
    assert_eq!(out["summary"]["missing_ids"], 1);
    assert_eq!(out["items"].as_array().unwrap().len(), 1101);
}

#[test]
fn diagnostic_and_response_budgets_are_explicit_incomplete_reports() {
    let root = fixture("doctor-diagnostic-limit", "## Tasks\n\n- [ ] **One**\n");
    let mut s = schema(&root);
    for n in 0..120 {
        s["fields"][format!("f{n:05}")] =
            json!({"type":"string","from":"label","label":format!("L{n}"),"required":true});
    }
    put_json(&root, ".rivet/schemas/task.json", s);
    let before = manifest(&root);
    let (code, out) = finish(spawn(
        &root,
        "doctor",
        &json!({}),
        &[("RIVET_TEST_DOCTOR_DIAGNOSTICS", "100".into())],
    ));
    assert_eq!(manifest(&root), before);
    assert_eq!(code, 2);
    assert_eq!(out["health"], "unknown");
    assert!(
        out["omitted_detail"]
            .as_str()
            .unwrap()
            .contains("diagnostic_count"),
        "{}",
        out["omitted_detail"]
    );
    assert_eq!(out["diagnostics"].as_array().unwrap().len(), 100);
    assert_eq!(out["counts"], "lower_bound");
    // Healthy retained details can hit the byte budget without any diagnostic cap.
    let name = "é".repeat(500);
    let mut c = config(&root);
    let binding = c["collections"]["tasks"].take();
    c["collections"] = json!({name.clone():binding});
    put_json(&root, ".rivet/project.json", c);
    let s = json!({"id":"task","version":1,"item":"checkbox","fields":{"checked":{"type":"boolean","from":"checkbox"}}});
    put_json(&root, ".rivet/schemas/task.json", s);
    let mut body = "## Tasks\n\n".to_string();
    for n in 0..240 {
        body.push_str(&format!(
            "- [ ] **Item {n}** <!-- stay:H{n:07} subhash=sha256:111111111111 -->\n"
        ));
    }
    put(&root, RECORD, &body);
    let before = manifest(&root);
    let mut cmd = command(&root, "doctor", &[]);
    cmd.arg("--pretty").env("RIVET_TEST_DOCTOR_BYTES", "262144");
    let mut child = cmd.spawn().unwrap();
    use std::io::Write;
    child.stdin.take().unwrap().write_all(b"{}").unwrap();
    let raw = child.wait_with_output().unwrap();
    assert_eq!(raw.status.code(), Some(2));
    assert!(
        raw.stdout.len() <= 256 * 1024,
        "actual pretty response budget"
    );
    assert_eq!(manifest(&root), before);
    let out: Value = serde_json::from_slice(&raw.stdout).unwrap();
    assert_eq!(out["health"], "unknown");
    assert!(
        out["omitted_detail"]
            .as_str()
            .unwrap()
            .contains("serialized_response_bytes")
    );
    assert!(out["diagnostics"].as_array().unwrap().is_empty());
    assert!(out["items"].as_array().unwrap().len() < 240);
}

#[test]
fn large_multibyte_diagnostic_messages_are_budgeted_before_retention() {
    let root = fixture("doctor-large-message", "## Tasks\n\n- [ ] **One**\n");
    let mut s = schema(&root);
    let name = "界".repeat(2760000);
    s["fields"][name] = json!({"type":"string","from":"label","label":"Required","required":true});
    put_json(&root, ".rivet/schemas/task.json", s);
    let (code, out) = doctor(&root, json!({}));
    assert_eq!(code, 2);
    assert!(
        out["omitted_detail"]
            .as_str()
            .unwrap()
            .contains("serialized_response_bytes")
    );
    assert!(serde_json::to_vec_pretty(&out).unwrap().len() <= 8 * 1024 * 1024);
}

#[test]
fn doctor_finishes_while_writer_holds_lock_and_adds_no_entries() {
    let root = fixture("doctor-concurrent", "## Tasks\n\n- [ ] **One**\n");
    stamp(&root, RECORD);
    let (_, q) = rivet(&root, "query", json!({"collection":"tasks"}));
    let item = &q["items"][0];
    let request = json!({"path":RECORD,"collection":"tasks","id":item["id"],"guard":item["revision"],"set":{"checked":true}});
    let before = manifest(&root);
    // Derive writer-only bytes using the same fixture in a second isolated project.
    let expected = fixture("doctor-writer-only", "## Tasks\n");
    put(&expected, RECORD, &read(&root, RECORD));
    assert_eq!(rivet(&expected, "update-item", request.clone()).0, 0);
    let after_expected = manifest(&expected);
    let pause = Pause::new(&root, "planned");
    let writer = spawn(&root, "update-item", &request, &[pause.env()]);
    pause.wait();
    let (code, out) = doctor(&root, json!({}));
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["items"][0]["checked"], false);
    assert_eq!(manifest(&root), before);
    pause.release();
    assert_eq!(finish(writer).0, 0);
    assert_eq!(manifest(&root), after_expected);
}

#[test]
fn marker_diagnostic_severities_are_defined_and_missing_digest_is_not_provenance() {
    for (name, body, code, severity, exit) in [
        (
            "orphan",
            "<!-- stay:orphan01 -->\n\n## Tasks\n",
            "orphan_marker",
            "error",
            4,
        ),
        (
            "unaddressed",
            "## Tasks\n\ntext\n\n<!-- stay:child001 subhash=sha256:111111111111 -->\n",
            "unaddressed_marker",
            "warning",
            0,
        ),
        (
            "malformed",
            "## Tasks\n\n<!-- stay:hash=sha256:111111111111 -->\n",
            "malformed_marker",
            "error",
            4,
        ),
        (
            "frontmatter",
            "---\ndescription: |\n  multi\n  line\n---\n## Tasks\n\n- [ ] **One**\n",
            "unsupported_frontmatter",
            "info",
            0,
        ),
    ] {
        let root = fixture(name, body);
        let out = diagnosed(&root, exit, code);
        let d = out["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .find(|d| d["code"] == code)
            .unwrap();
        assert_eq!(d["severity"], severity);
        if severity == "info" {
            // Only the unstamped item's missing-id warning remains; frontmatter adds none.
            assert_eq!(out["summary"]["errors"], 0);
            assert_eq!(out["summary"]["warnings"], 1);
            // Stamped, the file is healthy even under strict: informational never fails.
            stamp(&root, RECORD);
            let (code, out) = doctor(&root, json!({"strict":true}));
            assert_eq!(code, 0, "{out}");
            assert_eq!(out["health"], "healthy");
            assert_eq!(out["summary"]["informational"], 1);
            // The checkbox write path accepts the file.
            let (_, q) = rivet(&root, "query", json!({"collection":"tasks"}));
            let item = &q["items"][0];
            let (code, res) = rivet(
                &root,
                "update-item",
                json!({"path":RECORD,"collection":"tasks","id":item["id"],"guard":item["revision"],"set":{"checked":true},"preview":true}),
            );
            assert_eq!(code, 0, "{res}");
            continue;
        }
        assert_eq!(doctor(&root, json!({"strict":true})).0, 4);
    }
    let root = fixture(
        "doctor-attributes",
        "## Tasks\n\n- [ ] **One** <!-- stay:child001 subhash=sha256:111111111111 quote=hello -->\n",
    );
    let out = diagnosed(&root, 4, "rewrite_blocked");
    assert_eq!(out["items"][0]["readiness"]["checkbox"], "clear");
    // The write path agrees: a whole-item rewrite is refused, the checkbox path is not.
    let (_, q) = rivet(&root, "query", json!({"collection":"tasks"}));
    let item = &q["items"][0];
    for (set, want) in [(json!({"text":"Changed"}), 2), (json!({"checked":true}), 0)] {
        let before = manifest(&root);
        let (code, res) = rivet(
            &root,
            "update-item",
            json!({"path":RECORD,"collection":"tasks","id":item["id"],"guard":item["revision"],"set":set,"preview":true}),
        );
        assert_eq!(code, want, "{res}");
        if want == 2 {
            assert_eq!(res["error"]["code"], "unsupported_syntax", "{res}");
            assert!(res["error"]["span"].is_object(), "{res}");
        }
        assert_eq!(manifest(&root), before);
    }
}

#[test]
fn enumeration_permission_failure_is_incomplete_even_with_valid_evidence() {
    let root = fixture("doctor-enumeration", "## Tasks\n\n- [ ] **One**\n");
    let mut c = config(&root);
    c["collections"]["tasks"]["files"] = json!(["records/**/*.md"]);
    put_json(&root, ".rivet/project.json", c);
    // A matching directory the walker must list, and an unrelated one it prunes unopened.
    let dirs = ["records/nested", "unrelated"];
    for dir in dirs {
        std::fs::create_dir(root.join(dir)).unwrap();
    }
    let before = manifest(&root);
    // Restore the test-owned directories even when an assertion fails.
    struct Restore(Vec<(PathBuf, std::fs::Permissions)>);
    impl Drop for Restore {
        fn drop(&mut self) {
            for (path, mode) in &self.0 {
                std::fs::set_permissions(path, mode.clone()).unwrap();
            }
        }
    }
    let restore = Restore(
        dirs.iter()
            .map(|d| (root.join(d), std::fs::Permissions::from_mode(before[*d].0)))
            .collect(),
    );
    let run = |request: &str| {
        let mut cmd = command(&root, "doctor", &[]);
        if unsafe { libc::geteuid() } == 0 {
            use std::os::unix::process::CommandExt;
            cmd.uid(65534).gid(65534);
        }
        let mut child = cmd.spawn().unwrap();
        use std::io::Write;
        child
            .stdin
            .take()
            .unwrap()
            .write_all(request.as_bytes())
            .unwrap();
        finish(child)
    };
    let pruned = |out: &Value| {
        out["traversal_events"].as_array().unwrap().iter().any(|e| {
            e["path"] == "unrelated" && e["reason"] == "glob_pruned" && e["error"].is_null()
        })
    };
    // An unrelated unreadable directory is proven out of scope, whole-binding or path-scoped.
    std::fs::set_permissions(root.join("unrelated"), std::fs::Permissions::from_mode(0o0)).unwrap();
    for request in ["{}", r#"{"paths":["records/sample.md"]}"#] {
        let (code, out) = run(request);
        assert_eq!(code, 0, "{out}");
        assert_eq!(out["complete"], true);
        assert!(!has(&out, "enumeration"), "{out}");
        assert!(pruned(&out), "{out}");
    }
    std::fs::set_permissions(
        root.join("records/nested"),
        std::fs::Permissions::from_mode(0o0),
    )
    .unwrap();
    let (code, out) = run("{}");
    assert_eq!(code, 2, "{out}");
    assert_eq!(out["health"], "unknown");
    let enumeration: Vec<_> = out["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|d| d["code"] == "enumeration")
        .map(|d| d["path"].as_str().unwrap())
        .collect();
    assert_eq!(enumeration, ["records/nested"], "{out}");
    assert!(pruned(&out), "{out}");
    assert_eq!(out["summary"]["items"], 1);
    assert!(has(&out, "missing_id"));
    assert!(!root.join(".rivet/lock").exists());
    drop(restore);
    assert_eq!(manifest(&root), before);
}

#[test]
fn absent_project_uses_partial_report_and_parser_failure_keeps_source_revision() {
    let root = fixture("doctor-no-project", "## Tasks\n");
    std::fs::remove_dir_all(root.join(".rivet")).unwrap();
    assert_eq!(doctor(&root, json!({"paths":[]})).0, 1);
    let (code, out) = doctor(&root, json!({}));
    assert_eq!(code, 2);
    assert_eq!(out["status"], "incomplete");
    assert_eq!(out["health"], "unknown");
    assert!(has(&out, "configuration"));
    let root = fixture("doctor-parser-failure", "## Tasks\r\n\n- [ ] **One**\r");
    let out = diagnosed(&root, 2, "file_read_or_parse");
    assert!(out["files"][0]["revision"].is_string());
    assert_eq!(
        out["diagnostics"][0]["revision"],
        out["files"][0]["revision"]
    );
}

#[test]
fn item_findings_keep_affected_identity_separate_from_blocker_identity() {
    let root = fixture(
        "doctor-identity",
        "## Tasks\n\n- [ ] **One** <!-- stay:owned001 subhash=sha256:111111111111 quote=hello -->\n",
    );
    let mut spec = schema(&root);
    spec["fields"]["required"] =
        json!({"type":"string","from":"label","label":"Required","required":true});
    put_json(&root, ".rivet/schemas/task.json", spec);
    let out = diagnosed(&root, 4, "invalid_item");
    for code in ["invalid_item", "rewrite_blocked"] {
        let finding = out["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .find(|d| d["code"] == code)
            .unwrap();
        assert_eq!(finding["id"], "owned001");
    }
    put(
        &root,
        RECORD,
        "## Tasks\n\n- [ ] **One** a < b <!-- stay:owned001 subhash=sha256:111111111111 -->\n- [ ] **Two**\n",
    );
    let out = diagnosed(&root, 4, "stamp_blocked");
    let finding = out["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["code"] == "stamp_blocked")
        .unwrap();
    assert_eq!(out["items"][1]["id"], Value::Null);
    assert_eq!(
        finding["id"],
        Value::Null,
        "blocker identity must not become affected identity"
    );
}

#[test]
fn unavailable_envelope_charges_escaped_root_and_scope_in_both_formats() {
    // Below Linux's per-argument limit, but escaped JSON root costs 720 KB.
    let root = PathBuf::from("\u{1}".repeat(120_000));
    let request = json!({"collection":"a".repeat(8_050_000)});
    for pretty in [false, true] {
        let mut cmd = command(&root, "doctor", &[]);
        if pretty {
            cmd.arg("--pretty");
        }
        let mut child = cmd.spawn().unwrap();
        use std::io::Write;
        child
            .stdin
            .take()
            .unwrap()
            .write_all(request.to_string().as_bytes())
            .unwrap();
        let out = child.wait_with_output().unwrap();
        assert_eq!(out.status.code(), Some(2));
        assert!(
            out.stdout.len() <= 8 * 1024 * 1024,
            "{} output bytes",
            out.stdout.len()
        );
        let report: Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(report["complete"], false);
        assert_eq!(report["health"], "unknown");
        assert!(
            report["omitted_detail"]
                .as_str()
                .unwrap()
                .contains("serialized_response_bytes")
        );
        assert!(report["scope"]["omitted"].is_string());
    }
}

#[test]
fn explicit_paths_narrow_collections_before_loading_unrelated_schemas() {
    let root = fixture("doctor-path-collections", "## Tasks\n\n- [ ] **One**\n");
    stamp(&root, RECORD);
    let mut c = config(&root);
    c["collections"]["notes"] =
        json!({"schema":"missing@1","files":["notes/*.md"],"section":"Notes","level":2});
    put_json(&root, ".rivet/project.json", c.clone());
    std::fs::create_dir(root.join("notes")).unwrap();
    put(&root, "notes/sample.md", "## Notes\n\n- [ ] **Note**\n");
    let (code, out) = doctor(&root, json!({"paths":[RECORD]}));
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["health"], "healthy");
    assert_eq!(out["collections"].as_array().unwrap().len(), 1);
    assert_eq!(out["collections"][0]["name"], "tasks");
    assert_eq!(out["summary"]["files_discovered"], 1);
    assert_eq!(out["summary"]["items"], 1);
    assert!(!has(&out, "zero_files"));
    c["collections"]["overlap"] = c["collections"]["tasks"].clone();
    put_json(&root, ".rivet/project.json", c);
    let (code, out) = doctor(&root, json!({"paths":[RECORD,RECORD]}));
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["collections"].as_array().unwrap().len(), 2);
    assert_eq!(out["summary"]["items"], 2);
    assert_eq!(out["unique_items"], 1);
    assert_eq!(out["summary"]["files_requested"], 1);
    assert_eq!(out["files"][0]["memberships"].as_array().unwrap().len(), 2);
    assert_eq!(
        doctor(&root, json!({"collection":"notes","paths":[RECORD]})).0,
        1
    );
    assert_eq!(
        doctor(&root, json!({"paths":[RECORD,"elsewhere/missing.md"]})).0,
        1
    );
    let (code, out) = doctor(&root, json!({"paths":[RECORD,"records/missing.md"]}));
    assert_eq!(code, 2);
    assert_eq!(out["summary"]["files_failed"], 1);
    assert_eq!(out["collections"].as_array().unwrap().len(), 2);
    assert_eq!(
        doctor(&root, json!({"paths":[RECORD,"notes/sample.md"]})).0,
        2
    );
    assert_eq!(
        doctor(&root, json!({})).0,
        2,
        "unscoped requests must still validate every schema"
    );
}

#[test]
fn query_and_doctor_discover_the_same_documents_and_name_excluded_symlinks() {
    let root = fixture("shared-discovery", "## Tasks\n\n- [ ] **One**\n");
    stamp(&root, RECORD);
    let mut c = config(&root);
    c["collections"]["tasks"]["files"] = json!(["records/*"]);
    put_json(&root, ".rivet/project.json", c);
    put(&root, "records/notes.txt", "## Tasks\n\n- [ ] **Decoy**\n");
    put(
        &root,
        "records/extra.markdown",
        "## Tasks\n\n- [ ] **Two**\n",
    );
    std::os::unix::fs::symlink(root.join(RECORD), root.join("records/link.md")).unwrap();
    let before = manifest(&root);
    let (code, query) = rivet(&root, "query", json!({"collection":"tasks"}));
    assert_eq!(code, 0, "{query}");
    let (code, diagnosis) = doctor(&root, json!({"collection":"tasks"}));
    assert_eq!(code, 0, "{diagnosis}");
    let queried: BTreeSet<_> = query["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["path"].as_str().unwrap())
        .collect();
    let diagnosed: BTreeSet<_> = diagnosis["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["path"].as_str().unwrap())
        .collect();
    assert_eq!(queried, diagnosed);
    assert_eq!(queried, BTreeSet::from(["records/extra.markdown", RECORD]));
    assert!(
        query["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|d| d["code"] == "path_rejected" && d["path"] == "records/link.md")
    );
    assert!(
        diagnosis["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|d| d["code"] == "excluded_entry" && d["path"] == "records/link.md")
    );
    assert_eq!(manifest(&root), before);
}

#[test]
fn receipt_log_warning_precedes_the_status_cap_and_metadata_errors_are_explicit() {
    let root = fixture("doctor-log-size", "## Tasks\n\n- [ ] **First**\n");
    stamp(&root, RECORD);
    let log = root.join(".rivet/commits.jsonl");
    let env = [("RIVET_TEST_LOG_BYTES", "4096".to_owned())];
    for (size, warns) in [(2047, false), (2048, true), (4097, true)] {
        std::fs::write(&log, vec![b'x'; size]).unwrap();
        let before = manifest(&root);
        let (code, out) = finish(spawn(&root, "doctor", &json!({}), &env));
        assert_eq!(code, 0, "{out}");
        assert_eq!(out["complete"], true, "{out}");
        assert_eq!(has(&out, "receipt_log_size"), warns, "{out}");
        if warns {
            let finding = out["diagnostics"]
                .as_array()
                .unwrap()
                .iter()
                .find(|d| d["code"] == "receipt_log_size")
                .unwrap();
            assert_eq!(finding["severity"], "warning");
            assert_eq!(finding["path"], ".rivet/commits.jsonl");
            assert!(finding["message"].as_str().unwrap().contains("4096"));
            let (code, _) = finish(spawn(&root, "doctor", &json!({"strict":true}), &env));
            assert_eq!(code, 4);
        }
        assert_eq!(manifest(&root), before);
    }
    std::fs::remove_file(&log).unwrap();
    std::os::unix::fs::symlink(root.join(RECORD), &log).unwrap();
    let (code, out) = doctor(&root, json!({}));
    assert_eq!(code, 2, "{out}");
    assert!(has(&out, "receipt_log_read"), "{out}");
    assert_eq!(out["complete"], false);
}

// Doctor answers every carrier through one index per
// list. On each side of one mid-list offender sit an ordinary unstamped item,
// items owning a stay, and an item with one digest-less hand-written stay;
// each kind is judged as doctor treats it, and every blocker names the offender.
#[test]
fn one_offender_mid_list_blocks_each_item_kind_only_after_it() {
    let root = fixture(
        "doctor-carrier-kinds",
        "## Tasks\n\n- [ ] **A1** owned\n- [ ] **A2** owned\n- [ ] **A3** owned\n- [ ] **A4** owned\n",
    );
    stamp(&root, RECORD);
    let stamped = read(&root, RECORD);
    let mut text = String::new();
    for line in stamped.split_inclusive('\n') {
        if line.starts_with("- [ ] **A2**") {
            text.push_str("- [ ] **U1** plain\n- [ ] **H1** by hand <!-- stay:handID01 -->\n");
        } else if line.starts_with("- [ ] **A3**") {
            text.push_str("- [ ] **Mid** a < b\n");
        } else if line.starts_with("- [ ] **A4**") {
            text.push_str("- [ ] **U2** plain\n- [ ] **H2** by hand <!-- stay:handID02 -->\n");
        }
        text.push_str(line);
    }
    put(&root, RECORD, &text);
    let offender = (text.find("a < b").unwrap() + 2) as u64;
    let line_of = |title: &str| {
        text[..text.find(&format!("**{title}**")).unwrap()]
            .matches('\n')
            .count() as u64
            + 1
    };
    let mid = line_of("Mid");

    let (code, out) = doctor(&root, json!({}));
    assert_eq!(code, 4, "{out}");
    let items: BTreeMap<u64, &Value> = out["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|i| i["path"] == RECORD)
        .map(|i| (i["line"].as_u64().unwrap(), i))
        .collect();
    assert_eq!(items.len(), 9, "{out}");
    let diagnostics = out["diagnostics"].as_array().unwrap();
    let at_line = |code: &str, line: u64| -> Vec<&Value> {
        diagnostics
            .iter()
            .filter(|d| d["code"] == code && d["line"] == line)
            .collect()
    };
    let check_blocker = |d: &Value| {
        let b = &d["blocker"];
        assert_eq!(b["offset"], offender, "{d}");
        assert_eq!(b["character"], "<", "{d}");
        assert_eq!(b["origin"], "as_read", "{d}");
    };

    // Ordinary unstamped items: blocked exactly when their carrier follows the offender.
    for (title, blocked) in [("U1", false), ("Mid", true), ("U2", true)] {
        let line = line_of(title);
        let ready = &items[&line]["readiness"];
        let found = at_line("stamp_blocked", line);
        if blocked {
            assert_eq!(ready["stamp"], "blocked", "{title}: {ready}");
            assert_eq!(found.len(), 1, "{title}: {out}");
            check_blocker(found[0]);
        } else {
            assert_eq!(ready["stamp"], "clear", "{title}: {ready}");
            assert!(found.is_empty(), "{title}: {out}");
        }
        assert_eq!(ready["current_carrier"], "not_applicable", "{title}");
    }

    // Items owning a stay: current_carrier blocked only after the offender.
    let (_, q) = rivet(&root, "query", json!({"collection":"tasks","limit":1000}));
    for (title, blocked) in [("A1", false), ("A2", false), ("A3", true), ("A4", true)] {
        let line = line_of(title);
        assert_eq!(line > mid, blocked, "{title}");
        let ready = &items[&line]["readiness"];
        assert_eq!(ready["stamp"], "not_applicable", "{title}: {ready}");
        let id = q["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["fields"]["title"] == title)
            .unwrap()["id"]
            .clone();
        let found: Vec<&Value> = diagnostics
            .iter()
            .filter(|d| d["code"] == "carrier_blocked" && d["id"] == id)
            .collect();
        if blocked {
            assert_eq!(ready["current_carrier"], "blocked", "{title}: {ready}");
            assert_eq!(found.len(), 1, "{title}: {out}");
            check_blocker(found[0]);
        } else {
            assert_eq!(ready["current_carrier"], "clear", "{title}: {ready}");
            assert!(found.is_empty(), "{title}: {out}");
        }
    }

    // One hand-written stay: neither check runs; the adoption repair names
    // the carrier exactly for the item after the offender.
    for (title, id, blocked) in [("H1", "handID01", false), ("H2", "handID02", true)] {
        let line = line_of(title);
        assert_eq!(line > mid, blocked, "{title}");
        let ready = &items[&line]["readiness"];
        assert_eq!(ready["stamp"], "not_applicable", "{title}: {ready}");
        assert_eq!(
            ready["current_carrier"], "not_applicable",
            "{title}: {ready}"
        );
        assert!(at_line("stamp_blocked", line).is_empty(), "{title}: {out}");
        assert!(
            at_line("carrier_blocked", line).is_empty(),
            "{title}: {out}"
        );
        let missing = at_line("missing_id", line);
        assert_eq!(missing.len(), 1, "{title}: {out}");
        let message = missing[0]["message"].as_str().unwrap();
        assert!(message.contains(id), "{title}: {message}");
        let carrier =
            "clear its carrier (the list's source before this item carries '<' (markstay §3.4))";
        assert_eq!(message.contains(carrier), blocked, "{title}: {message}");
    }
}

#[test]
fn a_stay_id_in_several_files_is_reported_without_any_citation() {
    let root = fixture(
        "doctor-cross-file",
        "## Tasks\n\n- [ ] **Own** <!-- stay:OwnOwn01 -->\n",
    );
    for n in 0..10 {
        put(
            &root,
            &format!("records/copy{n}.md"),
            "## Tasks\n\n- [ ] **Copy** <!-- stay:SameOne1 -->\n",
        );
    }
    let out = diagnosed(&root, 4, "cross_file_duplicate_id");
    assert_eq!(out["health"], "unhealthy");
    let found: Vec<_> = out["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|d| d["code"] == "cross_file_duplicate_id")
        .collect();
    assert_eq!(found.len(), 10, "{out}");
    for d in &found {
        assert_eq!(d["id"], "SameOne1", "{d}");
        assert_eq!(d["line"], 3, "{d}");
        let message = d["message"].as_str().unwrap();
        // Eight other carriers are named, the ninth is counted.
        assert_eq!(message.matches("records/copy").count(), 8, "{message}");
        assert!(message.ends_with("1 more"), "{message}");
    }
    assert!(!has(&out, "citation_ambiguous"), "{out}");
}
