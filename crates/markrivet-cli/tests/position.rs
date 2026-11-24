//! Position handles for list items: the CLI contract of the reads (`map`,
//! `get`, `find`, `query`, `doctor`). Resolution itself is tested in
//! `markrivet-core/tests/position.rs`.
mod common;
use common::*;
use serde_json::{Value, json};
use std::io::Write;

const PLACES: &str = "tracking/TRACKING_PLACES.md";
const PLACES_SOURCE: &str = include_str!("fixtures/position_places.md");

fn setup(name: &str) -> std::path::PathBuf {
    let root = project(name);
    std::fs::write(root.join(PLACES), PLACES_SOURCE).unwrap();
    root
}

fn at(path: Value, index: u64) -> Value {
    json!({"kind": "position", "heading_path": path, "index": index})
}

fn ok(root: &std::path::Path, cmd: &str, request: Value) -> Value {
    let (code, out) = rivet(root, cmd, request);
    assert_eq!(code, 0, "{out}");
    out
}

/// The raw stdout of one command, for byte-for-byte comparisons.
fn raw(root: &std::path::Path, cmd: &str, request: Value) -> String {
    let mut child = command(root, cmd, &[]).spawn().unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(request.to_string().as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert_eq!(out.status.code(), Some(0));
    String::from_utf8(out.stdout).unwrap()
}

fn items(map: &Value) -> Vec<Value> {
    map["outline"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["kind"] == "item")
        .cloned()
        .collect()
}

#[test]
fn get_reads_an_unstamped_item_by_place() {
    let root = setup("position-get");
    let select = at(json!(["Action"]), 2);
    let got = ok(&root, "get", json!({"path": PLACES, "select": select}));
    assert_eq!(got["content"], "[ ] **Third. (2026-09-03)** three");
    assert_eq!(got["checked"], false);
    let target = &got["target"];
    assert_eq!(target["kind"], "item");
    assert_eq!(target["id"], Value::Null);
    assert_eq!(target["addressing"], "position");
    // The handle the engine returns carries the full path.
    assert_eq!(target["select"], at(json!(["Places", "Action"]), 2));
    assert_eq!(got["typed"]["actions"]["fields"]["title"], "Third.");
    // The preamble, by an absent heading path.
    let got = ok(
        &root,
        "get",
        json!({"path": PLACES, "select": {"kind": "position", "index": 1}}),
    );
    assert_eq!(got["content"], "preamble item two");
    assert_eq!(got["target"]["select"], at(Value::Null, 1));
    assert!(got.get("typed").is_none());
}

#[test]
fn malformed_and_unresolvable_handles_are_refused() {
    let root = setup("position-refuse");
    let before = read(&root, PLACES);
    for (select, code) in [
        (at(json!([]), 0), "invalid_request"),
        (at(json!(["Nowhere"]), 0), "missing_selector"),
        (at(json!(["Action"]), 3), "missing_selector"),
    ] {
        let (exit, out) = rivet(&root, "get", json!({"path": PLACES, "select": select}));
        assert_eq!(
            (exit, out["error"]["code"].as_str()),
            (2, Some(code)),
            "{out}"
        );
    }
    // A negative index fails request parsing.
    let (exit, out) = rivet(
        &root,
        "get",
        json!({"path": PLACES, "select": {"kind": "position", "heading_path": ["Action"], "index": -1}}),
    );
    assert_eq!(exit, 1, "{out}");
    std::fs::write(
        root.join(PLACES),
        "## Tasks\n\n- [ ] a\n\n## Tasks\n\n- [ ] b\n",
    )
    .unwrap();
    let (exit, out) = rivet(
        &root,
        "get",
        json!({"path": PLACES, "select": at(json!(["Tasks"]), 0)}),
    );
    assert_eq!(
        (exit, out["error"]["code"].as_str()),
        (2, Some("ambiguous_selector")),
        "{out}"
    );
    // Repeated item bytes: same list, another list of the section, another
    // section. Each refusal names the other copy's line.
    for (source, path, line) in [
        ("## A\n\n- [ ] dup\n- [ ] x\n- [ ] dup\n", "A", 5),
        ("## A\n\n- [ ] dup\n\npara\n\n- [ ] dup\n", "A", 7),
        ("## A\n\n- [ ] dup\n\n## B\n\n- [ ] dup\n", "A", 7),
    ] {
        std::fs::write(root.join(PLACES), source).unwrap();
        let (exit, out) = rivet(
            &root,
            "get",
            json!({"path": PLACES, "select": at(json!([path]), 0)}),
        );
        assert_eq!(
            (
                exit,
                out["error"]["code"].as_str(),
                out["error"]["line"].as_u64()
            ),
            (2, Some("ambiguous_selector"), Some(line)),
            "{source}: {out}"
        );
    }
    std::fs::write(root.join(PLACES), &before).unwrap();
}

/// On a stamped document a position and the id name the same item; across
/// a stamp the content stays and the guard does not.
#[test]
fn position_and_id_agree_and_a_stamp_changes_the_guard() {
    let root = setup("position-stamped");
    let select = at(json!(["Action"]), 1);
    let unstamped = ok(&root, "get", json!({"path": PLACES, "select": select}));
    stamp(&root, PLACES);
    let by_place = ok(&root, "get", json!({"path": PLACES, "select": select}));
    let id = by_place["target"]["id"]
        .as_str()
        .expect("an id after stamping");
    assert_eq!(by_place["target"]["addressing"], "id");
    assert_eq!(
        by_place["target"]["select"],
        json!({"kind": "id", "id": id})
    );
    let by_id = ok(
        &root,
        "get",
        json!({"path": PLACES, "select": {"kind": "id", "id": id}}),
    );
    for field in ["text", "content", "revision", "target", "typed"] {
        assert_eq!(by_place[field], by_id[field], "{field}");
    }
    assert_eq!(unstamped["content"], by_place["content"]);
    assert_ne!(unstamped["text"], by_place["text"]);
    assert_ne!(unstamped["revision"], by_place["revision"]);
    // The pre-stamp guard no longer holds for that place.
    let before = read(&root, PLACES);
    let (exit, out) = rivet(
        &root,
        "set",
        json!({"path": PLACES, "select": select, "guard": unstamped["revision"],
               "content": "[x] **Second. (2026-09-02)** two"}),
    );
    assert_eq!(
        (exit, out["error"]["code"].as_str()),
        (2, Some("guard_conflict")),
        "{out}"
    );
    assert_eq!(read(&root, PLACES), before);
}

/// Without `items`, `map` is what it was before positions existed, byte for
/// byte (the golden outputs were captured from the build before this change).
#[test]
fn map_without_items_is_unchanged() {
    let golden: Value =
        serde_json::from_str(include_str!("fixtures/map_outputs_e88c671.json")).unwrap();
    let root = setup("position-map-golden");
    for (key, request) in [
        ("places", json!({"path": PLACES})),
        ("places_page", json!({"path": PLACES, "limit": 3})),
        ("widgets", json!({"path": WIDGETS})),
        ("places", json!({"path": PLACES, "items": false})),
    ] {
        assert_eq!(
            raw(&root, "map", request).trim_end(),
            golden[key].as_str().unwrap(),
            "{key}"
        );
    }
}

#[test]
fn map_items_lists_every_direct_item_with_its_addressing() {
    let root = setup("position-map-items");
    let map = ok(&root, "map", json!({"path": PLACES, "items": true}));
    let listed = items(&map);
    let expected = [
        (Value::Null, 0, "preamble item one", 5),
        (Value::Null, 1, "preamble item two", 6),
        (json!(["Places", "Action"]), 0, "First", 14),
        (json!(["Places", "Action"]), 1, "Second", 15),
        (json!(["Places", "Action"]), 2, "Third", 20),
        (json!(["Places", "Action", "Sub"]), 0, "Sub item", 26),
        (json!(["Places", "Notes"]), 0, "note alpha", 30),
        (json!(["Places", "Notes"]), 1, "note beta", 31),
    ];
    assert_eq!(listed.len(), expected.len(), "{map}");
    for (entry, (path, index, text, line)) in listed.iter().zip(expected) {
        assert_eq!(entry["addressing"], "position", "{entry}");
        assert_eq!(entry["id"], Value::Null);
        assert_eq!(entry["select"], at(path, index), "{entry}");
        assert_eq!(entry["line"], line, "{entry}");
        let got = ok(
            &root,
            "get",
            json!({"path": PLACES, "select": entry["select"]}),
        );
        assert!(got["content"].as_str().unwrap().contains(text), "{got}");
        assert_eq!(entry["revision"], got["revision"]);
        assert_eq!(entry.get("checked"), got.get("checked"));
    }
    assert_eq!(map["diagnostics"], json!([]));
    // Items page with the outline, in document order after their list.
    let outline = map["outline"].as_array().unwrap();
    let first_item = outline.iter().position(|e| e["kind"] == "item").unwrap();
    assert_eq!(outline[first_item - 1]["kind"], "list");
    let page = ok(
        &root,
        "map",
        json!({"path": PLACES, "items": true, "limit": 2}),
    );
    assert_eq!(page["total"], outline.len());
    assert_eq!(page["outline"][1]["select"], at(Value::Null, 0));
    // Once stamped, every item is addressed by id.
    stamp(&root, PLACES);
    let map = ok(&root, "map", json!({"path": PLACES, "items": true}));
    let listed = items(&map);
    assert_eq!(listed.len(), 8);
    assert!(
        listed.iter().any(|e| e["addressing"] == "position"),
        "a default stamp leaves the non-collection lists alone"
    );
    for entry in &listed {
        match entry["addressing"].as_str().unwrap() {
            "id" => assert_eq!(
                entry["select"],
                json!({"kind": "id", "id": entry["id"]}),
                "{entry}"
            ),
            "position" => assert_eq!(entry["id"], Value::Null),
            other => panic!("{other}: {entry}"),
        }
    }
}

#[test]
fn find_names_the_item_holding_a_match() {
    let root = setup("position-find");
    let found = ok(
        &root,
        "find",
        json!({"text": "nested note", "path": PLACES}),
    );
    let hit = &found["matches"][0];
    // A nested item is part of its top-level item.
    assert_eq!(hit["kind"], "item");
    assert_eq!(hit["addressing"], "position");
    assert_eq!(hit["select"], at(json!(["Places", "Action"]), 1));
    let got = ok(
        &root,
        "get",
        json!({"path": PLACES, "select": hit["select"]}),
    );
    assert_eq!(hit["revision"], got["revision"]);
    assert_eq!(hit["typed"]["actions"], got["typed"]["actions"]["revision"]);
    // A quoted list is not a unit: its text is the section's.
    let found = ok(
        &root,
        "find",
        json!({"text": "quoted item", "path": PLACES}),
    );
    assert_eq!(found["matches"][0]["kind"], "section");
    // Stamped items are found by id.
    stamp(&root, PLACES);
    let found = ok(&root, "find", json!({"text": "Third", "path": PLACES}));
    let hit = &found["matches"][0];
    assert_eq!(hit["addressing"], "id");
    assert_eq!(hit["select"], json!({"kind": "id", "id": hit["id"]}));
}

#[test]
fn query_returns_ids_and_positions_without_a_diagnostic() {
    let root = setup("position-query");
    // A mixed file: the Action list's first item is stamped by hand.
    let mixed = PLACES_SOURCE.replace(
        "- [ ] **First. (2026-09-01)** one",
        "- [ ] **First. (2026-09-01)** one <!-- stay:MixMixMi subhash=sha256:111111111111 -->",
    );
    std::fs::write(root.join(PLACES), mixed).unwrap();
    let out = ok(
        &root,
        "query",
        json!({"collection": "actions", "paths": [PLACES]}),
    );
    let got: Vec<(Value, Value, Value)> = out["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| {
            (
                i["id"].clone(),
                i["addressing"].clone(),
                i["select"].clone(),
            )
        })
        .collect();
    assert_eq!(
        got,
        [
            (
                json!("MixMixMi"),
                json!("id"),
                json!({"kind": "id", "id": "MixMixMi"})
            ),
            (
                Value::Null,
                json!("position"),
                at(json!(["Places", "Action"]), 1)
            ),
            (
                Value::Null,
                json!("position"),
                at(json!(["Places", "Action"]), 2)
            ),
        ]
    );
    assert_eq!(out["diagnostics"], json!([]), "{out}");
    // The typed revision `query` returns is the one `get` reports.
    let third = &out["items"][2];
    let got = ok(
        &root,
        "get",
        json!({"path": PLACES, "select": third["select"]}),
    );
    assert_eq!(third["revision"], got["typed"]["actions"]["revision"]);
}

#[test]
fn doctor_still_warns_about_unstamped_items() {
    let root = setup("position-doctor");
    let out = ok(
        &root,
        "doctor",
        json!({"collection": "actions", "paths": [PLACES]}),
    );
    assert_eq!(out["health"], "warning", "{out}");
    assert_eq!(out["summary"]["missing_ids"], 3, "{out}");
}

/// An item no position can name without guessing is still listed, with
/// `addressing: "ambiguous"`, a null select and the refusal beside it.
#[test]
fn ambiguous_items_are_listed_with_the_refusal() {
    let root = project("position-ambiguous");
    std::fs::write(
        root.join(".rivet/project.json"),
        json!({"collections": {
            "actions": {"schema": "tracker-action@1", "files": ["tracking/*.md"],
                        "section": "Action", "level": 2, "subsections": true}}})
        .to_string(),
    )
    .unwrap();
    let source = "\
## Action

- [ ] **Unique. (2026-09-01)** fine
- [ ] **Twin. (2026-09-02)** same

### Batch

- [ ] **Left. (2026-09-03)** one

### Batch

- [ ] **Right. (2026-09-04)** two

## Notes

- [ ] **Twin. (2026-09-02)** same

### Notes

- alpha

### Notes

- beta
";
    std::fs::write(root.join(PLACES), source).unwrap();
    std::fs::remove_file(root.join(WIDGETS)).unwrap();
    std::fs::remove_file(root.join("tracking/TRACKING_DOCS.md")).unwrap();
    // (title or text, line, addressing) per direct item in document order.
    let expected = [
        ("Unique", 3, "position"),
        ("Twin", 4, "ambiguous"),
        ("Left", 8, "ambiguous"),
        ("Right", 12, "ambiguous"),
        ("Twin", 16, "ambiguous"),
        ("alpha", 20, "ambiguous"),
        ("beta", 24, "ambiguous"),
    ];
    let check = |entries: &[Value], diagnostics: &Value, what: &str| {
        let mut lines = vec![];
        for entry in entries {
            let line = entry["line"].as_u64().unwrap();
            let (_, _, addressing) = expected
                .iter()
                .find(|(_, l, _)| *l == line)
                .unwrap_or_else(|| panic!("{what}: {entry}"));
            assert_eq!(entry["addressing"], *addressing, "{what}: {entry}");
            if *addressing == "ambiguous" {
                assert_eq!(entry["select"], Value::Null, "{what}: {entry}");
                assert_eq!(entry["id"], Value::Null, "{what}: {entry}");
                lines.push(line);
            }
        }
        let mut flagged: Vec<u64> = diagnostics
            .as_array()
            .unwrap()
            .iter()
            .filter(|d| d["code"] == "ambiguous_selector")
            .map(|d| d["line"].as_u64().unwrap())
            .collect();
        flagged.sort();
        assert_eq!(flagged, lines, "{what}: {diagnostics}");
        assert!(!lines.is_empty(), "{what}");
    };
    let map = ok(&root, "map", json!({"path": PLACES, "items": true}));
    let listed = items(&map);
    assert_eq!(listed.len(), expected.len(), "{map}");
    check(&listed, &map["diagnostics"], "map");
    let found = ok(&root, "find", json!({"text": "(2026-09", "path": PLACES}));
    let matches = found["matches"].as_array().unwrap().clone();
    assert_eq!(matches.len(), 5, "{found}");
    check(&matches, &found["diagnostics"], "find");
    let found = ok(&root, "find", json!({"text": "alpha", "path": PLACES}));
    check(
        found["matches"].as_array().unwrap(),
        &found["diagnostics"],
        "find alpha",
    );
    let out = ok(&root, "query", json!({"collection": "actions"}));
    let queried = out["items"].as_array().unwrap().clone();
    assert_eq!(queried.len(), 4, "{out}");
    check(&queried, &out["diagnostics"], "query");
}

// ---------------------------------------------------------------- writes

const WRITES: &str = "\
# Places

## Action

- [ ] **Alpha. (2026-09-01)** a
- [ ] **Bravo. (2026-09-02)** b
- [ ] **Charlie. (2026-09-03)** c

Between the lists.

- [ ] **Delta. (2026-09-04)** d

## Notes

- note one
- note two
";

/// Another writer's change to the file between a read and a write.
type Rewrite = fn(&str) -> String;

const CHARLIE: &str = "- [ ] **Charlie. (2026-09-03)** c";
const CHARLIE_DONE: &str = "[x] **Charlie. (2026-09-03)** c";

fn writes(name: &str) -> std::path::PathBuf {
    let root = project(name);
    std::fs::write(root.join(PLACES), WRITES).unwrap();
    root
}

fn action(index: u64) -> Value {
    at(json!(["Action"]), index)
}

/// The native and typed guards of the item a selector names now.
fn guards(root: &std::path::Path, select: &Value) -> (Value, Value) {
    let got = ok(root, "get", json!({"path": PLACES, "select": select}));
    (
        got["revision"].clone(),
        got["typed"]["actions"]["revision"].clone(),
    )
}

fn set_at(root: &std::path::Path, select: &Value, guard: &Value, content: &str) -> (i32, Value) {
    rivet(
        root,
        "set",
        json!({"path": PLACES, "select": select, "guard": guard, "content": content}),
    )
}

/// A refused write: exit 2, the code, no `current`, and the file unchanged.
fn refused(root: &std::path::Path, (exit, out): (i32, Value), code: &str, before: &str) -> Value {
    assert_eq!(
        (exit, out["error"]["code"].as_str()),
        (2, Some(code)),
        "{out}"
    );
    assert!(out["error"].get("current").is_none(), "{out}");
    assert_eq!(read(root, PLACES), before, "a refused write left the file");
    out["error"].clone()
}

/// Read Charlie's guard, let `change` rewrite the file the way another
/// writer would, then write through the stale handle.
fn race(
    name: &str,
    change: impl Fn(&str) -> String,
) -> (std::path::PathBuf, Value, String, (i32, Value)) {
    let root = writes(name);
    let (guard, _) = guards(&root, &action(2));
    let changed = change(WRITES);
    assert_ne!(changed, WRITES);
    std::fs::write(root.join(PLACES), &changed).unwrap();
    let result = set_at(&root, &action(2), &guard, CHARLIE_DONE);
    (root, guard, changed, result)
}

/// Anything that moves another item into the target's place is
/// refused, with no `current`, and with the target's new place when exactly
/// one item still has the guarded revision; a retry there lands on it.
#[test]
fn moves_above_the_target_refuse_and_point_to_where_it_went() {
    let cases: [(&str, Rewrite, Option<u64>); 7] = [
        (
            "insert-same-list",
            |s| {
                s.replace(
                    "- [ ] **Alpha",
                    "- [ ] **New. (2026-09-05)** n\n- [ ] **Alpha",
                )
            },
            Some(3),
        ),
        (
            "insert-new-list-above",
            |s| {
                s.replace(
                    "- [ ] **Alpha",
                    "+ [ ] **Other. (2026-09-06)** o\n\n- [ ] **Alpha",
                )
            },
            Some(3),
        ),
        (
            "delete-above",
            |s| s.replace("- [ ] **Alpha. (2026-09-01)** a\n", ""),
            Some(1),
        ),
        (
            "reorder",
            |s| {
                s.replace(
                    "- [ ] **Bravo. (2026-09-02)** b\n- [ ] **Charlie. (2026-09-03)** c",
                    "- [ ] **Charlie. (2026-09-03)** c\n- [ ] **Bravo. (2026-09-02)** b",
                )
            },
            Some(1),
        ),
        (
            "delete-target",
            |s| s.replace("- [ ] **Charlie. (2026-09-03)** c\n", ""),
            None,
        ),
        (
            "edit-target",
            |s| s.replace("** c\n", "** c, edited\n"),
            None,
        ),
        // Two items now hold the guarded revision: no hint names one of them.
        (
            "copy-above",
            |s| s.replace("- [ ] **Alpha", &format!("{CHARLIE}\n- [ ] **Alpha")),
            None,
        ),
    ];
    for (name, change, relocated) in cases {
        let (root, guard, changed, result) = race(&format!("position-race-{name}"), change);
        let error = refused(&root, result, "guard_conflict", &changed);
        assert!(
            error["message"]
                .as_str()
                .unwrap()
                .contains("positional handle"),
            "{error}"
        );
        match relocated {
            Some(index) => {
                let select = &error["relocated"]["select"];
                assert_eq!(
                    *select,
                    at(json!(["Places", "Action"]), index),
                    "{name}: {error}"
                );
                let (code, out) = set_at(&root, select, &guard, CHARLIE_DONE);
                assert_eq!(code, 0, "{name}: {out}");
                let after = read(&root, PLACES);
                assert_eq!(
                    after,
                    changed.replace(CHARLIE, &format!("- {CHARLIE_DONE}")),
                    "{name}"
                );
            }
            None => assert!(error.get("relocated").is_none(), "{name}: {error}"),
        }
    }
    // An insert into an earlier list of the section shifts a later list's
    // items too.
    let root = writes("position-race-earlier-list");
    let (guard, _) = guards(&root, &action(3));
    let changed = WRITES.replace(
        "- [ ] **Alpha",
        "- [ ] **New. (2026-09-05)** n\n- [ ] **Alpha",
    );
    std::fs::write(root.join(PLACES), &changed).unwrap();
    let result = set_at(&root, &action(3), &guard, "[x] **Delta. (2026-09-04)** d");
    let error = refused(&root, result, "guard_conflict", &changed);
    assert_eq!(
        error["relocated"]["select"],
        at(json!(["Places", "Action"]), 4)
    );
}

/// A tick or a stamp changes the target's own bytes; no item keeps the
/// old revision, so there is nowhere to point.
#[test]
fn edits_to_the_target_refuse_without_a_hint() {
    let (root, _, changed, result) = race("position-race-tick", |s| {
        s.replace(CHARLIE, "- [x] **Charlie. (2026-09-03)** c")
    });
    let error = refused(&root, result, "guard_conflict", &changed);
    assert!(error.get("relocated").is_none(), "{error}");

    let root = writes("position-race-stamp");
    let (guard, _) = guards(&root, &action(2));
    stamp(&root, PLACES);
    let stamped = read(&root, PLACES);
    let error = refused(
        &root,
        set_at(&root, &action(2), &guard, CHARLIE_DONE),
        "guard_conflict",
        &stamped,
    );
    assert!(error.get("relocated").is_none(), "{error}");
    // Items a stamp leaves alone keep their bytes, place and handle.
    let notes = at(json!(["Notes"]), 1);
    let root2 = writes("position-race-stamp-notes");
    let (note_guard, _) = guards(&root2, &notes);
    stamp(&root2, PLACES);
    let (code, out) = set_at(&root2, &notes, &note_guard, "note two, edited");
    assert_eq!(code, 0, "{out}");
}

/// Changes below the target, or in another section, leave its index and
/// bytes alone: the write applies and touches nothing outside the item.
#[test]
fn changes_below_or_elsewhere_do_not_block_the_write() {
    let cases: [(&str, Rewrite); 3] = [
        ("insert-below", |s| {
            s.replace("** c\n", "** c\n- [ ] **Echo. (2026-09-07)** e\n")
        }),
        ("edit-later-list", |s| s.replace("** d\n", "** d, edited\n")),
        ("other-section", |s| {
            s.replace("- note one\n", "- note zero\n- note one\n")
        }),
    ];
    for (name, change) in cases {
        let (root, _, changed, (code, out)) = race(&format!("position-live-{name}"), change);
        assert_eq!(code, 0, "{name}: {out}");
        assert_eq!(
            read(&root, PLACES),
            changed.replace(CHARLIE, &format!("- {CHARLIE_DONE}")),
            "{name}"
        );
        assert_eq!(out["ops"][0]["addressing"], "position", "{out}");
        assert_eq!(
            out["ops"][0]["select"],
            at(json!(["Places", "Action"]), 2),
            "{out}"
        );
        let (fresh, _) = guards(&root, &action(2));
        assert_eq!(
            out["ops"][0]["revision"], fresh,
            "the returned guard is the next one"
        );
    }
}

/// For writes, an item whose bytes repeat cannot be written by position,
/// and a copy appearing between read and write turns the write away.
#[test]
fn repeated_bytes_refuse_every_positional_write() {
    let root = writes("position-dup");
    let mut refusals = 0;
    for source in [
        "## Action\n\n- [ ] A\n- [ ] B\n- [ ] A\n",
        "## Action\n\n- [ ] A\n\n## Notes\n\n- [ ] A\n",
    ] {
        std::fs::write(root.join(PLACES), source).unwrap();
        let doc_guard = ok(&root, "map", json!({"path": PLACES}))["revision"].clone();
        for select in [action(0), at(json!(["Action"]), 2), at(json!(["Notes"]), 0)] {
            let result = set_at(&root, &select, &doc_guard, "[x] A");
            let (exit, out) = &result;
            if out["error"]["code"] == "missing_selector" {
                continue; // that place does not exist in this source
            }
            assert_eq!(*exit, 2, "{out}");
            refused(&root, result, "ambiguous_selector", source);
            refusals += 1;
        }
    }
    assert_eq!(refusals, 4, "both copies in each source");
    let (root, _, changed, result) = race("position-dup-late", |s| {
        s.replace("- note two\n", &format!("- note two\n\n{CHARLIE}\n"))
    });
    let error = refused(&root, result, "ambiguous_selector", &changed);
    assert!(
        error["message"].as_str().unwrap().contains("line"),
        "{error}"
    );
}

/// The case bytes cannot tell apart: the target removed and an identical
/// item put back at its place. The write applies to that item, as a section
/// deleted and recreated byte-identical accepts its old guard.
#[test]
fn aba_an_identical_item_at_the_same_place_accepts_the_old_guard() {
    let root = writes("position-aba");
    let (guard, _) = guards(&root, &action(2));
    // Two separate writes: remove Charlie, then put an identical line back.
    let section = json!({"kind": "heading_path", "path": ["Action"]});
    let body = |root: &std::path::Path| ok(root, "get", json!({"path": PLACES, "select": section}));
    let got = body(&root);
    let without = got["body"]
        .as_str()
        .unwrap()
        .replace(&format!("{CHARLIE}\n"), "");
    ok(
        &root,
        "set",
        json!({"path": PLACES, "select": section, "part": "body",
        "guard": got["revision"], "content": without}),
    );
    assert!(!read(&root, PLACES).contains(CHARLIE));
    let got = body(&root);
    let with = got["body"]
        .as_str()
        .unwrap()
        .replace("** b\n", &format!("** b\n{CHARLIE}\n"));
    ok(
        &root,
        "set",
        json!({"path": PLACES, "select": section, "part": "body",
        "guard": got["revision"], "content": with}),
    );
    assert_eq!(read(&root, PLACES), WRITES);
    let (code, out) = set_at(&root, &action(2), &guard, CHARLIE_DONE);
    assert_eq!(code, 0, "{out}");
}

#[test]
fn update_item_by_position() {
    let root = writes("position-typed");
    std::fs::write(
        root.join(PLACES),
        WRITES.replace(
            "- [ ] **Delta. (2026-09-04)** d",
            "- [ ] **Delta. (2026-09-04)** d\n- [ ] untitled chore",
        ),
    )
    .unwrap();
    let update = |root: &std::path::Path, select: &Value, set: Value| {
        let (_, typed) = guards(root, select);
        rivet(
            root,
            "update-item",
            json!({"path": PLACES, "collection": "actions", "select": select, "guard": typed, "set": set}),
        )
    };
    // Tick, set a label, promote an untitled item.
    let (code, out) = update(&root, &action(2), json!({"done": true}));
    assert_eq!(code, 0, "{out}");
    assert!(read(&root, PLACES).contains("- [x] **Charlie. (2026-09-03)** c"));
    let op = &out["ops"][0];
    assert_eq!(op["select"], at(json!(["Places", "Action"]), 2));
    assert_eq!(op["addressing"], "position");
    assert_eq!(
        op["revision"],
        guards(&root, &action(2)).1,
        "the typed guard comes back"
    );
    assert!(op["collection_revision"].is_string(), "{out}");
    let (code, out) = update(&root, &action(1), json!({"gate": "the bravo check passes"}));
    assert_eq!(code, 0, "{out}");
    let got = ok(&root, "get", json!({"path": PLACES, "select": action(1)}));
    assert_eq!(
        got["typed"]["actions"]["fields"]["gate"],
        "the bravo check passes"
    );
    let (code, out) = update(&root, &action(4), json!({"title": "Promoted."}));
    assert_eq!(code, 0, "{out}");
    let got = ok(&root, "get", json!({"path": PLACES, "select": action(4)}));
    assert_eq!(
        got["typed"]["actions"]["fields"]["title"], "Promoted.",
        "{got}"
    );
    // The item is named by exactly one of id and a position or id select.
    let before = read(&root, PLACES);
    let (_, typed) = guards(&root, &action(0));
    for request in [
        json!({"path": PLACES, "collection": "actions", "id": "AbCdEfGh", "select": action(0), "guard": typed, "set": {"done": true}}),
        json!({"path": PLACES, "collection": "actions", "guard": typed, "set": {"done": true}}),
        json!({"path": PLACES, "collection": "actions", "select": {"kind": "heading_path", "path": ["Action"]}, "guard": typed, "set": {"done": true}}),
    ] {
        let result = rivet(&root, "update-item", request);
        refused(&root, result, "invalid_request", &before);
    }
    // A schema edit changes every typed guard.
    let (_, typed) = guards(&root, &action(0));
    let schema = root.join(".rivet/schemas/tracker-action.json");
    std::fs::write(
        &schema,
        format!("{}\n", std::fs::read_to_string(&schema).unwrap()),
    )
    .unwrap();
    let result = rivet(
        &root,
        "update-item",
        json!({"path": PLACES, "collection": "actions", "select": action(0), "guard": typed, "set": {"done": true}}),
    );
    refused(&root, result, "guard_conflict", &before);
    // A place outside the collection, or a section that is gone.
    let (_, typed) = guards(&root, &action(0));
    for select in [at(json!(["Notes"]), 0), at(json!(["Gone"]), 0)] {
        let result = rivet(
            &root,
            "update-item",
            json!({"path": PLACES, "collection": "actions", "select": select, "guard": typed, "set": {"done": true}}),
        );
        refused(&root, result, "missing_selector", &before);
    }
    // The bound-body set by position.
    let (native, typed) = guards(&root, &action(0));
    let (code, out) = rivet(
        &root,
        "set",
        json!({"path": PLACES, "select": action(0), "guard": native, "collection": "actions",
               "typed_guard": typed, "content": "[x] **Alpha. (2026-09-01)** a, done"}),
    );
    assert_eq!(code, 0, "{out}");
    assert!(read(&root, PLACES).contains("- [x] **Alpha. (2026-09-01)** a, done"));
    assert_eq!(out["ops"][0]["revision"], guards(&root, &action(0)).0);
    // A stale typed guard on a moved item points to its new place.
    let (_, typed) = guards(&root, &action(1));
    let moved = read(&root, PLACES).replace(
        "- [x] **Alpha",
        "- [ ] **Zulu. (2026-09-09)** z\n- [x] **Alpha",
    );
    std::fs::write(root.join(PLACES), &moved).unwrap();
    let result = rivet(
        &root,
        "update-item",
        json!({"path": PLACES, "collection": "actions", "select": action(1), "guard": typed, "set": {"done": true}}),
    );
    let error = refused(&root, result, "guard_conflict", &moved);
    assert_eq!(
        error["relocated"]["select"],
        at(json!(["Places", "Action"]), 2),
        "{error}"
    );
}

#[test]
fn batches_resolve_positions_against_one_read_and_return_where_items_went() {
    let root = writes("position-batch");
    let (a, _) = guards(&root, &action(0));
    let (b, _) = guards(&root, &action(1));
    let (code, out) = rivet(
        &root,
        "write",
        json!({"path": PLACES, "ops": [
            {"op": "set", "select": action(0), "guard": a, "content": "[x] **Alpha. (2026-09-01)** a"},
            {"op": "set", "select": action(1), "guard": b, "content": "[x] **Bravo. (2026-09-02)** b"}]}),
    );
    assert_eq!(code, 0, "{out}");
    assert!(
        read(&root, PLACES).contains("- [x] **Alpha")
            && read(&root, PLACES).contains("- [x] **Bravo")
    );
    // A positional set and an item insert on the same list: the existing
    // one-reshape-per-list rule refuses the batch.
    let before = read(&root, PLACES);
    let (c, _) = guards(&root, &action(2));
    let collection =
        ok(&root, "map", json!({"path": PLACES}))["collections"][0]["revision"].clone();
    let result = rivet(
        &root,
        "write",
        json!({"path": PLACES, "ops": [
            {"op": "set", "select": action(2), "guard": c, "content": CHARLIE_DONE},
            {"op": "insert_item", "collection": "actions", "position": "start", "guard": collection,
             "fields": {"title": "Fresh.", "done": false}}]}),
    );
    let (exit, out) = &result;
    assert_eq!(*exit, 2, "{out}");
    assert_eq!(out["error"]["code"], "invalid_request", "{out}");
    assert_eq!(read(&root, PLACES), before);

    // An insert_item into the section's first list shifts a later list's
    // item; the returned handle says so, and a follow-up write lands.
    let root = writes("position-batch-shift");
    let (_, typed) = guards(&root, &action(3));
    let collection =
        ok(&root, "map", json!({"path": PLACES}))["collections"][0]["revision"].clone();
    let (code, out) = rivet(
        &root,
        "write",
        json!({"path": PLACES, "ops": [
            {"op": "insert_item", "collection": "actions", "position": "start", "guard": collection,
             "fields": {"title": "Fresh.", "done": false}},
            {"op": "update_item", "collection": "actions", "select": action(3), "guard": typed,
             "set": {"done": true}}]}),
    );
    assert_eq!(code, 0, "{out}");
    let op = &out["ops"][1];
    assert_eq!(op["select"], at(json!(["Places", "Action"]), 4), "{out}");
    let (code, again) = rivet(
        &root,
        "update-item",
        json!({"path": PLACES, "collection": "actions", "select": op["select"], "guard": op["revision"],
               "set": {"gate": "delta gate"}}),
    );
    assert_eq!(code, 0, "{again}");
    let got = ok(
        &root,
        "get",
        json!({"path": PLACES, "select": op["select"]}),
    );
    assert_eq!(got["typed"]["actions"]["fields"]["title"], "Delta.");
    assert_eq!(got["typed"]["actions"]["fields"]["gate"], "delta gate");

    // A title set on the item's heading and a positional set in one batch:
    // the returned handle carries the new path.
    let root = writes("position-batch-title");
    let notes = json!({"kind": "heading_path", "path": ["Notes"]});
    let heading = ok(&root, "get", json!({"path": PLACES, "select": notes}))["revision"].clone();
    let (n, _) = guards(&root, &at(json!(["Notes"]), 1));
    let (code, out) = rivet(
        &root,
        "write",
        json!({"path": PLACES, "ops": [
            {"op": "set", "select": notes, "part": "title", "guard": heading, "content": "Jottings"},
            {"op": "set", "select": at(json!(["Notes"]), 1), "guard": n, "content": "note two, edited"}]}),
    );
    assert_eq!(code, 0, "{out}");
    let op = &out["ops"][1];
    assert_eq!(op["select"], at(json!(["Places", "Jottings"]), 1), "{out}");
    let (code, again) = set_at(&root, &op["select"], &op["revision"], "note two, twice");
    assert_eq!(code, 0, "{again}");
    assert!(read(&root, PLACES).contains("- note two, twice\n"));
}

/// The guard's stated coverage: it covers the item span. A consumed final
/// indentation sits outside every block span, so removing it leaves the item
/// guard valid; the write applies and keeps every other byte as it found it.
#[test]
fn the_guard_covers_the_item_span() {
    let root = writes("position-span");
    let source = "## A\n\n- other\n- <?\n ";
    std::fs::write(root.join(PLACES), source).unwrap();
    let select = at(json!(["A"]), 1);
    let got = ok(&root, "get", json!({"path": PLACES, "select": select}));
    assert_eq!(got["text"], "- <?", "{got}");
    let without = "## A\n\n- other\n- <?\n";
    std::fs::write(root.join(PLACES), without).unwrap();
    let after = ok(&root, "get", json!({"path": PLACES, "select": select}));
    assert_eq!(after["revision"], got["revision"]);
    let (code, out) = set_at(&root, &select, &got["revision"], "done");
    assert_eq!(code, 0, "{out}");
    assert_eq!(read(&root, PLACES), "## A\n\n- other\n- done\n");
}

/// A position is accepted by get, set and update-item only. Every other
/// selector field refuses it before anything else is checked.
#[test]
fn other_selector_fields_refuse_a_position() {
    let root = writes("position-d8");
    let before = read(&root, PLACES);
    let (guard, _) = guards(&root, &action(0));
    for (command, extra) in [
        ("delete", json!({})),
        ("stamp", json!({})),
        ("insert", json!({"position": "end", "content": "- [ ] x"})),
        ("append", json!({"content": "Para."})),
        ("replace-in-section", json!({"old": "a", "new": "b"})),
        (
            "remove-paragraph-in-section",
            json!({"old": "Between the lists."}),
        ),
        ("rejoin", json!({"line": 1})),
    ] {
        let mut request = json!({"path": PLACES, "select": action(0), "guard": guard});
        request
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        let error = refused(
            &root,
            rivet(&root, command, request),
            "invalid_request",
            &before,
        );
        assert!(
            error["message"]
                .as_str()
                .unwrap()
                .contains("accepted only by get, set and update-item"),
            "{command}: {error}"
        );
    }
    // In a batch, the refusal names the op and comes before any planning.
    let result = rivet(
        &root,
        "write",
        json!({"path": PLACES, "ops": [
            {"op": "set", "select": action(0), "guard": guard, "content": "[x] **Alpha. (2026-09-01)** a"},
            {"op": "delete", "select": action(1), "guard": guard}]}),
    );
    let error = refused(&root, result, "invalid_request", &before);
    assert!(
        error["message"].as_str().unwrap().starts_with("op 1:"),
        "{error}"
    );
}
