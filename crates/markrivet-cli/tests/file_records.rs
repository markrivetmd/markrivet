mod common;
#[path = "common/move_fixture.rs"]
mod fixture;
use common::*;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Checkbox guards from the example tracker, as literals, so no change to
/// the binding shape can move one unnoticed.
#[test]
fn checkbox_guards_are_pinned() {
    let root = project("file-records-pinned");
    let (code, out) = rivet(&root, "query", json!({"collection": "actions"}));
    assert_eq!(code, 0, "{out}");
    let first = &out["items"][0];
    assert_eq!(first["path"], "tracking/TRACKING_DOCS.md", "{out}");
    assert_eq!(
        first["revision"],
        "rivet:v1:action:3e2614e8fecf31ba2a3867c330e288e3"
    );
    let (code, map) = rivet(&root, "map", json!({"path": WIDGETS}));
    assert_eq!(code, 0, "{map}");
    assert_eq!(
        map["collections"][0]["revision"],
        "rivet:v1:collection:37611eeca9c98d8682550333d321476c"
    );
    let (code, doctor) = rivet(&root, "doctor", json!({}));
    assert!(code == 0 || code == 1, "{doctor}");
    assert_eq!(
        doctor["collections"][0]["binding_fingerprint"],
        "sha256:764524528fc5ca6effd508665b0fa909cef3bb824ca2854663c90b2ef51ae816"
    );
}

const CONCEPT: &str = r#"{"id": "concept", "version": 1, "item": "file", "fields": {
  "kind":         {"type": "string",    "from": "key", "key": "type", "required": true},
  "status":       {"type": "enum",      "from": "key", "key": "status", "values": ["draft", "current", "retired"]},
  "stale_after":  {"type": "timestamp", "from": "key", "key": "stale_after"},
  "tags":         {"type": "list",      "from": "key", "key": "tags"},
  "rank":         {"type": "number",    "from": "key", "key": "rank"},
  "pinned":       {"type": "boolean",   "from": "key", "key": "pinned"},
  "reviewed":     {"type": "date",      "from": "key", "key": "reviewed"},
  "generated_at": {"type": "timestamp", "from": "key", "key": "generated", "field": "at"},
  "verified":     {"type": "entries",   "from": "key", "key": "verified"}}}"#;

const MARGIN: &str = "bundle/margin.md";
const MARGIN_TEXT: &str = "---\ntype: Metric\ntitle: Gross margin\nstatus: current\nstale_after: 2026-12-31T00:00:00Z\ntags: [finance, kpi]\nrank: 1\npinned: true\nreviewed: 2026-11-02\ngenerated: {by: agent, at: 2026-11-01T09:30:00+13:00}\nverified:\n  - {by: \"human:sam\", at: 2026-11-02T10:00:00Z}\n---\n# Gross margin\n\nRevenue less the cost of goods.\n";
const CHURN: &str = "bundle/orders/churn.md";
const CHURN_TEXT: &str = "---\ntype: Metric\nstatus: draft\nstale_after: 2027-03-01T00:00:00-05:00\ntags:\n  - retention\nrank: 2\npinned: false\ngenerated:\n  by: agent\n  at: 2026-10-01T00:00:00Z\nverified: {by: \"human:sam\", at: 2026-10-02T00:00:00Z}\n---\n# Churn\n\nCustomers lost in a period.\n";
const AOV: &str = "bundle/orders/aov.md";
const AOV_TEXT: &str = "---\ntype: Metric\nstatus: retired\n---\n# Average order value\n";
const INDEX: &str = "bundle/index.md";
const INDEX_TEXT: &str = "# Bundle\n\nSee the margin concept.\n";
const LOG: &str = "bundle/log.md";
const LOG_TEXT: &str = "---\ntype: Log\n---\n# Log\n\nSee the margin concept.\n";

fn put(root: &Path, rel: &str, text: &str) {
    let path = root.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

fn config(root: &Path) -> Value {
    serde_json::from_str(&read(root, ".rivet/project.json")).unwrap()
}

fn set_config(root: &Path, config: &Value) {
    put(root, ".rivet/project.json", &config.to_string());
}

/// The example tracker plus a file-record collection over a bundle whose
/// index and log are excluded.
fn records(name: &str) -> PathBuf {
    let root = project(name);
    let mut c = config(&root);
    c["collections"]["concepts"] = json!({"schema": "concept@1", "files": ["bundle/**/*.md"],
        "exclude_files": ["**/index.md", "**/log.md"]});
    set_config(&root, &c);
    put(&root, ".rivet/schemas/concept.json", CONCEPT);
    for (path, text) in [
        (MARGIN, MARGIN_TEXT),
        (CHURN, CHURN_TEXT),
        (AOV, AOV_TEXT),
        (INDEX, INDEX_TEXT),
        (LOG, LOG_TEXT),
    ] {
        put(&root, path, text);
    }
    root
}

fn query(root: &Path, request: Value) -> Value {
    let (code, out) = rivet(root, "query", request);
    assert_eq!(code, 0, "{out}");
    out
}

fn paths(out: &Value) -> Vec<String> {
    out["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["path"].as_str().unwrap().to_string())
        .collect()
}

fn refuse(root: &Path, cmd: &str, request: Value, code: &str) -> Value {
    let before = snapshot(root);
    let (exit, out) = rivet(root, cmd, request);
    assert_ne!(exit, 0, "{out}");
    assert_eq!(out["error"]["code"], code, "{out}");
    assert_eq!(snapshot(root), before, "a refusal changed bytes: {out}");
    out
}

/// Every file under the project, `.rivet/` included, by path.
fn snapshot(root: &Path) -> BTreeMap<String, Vec<u8>> {
    fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<String, Vec<u8>>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            let rel = path.strip_prefix(root).unwrap().display().to_string();
            // The lock file and its ignore list appear with a project's first
            // write attempt, refused or not; neither is document state.
            if rel == ".git"
                || rel.starts_with(".test-home")
                || rel == ".rivet/lock"
                || rel == ".rivet/.gitignore"
            {
                continue;
            }
            if path.is_dir() {
                walk(root, &path, out);
            } else {
                out.insert(rel, std::fs::read(&path).unwrap());
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(root, root, &mut out);
    out
}

#[test]
fn query_reads_file_records_by_path() {
    let root = records("file-records-read");
    let out = query(&root, json!({"collection": "concepts"}));
    assert_eq!(out["item"], "file", "{out}");
    assert_eq!(out["diagnostics"], json!([]), "{out}");
    assert_eq!(paths(&out), [MARGIN, AOV, CHURN]);
    let margin = &out["items"][0];
    let keys: Vec<_> = margin.as_object().unwrap().keys().cloned().collect();
    assert_eq!(keys, ["fields", "path", "revision"], "{margin}");
    assert!(
        margin["revision"]
            .as_str()
            .unwrap()
            .starts_with("rivet:v1:record:")
    );
    assert_eq!(
        margin["fields"],
        json!({"kind": "Metric", "status": "current", "stale_after": "2026-12-31T00:00:00Z",
            "tags": ["finance", "kpi"], "rank": 1, "pinned": true, "reviewed": "2026-11-02",
            "generated_at": "2026-11-01T09:30:00+13:00",
            "verified": [{"by": "human:sam", "at": "2026-11-02T10:00:00Z"}]})
    );
    // Block forms read as their flow equivalents, and a bare mapping is one entry.
    assert_eq!(
        out["items"][2]["fields"],
        json!({"kind": "Metric", "status": "draft", "stale_after": "2027-03-01T00:00:00-05:00",
            "tags": ["retention"], "rank": 2, "pinned": false,
            "generated_at": "2026-10-01T00:00:00Z",
            "verified": [{"by": "human:sam", "at": "2026-10-02T00:00:00Z"}]})
    );
    assert_eq!(
        out["items"][1]["fields"],
        json!({"kind": "Metric", "status": "retired"})
    );
    // The checkbox collection beside it reads as before.
    let actions = query(&root, json!({"collection": "actions"}));
    assert!(actions.get("item").is_none(), "{actions}");
    assert!(actions["items"][0]["id"].is_null());
}

#[test]
fn query_filters_records_on_each_type() {
    let root = records("file-records-filter");
    let hits = |field: &str, op: &str, value: Value| {
        paths(&query(
            &root,
            json!({"collection": "concepts", "where": [{"field": field, "op": op, "value": value}]}),
        ))
    };
    assert_eq!(hits("kind", "eq", json!("Metric")), [MARGIN, AOV, CHURN]);
    assert_eq!(
        hits("status", "in", json!(["current", "retired"])),
        [MARGIN, AOV]
    );
    assert_eq!(
        hits("stale_after", "lt", json!("2027-01-01T00:00:00Z")),
        [MARGIN]
    );
    // 2027-03-01T00:00:00-05:00 is 05:00Z: later than 04:00Z, equal to 05:00Z.
    assert_eq!(
        hits("stale_after", "gt", json!("2027-03-01T04:00:00Z")),
        [CHURN]
    );
    assert_eq!(
        hits("stale_after", "eq", json!("2027-03-01T05:00:00Z")),
        [CHURN]
    );
    assert_eq!(
        hits("stale_after", "gte", json!("2027-03-01T05:00:00.000Z")),
        [CHURN]
    );
    assert_eq!(
        hits("generated_at", "gte", json!("2026-10-31T20:30:00Z")),
        [MARGIN]
    );
    assert_eq!(hits("tags", "contains", json!("finance")), [MARGIN]);
    assert_eq!(hits("tags", "missing", Value::Null), [AOV]);
    assert_eq!(hits("verified", "exists", Value::Null), [MARGIN, CHURN]);
    assert_eq!(hits("verified", "missing", Value::Null), [AOV]);
    assert_eq!(hits("rank", "gt", json!(1)), [CHURN]);
    assert_eq!(hits("pinned", "eq", json!(true)), [MARGIN]);
    assert_eq!(hits("reviewed", "lt", json!("2026-12-01")), [MARGIN]);
    let projected = query(
        &root,
        json!({"collection": "concepts", "fields": ["status"], "paths": [CHURN]}),
    );
    assert_eq!(projected["items"][0]["fields"], json!({"status": "draft"}));
    for clause in [
        json!({"field": "stale_after", "op": "lt", "value": "2027-01-01"}),
        json!({"field": "tags", "op": "eq", "value": ["finance", "kpi"]}),
        json!({"field": "verified", "op": "eq", "value": []}),
        json!({"field": "verified", "op": "in", "value": []}),
        json!({"field": "tags", "op": "in", "value": []}),
        json!({"field": "id", "op": "eq", "value": "x"}),
    ] {
        refuse(
            &root,
            "query",
            json!({"collection": "concepts", "where": [clause]}),
            "invalid_request",
        );
    }
}

#[test]
fn record_reads_refuse_checkbox_only_requests() {
    let root = records("file-records-refusals");
    let out = refuse(
        &root,
        "query",
        json!({"collection": "concepts", "ready": {"field": "kind"}}),
        "invalid_request",
    );
    assert!(
        out["error"]["message"]
            .as_str()
            .unwrap()
            .contains("no ready waits"),
        "{out}"
    );
    let out = refuse(
        &root,
        "query",
        json!({"collection": "concepts", "fields": ["id"]}),
        "invalid_request",
    );
    assert!(
        out["error"]["message"]
            .as_str()
            .unwrap()
            .contains("id is not a field"),
        "{out}"
    );
    let out = refuse(
        &root,
        "query",
        json!({"collection": "concepts", "paths": [INDEX]}),
        "invalid_request",
    );
    assert!(
        out["error"]["message"]
            .as_str()
            .unwrap()
            .contains("not in collection"),
        "{out}"
    );
    let (_, map) = rivet(&root, "map", json!({"path": MARGIN}));
    let guard = map["collections"][0]["revision"].clone();
    // Records are addressed by path: an id or a selector is refused (D9).
    for name in [
        json!({"id": "abcdefgh"}),
        json!({"select": {"kind": "position", "heading_path": ["Gross margin"], "index": 0}}),
    ] {
        let mut request = json!({"path": MARGIN, "collection": "concepts", "guard": guard, "set": {"status": "retired"}});
        request
            .as_object_mut()
            .unwrap()
            .extend(name.as_object().unwrap().clone());
        let out = refuse(&root, "update-item", request, "invalid_request");
        assert!(
            out["error"]["message"]
                .as_str()
                .unwrap()
                .contains("addressed by path"),
            "{out}"
        );
    }
    // An excluded file is in no collection.
    let out = refuse(
        &root,
        "update-item",
        json!({"path": INDEX, "collection": "concepts", "id": "abcdefgh", "guard": guard, "set": {"status": "retired"}}),
        "invalid_request",
    );
    assert!(
        out["error"]["message"]
            .as_str()
            .unwrap()
            .contains("is not in collection"),
        "{out}"
    );
    // A checkbox update naming neither keeps its message.
    let out = refuse(
        &root,
        "update-item",
        json!({"path": WIDGETS, "collection": "actions", "guard": guard, "set": {"done": true}}),
        "invalid_request",
    );
    assert!(
        out["error"]["message"]
            .as_str()
            .unwrap()
            .ends_with("update_item names its item by exactly one of id and select"),
        "{out}"
    );
}

fn doctor(root: &Path, request: Value) -> Value {
    let (code, out) = rivet(root, "doctor", request);
    assert!(code == 0 || code == 4, "{out}");
    out
}

fn findings<'a>(out: &'a Value, path: &str) -> Vec<&'a Value> {
    out["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|d| d["path"] == path)
        .collect()
}

#[test]
fn unreadable_records_are_diagnostics_not_silence() {
    let root = records("file-records-unreadable");
    put(&root, "bundle/plain.md", "# No frontmatter\n\nBody only.\n");
    put(
        &root,
        "bundle/odd.md",
        "---\ntype: &anchor Metric\n---\n# Odd\n",
    );
    put(
        &root,
        "bundle/wrong.md",
        "---\nstatus: draft\nrank: \"2\"\n---\n# Wrong\n",
    );
    let out = query(&root, json!({"collection": "concepts"}));
    assert_eq!(paths(&out), [MARGIN, AOV, CHURN]);
    let diagnostics = out["diagnostics"].as_array().unwrap();
    let of = |path: &str| {
        diagnostics
            .iter()
            .find(|d| d["path"] == path)
            .unwrap_or_else(|| panic!("no diagnostic for {path}: {out}"))
    };
    assert_eq!(of("bundle/plain.md")["code"], "invalid_item");
    assert_eq!(
        of("bundle/plain.md")["message"],
        "the file has no frontmatter"
    );
    assert_eq!(of("bundle/odd.md")["code"], "invalid_item");
    assert!(of("bundle/odd.md")["span"].is_object(), "{out}");
    assert_eq!(
        of("bundle/wrong.md")["message"],
        "kind: required field missing; rank: expected number"
    );
    assert_eq!(diagnostics.len(), 3, "{out}");

    let report = doctor(&root, json!({"collection": "concepts"}));
    let item = |path: &str| {
        report["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["path"] == path)
            .unwrap_or_else(|| panic!("no record {path}: {report}"))
            .clone()
    };
    for (path, valid, rewrite) in [
        ("bundle/plain.md", false, "blocked"),
        ("bundle/odd.md", false, "blocked"),
        ("bundle/wrong.md", false, "clear"),
        (MARGIN, true, "clear"),
    ] {
        let i = item(path);
        assert_eq!(i["kind"], "file");
        assert_eq!(i["valid"], valid, "{i}");
        assert_eq!(i["readiness"]["rewrite_prerequisites"], rewrite, "{i}");
        for null in ["list_index", "item_index", "id", "checked", "readiness"] {
            if null != "readiness" {
                assert!(i[null].is_null(), "{null}: {i}");
            }
        }
        assert_eq!(i["line"], 1);
        assert_eq!(i["addressing"], "resolvable");
        assert_eq!(i["readiness"]["stamp"], "not_applicable");
        assert_eq!(i["readiness"]["checkbox"], "not_applicable");
        assert_eq!(i["readiness"]["typed_update"], "not_checked");
    }
    let codes = |path: &str| -> Vec<String> {
        findings(&report, path)
            .iter()
            .map(|d| d["code"].as_str().unwrap().to_string())
            .collect()
    };
    assert_eq!(codes("bundle/plain.md"), ["invalid_item"]);
    assert_eq!(
        codes("bundle/odd.md"),
        ["invalid_item", "unsupported_frontmatter"]
    );
    assert_eq!(codes("bundle/wrong.md"), ["invalid_item", "invalid_item"]);
    assert_eq!(codes(MARGIN), Vec::<String>::new());
    // A bare-mapping `verified` is valid and reports nothing.
    assert_eq!(codes(CHURN), Vec::<String>::new());
    assert_eq!(item(CHURN)["valid"], true);
    let file = report["files"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["path"] == MARGIN)
        .unwrap();
    assert_eq!(
        file["memberships"],
        json!([{"collection": "concepts", "heading": "not_applicable", "member_sections": [],
            "excluded_sections": [], "unselected_sections": []}])
    );
    let summary = &report["summary"];
    assert_eq!(
        (
            summary["items"].clone(),
            summary["valid"].clone(),
            summary["invalid"].clone()
        ),
        (json!(6), json!(3), json!(3))
    );
    assert_eq!(report["unique_items"], 6);
}

#[test]
fn excluded_files_are_members_nowhere() {
    let root = records("file-records-excluded");
    let out = query(&root, json!({"collection": "concepts"}));
    assert!(!paths(&out).iter().any(|p| p == INDEX || p == LOG), "{out}");
    assert!(!out.to_string().contains(INDEX), "{out}");
    let report = doctor(&root, json!({}));
    for path in [INDEX, LOG] {
        assert!(findings(&report, path).is_empty(), "{report}");
        assert!(
            !report["items"]
                .as_array()
                .unwrap()
                .iter()
                .any(|i| i["path"] == path)
        );
        assert!(
            !report["files"]
                .as_array()
                .unwrap()
                .iter()
                .any(|f| f["path"] == path)
        );
    }
    assert_eq!(
        report["collections"][1]["coverage"]["files_discovered"], 3,
        "{report}"
    );
    // Pathless find scans members only; a named path is searched as asked.
    let (code, found) = rivet(&root, "find", json!({"text": "See the margin concept"}));
    assert_eq!(code, 0, "{found}");
    assert_eq!(found["total"], 0, "{found}");
    let (code, found) = rivet(
        &root,
        "find",
        json!({"text": "See the margin concept", "path": INDEX}),
    );
    assert_eq!(code, 0, "{found}");
    assert_eq!(found["total"], 1, "{found}");
    // An excluded file has no file binding, so map lists none.
    let (_, map) = rivet(&root, "map", json!({"path": LOG}));
    assert_eq!(map["collections"], json!([]), "{map}");
}

#[test]
fn map_hands_a_writer_the_record_guard_query_reports() {
    let root = records("file-records-map");
    let (code, map) = rivet(&root, "map", json!({"path": MARGIN}));
    assert_eq!(code, 0, "{map}");
    let out = query(&root, json!({"collection": "concepts", "paths": [MARGIN]}));
    let revision = out["items"][0]["revision"].clone();
    assert_eq!(
        map["collections"],
        json!([{"name": "concepts", "schema": "concept@1", "item": "file", "valid": true,
            "errors": null, "revision": revision}])
    );
    put(&root, AOV, "---\nstatus: draft\n---\n");
    let (_, map) = rivet(&root, "map", json!({"path": AOV}));
    assert_eq!(map["collections"][0]["valid"], false);
    assert_eq!(
        map["collections"][0]["errors"],
        json!(["kind: required field missing"])
    );
}

#[test]
fn the_record_guard_covers_the_frontmatter_block() {
    let root = records("file-records-guard");
    let guard = |root: &Path, path: &str| -> String {
        let out = query(root, json!({"collection": "concepts", "paths": [path]}));
        out["items"][0]["revision"].as_str().unwrap().to_string()
    };
    let base = guard(&root, MARGIN);
    put(
        &root,
        MARGIN,
        &MARGIN_TEXT.replace("Revenue less", "Revenue minus"),
    );
    assert_eq!(guard(&root, MARGIN), base, "a body edit keeps the guard");
    for (from, to) in [
        ("title: Gross margin", "title: Gross margins"),
        ("rank: 1\n", "rank: 1\n# a comment\n"),
    ] {
        put(&root, MARGIN, &MARGIN_TEXT.replace(from, to));
        assert_ne!(guard(&root, MARGIN), base, "{to}");
    }
    put(&root, MARGIN, MARGIN_TEXT);
    assert_eq!(guard(&root, MARGIN), base);
    put(
        &root,
        ".rivet/schemas/concept.json",
        &format!("{CONCEPT}\n"),
    );
    assert_ne!(guard(&root, MARGIN), base, "a schema byte moves it");
    put(&root, ".rivet/schemas/concept.json", CONCEPT);
    assert_eq!(guard(&root, MARGIN), base);
    let mut c = config(&root);
    c["collections"]["concepts"]["exclude_files"] =
        json!(["**/index.md", "**/log.md", "**/draft.md"]);
    set_config(&root, &c);
    assert_ne!(guard(&root, MARGIN), base, "a binding change moves it");
    let root = records("file-records-guard-paths");
    put(&root, "bundle/copy.md", MARGIN_TEXT);
    assert_ne!(
        guard(&root, "bundle/copy.md"),
        guard(&root, MARGIN),
        "the path is its identity"
    );
    // A rename is a different record: an earlier cursor is stale.
    let first = query(&root, json!({"collection": "concepts", "limit": 1}));
    let cursor = first["next_cursor"].clone();
    assert!(cursor.is_string(), "{first}");
    std::fs::rename(root.join(CHURN), root.join("bundle/orders/churn-rate.md")).unwrap();
    refuse(
        &root,
        "query",
        json!({"collection": "concepts", "limit": 1, "cursor": cursor}),
        "stale_cursor",
    );
    assert!(
        query(&root, json!({"collection": "concepts", "paths": [CHURN]}))["items"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}

const TRACKER: &str = r#"{"id": "tracker", "version": 1, "item": "file", "fields": {
  "status": {"type": "enum",   "from": "key", "key": "status", "values": ["active", "done"]},
  "owner":  {"type": "string", "from": "key", "key": "owner"}}}"#;

/// Bind every tracker file as a record too, beside its checkbox queue.
fn with_trackers(root: &Path) {
    let mut c = config(root);
    c["collections"]["trackers"] = json!({"schema": "tracker@1", "files": ["tracking/*.md"]});
    set_config(root, &c);
    put(root, ".rivet/schemas/tracker.json", TRACKER);
}

fn seeded(root: &Path, cmd: &str, request: Value) -> (i32, Value) {
    finish(spawn(
        root,
        cmd,
        &request,
        &[("RIVET_TEST_ID_SEED", "file-records".into())],
    ))
}

/// The parts of a doctor report that belong to one collection.
fn doctor_side(report: &Value, name: &str) -> Value {
    let of = |key: &str| -> Vec<Value> {
        report[key]
            .as_array()
            .unwrap()
            .iter()
            .filter(|v| v["collection"] == name || v["name"] == name)
            .cloned()
            .collect()
    };
    let files: Vec<Value> = report["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| {
            let mut f = f.clone();
            f["memberships"] = json!(
                f["memberships"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter(|m| m["collection"] == name)
                    .collect::<Vec<_>>()
            );
            f
        })
        .collect();
    json!({"collections": of("collections"), "items": of("items"),
        "diagnostics": of("diagnostics"), "files": files})
}

#[test]
fn a_file_bound_as_records_too_reads_the_same_on_its_checkbox_side() {
    let plain = project("file-records-shared-plain");
    let shared = project("file-records-shared");
    with_trackers(&shared);
    let both = |cmd: &str, request: Value| {
        let (a, x) = rivet(&plain, cmd, request.clone());
        let (b, y) = rivet(&shared, cmd, request);
        assert_eq!(a, b, "{cmd}: {x} / {y}");
        (x, y)
    };
    let (x, mut y) = both("map", json!({"path": WIDGETS}));
    let collections = y["collections"].as_array().unwrap().clone();
    assert_eq!(collections.len(), 2, "{y}");
    assert_eq!(collections[1]["name"], "trackers");
    assert_eq!(collections[1]["valid"], true, "{y}");
    y["collections"] = json!([collections[0]]);
    assert_eq!(x, y);
    let (x, y) = both("find", json!({"text": "fixture"}));
    assert_eq!(x, y);
    // Status names every collection a path is in; its checkbox side, and
    // everything else but the read clocks, is the same.
    let status = |mut v: Value| {
        v["git"]["read_at"] = Value::Null;
        v["receipts_read_at"] = Value::Null;
        for p in v["paths"].as_array_mut().unwrap() {
            p["read_at"] = Value::Null;
            let names: Vec<Value> = p["collections"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|c| *c != "trackers")
                .cloned()
                .collect();
            p["collections"] = json!(names);
        }
        v
    };
    let (x, y) = both("status", json!({}));
    assert!(
        y["paths"][0]["collections"]
            .as_array()
            .unwrap()
            .contains(&json!("trackers")),
        "{y}"
    );
    assert_eq!(status(x), status(y));
    let (x, y) = both("doctor", json!({}));
    assert_eq!(doctor_side(&x, "actions"), doctor_side(&y, "actions"));
    assert_eq!(y["collections"][1]["name"], "trackers", "{y}");
    // A collection stamp and a stamped create cover checkbox items only.
    for root in [&plain, &shared] {
        let (_, map) = rivet(root, "map", json!({"path": WIDGETS}));
        let (code, out) = seeded(
            root,
            "stamp",
            json!({"path": WIDGETS, "guard": map["revision"]}),
        );
        assert_eq!(code, 0, "{out}");
        let (code, out) = seeded(
            root,
            "create",
            json!({"path": "tracking/TRACKING_NEW.md", "stamp": true,
                "content": "---\nstatus: active\n---\n# New\n\n## Action\n\n- [ ] **Start.** Now.\n\n## Notes\n\n- a note\n"}),
        );
        assert_eq!(code, 0, "{out}");
    }
    for path in [WIDGETS, "tracking/TRACKING_NEW.md"] {
        assert_eq!(read(&plain, path), read(&shared, path), "{path}");
    }
    assert!(read(&shared, WIDGETS).contains("<!-- stay:"));
}

/// Every named checkbox entry point refuses a file collection, by shape,
/// with every byte left as it was.
#[test]
fn checkbox_entry_points_refuse_a_file_collection() {
    let root = records("file-records-entry-points");
    with_trackers(&root);
    stamp(&root, WIDGETS);
    let (_, actions) = rivet(
        &root,
        "query",
        json!({"collection": "actions", "paths": [WIDGETS]}),
    );
    let item = &actions["items"][0];
    let id = item["id"].as_str().unwrap().to_string();
    let guard = item_guard(&root, WIDGETS, &id);
    let (_, map) = rivet(&root, "map", json!({"path": WIDGETS}));
    let record = map["collections"][1]["revision"].clone();
    let shape = |out: &Value| {
        assert!(
            out["error"]["message"]
                .as_str()
                .unwrap()
                .contains("holds file records"),
            "{out}"
        );
    };
    let position = json!({"kind": "position", "heading_path": ["Widgets", "Action"], "index": 0});
    for (cmd, request) in [
        (
            "insert-item",
            json!({"path": MARGIN, "collection": "concepts", "guard": "rivet:v1:collection:0", "position": "end", "fields": {"kind": "Metric"}}),
        ),
        (
            "insert-item",
            json!({"path": WIDGETS, "collection": "trackers", "guard": record, "position": "end", "fields": {"owner": "x"}}),
        ),
        (
            "delete",
            json!({"path": WIDGETS, "select": {"kind": "id", "id": id}, "guard": guard, "collection": "trackers"}),
        ),
        (
            "set",
            json!({"path": WIDGETS, "select": {"kind": "id", "id": id}, "guard": guard, "collection": "trackers", "typed_guard": record, "content": "[x] **Done.**"}),
        ),
        (
            "set",
            json!({"path": WIDGETS, "select": position, "guard": guard, "collection": "trackers", "typed_guard": record, "content": "[x] **Done.**"}),
        ),
        (
            "stamp",
            json!({"path": WIDGETS, "guard": map["revision"], "collection": "trackers", "checkbox_only": true, "collection_guard": record}),
        ),
        ("analyze", json!({"collection": "concepts"})),
        (
            "export",
            json!({"bundle": "records", "query": {"collection": "concepts"}}),
        ),
    ] {
        let out = refuse(&root, cmd, request, "invalid_request");
        shape(&out);
    }
}

#[test]
fn a_checkbox_move_beside_a_file_binding_lands_as_without_it() {
    let s = fixture::MoveStore::new("tight", false);
    with_trackers(&s.root);
    s.commit();
    let request = s.request();
    let out = s.ok("move", request.clone());
    assert_eq!(out["commit"]["status"], "committed", "{out}");
    assert_eq!(
        s.read(fixture::A),
        fixture::MoveStore::literal("tight", "expected-source", false)
    );
    assert_eq!(
        s.read(fixture::B),
        fixture::MoveStore::literal("tight", "expected-destination", false)
    );
    let s = fixture::MoveStore::new("tight", false);
    with_trackers(&s.root);
    s.commit();
    let mut request = s.request();
    request["collection"] = json!("trackers");
    let out = s.refuses(request, "invalid_request", None);
    assert!(
        out["error"]["message"]
            .as_str()
            .unwrap()
            .contains("holds file records"),
        "{out}"
    );
}

const ARCHIVE: &str = "archive/month/closed.md";
const QUEUE: &str = "# Synthetic\n\n## Action\n\n- [ ] Keep this open.\n- [x] Archive this completed item.\n- [x] Another completed item.\n\n## Log\n\nSuffix.\n";

/// A per-write git project whose tracker file is a checkbox queue with an
/// archive and also a file record.
struct Store {
    root: PathBuf,
    home: PathBuf,
}

impl Store {
    fn new(name: &str) -> Self {
        let root = records(name);
        let home = root.with_extension("isolated-home");
        std::fs::create_dir_all(&home).unwrap();
        let s = Self { root, home };
        put(
            &s.root,
            WIDGETS,
            &format!("---\nstatus: active\nowner: sam\n---\n{QUEUE}"),
        );
        put(&s.root, ARCHIVE, "# Archive\n\n## Closed\n\nPreserved.\n");
        stamp(&s.root, WIDGETS);
        stamp(&s.root, ARCHIVE);
        with_trackers(&s.root);
        let mut c = config(&s.root);
        c["collections"]["actions"]["archive"] = json!(["archive/**/*.md"]);
        c["git"] = json!({"commit": "per-write", "on_dirty": "refuse"});
        c["prune"] = json!({"files": ["tracking/*.md"]});
        set_config(&s.root, &c);
        s.git(&["init", "-q", "-b", "main"]);
        s.git(&["config", "user.name", "Synthetic Tester"]);
        s.git(&["config", "user.email", "tester@example.invalid"]);
        s.git(&["add", "-A"]);
        s.git(&["commit", "-qm", "synthetic fixture"]);
        s
    }
    fn git(&self, args: &[&str]) -> String {
        let out = git_command(&self.root, &self.home, args).output().unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    }
    fn run(&self, cmd: &str, input: Value) -> (i32, Value) {
        let env = [
            ("HOME", self.home.display().to_string()),
            ("XDG_CONFIG_HOME", self.home.display().to_string()),
            ("GIT_CONFIG_NOSYSTEM", "1".into()),
            ("GIT_CONFIG_GLOBAL", "/dev/null".into()),
        ];
        finish(spawn(&self.root, cmd, &input, &env))
    }
    fn ok(&self, cmd: &str, input: Value) -> Value {
        let (code, out) = self.run(cmd, input);
        assert_eq!(code, 0, "{cmd}: {out}");
        out
    }
    fn id(&self, item: usize) -> String {
        let doc = markrivet_core::Document::parse(&read(&self.root, WIDGETS)).unwrap();
        doc.target_ids(markrivet_core::document::Target::Item { list: 0, item })[0].clone()
    }
    fn copy(&self, item: usize) -> Value {
        let id = self.id(item);
        let select = json!({"kind": "heading_path", "path": ["Closed"]});
        let (_, dest) = self.run("get", json!({"path": ARCHIVE, "select": select}));
        json!({"source": WIDGETS, "id": id, "guard": item_guard(&self.root, WIDGETS, &id),
            "path": ARCHIVE, "select": select, "destination_guard": dest["revision"]})
    }
}

#[test]
fn archive_copy_delete_and_restore_work_beside_a_file_binding() {
    let s = Store::new("file-records-git");
    let copied = s.ok("archive-copy", s.copy(1));
    assert_eq!(copied["commit"]["status"], "committed", "{copied}");
    let id = s.id(1);
    let snapshot = s.ok("prune-snapshot", json!({}));
    let deleted = s.ok(
        "delete",
        json!({"path": WIDGETS, "select": {"kind": "id", "id": id},
            "guard": item_guard(&s.root, WIDGETS, &id), "require_commit": true,
            "archive": {"path": ARCHIVE, "copy_op": copied["commit"]["copy_op"], "commit": copied["commit"]["sha"]},
            "policy_guard": snapshot["snapshot"]["revision"]}),
    );
    assert_eq!(deleted["commit"]["status"], "committed", "{deleted}");
    assert!(!read(&s.root, WIDGETS).contains("Archive this completed"));
    // A typed update, then its history and a restore of the item before it.
    let id = s.id(0);
    let typed = s.ok(
        "query",
        json!({"collection": "actions", "paths": [WIDGETS]}),
    );
    let item = typed["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["id"] == id.as_str())
        .unwrap();
    let updated = s.ok(
        "update-item",
        json!({"path": WIDGETS, "collection": "actions", "id": id, "guard": item["revision"], "set": {"done": true}}),
    );
    assert_eq!(updated["commit"]["status"], "committed", "{updated}");
    let log = s.ok(
        "item-log",
        json!({"path": WIDGETS, "collection": "actions", "id": id}),
    );
    assert!(log["restore_guard"].is_string(), "{log}");
    let records = s.ok(
        "item-log",
        json!({"path": WIDGETS, "collection": "trackers", "id": id}),
    );
    assert!(records["restore_guard"].is_null(), "{records}");
    assert_eq!(
        records["restore_diagnostic"]["code"], "invalid_request",
        "{records}"
    );
    let before = read(&s.root, WIDGETS);
    let (code, out) = s.run(
        "revert-item",
        json!({"path": WIDGETS, "collection": "trackers", "id": id,
            "history_tip": log["history_tip"], "guard": log["restore_guard"]}),
    );
    assert_eq!(
        (code, out["error"]["code"].clone()),
        (2, json!("invalid_request")),
        "{out}"
    );
    assert_eq!(read(&s.root, WIDGETS), before);
    let reverted = s.ok(
        "revert-item",
        json!({"path": WIDGETS, "collection": "actions", "id": id,
            "history_tip": log["history_tip"], "guard": log["restore_guard"]}),
    );
    assert_eq!(reverted["commit"]["status"], "committed", "{reverted}");
    assert!(read(&s.root, WIDGETS).contains("- [ ] Keep this open."));
}

#[test]
fn the_prune_snapshot_admits_policy_sources_by_membership() {
    let s = Store::new("file-records-prune");
    let mut c = config(&s.root);
    c["prune"] = json!({"files": ["tracking/*.md", "bundle/**/*.md"]});
    set_config(&s.root, &c);
    s.git(&["commit", "-qam", "prune the bundle too"]);
    let (code, out) = s.run("prune-snapshot", json!({}));
    assert_eq!(code, 2, "{out}");
    assert_eq!(out["error"]["code"], "policy_conflict", "{out}");
    assert_eq!(out["error"]["path"], INDEX, "{out}");
    // Without the excluded files among its sources the snapshot is taken.
    c["prune"] = json!({"files": ["tracking/*.md", "bundle/margin.md", "bundle/orders/*.md"]});
    set_config(&s.root, &c);
    s.git(&["commit", "-qam", "prune members only"]);
    let snapshot = s.ok("prune-snapshot", json!({}));
    let inputs = snapshot.to_string();
    assert!(
        inputs.contains(MARGIN) && !inputs.contains(INDEX),
        "{snapshot}"
    );
}

/// Doctor's raw output on the example tracker, which binds no file records,
/// is the bytes the engine printed before file records existed (its root
/// replaced by a placeholder).
#[test]
fn doctor_on_the_example_tracker_prints_the_bytes_it_did_before() {
    let root = project("file-records-doctor-tracker");
    let mut c = command(&root, "doctor", &[]);
    let mut child = c.spawn().unwrap();
    use std::io::Write;
    child.stdin.take().unwrap().write_all(b"{}").unwrap();
    let out = child.wait_with_output().unwrap();
    let text = String::from_utf8(out.stdout).unwrap();
    let root_json = serde_json::to_string(&root.display().to_string()).unwrap();
    let text = text.replace(&root_json, "\"<root>\"");
    assert_eq!(text, include_str!("fixtures/doctor_tracker_5bacd0f.json"));
}

const HISTORY: &str = "history/2026-11/LOG_HISTORY.md";

/// A tracker whose Log bullets and Log subsections archive through a
/// `section_archives` binding, while the file is also a file record.
fn section_store(name: &str) -> Store {
    let root = records(name);
    let home = root.with_extension("isolated-home");
    std::fs::create_dir_all(&home).unwrap();
    let s = Store { root, home };
    with_trackers(&s.root);
    put(
        &s.root,
        WIDGETS,
        "---\nstatus: active\nowner: sam\n---\n# Synthetic\n\n## Action\n\n- [ ] Keep this open.\n\n## Log\n\n- **2026-11-12: New** Body.\n- **2026-10-06: Old** Older.\n\n### Entry\n\nRefresh the fixtures.\n",
    );
    put(&s.root, HISTORY, "# Log history\n\n## Log\n\n## Closed\n");
    for path in [WIDGETS, HISTORY] {
        let (_, map) = rivet(&s.root, "map", json!({"path": path}));
        let (code, out) = rivet(
            &s.root,
            "stamp",
            json!({"path": path, "guard": map["revision"], "select": {"kind": "document"}}),
        );
        assert_eq!(code, 0, "{out}");
    }
    let mut c = config(&s.root);
    c["section_archives"] = json!({"history": {"files": ["tracking/*.md"],
        "archive": ["history/**/*.md"], "items": {"headings": [["Log"]]}}});
    c["git"] = json!({"commit": "per-write", "on_dirty": "refuse"});
    set_config(&s.root, &c);
    s.git(&["init", "-q", "-b", "main"]);
    s.git(&["config", "user.name", "Synthetic Tester"]);
    s.git(&["config", "user.email", "tester@example.invalid"]);
    s.git(&["add", "-A"]);
    s.git(&["commit", "-qm", "synthetic fixture"]);
    s
}

#[test]
fn section_archive_copies_work_beside_a_file_binding() {
    let s = section_store("file-records-sections");
    let doc = markrivet_core::Document::parse(&read(&s.root, WIDGETS)).unwrap();
    let stay = |title: &str| {
        let i = doc
            .sections
            .iter()
            .position(|x| x.path.last().is_some_and(|t| t == title))
            .unwrap();
        doc.section_ids(i)[0].clone()
    };
    let item = doc.lists[1].items[1].markers[0];
    let item = doc.markers[item].lexical.id.clone().unwrap();
    let request = |id: &str, parent: &str, heading: &str| {
        let select = json!({"kind": "heading_path", "path": [heading]});
        let (_, dest) = s.run("get", json!({"path": HISTORY, "select": select}));
        json!({"source": WIDGETS, "id": id, "guard": item_guard(&s.root, WIDGETS, id),
            "parent": parent, "path": HISTORY, "select": select,
            "destination_guard": dest["revision"]})
    };
    let copied = s.ok("archive-copy", request(&item, &stay("Log"), "Log"));
    assert_eq!(copied["commit"]["status"], "committed", "{copied}");
    let copied = s.ok(
        "archive-copy",
        request(&stay("Entry"), &stay("Log"), "Closed"),
    );
    assert_eq!(copied["commit"]["status"], "committed", "{copied}");
    let history = read(&s.root, HISTORY);
    assert!(
        history.contains("2026-10-06: Old") && history.contains("### Entry"),
        "{history}"
    );
}

/// A file only an `exclude_files` entry keeps out of its collection is in no
/// live namespace, so it is no move endpoint either way.
#[test]
fn an_excluded_file_is_no_move_endpoint() {
    let s = fixture::MoveStore::new("tight", false);
    let mut c = s.config();
    c["collections"]["pages"] = json!({"schema": "tracker@1", "files": ["notes/*.md"], "exclude_files": ["notes/excluded.md"]});
    s.set_config(c);
    put(&s.root, ".rivet/schemas/tracker.json", TRACKER);
    put(&s.root, "notes/excluded.md", &s.read(fixture::B));
    s.git(&["add", "-A"]);
    s.git(&["commit", "-qm", "an excluded note"]);
    for field in ["destination", "source"] {
        let mut request = s.request();
        request[field] = json!("notes/excluded.md");
        s.refuses(request, "move_refused", Some(&format!("{field}_not_live")));
    }
}

// Record writes: `update-item` without `id` on a file record.

/// The record guard `map` hands a writer for `path` in `collection`.
fn record_guard(root: &Path, path: &str, collection: &str) -> String {
    let (code, map) = rivet(root, "map", json!({"path": path}));
    assert_eq!(code, 0, "{map}");
    map["collections"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == collection)
        .unwrap_or_else(|| panic!("{path} is not in {collection}: {map}"))["revision"]
        .as_str()
        .unwrap()
        .to_string()
}

fn update(root: &Path, path: &str, set: Value) -> Value {
    let guard = record_guard(root, path, "concepts");
    let (code, out) = rivet(
        root,
        "update-item",
        json!({"path": path, "collection": "concepts", "guard": guard, "set": set}),
    );
    assert_eq!(code, 0, "{out}");
    out
}

/// What `get` answers for every stay id the file carries: where each one
/// attaches, its unit's bytes and its revision.
fn stays(root: &Path, path: &str) -> Vec<Value> {
    let text = read(root, path);
    text.match_indices("stay:")
        .map(|(at, _)| {
            let id: String = text[at + 5..]
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'))
                .collect();
            let (code, got) = rivet(
                root,
                "get",
                json!({"path": path, "select": {"kind": "id", "id": id}}),
            );
            assert_eq!(code, 0, "{got}");
            got
        })
        .collect()
}

const RICH: &str = "bundle/rich.md";

/// A BOM, CRLF, quoted keys, comments, a block mapping and no final newline.
fn rich_text() -> String {
    "\u{feff}---\r\n\"type\": Metric # what it is\r\n'status': 'draft'\r\n# about tags\r\ntags: [finance] # kept\r\nrank: 4\r\npinned: true\r\ngenerated:\r\n  by: agent # who\r\n  at: 2026-10-01T00:00:00Z\r\n---\r\n# Rich\r\n\r\nA body line.\r\n\r\n- [ ] one\r\n- [ ] two".to_string()
}

#[test]
fn a_record_write_changes_exactly_its_keys() {
    let root = records("file-records-write-bytes");
    put(&root, RICH, &rich_text());
    let (_, map) = rivet(&root, "map", json!({"path": RICH}));
    let (code, out) = rivet(
        &root,
        "stamp",
        json!({"path": RICH, "select": {"kind": "document"}, "guard": map["revision"]}),
    );
    assert_eq!(code, 0, "{out}");
    let before = read(&root, RICH);
    let ids = stays(&root, RICH);
    assert!(ids.len() >= 3, "{ids:?}");
    let out = update(
        &root,
        RICH,
        json!({"status": "current", "generated_at": "2026-12-01T00:00:00Z",
               "stale_after": "2027-01-01T00:00:00Z", "rank": null, "tags": ["finance", "a, b"]}),
    );
    let after = read(&root, RICH);
    let want = before
        .replace("'status': 'draft'", "'status': 'current'")
        .replace("at: 2026-10-01T00:00:00Z", "at: 2026-12-01T00:00:00Z")
        .replace("rank: 4\r\n", "")
        .replace("tags: [finance] # kept", "tags: [finance, 'a, b'] # kept")
        .replace(
            "  at: 2026-12-01T00:00:00Z\r\n---",
            "  at: 2026-12-01T00:00:00Z\r\nstale_after: 2027-01-01T00:00:00Z\r\n---",
        );
    assert_eq!(after, want);
    let body = |s: &str| s[s.find("# Rich").unwrap()..].to_string();
    assert_eq!(body(&after), body(&before), "the body is byte-exact");
    assert_eq!(stays(&root, RICH), ids, "every stay keeps its attachment");
    // The response hands back the next record guard, and nothing typed for
    // a collection of items.
    let op = &out["ops"][0];
    assert_eq!(
        op["revision"],
        record_guard(&root, RICH, "concepts"),
        "{out}"
    );
    assert!(
        op.get("id").is_none() && op.get("collection_revision").is_none(),
        "{out}"
    );
    let read = query(&root, json!({"collection": "concepts", "paths": [RICH]}));
    assert_eq!(
        read["items"][0]["fields"],
        json!({"kind": "Metric", "status": "current", "stale_after": "2027-01-01T00:00:00Z",
               "tags": ["finance", "a, b"], "pinned": true,
               "generated_at": "2026-12-01T00:00:00Z"}),
        "{read}"
    );
}

const NOTE: &str = r#"{"id": "note", "version": 1, "item": "file", "fields": {
  "status":       {"type": "enum",      "from": "key", "key": "status", "values": ["draft", "done"], "default": "draft"},
  "owner":        {"type": "string",    "from": "key", "key": "owner"},
  "due":          {"type": "date",      "from": "key", "key": "due"},
  "generated_by": {"type": "string",    "from": "key", "key": "generated", "field": "by"},
  "generated_at": {"type": "timestamp", "from": "key", "key": "generated", "field": "at"}}}"#;

/// A second file collection, every field optional, over `notes/`.
fn with_notes(root: &Path) {
    let mut c = config(root);
    c["collections"]["notes"] = json!({"schema": "note@1", "files": ["notes/*.md"]});
    set_config(root, &c);
    put(root, ".rivet/schemas/note.json", NOTE);
}

fn note(root: &Path, path: &str, set: Value) -> (i32, Value) {
    let guard = record_guard(root, path, "notes");
    rivet(
        root,
        "update-item",
        json!({"path": path, "collection": "notes", "guard": guard, "set": set}),
    )
}

#[test]
fn record_writes_compose_their_key_edits_exactly() {
    let root = records("file-records-compose");
    with_notes(&root);
    let cases = [
        // Removing the last key and adding two: one replacement at the end.
        (
            "---\nowner: sam\ndue: 2026-12-01\n---\n# A\n",
            json!({"due": null, "status": "done", "owner": "sam"}),
            None,
        ),
        (
            "---\nowner: sam\ndue: 2026-12-01\n---\n# A\n",
            json!({"due": null, "status": "done", "owner": "kim"}),
            Some("---\nowner: kim\nstatus: done\n---\n# A\n"),
        ),
        (
            "---\ndue: 2026-12-01\nowner: sam\n# trailing\n# comments\n---\n# A\n",
            json!({"owner": null, "status": "done"}),
            Some("---\ndue: 2026-12-01\n# trailing\n# comments\nstatus: done\n---\n# A\n"),
        ),
        // Sibling mapping fields: one edit of their key, on a flow line.
        (
            "---\ngenerated: {by: agent, at: 2026-10-01T00:00:00Z} # g\nowner: sam\n---\n# A\n",
            json!({"generated_by": "human:kim", "generated_at": "2026-12-01T00:00:00+13:00"}),
            Some(
                "---\ngenerated: {by: human:kim, at: 2026-12-01T00:00:00+13:00} # g\nowner: sam\n---\n# A\n",
            ),
        ),
        // ... and on a block mapping, with standalone comments at the end.
        (
            "---\nowner: sam\ngenerated:\n  by: agent\n  # between\n  at: 2026-10-01T00:00:00Z\n# trailing\n---\n# A\n",
            json!({"generated_by": "kim", "generated_at": "2026-12-01T00:00:00Z", "due": "2027-01-01"}),
            Some(
                "---\nowner: sam\ngenerated:\n  by: kim\n  # between\n  at: 2026-12-01T00:00:00Z\n# trailing\ndue: 2027-01-01\n---\n# A\n",
            ),
        ),
        (
            "---\nowner: sam\ngenerated:\n  by: agent\n  at: 2026-10-01T00:00:00Z\n---\n# A\n",
            json!({"generated_by": "kim", "generated_at": "2026-12-01T00:00:00Z", "owner": null}),
            Some("---\ngenerated:\n  by: kim\n  at: 2026-12-01T00:00:00Z\n---\n# A\n"),
        ),
        // `null` removes a key that is present but empty or `null`; on an
        // absent key it is no edit.
        (
            "---\nowner:\ndue: null\nstatus: done\n---\n# A\n",
            json!({"owner": null, "due": null}),
            Some("---\nstatus: done\n---\n# A\n"),
        ),
        (
            "---\nstatus: done\n---\n# A\n",
            json!({"owner": null, "due": null}),
            Some("---\nstatus: done\n---\n# A\n"),
        ),
    ];
    for (n, (text, set, want)) in cases.into_iter().enumerate() {
        let path = format!("notes/n{n}.md");
        put(&root, &path, text);
        let (code, out) = note(&root, &path, set.clone());
        match want {
            Some(want) => {
                assert_eq!(code, 0, "{set}: {out}");
                assert_eq!(read(&root, &path), want, "{set}");
            }
            None => {
                // A current value is no edit; removing `due` and adding
                // `status` still land.
                assert_eq!(code, 0, "{set}: {out}");
                assert_eq!(
                    read(&root, &path),
                    "---\nowner: sam\nstatus: done\n---\n# A\n",
                    "{set}"
                );
            }
        }
    }
    // Removing the last key while adding several: the added keys go in by
    // key name, where the removed lines were, or after trailing comments.
    for (n, (text, want)) in [
        (
            "---\ntype: Metric\nrank: 1\n---\n# A\n",
            "---\ntype: Metric\npinned: false\nstatus: draft\ntags: [a, b]\n---\n# A\n",
        ),
        (
            "---\ntype: Metric\nrank: 1\n# trailing\n---\n# A\n",
            "---\ntype: Metric\n# trailing\npinned: false\nstatus: draft\ntags: [a, b]\n---\n# A\n",
        ),
        (
            "---\r\ntype: Metric\r\nrank: 1 # r\r\n---\r\n# A",
            "---\r\ntype: Metric\r\npinned: false\r\nstatus: draft\r\ntags: [a, b]\r\n---\r\n# A",
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let path = format!("bundle/c{n}.md");
        put(&root, &path, text);
        update(
            &root,
            &path,
            json!({"rank": null, "tags": ["a", "b"], "status": "draft", "pinned": false}),
        );
        assert_eq!(read(&root, &path), want);
    }
}

/// A refusal leaves every file byte-identical and answers `code`.
fn refuse_note(root: &Path, path: &str, guard: &str, set: Value, code: &str) -> Value {
    refuse(
        root,
        "update-item",
        json!({"path": path, "collection": "notes", "guard": guard, "set": set}),
        code,
    )
}

fn message(out: &Value) -> &str {
    out["error"]["message"].as_str().unwrap()
}

#[test]
fn every_record_write_refusal_leaves_the_file_byte_identical() {
    let root = records("file-records-write-refusals");
    with_notes(&root);
    let g = record_guard(&root, MARGIN, "concepts");
    let concepts =
        |set: Value| json!({"path": MARGIN, "collection": "concepts", "guard": g, "set": set});
    for (set, code, says) in [
        (json!({"rank": "one"}), "invalid_item", "rank"),
        (json!({"status": "bogus"}), "invalid_item", "status"),
        (json!({"kind": null}), "invalid_item", "kind is required"),
        (json!({"nope": 1}), "invalid_request", "nope is not a field"),
        (
            json!({"generated_at": null}),
            "unsupported_syntax",
            "never removed",
        ),
        (
            json!({"verified": [{"by": "x"}]}),
            "unsupported_syntax",
            "entries",
        ),
        // An identical `entries` value is still a write of that form.
        (
            json!({"verified": [{"by": "human:sam", "at": "2026-11-02T10:00:00Z"}]}),
            "unsupported_syntax",
            "entries",
        ),
    ] {
        let out = refuse(&root, "update-item", concepts(set.clone()), code);
        assert!(message(&out).contains(says), "{set}: {out}");
    }
    let mut field = concepts(json!({"status": "retired"}));
    field["guard_scope"] = json!("field");
    refuse(&root, "update-item", field, "unsupported_syntax");
    // A stale guard and a guard of another kind conflict, with `current`.
    let (_, map) = rivet(&root, "map", json!({"path": MARGIN}));
    let mut other = concepts(json!({"status": "retired"}));
    other["guard"] = map["revision"].clone();
    let out = refuse(&root, "update-item", other, "guard_conflict");
    assert!(message(&out).contains("needs a record revision"), "{out}");
    assert_eq!(out["error"]["current"], g.as_str(), "{out}");
    put(&root, MARGIN, &MARGIN_TEXT.replace("rank: 1", "rank: 2"));
    let out = refuse(
        &root,
        "update-item",
        concepts(json!({"status": "retired"})),
        "guard_conflict",
    );
    assert_eq!(
        out["error"]["current"],
        record_guard(&root, MARGIN, "concepts")
    );
    // A block list is read, not written, even with an identical value; a
    // stale guard refuses first.
    let churn = record_guard(&root, CHURN, "concepts");
    for (tags, guard, code) in [
        (
            json!(["retention", "x"]),
            churn.as_str(),
            "unsupported_syntax",
        ),
        (json!(["retention"]), churn.as_str(), "unsupported_syntax"),
        (json!(["retention"]), g.as_str(), "guard_conflict"),
    ] {
        refuse(
            &root,
            "update-item",
            json!({"path": CHURN, "collection": "concepts", "guard": guard, "set": {"tags": tags}}),
            code,
        );
    }
    let out = refuse(
        &root,
        "update-item",
        json!({"path": MARGIN, "collection": "concepts", "guard": g,
               "set": {"verified": [{"by": "human:sam", "at": "2026-11-02T10:00:00Z"}]}}),
        "guard_conflict",
    );
    assert!(message(&out).contains("changed since"), "{out}");
    // An absent mapping parent, an absent field, an empty field value.
    for (text, set, code) in [
        (
            "---\nowner: sam\n---\n",
            json!({"generated_at": "2026-12-01T00:00:00Z"}),
            "missing_selector",
        ),
        (
            "---\ngenerated: {by: agent}\n---\n",
            json!({"generated_at": "2026-12-01T00:00:00Z"}),
            "missing_selector",
        ),
        (
            "---\ngenerated: {by: agent, at: }\n---\n",
            json!({"generated_at": "2026-12-01T00:00:00Z"}),
            "unsupported_syntax",
        ),
        // An empty parent reads as absent, and holds no mapping to write.
        (
            "---\ngenerated:\nowner: sam\n---\n",
            json!({"generated_at": "2026-12-01T00:00:00Z"}),
            "unsupported_syntax",
        ),
        // An empty value is no single-line scalar to rewrite.
        (
            "---\nowner:\ndue: 2026-12-01\n---\n",
            json!({"owner": "sam"}),
            "unsupported_syntax",
        ),
        // Removing the only key would turn the fences into Markdown, an
        // empty or `null` one included.
        (
            "---\nowner: sam\n---\n# A\n",
            json!({"owner": null}),
            "unsupported_syntax",
        ),
        (
            "---\nowner:\n---\n# A\n",
            json!({"owner": null}),
            "unsupported_syntax",
        ),
        (
            "---\nowner: null\n---\n# A\n",
            json!({"owner": null}),
            "unsupported_syntax",
        ),
        (
            "---\nowner: sam\ndue: 2026-12-01\n---\n",
            json!({"owner": null, "due": null}),
            "unsupported_syntax",
        ),
        // A record invalid before the edit is fixed with native key writes.
        (
            "---\nowner: sam\ndue: soon\n---\n",
            json!({"owner": "kim"}),
            "invalid_item",
        ),
        // No frontmatter, and frontmatter that does not read.
        ("# Bare\n", json!({"owner": "kim"}), "unsupported_syntax"),
        (
            "---\nowner: !!str sam\n---\n",
            json!({"owner": "kim"}),
            "unsupported_syntax",
        ),
    ] {
        put(&root, "notes/r.md", text);
        let guard = record_guard(&root, "notes/r.md", "notes");
        refuse_note(&root, "notes/r.md", &guard, set.clone(), code);
    }
    // A renamed record is gone: its old path's guard finds no file.
    put(&root, "notes/old.md", "---\nowner: sam\n---\n");
    let guard = record_guard(&root, "notes/old.md", "notes");
    std::fs::rename(root.join("notes/old.md"), root.join("notes/new.md")).unwrap();
    refuse_note(
        &root,
        "notes/old.md",
        &guard,
        json!({"owner": "kim"}),
        "not_found",
    );
    // A checkbox collection takes no record write; a file record takes no
    // typed insert.
    let out = refuse(
        &root,
        "update-item",
        json!({"path": WIDGETS, "collection": "actions", "guard": g, "set": {"done": true}}),
        "invalid_request",
    );
    assert!(message(&out).ends_with("update_item names its item by exactly one of id and select"));
}

#[test]
fn a_record_write_whose_values_are_current_is_unchanged() {
    let root = records("file-records-noop");
    let g = record_guard(&root, MARGIN, "concepts");
    let set = json!({"status": "current", "tags": ["finance", "kpi"], "rank": 1,
                     "generated_at": "2026-11-01T09:30:00+13:00", "reviewed": "2026-11-02"});
    let (code, out) = rivet(
        &root,
        "update-item",
        json!({"path": MARGIN, "collection": "concepts", "guard": g, "set": set}),
    );
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["status"], "unchanged", "{out}");
    assert_eq!(read(&root, MARGIN), MARGIN_TEXT);
    put(
        &root,
        MARGIN,
        &MARGIN_TEXT.replace("title: Gross margin", "title: GM"),
    );
    refuse(
        &root,
        "update-item",
        json!({"path": MARGIN, "collection": "concepts", "guard": g, "set": set}),
        "guard_conflict",
    );
}

#[test]
fn a_record_write_combines_with_body_ops_but_not_frontmatter_ops() {
    let root = records("file-records-batch");
    let g = record_guard(&root, MARGIN, "concepts");
    let (_, got) = rivet(
        &root,
        "get",
        json!({"path": MARGIN, "select": {"kind": "heading_path", "path": ["Gross margin"]}}),
    );
    let section = got["revision"].clone();
    assert!(section.is_string(), "{got}");
    let record = json!({"op": "update_item", "collection": "concepts", "guard": g,
                        "set": {"status": "retired"}});
    let body = json!({"op": "append", "select": {"kind": "heading_path", "path": ["Gross margin"]},
                      "guard": section, "content": "Retired 2026-11-28."});
    let (_, map) = rivet(&root, "map", json!({"path": MARGIN}));
    let key = json!({"op": "add_key", "key": "note", "value": "x", "guard": map["revision"]});
    let out = refuse(
        &root,
        "write",
        json!({"path": MARGIN, "ops": [record.clone(), key]}),
        "invalid_request",
    );
    assert!(message(&out).contains("same unit"), "{out}");
    let (code, out) = rivet(
        &root,
        "write",
        json!({"path": MARGIN, "ops": [record, body]}),
    );
    assert_eq!(code, 0, "{out}");
    assert_eq!(
        read(&root, MARGIN),
        MARGIN_TEXT
            .replace("status: current", "status: retired")
            .replace(
                "cost of goods.\n",
                "cost of goods.\n\nRetired 2026-11-28.\n"
            )
    );
}

/// The record write § Record writes shows, on the fixture it names.
#[test]
fn the_documented_record_write_lands() {
    let root = records("file-records-documented");
    let out = update(
        &root,
        MARGIN,
        json!({"status": "retired", "stale_after": "2027-06-30T00:00:00Z"}),
    );
    assert_eq!(out["status"], "applied", "{out}");
    assert_eq!(
        read(&root, MARGIN),
        MARGIN_TEXT
            .replace("status: current", "status: retired")
            .replace(
                "stale_after: 2026-12-31T00:00:00Z",
                "stale_after: 2027-06-30T00:00:00Z"
            )
    );
}

// Entries appends: `append` adds one flat mapping to an `entries` field in
// the same change as `set`.

fn record_write(root: &Path, path: &str, set: Value, append: Value) -> (i32, Value) {
    let guard = record_guard(root, path, "concepts");
    rivet(
        root,
        "update-item",
        json!({"path": path, "collection": "concepts", "guard": guard, "set": set,
               "append": append}),
    )
}

fn appended(root: &Path, path: &str, set: Value, append: Value) -> Value {
    let (code, out) = record_write(root, path, set, append);
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["status"], "applied", "{out}");
    out
}

const ENTRY: &str = "{by: \"human:kim\", at: 2026-12-01T00:00:00Z}";

/// Into a flow and a block `verified` and onto an absent key, each beside a
/// `set`: exact bytes, the body byte-exact and every stay still attached.
#[test]
fn an_append_lands_in_each_form_beside_a_set() {
    let root = records("file-records-append-forms");
    let flow = "bundle/flow.md";
    put(
        &root,
        flow,
        "---\ntype: Metric\nverified: [{by: agent, at: 2026-10-01T00:00:00Z}] # v\nstatus: draft\n---\n# Flow\n\n- [ ] one\n- [ ] two\n",
    );
    let entry = json!({"by": "human:kim", "at": "2026-12-01T00:00:00Z"});
    let cases = [
        (
            flow,
            json!({"status": "current"}),
            "---\ntype: Metric\nverified: [{by: agent, at: 2026-10-01T00:00:00Z}, {by: human:kim, at: 2026-12-01T00:00:00Z}] # v\nstatus: current\n---\n# Flow\n\n- [ ] one\n- [ ] two\n".to_string(),
        ),
        (
            MARGIN,
            json!({"stale_after": "2027-06-30T00:00:00Z"}),
            MARGIN_TEXT
                .replace("2026-12-31T00:00:00Z", "2027-06-30T00:00:00Z")
                .replace(
                    "10:00:00Z}\n---",
                    &format!("10:00:00Z}}\n  - {ENTRY}\n---"),
                ),
        ),
        // Absent: added among the new keys, by key name.
        (
            AOV,
            json!({"stale_after": "2027-01-01T00:00:00Z", "pinned": true}),
            "---\ntype: Metric\nstatus: retired\npinned: true\nstale_after: 2027-01-01T00:00:00Z\nverified: [{at: 2026-12-01T00:00:00Z, by: human:kim}]\n---\n# Average order value\n".to_string(),
        ),
    ];
    for (path, set, want) in cases {
        let (_, map) = rivet(&root, "map", json!({"path": path}));
        let (code, out) = rivet(
            &root,
            "stamp",
            json!({"path": path, "select": {"kind": "document"}, "guard": map["revision"]}),
        );
        assert_eq!(code, 0, "{out}");
        let before = read(&root, path);
        let ids = stays(&root, path);
        assert!(!ids.is_empty(), "{path}");
        let want = {
            // The stamp's markers sit in the body; the frontmatter is as
            // written above.
            let body = |s: &str| s[s.find("\n---\n").unwrap()..].to_string();
            let head = |s: &str| s[..s.find("\n---\n").unwrap()].to_string();
            format!("{}{}", head(&want), body(&before))
        };
        let out = appended(&root, path, set, json!({"verified": entry}));
        let after = read(&root, path);
        assert_eq!(after, want, "{path}");
        // An appended line moves the body down; nothing else may change.
        let unlined = |v: Vec<Value>| -> Vec<Value> {
            v.into_iter()
                .map(|mut g| {
                    let t = g["target"].as_object_mut().unwrap();
                    t.remove("line");
                    t.remove("end_line");
                    g
                })
                .collect()
        };
        assert_eq!(
            unlined(stays(&root, path)),
            unlined(ids),
            "{path}: every stay keeps its attachment"
        );
        assert_eq!(
            out["ops"][0]["revision"],
            record_guard(&root, path, "concepts"),
            "{out}"
        );
        let read = query(&root, json!({"collection": "concepts", "paths": [path]}));
        let verified = read["items"][0]["fields"]["verified"].as_array().unwrap();
        assert_eq!(verified.last(), Some(&entry), "{read}");
    }
}

/// An append to a block `verified` that is the last key, beside a key the
/// same request adds: the entry under `verified`, the new key after it,
/// after any trailing comments.
#[test]
fn adding_a_key_while_appending_puts_the_entry_first() {
    let root = records("file-records-append-add");
    for (n, (text, want)) in [
        (
            "---\ntype: Metric\nverified:\n  - by: agent\n    at: 2026-10-01T00:00:00Z\n---\n# A\n",
            "---\ntype: Metric\nverified:\n  - by: agent\n    at: 2026-10-01T00:00:00Z\n  - by: human:kim\n    at: 2026-12-01T00:00:00Z\nstatus: draft\n---\n# A\n",
        ),
        (
            "---\ntype: Metric\nverified:\n  - by: agent\n    at: 2026-10-01T00:00:00Z\n# trailing\n# comments\n---\n# A\n",
            "---\ntype: Metric\nverified:\n  - by: agent\n    at: 2026-10-01T00:00:00Z\n  - by: human:kim\n    at: 2026-12-01T00:00:00Z\n# trailing\n# comments\nstatus: draft\n---\n# A\n",
        ),
        (
            "---\r\ntype: Metric\r\nverified:\r\n- {by: agent, at: 2026-10-01T00:00:00Z}\r\n---\r\n# A",
            "---\r\ntype: Metric\r\nverified:\r\n- {by: agent, at: 2026-10-01T00:00:00Z}\r\n- {by: human:kim, at: 2026-12-01T00:00:00Z}\r\nstatus: draft\r\n---\r\n# A",
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let path = format!("bundle/add{n}.md");
        put(&root, &path, text);
        appended(
            &root,
            &path,
            json!({"status": "draft"}),
            json!({"verified": {"by": "human:kim", "at": "2026-12-01T00:00:00Z"}}),
        );
        assert_eq!(read(&root, &path), want, "{text:?}");
    }
}

/// Removing the key that follows a block sequence while appending to it:
/// the entry lands where the removed lines were, standalone comments stay,
/// and keys the same request adds still go in at the end.
#[test]
fn deleting_the_key_after_a_block_sequence_while_appending_to_it() {
    let root = records("file-records-append-remove");
    let entry = json!({"verified": {"by": "human:kim", "at": "2026-12-01T00:00:00Z"}});
    for (n, (text, set, want)) in [
        (
            "---\ntype: Metric\nverified:\n  - {by: agent, at: 2026-10-01T00:00:00Z}\nrank: 1\n# kept\nstatus: draft\n---\n# A\n",
            json!({"rank": null}),
            "---\ntype: Metric\nverified:\n  - {by: agent, at: 2026-10-01T00:00:00Z}\n  - {by: human:kim, at: 2026-12-01T00:00:00Z}\n# kept\nstatus: draft\n---\n# A\n",
        ),
        // The removed key is the last: the entry, then the added keys.
        (
            "---\ntype: Metric\nverified:\n  - {by: agent, at: 2026-10-01T00:00:00Z}\nrank: 1\n---\n# A\n",
            json!({"rank": null, "status": "draft", "pinned": false}),
            "---\ntype: Metric\nverified:\n  - {by: agent, at: 2026-10-01T00:00:00Z}\n  - {by: human:kim, at: 2026-12-01T00:00:00Z}\npinned: false\nstatus: draft\n---\n# A\n",
        ),
        // ... with comments of its own after it, which stay.
        (
            "---\ntype: Metric\nverified:\n  - {by: agent, at: 2026-10-01T00:00:00Z}\nrank: 1 # r\n# trailing\n---\n# A\n",
            json!({"rank": null, "status": "draft"}),
            "---\ntype: Metric\nverified:\n  - {by: agent, at: 2026-10-01T00:00:00Z}\n  - {by: human:kim, at: 2026-12-01T00:00:00Z}\n# trailing\nstatus: draft\n---\n# A\n",
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let path = format!("bundle/rm{n}.md");
        put(&root, &path, text);
        appended(&root, &path, set.clone(), entry.clone());
        assert_eq!(read(&root, &path), want, "{set}");
    }
}

/// Every refusal an append can meet leaves the project byte-identical:
/// `append_key_item`'s own, the record's request shape, and the shapes a
/// record write never rewrites.
#[test]
fn every_append_refusal_leaves_the_project_byte_identical() {
    let root = records("file-records-append-refusals");
    let good = json!({"by": "human:kim", "at": "2026-12-01T00:00:00Z"});
    let g = record_guard(&root, MARGIN, "concepts");
    let request = |set: Value, append: Value| {
        json!({"path": MARGIN, "collection": "concepts", "guard": g, "set": set,
               "append": append})
    };
    for (set, append, code, says) in [
        // The request's shape.
        (
            json!({}),
            json!({"nope": good}),
            "invalid_request",
            "nope is not a field",
        ),
        (
            json!({}),
            json!({"tags": good}),
            "invalid_request",
            "does not hold entries",
        ),
        (
            json!({"verified": []}),
            json!({"verified": good}),
            "invalid_request",
            "both set and appended",
        ),
        // `append_key_item`'s item rules.
        (
            json!({}),
            json!({"verified": {}}),
            "invalid_request",
            "nonempty",
        ),
        (
            json!({}),
            json!({"verified": {"by": ["x"]}}),
            "invalid_request",
            "must be a string",
        ),
        (
            json!({}),
            json!({"verified": {"true": "x"}}),
            "invalid_request",
            "read back",
        ),
        (
            json!({}),
            json!({"verified": {"by": "x\ny"}}),
            "unsupported_syntax",
            "single-line",
        ),
        // A `set` the append rides with is still checked.
        (
            json!({"status": "bogus"}),
            json!({"verified": good}),
            "invalid_item",
            "status",
        ),
    ] {
        let out = refuse(
            &root,
            "update-item",
            request(set.clone(), append.clone()),
            code,
        );
        assert!(message(&out).contains(says), "{set} {append}: {out}");
    }
    // Not a mapping: the request does not parse.
    let before = snapshot(&root);
    for item in [json!("by: x"), json!([{"by": "x"}])] {
        let (code, out) = rivet(
            &root,
            "update-item",
            request(json!({}), json!({"verified": item})),
        );
        assert_eq!(code, 1, "{out}");
    }
    assert_eq!(snapshot(&root), before);
    // A stale guard refuses first, append or not.
    put(&root, MARGIN, &MARGIN_TEXT.replace("rank: 1", "rank: 2"));
    let out = refuse(
        &root,
        "update-item",
        request(json!({}), json!({"verified": good})),
        "guard_conflict",
    );
    assert!(message(&out).contains("changed since"), "{out}");
    // Shapes `append_key_item` does not append to, and the bare mapping a
    // record reads as one entry, which append never rewrites.
    for (n, (verified, code, says)) in [
        (
            "verified:\n",
            "unsupported_syntax",
            "flow or block sequence",
        ),
        (
            "verified: [{by: a, at: 2026-10-01T00:00:00Z},\n  {by: b, at: 2026-10-02T00:00:00Z}]\n",
            "unsupported_syntax",
            "flow or block sequence",
        ),
        (
            "verified: {by: a, at: 2026-10-01T00:00:00Z}\n",
            "unsupported_syntax",
            "bare mapping",
        ),
        (
            "verified:\n  - {by: a}\n  - b\n",
            "invalid_item",
            "invalid before the edit",
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let path = format!("bundle/shape{n}.md");
        put(
            &root,
            &path,
            &format!("---\ntype: Metric\n{verified}status: draft\n---\n# S\n"),
        );
        let guard = record_guard(&root, &path, "concepts");
        let out = refuse(
            &root,
            "update-item",
            json!({"path": path, "collection": "concepts", "guard": guard,
                   "set": {"status": "current"}, "append": {"verified": good}}),
            code,
        );
        assert!(message(&out).contains(says), "{verified}: {out}");
        if says == "bare mapping" {
            assert!(message(&out).contains("list of entries"), "{out}");
        }
    }
    // The churn fixture's bare mapping reads as one entry, and append
    // refuses it the same way.
    let churn = record_guard(&root, CHURN, "concepts");
    refuse(
        &root,
        "update-item",
        json!({"path": CHURN, "collection": "concepts", "guard": churn, "set": {},
               "append": {"verified": good}}),
        "unsupported_syntax",
    );
    // A checkbox collection takes no append.
    let (_, q) = rivet(&root, "query", json!({"collection": "actions"}));
    let item = &q["items"][0];
    let out = refuse(
        &root,
        "update-item",
        json!({"path": item["path"], "collection": "actions", "id": item["id"],
               "guard": item["revision"], "set": {}, "append": {"verified": good}}),
        "invalid_request",
    );
    assert!(message(&out).contains("file records only"), "{out}");
}

/// An append is never a no-op: with every `set` value current it still
/// writes the entry.
#[test]
fn an_append_with_current_values_still_writes() {
    let root = records("file-records-append-current");
    appended(
        &root,
        MARGIN,
        json!({"status": "current"}),
        json!({"verified": {"by": "human:kim", "at": "2026-12-01T00:00:00Z"}}),
    );
    assert_eq!(
        read(&root, MARGIN),
        MARGIN_TEXT.replace("10:00:00Z}\n---", &format!("10:00:00Z}}\n  - {ENTRY}\n---"))
    );
}

/// The append § Record writes shows, on the fixture it names.
#[test]
fn the_documented_record_append_lands() {
    let root = records("file-records-documented-append");
    appended(
        &root,
        MARGIN,
        json!({"stale_after": "2027-06-30T00:00:00Z"}),
        json!({"verified": {"by": "human:sam", "at": "2026-11-22T10:00:00Z"}}),
    );
    assert_eq!(
        read(&root, MARGIN),
        MARGIN_TEXT
            .replace("2026-12-31T00:00:00Z", "2027-06-30T00:00:00Z")
            .replace(
                "10:00:00Z}\n---",
                "10:00:00Z}\n  - {by: \"human:sam\", at: 2026-11-22T10:00:00Z}\n---"
            )
    );
}

/// `examples/records/walkthrough.sh`, which usage.md's records walkthrough
/// abbreviates, passes against this build.
#[test]
fn the_records_walkthrough_passes() {
    use std::process::Command;
    if Command::new("jq").arg("--version").output().is_err() {
        eprintln!("skipping: the walkthrough needs jq");
        return;
    }
    let script =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/records/walkthrough.sh");
    let mut command = Command::new("bash");
    let out = common::without_debug_hooks(&mut command)
        .arg(script)
        .env("RIVET", env!("CARGO_BIN_EXE_markrivet"))
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{text}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(text.ends_with("walkthrough ok\n"), "{text}");
}
