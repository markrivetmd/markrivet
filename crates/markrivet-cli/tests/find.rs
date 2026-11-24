mod common;
use common::*;
use serde_json::{Value, json};

const DOC: &str = "tracking/TRACKING_FIND.md";

/// A synthetic tracker: a preamble before any heading, a bound Action list
/// with a nested item, a code span wrapped across a line, and a prose
/// section whose paragraphs a blank line separates.
const SOURCE: &str = "Preamble about the parcel depot.

# Parcels

## Action

- [ ] **Chase the missing (2026-09-14)** The courier lost the
      invoice for the depot.
      **Gate**: `parcel-cli audit
      --strict` passes.
  - a nested note on the spare crate, and a crate lid.

- [ ] **Weigh the crate. (2026-09-15)** Twice.

## Notes

The last paragraph ends here.

Next paragraph starts here.
";

fn find(root: &std::path::Path, request: Value) -> Value {
    let (code, out) = rivet(root, "find", request);
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["status"], "read", "{out}");
    out
}

fn hits(root: &std::path::Path, text: &str) -> Vec<Value> {
    find(root, json!({"text": text, "path": DOC}))["matches"]
        .as_array()
        .unwrap()
        .clone()
}

/// 1-based line of `text` in the file as it is now (stamping adds lines).
fn line_of(root: &std::path::Path, text: &str) -> usize {
    let source = read(root, DOC);
    source[..source.find(text).unwrap()].matches('\n').count() + 1
}

fn setup(name: &str, stamped: bool) -> std::path::PathBuf {
    let root = project(name);
    std::fs::write(root.join(DOC), SOURCE).unwrap();
    if stamped {
        stamp(&root, DOC);
    }
    root
}

/// A match answers with the unit that holds it, the selector that names it
/// and that unit's guards, so a caller goes from `find` straight to a guarded
/// write, typed or not.
#[test]
fn a_match_names_its_item_and_the_guards_a_write_takes() {
    let root = setup("find-item", true);
    let found = hits(&root, "courier lost");
    assert_eq!(found.len(), 1, "{found:?}");
    let hit = &found[0];
    assert_eq!(hit["kind"], "item");
    assert_eq!(hit["path"], DOC);
    assert_eq!(hit["heading_path"], json!(["Parcels", "Action"]));
    assert_eq!(hit["line"], line_of(&root, "courier lost"));
    assert_eq!(hit["count"], 1);
    let id = hit["id"].as_str().unwrap();
    assert_eq!(hit["select"], json!({"kind": "id", "id": id}));
    let (_, got) = rivet(&root, "get", json!({"path": DOC, "select": hit["select"]}));
    assert_eq!(hit["revision"], got["revision"]);
    assert_eq!(hit["typed"]["actions"], got["typed"]["actions"]["revision"]);
    let (code, out) = rivet(
        &root,
        "update-item",
        json!({"path": DOC, "collection": "actions", "id": id,
               "guard": hit["typed"]["actions"], "set": {"done": true}}),
    );
    assert_eq!(code, 0, "{out}");
}

/// Matching runs on the source, so a code span's backticks are literal
/// characters: text inside the span matches, and so does the span with its
/// backticks, but not prose that skips them. The span wraps across a line,
/// which CommonMark renders as a space, and a whitespace run matches one.
#[test]
fn a_code_span_matches_as_source_text() {
    let root = setup("find-code-span", true);
    let item = |text: &str| {
        let found = hits(&root, text);
        assert_eq!(found.len(), 1, "{text}: {found:?}");
        assert_eq!(found[0]["kind"], "item", "{text}");
        found[0]["excerpt"].as_str().unwrap().to_string()
    };
    item("parcel-cli audit");
    item("`parcel-cli audit --strict`");
    let excerpt = item("audit --strict");
    assert!(
        excerpt.contains("`parcel-cli audit --strict` passes"),
        "{excerpt}"
    );
    assert!(hits(&root, "Gate: parcel-cli").is_empty());
}

/// A soft line break and the continuation indentation after it are one
/// space, so a phrase wrapped across lines matches. A blank line ends a
/// paragraph, and nothing matches across it.
#[test]
fn a_soft_line_break_matches_a_space_and_a_blank_line_does_not() {
    let root = setup("find-soft-break", true);
    let found = hits(&root, "lost the invoice");
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0]["kind"], "item");
    assert_eq!(
        found[0]["line"],
        line_of(&root, "lost the\n"),
        "where it starts"
    );
    assert!(
        found[0]["excerpt"]
            .as_str()
            .unwrap()
            .contains("courier lost the invoice for the depot."),
        "{found:?}"
    );
    assert!(hits(&root, "lost   the\tinvoice").len() == 1);
    assert_eq!(hits(&root, "ends here.").len(), 1);
    assert!(hits(&root, "ends here. Next paragraph").is_empty());
}

/// Text in a nested item, which owns no stay, reports the direct item that
/// does, once, with every match in it counted there rather than repeated up
/// the tree.
#[test]
fn a_nested_match_reports_the_innermost_addressed_unit_once() {
    let root = setup("find-nested", true);
    let found = hits(&root, "crate");
    assert_eq!(found.len(), 2, "{found:?}");
    assert_eq!(found[0]["kind"], "item");
    assert_eq!(
        found[0]["line"],
        line_of(&root, "spare crate"),
        "the nested note"
    );
    assert_eq!(
        found[0]["count"], 2,
        "both matches in the nested note, counted once"
    );
    assert_eq!(found[1]["kind"], "item");
    assert_ne!(found[0]["id"], found[1]["id"]);
    let found = hits(&root, "depot");
    assert_eq!(found.len(), 2, "{found:?}");
    assert_eq!(found[0]["kind"], "document", "the preamble has no section");
    assert_eq!(found[1]["count"], 1);
}

/// Text in an unstamped list item is reported as that item, with a null id
/// and the position handle that selects it. Text outside every item is the
/// innermost section, with the heading path that selects it, or the document
/// for text before the first heading. The revision is the guard for that
/// selector.
#[test]
fn unaddressed_matches_are_reported_by_heading_path() {
    let root = setup("find-unstamped", false);
    let found = hits(&root, "courier lost");
    assert_eq!(found.len(), 1, "{found:?}");
    let hit = &found[0];
    assert_eq!(hit["kind"], "item");
    assert_eq!(hit["id"], Value::Null);
    assert_eq!(hit["addressing"], "position");
    assert_eq!(
        hit["select"],
        json!({"kind": "position", "heading_path": ["Parcels", "Action"], "index": 0})
    );
    let (_, got) = rivet(&root, "get", json!({"path": DOC, "select": hit["select"]}));
    assert_eq!(hit["revision"], got["revision"]);
    assert_eq!(hit["typed"]["actions"], got["typed"]["actions"]["revision"]);
    // The nested note is part of its top-level item, not a unit of its own.
    let found = hits(&root, "spare crate");
    assert_eq!(found[0]["select"], hit["select"], "{found:?}");
    let found = hits(&root, "last paragraph");
    let hit = &found[0];
    assert_eq!(hit["kind"], "section");
    assert_eq!(hit["id"], Value::Null);
    assert!(hit.get("typed").is_none());
    assert!(hit.get("addressing").is_none());
    assert_eq!(
        hit["select"],
        json!({"kind": "heading_path", "path": hit["heading_path"]})
    );
    let (_, got) = rivet(&root, "get", json!({"path": DOC, "select": hit["select"]}));
    assert_eq!(hit["revision"], got["revision"]);
    let found = hits(&root, "preamble");
    assert_eq!(found[0]["kind"], "document");
    assert_eq!(found[0]["id"], Value::Null);
    assert!(found[0].get("heading_path").is_none());
    assert_eq!(found[0]["select"], json!({"kind": "document"}));
    let (_, got) = rivet(
        &root,
        "get",
        json!({"path": DOC, "select": found[0]["select"]}),
    );
    assert_eq!(found[0]["revision"], got["revision"]);
}

#[test]
fn case_markers_scope_limits_and_refusals() {
    let root = setup("find-misc", true);
    // Case-insensitive unless asked; a stay marker is not text.
    assert_eq!(hits(&root, "COURIER LOST").len(), 1);
    let exact = find(
        &root,
        json!({"text": "COURIER LOST", "path": DOC, "match_case": true}),
    );
    assert_eq!(exact["total"], 0);
    assert!(read(&root, DOC).contains("stay:"));
    assert!(hits(&root, "stay:").is_empty());
    // Without a path, every document a collection binds, in path order.
    let all = find(&root, json!({"text": "the"}));
    assert_eq!(all["files"], 3);
    let paths: Vec<&str> = all["matches"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["path"].as_str().unwrap())
        .collect();
    assert!(paths.is_sorted(), "{paths:?}");
    let total = all["total"].as_u64().unwrap();
    assert!(total > 3);
    let page = find(&root, json!({"text": "the", "limit": 2}));
    assert_eq!(page["matches"].as_array().unwrap().len(), 2);
    assert_eq!(page["total"], total);
    assert_eq!(page["truncated"], true);
    // Blank text is refused; so is a path outside the project.
    let (code, out) = rivet(&root, "find", json!({"text": " \n "}));
    assert_eq!(
        (code, out["error"]["code"].as_str()),
        (2, Some("invalid_request"))
    );
    let (code, out) = rivet(&root, "find", json!({"text": "x", "path": "../x.md"}));
    assert_eq!(
        (code, out["error"]["code"].as_str()),
        (2, Some("path_rejected"))
    );
    let (code, _) = rivet(&root, "find", json!({"text": "x", "op": "find"}));
    assert_eq!(code, 1);
}

/// Write `source` as the test document and search it.
fn search(name: &str, source: &str, request: Value) -> Value {
    let root = project(name);
    std::fs::write(root.join(DOC), source).unwrap();
    let mut request = request;
    request["path"] = json!(DOC);
    find(&root, request)
}

/// A stay marker is neither text nor whitespace: text on both sides of an
/// inline marker reads as it does with the marker removed.
#[test]
fn an_inline_marker_joins_the_text_around_it() {
    let source = "# T\n\nfoo<!-- stay:AbCdEfGh -->bar baz\n";
    let found = search("find-inline-marker", source, json!({"text": "foobar"}));
    assert_eq!(found["total"], 1, "{found}");
    let found = search("find-inline-marker", source, json!({"text": "foo bar"}));
    assert_eq!(found["total"], 0, "{found}");
    // A malformed marker is not a stay; its bytes are text like any comment.
    let found = search(
        "find-malformed-marker",
        "# T\n\n<!-- stay -->\n",
        json!({"text": "stay"}),
    );
    assert_eq!(found["total"], 1, "{found}");
}

/// Every entry names a selector `get` accepts, or `null` with the reason in
/// `diagnostics`. A duplicated stay id is passed
/// over for the unit around it, and a heading path that selects more than
/// one section is reported in `diagnostics` beside its entry.
#[test]
fn entries_name_selectors_that_resolve_or_say_why_not() {
    let dup = "## Action\n<!-- stay:SecSecSe -->\n\n- [ ] one dup <!-- stay:DupDupDu -->\n- [ ] two dup <!-- stay:DupDupDu -->\n";
    // Text outside every item falls back to the addressed section.
    let found = search("find-dup", dup, json!({"text": "action"}));
    let matches = found["matches"].as_array().unwrap();
    assert_eq!(matches.len(), 1, "{found}");
    assert_eq!(matches[0]["kind"], "section");
    assert_eq!(matches[0]["id"], "SecSecSe");
    assert_eq!(
        matches[0]["select"],
        json!({"kind": "id", "id": "SecSecSe"})
    );
    let root = project("find-dup-get");
    std::fs::write(root.join(DOC), dup).unwrap();
    let (_, got) = rivet(
        &root,
        "get",
        json!({"path": DOC, "select": matches[0]["select"]}),
    );
    assert_eq!(matches[0]["revision"], got["revision"]);

    // Items whose stays are duplicated are still the unit; no id selects
    // them, so each is named by its position.
    let list = "## Action\n\n- [ ] one dup <!-- stay:DupDupDu -->\n- [ ] two dup <!-- stay:DupDupDu -->\n<!-- stay:LstLstLs -->\n";
    let root = project("find-dup-list");
    std::fs::write(root.join(DOC), list).unwrap();
    let found = find(&root, json!({"text": "dup", "path": DOC}));
    let matches = found["matches"].as_array().unwrap();
    assert_eq!(matches.len(), 2, "{found}");
    for (n, hit) in matches.iter().enumerate() {
        assert_eq!(
            (&hit["kind"], &hit["id"], &hit["addressing"], &hit["select"]),
            (
                &json!("item"),
                &json!(null),
                &json!("position"),
                &json!({"kind": "position", "heading_path": ["Action"], "index": n})
            ),
            "{found}"
        );
        let (_, got) = rivet(&root, "get", json!({"path": DOC, "select": hit["select"]}));
        assert_eq!(hit["revision"], got["revision"]);
    }

    // A unit whose first id is duplicated is still addressed by another id
    // it carries, and the entry names that one.
    let second = "## One\n<!-- stay:DupDupDu -->\n<!-- stay:UniqUniq -->\n\nbeta\n\n## Two\n<!-- stay:DupDupDu -->\n";
    let root = project("find-second-id");
    std::fs::write(root.join(DOC), second).unwrap();
    let found = find(&root, json!({"text": "beta", "path": DOC}));
    let hit = &found["matches"][0];
    assert_eq!(
        (&hit["kind"], &hit["id"]),
        (&json!("section"), &json!("UniqUniq")),
        "{found}"
    );
    let (_, got) = rivet(&root, "get", json!({"path": DOC, "select": hit["select"]}));
    assert_eq!(hit["revision"], got["revision"]);

    let twice = "## Tasks\n\nalpha one\n\n## Tasks\n\nalpha two\n";
    let found = search("find-ambiguous", twice, json!({"text": "alpha"}));
    let matches = found["matches"].as_array().unwrap();
    assert_eq!(matches.len(), 2, "{found}");
    // Neither heading path selects one section, so neither entry names a
    // selector: Rivet never guesses a target.
    assert!(
        matches
            .iter()
            .all(|m| m["id"].is_null() && m["select"].is_null())
    );
    let codes: Vec<_> = found["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| (d["code"].as_str().unwrap(), d["line"].as_u64().unwrap()))
        .collect();
    assert_eq!(
        codes,
        [("ambiguous_selector", 3), ("ambiguous_selector", 7)]
    );
}

/// Case folding is Unicode lowercase with its expansions, and a final sigma
/// matches a sigma; `match_case` compares characters as written.
#[test]
fn case_folding_covers_expansions_and_final_sigma() {
    let source = "# T\n\nΟΔΟΣ and İstanbul.\n";
    for (text, total) in [
        ("οδος", 1),
        ("ΟΔΟΣ", 1),
        ("i\u{307}stanbul", 1),
        ("istanbul", 0),
    ] {
        let found = search("find-fold", source, json!({"text": text}));
        assert_eq!(found["total"], total, "{text}: {found}");
    }
    let found = search(
        "find-fold",
        source,
        json!({"text": "οδος", "match_case": true}),
    );
    assert_eq!(found["total"], 0);
}

/// `get` and `map` name a unit by the first id that selects it, as `find`
/// does, so a unit whose first id is duplicated reports one `get` accepts. A
/// unit with no such id keeps its first, which `doctor` reports.
#[test]
fn get_and_map_report_the_first_id_that_resolves() {
    let second = "## One\n<!-- stay:DupDupDu -->\n<!-- stay:UniqUniq -->\n\nbeta\n\n## Two\n<!-- stay:DupDupDu -->\n";
    let root = project("get-second-id");
    std::fs::write(root.join(DOC), second).unwrap();
    let (code, got) = rivet(
        &root,
        "get",
        json!({"path": DOC, "select": {"kind": "id", "id": "UniqUniq"}}),
    );
    assert_eq!(code, 0, "{got}");
    assert_eq!(got["target"]["id"], "UniqUniq", "{got}");
    let (_, map) = rivet(&root, "map", json!({"path": DOC}));
    let id = |title: &str| {
        map["outline"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["path"] == json!([title]))
            .unwrap()["id"]
            .clone()
    };
    assert_eq!(id("One"), "UniqUniq", "{map}");
    assert_eq!(id("Two"), "DupDupDu", "{map}");
}
