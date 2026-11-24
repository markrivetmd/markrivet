mod common;
use common::*;
use serde_json::json;
use std::io::Write;
use std::process::{Command, Stdio};

fn raw(args: &[&str], input: &[u8]) -> std::process::Output {
    let mut child = raw_command()
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // Oversized requests may close stdin after the limit has been read.
    if let Err(e) = child.stdin.take().unwrap().write_all(input) {
        assert_eq!(e.kind(), std::io::ErrorKind::BrokenPipe);
    }
    child.wait_with_output().unwrap()
}

#[test]
fn root_requires_a_directory_before_running_any_command() {
    for args in [
        vec!["init", "--root"],
        vec!["--root"],
        vec!["inspect", "--root"],
        vec!["map", "--root"],
        vec!["--root", ".", "init", "--root"],
        vec!["init", "--root", "--pretty"],
        vec!["--root", "--pretty", "init"],
        vec!["--root", "-", "init"],
        vec!["--root", "--help"],
        vec!["--root", "--version"],
    ] {
        let dir = scratch("root-missing-value");
        let out = raw_command()
            .current_dir(&dir)
            .args(&args)
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(1), "{args:?}: {out:?}");
        assert!(out.stdout.is_empty(), "{args:?}: {out:?}");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.contains("--root requires a directory"),
            "{args:?}: {stderr}"
        );
        assert!(stderr.contains("usage: markrivet "), "{args:?}: {stderr}");
        assert!(!dir.join(".rivet").exists(), "{args:?}: init must not run");
    }
}

#[test]
fn root_reaches_a_dash_named_directory_as_a_relative_path() {
    let dir = scratch("root-dash-name");
    std::fs::create_dir(dir.join("-d")).unwrap();
    let out = raw_command()
        .current_dir(&dir)
        .args(["--root", "./-d", "init"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert!(dir.join("-d/.rivet").is_dir(), "{out:?}");
    assert!(!dir.join(".rivet").exists(), "{out:?}");
}

#[test]
fn inspect_exit_statuses_and_request_limits() {
    // Diagnostics are part of a successful read, as they are for `map`.
    for (source, clean) in [
        ("# hello\n", true),
        ("---\nx: &a v\n---\n", false),
        ("---\nvalid: true\nx: a\u{feff}b\n---\n", false),
    ] {
        let out = raw(
            &["inspect"],
            json!({"source": source}).to_string().as_bytes(),
        );
        assert_eq!(out.status.code(), Some(0));
        let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(v["ok"], true);
        assert_eq!(v["diagnostics"].as_array().unwrap().is_empty(), clean);
    }
    let out = raw(&["inspect"], br#"{"op":"inspect","source":""}"#);
    assert_eq!(out.status.code(), Some(1), "the command names the op");
    let out = raw(&["set"], br#"not json"#);
    assert_eq!(out.status.code(), Some(1));
    let mut oversized = vec![b' '; 16 * 1024 * 1024];
    oversized.extend_from_slice("😀".as_bytes());
    let out = raw(&["inspect"], &oversized);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stdout).contains("exceeds 16 MiB"));
    assert_eq!(raw(&["frobnicate"], b"{}").status.code(), Some(1));
}

#[test]
fn usage_section_archive_binding_loads_beside_collection_archives() {
    let root = project("usage-section-archives");
    let example = include_str!("../../../docs/usage.md")
        .split("### `section_archives`")
        .nth(1)
        .unwrap()
        .split("```json\n")
        .nth(1)
        .unwrap()
        .split("\n```")
        .next()
        .unwrap();
    let example: serde_json::Value = serde_json::from_str(example).unwrap();
    let mut config: serde_json::Value =
        serde_json::from_str(&read(&root, ".rivet/project.json")).unwrap();
    config["collections"]["actions"]["archive"] = json!(["archive/*/*_CLOSED_ITEMS_*.md"]);
    config["section_archives"] = example["section_archives"].clone();
    std::fs::write(root.join(".rivet/project.json"), config.to_string()).unwrap();
    let (code, out) = rivet(&root, "query", json!({"collection": "actions"}));
    assert_eq!(
        code, 0,
        "the documented archive namespaces must coexist: {out}"
    );

    // A flat history namespace can match an item archive too, even in an
    // empty archive directory (for example, EXAMPLE_CLOSED_ITEMS_HISTORY_2026-09.md).
    config["section_archives"]["history"]["archive"] = json!(["archive/*/*_HISTORY_*.md"]);
    std::fs::write(root.join(".rivet/project.json"), config.to_string()).unwrap();
    let (code, out) = rivet(&root, "query", json!({"collection": "actions"}));
    assert_eq!(code, 2, "{out}");
    assert_eq!(out["error"]["code"], "invalid_schema", "{out}");
    let message = out["error"]["message"].as_str().unwrap();
    assert!(message.contains("overlaps"), "{out}");
    assert!(message.contains("archive/*/*_CLOSED_ITEMS_*.md"), "{out}");
}

#[test]
fn init_creates_configuration_directory() {
    let dir = project("init");
    std::fs::remove_dir_all(dir.join(".rivet")).unwrap();
    let out = raw(&["--root", dir.to_str().unwrap(), "init"], b"");
    assert_eq!(out.status.code(), Some(0));
    assert!(dir.join(".rivet/schemas").is_dir());
    assert_eq!(
        read(&dir, ".rivet/.gitignore"),
        "/lock\n/seq\n/pending/\n/commits.jsonl\n/push.jsonl\n"
    );
    // Re-run on a project, init appends only the lines that are missing.
    std::fs::write(dir.join(".rivet/.gitignore"), "/lock\n# kept\n/pending/").unwrap();
    let out = raw(&["--root", dir.to_str().unwrap(), "init"], b"");
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(
        read(&dir, ".rivet/.gitignore"),
        "/lock\n# kept\n/pending/\n/seq\n/commits.jsonl\n/push.jsonl\n"
    );
    // Git keeps a leading space in a pattern, so ` /seq` is not `/seq`.
    std::fs::write(dir.join(".rivet/.gitignore"), " /seq\n").unwrap();
    raw(&["--root", dir.to_str().unwrap(), "init"], b"");
    assert!(read(&dir, ".rivet/.gitignore").starts_with(" /seq\n/lock\n/seq\n"));
    // Appending keeps every existing byte and the file's mode.
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(dir.join(".rivet/.gitignore"), b"\xff no newline").unwrap();
    std::fs::set_permissions(
        dir.join(".rivet/.gitignore"),
        std::fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    raw(&["--root", dir.to_str().unwrap(), "init"], b"");
    let bytes = std::fs::read(dir.join(".rivet/.gitignore")).unwrap();
    assert!(bytes.starts_with(b"\xff no newline\n/lock\n"), "{bytes:?}");
    let mode = std::fs::metadata(dir.join(".rivet/.gitignore"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);
    // A hard-linked or setuid ignore file is refused, both names untouched.
    let ignore = dir.join(".rivet/.gitignore");
    std::fs::write(&ignore, "/lock\n").unwrap();
    let other = dir.join("linked.txt");
    std::fs::hard_link(&ignore, &other).unwrap();
    let out = raw(&["--root", dir.to_str().unwrap(), "init"], b"");
    assert_eq!(out.status.code(), Some(2));
    assert_eq!(std::fs::read(&other).unwrap(), b"/lock\n");
    std::fs::remove_file(&other).unwrap();
    std::fs::set_permissions(&ignore, std::fs::Permissions::from_mode(0o4644)).unwrap();
    let out = raw(&["--root", dir.to_str().unwrap(), "init"], b"");
    assert_eq!(out.status.code(), Some(2));
    assert_eq!(read(&dir, ".rivet/.gitignore"), "/lock\n");
    assert_eq!(
        std::fs::metadata(&ignore).unwrap().permissions().mode() & 0o7777,
        0o4644
    );
    std::fs::set_permissions(&ignore, std::fs::Permissions::from_mode(0o644)).unwrap();
    // A failed sync after the append is uncertain, not a refusal.
    std::fs::write(dir.join(".rivet/.gitignore"), "/lock\n").unwrap();
    let out = raw_command()
        .args(["--root", dir.to_str().unwrap(), "init"])
        .env("RIVET_TEST_FAULT", "config_sync")
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(3),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("\"uncertain\""));
    // Concurrent inits converge: every line once, the existing bytes kept,
    // no temporary file left behind.
    std::fs::write(dir.join(".rivet/.gitignore"), "# mine\n").unwrap();
    let inits: Vec<_> = (0..8)
        .map(|_| {
            raw_command()
                .args(["--root", dir.to_str().unwrap(), "init"])
                .stdout(std::process::Stdio::null())
                .spawn()
                .unwrap()
        })
        .collect();
    for mut c in inits {
        assert!(c.wait().unwrap().success());
    }
    assert_eq!(
        read(&dir, ".rivet/.gitignore"),
        "# mine\n/lock\n/seq\n/pending/\n/commits.jsonl\n/push.jsonl\n"
    );
    let leftovers: Vec<_> = std::fs::read_dir(dir.join(".rivet"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
        .filter(|n| n.contains(".rivet-"))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
    // Without .rivet a project command is refused, not guessed.
    let bare = project("bare");
    std::fs::remove_dir_all(bare.join(".rivet")).unwrap();
    let (code, out) = rivet(&bare, "map", json!({"path": WIDGETS}));
    assert_eq!(code, 2);
    assert_eq!(out["error"]["code"], "not_found");
}

#[test]
fn tracker_walkthrough_through_the_cli() {
    let root = project("walkthrough");
    // Before stamping, items are listed with a position handle and no id.
    let (code, out) = rivet(&root, "query", json!({"collection": "actions"}));
    assert_eq!(code, 0);
    assert_eq!(out["total"], 5);
    assert_eq!(out["diagnostics"], json!([]), "{out}");
    for item in out["items"].as_array().unwrap() {
        assert_eq!(item["id"], json!(null), "{item}");
        assert_eq!(item["addressing"], "position", "{item}");
        assert_eq!(item["select"]["kind"], "position", "{item}");
    }
    stamp(&root, WIDGETS);
    stamp(&root, "tracking/TRACKING_DOCS.md");
    let open = open_actions(&root);
    assert_eq!(open.len(), 4);
    let (_, fixtures_id, _) = open
        .iter()
        .find(|(t, _, _)| t == "Refresh the fixtures.")
        .unwrap()
        .clone();
    let (_, item) = rivet(
        &root,
        "get",
        json!({"path": WIDGETS, "select": {"kind": "id", "id": fixtures_id}}),
    );
    let typed = &item["typed"]["actions"];
    assert_eq!(typed["fields"]["actor"], "agent");
    assert_eq!(typed["fields"]["date"], "2026-09-03");
    let (_, map) = rivet(&root, "map", json!({"path": WIDGETS}));
    let log = map["outline"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["title"] == "Log")
        .unwrap()
        .clone();
    let before = read(&root, WIDGETS);
    let (code, out) = rivet(
        &root,
        "write",
        json!({"path": WIDGETS, "ops": [
            {"op": "update_item", "collection": "actions", "id": fixtures_id,
             "guard": typed["revision"], "set": {"done": true}},
            {"op": "insert", "select": {"kind": "id", "id": log["id"]}, "position": "start",
             "guard": log["revision"], "stamp": true,
             "content": "### 2026-09-11: fixtures regenerated\n\nThe diff is empty."},
        ]}),
    );
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["status"], "applied");
    assert_eq!(out["durable"], true);
    let after = read(&root, WIDGETS);
    assert!(after.contains("- [x] `agent` **Refresh the fixtures."));
    assert!(after.contains(
        "Newest first, one entry per event.\n\n### 2026-09-11: fixtures regenerated\n<!-- stay:"
    ));
    // Unrelated bytes are untouched: the frontmatter and first action.
    let head = &before[..before.find("- [ ] `agent`").unwrap()];
    assert!(after.starts_with(head));
    assert_eq!(open_actions(&root).len(), 3);
    // The stale guard is refused with the current revision; nothing changes.
    let (code, out) = rivet(
        &root,
        "update-item",
        json!({"path": WIDGETS, "collection": "actions", "id": fixtures_id,
               "guard": typed["revision"], "set": {"done": false}}),
    );
    assert_eq!(code, 2);
    assert_eq!(out["error"]["code"], "guard_conflict");
    assert!(
        out["error"]["current"]
            .as_str()
            .unwrap()
            .starts_with("rivet:v1:action:")
    );
    assert_eq!(read(&root, WIDGETS), after);
    // A structural item guard is not a typed guard.
    let structural = item_guard(&root, WIDGETS, &fixtures_id);
    let (code, out) = rivet(
        &root,
        "update-item",
        json!({"path": WIDGETS, "collection": "actions", "id": fixtures_id, "guard": structural, "set": {"done": false}}),
    );
    assert_eq!(code, 2);
    assert!(
        out["error"]["message"]
            .as_str()
            .unwrap()
            .contains("item revision")
    );
}

#[test]
fn list_reshape_batch_refusal_leaves_the_file_byte_identical() {
    let root = project("reshape-batch");
    stamp(&root, WIDGETS);
    let before = read(&root, WIDGETS);
    let doc = markrivet_core::Document::parse(&before).unwrap();
    let list = &doc.lists[0];
    let list_id = &doc.stay_ids(&doc.blocks[list.block].markers)[0];
    let (_, item_id, _) = open_actions(&root)
        .into_iter()
        .find(|(title, _, _)| title == "Refresh the fixtures.")
        .unwrap();
    let (_, item) = rivet(
        &root,
        "get",
        json!({"path": WIDGETS, "select": {"kind": "id", "id": item_id}}),
    );
    let insert = json!({
        "op": "insert", "select": {"kind": "id", "id": list_id},
        "position": "end", "guard": doc.target_revision(markrivet_core::document::Target::Block(list.block)),
        "content": "[ ] New action.", "stamp": true
    });
    let tick = json!({
        "op": "update_item", "collection": "actions", "id": item_id,
        "guard": item["typed"]["actions"]["revision"], "set": {"done": true}
    });
    for preview in [false, true] {
        for ops in [json!([insert, tick]), json!([tick, insert])] {
            let (code, out) = rivet(
                &root,
                "write",
                json!({"path": WIDGETS, "ops": ops, "preview": preview}),
            );
            assert_eq!(code, 2, "{out}");
            assert_eq!(out["error"]["code"], "invalid_request", "{out}");
            assert!(
                out["error"]["message"]
                    .as_str()
                    .unwrap()
                    .contains("two operations touch the same unit"),
                "{out}"
            );
            assert_eq!(read(&root, WIDGETS), before);
        }
    }
}

#[test]
fn typed_insert_and_invalid_items_are_diagnosed() {
    let root = project("typed");
    stamp(&root, WIDGETS);
    let (_, map) = rivet(&root, "map", json!({"path": WIDGETS}));
    let guard = map["collections"][0]["revision"].clone();
    let (code, out) = rivet(
        &root,
        "insert-item",
        json!({"path": WIDGETS, "collection": "actions", "position": "end", "guard": guard,
               "fields": {"title": "Publish the fixtures", "actor": "agent", "date": "2026-09-11",
                          "gate": "the fixtures are on main."}}),
    );
    assert_eq!(code, 0, "{out}");
    let id = out["ops"][0]["minted"][0]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let (_, q) = rivet(
        &root,
        "query",
        json!({"collection": "actions", "where": [{"field": "id", "op": "eq", "value": id}]}),
    );
    assert_eq!(q["items"][0]["fields"]["gate"], "the fixtures are on main.");
    // Values are validated, not coerced.
    let before = std::fs::read(root.join(WIDGETS)).unwrap();
    let (code, map) = rivet(&root, "map", json!({"path": WIDGETS}));
    assert_eq!(code, 0, "{map}");
    let guard = map["collections"][0]["revision"].clone();
    let (code, out) = rivet(
        &root,
        "insert-item",
        json!({"path": WIDGETS, "collection": "actions", "position": "end", "guard": guard,
               "fields": {"title": "Bad date", "date": "2026-02-30"}}),
    );
    assert_eq!(code, 2, "{out}");
    assert_eq!(out["error"]["code"], "invalid_item", "{out}");
    assert_eq!(std::fs::read(root.join(WIDGETS)).unwrap(), before);
    // This schema does not require a title, so a hand-edit that removes one
    // leaves a usable action rather than an invalid item: its text moves to
    // the context field. Requiring the title is a per-project choice, and the
    // diagnostic it produces is covered in `a_required_title_is_still_enforced`.
    let text =
        read(&root, WIDGETS).replace("**Ship the widget. (2026-09-01)**", "Ship the widget.");
    std::fs::write(root.join(WIDGETS), text).unwrap();
    let (_, q) = rivet(&root, "query", json!({"collection": "actions"}));
    assert!(
        q["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .all(|d| d["path"] != WIDGETS),
        "{q}"
    );
    let stripped = q["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["fields"]["context"] == "Ship the widget. The release branch is cut; the\nchangelog still needs the migration note.")
        .unwrap_or_else(|| panic!("{q}"));
    assert!(stripped["fields"].get("title").is_none(), "{stripped}");
    assert!(stripped["fields"].get("date").is_none(), "{stripped}");
    // A clause that does not fit the field type is a request error.
    let (code, out) = rivet(
        &root,
        "query",
        json!({"collection": "actions", "where": [{"field": "done", "op": "eq", "value": "false"}]}),
    );
    assert_eq!(code, 2);
    assert_eq!(out["error"]["code"], "invalid_request");
}

#[test]
fn schema_changes_invalidate_typed_guards() {
    let root = project("schema");
    stamp(&root, WIDGETS);
    let open = open_actions(&root);
    let (_, id, revision) = &open
        .iter()
        .find(|(t, _, _)| t == "Ship the widget.")
        .unwrap();
    let schema = root.join(".rivet/schemas/tracker-action.json");
    let text = std::fs::read_to_string(&schema).unwrap();
    std::fs::write(&schema, text.replace("\"human:owner\"", "\"human:lead\"")).unwrap();
    let (code, out) = rivet(
        &root,
        "update-item",
        json!({"path": WIDGETS, "collection": "actions", "id": id, "guard": revision, "set": {"done": true}}),
    );
    assert_eq!(code, 2);
    assert_eq!(out["error"]["code"], "guard_conflict");
    // A missing schema is a diagnostic error, not an empty result.
    std::fs::remove_file(&schema).unwrap();
    let (code, out) = rivet(&root, "query", json!({"collection": "actions"}));
    assert_eq!(code, 2);
    assert_eq!(out["error"]["code"], "invalid_schema");
    // Field guard scope is reserved.
    std::fs::write(&schema, text).unwrap();
    let (code, out) = rivet(
        &root,
        "update-item",
        json!({"path": WIDGETS, "collection": "actions", "id": id, "guard": revision,
               "guard_scope": "field", "set": {"done": true}}),
    );
    assert_eq!(code, 2);
    assert_eq!(out["error"]["code"], "unsupported_syntax");
}

#[test]
fn paths_are_confined_and_metadata_is_checked() {
    let root = project("paths");
    for bad in [
        "/etc/passwd",
        "../x.md",
        "tracking/../tracking/TRACKING_DOCS.md",
        ".rivet/project.json",
        "tracking",
    ] {
        let (code, out) = rivet(&root, "map", json!({"path": bad}));
        assert_eq!(code, 2, "{bad}");
        assert!(
            ["path_rejected", "not_found"].contains(&out["error"]["code"].as_str().unwrap()),
            "{bad}: {out}"
        );
    }
    std::os::unix::fs::symlink(root.join(WIDGETS), root.join("tracking/LINK.md")).unwrap();
    let (code, out) = rivet(&root, "map", json!({"path": "tracking/LINK.md"}));
    assert_eq!(code, 2);
    assert_eq!(out["error"]["code"], "path_rejected");
    // A hard-linked target is readable but never replaced.
    std::fs::hard_link(root.join(WIDGETS), root.join("tracking/HARD.md")).unwrap();
    let (_, map) = rivet(&root, "map", json!({"path": WIDGETS}));
    let before = read(&root, WIDGETS);
    let (code, out) = rivet(
        &root,
        "stamp",
        json!({"path": WIDGETS, "guard": map["revision"]}),
    );
    assert_eq!(code, 2);
    assert_eq!(out["error"]["code"], "path_rejected");
    assert_eq!(read(&root, WIDGETS), before);
}

#[test]
fn query_normalizes_requested_paths_before_binding_and_filtering() {
    let root = project("query-path-spellings");
    stamp(&root, WIDGETS);
    let before = read(&root, WIDGETS);
    let (_, expected) = rivet(
        &root,
        "query",
        json!({"collection":"actions","paths":[WIDGETS]}),
    );
    assert!(
        !expected["items"].as_array().unwrap().is_empty(),
        "{expected}"
    );
    for path in [
        format!("./{WIDGETS}"),
        WIDGETS.replacen('/', "/./", 1),
        WIDGETS.replacen('/', "//", 1),
    ] {
        let (code, out) = rivet(
            &root,
            "query",
            json!({"collection":"actions","paths":[path]}),
        );
        assert_eq!(code, 0, "{out}");
        assert_eq!(out, expected);
    }
    let (code, out) = rivet(
        &root,
        "query",
        json!({"collection":"actions","paths":["../escape.md"]}),
    );
    assert_eq!(code, 2, "{out}");
    assert_eq!(out["error"]["code"], "path_rejected");
    assert_eq!(read(&root, WIDGETS), before);
}

#[test]
fn preview_and_noop_writes_do_not_touch_the_file() {
    let root = project("preview");
    let (_, map) = rivet(&root, "map", json!({"path": WIDGETS}));
    let before = std::fs::metadata(root.join(WIDGETS)).unwrap();
    let (code, out) = rivet(
        &root,
        "stamp",
        json!({"path": WIDGETS, "guard": map["revision"], "preview": true}),
    );
    assert_eq!(code, 0);
    assert_eq!(out["status"], "preview");
    assert_eq!(out["ops"][0]["minted"].as_array().unwrap().len(), 10);
    let after = std::fs::metadata(root.join(WIDGETS)).unwrap();
    assert_eq!(before.modified().unwrap(), after.modified().unwrap());
    stamp(&root, WIDGETS);
    let (_, map) = rivet(&root, "map", json!({"path": WIDGETS}));
    let (code, out) = rivet(
        &root,
        "stamp",
        json!({"path": WIDGETS, "guard": map["revision"]}),
    );
    assert_eq!(code, 0);
    assert_eq!(out["status"], "unchanged");
}

#[test]
fn selectors_fail_closed() {
    let root = project("selectors");
    stamp(&root, WIDGETS);
    let (code, out) = rivet(
        &root,
        "get",
        json!({"path": WIDGETS, "select": {"kind": "id", "id": "nope"}}),
    );
    assert_eq!(code, 2);
    assert_eq!(out["error"]["code"], "missing_selector");
    let (code, out) = rivet(
        &root,
        "get",
        json!({"path": WIDGETS, "select": {"kind": "heading_path", "path": ["Widgets", "Log"]}}),
    );
    assert_eq!(code, 0);
    assert_eq!(out["title"], "Log");
    // A duplicated id resolves to nothing; other stays still work.
    let text = read(&root, WIDGETS);
    let first = open_actions(&root)
        .into_iter()
        .find(|a| !a.1.is_empty())
        .unwrap()
        .1;
    std::fs::write(
        root.join(WIDGETS),
        format!("{text}\nCopied.\n<!-- stay:{first} -->\n"),
    )
    .unwrap();
    let (code, out) = rivet(
        &root,
        "get",
        json!({"path": WIDGETS, "select": {"kind": "id", "id": first}}),
    );
    assert_eq!(code, 2);
    assert_eq!(out["error"]["code"], "duplicate_id");
    let (code, _) = rivet(
        &root,
        "get",
        json!({"path": WIDGETS, "select": {"kind": "heading_path", "path": ["Log"]}}),
    );
    assert_eq!(code, 0);
    // Oversized reads refuse instead of truncating.
    let (code, out) = rivet(
        &root,
        "get",
        json!({"path": WIDGETS, "select": {"kind": "heading_path", "path": ["Widgets"]}, "max_bytes": 10}),
    );
    assert_eq!(code, 2);
    assert_eq!(out["error"]["code"], "limit_exceeded");
}

#[test]
fn moved_items_keep_identity_and_guards() {
    let root = project("moved");
    stamp(&root, WIDGETS);
    let open = open_actions(&root);
    let (_, id, _) = open
        .iter()
        .find(|(t, _, _)| t == "Ship the widget.")
        .unwrap()
        .clone();
    let guard = item_guard(&root, WIDGETS, &id);
    // A human moves the first action to the end of the list.
    let text = read(&root, WIDGETS);
    let start = text.find("- [ ] **Ship").unwrap();
    let end = start + text[start..].find("\n\n").unwrap() + 2;
    let item = text[start..end - 2].to_string();
    let mut moved = text.clone();
    moved.replace_range(start..end, "");
    let anchor = moved.find("reply on the thread. <!--").unwrap();
    let line_end = anchor + moved[anchor..].find('\n').unwrap();
    moved.insert_str(line_end, &format!("\n\n{item}"));
    std::fs::write(root.join(WIDGETS), &moved).unwrap();
    let (code, out) = rivet(
        &root,
        "set",
        json!({"path": WIDGETS, "select": {"kind": "id", "id": id}, "guard": guard,
               "content": "[x] **Ship the widget. (2026-09-01)** Shipped.\n    **Gate**: `make release-check` passes."}),
    );
    assert_eq!(code, 0, "{out}");
    let after = read(&root, WIDGETS);
    assert!(after.find("Shipped.").unwrap() > after.find("reply on the thread.").unwrap());
}

#[test]
fn typed_writes_return_the_next_typed_guards() {
    let root = project("follow");
    stamp(&root, WIDGETS);
    let (_, id, revision) = open_actions(&root)
        .into_iter()
        .find(|a| a.0 == "Ship the widget.")
        .unwrap();
    let (code, out) = rivet(
        &root,
        "update-item",
        json!({"path": WIDGETS, "collection": "actions", "id": id, "guard": revision,
               "set": {"gate": "the release is tagged."}}),
    );
    assert_eq!(code, 0, "{out}");
    let next = out["ops"][0]["revision"].as_str().unwrap().to_string();
    assert!(next.starts_with("rivet:v1:action:"));
    let collection = out["ops"][0]["collection_revision"]
        .as_str()
        .unwrap()
        .to_string();
    // The returned guards work immediately, with no re-read.
    let (code, out) = rivet(
        &root,
        "update-item",
        json!({"path": WIDGETS, "collection": "actions", "id": id, "guard": next,
               "set": {"done": true}}),
    );
    assert_eq!(code, 0, "{out}");
    let (code, out) = rivet(
        &root,
        "insert-item",
        json!({"path": WIDGETS, "collection": "actions", "position": "end",
               "guard": out["ops"][0]["collection_revision"], "fields": {"title": "Next"}}),
    );
    assert_eq!(code, 0, "{out}");
    assert_ne!(
        collection,
        out["ops"][0]["collection_revision"].as_str().unwrap()
    );
}

/// usage.md § Typed writes: standalone, `path` and no `op`; in a batch, `op`
/// and no `path`. Each mix-up is refused with the message the docs quote.
#[test]
fn typed_writes_take_the_documented_standalone_and_batch_shapes() {
    let root = project("shapes");
    stamp(&root, WIDGETS);
    let (_, id, revision) = open_actions(&root)
        .into_iter()
        .find(|a| a.0 == "Ship the widget.")
        .unwrap();
    let before = read(&root, WIDGETS);
    for (request, message) in [
        (
            json!({"op": "update_item", "path": WIDGETS, "collection": "actions", "id": id,
                   "guard": revision, "set": {"done": true}}),
            "drop \"op\" from the request",
        ),
        (
            json!({"collection": "actions", "id": id, "guard": revision,
                   "set": {"done": true}}),
            "missing field `path`",
        ),
    ] {
        let (code, out) = rivet(&root, "update-item", request);
        assert_eq!(code, 1, "{out}");
        assert_eq!(out["error"]["code"], "invalid_request");
        assert!(
            out["error"]["message"].as_str().unwrap().contains(message),
            "{out}"
        );
        assert_eq!(read(&root, WIDGETS), before);
    }
    let (code, out) = rivet(
        &root,
        "update-item",
        json!({"path": WIDGETS, "collection": "actions", "id": id, "guard": revision,
               "set": {"done": true}}),
    );
    assert_eq!(code, 0, "{out}");
    let next = out["ops"][0]["revision"].clone();
    let (code, out) = rivet(
        &root,
        "write",
        json!({"path": WIDGETS, "ops": [{"op": "update_item", "collection": "actions",
               "id": id, "guard": next, "set": {"done": false}}]}),
    );
    assert_eq!(code, 0, "{out}");
}

#[test]
fn a_swapped_parent_directory_cannot_redirect_the_write() {
    let root = project("swap");
    stamp(&root, WIDGETS);
    let (_, id, _) = open_actions(&root)
        .into_iter()
        .find(|a| a.0 == "Ship the widget.")
        .unwrap();
    let guard = item_guard(&root, WIDGETS, &id);
    let original = read(&root, WIDGETS);
    let pause = Pause::new(&root, "before_rename");
    let writer = spawn(
        &root,
        "set",
        &json!({"path": WIDGETS, "select": {"kind": "id", "id": id}, "guard": guard,
                "content": "[x] **Ship the widget.** Written."}),
        &[pause.env()],
    );
    pause.wait();
    // Move the validated directory away and put an identical decoy in its place.
    // Keep the moved directory inside the fresh fixture. A sibling survives
    // fixture resets and collides when a later test process reuses its PID.
    let moved = root.join("moved-tracking");
    std::fs::rename(root.join("tracking"), &moved).unwrap();
    std::fs::create_dir(root.join("tracking")).unwrap();
    std::fs::write(root.join(WIDGETS), &original).unwrap();
    pause.release();
    let (code, out) = finish(writer);
    assert_eq!(code, 0, "{out}");
    // The write went to the file that was checked, never to the decoy.
    assert_eq!(read(&root, WIDGETS), original);
    assert!(
        std::fs::read_to_string(moved.join("TRACKING_WIDGETS.md"))
            .unwrap()
            .contains("Written.")
    );
}

#[test]
fn documented_walkthrough_preserves_redirected_log() {
    if Command::new("jq").arg("--version").output().is_err() {
        eprintln!("skipping: the walkthrough needs jq");
        return;
    }
    let script = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/tracker/walkthrough.sh");
    let root = project("walkthrough-log");
    let log_path = root.join("gate.log");
    let mut log = std::fs::File::create(&log_path).unwrap();
    let marker = b"gate output before walkthrough\n";
    log.write_all(marker).unwrap();
    // Match `{ echo marker; walkthrough; } > log 2>&1`: both streams share
    // the regular file's offset, with earlier gate output already written.
    let mut command = Command::new("bash");
    let status = common::without_debug_hooks(&mut command)
        .arg(script)
        .env("RIVET", env!("CARGO_BIN_EXE_markrivet"))
        .stdout(log.try_clone().unwrap())
        .stderr(log)
        .status()
        .unwrap();
    let output = std::fs::read(&log_path).unwrap();
    let text = String::from_utf8_lossy(&output);
    assert!(status.success(), "{text}");
    assert!(
        output.starts_with(marker),
        "walkthrough truncated earlier gate output: {text}"
    );
    assert!(text.ends_with("walkthrough ok\n"), "{text}");
}

#[cfg(target_os = "linux")]
#[test]
fn documented_walkthrough_reports_stderr_failure() {
    if Command::new("jq").arg("--version").output().is_err() {
        eprintln!("skipping: the walkthrough needs jq");
        return;
    }
    let script = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/tracker/walkthrough.sh");
    let sink = std::fs::OpenOptions::new()
        .write(true)
        .open("/dev/full")
        .unwrap();
    let mut command = Command::new("bash");
    let out = common::without_debug_hooks(&mut command)
        .arg(script)
        .env("RIVET", env!("CARGO_BIN_EXE_markrivet"))
        .stderr(sink)
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    // The first step that writes to stderr through `show`.
    assert!(
        text.contains("U2b. Unstamped: find names the same place"),
        "{text}"
    );
    assert!(!out.status.success(), "stderr failure was ignored: {text}");
    assert!(!text.contains("walkthrough ok"), "{text}");
}

#[test]
fn trials_harness_runs_quickly() {
    if Command::new("python3").arg("--version").output().is_err() {
        eprintln!("skipping: the trials harness needs python3");
        return;
    }
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let output = project("trials").join("results.json");
    let mut command = Command::new("python3");
    let out = common::without_debug_hooks(&mut command)
        .arg(root.join("benchmark/trials/trials.py"))
        .arg("--quick")
        .arg("--output")
        .arg(&output)
        .env("RIVET", env!("CARGO_BIN_EXE_markrivet"))
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let results: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(output).unwrap()).unwrap();
    for r in results["results"].as_array().unwrap() {
        if r["condition"] == "rivet" {
            assert_eq!(r["correct"], true, "{r}");
            assert_eq!(r["lost_updates"], 0, "{r}");
        }
    }
}

#[test]
fn second_review_regressions() {
    let root = project("regressions");
    stamp(&root, WIDGETS);
    let open = open_actions(&root);
    let (_, id, revision) = open
        .iter()
        .find(|a| a.0 == "Ship the widget.")
        .unwrap()
        .clone();
    // A live MDX-form marker blocks typed writes as well as structural ones.
    let text = read(&root, WIDGETS);
    std::fs::write(
        root.join(WIDGETS),
        format!("{text}\nNote {{/* stay:mdx */}} here.\n"),
    )
    .unwrap();
    let (code, out) = rivet(
        &root,
        "update-item",
        json!({"path": WIDGETS, "collection": "actions", "id": id, "guard": revision, "set": {"done": true}}),
    );
    assert_eq!(code, 2);
    assert_eq!(out["error"]["code"], "unsupported_syntax");
    std::fs::write(root.join(WIDGETS), &text).unwrap();
    // A result larger than the read limit is refused before writing.
    let (_, log) = rivet(
        &root,
        "get",
        json!({"path": WIDGETS, "select": {"kind": "heading_path", "path": ["Log"]}}),
    );
    let huge = "a".repeat(8 * 1024 * 1024);
    let (code, out) = rivet(
        &root,
        "set",
        json!({"path": WIDGETS, "select": {"kind": "heading_path", "path": ["Log"]}, "part": "body",
               "guard": log["revision"], "content": huge, "allow_drop": true}),
    );
    assert_eq!(code, 2);
    assert_eq!(out["error"]["code"], "limit_exceeded");
    assert_eq!(read(&root, WIDGETS), text);
    // Dates and enums in clauses are validated, not compared as strings.
    let (code, out) = rivet(
        &root,
        "query",
        json!({"collection": "actions", "where": [{"field": "date", "op": "lt", "value": "yesterday"}]}),
    );
    assert_eq!(code, 2);
    assert_eq!(out["error"]["code"], "invalid_request");
    // A cursor does not survive a binding change.
    let (_, first) = rivet(&root, "query", json!({"collection": "actions", "limit": 1}));
    let cursor = first["next_cursor"].as_str().unwrap().to_string();
    let config = root.join(".rivet/project.json");
    let original = std::fs::read_to_string(&config).unwrap();
    std::fs::write(
        &config,
        original
            .replace("\"level\": 2", "\"level\": 2, \"section\": \"Log\"")
            .replace("\"section\": \"Action\",\n      ", ""),
    )
    .unwrap();
    let (code, out) = rivet(
        &root,
        "query",
        json!({"collection": "actions", "limit": 1, "cursor": cursor}),
    );
    assert_eq!(code, 2, "{out}");
    assert!(
        ["stale_cursor", "invalid_schema"].contains(&out["error"]["code"].as_str().unwrap()),
        "{out}"
    );
    std::fs::write(&config, &original).unwrap();
    // Configuration is read without following symlinks.
    let schema = root.join(".rivet/schemas/tracker-action.json");
    let outside = root.with_extension("outside-schema.json");
    std::fs::rename(&schema, &outside).unwrap();
    std::os::unix::fs::symlink(&outside, &schema).unwrap();
    let (code, out) = rivet(&root, "query", json!({"collection": "actions"}));
    assert_eq!(code, 2);
    assert_eq!(out["error"]["code"], "path_rejected", "{out}");
}

#[test]
fn numbers_compare_exactly_and_null_removes_optional_fields() {
    let root = project("numbers");
    let schema = json!({"id": "estimate", "version": 1, "item": "checkbox", "fields": {
        "done": {"type": "boolean", "from": "checkbox"},
        "title": {"type": "string", "from": "title"},
        "size": {"type": "number", "from": "label", "label": "Size"}}});
    std::fs::write(
        root.join(".rivet/schemas/estimate.json"),
        schema.to_string(),
    )
    .unwrap();
    std::fs::write(
        root.join(".rivet/project.json"),
        json!({"collections": {"estimates": {"schema": "estimate@1", "files": ["est/*.md"], "section": "Work"}}}).to_string(),
    )
    .unwrap();
    std::fs::create_dir(root.join("est")).unwrap();
    std::fs::write(
        root.join("est/E.md"),
        "## Work\n\n- [ ] **Big**\n  **Size**: 9223372036854775809\n- [ ] **Small**\n  **Size**: 9223372036854775807\n",
    )
    .unwrap();
    stamp(&root, "est/E.md");
    let (code, q) = rivet(
        &root,
        "query",
        json!({"collection": "estimates", "where": [{"field": "size", "op": "gt", "value": 9223372036854775808u64}]}),
    );
    assert_eq!(code, 0, "{q}");
    let titles: Vec<&str> = q["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["fields"]["title"].as_str().unwrap())
        .collect();
    assert_eq!(titles, ["Big"]);
    // Exponents are compared exactly; one too large to order is refused.
    let (code, q2) = rivet(
        &root,
        "query",
        json!({"collection": "estimates", "where": [{"field": "size", "op": "lt", "value": 1e30}]}),
    );
    assert_eq!(code, 0, "{q2}");
    assert_eq!(q2["total"], 2);
    let (code, out) = rivet_raw(
        &root,
        "query",
        r#"{"collection": "estimates", "where": [{"field": "size", "op": "lt", "value": 1e9223372036854775807}]}"#,
    );
    assert_eq!(code, 2, "{out}");
    let big = q["items"][0].clone();
    let (code, out) = rivet(
        &root,
        "update-item",
        json!({"path": "est/E.md", "collection": "estimates", "id": big["id"], "guard": big["revision"],
               "set": {"title": null}}),
    );
    assert_eq!(code, 0, "{out}");
    let (_, q) = rivet(
        &root,
        "query",
        json!({"collection": "estimates", "where": [{"field": "id", "op": "eq", "value": big["id"]}]}),
    );
    let fields = &q["items"][0]["fields"];
    assert!(fields.get("title").is_none(), "{q}");
    assert_eq!(fields["size"].to_string(), "9223372036854775809");
}

#[test]
fn unlistable_directories_are_reported() {
    // SAFETY: geteuid has no preconditions.
    if unsafe { libc::geteuid() } == 0 {
        eprintln!("skipping: root can list any directory");
        return;
    }
    let root = project("unlistable");
    let config = root.join(".rivet/project.json");
    let text = std::fs::read_to_string(&config).unwrap();
    std::fs::write(&config, text.replace("tracking/*.md", "tracking/**/*.md")).unwrap();
    let hidden = root.join("tracking/private");
    std::fs::create_dir(&hidden).unwrap();
    std::fs::copy(root.join(WIDGETS), hidden.join("TRACKING_HIDDEN.md")).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&hidden, std::fs::Permissions::from_mode(0o000)).unwrap();
    let (code, q) = rivet(&root, "query", json!({"collection": "actions"}));
    std::fs::set_permissions(&hidden, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(code, 0);
    assert!(
        q["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|d| d["code"] == "io" && d["path"] == "tracking/private"),
        "{q}"
    );
}

/// Send a request verbatim, for JSON that serde_json values cannot express.
fn rivet_raw(root: &std::path::Path, cmd: &str, request: &str) -> (i32, serde_json::Value) {
    let mut child = common::command(root, cmd, &[]).spawn().unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(request.as_bytes())
        .unwrap();
    common::finish(child)
}

/// A stamp defaults to the sections a collection binds: every section still
/// gets an id, so a Log heading stays addressable, but prose list items do
/// not fill the file with ids nothing asks for.
#[test]
fn stamp_defaults_to_bound_collections() {
    let root = project("stamp-scope");
    let kinds = |out: &serde_json::Value| {
        let mut n = (0, 0, 0);
        for m in out["ops"][0]["minted"].as_array().unwrap() {
            match m["kind"].as_str().unwrap() {
                "section" => n.0 += 1,
                "item" => n.1 += 1,
                _ => n.2 += 1,
            }
        }
        n
    };
    let (_, map) = rivet(&root, "map", json!({"path": WIDGETS}));
    let guard = map["revision"].clone();
    let (code, narrow) = rivet(
        &root,
        "stamp",
        json!({"path": WIDGETS, "guard": guard, "preview": true}),
    );
    assert_eq!(code, 0);
    let (code, whole) = rivet(
        &root,
        "stamp",
        json!({"path": WIDGETS, "guard": guard,
               "select": {"kind": "document"}, "preview": true}),
    );
    assert_eq!(code, 0);
    let (ns, ni, _) = kinds(&narrow);
    let (ws, wi, _) = kinds(&whole);
    assert_eq!(ns, ws, "every section is stamped either way");
    assert!(ni < wi, "the default stamps fewer items: {ni} vs {wi}");

    // The narrow stamp is enough to run the workflow: the Log section has an
    // id, and the actions it did stamp are addressable.
    stamp(&root, WIDGETS);
    let (_, map) = rivet(&root, "map", json!({"path": WIDGETS}));
    let log = map["outline"]
        .as_array()
        .unwrap()
        .iter()
        .find(|o| o["title"] == "Log")
        .expect("a Log section");
    assert!(log["id"].is_string(), "the Log heading has a stay");
}

/// A collection whose first list does not exist yet renders the new item from
/// fields and stamps it. That path ran its own §3.4 check, which predated
/// v1.8's code-span mask, so a backticked `<...>` was refused there while the
/// rest of the engine accepted it. It now runs the same scan as everything else.
///
/// Text rendered from fields is nowhere in the document, so a refusal here
/// carries no blocker: the reason names the character and the caller holds the
/// fields it came from.
///
/// Both cases run against the *empty* collection, refusal first: the accepted
/// insert creates the list, after which nothing reaches this path again.
#[test]
fn a_first_list_applies_the_same_carrier_rule_as_the_rest() {
    let root = project("first-list");
    let empty = "---\nstatus: active\nentity: empty\nowner: owner\n---\n\n\
                 # Empty\n\n## Action\n\n## Log\n\nNewest first.\n";
    let path = "tracking/TRACKING_EMPTY.md";
    std::fs::write(root.join(path), empty).unwrap();
    let guard = |root: &std::path::Path| {
        let (_, map) = rivet(root, "map", json!({"path": path}));
        map["collections"][0]["revision"].clone()
    };
    let insert = |root: &std::path::Path, title: &str| {
        rivet(
            root,
            "insert-item",
            json!({"path": path, "collection": "actions", "position": "end",
                   "guard": guard(root), "fields": {"title": title, "gate": "it parses."}}),
        )
    };
    // A raw '<' refuses, and names no position in a file it is not in.
    let (code, out) = insert(&root, "Handle <tag> input");
    assert_eq!(code, 2, "{out}");
    assert_eq!(out["error"]["code"], "stamp_refused");
    assert!(out["error"]["blocker"].is_null(), "{out}");
    assert_eq!(read(&root, path), empty, "a refusal changes nothing");

    // A closed code span is inert under v1.8, here as anywhere else, and this
    // is still the first list: the refusal above left the collection empty.
    let (code, out) = insert(&root, "Handle `<tag>` input");
    assert_eq!(code, 0, "{out}");
    let after = read(&root, path);
    assert!(after.contains("Handle `<tag>` input"));
    // Both stays land, and each is live rather than merely present as text: a
    // bare id in the prose would satisfy a substring search.
    let minted: std::collections::HashMap<&str, &str> = out["ops"][0]["minted"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| (m["kind"].as_str().unwrap(), m["id"].as_str().unwrap()))
        .collect();
    let (code, item) = rivet(
        &root,
        "get",
        json!({"path": path, "select": {"kind": "id", "id": minted["item"]}}),
    );
    assert_eq!(code, 0, "the item stay does not resolve: {item}");
    assert_eq!(
        item["typed"]["actions"]["fields"]["title"], "Handle `<tag>` input",
        "the item stay is on some other item"
    );
    let (code, list) = rivet(
        &root,
        "get",
        json!({"path": path, "select": {"kind": "id", "id": minted["list"]}}),
    );
    assert_eq!(code, 0, "the list stay does not resolve: {list}");
    // Resolving is not enough: moved under the heading it would still resolve,
    // as that heading's stay. It has to be a list, and hold the item's marker.
    assert_eq!(list["target"]["kind"], "list", "{list}");
    assert!(
        list["text"].as_str().unwrap().contains(minted["item"]),
        "the list does not contain the item it stamped: {list}"
    );
    // Everything outside the inserted region is byte-identical. The whole prefix
    // up to the insertion point, not just up to the heading: stopping at
    // `## Action\n` leaves the blank line after it unchecked, and a splice of
    // `[at - 1, at)` instead of `[at, at)` would eat it and pass everything else.
    let insertion_point = empty.find("## Log\n").unwrap();
    let log_body = &empty[insertion_point + "## Log\n".len()..];
    assert!(
        after.starts_with(&empty[..insertion_point]),
        "text before the list moved"
    );
    assert!(
        after.ends_with(&format!("## Log\n{log_body}")),
        "text after the list moved"
    );
    // The prefix and suffix bound the inserted region but say nothing about what
    // landed inside it, so pin the content too: a duplicated item would preserve
    // both ends and every string asserted above.
    assert_eq!(after.matches("- [ ]").count(), 1, "{after}");
    // The list is separated from the next heading, which the prefix comparison
    // cannot cover because the inserted text lands between them.
    assert!(after.contains("\n\n## Log\n"), "{after}");
    let (_, q) = rivet(&root, "query", json!({"collection": "actions"}));
    assert!(
        !q["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|d| d["path"] == path),
        "the inserted item is addressable: {q}"
    );
}

/// Trackers written before the typed schema existed carry actions with no
/// `**strong**` title. They are ordinary actions, and promoting one to the
/// canonical shape is a single guarded write rather than a migration.
#[test]
fn legacy_actions_without_a_strong_title_are_usable() {
    let root = project("legacy");
    let path = "tracking/TRACKING_LEGACY.md";
    let source = "# Legacy\n\n## Action\n\n\
        - [ ] `agent` Ask the vendor about pricing before the renewal date.\n\
        \x20     **Gate**: a quote in writing.\n\n\
        - [ ] Chase the missing invoice from last month.\n\n\
        - [ ] **Canonical. (2026-09-01)** Already the current shape.\n";
    std::fs::write(root.join(path), source).unwrap();
    stamp(&root, path);

    // All three are typed actions. The two legacy ones carry their prose in
    // `context` and simply have no title, which is absence, not an error.
    let (code, q) = rivet(&root, "query", json!({"collection": "actions"}));
    assert_eq!(code, 0, "{q}");
    let legacy: Vec<&serde_json::Value> = q["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|i| i["path"] == path)
        .collect();
    assert_eq!(legacy.len(), 3, "{q}");
    assert!(
        q["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .all(|d| d["path"] != path),
        "{q}"
    );
    let vendor = legacy
        .iter()
        .find(|i| i["fields"]["actor"] == "agent")
        .unwrap();
    assert!(vendor["fields"].get("title").is_none(), "{vendor}");
    assert_eq!(
        vendor["fields"]["context"],
        "Ask the vendor about pricing before the renewal date."
    );
    assert_eq!(vendor["fields"]["gate"], "a quote in writing.");

    // Completing one touches the checkbox and nothing else: the prose a person
    // wrote is still there, byte for byte.
    let before = read(&root, path);
    let (code, out) = rivet(
        &root,
        "update-item",
        json!({"path": path, "collection": "actions", "id": vendor["id"],
               "guard": vendor["revision"], "set": {"done": true}}),
    );
    assert_eq!(code, 0, "{out}");
    let after = read(&root, path);
    // The write landed: the action reads back closed, under the id it had.
    let (_, q) = rivet(
        &root,
        "query",
        json!({"collection": "actions", "where": [{"field": "id", "op": "eq", "value": vendor["id"]}]}),
    );
    assert_eq!(q["total"], 1, "{q}");
    assert_eq!(q["items"][0]["fields"]["done"], true, "{q}");
    assert_eq!(
        q["items"][0]["fields"]["context"],
        "Ask the vendor about pricing before the renewal date."
    );
    // Stripping the markers to compare prose would also conceal a marker that
    // changed, so account for every one of them first. Each is parsed as
    // (id, attribute, value): the id and attribute sequences must be identical
    // and every value non-empty, which is what stops a digest from being
    // dropped rather than refreshed. Only the values may move, and only for the
    // edited item and its containers, whose contents did change.
    let stays = |s: &str| {
        let mut out: Vec<(String, String, String)> = vec![];
        let mut rest = s;
        while let Some(i) = rest.find("<!-- stay:") {
            let end = i + rest[i..].find("-->").unwrap();
            let body = rest[i + "<!-- stay:".len()..end].trim();
            let (id, attribute) = body.split_once(' ').unwrap_or((body, ""));
            let (attribute, value) = attribute.split_once('=').unwrap_or((attribute, ""));
            out.push((id.into(), attribute.into(), value.into()));
            rest = &rest[end + 3..];
        }
        out
    };
    let (was, now) = (stays(&before), stays(&after));
    let shape = |v: &[(String, String, String)]| {
        v.iter()
            .map(|(i, a, _)| (i.clone(), a.clone()))
            .collect::<Vec<_>>()
    };
    assert_eq!(
        shape(&was),
        shape(&now),
        "a stay was added, dropped, renamed or lost its evidence attribute"
    );
    assert!(
        now.iter().all(|(_, a, v)| !a.is_empty() && !v.is_empty()),
        "a stay carries no evidence: {now:?}"
    );
    let digest_of = |v: &[(String, String, String)], id: &str| {
        v.iter()
            .find(|(i, _, _)| i == id)
            .unwrap_or_else(|| panic!("no stay {id}"))
            .2
            .clone()
    };
    let vendor_id = vendor["id"].as_str().unwrap();
    assert_ne!(
        digest_of(&was, vendor_id),
        digest_of(&now, vendor_id),
        "the edited item's evidence was left stale"
    );
    for sibling in legacy.iter().filter(|i| i["id"] != vendor["id"]) {
        let id = sibling["id"].as_str().unwrap();
        assert_eq!(
            digest_of(&was, id),
            digest_of(&now, id),
            "an untouched item's evidence moved"
        );
    }
    // Only the digests differ, so compare the prose the markers annotate.
    let prose = |s: &str| {
        let mut out = String::new();
        let mut rest = s;
        while let Some(i) = rest.find("<!-- stay:") {
            out.push_str(&rest[..i]);
            rest = &rest[i + rest[i..].find("-->").unwrap() + 3..];
        }
        out + rest
    };
    let reopened = prose(&after).replace("- [x] `agent` Ask", "- [ ] `agent` Ask");
    assert_ne!(
        prose(&after),
        prose(&before),
        "the checkbox never changed, so preservation proves nothing"
    );
    assert_eq!(
        reopened,
        prose(&before),
        "completing a legacy action changed more than its checkbox"
    );

    // A date has nowhere to live without a title, and the refusal says so
    // rather than inventing one.
    let invoice = legacy
        .iter()
        .find(|i| i["fields"]["context"] == "Chase the missing invoice from last month.")
        .unwrap();
    let (code, out) = rivet(
        &root,
        "update-item",
        json!({"path": path, "collection": "actions", "id": invoice["id"],
               "guard": invoice["revision"], "set": {"date": "2026-09-14"}}),
    );
    assert_eq!(code, 2, "{out}");
    assert_eq!(out["error"]["code"], "invalid_item");
    assert!(
        out["error"]["message"]
            .as_str()
            .unwrap()
            .contains("a date lives in the title"),
        "{out}"
    );
    // Clearing a date it never had is a no-op, not that same refusal.
    let (code, out) = rivet(
        &root,
        "update-item",
        json!({"path": path, "collection": "actions", "id": invoice["id"],
               "guard": invoice["revision"], "set": {"date": null}}),
    );
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["status"], "unchanged");

    // Promotion is one write. The title, its date and a trimmed context are
    // applied together even though no title span exists to hang the date on
    // yet, and the result is spaced as if it had been written that way.
    let (code, out) = rivet(
        &root,
        "update-item",
        json!({"path": path, "collection": "actions", "id": invoice["id"],
               "guard": invoice["revision"],
               "set": {"title": "Chase the missing invoice", "date": "2026-09-14",
                       "context": "From last month."}}),
    );
    assert_eq!(code, 0, "{out}");
    assert!(
        read(&root, path)
            .contains("- [ ] **Chase the missing invoice (2026-09-14)** From last month."),
        "{}",
        read(&root, path)
    );
    let (_, q) = rivet(
        &root,
        "query",
        json!({"collection": "actions", "where": [{"field": "id", "op": "eq", "value": invoice["id"]}]}),
    );
    assert_eq!(
        q["items"][0]["fields"]["title"],
        "Chase the missing invoice"
    );
    assert_eq!(q["items"][0]["fields"]["date"], "2026-09-14");

    // Inserting an untitled action obeys the same separator rule. The checkbox
    // and a tag each leave their own trailing space, so the text must not add
    // a second one; a title, which ends flush, still needs it.
    for (fields, line) in [
        (json!({"context": "Untitled task."}), "- [ ] Untitled task."),
        (
            json!({"actor": "agent", "context": "Tagged untitled task."}),
            "- [ ] `agent` Tagged untitled task.",
        ),
        (
            json!({"title": "Titled", "date": "2026-09-14", "context": "With a title."}),
            "- [ ] **Titled (2026-09-14)** With a title.",
        ),
    ] {
        let (_, map) = rivet(&root, "map", json!({"path": path}));
        let (code, out) = rivet(
            &root,
            "insert-item",
            json!({"path": path, "collection": "actions", "position": "end",
                   "guard": map["collections"][0]["revision"], "fields": fields}),
        );
        assert_eq!(code, 0, "{out}");
        let text = read(&root, path);
        assert!(text.contains(&format!("{line} <!-- stay:")), "{text}");
    }

    // The two write paths must render the same logical item identically. An
    // action that gains a title while losing its text keeps a label on the next
    // line, so a stray separator space would be internal and survive trimming:
    // update and insert disagreeing here is a silent byte-level divergence.
    let (_, map) = rivet(&root, "map", json!({"path": path}));
    let (code, out) = rivet(
        &root,
        "insert-item",
        json!({"path": path, "collection": "actions", "position": "end",
               "guard": map["collections"][0]["revision"],
               "fields": {"context": "Becomes titled."}}),
    );
    assert_eq!(code, 0, "{out}");
    let seeded = out["ops"][0]["minted"][0]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let (_, item) = rivet(
        &root,
        "get",
        json!({"path": path, "select": {"kind": "id", "id": seeded}}),
    );
    let (code, out) = rivet(
        &root,
        "update-item",
        json!({"path": path, "collection": "actions", "id": seeded,
               "guard": item["typed"]["actions"]["revision"],
               "set": {"title": "Same", "date": "2026-09-14", "context": null, "gate": "G"}}),
    );
    assert_eq!(code, 0, "{out}");
    let (_, map) = rivet(&root, "map", json!({"path": path}));
    let (code, out) = rivet(
        &root,
        "insert-item",
        json!({"path": path, "collection": "actions", "position": "end",
               "guard": map["collections"][0]["revision"],
               "fields": {"title": "Same", "date": "2026-09-14", "gate": "G"}}),
    );
    assert_eq!(code, 0, "{out}");
    let text = read(&root, path);
    let rendered: Vec<&str> = text
        .lines()
        .filter(|l| l.contains("**Same (2026-09-14)**"))
        .map(|l| l.split(" <!-- stay:").next().unwrap())
        .collect();
    assert_eq!(rendered.len(), 2, "{text}");
    assert_eq!(
        rendered[0], rendered[1],
        "update and insert rendered the same item differently"
    );
    assert_eq!(rendered[0], "- [ ] **Same (2026-09-14)**", "{text}");
}

/// Dropping `required` is a per-project choice, not a change to what the flag
/// means: a project that wants every action titled still gets the diagnostic.
#[test]
fn a_required_title_is_still_enforced() {
    let root = project("required-title");
    let schema = read(&root, ".rivet/schemas/tracker-action.json").replace(
        r#""title": {"type": "string", "from": "title"}"#,
        r#""title": {"type": "string", "from": "title", "required": true}"#,
    );
    assert!(schema.contains("\"required\": true"), "{schema}");
    std::fs::write(root.join(".rivet/schemas/tracker-action.json"), schema).unwrap();
    let path = "tracking/TRACKING_LEGACY.md";
    std::fs::write(
        root.join(path),
        "# Legacy\n\n## Action\n\n- [ ] Chase the missing invoice from last month.\n",
    )
    .unwrap();
    stamp(&root, path);
    let (code, q) = rivet(&root, "query", json!({"collection": "actions"}));
    assert_eq!(code, 0, "{q}");
    let diags = q["diagnostics"].as_array().unwrap();
    assert!(
        diags.iter().any(|d| d["path"] == path
            && d["code"] == "invalid_item"
            && d["message"].as_str().unwrap().contains("title")),
        "{q}"
    );
}

/// A first line that carries no text of its own (a label-only action) is a
/// shape optional titles made reachable: the parser reports the paragraph as
/// starting at the separator space rather than after it, and a title and a text
/// added together both address the same zero-width point.
#[test]
fn actions_whose_first_line_is_empty_can_be_filled_in() {
    let root = project("empty-first-line");
    let path = "tracking/TRACKING_LEGACY.md";
    std::fs::write(root.join(path), "# Legacy\n\n## Action\n\n- [ ] Seed.\n").unwrap();
    stamp(&root, path);
    let insert = |fields: serde_json::Value| {
        let (_, map) = rivet(&root, "map", json!({"path": path}));
        let (code, out) = rivet(
            &root,
            "insert-item",
            json!({"path": path, "collection": "actions", "position": "end",
                   "guard": map["collections"][0]["revision"], "fields": fields}),
        );
        (code, out)
    };
    let update = |id: &serde_json::Value, set: serde_json::Value| {
        let (_, item) = rivet(
            &root,
            "get",
            json!({"path": path, "select": {"kind": "id", "id": id}}),
        );
        rivet(
            &root,
            "update-item",
            json!({"path": path, "collection": "actions", "id": id,
                   "guard": item["typed"]["actions"]["revision"], "set": set}),
        )
    };
    let last = || {
        let (_, q) = rivet(&root, "query", json!({"collection": "actions"}));
        q["items"]
            .as_array()
            .unwrap()
            .iter()
            .rfind(|i| i["path"] == path)
            .unwrap_or_else(|| panic!("{q}"))
            .clone()
    };

    // An action can exist with nothing but a gate, and be filled in afterwards.
    let (code, out) = insert(json!({"gate": "G"}));
    assert_eq!(code, 0, "{out}");
    let (code, out) = update(&last()["id"], json!({"title": "T", "context": "C"}));
    assert_eq!(code, 0, "{out}");
    assert!(
        read(&root, path).contains("- [ ] **T** C\n"),
        "{}",
        read(&root, path)
    );

    // The same, with a tag and a date arriving in that one write too.
    let (code, out) = insert(json!({"gate": "H"}));
    assert_eq!(code, 0, "{out}");
    let (code, out) = update(
        &last()["id"],
        json!({"actor": "agent", "title": "U", "date": "2026-09-14", "context": "D"}),
    );
    assert_eq!(code, 0, "{out}");
    let text = read(&root, path);
    assert!(
        text.contains("- [ ] `agent` **U (2026-09-14)** D\n"),
        "{text}"
    );
    let fields = &last()["fields"];
    assert_eq!(fields["actor"], "agent");
    assert_eq!(fields["title"], "U");
    assert_eq!(fields["date"], "2026-09-14");
    assert_eq!(fields["context"], "D");
    assert_eq!(fields["gate"], "H");
}

/// An actor and a title arriving together land in the same place, and a tag
/// with nothing after it on the line stops being a tag when it is read back.
#[test]
fn a_tag_is_written_only_where_it_reads_back_as_one() {
    let root = project("tag-shapes");
    let path = "tracking/TRACKING_LEGACY.md";
    std::fs::write(
        root.join(path),
        "# Legacy\n\n## Action\n\n- [ ] Untitled task.\n",
    )
    .unwrap();
    stamp(&root, path);
    let (_, q) = rivet(&root, "query", json!({"collection": "actions"}));
    let item = q["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["path"] == path)
        .unwrap()
        .clone();

    // Promotion that also assigns an actor is one write, not two.
    let (code, out) = rivet(
        &root,
        "update-item",
        json!({"path": path, "collection": "actions", "id": item["id"],
               "guard": item["revision"], "set": {"actor": "agent", "title": "T"}}),
    );
    assert_eq!(code, 0, "{out}");
    let text = read(&root, path);
    assert!(
        text.contains("- [ ] `agent` **T** Untitled task."),
        "{text}"
    );
    let (_, q) = rivet(
        &root,
        "query",
        json!({"collection": "actions", "where": [{"field": "id", "op": "eq", "value": item["id"]}]}),
    );
    assert_eq!(q["items"][0]["fields"]["actor"], "agent");
    assert_eq!(q["items"][0]["fields"]["title"], "T");

    // A tag alone on the line would read back as context, so it is refused by
    // name rather than by the re-read comparison noticing the fields moved.
    let (_, map) = rivet(&root, "map", json!({"path": path}));
    let (code, out) = rivet(
        &root,
        "insert-item",
        json!({"path": path, "collection": "actions", "position": "end",
               "guard": map["collections"][0]["revision"],
               "fields": {"actor": "agent", "gate": "G"}}),
    );
    assert_eq!(code, 2, "{out}");
    assert!(
        out["error"]["message"]
            .as_str()
            .unwrap()
            .contains("a tag needs a title, text or label on the same line"),
        "{out}"
    );
    // With a title it is fine.
    let (code, out) = rivet(
        &root,
        "insert-item",
        json!({"path": path, "collection": "actions", "position": "end",
               "guard": map["collections"][0]["revision"],
               "fields": {"actor": "agent", "title": "Titled", "gate": "G"}}),
    );
    assert_eq!(code, 0, "{out}");
    assert!(
        read(&root, path).contains("- [ ] `agent` **Titled**"),
        "{}",
        read(&root, path)
    );
}

/// A title belongs on the item's own first line whatever that line already
/// holds. The paragraph can begin on a continuation line, so the insertion
/// addresses the checkbox rather than the paragraph, and the separator after
/// the title is written only when text follows it on that same line.
#[test]
fn a_title_lands_on_the_first_line_of_every_item_shape() {
    let cases = [
        (
            "- [ ] Text here.",
            json!({"title": "T"}),
            "- [ ] **T** Text here.",
        ),
        (
            "- [ ]\n  On line two.",
            json!({"title": "T"}),
            "- [ ] **T**",
        ),
        (
            "- [ ] \n      **Gate**: G",
            json!({"title": "T"}),
            "- [ ] **T**",
        ),
        (
            "- [ ] \n      **Gate**: G",
            json!({"title": "T", "context": "C"}),
            "- [ ] **T** C",
        ),
        (
            "- [ ] `agent` Tagged text.",
            json!({"title": "T"}),
            "- [ ] `agent` **T** Tagged text.",
        ),
        (
            "- [ ] Text here.",
            json!({"actor": "agent", "title": "T"}),
            "- [ ] `agent` **T** Text here.",
        ),
        (
            "- [ ] Text here.",
            json!({"title": "T", "context": null}),
            "- [ ] **T**",
        ),
        (
            "- [ ] Wrapped text\n      more.",
            json!({"title": "T"}),
            "- [ ] **T** Wrapped text",
        ),
    ];
    let path = "tracking/TRACKING_SHAPES.md";
    for (n, (item, set, want)) in cases.iter().enumerate() {
        let root = project(&format!("shape-{n}"));
        std::fs::write(
            root.join(path),
            format!("# Shapes\n\n## Action\n\n{item}\n"),
        )
        .unwrap();
        stamp(&root, path);
        let (_, q) = rivet(&root, "query", json!({"collection": "actions"}));
        let subject = q["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["path"] == path)
            .unwrap_or_else(|| panic!("case {n}: {q}"))
            .clone();
        let (code, out) = rivet(
            &root,
            "update-item",
            json!({"path": path, "collection": "actions", "id": subject["id"],
                   "guard": subject["revision"], "set": set}),
        );
        assert_eq!(code, 0, "case {n}: {out}");
        let text = read(&root, path);
        let line = text
            .lines()
            .find(|l| l.starts_with("- ["))
            .unwrap_or_else(|| panic!("case {n}: {text}"))
            .split(" <!-- stay:")
            .next()
            .unwrap()
            .to_string();
        assert_eq!(&line, want, "case {n} rendered wrongly:\n{text}");
        // Whatever it rendered, the title must read back as a title.
        let (_, q) = rivet(
            &root,
            "query",
            json!({"collection": "actions", "where": [{"field": "id", "op": "eq", "value": subject["id"]}]}),
        );
        assert_eq!(q["items"][0]["fields"]["title"], "T", "case {n}: {q}");
    }
}

/// Demotion is the mirror of promotion, and optional titles made it reachable:
/// a date lives inside the title envelope, so removing the title takes the date
/// with it whether or not the request said so.
#[test]
fn a_removed_title_takes_its_date_with_it() {
    let root = project("demote");
    let path = "tracking/TRACKING_DEMOTE.md";
    std::fs::write(
        root.join(path),
        "# Demote\n\n## Action\n\n\
         - [ ] **Ship it. (2026-09-01)** Context here.\n\n\
         - [ ] **Ship two. (2026-09-02)** More context.\n\n\
         - [ ] `agent` **Tagged. (2026-09-03)** Tag context.\n\n\
         - [ ] **Nothing else. (2026-09-04)**\n",
    )
    .unwrap();
    stamp(&root, path);
    let subjects = |root: &std::path::Path| {
        let (_, q) = rivet(root, "query", json!({"collection": "actions"}));
        q["items"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|i| i["path"] == path)
            .cloned()
            .collect::<Vec<_>>()
    };
    let items = subjects(&root);
    assert_eq!(items.len(), 4, "{items:?}");
    let demote = |item: &serde_json::Value, set: serde_json::Value| {
        let (_, got) = rivet(
            &root,
            "get",
            json!({"path": path, "select": {"kind": "id", "id": item["id"]}}),
        );
        rivet(
            &root,
            "update-item",
            json!({"path": path, "collection": "actions", "id": item["id"],
                   "guard": got["typed"]["actions"]["revision"], "set": set}),
        )
    };

    // Saying only `title: null` is enough; the date goes with the envelope.
    let (code, out) = demote(&items[0], json!({"title": null}));
    assert_eq!(code, 0, "{out}");
    // Saying both is the same edit, not two overlapping ones.
    let (code, out) = demote(&items[1], json!({"title": null, "date": null}));
    assert_eq!(code, 0, "{out}");
    // A tag in front of the title is untouched by its removal.
    let (code, out) = demote(&items[2], json!({"title": null}));
    assert_eq!(code, 0, "{out}");

    let text = read(&root, path);
    for want in [
        "- [ ] Context here.",
        "- [ ] More context.",
        "- [ ] `agent` Tag context.",
    ] {
        assert!(text.contains(&format!("{want} <!-- stay:")), "{text}");
    }
    for item in subjects(&root).iter().take(3) {
        assert!(item["fields"].get("title").is_none(), "{item}");
        assert!(item["fields"].get("date").is_none(), "{item}");
    }

    // An item with nothing but a title cannot lose it: the stay would have no
    // paragraph left to hang on, so the write is refused and nothing moves.
    let before = read(&root, path);
    let (code, out) = demote(&items[3], json!({"title": null}));
    assert_eq!(code, 2, "{out}");
    assert_eq!(out["error"]["code"], "invalid_content", "{out}");
    assert!(
        out["error"]["message"]
            .as_str()
            .unwrap()
            .contains("no paragraph to carry the item's stay"),
        "{out}"
    );
    assert_eq!(read(&root, path), before);
}

/// The tag, the title and the date inside it share one line and one set of
/// separators, so they are rewritten as a single splice. These are the
/// combinations that collided when each field planned its own edit.
#[test]
fn fields_sharing_the_first_line_are_edited_together() {
    let cases: [(&str, serde_json::Value, &str); 16] = [
        // An actor arriving alongside a title the item already has.
        (
            "- [ ] **Old** C",
            json!({"actor": "agent", "title": "T"}),
            "- [ ] `agent` **T** C",
        ),
        // A wider gap after the checkbox than the one byte it usually is.
        (
            "- [ ]  Old.",
            json!({"title": "T", "context": "C"}),
            "- [ ] **T** C",
        ),
        // A first line holding nothing at all, with and without its space.
        (
            "- [ ]\n      **Gate**: G",
            json!({"title": "T", "context": "C"}),
            "- [ ] **T** C",
        ),
        (
            "- [ ] \n      **Gate**: G",
            json!({"actor": "agent", "context": "C"}),
            "- [ ] `agent` C",
        ),
        (
            "- [ ] \n      **Gate**: G",
            json!({"title": "T"}),
            "- [ ] **T**",
        ),
        // A title leaving while the text it sat in front of is replaced.
        (
            "- [ ] **T** Old.",
            json!({"title": null, "context": "C"}),
            "- [ ] C",
        ),
        // A date only exists inside a title, in both directions.
        (
            "- [ ] **Ship. (2026-09-01)** C",
            json!({"title": null}),
            "- [ ] C",
        ),
        (
            "- [ ] **Ship. (2026-09-01)** C",
            json!({"title": null, "date": null}),
            "- [ ] C",
        ),
        (
            "- [ ] Text.",
            json!({"title": "T", "date": "2026-09-14"}),
            "- [ ] **T (2026-09-14)** Text.",
        ),
        (
            "- [ ] **A (2026-01-01)** C",
            json!({"date": "2026-09-14"}),
            "- [ ] **A (2026-09-14)** C",
        ),
        // Text already on a continuation line is left on it.
        (
            "- [ ]\n  On line two.",
            json!({"title": "T"}),
            "- [ ] **T**",
        ),
        // A request that touches no first-line field rebuilds nothing.
        ("- [ ] **A** C", json!({"context": "D"}), "- [ ] **A** D"),
        // A part the request did not name keeps its own source bytes rather
        // than being re-rendered into the canonical spelling.
        (
            "- [ ] __T__ C",
            json!({"actor": "agent"}),
            "- [ ] `agent` __T__ C",
        ),
        // A title spanning two lines is replaced whole, not left half behind.
        (
            "- [ ] **Long\n      title** C",
            json!({"title": "U"}),
            "- [ ] **U** C",
        ),
        // Text with no span of its own goes onto the first line, which also
        // keeps it clear of the position a new label claims.
        (
            "- [ ]\n      **Gate**: G",
            json!({"context": "C"}),
            "- [ ] C",
        ),
        (
            "- [ ] **T**",
            json!({"title": "U", "context": "C", "gate": "G"}),
            "- [ ] **U** C",
        ),
    ];
    let path = "tracking/TRACKING_HEAD.md";
    for (n, (item, set, want)) in cases.iter().enumerate() {
        let root = project(&format!("head-{n}"));
        std::fs::write(root.join(path), format!("# Head\n\n## Action\n\n{item}\n")).unwrap();
        stamp(&root, path);
        let (_, q) = rivet(&root, "query", json!({"collection": "actions"}));
        let subject = q["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["path"] == path)
            .unwrap_or_else(|| panic!("case {n}: {q}"))
            .clone();
        let (code, out) = rivet(
            &root,
            "update-item",
            json!({"path": path, "collection": "actions", "id": subject["id"],
                   "guard": subject["revision"], "set": set}),
        );
        assert_eq!(code, 0, "case {n} refused: {out}");
        let text = read(&root, path);
        let line = text
            .lines()
            .find(|l| l.starts_with("- ["))
            .unwrap_or_else(|| panic!("case {n}: {text}"))
            .split(" <!-- stay:")
            .next()
            .unwrap()
            .to_string();
        assert_eq!(&line, want, "case {n} rendered wrongly:\n{text}");
        // The fields the request named are the fields it got.
        let (_, q) = rivet(
            &root,
            "query",
            json!({"collection": "actions", "where": [{"field": "id", "op": "eq", "value": subject["id"]}]}),
        );
        for (name, value) in set.as_object().unwrap() {
            match value {
                serde_json::Value::Null => {
                    assert!(q["items"][0]["fields"].get(name).is_none(), "case {n}: {q}")
                }
                v => assert_eq!(&q["items"][0]["fields"][name], v, "case {n}: {q}"),
            }
        }
    }
}

/// Markdown that means something by its whitespace, and spellings the parser
/// accepts but would not have chosen, survive an edit to a different field.
/// Verification cannot catch these on its own: extraction trims the difference
/// away, so a lost hard break reads back as a clean success.
#[test]
fn an_edit_leaves_the_bytes_it_was_not_asked_about() {
    let path = "tracking/TRACKING_BYTES.md";
    let cases: [(&str, serde_json::Value, &str); 6] = [
        // Two trailing spaces are a hard line break, not padding.
        (
            "- [ ] **T**  \n      C",
            json!({"title": "U"}),
            "- [ ] **U**  ",
        ),
        // An unusual gap is the author's, and no field named it.
        ("- [ ] **A**  C", json!({"done": true}), "- [x] **A**  C"),
        (
            "- [ ]   Wide gap.",
            json!({"done": true}),
            "- [x]   Wide gap.",
        ),
        // Underscores are a strong emphasis this parser reads and keeps.
        ("- [ ] __T__ C", json!({"done": true}), "- [x] __T__ C"),
        // A wide, authored gap between an untouched tag and the title the
        // request is actually changing is not this request's to respell.
        (
            "- [ ] `agent`   **T** C",
            json!({"title": "U"}),
            "- [ ] `agent`   **U** C",
        ),
        // Setting a field to the value it already reads back as is not a
        // change, even when that field is routed through the head splice: no
        // rebuild should fire, so an unrelated wide gap survives.
        (
            "- [ ]   Wide gap.",
            json!({"actor": "human:owner"}),
            "- [ ]   Wide gap.",
        ),
    ];
    for (n, (item, set, want)) in cases.iter().enumerate() {
        let root = project(&format!("bytes-{n}"));
        std::fs::write(root.join(path), format!("# Bytes\n\n## Action\n\n{item}\n")).unwrap();
        stamp(&root, path);
        let (_, q) = rivet(&root, "query", json!({"collection": "actions"}));
        let subject = q["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["path"] == path)
            .unwrap_or_else(|| panic!("case {n}: {q}"))
            .clone();
        let (code, out) = rivet(
            &root,
            "update-item",
            json!({"path": path, "collection": "actions", "id": subject["id"],
                   "guard": subject["revision"], "set": set}),
        );
        assert_eq!(code, 0, "case {n}: {out}");
        let text = read(&root, path);
        let line = text
            .lines()
            .find(|l| l.starts_with("- ["))
            .unwrap_or_else(|| panic!("case {n}: {text}"))
            .split(" <!-- stay:")
            .next()
            .unwrap()
            .to_string();
        assert_eq!(
            &line, want,
            "case {n} moved bytes it was not asked about:\n{text}"
        );
    }
}

/// Setting fields to the values they already hold writes nothing at all. A
/// separator quietly added on each such write would never fail verification,
/// because the fields always read back correct.
#[test]
fn writing_the_same_values_back_changes_no_bytes() {
    let path = "tracking/TRACKING_IDEMPOTENT.md";
    let items = [
        "- [ ] **T** C",
        "- [ ] **T**\n  On line two.",
        "- [ ] **T**  \n      C",
        "- [ ] `agent` **T (2026-09-01)** C",
        "- [ ] Untitled context only.",
        "- [ ] __T__ C",
    ];
    for (n, item) in items.iter().enumerate() {
        let root = project(&format!("idem-{n}"));
        std::fs::write(root.join(path), format!("# Idem\n\n## Action\n\n{item}\n")).unwrap();
        stamp(&root, path);
        let before = read(&root, path);
        let (_, q) = rivet(&root, "query", json!({"collection": "actions"}));
        let subject = q["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["path"] == path)
            .unwrap_or_else(|| panic!("case {n}: {q}"))
            .clone();
        let (code, out) = rivet(
            &root,
            "update-item",
            json!({"path": path, "collection": "actions", "id": subject["id"],
                   "guard": subject["revision"], "set": subject["fields"]}),
        );
        assert_eq!(code, 0, "case {n}: {out}");
        assert_eq!(out["status"], "unchanged", "case {n}: {out}");
        assert_eq!(read(&root, path), before, "case {n} rewrote the item");
    }
}

#[test]
fn wrapped_titles_round_trip_and_survive_neighboring_edits() {
    let path = "tracking/TRACKING_WRAPPED.md";
    for (n, item) in [
        "- [ ] **Long\n      title** C",
        "- [ ] `agent`   __Long\n        title (2026-09-01)__  \n      C",
        "- [ ] **Long  \n      title** C",
        "- [ ] **Title with `**` inside** C",
    ]
    .iter()
    .enumerate()
    {
        for newline in ["\n", "\r\n"] {
            let root = project(&format!("wrapped-title-{n}"));
            std::fs::write(root.join(path), format!(
                "# Wrapped\n\nUntouched introduction.\n\n## Action\n\n{item}\n\n## Other\n\nUntouched ending.\n"
            ).replace('\n', newline)).unwrap();
            stamp(&root, path);
            let subject = || {
                let (code, q) = rivet(&root, "query", json!({"collection": "actions"}));
                assert_eq!(code, 0, "{q}");
                q["items"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|i| i["path"] == path)
                    .unwrap()
                    .clone()
            };
            let original = subject();
            let before = read(&root, path);
            let (code, out) = rivet(
                &root,
                "update-item",
                json!({
                    "path": path, "collection": "actions", "id": original["id"],
                    "guard": original["revision"], "set": original["fields"]
                }),
            );
            assert_eq!(code, 0, "{item}: {out}");
            assert_eq!(out["status"], "unchanged", "{out}");
            assert_eq!(read(&root, path), before);

            // A changed neighbor must leave the title and every authored gap alone.
            let actor = if n == 1 { "human:lead" } else { "agent" };
            let (code, out) = rivet(
                &root,
                "update-item",
                json!({
                    "path": path, "collection": "actions", "id": original["id"],
                    "guard": original["revision"], "set": {"actor": actor, "title": original["fields"]["title"]}
                }),
            );
            assert_eq!(code, 0, "{item}: {out}");
            let expected = if n == 1 {
                before.replacen("`agent`", "`human:lead`", 1)
            } else {
                before.replacen("- [ ] ", "- [ ] `agent` ", 1)
            };
            assert_eq!(undigest(&read(&root, path)), undigest(&expected));
            let updated = subject();
            assert_eq!(updated["id"], original["id"]);
            assert_eq!(updated["fields"]["title"], original["fields"]["title"]);
            assert_eq!(updated["fields"]["context"], original["fields"]["context"]);

            // New wrapped values still fail explicitly and publish no bytes.
            let before_refusal = read(&root, path);
            let (code, out) = rivet(
                &root,
                "update-item",
                json!({
                    "path": path, "collection": "actions", "id": updated["id"],
                    "guard": updated["revision"], "set": {"title": "Different\n      title"}
                }),
            );
            assert_ne!(code, 0, "{out}");
            assert_eq!(out["error"]["code"], "invalid_item", "{out}");
            assert_eq!(read(&root, path), before_refusal);
            assert_eq!(subject()["id"], original["id"]);
        }
    }
}

/// Text carrying its own indentation keeps it across writes. Extraction strips
/// the continuation lines' common indent, so rendering must add back that
/// common indent and no more. Taking the first continuation line's full
/// indentation instead grows a nested line by the difference on every write
/// that renders the text, and puts a new label at the nested depth rather than
/// the item's. A pure write-back never renders the text (it is skipped as a
/// no-op), so each round changes the text and a label for real, then checks
/// that writing the read-back fields is still a fixed point.
#[test]
fn indented_continuation_text_keeps_its_depth() {
    let path = "tracking/TRACKING_INDENT.md";
    let root = project("indent-depth");
    std::fs::write(root.join(path), "# Indent\n\n## Action\n\n- [ ] **Seed**\n").unwrap();
    stamp(&root, path);
    let (_, map) = rivet(&root, "map", json!({"path": path}));
    let (code, out) = rivet(
        &root,
        "insert-item",
        json!({"path": path, "collection": "actions", "position": "end",
               "guard": map["collections"][0]["revision"],
               "fields": {"title": "T", "context": "Top\n  Indented\nBase"}}),
    );
    assert_eq!(code, 0, "{out}");
    let id = out["ops"][0]["minted"][0]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let subject = |root: &std::path::Path| {
        let (_, q) = rivet(root, "query", json!({"collection": "actions"}));
        q["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["id"] == id)
            .unwrap_or_else(|| panic!("{q}"))
            .clone()
    };
    for round in 0..3 {
        let context = format!("Top\n  Indented\nBase {round}");
        let gate = format!("G{round}");
        let current = subject(&root);
        let (code, out) = rivet(
            &root,
            "update-item",
            json!({"path": path, "collection": "actions", "id": id,
                   "guard": current["revision"], "set": {"context": context, "gate": gate}}),
        );
        assert_eq!(code, 0, "round {round}: {out}");
        assert_eq!(out["status"], "applied", "round {round}: {out}");
        let text = read(&root, path);
        let expected = format!(
            "- [ ] **T** Top\n        Indented\n      Base {round}\n      **Gate**: G{round}"
        );
        assert!(text.contains(&expected), "round {round}:\n{text}");
        let back = subject(&root);
        assert_eq!(back["fields"]["context"], context.as_str(), "round {round}");
        assert_eq!(back["fields"]["gate"], gate.as_str(), "round {round}");
        let (code, out) = rivet(
            &root,
            "update-item",
            json!({"path": path, "collection": "actions", "id": id,
                   "guard": back["revision"], "set": back["fields"]}),
        );
        assert_eq!(code, 0, "round {round}: {out}");
        assert_eq!(out["status"], "unchanged", "round {round}: {out}");
        assert_eq!(read(&root, path), text, "round {round}: write-back rewrote");
    }
}

/// Text and labels keep their own depths. When every text continuation line
/// sits deeper than the item's labels, a write that re-renders the text keeps
/// the text at its depth rather than pulling it up to the labels'. With no
/// labels, a new one lands at the text's depth. A line the request did not
/// change must keep its bytes.
#[test]
fn text_deeper_than_its_labels_keeps_its_depth() {
    let path = "tracking/TRACKING_DEPTHS.md";
    let cases = [
        (
            "- [ ] **T** Top\n        Deep1\n        Deep2\n      **Gate**: G0",
            "- [ ] **T** Top\n        Deep1\n        Deep2 X\n      **Gate**: G0",
        ),
        (
            "- [ ] **T** Top\n    Deep1\n    Deep2\n  **Gate**: G0",
            "- [ ] **T** Top\n    Deep1\n    Deep2 X\n  **Gate**: G0",
        ),
        (
            "- [ ]\n    Deep1\n    Deep2",
            "- [ ]\n    Deep1\n    Deep2 X\n    **Gate**: G",
        ),
        ("- [ ]\n    Deep1", "- [ ]\n    Deep1 X\n    **Gate**: G"),
    ];
    for (n, (item, expected)) in cases.iter().enumerate() {
        let root = project(&format!("depths-{n}"));
        std::fs::write(
            root.join(path),
            format!("# Depths\n\n## Action\n\n{item}\n"),
        )
        .unwrap();
        stamp(&root, path);
        let (_, q) = rivet(&root, "query", json!({"collection": "actions"}));
        let subject = q["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["path"] == path)
            .unwrap_or_else(|| panic!("case {n}: {q}"))
            .clone();
        let context = subject["fields"]["context"].as_str().unwrap().to_string();
        assert!(
            context.ends_with("Deep2") || context == "Deep1",
            "case {n}: {context:?}"
        );
        let (code, out) = rivet(
            &root,
            "update-item",
            json!({"path": path, "collection": "actions", "id": subject["id"],
                   "guard": subject["revision"],
                   "set": {"context": format!("{context} X"), "gate": subject["fields"]["gate"].as_str().unwrap_or("G")}}),
        );
        assert_eq!(code, 0, "case {n}: {out}");
        let text = read(&root, path);
        let body: String = text
            .lines()
            .skip_while(|l| !l.starts_with("- ["))
            .map(|l| l.split(" <!-- stay:").next().unwrap())
            .filter(|l| !l.starts_with("<!--"))
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(body.trim_end(), *expected, "case {n}:\n{text}");
    }
}

/// A continuation line the write does not change keeps its own indentation,
/// even where that is not the item's width: a lazy line at column 0, one
/// space short of it, or a tab. The item path strips exactly the width and
/// used to rebuild every line from spaces, which moved these bytes on a
/// request that never named them while still reading back correctly. The
/// middle case changes the lines on both sides of the lazy one, so keeping it
/// cannot depend on it sitting in an unchanged run at either end.
#[test]
fn unchanged_continuation_lines_keep_their_own_indentation() {
    let path = "tracking/TRACKING_LAZY.md";
    let cases = [
        (
            "- [ ] **T** Top\n    Deep\n**Gate**: G0",
            json!({"done": true}),
            "- [x] **T** Top\n    Deep\n**Gate**: G0",
        ),
        (
            "- [ ] **T** Top\n    Deep\n**Gate**: G0",
            json!({"context": "Top\nDeep X"}),
            "- [ ] **T** Top\n    Deep X\n**Gate**: G0",
        ),
        (
            "-\t[ ] **T** Top\n\t**Gate**: G",
            json!({"done": true}),
            "-\t[x] **T** Top\n\t**Gate**: G",
        ),
        (
            "- [ ] **T** Top\n **Gate**: G0",
            json!({"context": "Top X"}),
            "- [ ] **T** Top X\n **Gate**: G0",
        ),
        (
            "- [ ] **T** Top\n  More\nLazy middle\n  **Gate**: G0",
            json!({"title": "U", "context": "Top\nMore X\nLazy middle", "gate": "G1"}),
            "- [ ] **U** Top\n  More X\nLazy middle\n  **Gate**: G1",
        ),
    ];
    for (n, (item, set, expected)) in cases.iter().enumerate() {
        let root = project(&format!("lazy-{n}"));
        std::fs::write(root.join(path), format!("# Lazy\n\n## Action\n\n{item}\n")).unwrap();
        stamp(&root, path);
        let (_, q) = rivet(&root, "query", json!({"collection": "actions"}));
        let subject = q["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["path"] == path)
            .unwrap_or_else(|| panic!("case {n}: {q}"))
            .clone();
        let (code, out) = rivet(
            &root,
            "update-item",
            json!({"path": path, "collection": "actions", "id": subject["id"],
                   "guard": subject["revision"], "set": set}),
        );
        assert_eq!(code, 0, "case {n}: {out}");
        assert_eq!(out["status"], "applied", "case {n}: {out}");
        let text = read(&root, path);
        let body: String = text
            .lines()
            .skip_while(|l| !l.starts_with('-'))
            .map(|l| l.split(" <!-- stay:").next().unwrap())
            .filter(|l| !l.starts_with("<!--"))
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(body.trim_end(), *expected, "case {n}:\n{text}");
    }
}

/// Clearing continuation text while adding a label must not leave the text's
/// own leading line break behind: a dangling one pushes the label's own
/// leading newline into a blank line, which comrak reads as a new paragraph,
/// which puts the label outside the item (or, on a checkbox-only first line,
/// makes the label read as the title) and refuses the update. An untitled
/// item's checkbox-only first line is covered alongside the titled one, since
/// the point that swallows the gap (`gap_start`) differs for each.
#[test]
fn clearing_text_and_adding_a_label_stay_in_one_paragraph() {
    let path = "tracking/TRACKING_CLEAR.md";
    let cases = ["- [ ] **T**\n  On line two.", "- [ ]\n  On line two."];
    for (n, item) in cases.iter().enumerate() {
        let root = project(&format!("clear-and-label-{n}"));
        std::fs::write(root.join(path), format!("# Clear\n\n## Action\n\n{item}\n")).unwrap();
        stamp(&root, path);
        let (_, q) = rivet(&root, "query", json!({"collection": "actions"}));
        let subject = q["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["path"] == path)
            .unwrap_or_else(|| panic!("case {n}: {q}"));
        let (code, out) = rivet(
            &root,
            "update-item",
            json!({"path": path, "collection": "actions", "id": subject["id"],
                   "guard": subject["revision"], "set": {"context": null, "gate": "G"}}),
        );
        assert_eq!(code, 0, "case {n}: {out}");
        assert_eq!(out["status"], "applied", "case {n}: {out}");
        let text = read(&root, path);
        assert!(
            !text.contains("\n\n      **Gate**") && !text.contains("\n\n**Gate**"),
            "case {n}: a blank line separates the label from the item:\n{text}"
        );
        let (_, q2) = rivet(&root, "query", json!({"collection": "actions"}));
        let refound = q2["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["path"] == path)
            .unwrap_or_else(|| panic!("case {n}: {q2}"));
        assert_eq!(
            refound["fields"]["gate"], "G",
            "case {n}: label did not read back inside the item: {text}"
        );
        assert!(
            refound["fields"].get("context").is_none(),
            "case {n}: context should read as absent: {text}"
        );
    }
}

/// Setting an already-absent text field to null again is a no-op, even when
/// the item's paragraph carries a hard break the removal path would
/// otherwise reach into: nothing was actually removed, so nothing but the
/// requested field may move. A non-breaking space is the discriminating
/// case: extraction trims it away so the field still reads as absent, but a
/// byte-level ASCII-only whitespace walk does not recognize it as skippable,
/// which is exactly the gap a careless "clear reaches back to the gap" fix
/// would fall into.
#[test]
fn clearing_an_already_absent_field_keeps_the_hard_break() {
    let path = "tracking/TRACKING_NOOP_CLEAR.md";
    let items = [
        "- [ ] **T**  \n      **Gate**: G",
        "- [ ] **T**\u{a0} \n      **Gate**: G",
    ];
    for (n, item) in items.iter().enumerate() {
        let root = project(&format!("noop-clear-hard-break-{n}"));
        std::fs::write(root.join(path), format!("# Noop\n\n## Action\n\n{item}\n")).unwrap();
        stamp(&root, path);
        let before = read(&root, path);
        let (_, q) = rivet(&root, "query", json!({"collection": "actions"}));
        let subject = q["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["path"] == path)
            .unwrap_or_else(|| panic!("case {n}: {q}"));
        assert!(
            subject["fields"].get("context").is_none(),
            "case {n}: {subject}"
        );
        let (code, out) = rivet(
            &root,
            "update-item",
            json!({"path": path, "collection": "actions", "id": subject["id"],
                   "guard": subject["revision"], "set": {"context": null}}),
        );
        assert_eq!(code, 0, "case {n}: {out}");
        assert_eq!(out["status"], "unchanged", "case {n}: {out}");
        assert_eq!(
            read(&root, path),
            before,
            "case {n}: clearing an absent field rewrote the item"
        );
    }
}

/// A tag needs a title or text to read back as one, whether it arrives on
/// insert or is added by an update to a bare item.
#[test]
fn a_tag_added_by_update_needs_a_title_or_text() {
    let path = "tracking/TRACKING_BARE.md";
    let root = project("bare-tag-update");
    std::fs::write(
        root.join(path),
        "# Bare\n\n## Action\n\n- [ ] Untitled context only.\n",
    )
    .unwrap();
    stamp(&root, path);
    let before = read(&root, path);
    let (_, q) = rivet(&root, "query", json!({"collection": "actions"}));
    let subject = q["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["path"] == path)
        .unwrap_or_else(|| panic!("{q}"));
    let (code, out) = rivet(
        &root,
        "update-item",
        json!({"path": path, "collection": "actions", "id": subject["id"],
               "guard": subject["revision"], "set": {"actor": "agent", "context": null}}),
    );
    assert_eq!(code, 2, "{out}");
    assert_eq!(out["error"]["code"], "invalid_item", "{out}");
    assert!(
        out["error"]["message"]
            .as_str()
            .unwrap()
            .contains("a tag needs a title, text or label"),
        "{out}"
    );
    assert_eq!(
        read(&root, path),
        before,
        "a refused update touched the file"
    );
}

/// An item can hold more than the line its fields live on. Rewriting that line
/// must not reach past it, respell what it did not name, or drop whitespace
/// that carries meaning. Each of these was a real defect: the nested list was
/// deleted outright, and verification accepted it because a child list
/// contributes no extracted field.
#[test]
fn rewriting_the_field_line_leaves_the_rest_of_the_item_alone() {
    let path = "tracking/TRACKING_AROUND.md";
    // (item, request, the whole item afterwards, stay markers stripped)
    let cases: [(&str, serde_json::Value, &str); 6] = [
        // A nested list before the paragraph is not the head's to rewrite.
        (
            "- [ ]\n  - child\n\n  **T** C",
            json!({"title": "U"}),
            "- [ ]\n  - child\n\n  **U** C",
        ),
        // Two trailing spaces are a hard break even with no text after them.
        (
            "- [ ] **T**  \n      **Gate**: G",
            json!({"title": "U"}),
            "- [ ] **U**  \n      **Gate**: G",
        ),
        // A gap between tag and title is the author's too.
        (
            "- [ ] `agent`   **T** C",
            json!({"title": "T"}),
            "- [ ] `agent`   **T** C",
        ),
        // Editing the date must not respell the title around it.
        (
            "- [ ] __T__ C",
            json!({"date": "2026-09-14"}),
            "- [ ] __T (2026-09-14)__ C",
        ),
        // Content stays one space clear of the checkbox, whatever it replaced.
        ("- [ ] **T**C", json!({"title": null}), "- [ ] C"),
        // A label leaving takes its own line break, not the head's.
        (
            "- [ ]\n      **Gate**: G",
            json!({"title": "T", "gate": null}),
            "- [ ] **T**",
        ),
    ];
    for (n, (item, set, want)) in cases.iter().enumerate() {
        let root = project(&format!("around-{n}"));
        std::fs::write(
            root.join(path),
            format!("# Around\n\n## Action\n\n{item}\n"),
        )
        .unwrap();
        stamp(&root, path);
        let (_, q) = rivet(&root, "query", json!({"collection": "actions"}));
        let subject = q["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["path"] == path)
            .unwrap_or_else(|| panic!("case {n}: {q}"))
            .clone();
        let (code, out) = rivet(
            &root,
            "update-item",
            json!({"path": path, "collection": "actions", "id": subject["id"],
                   "guard": subject["revision"], "set": set}),
        );
        assert_eq!(code, 0, "case {n} refused: {out}");
        // Compare the item as written, with only the markers taken out.
        let text = read(&root, path);
        let body = text
            .split("## Action\n")
            .nth(1)
            .unwrap_or_else(|| panic!("case {n}: {text}"));
        let got: String = body
            .lines()
            .filter(|l| !l.trim_start().starts_with("<!-- stay:"))
            .map(|l| l.split(" <!-- stay:").next().unwrap())
            .collect::<Vec<_>>()
            .join("\n")
            .trim_matches('\n')
            .to_string();
        assert_eq!(&got, want, "case {n} changed more than its field line");
    }
}

/// Structured provenance frontmatter reads through `map` and `get` as JSON,
/// refuses a write with nothing changed on disk, and does not stop a write to
/// a scalar beside it, which leaves the structured bytes exactly as they were.
#[test]
fn structured_frontmatter_reads_and_refuses_writes() {
    let path = "tracking/TRACKING_PROVENANCE.md";
    let root = project("structured-frontmatter");
    let head = "---\nstatus: active\ngenerated:\n  by: human:owner   # who\n  at: '2026-09-01'\nverified:\n  - {by: 'process:refresh', at: '2026-09-02'}\nsources:\n- id: notes\n  resource: a/b.md\n---\n";
    std::fs::write(root.join(path), format!("{head}\n# Provenance\n\nBody.\n")).unwrap();
    let (code, map) = rivet(&root, "map", json!({"path": path}));
    assert_eq!(code, 0, "{map}");
    assert_eq!(map["frontmatter"]["status"], "ok", "{map}");
    assert_eq!(
        map["frontmatter"]["keys"],
        json!(["status", "generated", "verified", "sources"])
    );
    assert_eq!(map["frontmatter"]["writable"], json!(["status"]));
    let key = |k: &str| json!({"kind": "frontmatter_key", "key": k});
    let (code, got) = rivet(
        &root,
        "get",
        json!({"path": path, "select": key("generated")}),
    );
    assert_eq!(code, 0, "{got}");
    assert_eq!(
        got["value"],
        json!({"by": "human:owner", "at": "2026-09-01"})
    );
    let (_, got) = rivet(
        &root,
        "get",
        json!({"path": path, "select": key("sources")}),
    );
    assert_eq!(got["value"], json!([{"id": "notes", "resource": "a/b.md"}]));
    let before = read(&root, path);
    let (_, g) = rivet(
        &root,
        "get",
        json!({"path": path, "select": key("generated")}),
    );
    let (code, out) = rivet(
        &root,
        "set",
        json!({"path": path, "select": key("generated"), "guard": g["revision"],
               "content": {"by": "human:owner", "at": "2026-09-03"}}),
    );
    assert_eq!(code, 2, "{out}");
    assert_eq!(out["error"]["code"], "unsupported_syntax");
    assert_eq!(
        read(&root, path),
        before,
        "a refused write changed the file"
    );
    let (_, s) = rivet(&root, "get", json!({"path": path, "select": key("status")}));
    let (code, out) = rivet(
        &root,
        "set",
        json!({"path": path, "select": key("status"), "guard": s["revision"], "content": "complete"}),
    );
    assert_eq!(code, 0, "{out}");
    assert_eq!(
        read(&root, path),
        before.replacen("status: active", "status: complete", 1)
    );
    // A duplicate key leaves frontmatter unreadable, names the key, and still
    // lets the body be read.
    let dup = "tracking/TRACKING_DUPLICATE.md";
    std::fs::write(
        root.join(dup),
        "---\nrevised: 2026-01-01\nrevised: 2026-02-01\n---\n\n# Dup\n\nBody.\n",
    )
    .unwrap();
    let (code, map) = rivet(&root, "map", json!({"path": dup}));
    assert_eq!(code, 0, "{map}");
    assert_eq!(map["frontmatter"]["status"], "unsupported");
    assert!(
        map["diagnostics"].to_string().contains("\\\"revised\\\""),
        "{map}"
    );
    assert_eq!(map["outline"].as_array().map(Vec::len), Some(1), "{map}");
}

#[test]
fn quoted_yaml_strings_preserve_disk_bytes_and_refuse_unsupported_values() {
    let root = project("yaml-11-strings");
    let path = "tracking/SCALARS.md";
    std::fs::write(
        root.join(path),
        "---\nreviewed: pending # preserved\n---\n\n## Action\n\n- [ ] Keep this item.\n",
    )
    .unwrap();
    stamp(&root, path);
    for value in ["no", "YeS", "On", "OFF", "N", "~", "nUlL", ""] {
        let before = read(&root, path);
        let select = json!({"kind":"frontmatter_key","key":"reviewed"});
        let (_, got) = rivet(&root, "get", json!({"path":path,"select":select}));
        let (code, out) = rivet(
            &root,
            "set",
            json!({
                "path":path,"select":select,"guard":got["revision"],"content":value
            }),
        );
        assert_eq!(code, 0, "{out}");
        let after = read(&root, path);
        let before_line = before.lines().find(|l| l.starts_with("reviewed:")).unwrap();
        assert_eq!(
            after,
            before.replacen(before_line, &format!("reviewed: '{value}' # preserved"), 1)
        );
        let (_, got) = rivet(&root, "get", json!({"path":path,"select":select}));
        assert_eq!(got["value"], value);
        let (code, out) = rivet(
            &root,
            "set",
            json!({
                "path":path,"select":select,"guard":got["revision"],"content":"unsupported\nvalue"
            }),
        );
        assert_eq!(code, 2, "{out}");
        assert_eq!(out["error"]["code"], "unsupported_syntax");
        assert_eq!(read(&root, path), after);
    }
}

#[test]
fn a_lead_in_the_schema_does_not_declare_is_text() {
    // `**Progress**:` opens a line the way `**Gate**:` does, but the schema
    // declares only `Gate`. Reading it as a label would end the text there, so
    // the context would come back short and a context write carrying such a
    // line would re-read as something other than what was written.
    let root = project("lead-in");
    let path = "tracking/TRACKING_LEAD.md";
    let item = "- [ ] **Ship it. (2026-09-01)** First.\n      **Progress 2026-09-02**: half done.\n      **Gate**: it ships.\n";
    std::fs::write(root.join(path), format!("# Lead\n\n## Action\n\n{item}")).unwrap();
    stamp(&root, path);
    let (_, q) = rivet(
        &root,
        "query",
        json!({"collection": "actions", "paths": [path]}),
    );
    let subject = q["items"][0].clone();
    assert_eq!(
        subject["fields"]["context"], "First.\n**Progress 2026-09-02**: half done.",
        "{q}"
    );
    assert_eq!(subject["fields"]["gate"], "it ships.");
    // Appending another lead-in paragraph round-trips, and the gate stays put.
    let context = "First.\n**Progress 2026-09-02**: half done.\n**Reviewed 2026-09-03**: clean.";
    let (code, out) = rivet(
        &root,
        "update-item",
        json!({"path": path, "collection": "actions", "id": subject["id"],
               "guard": subject["revision"], "set": {"context": context}}),
    );
    assert_eq!(code, 0, "{out}");
    let (_, q) = rivet(
        &root,
        "query",
        json!({"collection": "actions", "paths": [path]}),
    );
    assert_eq!(q["items"][0]["fields"]["context"], context, "{q}");
    assert_eq!(q["items"][0]["fields"]["gate"], "it ships.");
    let text = read(&root, path);
    let reviewed = text.find("**Reviewed 2026-09-03**").unwrap();
    assert!(reviewed < text.find("**Gate**").unwrap(), "{text}");
    assert!(
        text.contains("\n- [ ] **Ship it. (2026-09-01)** First.\n"),
        "{text}"
    );
}

#[test]
fn ticking_rewrites_one_byte_and_moves_no_stay() {
    // An item whose body carries a stay owned by one of its own blocks cannot
    // be rewritten whole: that stay would have to be re-placed. A tick needs
    // none of that, so it changes the byte between the brackets and nothing else.
    let root = project("tick");
    let path = "tracking/TRACKING_TICK.md";
    let body = "# Tick\n\n## Action\n\n\
                - [ ] **First.** Plain text.\n\n\
                - [ ] **Second.** Has a table.\n\n\
                \x20     | a | b |\n\
                \x20     |---|---|\n\
                \x20     | 1 | 2 |\n\n\
                \x20     Closing paragraph. <!-- stay:innerP1 -->\n\
                \x20     **Gate**: done.\n";
    std::fs::write(root.join(path), body).unwrap();
    stamp(&root, path);
    // A bare `<` added by hand above both items after they were stamped. It
    // refuses any write that places a stay behind it (§3.4), and a tick places none.
    let stamped = read(&root, path).replacen("Plain text.", "Plain text, a < b.", 1);
    std::fs::write(root.join(path), &stamped).unwrap();
    let (_, q) = rivet(
        &root,
        "query",
        json!({"collection": "actions", "paths": [path]}),
    );
    let items = q["items"].as_array().unwrap().clone();
    assert_eq!(items.len(), 2, "{q}");
    let second = items[1].clone();
    let (code, out) = rivet(
        &root,
        "update-item",
        json!({"path": path, "collection": "actions", "id": second["id"],
               "guard": second["revision"], "set": {"done": true}}),
    );
    assert_eq!(code, 0, "{out}");
    let after = read(&root, path);
    let at = stamped.find("- [ ] **Second.**").unwrap() + 3;
    let mut want = stamped.clone();
    want.replace_range(at..at + 1, "x");
    // Every write refreshes the digests over what it changed; nothing else moves.
    assert_eq!(
        undigest(&after),
        undigest(&want),
        "only the checkbox byte may change"
    );
    let (_, q) = rivet(
        &root,
        "query",
        json!({"collection": "actions", "paths": [path]}),
    );
    assert_eq!(q["items"][1]["fields"]["done"], true, "{q}");
    // The boundary: rewriting the item's text moves its stay, which §3.4 still
    // refuses behind the `<`.
    let (code, out) = rivet(
        &root,
        "update-item",
        json!({"path": path, "collection": "actions", "id": q["items"][1]["id"],
               "guard": q["items"][1]["revision"], "set": {"done": false, "context": "Changed."}}),
    );
    assert_eq!(code, 2, "{out}");
    assert_eq!(
        read(&root, path),
        after,
        "a refused write leaves the file byte-identical"
    );
    // Unticking alone is the same one-byte write back.
    let (code, out) = rivet(
        &root,
        "update-item",
        json!({"path": path, "collection": "actions", "id": q["items"][1]["id"],
               "guard": q["items"][1]["revision"], "set": {"done": false}}),
    );
    assert_eq!(code, 0, "{out}");
    assert_eq!(undigest(&read(&root, path)), undigest(&stamped));
}

/// The document with every digest value blanked, for comparing text and stay ids.
fn undigest(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(i) = rest.find("sha256:") {
        out.push_str(&rest[..i + 7]);
        rest = rest[i + 7..].trim_start_matches(|c: char| c.is_ascii_hexdigit());
    }
    out.push_str(rest);
    out
}

#[test]
fn ticking_refreshes_the_digests_of_stays_inside_the_item() {
    // A stay inside the item that is not the item's own (here the list's, on the
    // item's first line and on a later paragraph) is refreshed by the tick like
    // any other write; the postcondition has to allow exactly that to change.
    for (n, body) in [
        "- [ ] **T** C <!-- stay:a1b2c3d4 subhash=sha256:000000000000 --> <!-- stay:L1st0001 hash=sha256:000000000000 -->\n",
        "- [ ] **T** C <!-- stay:a1b2c3d4 subhash=sha256:000000000000 -->\n\n  Second. <!-- stay:b10ck001 hash=sha256:000000000000 -->\n",
    ]
    .iter()
    .enumerate()
    {
        let root = project(&format!("tick-digest-{n}"));
        let path = "tracking/TRACKING_TICKD.md";
        std::fs::write(root.join(path), format!("# T\n\n## Action\n\n{body}")).unwrap();
        let (_, q) = rivet(&root, "query", json!({"collection": "actions", "paths": [path]}));
        let subject = q["items"][0].clone();
        assert_eq!(subject["id"], "a1b2c3d4", "case {n}: {q}");
        let before = read(&root, path);
        let (code, out) = rivet(
            &root,
            "update-item",
            json!({"path": path, "collection": "actions", "id": subject["id"],
                   "guard": subject["revision"], "set": {"done": true}}),
        );
        assert_eq!(code, 0, "case {n}: {out}");
        let mut want = before.clone();
        let at = want.find("- [ ]").unwrap() + 3;
        want.replace_range(at..at + 1, "x");
        assert_eq!(undigest(&read(&root, path)), undigest(&want), "case {n}");
    }
}

#[test]
fn ticking_back_to_stored_evidence_reports_prior_drift() {
    let root = project("tick-restores-digest");
    let path = "tracking/TRACKING_TICKD.md";
    std::fs::write(root.join(path), "# T\n\n## Action\n\n- [ ] **T** C\n").unwrap();
    stamp(&root, path);
    let stamped = read(&root, path);
    let changed = stamped.replacen("[ ]", "[x]", 1);
    std::fs::write(root.join(path), &changed).unwrap();
    let (_, q) = rivet(
        &root,
        "query",
        json!({"collection": "actions", "paths": [path]}),
    );
    let item = &q["items"][0];
    let (code, out) = rivet(
        &root,
        "update-item",
        json!({"path": path, "collection": "actions", "id": item["id"],
               "guard": item["revision"], "set": {"done": false}}),
    );
    assert_eq!(code, 0, "{out}");
    assert_eq!(read(&root, path), stamped, "only the checkbox byte changes");
    assert_eq!(
        out["evidence_refreshed"], 0,
        "the stored digests need no rewrite"
    );
    let drift = out["drift"].as_array().expect("prior drift is reported");
    assert!(
        drift
            .iter()
            .any(|e| e["key"] == "subhash" && e["prior_drift"] == true),
        "{out}"
    );
    assert!(
        drift
            .iter()
            .any(|e| e["key"] == "hash" && e["prior_drift"] == true),
        "{out}"
    );
}

const NESTED: &str = "tracking/TRACKING_NESTED.md";

/// A queue kept in dated `###` batches under `## Action`, with a `Plans`
/// subsection (and its own `####`) that is not part of the queue.
const NESTED_BODY: &str = "# Nested\n\n## Action\n\n\
    - [ ] **Top one.**\n\n\
    ### Open (2026-09-28): fresh\n\n\
    Nothing yet.\n\n\
    ### Open (2026-09-27): batch two\n\n\
    - [ ] **Two a.**\n\
    - [x] **Two b.**\n\n\
    #### Detail\n\n\
    - [ ] **Deep.**\n\n\
    ### Plans\n\n\
    - [ ] **Plan item.**\n\n\
    #### Plan detail\n\n\
    - [ ] **Plan deep.**\n\n\
    ### Open (2026-09-20): batch one\n\n\
    Prose.\n\n\
    - [ ] **One a.**\n\n\
    ## Log\n\n\
    - [ ] **Not an action.**\n";

/// The example project plus a `nested` collection over `NESTED`'s `## Action`
/// that takes its subsections. `actions` still matches the file too, so every
/// test can compare the opt-in binding against the existing one.
fn nested_project(name: &str) -> std::path::PathBuf {
    let root = project(name);
    std::fs::write(
        root.join(".rivet/project.json"),
        json!({"collections": {
            "actions": {"schema": "tracker-action@1", "files": ["tracking/*.md"],
                        "section": "Action", "level": 2},
            "nested": {"schema": "tracker-action@1", "files": [NESTED],
                       "section": "Action", "level": 2, "subsections": true,
                       "exclude": ["Plans", "Numbered threads"]},
            "batch": {"schema": "tracker-action@1", "files": [NESTED],
                      "section": "Open (2026-09-27): batch two", "subsections": true}}})
        .to_string(),
    )
    .unwrap();
    std::fs::write(root.join(NESTED), NESTED_BODY).unwrap();
    root
}

fn titles(root: &std::path::Path, collection: &str) -> Vec<(String, serde_json::Value)> {
    let (code, q) = rivet(
        root,
        "query",
        json!({"collection": collection, "paths": [NESTED]}),
    );
    assert_eq!(code, 0, "{q}");
    q["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| {
            (
                i["fields"]["title"].as_str().unwrap().to_string(),
                i["subsection"].clone(),
            )
        })
        .collect()
}

fn collection_guard(root: &std::path::Path, name: &str) -> serde_json::Value {
    let (_, map) = rivet(root, "map", json!({"path": NESTED}));
    map["collections"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == name)
        .unwrap_or_else(|| panic!("no {name} in {map}"))["revision"]
        .clone()
}

/// With `subsections`, every descendant's own body is a member and an
/// excluded title drops its whole subtree; without it, only the bound
/// section's own body counts, exactly as before the option existed.
#[test]
fn subsections_join_a_collection_and_excluded_subtrees_do_not() {
    let root = nested_project("nested-members");
    let batch = json!(["Open (2026-09-27): batch two"]);
    assert_eq!(
        titles(&root, "nested"),
        vec![
            ("Top one.".into(), json!([])),
            ("Two a.".into(), batch.clone()),
            ("Two b.".into(), batch),
            (
                "Deep.".into(),
                json!(["Open (2026-09-27): batch two", "Detail"])
            ),
            ("One a.".into(), json!(["Open (2026-09-20): batch one"])),
        ]
    );
    // The existing binding: own body only, and no subsection in its output.
    assert_eq!(
        titles(&root, "actions"),
        vec![("Top one.".into(), serde_json::Value::Null)]
    );
    let (_, map) = rivet(&root, "map", json!({"path": NESTED}));
    let nested = map["collections"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "nested")
        .unwrap()
        .clone();
    assert_eq!(nested["items"], 5, "{nested}");
    assert_eq!(
        nested["subsections"],
        json!([
            ["Open (2026-09-28): fresh"],
            ["Open (2026-09-27): batch two"],
            ["Open (2026-09-27): batch two", "Detail"],
            ["Open (2026-09-20): batch one"]
        ])
    );
    assert!(map["collections"][0].get("subsections").is_none(), "{map}");
    // Pin the default binding's typed revision, including its canonical
    // binding fingerprint. Native guard compatibility is checked separately.
    let (_, map) = rivet(&root, "map", json!({"path": WIDGETS}));
    assert_eq!(
        map["collections"][0]["revision"],
        "rivet:v1:collection:37611eeca9c98d8682550333d321476c"
    );
    // `get` names the item's subsection under the collections holding it.
    stamp(&root, NESTED);
    let (_, q) = rivet(
        &root,
        "query",
        json!({"collection": "nested", "paths": [NESTED],
               "where": [{"field": "title", "op": "eq", "value": "Deep."}]}),
    );
    let (code, got) = rivet(
        &root,
        "get",
        json!({"path": NESTED, "select": {"kind": "id", "id": q["items"][0]["id"]}}),
    );
    assert_eq!(code, 0, "{got}");
    assert_eq!(
        got["typed"]["nested"]["subsection"],
        json!(["Open (2026-09-27): batch two", "Detail"])
    );
    assert!(got["typed"].get("actions").is_none(), "{got}");
    // A binding on a `###` with no level takes its `####` the same way, with
    // paths relative to the section it binds.
    assert_eq!(
        titles(&root, "batch"),
        vec![
            ("Two a.".into(), json!([])),
            ("Two b.".into(), json!([])),
            ("Deep.".into(), json!(["Detail"])),
        ]
    );
    // A new dated batch joins by being written; nothing in the binding names it.
    let text = read(&root, NESTED).replacen(
        "### Open (2026-09-28): fresh",
        "### Open (2026-09-29): newest\n\n- [ ] **Newest.**\n\n### Open (2026-09-28): fresh",
        1,
    );
    std::fs::write(root.join(NESTED), text).unwrap();
    assert_eq!(
        titles(&root, "nested")[1],
        ("Newest.".into(), json!(["Open (2026-09-29): newest"]))
    );
}

/// `exclude` only means something for a binding that takes subsections, so
/// naming titles in it alone is refused rather than silently ignored. An empty
/// list excludes nothing either way and is accepted.
#[test]
fn exclude_without_subsections_is_refused() {
    let root = nested_project("nested-exclude-alone");
    std::fs::write(
        root.join(".rivet/project.json"),
        json!({"collections": {"nested": {"schema": "tracker-action@1", "files": [NESTED],
               "section": "Action", "exclude": ["Plans"]}}})
        .to_string(),
    )
    .unwrap();
    let (code, out) = rivet(&root, "query", json!({"collection": "nested"}));
    assert_eq!(code, 2, "{out}");
    assert_eq!(out["error"]["code"], "invalid_schema", "{out}");
    std::fs::write(
        root.join(".rivet/project.json"),
        json!({"collections": {"nested": {"schema": "tracker-action@1", "files": [NESTED],
               "section": "Action", "exclude": []}}})
        .to_string(),
    )
    .unwrap();
    let (code, out) = rivet(&root, "query", json!({"collection": "nested"}));
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["total"], 1, "{out}");
}

/// The collection guard covers each member's heading and own body: an edit
/// inside an excluded subtree leaves it alone, an edit in a member moves it.
#[test]
fn an_excluded_subsection_does_not_move_the_collection_guard() {
    let root = nested_project("nested-guard");
    let before = collection_guard(&root, "nested");
    let edit = |from: &str, to: &str| {
        let text = read(&root, NESTED).replacen(from, to, 1);
        std::fs::write(root.join(NESTED), text).unwrap();
    };
    edit("**Plan item.**", "**Plan item, reworded.**");
    edit("**Plan deep.**", "**Plan deep, reworded.**");
    assert_eq!(collection_guard(&root, "nested"), before);
    edit("Prose.", "Prose, reworded.");
    let moved = collection_guard(&root, "nested");
    assert_ne!(moved, before, "prose in a member moves the guard");
    edit("#### Detail", "#### Details");
    assert_ne!(
        collection_guard(&root, "nested"),
        moved,
        "so does a member heading"
    );
    // An insert under the stale guard is refused and writes nothing.
    let text = read(&root, NESTED);
    let (code, out) = rivet(
        &root,
        "insert-item",
        json!({"path": NESTED, "collection": "nested", "position": "end",
               "guard": before, "fields": {"title": "Late."}}),
    );
    assert_eq!(code, 2, "{out}");
    assert_eq!(out["error"]["code"], "guard_conflict", "{out}");
    assert_eq!(read(&root, NESTED), text);
}

/// `end` goes to the last member, `start` to the bound section's own body, and
/// `under` to the member it names; a list starts in a member that has none.
/// A name outside the members, or shared by two, is refused.
#[test]
fn a_typed_insert_lands_in_the_member_it_targets() {
    let root = nested_project("nested-insert");
    stamp(&root, NESTED);
    let insert = |position: &str, under: Option<&str>, title: &str| {
        let mut req = json!({"path": NESTED, "collection": "nested", "position": position,
                             "guard": collection_guard(&root, "nested"),
                             "fields": {"title": title}});
        if let Some(u) = under {
            req["under"] = json!(u);
        }
        rivet(&root, "insert-item", req)
    };
    for (position, under, title) in [
        ("end", None, "Appended."),
        ("start", None, "Prepended."),
        ("end", Some("Detail"), "Deeper."),
        ("start", Some("Open (2026-09-27): batch two"), "Two first."),
        ("end", Some("Open (2026-09-28): fresh"), "Fresh one."),
    ] {
        let before = read(&root, NESTED);
        let (code, out) = insert(position, under, title);
        assert_eq!(code, 0, "{title}: {out}");
        // The follow-up guard is the member-scoped one a fresh read gives.
        assert_eq!(
            out["ops"][0]["collection_revision"],
            collection_guard(&root, "nested"),
            "{title}"
        );
        // The write adds the item's line, and for a first list the list's
        // marker line and a blank line after it, and nothing else: every stay
        // already in the file keeps its place.
        let after = read(&root, NESTED);
        let lines: Vec<&str> = after.split_inclusive('\n').collect();
        let at = lines
            .iter()
            .position(|l| l.contains(&format!("**{title}**")))
            .unwrap();
        let added = if under == Some("Open (2026-09-28): fresh") {
            assert!(lines[at + 1].starts_with("<!-- stay:"), "{after}");
            assert_eq!(lines[at + 2], "\n", "{after}");
            3
        } else {
            1
        };
        let kept = [&lines[..at], &lines[at + added..]].concat().concat();
        assert_eq!(undigest(&kept), undigest(&before), "{title}");
    }
    let got = titles(&root, "nested");
    let one = json!(["Open (2026-09-20): batch one"]);
    let two = json!(["Open (2026-09-27): batch two"]);
    let detail = json!(["Open (2026-09-27): batch two", "Detail"]);
    assert_eq!(
        got,
        vec![
            ("Prepended.".into(), json!([])),
            ("Top one.".into(), json!([])),
            ("Fresh one.".into(), json!(["Open (2026-09-28): fresh"])),
            ("Two first.".into(), two.clone()),
            ("Two a.".into(), two.clone()),
            ("Two b.".into(), two),
            ("Deep.".into(), detail.clone()),
            ("Deeper.".into(), detail),
            ("One a.".into(), one.clone()),
            ("Appended.".into(), one),
        ]
    );
    let text = read(&root, NESTED);
    assert!(
        text.contains("Nothing yet.\n\n- [ ] **Fresh one.**"),
        "a first list starts under the member's prose:\n{text}"
    );
    // Excluded, absent and duplicated names are refused, byte-identically.
    std::fs::write(
        root.join(NESTED),
        text.replace("#### Detail", "#### Open (2026-09-20): batch one"),
    )
    .unwrap();
    let text = read(&root, NESTED);
    for (under, code_want) in [
        ("Plans", "missing_selector"),
        ("Plan detail", "missing_selector"),
        ("No such batch", "missing_selector"),
        ("Open (2026-09-20): batch one", "ambiguous_selector"),
    ] {
        let (code, out) = insert("end", Some(under), "Refused.");
        assert_eq!(code, 2, "{under}: {out}");
        assert_eq!(out["error"]["code"], code_want, "{under}: {out}");
        assert_eq!(read(&root, NESTED), text, "{under}");
    }
}

/// The default stamp scope and its §3.4 preview cover member lists: items in
/// every member get stays, excluded lists get none and cannot refuse a stamp.
#[test]
fn a_collection_stamp_covers_member_sections_only() {
    let root = nested_project("nested-stamp");
    let text = read(&root, NESTED)
        .replacen("**Plan item.**", "**Plan item.** Uses <placeholder>.", 1)
        .replacen("**Plan deep.**", "**Plan deep.** Uses a \\ backslash.", 1);
    std::fs::write(root.join(NESTED), &text).unwrap();
    let (_, map) = rivet(&root, "map", json!({"path": NESTED}));
    let (code, out) = rivet(
        &root,
        "stamp",
        json!({"path": NESTED, "guard": map["revision"], "preview": true}),
    );
    assert_eq!(code, 0, "{out}");
    assert!(out["ops"][0].get("refused").is_none(), "{out}");
    stamp(&root, NESTED);
    let stamped = read(&root, NESTED);
    for line in stamped.lines().filter(|l| l.starts_with("- [")) {
        let member = !line.contains("Plan") && !line.contains("Not an action");
        assert_eq!(line.contains("stay:"), member, "{line}");
    }
    // A blocker inside a member is the stamp's business and is reported.
    let blocked = text.replacen("**One a.**", "**One a.** Uses <placeholder>.", 1);
    std::fs::write(root.join(NESTED), &blocked).unwrap();
    let (_, map) = rivet(&root, "map", json!({"path": NESTED}));
    let (code, out) = rivet(
        &root,
        "stamp",
        json!({"path": NESTED, "guard": map["revision"], "preview": true}),
    );
    assert_eq!(code, 0, "{out}");
    let refused = out["ops"][0]["refused"].as_array().expect("a refusal");
    assert_eq!(refused.len(), 1, "{out}");
    assert_eq!(refused[0]["blocker"]["character"], "<", "{out}");
}

/// The defaults target a section, not a list: `end` starts a list in the last
/// member when it has none, though an earlier member has one, and `start`
/// starts one in the bound section's own body. Reordering `exclude` keeps the
/// guard, since membership is the same; changing it does not.
#[test]
fn an_insert_starts_a_list_in_an_empty_target_member() {
    let root = nested_project("nested-empty-target");
    let body = "# Nested\n\n## Action\n\nIntro prose.\n\n\
                ### Open (2026-09-27): has items\n\n- [ ] **Listed.**\n\n\
                ### Open (2026-09-20): prose only\n\nNo items yet.\n\n## Log\n";
    std::fs::write(root.join(NESTED), body).unwrap();
    for (position, title) in [("end", "At the end."), ("start", "At the start.")] {
        let before = read(&root, NESTED);
        let (code, out) = rivet(
            &root,
            "insert-item",
            json!({"path": NESTED, "collection": "nested", "position": position,
                   "guard": collection_guard(&root, "nested"), "fields": {"title": title}}),
        );
        assert_eq!(code, 0, "{out}");
        let after = read(&root, NESTED);
        let lines: Vec<&str> = after.split_inclusive('\n').collect();
        let at = lines
            .iter()
            .position(|l| l.contains(&format!("**{title}**")))
            .unwrap();
        assert!(lines[at + 1].starts_with("<!-- stay:"), "{after}");
        let kept = [&lines[..at], &lines[at + 3..]].concat().concat();
        assert_eq!(kept, before, "{title}");
    }
    assert_eq!(
        titles(&root, "nested"),
        vec![
            ("At the start.".into(), json!([])),
            ("Listed.".into(), json!(["Open (2026-09-27): has items"])),
            (
                "At the end.".into(),
                json!(["Open (2026-09-20): prose only"])
            ),
        ]
    );
    let text = read(&root, NESTED);
    assert!(
        text.contains("Intro prose.\n\n- [ ] **At the start.**"),
        "{text}"
    );
    assert!(
        text.contains("No items yet.\n\n- [ ] **At the end.**"),
        "{text}"
    );
    let guard = collection_guard(&root, "nested");
    let rebind = |exclude: serde_json::Value| {
        std::fs::write(
            root.join(".rivet/project.json"),
            json!({"collections": {"nested": {"schema": "tracker-action@1", "files": [NESTED],
                   "section": "Action", "level": 2, "subsections": true,
                   "exclude": exclude}}})
            .to_string(),
        )
        .unwrap();
        collection_guard(&root, "nested")
    };
    assert_eq!(rebind(json!(["Numbered threads", "Plans", "Plans"])), guard);
    assert_ne!(rebind(json!(["Plans"])), guard);
}

/// Without `subsections` the bound section is the only member, so `under` can
/// name it and nothing else, not even one of its own subsections.
#[test]
fn under_on_a_plain_binding_names_only_the_bound_section() {
    let root = nested_project("plain-under");
    let insert = |under: &str| {
        rivet(
            &root,
            "insert-item",
            json!({"path": NESTED, "collection": "actions", "position": "end", "under": under,
                   "guard": collection_guard(&root, "actions"), "fields": {"title": "Named."}}),
        )
    };
    let before = read(&root, NESTED);
    let (code, out) = insert("Open (2026-09-20): batch one");
    assert_eq!(code, 2, "{out}");
    assert_eq!(out["error"]["code"], "missing_selector", "{out}");
    assert_eq!(read(&root, NESTED), before);
    let (code, out) = insert("Action");
    assert_eq!(code, 0, "{out}");
    assert_eq!(
        titles(&root, "actions"),
        vec![
            ("Top one.".into(), serde_json::Value::Null),
            ("Named.".into(), serde_json::Value::Null),
        ]
    );
}

const LABELLED: &str = "tracking/TRACKING_LABELLED.md";

/// A project whose one collection binds a tracker-shaped schema with two
/// labels: `Waits`, which is one line, and `Gate`, which runs on across its
/// continuation lines when `multiline_gate` says so. The file holds `item`
/// alone under `## Action`, stamped.
fn labelled_project(name: &str, multiline_gate: bool, item: &str) -> std::path::PathBuf {
    let root = project(name);
    let gate = if multiline_gate {
        json!({"type": "string", "from": "label", "label": "Gate", "multiline": true})
    } else {
        json!({"type": "string", "from": "label", "label": "Gate"})
    };
    let schema = json!({"id": "labelled-action", "version": 1, "item": "checkbox", "fields": {
        "done": {"type": "boolean", "from": "checkbox"},
        "actor": {"type": "string", "from": "tag", "tags": ["agent", "human:*"], "default": "human:owner"},
        "title": {"type": "string", "from": "title"},
        "date": {"type": "date", "from": "title_date"},
        "context": {"type": "string", "from": "text"},
        "gate": gate,
        "waits": {"type": "string", "from": "label", "label": "Waits"}}});
    std::fs::write(
        root.join(".rivet/schemas/labelled-action.json"),
        schema.to_string(),
    )
    .unwrap();
    std::fs::write(
        root.join(".rivet/project.json"),
        json!({"collections": {"labelled": {"schema": "labelled-action@1",
               "files": [LABELLED], "section": "Action", "level": 2}}})
        .to_string(),
    )
    .unwrap();
    std::fs::write(
        root.join(LABELLED),
        format!("# Labelled\n\n## Action\n\n{item}\n\n## Log\n\nUntouched.\n"),
    )
    .unwrap();
    stamp(&root, LABELLED);
    root
}

/// The one item's fields, id and guard.
fn labelled_item(root: &std::path::Path) -> serde_json::Value {
    let (code, q) = rivet(root, "query", json!({"collection": "labelled"}));
    assert_eq!(code, 0, "{q}");
    assert_eq!(q["items"].as_array().map(Vec::len), Some(1), "{q}");
    q["items"][0].clone()
}

fn labelled_set(root: &std::path::Path, set: serde_json::Value) -> (i32, serde_json::Value) {
    let item = labelled_item(root);
    rivet(
        root,
        "update-item",
        json!({"path": LABELLED, "collection": "labelled", "id": item["id"],
               "guard": item["revision"], "set": set}),
    )
}

/// The `## Action` body as written, stay markers taken out.
fn labelled_body(root: &std::path::Path) -> String {
    let text = read(root, LABELLED);
    assert!(text.trim_end().ends_with("\n\nUntouched."), "{text}");
    text.split("## Action\n")
        .nth(1)
        .unwrap()
        .split("\n## Log")
        .next()
        .unwrap()
        .lines()
        .filter(|l| !l.trim_start().starts_with("<!-- stay:"))
        .map(|l| l.split(" <!-- stay:").next().unwrap())
        .collect::<Vec<_>>()
        .join("\n")
        .trim_matches('\n')
        .to_string()
}

#[test]
fn a_leading_declared_label_is_a_field_and_preserves_its_item() {
    for (name, item, multiline) in [
        ("first-label", "- [ ] **Gate**: original", false),
        ("first-label-underscores", "- [ ] __Gate__: original", false),
        ("first-label-tag", "- [ ] `agent` **Gate**: original", false),
        (
            "first-label-spaced-tag",
            "- [ ] `agent`   **Gate**: original",
            false,
        ),
        (
            "first-label-multiline",
            "- [ ] **Gate**: original\n      continued",
            true,
        ),
    ] {
        let root = labelled_project(name, multiline, item);
        let before = read(&root, LABELLED);
        let original = labelled_item(&root);
        assert!(original["fields"].get("title").is_none(), "{original}");
        assert_eq!(
            original["fields"]["gate"],
            if multiline {
                "original\ncontinued"
            } else {
                "original"
            }
        );
        let value = if multiline {
            "replacement\ncontinued"
        } else {
            "replacement"
        };
        let (code, out) = labelled_set(&root, json!({"gate":value}));
        assert_eq!(code, 0, "{out}");
        let after = read(&root, LABELLED);
        let expected = before.replacen("original", "replacement", 1);
        // Digest refresh is allowed, but every other byte belongs to the source.
        assert_eq!(undigest(&after), undigest(&expected));
        let updated = labelled_item(&root);
        assert_eq!(updated["id"], original["id"]);
        assert_eq!(updated["fields"]["gate"], value);
        assert!(updated["fields"].get("title").is_none());
        assert_eq!(
            after.matches("**Gate**:").count() + after.matches("__Gate__:").count(),
            1
        );
        let (code, out) = labelled_set(&root, json!({"gate":42}));
        assert_eq!(code, 2, "{out}");
        assert_eq!(read(&root, LABELLED), after);
    }
}

#[test]
fn head_updates_keep_a_leading_label_separate_and_attached() {
    for (source_item, set, expected) in [
        (
            "- [ ] **Gate**: original",
            json!({"actor":"agent"}),
            "- [ ] `agent` **Gate**: original",
        ),
        (
            "- [ ] **Gate**: original",
            json!({"title":"Title"}),
            "- [ ] **Title**\n      **Gate**: original",
        ),
        (
            "- [ ] **Gate**: original",
            json!({"context":"Context"}),
            "- [ ] Context\n      **Gate**: original",
        ),
        (
            "- [ ] **Gate**: original",
            json!({"actor":"agent","title":"Title","context":"Context","gate":"updated"}),
            "- [ ] `agent` **Title** Context\n      **Gate**: updated",
        ),
        (
            "- [ ] `agent`   **Gate**: original",
            json!({"context":"Context"}),
            "- [ ] `agent`   Context\n      **Gate**: original",
        ),
        (
            "- [ ] `agent`   **Gate**: original",
            json!({"title":"Title","context":"Context","gate":"updated"}),
            "- [ ] `agent` **Title**   Context\n      **Gate**: updated",
        ),
        (
            "- [ ]   **Gate**: original",
            json!({"context":"Context"}),
            "- [ ]   Context\n      **Gate**: original",
        ),
    ] {
        for newline in ["\n", "\r\n"] {
            let root = labelled_project("inline-label-head", false, source_item);
            if newline != "\n" {
                std::fs::write(
                    root.join(LABELLED),
                    read(&root, LABELLED).replace('\n', newline),
                )
                .unwrap();
            }
            let before = read(&root, LABELLED);
            let original = labelled_item(&root);
            let (code, out) = labelled_set(&root, set.clone());
            assert_eq!(code, 0, "{source_item}: {set}: {out}");
            let after = read(&root, LABELLED);
            let expected = before.replacen(source_item, &expected.replace('\n', newline), 1);
            assert_eq!(undigest(&after), undigest(&expected));
            let updated = labelled_item(&root);
            assert_eq!(updated["id"], original["id"]);
            for (field, value) in set.as_object().unwrap() {
                assert_eq!(updated["fields"][field], *value, "{updated}");
            }
            assert_eq!(
                updated["fields"]["gate"],
                set.get("gate").cloned().unwrap_or(json!("original"))
            );
        }
    }
}

#[test]
fn removing_a_tag_keeps_the_inline_label_and_authored_gap() {
    for gap in [" ", "   "] {
        let source_item = format!("- [ ] `agent`{gap}**Gate**: original");
        let root = labelled_project("inline-label-remove-tag", false, &source_item);
        let before = read(&root, LABELLED);
        let original = labelled_item(&root);
        let (code, out) = labelled_set(&root, json!({"actor":null,"title":null}));
        assert_eq!(code, 0, "{out}");
        assert_eq!(
            undigest(&read(&root, LABELLED)),
            undigest(&before.replacen(" `agent`", "", 1))
        );
        let after = labelled_item(&root);
        assert_eq!(after["id"], original["id"]);
        assert_eq!(after["fields"]["gate"], "original");
        assert_eq!(after["fields"]["actor"], "human:owner");
    }
}

#[test]
fn a_leading_bold_run_needs_a_declared_label_and_colon_to_be_a_field() {
    for (name, item, title, context) in [
        ("first-title", "- [ ] **Gate** original", "Gate", "original"),
        (
            "first-undeclared",
            "- [ ] **Progress**: original",
            "Progress",
            ": original",
        ),
        (
            "first-title-before-label",
            "- [ ] **Title**\n      **Gate**: original",
            "Title",
            "",
        ),
    ] {
        let root = labelled_project(name, false, item);
        let item = labelled_item(&root);
        assert_eq!(item["fields"]["title"], title, "{item}");
        if !context.is_empty() {
            assert_eq!(item["fields"]["context"], context, "{item}");
        }
    }
}

/// Punctuation can delimit a bold title without a space. Replacing that text
/// with a word needs a separator or the title becomes part of the context.
#[test]
fn a_context_write_next_to_a_title_keeps_the_title() {
    for (n, open, close, context) in [
        (0, "**", "**", ": Old context."),
        (1, "**", "**", ":Old context."),
        (2, "__", "__", ": Old context."),
    ] {
        let item = format!(
            "- [ ] `agent` {open}T.{close}{context}\n      **Waits**: none\n      **Gate**: ready."
        );
        let root = labelled_project(&format!("adjacent-text-{n}"), false, &item);
        let first = labelled_item(&root);
        assert_eq!(first["fields"]["title"], "T.");
        assert_eq!(first["fields"]["context"], context);
        let before = read(&root, LABELLED);
        let (code, out) = labelled_set(&root, json!({"context": context}));
        assert_eq!(code, 0, "{out}");
        assert_eq!(
            read(&root, LABELLED),
            before,
            "a no-op keeps the punctuation"
        );
        let (code, out) = labelled_set(&root, json!({"context": "New context."}));
        assert_eq!(code, 0, "{out}");
        assert_eq!(
            labelled_body(&root),
            format!(
                "- [ ] `agent` {open}T.{close} New context.\n      **Waits**: none\n      **Gate**: ready."
            )
        );
        let after = labelled_item(&root);
        assert_eq!(after["id"], first["id"], "the action keeps its address");
        assert_eq!(after["fields"]["title"], "T.");
        assert_eq!(after["fields"]["context"], "New context.");
        let before = read(&root, LABELLED);
        let (code, out) = rivet(
            &root,
            "update-item",
            json!({"path": LABELLED,
            "collection": "labelled", "id": first["id"], "guard": first["revision"],
            "set": {"context": "Stale."}}),
        );
        assert_eq!(code, 2, "{out}");
        assert_eq!(out["error"]["code"], "guard_conflict");
        assert_eq!(read(&root, LABELLED), before);
        let (code, out) = labelled_set(&root, json!({"context": "New.\n**Waits**: injected"}));
        assert_eq!(code, 2, "{out}");
        assert_eq!(
            read(&root, LABELLED),
            before,
            "a refused write keeps all bytes"
        );
    }
    let root = labelled_project(
        "adjacent-combined",
        false,
        "- [ ] **T.**: Old context.\n      **Waits**: none",
    );
    let (code, out) = labelled_set(&root, json!({"title": "Changed.", "context": "New."}));
    assert_eq!(code, 0, "{out}");
    assert_eq!(
        labelled_body(&root),
        "- [ ] **Changed.** New.\n      **Waits**: none"
    );
}

/// Several missing labels share the paragraph's insertion point. They are
/// one ordered append, not conflicting replacements of the existing text.
#[test]
fn several_new_labels_can_be_written_together() {
    for (n, item) in [
        (
            0,
            "- [ ] **Choose the cleanup scope.** Existing context. Gate: approval required.",
        ),
        (
            1,
            "- [ ] `human:owner` **Choose a service option.** Existing\n      context\n      **Gate:** decision required;\n      continuation.",
        ),
    ] {
        let root = labelled_project(&format!("new-labels-{n}"), false, item);
        let first = labelled_item(&root);
        let (code, out) = labelled_set(
            &root,
            json!({"actor": "agent", "context": "New context.",
            "gate": "New gate.", "title": "Changed.", "waits": "none"}),
        );
        assert_eq!(code, 0, "{out}");
        assert_eq!(
            labelled_body(&root),
            "- [ ] `agent` **Changed.** New context.\n      **Gate**: New gate.\n      **Waits**: none"
        );
        let after = labelled_item(&root);
        assert_eq!(after["id"], first["id"]);
        assert_eq!(after["fields"]["actor"], "agent");
        assert_eq!(after["fields"]["context"], "New context.");
        assert_eq!(after["fields"]["gate"], "New gate.");
        assert_eq!(after["fields"]["waits"], "none");
    }
    let root = labelled_project(
        "new-labels-preserve",
        false,
        "- [ ] **T.** Context with  spacing.\n      More context.",
    );
    let before = read(&root, LABELLED);
    let first = labelled_item(&root);
    let (code, out) = labelled_set(&root, json!({"gate": "Ready.", "waits": "none"}));
    assert_eq!(code, 0, "{out}");
    assert_eq!(
        labelled_body(&root),
        "- [ ] **T.** Context with  spacing.\n      More context.\n      **Gate**: Ready.\n      **Waits**: none"
    );
    assert_eq!(labelled_item(&root)["id"], first["id"]);
    assert_eq!(
        labelled_item(&root)["fields"]["context"],
        first["fields"]["context"]
    );
    // A value that would create an extra undeclared change is refused whole.
    let after = read(&root, LABELLED);
    let (code, out) = labelled_set(
        &root,
        json!({"gate": "Ready.\n**Waits**: injected", "waits": "judgment"}),
    );
    assert_eq!(code, 2, "{out}");
    assert_eq!(read(&root, LABELLED), after);
    assert_ne!(after, before);
}

/// Lines after a one-line label, before the next label, are the item's text.
/// They used to sit in no field: `get` left them out of `context`, and a
/// context write put the new text on the title line while the old lines stayed
/// where they were, so the item carried both and the write reported success.
#[test]
fn text_after_a_one_line_label_is_text() {
    let item = "- [ ] `agent` **Ship it. (2026-09-01)**\n      **Waits**: none\n      Old context, line one,\n      and line two.\n      **Gate**: it ships.";
    let root = labelled_project("tail-text", false, item);
    let fields = labelled_item(&root)["fields"].clone();
    assert_eq!(
        fields["context"], "Old context, line one,\nand line two.",
        "{fields}"
    );
    assert_eq!(fields["waits"], "none");
    assert_eq!(fields["gate"], "it ships.");
    let (code, out) = labelled_set(&root, json!({"context": "New context."}));
    assert_eq!(code, 0, "{out}");
    assert_eq!(
        labelled_body(&root),
        "- [ ] `agent` **Ship it. (2026-09-01)**\n      **Waits**: none\n      New context.\n      **Gate**: it ships."
    );
    let (code, out) = labelled_set(&root, json!({"context": "New context,\nover two lines."}));
    assert_eq!(code, 0, "{out}");
    assert_eq!(
        labelled_body(&root),
        "- [ ] `agent` **Ship it. (2026-09-01)**\n      **Waits**: none\n      New context,\n      over two lines.\n      **Gate**: it ships."
    );
    // The head is rebuilt around the text, not over it.
    let (code, out) = labelled_set(&root, json!({"title": "Ship it now", "waits": "judgment"}));
    assert_eq!(code, 0, "{out}");
    assert_eq!(
        labelled_body(&root),
        "- [ ] `agent` **Ship it now (2026-09-01)**\n      **Waits**: judgment\n      New context,\n      over two lines.\n      **Gate**: it ships."
    );
    // Removing the text takes its lines, and leaves the labels as they were.
    let (code, out) = labelled_set(&root, json!({"context": null}));
    assert_eq!(code, 0, "{out}");
    assert_eq!(
        labelled_body(&root),
        "- [ ] `agent` **Ship it now (2026-09-01)**\n      **Waits**: judgment\n      **Gate**: it ships."
    );
    assert!(labelled_item(&root)["fields"].get("context").is_none());
}

/// Text on both sides of a one-line label is one field. A write replaces all
/// of it, landing where the text starts, so the result re-reads as exactly the
/// value written rather than the new text beside the old.
#[test]
fn text_split_by_a_one_line_label_is_one_field() {
    let item = "- [ ] **T.** Before.\n      **Waits**: judgment\n      After.\n      **Gate**: g.";
    let root = labelled_project("split-text", false, item);
    assert_eq!(labelled_item(&root)["fields"]["context"], "Before.\nAfter.");
    // A write that names only a label leaves both halves where they are.
    let (code, out) = labelled_set(&root, json!({"waits": "none"}));
    assert_eq!(code, 0, "{out}");
    assert_eq!(
        labelled_body(&root),
        "- [ ] **T.** Before.\n      **Waits**: none\n      After.\n      **Gate**: g."
    );
    let (code, out) = labelled_set(&root, json!({"context": "Before.\nAfter.\nMore."}));
    assert_eq!(code, 0, "{out}");
    assert_eq!(
        labelled_body(&root),
        "- [ ] **T.** Before.\n      After.\n      More.\n      **Waits**: none\n      **Gate**: g."
    );
    assert_eq!(
        labelled_item(&root)["fields"]["context"],
        "Before.\nAfter.\nMore."
    );
}

/// A `multiline` label's value runs from its own line to the next declared
/// label line or the end of the paragraph, so a gate wrapped across lines
/// reads whole, a multi-line gate can be written, and a shorter one replaces
/// every old line rather than only the first.
#[test]
fn a_multiline_label_runs_to_the_next_label() {
    let item = "- [ ] **T.** Context.\n      **Gate**: first line of the gate\n      and its second line.\n      **Progress 2026-09-02**: a lead-in inside it.\n      **Waits**: none";
    let root = labelled_project("multiline", true, item);
    let fields = labelled_item(&root)["fields"].clone();
    assert_eq!(
        fields["gate"],
        "first line of the gate\nand its second line.\n**Progress 2026-09-02**: a lead-in inside it.",
        "{fields}"
    );
    assert_eq!(fields["context"], "Context.");
    assert_eq!(fields["waits"], "none");
    let (code, out) = labelled_set(&root, json!({"gate": "a new gate\nover two lines."}));
    assert_eq!(code, 0, "{out}");
    assert_eq!(
        labelled_body(&root),
        "- [ ] **T.** Context.\n      **Gate**: a new gate\n      over two lines.\n      **Waits**: none"
    );
    let (code, out) = labelled_set(&root, json!({"gate": "one line."}));
    assert_eq!(code, 0, "{out}");
    assert_eq!(
        labelled_body(&root),
        "- [ ] **T.** Context.\n      **Gate**: one line.\n      **Waits**: none"
    );
    let (code, out) = labelled_set(
        &root,
        json!({"gate": "back to\ntwo lines.", "context": "New."}),
    );
    assert_eq!(code, 0, "{out}");
    let (code, out) = labelled_set(&root, json!({"gate": null}));
    assert_eq!(code, 0, "{out}");
    assert_eq!(
        labelled_body(&root),
        "- [ ] **T.** New.\n      **Waits**: none"
    );
    // A one-line label still refuses a value that spans lines, unchanged.
    let before = read(&root, LABELLED);
    let (code, out) = labelled_set(&root, json!({"waits": "a\nb"}));
    assert_eq!(code, 2, "{out}");
    assert_eq!(out["error"]["code"], "invalid_item", "{out}");
    assert_eq!(read(&root, LABELLED), before);
    // An insert lays a multi-line gate out the same way and it reads back.
    let (_, map) = rivet(&root, "map", json!({"path": LABELLED}));
    let (code, out) = rivet(
        &root,
        "insert-item",
        json!({"path": LABELLED, "collection": "labelled", "position": "end",
               "guard": map["collections"][0]["revision"],
               "fields": {"title": "Added.", "gate": "x\ny", "waits": "none"}}),
    );
    assert_eq!(code, 0, "{out}");
    let (_, q) = rivet(&root, "query", json!({"collection": "labelled"}));
    assert_eq!(q["items"][1]["fields"]["gate"], "x\ny", "{q}");
    assert_eq!(q["items"][1]["fields"]["waits"], "none", "{q}");
    // Writing a multiline value back as it reads changes no byte of it, a hard
    // break's trailing spaces included, whatever else the request changes.
    let root = labelled_project(
        "multiline-same",
        true,
        "- [ ] **T.** Context.\n      **Gate**: first line  \n      second line.\n      **Waits**: none",
    );
    let gate = labelled_item(&root)["fields"]["gate"].clone();
    assert_eq!(gate, "first line\nsecond line.");
    let (code, out) = labelled_set(&root, json!({"gate": gate, "waits": "judgment"}));
    assert_eq!(code, 0, "{out}");
    assert!(
        read(&root, LABELLED)
            .contains("**Gate**: first line  \n      second line.\n      **Waits**: judgment"),
        "{}",
        read(&root, LABELLED)
    );
    // `multiline` is for labels only.
    let schema = root.join(".rivet/schemas/labelled-action.json");
    let text = read(&root, ".rivet/schemas/labelled-action.json");
    std::fs::write(
        &schema,
        text.replace(r#""from":"text""#, r#""from":"text","multiline":true"#),
    )
    .unwrap();
    let (code, out) = rivet(&root, "query", json!({"collection": "labelled"}));
    assert_eq!(code, 2, "{out}");
    assert_eq!(out["error"]["code"], "invalid_schema", "{out}");
}

/// A refused typed write names the fields that would not read back, and a
/// title ending in a date is refused before planning, since the date always
/// reads back as the `date` field and never as part of the title.
#[test]
fn a_write_that_would_not_read_back_names_its_fields() {
    let root = labelled_project("names", false, "- [ ] **T.** Context.");
    let insert = |fields: serde_json::Value| {
        let (_, map) = rivet(&root, "map", json!({"path": LABELLED}));
        rivet(
            &root,
            "insert-item",
            json!({"path": LABELLED, "collection": "labelled", "position": "end",
                   "guard": map["collections"][0]["revision"], "fields": fields}),
        )
    };
    let before = read(&root, LABELLED);
    let (code, out) = insert(json!({"title": "Ship it (2026-09-28)"}));
    assert_eq!(code, 2, "{out}");
    assert_eq!(out["error"]["code"], "invalid_item", "{out}");
    let message = out["error"]["message"].as_str().unwrap();
    assert!(
        message.contains("title") && message.contains("`date`") && message.contains("2026-09-28"),
        "{message}"
    );
    let (code, out) = labelled_set(&root, json!({"title": "Ship it (2026-09-28)"}));
    assert_eq!(code, 2, "{out}");
    let message = out["error"]["message"].as_str().unwrap();
    assert!(message.contains("`date`"), "{message}");
    // Text carrying a line that opens with a declared label re-reads as that
    // label: the refusal says which field came back short, and with what.
    let (code, out) = insert(json!({"title": "U", "context": "One.\n**Waits**: sneaked in"}));
    assert_eq!(code, 2, "{out}");
    let message = out["error"]["message"].as_str().unwrap();
    assert!(
        message.contains(r#"context re-reads as "One." rather than"#),
        "{message}"
    );
    let (code, out) = labelled_set(&root, json!({"context": "One.\n**Waits**: sneaked in"}));
    assert_eq!(code, 2, "{out}");
    let message = out["error"]["message"].as_str().unwrap();
    assert!(
        message.contains(r#"waits re-reads as "sneaked in", which was not asked for"#),
        "{message}"
    );
    assert_eq!(read(&root, LABELLED), before);
}

/// Marker-shaped text inside a code span is content, the way the document
/// model already reads it, so an item quoting one can be written; a live
/// marker in the content is still refused.
#[test]
fn marker_shaped_text_in_a_code_span_is_content() {
    let item = "- [ ] **T.** Quotes `<!-- stay:AbCd1234 -->` in a span.\n      **Waits**: none";
    let root = labelled_project("quoted-marker", false, item);
    let (code, out) = labelled_set(&root, json!({"waits": "judgment"}));
    assert_eq!(code, 0, "{out}");
    let (code, out) = labelled_set(
        &root,
        json!({"context": "Quotes `<!-- stay:AbCd1234 -->` in a span.\nAnd `<!-- stay:ZyXw9876 subhash=sha256:0 -->` too."}),
    );
    assert_eq!(code, 0, "{out}");
    assert_eq!(
        labelled_body(&root),
        "- [ ] **T.** Quotes `<!-- stay:AbCd1234 -->` in a span.\n      And `<!-- stay:ZyXw9876 subhash=sha256:0 -->` too.\n      **Waits**: judgment"
    );
    for id in ["AbCd1234", "ZyXw9876"] {
        let (code, out) = rivet(
            &root,
            "get",
            json!({"path": LABELLED, "select": {"kind": "id", "id": id}}),
        );
        assert_eq!(code, 2, "{out}");
        assert_eq!(out["error"]["code"], "missing_selector", "{out}");
    }
    // A marker in an indented code block nested under a list item is content
    // too, the code block's extent read the way the document model reads it.
    let select = json!({"kind": "heading_path", "path": ["Log"]});
    let (_, log) = rivet(&root, "get", json!({"path": LABELLED, "select": select}));
    let preview = |body: &str| {
        rivet(
            &root,
            "set",
            json!({"path": LABELLED, "select": select, "part": "body",
                   "guard": log["revision"], "content": body, "preview": true}),
        )
    };
    let (code, out) = preview("- example\n\n      one\n      <!-- stay:AbCd1234 -->\n\n- other");
    assert_eq!(code, 0, "{out}");
    // An opening no markstay reader takes for a marker is content in code of
    // either kind, and refused in prose.
    let (code, out) = preview("```\n<!-- stay:unfinished\n```");
    assert_eq!(code, 0, "{out}");
    let (code, out) = preview("    code\n    <!-- stay:unfinished");
    assert_eq!(code, 0, "{out}");
    let (code, out) = preview("Prose with <!-- stay:unfinished in it.");
    assert_eq!(code, 2, "{out}");
    assert_eq!(out["error"]["code"], "invalid_content", "{out}");
    // Past the extent comrak reports for a nested indented block, the safe side.
    let (code, out) = preview("- item\n\n      code\n      <!-- stay:unfinished\n\n- other");
    assert_eq!(code, 2, "{out}");
    // Applied, not only previewed: the body reads back as written, the text
    // before it is byte-identical, and the item's stay is still attached.
    let item = labelled_item(&root);
    let text = read(&root, LABELLED);
    let head = &text[..text.find("## Log").unwrap()];
    let body = "```\n<!-- stay:unfinished\n```";
    let (code, out) = rivet(
        &root,
        "set",
        json!({"path": LABELLED, "select": select, "part": "body",
               "guard": log["revision"], "content": body}),
    );
    assert_eq!(code, 0, "{out}");
    let after = read(&root, LABELLED);
    assert!(after.starts_with(head), "{after}");
    assert!(
        after.contains("\n```\n<!-- stay:unfinished\n```"),
        "{after}"
    );
    assert_eq!(labelled_item(&root)["id"], item["id"]);
    let before = read(&root, LABELLED);
    for context in [
        "A live <!-- stay:ZyXw9876 --> marker.",
        "An unfinished <!-- stay:ZyXw9876 outside code.",
        "[<!-- stay:ZyXw9876 -->](https://example.com) is link text, not code.",
    ] {
        let (code, out) = labelled_set(&root, json!({"context": context}));
        assert_eq!(code, 2, "{context}: {out}");
        assert_eq!(out["error"]["code"], "invalid_content", "{context}: {out}");
        assert_eq!(read(&root, LABELLED), before);
    }
}

mod analytics_cases;

#[test]
fn version_names_the_build_commit() {
    for flag in ["--version", "-V"] {
        let out = raw(&[flag], b"");
        assert_eq!(out.status.code(), Some(0));
        let text = String::from_utf8(out.stdout).unwrap();
        let mut words = text.trim().split(' ');
        assert_eq!(words.next(), Some("markrivet"));
        assert_eq!(words.next(), Some(env!("CARGO_PKG_VERSION")));
        // Built from this checkout: HEAD, dirty exactly when the build's
        // sources differ from it.
        let git = |args: &[&str]| {
            let out = Command::new("git")
                .args(args)
                .current_dir(env!("CARGO_MANIFEST_DIR"))
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}");
            String::from_utf8(out.stdout).unwrap().trim().to_string()
        };
        let head = git(&["rev-parse", "HEAD"]);
        let dirty = !git(&[
            "-C",
            "../..",
            "status",
            "--porcelain",
            "--",
            "Cargo.toml",
            "Cargo.lock",
            "crates",
        ])
        .is_empty();
        let want = if dirty { format!("{head}-dirty") } else { head };
        assert_eq!(words.next(), Some(want.as_str()), "{text}");
    }
}

#[test]
fn create_writes_a_new_document_and_refuses_everything_else() {
    let root = project("create-off");
    let path = "tracking/TRACKING_NEW.md";
    let content = "# New\n\n## Action\n\n- [ ] **First.** Opened.\n";
    // A preview writes nothing.
    let (code, out) = rivet(
        &root,
        "create",
        json!({"path": path, "content": content, "stamp": true, "preview": true}),
    );
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["status"], "preview");
    assert!(!root.join(path).exists());
    let (code, out) = rivet(
        &root,
        "create",
        json!({"path": path, "content": content, "stamp": true}),
    );
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["status"], "applied");
    assert!(out.get("commit").is_none(), "no git key, no commit field");
    let written = read(&root, path);
    let minted = out["ops"][0]["minted"].as_array().unwrap();
    // Two headings and the bound action item with its list.
    assert_eq!(minted.len(), 4, "{out}");
    let (_, map) = rivet(&root, "map", json!({"path": path}));
    assert_eq!(map["revision"], out["revision"]);
    let mode = std::os::unix::fs::PermissionsExt::mode(
        &std::fs::metadata(root.join(path)).unwrap().permissions(),
    );
    assert_eq!(mode & 0o111, 0, "not executable");
    // Without stamping, the bytes are exactly the content, CRLF included.
    let crlf = "# C\r\n\r\nText.\r\n";
    let (code, out) = rivet(&root, "create", json!({"path": "c.md", "content": crlf}));
    assert_eq!(code, 0, "{out}");
    assert_eq!(read(&root, "c.md"), crlf);
    // Refusals leave everything as it was.
    for (request, want) in [
        (
            json!({"path": path, "content": "# Other\n"}),
            "already_exists",
        ),
        (
            json!({"path": "missing/dir.md", "content": "# X\n"}),
            "not_found",
        ),
        (
            json!({"path": "x.txt", "content": "# X\n"}),
            "path_rejected",
        ),
        (
            json!({"path": "../x.md", "content": "# X\n"}),
            "path_rejected",
        ),
        (json!({"path": "e.md", "content": " \n"}), "invalid_content"),
        (
            json!({"path": "e.md", "content": "a\rb\n"}),
            "invalid_content",
        ),
        (
            json!({"path": "e.md", "content": "# X\n<!-- stay:abcDEF12 hash=sha256:000000000000 -->\n"}),
            "invalid_content",
        ),
        (
            json!({"path": "e.md", "content": "# X\n", "message": "x\nRivet-Op: y"}),
            "invalid_request",
        ),
    ] {
        let (code, out) = rivet(&root, "create", request.clone());
        assert_eq!(code, 2, "{request}: {out}");
        assert_eq!(out["error"]["code"], want, "{request}: {out}");
    }
    assert_eq!(read(&root, path), written);
    assert!(!root.join("e.md").exists());
    assert!(!root.join("missing").exists(), "no directory is created");
    let names: Vec<String> = std::fs::read_dir(root.join("tracking"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
        .collect();
    assert!(!names.iter().any(|n| n.contains(".rivet-")), "{names:?}");
}

#[test]
fn adopt_and_the_new_ops_are_single_commands() {
    let root = project("new-op-commands");
    stamp(&root, WIDGETS);
    let (_, map) = rivet(&root, "map", json!({"path": WIDGETS}));
    let doc = map["revision"].as_str().unwrap().to_string();
    let (code, out) = rivet(
        &root,
        "add-key",
        json!({"path": WIDGETS, "key": "reviewed", "value": "2026-10-10", "guard": doc}),
    );
    assert_eq!(code, 0, "{out}");
    assert!(read(&root, WIDGETS).contains("\nreviewed: 2026-10-10\n---\n"));
    let select = json!({"kind": "heading_path", "path": ["Notes"]});
    let (_, got) = rivet(&root, "get", json!({"path": WIDGETS, "select": select}));
    let (code, out) = rivet(
        &root,
        "append",
        json!({"path": WIDGETS, "select": select, "guard": got["revision"], "content": "Appended."}),
    );
    assert_eq!(code, 0, "{out}");
    // The op's own name in the request is refused, as for every command.
    let (code, _) = rivet(
        &root,
        "rejoin",
        json!({"op": "rejoin", "path": WIDGETS, "select": select, "guard": "x", "line": 1}),
    );
    assert_eq!(code, 1);
    let (code, out) = rivet(&root, "adopt", json!({"path": WIDGETS}));
    assert_eq!(code, 2, "{out}");
    assert_eq!(out["error"]["code"], "invalid_request", "commit mode only");
}

/// Every entry in `dir` whose name marks a Rivet temporary file.
fn rivet_temps(dir: &std::path::Path) -> Vec<String> {
    std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.contains(".rivet-"))
        .collect()
}

#[test]
fn a_live_emacs_lock_refuses_a_write_and_a_dead_one_does_not() {
    let host = host_name();
    let (pid, boot, unused) = (std::process::id(), boot_time(), unused_pid());
    // (case, lock text, as a regular file, refused)
    let cases = [
        ("live", live_lock_text(), false, true),
        ("live-no-boot", format!("ada@{host}.{pid}"), false, true),
        ("live-file", live_lock_text(), true, true),
        // The boot field is parsed, never compared: no clock decides.
        (
            "live-other-boot",
            format!("ada@{host}.{pid}:1"),
            false,
            true,
        ),
        (
            "foreign",
            format!("ada@elsewhere.invalid.{unused}:{boot}"),
            false,
            true,
        ),
        ("unparseable", "not a lock".to_string(), false, true),
        (
            "dead-pid",
            format!("ada@{host}.{unused}:{boot}"),
            false,
            false,
        ),
        (
            "dead-pid-zero",
            format!("ada@{host}.0:{boot}"),
            false,
            false,
        ),
        // A lock whose mtime predates its pid's process start (a reused pid)
        // still refuses: no clock decides.
        ("reused-pid", live_lock_text(), false, true),
    ];
    for (case, text, file, refused) in cases {
        let root = project(&format!("emacs-{case}"));
        stamp(&root, WIDGETS);
        let id = open_actions(&root)
            .into_iter()
            .find(|a| !a.1.is_empty())
            .unwrap()
            .1;
        let guard = item_guard(&root, WIDGETS, &id);
        let lock = emacs_lock(&root, WIDGETS, &text, file);
        if case == "reused-pid" {
            set_lock_mtime(&lock, 1_000_000_000);
            if !initial_pid_namespace() {
                eprintln!(
                    "reused-pid: in a child pid namespace the refusal comes before the pid check"
                );
            }
        }
        let before = std::fs::read(root.join(WIDGETS)).unwrap();
        let (code, out) = rivet(
            &root,
            "set",
            json!({"path": WIDGETS, "select": {"kind": "id", "id": id}, "guard": guard,
                   "content": "[x] **Ship the widget.** Written past a lock."}),
        );
        // In a child pid namespace no positive pid can be shown dead; pid 0
        // is dead everywhere.
        if refused || (!initial_pid_namespace() && case == "dead-pid") {
            assert_eq!(code, 2, "{case}: {out}");
            assert_eq!(out["error"]["code"], "editor_lock", "{case}: {out}");
            assert_eq!(out["error"]["path"], WIDGETS, "{case}");
            assert_eq!(
                std::fs::read(root.join(WIDGETS)).unwrap(),
                before,
                "{case}: a refused write leaves the file byte-identical"
            );
        } else {
            assert_eq!(code, 0, "{case}: {out}");
            assert!(read(&root, WIDGETS).contains("Written past a lock."));
        }
        assert!(rivet_temps(&root.join("tracking")).is_empty(), "{case}");
        // Rivet never removes or rewrites an editor's lock, dead or live.
        assert_eq!(lock_text(&lock), text, "{case}");
    }
    let message = |text: String| {
        let root = project("emacs-message");
        stamp(&root, WIDGETS);
        let id = open_actions(&root)
            .into_iter()
            .find(|a| !a.1.is_empty())
            .unwrap()
            .1;
        let guard = item_guard(&root, WIDGETS, &id);
        emacs_lock(&root, WIDGETS, &text, false);
        let (_, out) = rivet(
            &root,
            "set",
            json!({"path": WIDGETS, "select": {"kind": "id", "id": id}, "guard": guard,
                   "content": "[x] **Ship the widget.** Refused."}),
        );
        out["error"]["message"].as_str().unwrap().to_string()
    };
    let live = message(live_lock_text());
    assert!(
        live.contains(&format!("pid {pid} on this host"))
            && live.contains("tracking/.#TRACKING_WIDGETS.md")
            && live.contains("save or revert"),
        "{live}"
    );
    let foreign = message(format!("ada@elsewhere.invalid.{unused}"));
    assert!(
        foreign.contains("on another host, elsewhere.invalid"),
        "{foreign}"
    );
    let unreadable = message("not a lock".into());
    assert!(unreadable.contains("remove it"), "{unreadable}");
}

#[test]
fn a_lock_that_cannot_be_read_refuses_a_write() {
    let root = project("emacs-unreadable");
    stamp(&root, WIDGETS);
    let id = open_actions(&root)
        .into_iter()
        .find(|a| !a.1.is_empty())
        .unwrap()
        .1;
    let guard = item_guard(&root, WIDGETS, &id);
    std::fs::create_dir(root.join("tracking/.#TRACKING_WIDGETS.md")).unwrap();
    let before = std::fs::read(root.join(WIDGETS)).unwrap();
    let (code, out) = rivet(
        &root,
        "set",
        json!({"path": WIDGETS, "select": {"kind": "id", "id": id}, "guard": guard,
               "content": "[x] **Ship the widget.** Refused."}),
    );
    assert_eq!(code, 2, "{out}");
    assert_eq!(out["error"]["code"], "editor_lock");
    assert!(
        out["error"]["message"]
            .as_str()
            .unwrap()
            .contains("not a symlink or a regular file"),
        "{out}"
    );
    assert_eq!(std::fs::read(root.join(WIDGETS)).unwrap(), before);
    assert!(rivet_temps(&root.join("tracking")).is_empty());
    // A read that fails: a regular-file lock this process cannot open.
    std::fs::remove_dir(root.join("tracking/.#TRACKING_WIDGETS.md")).unwrap();
    let lock = emacs_lock(&root, WIDGETS, &live_lock_text(), true);
    std::fs::set_permissions(&lock, std::os::unix::fs::PermissionsExt::from_mode(0o000)).unwrap();
    if std::fs::read(&lock).is_ok() {
        eprintln!("skipped: mode 000 does not stop this process reading (root or DAC override)");
        return;
    }
    let (code, out) = rivet(
        &root,
        "set",
        json!({"path": WIDGETS, "select": {"kind": "id", "id": id}, "guard": guard,
               "content": "[x] **Ship the widget.** Refused."}),
    );
    assert_eq!(code, 2, "{out}");
    assert_eq!(out["error"]["code"], "editor_lock");
    assert!(
        out["error"]["message"]
            .as_str()
            .unwrap()
            .contains("Permission denied"),
        "{out}"
    );
    assert_eq!(std::fs::read(root.join(WIDGETS)).unwrap(), before);
    assert!(rivet_temps(&root.join("tracking")).is_empty());
}

#[test]
fn from_a_child_pid_namespace_only_a_pid_that_cannot_exist_is_dead() {
    // Rivet in a sandbox's own pid namespace cannot see the host's Emacs,
    // so `ESRCH` there proves nothing.
    let probe = Command::new("unshare").args(["-Urpf", "true"]).output();
    if !probe.is_ok_and(|o| o.status.success()) {
        eprintln!("skipped: unshare -Urpf is not available here");
        return;
    }
    let host = host_name();
    // (case, lock text, refused): a positive pid refuses, while pid 0 and
    // a pid out of range name no process in any namespace.
    for (case, text, refused) in [
        (
            "unused",
            format!("ada@{host}.{}:{}", unused_pid(), boot_time()),
            true,
        ),
        ("zero", format!("ada@{host}.0:1"), false),
        (
            "out-of-range",
            format!("ada@{host}.99999999999999:1"),
            false,
        ),
    ] {
        let root = project(&format!("emacs-pidns-{case}"));
        stamp(&root, WIDGETS);
        let id = open_actions(&root)
            .into_iter()
            .find(|a| !a.1.is_empty())
            .unwrap()
            .1;
        let guard = item_guard(&root, WIDGETS, &id);
        emacs_lock(&root, WIDGETS, &text, false);
        let before = std::fs::read(root.join(WIDGETS)).unwrap();
        let request = json!({"path": WIDGETS, "select": {"kind": "id", "id": id}, "guard": guard,
                             "content": "[x] **Ship the widget.** From a sandbox."});
        let mut command = Command::new("unshare");
        let mut child = common::without_debug_hooks(&mut command)
            .args(["-Urpf", env!("CARGO_BIN_EXE_markrivet"), "--root"])
            .arg(&root)
            .arg("set")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(request.to_string().as_bytes())
            .unwrap();
        let (code, out) = finish(child);
        if !refused {
            assert_eq!(code, 0, "{case}: {out}");
            assert!(read(&root, WIDGETS).contains("From a sandbox."), "{case}");
            continue;
        }
        assert_eq!(code, 2, "{case}: {out}");
        assert_eq!(out["error"]["code"], "editor_lock", "{case}: {out}");
        assert!(
            out["error"]["message"]
                .as_str()
                .unwrap()
                .contains("which this pid namespace cannot see"),
            "{case}: {out}"
        );
        assert_eq!(std::fs::read(root.join(WIDGETS)).unwrap(), before);
        // The same lock outside the namespace is dead, and the write lands.
        if initial_pid_namespace() {
            let (code, out) = rivet(&root, "set", request);
            assert_eq!(code, 0, "{case}: {out}");
        }
    }
}

#[test]
fn a_live_emacs_lock_refuses_a_create_of_a_file_not_yet_on_disk() {
    let root = project("emacs-create");
    let path = "tracking/TRACKING_NEW.md";
    let lock = emacs_lock(&root, path, &live_lock_text(), false);
    let (code, out) = rivet(&root, "create", json!({"path": path, "content": "# New\n"}));
    assert_eq!(code, 2, "{out}");
    assert_eq!(out["error"]["code"], "editor_lock");
    assert!(!root.join(path).exists());
    assert!(rivet_temps(&root.join("tracking")).is_empty());
    // Once Emacs lets go, the same create lands.
    std::fs::remove_file(&lock).unwrap();
    let (code, out) = rivet(&root, "create", json!({"path": path, "content": "# New\n"}));
    assert_eq!(code, 0, "{out}");
    assert_eq!(read(&root, path), "# New\n");
}

/// A listed file that does not parse still guards a `query` cursor by its
/// bytes, so an edit to it makes the continuation stale.
#[test]
fn an_unparsable_listed_file_guards_the_query_cursor() {
    let root = project("query-unparsable");
    let broken = "tracking/TRACKING_BROKEN.md";
    // A lone CR is refused by the parser, but the bytes were read.
    std::fs::write(root.join(broken), "# Broken\r\nlone\rCR\n").unwrap();
    let request = json!({"collection": "actions", "limit": 1});
    let (code, out) = rivet(&root, "query", request.clone());
    assert_eq!(code, 0, "{out}");
    assert!(
        out["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|d| d["path"] == broken),
        "{out}"
    );
    let mut next = request;
    next["cursor"] = out["next_cursor"].clone();
    let (code, out) = rivet(&root, "query", next.clone());
    assert_eq!(code, 0, "{out}");
    std::fs::write(root.join(broken), "# Broken\r\nlone\rCR, edited\n").unwrap();
    let (code, out) = rivet(&root, "query", next);
    assert_eq!(code, 2, "{out}");
    assert_eq!(out["error"]["code"], "stale_cursor");
}

// One-pass carrier verification (markstay §3.4) on a long list: refusals,
// byte preservation and attachment through the real binary.

const LONG: &str = "tracking/TRACKING_LONG.md";

/// A tracker whose Action list holds `n` one-line items; `text(i)` is item
/// `i`'s text after its title (items count from 1). Item `i` sits on line
/// `10 + i`.
fn long_tracker(n: usize, text: impl Fn(usize) -> String) -> String {
    let mut out = String::from(
        "---\nstatus: active\nentity: long\nowner: owner\n---\n\n# Long\n\n## Action\n\n",
    );
    for i in 1..=n {
        out.push_str(&format!("- [ ] **Item {i}.** {}\n", text(i)));
    }
    out.push_str("\n## Log\n\nNewest first.\n");
    out
}

fn long_root(name: &str, text: &str) -> std::path::PathBuf {
    let root = project(name);
    std::fs::write(root.join(LONG), text).unwrap();
    root
}

fn stamp_long(root: &std::path::Path) -> (i32, serde_json::Value) {
    let (_, map) = rivet(root, "map", json!({"path": LONG}));
    rivet(
        root,
        "stamp",
        json!({"path": LONG, "guard": map["revision"]}),
    )
}

/// Every stay in the file as `(id, what it attaches to)`.
fn stays(root: &std::path::Path) -> Vec<(String, String)> {
    let (code, map) = rivet(root, "map", json!({"path": LONG, "limit": 1000}));
    assert_eq!(code, 0, "{map}");
    let mut out = vec![];
    fn walk(v: &serde_json::Value, out: &mut Vec<(String, String)>) {
        match v {
            serde_json::Value::Object(o) => {
                if let (Some(id), Some(kind)) = (
                    o.get("id").and_then(|v| v.as_str()),
                    o.get("kind").and_then(|v| v.as_str()),
                ) {
                    out.push((id.to_string(), kind.to_string()));
                }
                o.values().for_each(|v| walk(v, out));
            }
            serde_json::Value::Array(a) => a.iter().for_each(|v| walk(v, out)),
            _ => {}
        }
    }
    walk(&map, &mut out);
    // `map`'s outline holds section and list stays; each item's stay is read
    // through the collection, as the id the item's title answers to.
    let (code, query) = rivet(
        root,
        "query",
        json!({"collection": "actions", "paths": [LONG], "limit": 1000}),
    );
    assert_eq!(code, 0, "{query}");
    for item in query["items"].as_array().unwrap() {
        if let Some(id) = item["id"].as_str() {
            let title = item["fields"]["title"].as_str().unwrap();
            out.push((id.to_string(), format!("item {title}")));
        }
    }
    out.sort();
    out
}

/// `(id, revision)` of the action titled `Item {i}.`, from one page of up to
/// 1000 (the default page is 50).
fn long_action(root: &std::path::Path, i: usize) -> (String, String) {
    let title = format!("Item {i}.");
    let (code, out) = rivet(
        root,
        "query",
        json!({"collection": "actions", "paths": [LONG], "limit": 1000}),
    );
    assert_eq!(code, 0, "{out}");
    let item = out["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|it| it["fields"]["title"] == title.as_str())
        .unwrap_or_else(|| panic!("no action {title}: {out}"));
    (
        item["id"].as_str().unwrap().to_string(),
        item["revision"].as_str().unwrap().to_string(),
    )
}

fn long_id(root: &std::path::Path, i: usize) -> String {
    long_action(root, i).0
}

/// Every digest value blanked, so text that differs only by a refreshed
/// digest compares equal.
fn without_digests(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find("sha256:") {
        let start = at + "sha256:".len();
        out.push_str(&rest[..start]);
        let hex = rest[start..]
            .bytes()
            .take_while(u8::is_ascii_hexdigit)
            .count();
        out.push_str(&"0".repeat(hex));
        rest = &rest[start + hex..];
    }
    out.push_str(rest);
    out
}

#[test]
fn stamp_on_a_long_list_refuses_from_the_offender_on_and_preserves_every_other_byte() {
    let original = long_tracker(300, |i| {
        if i == 150 {
            "carries a raw < here".into()
        } else {
            format!("text {i} with `<code>` inside")
        }
    });
    let root = long_root("carrier-long-stamp", &original);
    let offender = original.find("raw <").unwrap() + "raw ".len();
    let (code, out) = stamp_long(&root);
    assert_eq!(code, 0, "{out}");
    let op = &out["ops"][0];
    let minted: Vec<(&str, &str)> = op["minted"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| (m["id"].as_str().unwrap(), m["kind"].as_str().unwrap()))
        .collect();
    assert_eq!(minted.iter().filter(|m| m.1 == "item").count(), 149, "{op}");
    assert_eq!(minted.iter().filter(|m| m.1 == "list").count(), 1, "{op}");
    let refused = op["refused"].as_array().unwrap();
    let lines: Vec<u64> = refused
        .iter()
        .map(|r| r["line"].as_u64().unwrap())
        .collect();
    assert_eq!(
        lines,
        (160..=310).collect::<Vec<u64>>(),
        "items 150-300 refuse"
    );
    for r in refused {
        assert_eq!(r["code"], "stamp_refused", "{r}");
        assert_eq!(
            r["blocker"]["offset"].as_u64().unwrap() as usize,
            offender,
            "{r}"
        );
        assert_eq!(r["blocker"]["character"], "<", "{r}");
    }
    // Deleting exactly what stamp inserted gives the original back: each item
    // stay with the space before it, a section's or the list's stay with its
    // newline.
    let mut stamped = read(&root, LONG);
    for (id, kind) in &minted {
        let at = stamped.find(&format!("<!-- stay:{id} ")).unwrap();
        let end = at + stamped[at..].find("-->").unwrap() + 3;
        let separator = if *kind == "item" { ' ' } else { '\n' };
        assert_eq!(stamped[..at].chars().last(), Some(separator), "{id}");
        stamped.replace_range(at - 1..end, "");
    }
    assert_eq!(stamped, original);

    // The same request again, with a fresh guard: the same refusals, nothing
    // minted and nothing written. Blockers address the document as read, so
    // the offender is found in the stamped bytes.
    let before = read(&root, LONG);
    let offender = before.find("raw <").unwrap() + "raw ".len();
    let (code, again) = stamp_long(&root);
    assert_eq!(code, 0, "{again}");
    let op = &again["ops"][0];
    assert!(op["minted"].as_array().is_none_or(|m| m.is_empty()), "{op}");
    let refused = op["refused"].as_array().unwrap();
    assert_eq!(refused.len(), 151, "{op}");
    for r in refused {
        assert_eq!(
            r["blocker"]["offset"].as_u64().unwrap() as usize,
            offender,
            "{r}"
        );
    }
    assert_eq!(read(&root, LONG), before, "a refused stamp writes nothing");
}

#[test]
fn a_write_that_puts_an_offender_before_later_stays_is_refused_byte_for_byte() {
    let root = long_root(
        "carrier-long-refuse",
        &long_tracker(300, |i| format!("text {i}")),
    );
    let (code, out) = stamp_long(&root);
    assert_eq!(code, 0, "{out}");
    let before = read(&root, LONG);
    let attached = stays(&root);
    assert_eq!(
        attached.len(),
        300 + 4,
        "300 item stays and 4 outline stays"
    );
    let id = long_id(&root, 100);
    // A whole-item rewrite whose new text carries a raw '<' ahead of 200
    // stamped items.
    let (code, out) = rivet(
        &root,
        "set",
        json!({"path": LONG, "select": {"kind": "id", "id": id}, "guard": item_guard(&root, LONG, &id),
               "content": "[ ] **Item 100.** now carries a raw < here"}),
    );
    assert_ne!(code, 0, "{out}");
    assert_eq!(out["error"]["code"], "stamp_refused", "{out}");
    assert_eq!(read(&root, LONG), before, "refused set changes nothing");
    // The typed write of the same text refuses the same way.
    let (id, revision) = long_action(&root, 100);
    let (code, out) = rivet(
        &root,
        "update-item",
        json!({"path": LONG, "collection": "actions", "id": id, "guard": revision,
               "set": {"title": "Item 100 <b>"}}),
    );
    assert_ne!(code, 0, "{out}");
    assert_eq!(out["error"]["code"], "stamp_refused", "{out}");
    assert_eq!(
        read(&root, LONG),
        before,
        "refused update-item changes nothing"
    );
    assert_eq!(stays(&root), attached, "every stay keeps its attachment");
}

#[test]
fn a_rewrite_behind_masked_code_spans_lands_and_changes_nothing_else() {
    let root = long_root(
        "carrier-long-accept",
        &long_tracker(300, |i| format!("text {i} with `<code>` and `a\\b` spans")),
    );
    let (code, out) = stamp_long(&root);
    assert_eq!(code, 0, "{out}");
    let before = read(&root, LONG);
    let attached = stays(&root);
    let id = long_id(&root, 299);
    let (code, out) = rivet(
        &root,
        "set",
        json!({"path": LONG, "select": {"kind": "id", "id": id}, "guard": item_guard(&root, LONG, &id),
               "content": "[ ] **Item 299.** rewritten with `<new>` code"}),
    );
    assert_eq!(code, 0, "{out}");
    let after = read(&root, LONG);
    assert!(after.contains("rewritten with `<new>` code"));
    // Outside item 299's own line, only refreshed digest values may differ.
    let line = |text: &str| {
        let at = text.find("**Item 299.**").unwrap();
        let start = text[..at].rfind('\n').unwrap() + 1;
        let end = at + text[at..].find('\n').unwrap();
        (start, end)
    };
    let ((bs, be), (as_, ae)) = (line(&before), line(&after));
    assert_eq!(
        without_digests(&before[..bs]),
        without_digests(&after[..as_])
    );
    assert_eq!(
        without_digests(&before[be..]),
        without_digests(&after[ae..])
    );
    assert_eq!(stays(&root), attached, "every stay keeps its attachment");
    let (_, doctor) = rivet(&root, "doctor", json!({}));
    assert!(
        !doctor.to_string().contains("evidence_drift"),
        "refreshed digests are current: {doctor}"
    );
}

#[test]
fn a_fenced_offender_still_refuses_a_rewrite_but_not_a_checkbox_tick() {
    let root = long_root(
        "carrier-long-fenced",
        &long_tracker(300, |i| format!("text {i}")),
    );
    let (code, out) = stamp_long(&root);
    assert_eq!(code, 0, "{out}");
    // A person adds a fenced block holding '<' to item 10 after stamping.
    // Fenced content is visible to §3.4, so every later stay now sits behind
    // a non-plain carrier.
    let stamped = read(&root, LONG);
    let fenced = stamped.replacen(
        "\n- [ ] **Item 11.**",
        "\n\n  ```\n  a < b\n  ```\n- [ ] **Item 11.**",
        1,
    );
    assert_ne!(fenced, stamped);
    std::fs::write(root.join(LONG), &fenced).unwrap();
    let attached = stays(&root);
    let id = long_id(&root, 299);
    // A rewrite moves item 299's stay, which is judged as an insertion.
    let (code, out) = rivet(
        &root,
        "set",
        json!({"path": LONG, "select": {"kind": "id", "id": id}, "guard": item_guard(&root, LONG, &id),
               "content": "[ ] **Item 299.** rewritten"}),
    );
    assert_ne!(code, 0, "{out}");
    assert_eq!(out["error"]["code"], "stamp_refused", "{out}");
    assert_eq!(read(&root, LONG), fenced, "refused rewrite changes nothing");
    assert_eq!(stays(&root), attached, "every stay keeps its attachment");
    // A checkbox tick splices one byte and moves no stay, so it lands.
    let (id, revision) = long_action(&root, 299);
    let (code, out) = rivet(
        &root,
        "update-item",
        json!({"path": LONG, "collection": "actions", "id": id, "guard": revision,
               "set": {"done": true}}),
    );
    assert_eq!(code, 0, "{out}");
    let ticked = fenced.replacen("- [ ] **Item 299.**", "- [x] **Item 299.**", 1);
    assert_eq!(
        without_digests(&read(&root, LONG)),
        without_digests(&ticked)
    );
    assert_eq!(stays(&root), attached);
}
