//! `refresh`: rewrite named stays' stored digests and no other byte. Every
//! refusal leaves the file byte-identical.
mod common;
use common::*;
use markrivet_core::Document;
use markrivet_core::document::Attach;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

const T: &str = "tracking/T.md";
const ZERO: &str = "000000000000";

fn put(name: &str, text: &str) -> PathBuf {
    let root = project(name);
    std::fs::write(root.join(T), text).unwrap();
    root
}

fn guard(root: &Path) -> String {
    let (code, out) = rivet(root, "map", json!({"path": T}));
    assert_eq!(code, 0, "{out}");
    out["revision"].as_str().unwrap().to_string()
}

fn select(pairs: &[(&str, &str)]) -> Value {
    pairs
        .iter()
        .map(|(id, expect)| json!({"id": id, "expect": expect}))
        .collect()
}

fn refresh(root: &Path, pairs: &[(&str, &str)], preview: bool) -> (i32, Value) {
    rivet(
        root,
        "refresh",
        json!({"path": T, "guard": guard(root), "select": select(pairs), "preview": preview}),
    )
}

/// The unit's current full digest for the stay `id` (its only live marker).
fn fresh(text: &str, id: &str) -> String {
    let doc = Document::parse(text).unwrap();
    let i = doc
        .markers
        .iter()
        .position(|m| m.lexical.id.as_deref() == Some(id))
        .unwrap();
    doc.marker_evidence(i).unwrap().0
}

fn drifted(text: &str, id: &str) -> bool {
    let doc = Document::parse(text).unwrap();
    let i = doc
        .markers
        .iter()
        .position(|m| m.lexical.id.as_deref() == Some(id))
        .unwrap();
    let (fresh, stored) = doc.marker_evidence(i).unwrap();
    !fresh.starts_with(&stored.unwrap())
}

/// The id -> attachment map, so a write can be shown to keep every owner.
fn owners(text: &str) -> Vec<(Option<String>, String)> {
    let doc = Document::parse(text).unwrap();
    doc.markers
        .iter()
        .map(|m| {
            let attach = match m.attach {
                Attach::Block(b) => format!("block {}", doc.blocks[b].span.start),
                Attach::Item { list, item } => format!("item {list}.{item}"),
                other => format!("{other:?}"),
            };
            (m.lexical.id.clone(), attach)
        })
        .collect()
}

/// `before` with exactly the named `stay:<id> <key>=sha256:<old>` values replaced.
fn expected(before: &str, rewrites: &[(&str, &str, &str, &str)]) -> String {
    let mut out = before.to_string();
    for (id, key, old, new) in rewrites {
        let from = format!("stay:{id} {key}=sha256:{old}");
        assert_eq!(out.matches(&from).count(), 1, "{from}");
        out = out.replace(&from, &format!("stay:{id} {key}=sha256:{new}"));
    }
    out
}

/// A refused request: exit 2, the code, and the file byte-identical.
fn refused(root: &Path, request: Value, code: &str) -> Value {
    let before = std::fs::read(root.join(T)).unwrap();
    let (exit, out) = rivet(root, "refresh", request);
    assert_eq!(exit, 2, "{out}");
    assert_eq!(out["error"]["code"], code, "{out}");
    assert_eq!(
        std::fs::read(root.join(T)).unwrap(),
        before,
        "refusal changed bytes: {out}"
    );
    out
}

fn request(root: &Path, pairs: &[(&str, &str)]) -> Value {
    json!({"path": T, "guard": guard(root), "select": select(pairs)})
}

const SHAPES: &str = "# Titlé ✓\n<!-- stay:hd000001 hash=sha256:000000000000 -->\n\nPara ünïcode. <!-- stay:pa000001 hash=sha256:000000000000 -->\n\n- one ✓ <!-- stay:it000001 subhash=sha256:000000000000 -->\n\n- two <!-- stay:it000002 subhash=sha256:000000000000 -->\n<!-- stay:li000001 hash=sha256:000000000000 -->\n";

#[test]
fn block_heading_item_and_loose_list_stays_refresh_only_their_digests() {
    for crlf in [false, true] {
        let text = if crlf {
            SHAPES.replace('\n', "\r\n")
        } else {
            SHAPES.to_string()
        };
        let root = put(&format!("refresh-shapes-{crlf}"), &text);
        let named = ["hd000001", "pa000001", "it000001", "li000001"];
        let pairs: Vec<(&str, &str)> = named.iter().map(|id| (*id, ZERO)).collect();
        let (code, preview) = refresh(&root, &pairs, true);
        assert_eq!(code, 0, "{preview}");
        assert_eq!(preview["status"], "preview");
        assert_eq!(read(&root, T), text, "a preview writes nothing");
        let (code, out) = refresh(&root, &pairs, false);
        assert_eq!(code, 0, "{out}");
        assert_eq!(out["status"], "applied", "{out}");
        assert_eq!(out["ops"][0]["refreshed"], preview["ops"][0]["refreshed"]);
        assert_eq!(out["evidence_refreshed"], 4);
        assert!(
            out.get("drift").is_none(),
            "named rewrites are not news: {out}"
        );
        let rows = out["ops"][0]["refreshed"].as_array().unwrap();
        let mut rewrites = vec![];
        for (row, (id, owner, key)) in rows.iter().zip([
            ("hd000001", "section", "hash"),
            ("pa000001", "block", "hash"),
            ("it000001", "item", "subhash"),
            ("li000001", "list", "hash"),
        ]) {
            assert_eq!(row["id"], id);
            assert_eq!(row["owner"], owner, "{row}");
            assert_eq!(row["key"], key);
            assert_eq!(row["stored"], ZERO);
            assert_eq!(row["status"], "refreshed");
            let want = fresh(&text, id)[..12].to_string();
            assert_eq!(row["fresh"], want.as_str());
            rewrites.push((id, key, ZERO, want));
        }
        let rewrites: Vec<(&str, &str, &str, &str)> = rewrites
            .iter()
            .map(|(a, b, c, d)| (*a, *b, *c, d.as_str()))
            .collect();
        let after = read(&root, T);
        assert_eq!(after, expected(&text, &rewrites));
        assert_eq!(owners(&after), owners(&text));
        for id in named {
            assert!(!drifted(&after, id), "{id} is current");
        }
        assert!(drifted(&after, "it000002"), "unnamed drift is left alone");
        // An item refresh never moves its list's digest: the list stays current.
        let (code, again) = refresh(&root, &[("li000001", rewrites[3].3)], false);
        assert_eq!(code, 0, "{again}");
        assert_eq!(again["status"], "unchanged", "{again}");
        assert_eq!(again["ops"][0]["refreshed"][0]["status"], "unchanged");
    }
}

#[test]
fn short_and_full_precision_keep_their_length_and_overlong_refuses() {
    let full = "0".repeat(64);
    let text = format!(
        "# Short\n<!-- stay:short001 hash=sha256:abc -->\n\n# Full\n<!-- stay:full0001 hash=sha256:{full} -->\n"
    );
    let root = put("refresh-precision", &text);
    let (code, out) = refresh(&root, &[("short001", "abc"), ("full0001", &full)], false);
    assert_eq!(code, 0, "{out}");
    let short = fresh(&text, "short001")[..3].to_string();
    let long = fresh(&text, "full0001");
    assert_eq!(long.len(), 64);
    assert_eq!(
        read(&root, T),
        expected(
            &text,
            &[
                ("short001", "hash", "abc", &short),
                ("full0001", "hash", &full, &long)
            ]
        )
    );
    let over = "0".repeat(65);
    let root = put(
        "refresh-overlong",
        &format!("# Over\n<!-- stay:over0001 hash=sha256:{over} -->\n"),
    );
    let out = refused(
        &root,
        request(&root, &[("over0001", &over)]),
        "unsupported_syntax",
    );
    assert!(
        out["error"]["message"]
            .as_str()
            .unwrap()
            .contains("65 hex digits"),
        "{out}"
    );
}

#[test]
fn bare_quoted_extra_and_duplicate_attributes_refuse_at_selection() {
    for (name, marker, code) in [
        ("bare", "<!-- stay:attr0001 -->", "invalid_request"),
        (
            "quoted",
            "<!-- stay:attr0001 hash=\"sha256:000000000000\" -->",
            "unsupported_syntax",
        ),
        (
            "extra",
            "<!-- stay:attr0001 hash=sha256:000000000000 note=x -->",
            "unsupported_syntax",
        ),
        (
            "duplicate",
            "<!-- stay:attr0001 hash=sha256:000000000000 hash=sha256:000000000000 -->",
            "unsupported_syntax",
        ),
    ] {
        // A valid drifted stay first: a build that splices before validating
        // the whole selection would have written it.
        let root = put(
            &format!("refresh-attr-{name}"),
            &format!(
                "# Valid\n<!-- stay:valid001 hash=sha256:000000000000 -->\n\n# Attr\n{marker}\n"
            ),
        );
        refused(
            &root,
            request(&root, &[("valid001", ZERO), ("attr0001", ZERO)]),
            code,
        );
    }
}

#[test]
fn a_fenced_copy_is_content_and_an_html_block_copy_collides() {
    let fenced = "# Head\n<!-- stay:hd000001 hash=sha256:000000000000 -->\n\n```\n<!-- stay:hd000001 hash=sha256:000000000000 -->\n```\n";
    let root = put("refresh-fenced", fenced);
    let (code, out) = refresh(&root, &[("hd000001", ZERO)], false);
    assert_eq!(code, 0, "{out}");
    let want = fresh(fenced, "hd000001")[..12].to_string();
    let after = read(&root, T);
    assert_eq!(
        after,
        fenced.replacen(
            "stay:hd000001 hash=sha256:000000000000",
            &format!("stay:hd000001 hash=sha256:{want}"),
            1
        ),
        "the fenced copy keeps its bytes"
    );
    let html = "# Head\n<!-- stay:hd000001 hash=sha256:000000000000 -->\n\n<div>\n<!-- stay:hd000001 -->\n</div>\n";
    let root = put("refresh-html", html);
    refused(&root, request(&root, &[("hd000001", ZERO)]), "duplicate_id");
}

#[test]
fn addressed_and_orphaned_id_collisions_refuse() {
    for (name, text) in [
        (
            "addressed",
            "# One\n<!-- stay:dup00001 hash=sha256:000000000000 -->\n\n# Two\n<!-- stay:dup00001 hash=sha256:000000000000 -->\n",
        ),
        (
            "orphaned",
            "<!-- stay:dup00001 hash=sha256:000000000000 -->\n\n# One\n<!-- stay:dup00001 hash=sha256:000000000000 -->\n",
        ),
        (
            "legacy",
            "# One {#dup00001}\n\n# Two\n<!-- stay:dup00001 hash=sha256:000000000000 -->\n",
        ),
    ] {
        let root = put(&format!("refresh-collide-{name}"), text);
        refused(&root, request(&root, &[("dup00001", ZERO)]), "duplicate_id");
    }
}

#[test]
fn unaddressed_unknown_and_repeated_selections_refuse() {
    let text = "# Head\n<!-- stay:hd000001 hash=sha256:000000000000 -->\n\n- a\n  - nested <!-- stay:nest0001 subhash=sha256:000000000000 -->\n";
    let root = put("refresh-unaddressed", text);
    refused(
        &root,
        request(&root, &[("nest0001", ZERO)]),
        "unaddressed_marker",
    );
    refused(
        &root,
        request(&root, &[("absent01", ZERO)]),
        "missing_selector",
    );
    refused(
        &root,
        request(&root, &[("hd000001", ZERO), ("hd000001", ZERO)]),
        "invalid_request",
    );
    refused(&root, request(&root, &[]), "invalid_request");
}

#[test]
fn wrong_expect_and_stale_guard_refuse_even_a_no_op() {
    let text = "# Head\n<!-- stay:hd000001 hash=sha256:000000000000 -->\n";
    let root = put("refresh-expect", text);
    // Drifted: a wrong expect under a current guard.
    refused(
        &root,
        request(&root, &[("hd000001", "111111111111")]),
        "guard_conflict",
    );
    let (code, out) = refresh(&root, &[("hd000001", ZERO)], false);
    assert_eq!(code, 0, "{out}");
    let current = fresh(text, "hd000001")[..12].to_string();
    // Current: nothing to rewrite, still validated.
    refused(
        &root,
        request(&root, &[("hd000001", ZERO)]),
        "guard_conflict",
    );
    let stale = json!({"path": T, "guard": "rivet:v1:document:00000000000000000000000000000000",
        "select": select(&[("hd000001", &current)])});
    refused(&root, stale, "guard_conflict");
    let (code, out) = refresh(&root, &[("hd000001", &current)], false);
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["status"], "unchanged", "{out}");
}

#[test]
fn a_mixed_batch_refuses_atomically() {
    let text = "# A\n<!-- stay:aaaa0001 hash=sha256:000000000000 -->\n\n# B\n<!-- stay:bbbb0001 hash=sha256:000000000000 -->\n";
    for (name, bad, code) in [
        ("unknown", ("absent01", ZERO), "missing_selector"),
        ("expect", ("bbbb0001", "111111111111"), "guard_conflict"),
    ] {
        let root = put(&format!("refresh-batch-{name}"), text);
        refused(&root, request(&root, &[("aaaa0001", ZERO), bad]), code);
    }
}

#[test]
fn refresh_is_its_own_write() {
    let root = put(
        "refresh-alone",
        "# A\n<!-- stay:aaaa0001 hash=sha256:000000000000 -->\n",
    );
    let g = guard(&root);
    let before = read(&root, T);
    let (code, out) = rivet(
        &root,
        "write",
        json!({"path": T, "ops": [
            {"op": "refresh", "guard": g, "select": select(&[("aaaa0001", ZERO)])},
            {"op": "add_key", "key": "x", "value": 1, "guard": g}
        ]}),
    );
    assert_eq!(code, 2, "{out}");
    assert_eq!(out["error"]["code"], "invalid_request", "{out}");
    assert_eq!(read(&root, T), before);
}

fn vector(name: &str) -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../markrivet-core/tests/fixtures/markstay/profile-divergence.json");
    let corpus: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    corpus["vectors"]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["name"] == name)
        .unwrap()
        .clone()
}

#[test]
fn a_blank_line_profile_heading_digest_refreshes_to_the_commonmark_vector() {
    let v = vector("heading-glued-under-list-marker");
    let stay = |id: &str, profile: &str| {
        v["stays"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["id"] == id)
            .unwrap()[profile]
            .as_str()
            .unwrap()[..12]
            .to_string()
    };
    let blank = stay("hBase", "blank_line");
    let tree = stay("hBase", "tree");
    assert_ne!(blank, tree, "the vector diverges");
    // The heading holds what a default (§5.1) restamp writes.
    let text = v["doc"].as_str().unwrap().replace(
        "stay:hBase hash=sha256:000000000000",
        &format!("stay:hBase hash=sha256:{blank}"),
    );
    let root = put("refresh-profile", &text);
    let (code, out) = refresh(&root, &[("hBase", &blank)], false);
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["ops"][0]["refreshed"][0]["fresh"], tree.as_str());
    assert_eq!(
        read(&root, T),
        expected(&text, &[("hBase", "hash", &blank, &tree)])
    );
}

/// The §5.1 (blank-line) digest markstay's default restamp writes for `id`.
pub fn blank_line_digest(text: &str, id: &str) -> String {
    let block = markstay::parse_document(text)
        .into_iter()
        .find(|b| b.markers.iter().any(|m| m.id.as_deref() == Some(id)))
        .unwrap();
    markstay::body_hash(&block.content, None)
}

#[test]
fn the_review_family_shape_refreshes_the_glued_heading_alone() {
    // A paragraph's block stay, a standalone `subhash=sha256:0` marker line,
    // then a heading glued under them whose stay holds the §5.1 digest.
    let shape = "## Parent\n<!-- stay:parent01 hash=sha256:000000000000 -->\n\nPrior paragraph.\n<!-- stay:para0001 hash=sha256:000000000000 -->\n<!-- stay:debris01 subhash=sha256:0 -->\n### Child\n<!-- stay:child001 hash=sha256:000000000000 -->\n\nChild body.\n";
    let mut text = shape.to_string();
    for id in ["parent01", "para0001"] {
        let now = fresh(&text, id)[..12].to_string();
        text = expected(&text, &[(id, "hash", ZERO, &now)]);
    }
    let blank = blank_line_digest(&text, "child001")[..12].to_string();
    let tree = fresh(&text, "child001")[..12].to_string();
    assert_ne!(blank, tree, "the shape is outside §5.4's agreement subset");
    let text = expected(&text, &[("child001", "hash", ZERO, &blank)]);
    assert!(drifted(&text, "child001"));
    let root = put("refresh-review-shape", &text);
    let (code, out) = refresh(&root, &[("child001", &blank)], false);
    assert_eq!(code, 0, "{out}");
    let row = &out["ops"][0]["refreshed"][0];
    assert_eq!(row["owner"], "section");
    assert_eq!(row["fresh"], tree.as_str());
    let after = read(&root, T);
    assert_eq!(
        after,
        expected(&text, &[("child001", "hash", &blank, &tree)])
    );
    assert_eq!(owners(&after), owners(&text));
    // The debris marker is a different id and untouched.
    assert!(after.contains("<!-- stay:debris01 subhash=sha256:0 -->"));
}

#[test]
fn the_documented_example_runs() {
    // docs/usage.md `### refresh` and the `## Action` sample it reuses.
    let text = "## Action\n<!-- stay:hAct9x2Q hash=sha256:2261b4c779c1 -->\n\n- [ ] **Ship the widget.** Context, edited by hand. <!-- stay:aOne7yT3 subhash=sha256:3379e4c27a51 -->\n<!-- stay:lAct4bQ8 hash=sha256:b0f9c55b88f6 -->\n";
    let root = put("refresh-doc-example", text);
    let (code, out) = rivet(
        &root,
        "write",
        json!({"path": T, "ops": [{"op": "refresh", "guard": guard(&root),
            "select": [{"id": "aOne7yT3", "expect": "3379e4c27a51"}]}]}),
    );
    assert_eq!(code, 0, "{out}");
    assert_eq!(
        out["ops"][0]["refreshed"][0]["status"], "refreshed",
        "{out}"
    );
    assert!(!drifted(&read(&root, T), "aOne7yT3"));
}

fn doctor(root: &Path) -> Value {
    let (code, out) = rivet(root, "doctor", json!({"paths": [T]}));
    assert!(code == 0 || code == 4, "{out}");
    out
}

fn evidence(out: &Value) -> Vec<(String, String, Value)> {
    out["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|d| d["code"].as_str().unwrap().starts_with("evidence_"))
        .map(|d| {
            (
                d["code"].as_str().unwrap().to_string(),
                d["id"].as_str().unwrap().to_string(),
                d["evidence"].clone(),
            )
        })
        .collect()
}

#[test]
fn doctor_reports_drift_on_every_live_stay_once_and_not_fenced_copies() {
    // No action items; a heading, a plain Log bullet and a list container.
    let text = "# T\n\n## Action\n\n## Log\n<!-- stay:hLog0001 hash=sha256:000000000000 -->\n\n- **2026-01-02: Entry** Body. <!-- stay:bull0001 subhash=sha256:000000000000 -->\n\n- **2026-01-01: Older** Body. <!-- stay:bull0002 subhash=sha256:000000000000 -->\n<!-- stay:list0001 hash=sha256:000000000000 -->\n\n```\n<!-- stay:fence001 hash=sha256:000000000000 -->\n```\n";
    let root = put("refresh-doctor", text);
    let out = doctor(&root);
    let found = evidence(&out);
    let ids: Vec<&str> = found.iter().map(|(_, id, _)| id.as_str()).collect();
    assert_eq!(
        ids,
        ["hLog0001", "bull0001", "bull0002", "list0001"],
        "{out}"
    );
    for (code, id, ev) in &found {
        assert_eq!(code, "evidence_drift");
        assert_eq!(ev["refreshable"], true);
        assert_eq!(ev["stored"], ZERO);
        assert_eq!(ev["fresh"], &fresh(text, id)[..12]);
    }
    assert_eq!(found[0].2["owner"], "section");
    assert_eq!(found[1].2["owner"], "item");
    assert_eq!(found[1].2["key"], "subhash");
    assert_eq!(found[3].2["owner"], "list");
    let finding = out["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["id"] == "hLog0001")
        .unwrap();
    assert!(finding["collection"].is_null(), "{finding}");
    assert_eq!(finding["severity"], "warning");
    assert_eq!(finding["line"], 6);
    assert_eq!(out["summary"]["warnings"], 4, "{out}");
    let (code, strict) = rivet(&root, "doctor", json!({"paths": [T], "strict": true}));
    assert_eq!(code, 4, "{strict}");
    // Once the drift is refreshed, the findings are gone.
    let pairs: Vec<(&str, &str)> = ids.iter().map(|id| (*id, ZERO)).collect();
    let (code, applied) = refresh(&root, &pairs, false);
    assert_eq!(code, 0, "{applied}");
    assert!(evidence(&doctor(&root)).is_empty());
}

#[test]
fn doctor_lists_unrefreshable_evidence_separately() {
    let text = "# T\n\n## Action\n\n## Quoted\n<!-- stay:quot0001 hash=\"sha256:000000000000\" -->\n\n## Plain\n<!-- stay:plain001 hash=sha256:000000000000 note=x -->\n\n- odd item <!-- stay:bare0001 subhash=sha256:000000000000 note=x -->\n- ok item <!-- stay:okit0001 subhash=sha256:000000000000 -->\n- quoted item <!-- stay:qitm0001 subhash=\"sha256:000000000000\" -->\n\n- a\n  - nested <!-- stay:nest0001 subhash=sha256:000000000000 -->\n";
    let root = put("refresh-doctor-unrefreshable", text);
    let out = doctor(&root);
    let found = evidence(&out);
    let codes: Vec<(&str, &str, bool)> = found
        .iter()
        .map(|(c, id, ev)| (c.as_str(), id.as_str(), ev["refreshable"] == true))
        .collect();
    assert!(
        codes.contains(&("evidence_unrefreshable", "plain001", false)),
        "drift on unsupported attributes: {out}"
    );
    assert!(
        codes.contains(&("evidence_unrefreshable", "bare0001", false)),
        "an item stay with no child evidence: {out}"
    );
    assert!(
        codes.contains(&("evidence_drift", "okit0001", true)),
        "{out}"
    );
    assert!(
        !codes.iter().any(|(_, id, _)| *id == "nest0001"),
        "unaddressed stays keep their own code: {out}"
    );
    assert!(
        out["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|d| d["code"] == "unaddressed_marker" && d["line"] == 16)
    );
    // Unsupported attributes: the digest is still readable and reported.
    let bare = found.iter().find(|(_, id, _)| id == "bare0001").unwrap();
    assert_eq!(bare.2["stored"], ZERO, "{out}");
    assert_eq!(bare.2["fresh"], &fresh(text, "bare0001")[..12]);
    // No readable stored digest: both values are null.
    let quoted = found.iter().find(|(_, id, _)| id == "qitm0001").unwrap();
    assert_eq!(quoted.0, "evidence_unrefreshable", "{out}");
    assert!(
        quoted.2["stored"].is_null() && quoted.2["fresh"].is_null(),
        "{out}"
    );
    for (code, _, _) in &found {
        let d = out["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .find(|d| d["code"] == code.as_str())
            .unwrap();
        let names_refresh = d["next_action"].as_str().unwrap().contains("refresh write");
        assert_eq!(names_refresh, code == "evidence_drift", "{d}");
    }
}

#[test]
fn doctor_counts_a_file_bound_by_two_collections_once() {
    let text = "# T\n\n## Action\n\n## Log\n<!-- stay:hLog0001 hash=sha256:000000000000 -->\n";
    let root = put("refresh-doctor-overlap", text);
    let path = root.join(".rivet/project.json");
    let mut config: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    config["collections"]["again"] = config["collections"]["actions"].clone();
    std::fs::write(&path, config.to_string()).unwrap();
    let out = doctor(&root);
    assert_eq!(evidence(&out).len(), 1, "{out}");
}

#[test]
fn a_write_blocker_makes_drift_unrefreshable_in_doctor_and_refresh() {
    let text = "# T\n\n## Action\n\n## Log\n<!-- stay:hLog0001 hash=sha256:000000000000 -->\n\n{/* stay:mdx00001 */}\n";
    let root = put("refresh-blocker", text);
    let out = doctor(&root);
    let found = evidence(&out);
    assert_eq!(found.len(), 1, "{out}");
    assert_eq!(found[0].0, "evidence_unrefreshable", "{out}");
    assert_eq!(found[0].2["refreshable"], false);
    assert_eq!(found[0].2["stored"], ZERO);
    assert!(
        out["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|d| d["code"] == "unsupported_syntax"),
        "the blocker keeps its own finding: {out}"
    );
    refused(
        &root,
        request(&root, &[("hLog0001", ZERO)]),
        "unsupported_syntax",
    );
}
