//! Stay-id citations: `doctor` checks `item:` and `section:` citations
//! against every collection and archive file, and a write that drops a stay
//! lists where it is still mentioned (usage.md § doctor, § delete).
mod common;
use common::*;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

const ALPHA: &str = "tracking/TRACKING_ALPHA.md";
const BETA: &str = "tracking/TRACKING_BETA.md";
const ARCHIVE: &str = "archive/2026-09/ALPHA_CLOSED_ITEMS_2026-09.md";

const SUB: &str = "subhash=sha256:111111111111";

fn alpha() -> String {
    format!(
        "# Alpha

## Action
<!-- stay:SecAlpha -->

- [ ] **One** first. <!-- stay:ItemOne1 {SUB} -->
- [ ] **Two** second, after item:ItemOne1. <!-- stay:ItemTwo2 {SUB} -->
- [ ] **Three** third. <!-- stay:DupDupDu {SUB} -->

## Log

Resolved: item:ItemOne1, section:SecAlpha, item:Archived, item:BetaItem, item:LiveCopy.
Wrong kind: item:SecAlpha and section:ItemOne1.
Unresolved: item:GoneGone. Ambiguous: item:DupDupDu.
Not citations: item:cd89cbcf/2/b6ce, item:connect-test#162, the `item:` form.
Bare, unchecked: ItemOne1 and `ItemOne1`; another scheme's: item:ItemOne1/2/note.

```
item:InCodeXx
```
"
    )
}

fn beta() -> String {
    format!(
        "# Beta

## Action

- [ ] **Beta** item. <!-- stay:BetaItem {SUB} -->
- [ ] **Live** copy. <!-- stay:LiveCopy {SUB} -->
- [ ] **Dup** again. <!-- stay:DupDupDu {SUB} -->
"
    )
}

fn archive() -> String {
    format!(
        "# Alpha: closed Action items

- [x] **Archived** done. <!-- stay:Archived {SUB} -->
- [x] **Live** copy, pruned before it reopened. <!-- stay:LiveCopy {SUB} -->
"
    )
}

fn setup(name: &str, archived: bool) -> PathBuf {
    let root = project(name);
    let mut actions = json!({"schema": "tracker-action@1", "files": ["tracking/TRACKING_*.md"],
                             "section": "Action", "level": 2});
    if archived {
        actions["archive"] = json!(["archive/*/*_CLOSED_ITEMS_*.md"]);
    }
    std::fs::write(
        root.join(".rivet/project.json"),
        json!({"collections": {"actions": actions}}).to_string(),
    )
    .unwrap();
    std::fs::remove_dir_all(root.join("tracking")).unwrap();
    std::fs::create_dir_all(root.join("tracking")).unwrap();
    std::fs::create_dir_all(root.join("archive/2026-09")).unwrap();
    std::fs::write(root.join(ALPHA), alpha()).unwrap();
    std::fs::write(root.join(BETA), beta()).unwrap();
    std::fs::write(root.join(ARCHIVE), archive()).unwrap();
    root
}

/// 1-based line of the first occurrence of `text` in `rel`.
fn line_of(root: &Path, rel: &str, text: &str) -> usize {
    let source = read(root, rel);
    source[..source.find(text).unwrap()].matches('\n').count() + 1
}

/// `(code, id, line)` of every finding on a citation, in report order.
fn cited(root: &Path, request: Value) -> Vec<(String, String, usize)> {
    let (_, out) = rivet(root, "doctor", request);
    out["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|d| {
            let code = d["code"].as_str().unwrap();
            code.starts_with("citation_") && code != "citation_scope"
        })
        .map(|d| {
            assert_eq!(d["severity"], "warning", "{d}");
            assert_eq!(d["path"], ALPHA, "{d}");
            (
                d["code"].as_str().unwrap().to_string(),
                d["id"].as_str().unwrap().to_string(),
                d["line"].as_u64().unwrap() as usize,
            )
        })
        .collect()
}

/// A citation resolves when exactly one unit of its kind carries the id:
/// a live copy beside an archive copy is that one, two live copies are not.
/// Another scheme's value, an empty prefix and a code block are not read.
#[test]
fn doctor_reports_each_citation_that_does_not_name_one_unit_of_its_kind() {
    let root = setup("cite-doctor", true);
    let at = |text| line_of(&root, ALPHA, text);
    let want = vec![
        ("citation_kind".into(), "SecAlpha".into(), at("Wrong kind")),
        ("citation_kind".into(), "ItemOne1".into(), at("Wrong kind")),
        (
            "citation_unresolved".into(),
            "GoneGone".into(),
            at("Unresolved"),
        ),
        (
            "citation_ambiguous".into(),
            "DupDupDu".into(),
            at("Unresolved"),
        ),
    ];
    assert_eq!(cited(&root, json!({})), want);
    // The id map covers every collection file whatever the scope; citations
    // are read from the selected files only.
    assert_eq!(cited(&root, json!({"paths": [ALPHA]})), want);
    assert_eq!(cited(&root, json!({"paths": [BETA]})), vec![]);
    let (_, out) = rivet(&root, "doctor", json!({}));
    let kind = out["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["code"] == "citation_kind")
        .unwrap();
    assert_eq!(
        kind["message"],
        format!(
            "item:SecAlpha names {ALPHA}:{}, which is a section",
            at("## Action")
        )
    );
    assert_eq!(kind["column"], 13);
}

/// Without the `archive` key an archived item resolves nowhere, and a
/// collection file that does not parse is named, since the ids it carries
/// are then unknown.
#[test]
fn the_id_map_is_only_as_wide_as_the_files_it_reads() {
    let root = setup("cite-scope", false);
    let unresolved: Vec<String> = cited(&root, json!({}))
        .into_iter()
        .filter(|(code, _, _)| code == "citation_unresolved")
        .map(|(_, id, _)| id)
        .collect();
    assert_eq!(unresolved, ["Archived", "GoneGone"]);

    // A lone CR is refused by the parser, but the bytes were read.
    std::fs::write(root.join(BETA), "# Beta\r\nlone\rCR\n").unwrap();
    let (_, out) = rivet(&root, "doctor", json!({"paths": [ALPHA]}));
    let scope: Vec<_> = out["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|d| d["code"] == "citation_scope")
        .collect();
    assert_eq!(scope.len(), 1, "{out}");
    assert_eq!(scope[0]["path"], BETA);
    assert_eq!(scope[0]["severity"], "info");
    let unresolved: Vec<String> = cited(&root, json!({"paths": [ALPHA]}))
        .into_iter()
        .filter(|(code, _, _)| code == "citation_unresolved")
        .map(|(_, id, _)| id)
        .collect();
    assert_eq!(unresolved, ["Archived", "BetaItem", "LiveCopy", "GoneGone"]);
}

fn delete(root: &Path, rel: &str, id: &str, preview: bool) -> Value {
    let (_, got) = rivet(
        root,
        "get",
        json!({"path": rel, "select": {"kind": "id", "id": id}}),
    );
    let (code, out) = rivet(
        root,
        "delete",
        json!({"path": rel, "select": {"kind": "id", "id": id}, "guard": got["revision"],
               "preview": preview}),
    );
    assert_eq!(code, 0, "{out}");
    out
}

/// A delete names every mention of a stay id it leaves carried nowhere,
/// bare or prefixed, and is still a delete: the warning refuses nothing.
#[test]
fn a_delete_lists_the_citations_it_leaves_dangling() {
    let root = setup("cite-delete", true);
    let before = read(&root, ALPHA);
    let preview = delete(&root, ALPHA, "ItemOne1", true);
    assert_eq!(read(&root, ALPHA), before, "a preview writes nothing");
    let out = delete(&root, ALPHA, "ItemOne1", false);
    assert_eq!(out["status"], "applied", "{out}");
    assert_eq!(out["ops"][0]["dropped"], json!(["ItemOne1"]));
    let at = |text| line_of(&root, ALPHA, text);
    let want = json!([
        {"id": "ItemOne1", "path": ALPHA, "line": at("**Two**")},
        {"id": "ItemOne1", "path": ALPHA, "line": at("Resolved:")},
        {"id": "ItemOne1", "path": ALPHA, "line": at("Wrong kind")},
        {"id": "ItemOne1", "path": ALPHA, "line": at("Bare, unchecked")},
        {"id": "ItemOne1", "path": ALPHA, "line": at("Bare, unchecked")},
    ]);
    assert_eq!(out["dangling_citations"], want, "{out}");
    // A preview reads the candidate too, so it warns with the same lines.
    assert_eq!(preview["dangling_citations"], want, "{preview}");

    // An id the archive still carries is not left dangling.
    let out = delete(&root, BETA, "LiveCopy", false);
    assert_eq!(out["ops"][0]["dropped"], json!(["LiveCopy"]));
    assert!(out.get("dangling_citations").is_none(), "{out}");
}

/// A stay id live in two selected files is an error in each, whatever cites
/// it; a live copy beside an archive copy is not. Scope is the selected files.
#[test]
fn doctor_reports_a_stay_id_live_in_two_files() {
    let root = setup("dup-doctor", true);
    let found = |request: Value| -> (i32, Vec<(String, String, usize)>) {
        let (code, out) = rivet(&root, "doctor", request);
        let found = out["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|d| d["code"] == "cross_file_duplicate_id")
            .map(|d| {
                assert_eq!(d["severity"], "error", "{d}");
                assert!(d["collection"].is_null(), "{d}");
                (
                    d["path"].as_str().unwrap().to_string(),
                    d["id"].as_str().unwrap().to_string(),
                    d["line"].as_u64().unwrap() as usize,
                )
            })
            .collect();
        if code == 4 {
            assert_eq!(out["health"], "unhealthy", "{out}");
        }
        (code, found)
    };
    let (code, dups) = found(json!({}));
    assert_eq!(code, 4);
    assert_eq!(
        dups,
        [
            (
                ALPHA.to_string(),
                "DupDupDu".to_string(),
                line_of(&root, ALPHA, "stay:DupDupDu")
            ),
            (
                BETA.to_string(),
                "DupDupDu".to_string(),
                line_of(&root, BETA, "stay:DupDupDu")
            ),
        ]
    );
    assert!(found(json!({"paths": [ALPHA]})).1.is_empty());
    let beta = beta().replace("<!-- stay:DupDupDu", "<!-- stay:BetaOwn1");
    std::fs::write(root.join(BETA), beta).unwrap();
    assert!(found(json!({})).1.is_empty());
}
