//! `query` with `ready`: waits resolved across a collection's files and its
//! archive files (usage.md § query, Ready work).
mod common;
use common::*;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

const ALPHA: &str = "tracking/TRACKING_ALPHA.md";
const BETA: &str = "tracking/TRACKING_BETA.md";
const ARCHIVE: &str = "archive/2026-09/ALPHA_CLOSED_ITEMS_2026-09.md";

fn binding(archive: bool) -> Value {
    let mut actions = json!({"schema": "waits-action@1", "files": ["tracking/TRACKING_*.md"],
                             "section": "Action", "level": 2});
    if archive {
        actions["archive"] = json!(["archive/*/*_CLOSED_ITEMS_*.md"]);
    }
    json!({"collections": {"actions": actions}})
}

/// A project of two trackers and one archive file, every unit stamped. Items
/// are named by title; `waits` sets each named item's `Waits` label, written
/// through `update-item` once the ids exist, with `{Title}` replaced by that
/// item's stay id.
fn ready_project(
    name: &str,
    alpha: &[&str],
    beta: &[&str],
    archived: &[&str],
    waits: &[(&str, &str)],
) -> (PathBuf, BTreeMap<String, String>) {
    let root = project(name);
    std::fs::write(
        root.join(".rivet/schemas/waits-action.json"),
        json!({"id": "waits-action", "version": 1, "item": "checkbox", "fields": {
            "done": {"type": "boolean", "from": "checkbox"},
            "title": {"type": "string", "from": "title"},
            "context": {"type": "string", "from": "text"},
            "waits": {"type": "string", "from": "label", "label": "Waits"}}})
        .to_string(),
    )
    .unwrap();
    std::fs::write(root.join(".rivet/project.json"), binding(true).to_string()).unwrap();
    std::fs::remove_dir_all(root.join("tracking")).unwrap();
    std::fs::create_dir_all(root.join("tracking")).unwrap();
    std::fs::create_dir_all(root.join("archive/2026-09")).unwrap();
    let tracker = |title: &str, items: &[&str]| {
        format!(
            "# {title}\n\n## Action\n\n{}\n## Log\n\nNewest first.\n",
            items.iter().map(|i| format!("{i}\n\n")).collect::<String>()
        )
    };
    std::fs::write(root.join(ALPHA), tracker("Alpha", alpha)).unwrap();
    std::fs::write(root.join(BETA), tracker("Beta", beta)).unwrap();
    std::fs::write(
        root.join(ARCHIVE),
        format!(
            "# Alpha: closed Action items\n\n{}",
            archived
                .iter()
                .map(|i| format!("{i}\n\n"))
                .collect::<String>()
        ),
    )
    .unwrap();
    for path in [ALPHA, BETA, ARCHIVE] {
        let (_, m) = rivet(&root, "map", json!({"path": path}));
        // An archive file is in no collection, so its items need the
        // document scope; a prune moves them with their stays.
        let (code, out) = rivet(
            &root,
            "stamp",
            json!({"path": path, "guard": m["revision"], "select": {"kind": "document"}}),
        );
        assert_eq!(code, 0, "{out}");
    }
    let ids = ids(&root);
    for (title, value) in waits {
        let mut value = value.to_string();
        for (t, id) in &ids {
            value = value.replace(&format!("{{{t}}}"), id);
        }
        let (path, guard) = item(&root, &ids[*title]);
        let (code, out) = rivet(
            &root,
            "update-item",
            json!({"path": path, "collection": "actions", "id": ids[*title],
                   "guard": guard, "set": {"waits": value}}),
        );
        assert_eq!(code, 0, "{title}: {out}");
    }
    (root, ids)
}

/// Title (without its full stop) to stay id, for every item in every file.
/// An item's stay closes its last line, which may be a label line.
fn ids(root: &Path) -> BTreeMap<String, String> {
    let mut ids = BTreeMap::new();
    for path in [ALPHA, BETA, ARCHIVE] {
        let mut title: Option<String> = None;
        for line in read(root, path).lines() {
            if let Some(rest) = line.strip_prefix("- [") {
                title = rest.find("**").map(|t| {
                    let t = rest[t + 2..].split("**").next().unwrap();
                    t.trim_end_matches('.').to_string()
                });
            } else if !line.starts_with(' ') {
                title = None;
            }
            if let (Some(t), Some(s)) = (&title, line.find("<!-- stay:")) {
                let id = &line[s + "<!-- stay:".len()..][..8];
                ids.entry(t.clone()).or_insert_with(|| id.to_string());
            }
        }
    }
    ids
}

fn item(root: &Path, id: &str) -> (String, Value) {
    let (code, out) = rivet(
        root,
        "query",
        json!({"collection": "actions", "where": [{"field": "id", "op": "eq", "value": id}]}),
    );
    assert_eq!(code, 0, "{out}");
    let it = &out["items"][0];
    (
        it["path"].as_str().unwrap().to_string(),
        it["revision"].clone(),
    )
}

fn ready(root: &Path, request: Value) -> Value {
    let (code, out) = rivet(root, "query", request);
    assert_eq!(code, 0, "{out}");
    out
}

const ALL: [&str; 5] = ["ready", "waiting", "condition", "unlabelled", "unresolved"];

/// Title to (readiness, reason) for every listed item.
fn verdicts(out: &Value) -> BTreeMap<String, (String, Option<String>)> {
    out["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| {
            (
                i["fields"]["title"]
                    .as_str()
                    .unwrap()
                    .trim_end_matches('.')
                    .to_string(),
                (
                    i["wait"]["readiness"].as_str().unwrap().to_string(),
                    i["wait"]["reason"].as_str().map(str::to_string),
                ),
            )
        })
        .collect()
}

fn class(r: &str) -> (String, Option<String>) {
    (r.to_string(), None)
}

fn unresolved(reason: &str) -> (String, Option<String>) {
    ("unresolved".to_string(), Some(reason.to_string()))
}

/// usage.md § query, Ready work: the documented request and the classes it
/// reports, a pruned dependency resolving through the archive binding.
#[test]
fn ready_query_takes_the_documented_shape() {
    let (root, ids) = ready_project(
        "ready-shape",
        &[
            "- [ ] **Ship the widget.** Nothing in the way.",
            "- [ ] **Write the release note.** After shipping.",
            "- [ ] **Publish the release.** After the audit.",
            "- [ ] **Ask the vendor.** Their call.",
            "- [ ] **Tidy the backlog.** Never labelled.",
            "- [x] **Old work.** Finished.",
        ],
        &["- [ ] **Order parts.** For the widget."],
        &["- [x] **Run the audit.** Moved here when pruned."],
        &[
            ("Ship the widget", "none"),
            (
                "Write the release note",
                "item:{Ship the widget} (the release itself)",
            ),
            ("Publish the release", "item:{Run the audit}"),
            ("Ask the vendor", "human:sam"),
            ("Order parts", "item:{Old work}"),
            ("Old work", "none"),
        ],
    );
    let before: Vec<String> = [ALPHA, BETA, ARCHIVE]
        .iter()
        .map(|p| read(&root, p))
        .collect();

    // The documented request: ready items only, every class counted.
    let out = ready(
        &root,
        json!({"collection": "actions", "ready": {"field": "waits"}}),
    );
    assert_eq!(
        out["readiness"],
        json!({"ready": 3, "waiting": 1, "condition": 1, "unlabelled": 1, "unresolved": 0}),
        "{out}"
    );
    let listed = verdicts(&out);
    assert_eq!(
        listed.keys().collect::<Vec<_>>(),
        ["Order parts", "Publish the release", "Ship the widget"],
        "done items are never listed: {out}"
    );
    let publish = out["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["fields"]["title"] == "Publish the release.")
        .unwrap();
    assert_eq!(
        publish["wait"],
        json!({"readiness": "ready", "value": format!("item:{}", ids["Run the audit"]),
               "on": {"id": ids["Run the audit"], "path": ARCHIVE, "line": 4,
                      "done": true, "archived": true}})
    );
    assert_eq!(
        out["resolved_from"], 1,
        "the archive file is read to resolve"
    );
    assert_eq!(out["diagnostics"], json!([]));

    // `include` lists other classes; `paths` narrows the listing, not the
    // resolution, so a wait on an item in another file still resolves.
    let out = ready(
        &root,
        json!({"collection": "actions", "paths": [ALPHA],
               "ready": {"field": "waits", "include": ALL}}),
    );
    assert_eq!(
        verdicts(&out),
        BTreeMap::from([
            ("Ship the widget".into(), class("ready")),
            ("Write the release note".into(), class("waiting")),
            ("Publish the release".into(), class("ready")),
            ("Ask the vendor".into(), class("condition")),
            ("Tidy the backlog".into(), class("unlabelled")),
        ])
    );
    assert_eq!(
        out["resolved_from"], 2,
        "beta and the archive, read but not listed"
    );
    let note = out["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["fields"]["title"] == "Write the release note.")
        .unwrap();
    assert_eq!(note["wait"]["on"]["done"], false);
    assert_eq!(note["wait"]["on"]["path"], ALPHA);

    // `where` still filters, and the counts cover what it lets through.
    let out = ready(
        &root,
        json!({"collection": "actions", "ready": {"field": "waits"},
               "where": [{"field": "title", "op": "contains", "value": "release"}]}),
    );
    assert_eq!(
        out["readiness"],
        json!({"ready": 1, "waiting": 1, "condition": 0, "unlabelled": 0, "unresolved": 0})
    );

    let after: Vec<String> = [ALPHA, BETA, ARCHIVE]
        .iter()
        .map(|p| read(&root, p))
        .collect();
    assert_eq!(before, after, "a ready query writes nothing");
}

#[test]
fn a_dangling_wait_is_reported_not_guessed() {
    let (root, ids) = ready_project(
        "ready-dangling",
        &[
            "- [ ] **Follow the ghost.** Its blocker never existed.",
            "- [ ] **Follow the pruned.** Its blocker moved to the archive.",
        ],
        &[],
        &["- [x] **Pruned blocker.** Done long ago."],
        &[
            ("Follow the ghost", "item:Zz9Zz9Zz (gone)"),
            ("Follow the pruned", "item:{Pruned blocker}"),
        ],
    );
    let out = ready(
        &root,
        json!({"collection": "actions", "ready": {"field": "waits", "include": ALL}}),
    );
    assert_eq!(
        verdicts(&out),
        BTreeMap::from([
            ("Follow the ghost".into(), unresolved("dangling")),
            ("Follow the pruned".into(), class("ready")),
        ])
    );
    let diagnostics = out["diagnostics"].as_array().unwrap();
    assert_eq!(diagnostics.len(), 1, "{out}");
    assert_eq!(diagnostics[0]["code"], "unresolved_wait");
    assert_eq!(diagnostics[0]["path"], ALPHA);
    assert_eq!(diagnostics[0]["id"], ids["Follow the ghost"]);
    assert!(
        diagnostics[0]["message"]
            .as_str()
            .unwrap()
            .contains("Zz9Zz9Zz"),
        "{out}"
    );

    // Without the archive binding the pruned blocker cannot be found, and the
    // wait is dangling rather than assumed closed.
    std::fs::write(root.join(".rivet/project.json"), binding(false).to_string()).unwrap();
    let out = ready(
        &root,
        json!({"collection": "actions", "ready": {"field": "waits", "include": ALL}}),
    );
    assert_eq!(
        verdicts(&out)["Follow the pruned"],
        unresolved("dangling"),
        "{out}"
    );
    assert_eq!(out["readiness"]["unresolved"], 2);
}

#[test]
fn a_cyclic_wait_is_reported_for_every_member_of_the_loop() {
    let (root, _) = ready_project(
        "ready-cycle",
        &[
            "- [ ] **First.** Waits on second.",
            "- [ ] **Self.** Waits on itself.",
            "- [ ] **Into.** Waits on a member of the loop.",
        ],
        &["- [ ] **Second.** Waits on first."],
        &[],
        &[
            ("First", "item:{Second}"),
            ("Second", "item:{First}"),
            ("Self", "item:{Self}"),
            ("Into", "item:{First}"),
        ],
    );
    let out = ready(
        &root,
        json!({"collection": "actions", "ready": {"field": "waits", "include": ALL}}),
    );
    assert_eq!(
        verdicts(&out),
        BTreeMap::from([
            ("First".into(), unresolved("cycle")),
            ("Second".into(), unresolved("cycle")),
            ("Self".into(), unresolved("cycle")),
            // It waits on an open item; the loop is that item's problem.
            ("Into".into(), class("waiting")),
        ])
    );
    let codes: Vec<&Value> = out["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| &d["code"])
        .collect();
    assert_eq!(codes, [&json!("unresolved_wait"); 3], "{out}");

    // Closing one member breaks the loop: the other becomes ready.
    let ids = ids(&root);
    let (path, guard) = item(&root, &ids["Second"]);
    let (code, out) = rivet(
        &root,
        "update-item",
        json!({"path": path, "collection": "actions", "id": ids["Second"],
               "guard": guard, "set": {"done": true}}),
    );
    assert_eq!(code, 0, "{out}");
    let out = ready(
        &root,
        json!({"collection": "actions", "ready": {"field": "waits", "include": ALL}}),
    );
    assert_eq!(verdicts(&out)["First"], class("ready"));
    assert_eq!(verdicts(&out)["Into"], class("waiting"));
}

#[test]
fn waits_prefer_live_carriers_and_reject_non_task_targets() {
    let (root, ids) = ready_project(
        "ready-unresolved",
        &[
            "- [ ] **Twice.** Names an id two items carry.",
            "- [ ] **Section.** Names a heading.",
            "- [ ] **Cancelled.** Names a cancelled item.",
            "- [ ] **Malformed.** Names no id at all.",
            "- [ ] **Copied.** Also in the archive.",
            "- [-] **Dropped.** Not a task to the parser.",
        ],
        &[],
        &[],
        &[
            ("Twice", "item:{Copied}"),
            ("Section", "item:SECTION"),
            ("Cancelled", "item:{Dropped}"),
            ("Malformed", "item:repo#162 merge"),
        ],
    );
    // A live carrier takes precedence over its verbatim archive copy.
    let copied = read(&root, ALPHA)
        .lines()
        .find(|l| l.contains("**Copied.**"))
        .unwrap()
        .to_string();
    let mut archive = read(&root, ARCHIVE);
    archive.push_str(&format!("\n{copied}\n"));
    std::fs::write(root.join(ARCHIVE), archive).unwrap();
    // Point `Section` at the Action heading's own stay.
    let (_, section) = rivet(
        &root,
        "get",
        json!({"path": ALPHA, "select": {"kind": "heading_path", "path": ["Action"]}}),
    );
    let heading = section["target"]["id"].as_str().unwrap().to_string();
    let (path, guard) = item(&root, &ids["Section"]);
    let (code, out) = rivet(
        &root,
        "update-item",
        json!({"path": path, "collection": "actions", "id": ids["Section"],
               "guard": guard, "set": {"waits": format!("item:{heading}")}}),
    );
    assert_eq!(code, 0, "{out}");

    let out = ready(
        &root,
        json!({"collection": "actions", "ready": {"field": "waits", "include": ["waiting", "unresolved"]}}),
    );
    assert_eq!(
        verdicts(&out),
        BTreeMap::from([
            ("Twice".into(), class("waiting")),
            ("Section".into(), unresolved("not_an_item")),
            ("Cancelled".into(), unresolved("not_an_item")),
            ("Malformed".into(), unresolved("malformed")),
        ])
    );
    assert_eq!(out["diagnostics"].as_array().unwrap().len(), 3, "{out}");
    let twice = out["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["id"] == ids["Twice"])
        .unwrap();
    assert_eq!(twice["wait"]["on"]["path"], ALPHA);
    assert_eq!(twice["wait"]["on"]["archived"], false);
}

#[test]
fn a_ready_request_with_a_bad_field_or_class_is_refused() {
    let (root, _) = ready_project("ready-refused", &["- [ ] **One.** x"], &[], &[], &[]);
    for (field, why) in [
        ("nope", "is not a field"),
        ("done", "is not a string field"),
    ] {
        let (code, out) = rivet(
            &root,
            "query",
            json!({"collection": "actions", "ready": {"field": field}}),
        );
        assert_eq!(code, 2, "{out}");
        assert_eq!(out["error"]["code"], "invalid_request");
        assert!(
            out["error"]["message"].as_str().unwrap().contains(why),
            "{out}"
        );
    }
    let (code, out) = rivet(
        &root,
        "query",
        json!({"collection": "actions", "ready": {"field": "waits", "include": ["soon"]}}),
    );
    assert_eq!(code, 1, "an unknown class is a bad shape: {out}");
}

#[test]
fn a_ready_cursor_goes_stale_when_an_archive_file_changes() {
    let (root, _) = ready_project(
        "ready-cursor",
        &["- [ ] **One.** a", "- [ ] **Two.** b", "- [ ] **Three.** c"],
        &[],
        &["- [x] **Gone.** d"],
        &[("One", "none"), ("Two", "none"), ("Three", "none")],
    );
    let request = json!({"collection": "actions", "limit": 1, "ready": {"field": "waits"}});
    let out = ready(&root, request.clone());
    let cursor = out["next_cursor"].as_str().unwrap().to_string();
    let mut next = request.clone();
    next["cursor"] = json!(cursor);
    assert_eq!(
        ready(&root, next.clone())["items"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let mut archive = read(&root, ARCHIVE);
    archive.push_str("\nAppended by a prune.\n");
    std::fs::write(root.join(ARCHIVE), archive).unwrap();
    let (code, out) = rivet(&root, "query", next);
    assert_eq!(code, 2);
    assert_eq!(out["error"]["code"], "stale_cursor");
}

#[test]
fn paths_narrows_the_listing_not_the_resolution() {
    let (root, ids) = ready_project(
        "ready-paths",
        &["- [ ] **Assemble.** Needs parts.", "- [ ] **Test.** a"],
        &["- [ ] **Order parts.** Still open."],
        &[],
        &[("Assemble", "item:{Order parts}"), ("Test", "none")],
    );
    let request = json!({"collection": "actions", "paths": [ALPHA], "limit": 1,
                         "ready": {"field": "waits", "include": ALL}});
    let out = ready(&root, request.clone());
    let first = &out["items"][0];
    assert_eq!(first["wait"]["readiness"], "waiting", "{out}");
    assert_eq!(first["wait"]["on"]["path"], BETA);
    assert_eq!(first["wait"]["on"]["id"], ids["Order parts"]);
    // Beta is read only to resolve, and its bytes still guard the cursor.
    let mut next = request.clone();
    next["cursor"] = out["next_cursor"].clone();
    ready(&root, next.clone());
    let beta = read(&root, BETA).replace("Still open.", "Still open, chased.");
    std::fs::write(root.join(BETA), beta).unwrap();
    let (code, out) = rivet(&root, "query", next);
    assert_eq!(code, 2, "{out}");
    assert_eq!(out["error"]["code"], "stale_cursor");
}

#[test]
fn an_unparsable_archive_file_is_reported_and_still_guards_the_cursor() {
    let (root, _) = ready_project(
        "ready-unparsable",
        &["- [ ] **One.** a", "- [ ] **Two.** b"],
        &[],
        &[],
        &[("One", "none"), ("Two", "none")],
    );
    // A lone CR is refused by the parser, but the bytes were read.
    std::fs::write(root.join(ARCHIVE), "# Closed\r\nlone\rCR\n").unwrap();
    let request = json!({"collection": "actions", "limit": 1, "ready": {"field": "waits"}});
    let out = ready(&root, request.clone());
    assert!(
        out["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|d| d["path"] == ARCHIVE),
        "{out}"
    );
    assert_eq!(
        out["resolved_from"], 0,
        "an unparsable file resolves nothing"
    );
    let mut next = request.clone();
    next["cursor"] = out["next_cursor"].clone();
    ready(&root, next.clone());
    std::fs::write(root.join(ARCHIVE), "# Closed\r\nlone\rCR, edited\n").unwrap();
    let (code, out) = rivet(&root, "query", next);
    assert_eq!(code, 2, "{out}");
    assert_eq!(out["error"]["code"], "stale_cursor");

    // Bytes that are not UTF-8 are read too, and an edit to them is seen.
    std::fs::write(root.join(ARCHIVE), b"# Closed\n\xff\n").unwrap();
    let out = ready(&root, request.clone());
    assert_eq!(out["resolved_from"], 0, "{out}");
    assert!(
        out["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|d| d["path"] == ARCHIVE && d["code"] == "unsupported_syntax"),
        "{out}"
    );
    let mut next = request.clone();
    next["cursor"] = out["next_cursor"].clone();
    ready(&root, next.clone());
    std::fs::write(root.join(ARCHIVE), b"# Closed\n\xfe\n").unwrap();
    let (code, out) = rivet(&root, "query", next);
    assert_eq!(code, 2, "{out}");
    assert_eq!(out["error"]["code"], "stale_cursor");
}

#[test]
fn a_file_both_globs_match_is_read_once_as_a_member() {
    let (root, _) = ready_project(
        "ready-overlap",
        &["- [ ] **Assemble.** a"],
        &["- [x] **Order parts.** b"],
        &[],
        &[("Assemble", "item:{Order parts}")],
    );
    let mut config = binding(true);
    config["collections"]["actions"]["archive"] = json!(["tracking/*.md", "archive/*/*.md"]);
    std::fs::write(root.join(".rivet/project.json"), config.to_string()).unwrap();
    let out = ready(
        &root,
        json!({"collection": "actions", "ready": {"field": "waits", "include": ALL}}),
    );
    assert_eq!(verdicts(&out)["Assemble"], class("ready"), "{out}");
    assert_eq!(out["items"][0]["wait"]["on"]["archived"], false);
    assert_eq!(
        out["resolved_from"], 1,
        "only the archive file is extra: {out}"
    );
}

#[test]
fn a_heading_identity_conflict_does_not_resolve() {
    let (root, ids) = ready_project(
        "ready-conflict",
        &["- [ ] **Follow.** a", "- [x] **Blocker.** b"],
        &[],
        &[],
        &[("Follow", "item:{Blocker}"), ("Blocker", "none")],
    );
    let before = ready(
        &root,
        json!({"collection": "actions", "ready": {"field": "waits", "include": ALL}}),
    );
    assert_eq!(verdicts(&before)["Follow"], class("ready"));
    // The Log heading now claims the blocker's id beside its own stay.
    let text = read(&root, ALPHA).replace("## Log\n", &format!("## Log {{#{}}}\n", ids["Blocker"]));
    std::fs::write(root.join(ALPHA), text).unwrap();
    let out = ready(
        &root,
        json!({"collection": "actions", "ready": {"field": "waits", "include": ALL}}),
    );
    assert_eq!(verdicts(&out)["Follow"], unresolved("ambiguous"), "{out}");

    // The same claim in a second file: the message names both.
    let text = read(&root, BETA).replace("## Log\n", &format!("## Log {{#{}}}\n", ids["Blocker"]));
    std::fs::write(root.join(BETA), text).unwrap();
    let out = ready(
        &root,
        json!({"collection": "actions", "ready": {"field": "waits", "include": ALL}}),
    );
    assert_eq!(verdicts(&out)["Follow"], unresolved("ambiguous"), "{out}");
    let message = out["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["id"] == ids["Follow"])
        .unwrap()["message"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(
        message.contains(ALPHA) && message.contains(BETA),
        "{message}"
    );
}

#[test]
fn a_wait_field_with_a_default_is_refused() {
    let (root, _) = ready_project("ready-default", &["- [ ] **One.** x"], &[], &[], &[]);
    let path = root.join(".rivet/schemas/waits-action.json");
    let mut schema: Value =
        serde_json::from_str(&read(&root, ".rivet/schemas/waits-action.json")).unwrap();
    schema["fields"]["waits"]["default"] = json!("none");
    std::fs::write(path, schema.to_string()).unwrap();
    let (code, out) = rivet(
        &root,
        "query",
        json!({"collection": "actions", "ready": {"field": "waits"}}),
    );
    assert_eq!(code, 2, "{out}");
    assert_eq!(out["error"]["code"], "invalid_request");
    assert!(
        out["error"]["message"]
            .as_str()
            .unwrap()
            .contains("default"),
        "{out}"
    );
}

#[test]
fn a_loop_through_an_archived_blocker_reads_as_waiting() {
    let (root, ids) = ready_project(
        "ready-archive-loop",
        &["- [ ] **Member.** Waits on an archived open item."],
        &[],
        &["- [ ] **Archived.** Left open when its tracker retired."],
        &[("Member", "item:{Archived}")],
    );
    // The archived item's own wait points back, but archive files carry ids
    // only: their waits are no collection's and are not followed.
    let (_, got) = rivet(
        &root,
        "get",
        json!({"path": ARCHIVE, "select": {"kind": "id", "id": ids["Archived"]}}),
    );
    let content = format!(
        "{}\n**Waits**: item:{}",
        got["content"].as_str().unwrap(),
        ids["Member"]
    );
    let (code, out) = rivet(
        &root,
        "set",
        json!({"path": ARCHIVE, "select": {"kind": "id", "id": ids["Archived"]},
               "guard": got["revision"], "content": content}),
    );
    assert_eq!(code, 0, "{out}");
    let out = ready(
        &root,
        json!({"collection": "actions", "ready": {"field": "waits", "include": ALL}}),
    );
    assert_eq!(verdicts(&out)["Member"], class("waiting"), "{out}");
    assert_eq!(out["items"][0]["wait"]["on"]["archived"], true);
}
