mod common;
use common::*;
use serde_json::{Value, json};
use std::path::Path;

const FILE: &str = "tracking/TRACKING_FIELDS.md";

fn key_guard(root: &Path, key: &str) -> Value {
    let (code, got) = rivet(
        root,
        "get",
        json!({"path": FILE, "select": {"kind": "frontmatter_key", "key": key}}),
    );
    assert_eq!(code, 0, "{got}");
    got["revision"].clone()
}

#[test]
fn documented_mapping_and_delete_commands_preserve_bytes_and_check_their_own_guards() {
    let root = project("frontmatter-write-commands");
    for mapping in [
        "generated: {by: 'process:example', at: 'old'} # kept",
        "generated:\n  by: 'process:example' # kept\n  at: 'old' # date",
    ] {
        let source = format!(
            "---\n{mapping}\nretired: old # removed with key\nstatus: active\n---\n\n# Widgets\n\n- [ ] Synthetic action.\n"
        );
        std::fs::write(root.join(FILE), source).unwrap();
        stamp(&root, FILE);
        let before = read(&root, FILE);
        let guard = key_guard(&root, "generated");
        // This request is the documented set-key-field example.
        let request = json!({"path": FILE, "key_path": ["generated", "at"], "value": "2026-11-02T12:00:00+13:00", "guard": guard});
        let mut preview = request.clone();
        preview["preview"] = json!(true);
        let (code, out) = rivet(&root, "set-key-field", preview);
        assert_eq!(code, 0, "{out}");
        assert_eq!(read(&root, FILE), before);
        let (code, out) = rivet(&root, "set-key-field", request.clone());
        assert_eq!(code, 0, "{out}");
        let after = before.replacen("'old'", "'2026-11-02T12:00:00+13:00'", 1);
        assert_eq!(read(&root, FILE), after);
        let (code, out) = rivet(&root, "set-key-field", request);
        assert_eq!(code, 2, "{out}");
        assert_eq!(out["error"]["code"], "guard_conflict");
        assert_eq!(read(&root, FILE), after);
        // A fresh no-op still verifies the target and returns unchanged.
        let (code, out) = rivet(
            &root,
            "set-key-field",
            json!({"path": FILE, "key_path": ["generated", "at"], "value": "2026-11-02T12:00:00+13:00", "guard": key_guard(&root, "generated")}),
        );
        assert_eq!(code, 0, "{out}");
        assert_eq!(out["status"], "unchanged", "{out}");
        let retired_guard = key_guard(&root, "retired");
        // A neighbouring field guard and the document guard cannot authorize deletion.
        let (_, map) = rivet(&root, "map", json!({"path": FILE}));
        for wrong_guard in [key_guard(&root, "generated"), map["revision"].clone()] {
            let (code, out) = rivet(
                &root,
                "delete-key",
                json!({"path": FILE, "key": "retired", "guard": wrong_guard}),
            );
            assert_eq!(code, 2, "{out}");
            assert_eq!(read(&root, FILE), after);
        }
        // This request is the documented delete-key example.
        let request = json!({"path": FILE, "key": "retired", "guard": retired_guard});
        let (code, out) = rivet(&root, "delete-key", request.clone());
        assert_eq!(code, 0, "{out}");
        let after = after.replace("retired: old # removed with key\n", "");
        assert_eq!(read(&root, FILE), after);
        let (code, out) = rivet(&root, "delete-key", request);
        assert_eq!(code, 2, "{out}");
        assert_eq!(out["error"]["code"], "missing_selector");
        assert_eq!(read(&root, FILE), after);
    }
}

#[test]
fn refused_frontmatter_writes_leave_files_byte_identical() {
    let root = project("frontmatter-write-refusals");
    for (mapping, key_path, value, expected) in [
        (
            "generated: scalar",
            json!(["generated", "at"]),
            json!("new"),
            "unsupported_syntax",
        ),
        (
            "generated: [a, b]",
            json!(["generated", "at"]),
            json!("new"),
            "unsupported_syntax",
        ),
        (
            "generated: {by: synthetic}",
            json!(["generated", "at"]),
            json!("new"),
            "missing_selector",
        ),
        (
            "generated: {at: old}",
            json!(["generated", "at", "deeper"]),
            json!("new"),
            "invalid_request",
        ),
        (
            "generated: {at: old}",
            json!(["generated", "at"]),
            json!({"x": "new"}),
            "unsupported_syntax",
        ),
        (
            "generated:\n  at:",
            json!(["generated", "at"]),
            json!("new"),
            "unsupported_syntax",
        ),
        (
            "generated: {at: old}",
            json!(["generated", "at"]),
            json!("new\nline"),
            "unsupported_syntax",
        ),
    ] {
        let before = format!("---\n{mapping}\nstatus: active\n---\n# Widgets\n");
        std::fs::write(root.join(FILE), &before).unwrap();
        let (code, out) = rivet(
            &root,
            "set-key-field",
            json!({"path": FILE, "key_path": key_path, "value": value, "guard": key_guard(&root, "generated")}),
        );
        assert_eq!(code, 2, "{out}");
        assert_eq!(out["error"]["code"], expected);
        assert_eq!(read(&root, FILE), before);
    }
    // Unsupported YAML cannot be used to target either operation, including deletion.
    for mapping in [
        "generated: &parent {at: old}",
        "generated: {at: &date old, copy: *date}",
        "generated: {at: *date}",
    ] {
        let before = format!("---\n{mapping}\nstatus: active\n---\n# Widgets\n");
        std::fs::write(root.join(FILE), &before).unwrap();
        for (command, request) in [
            (
                "set-key-field",
                json!({"path": FILE, "key_path": ["generated", "at"], "value": "new", "guard": "unused"}),
            ),
            (
                "delete-key",
                json!({"path": FILE, "key": "generated", "guard": "unused"}),
            ),
        ] {
            let (code, out) = rivet(&root, command, request);
            assert_eq!(code, 2, "{out}");
            assert_eq!(out["error"]["code"], "unsupported_syntax");
            assert_eq!(read(&root, FILE), before);
        }
    }
    let before = "---\nretired: old\n---\n# Widgets\n";
    std::fs::write(root.join(FILE), before).unwrap();
    let (code, out) = rivet(
        &root,
        "delete-key",
        json!({"path": FILE, "key": "retired", "guard": key_guard(&root, "retired")}),
    );
    assert_eq!(code, 2, "{out}");
    assert_eq!(out["error"]["code"], "unsupported_syntax");
    assert_eq!(read(&root, FILE), before);
}

#[test]
fn deletion_preserves_comments_even_when_the_previous_collection_absorbs_them() {
    let root = project("frontmatter-delete-trailing-comments");
    for newline in ["\n", "\r\n"] {
        let before = "---\nprevious:\n  - a\ngenerated:\n  by: synthetic\n  at: old\n  # trailing comment\n# another comment\nstatus: active\n---\n# Widgets\n";
        let before = before.replace('\n', newline);
        std::fs::write(root.join(FILE), &before).unwrap();
        let (code, out) = rivet(
            &root,
            "delete-key",
            json!({
                "path": FILE, "key": "generated", "guard": key_guard(&root, "generated")
            }),
        );
        assert_eq!(code, 0, "{out}");
        let removed = "generated:\n  by: synthetic\n  at: old\n".replace('\n', newline);
        assert_eq!(read(&root, FILE), before.replacen(&removed, "", 1));
    }
}

// ---------------------------------------------------------------------------
// add_key_yaml and set_key_yaml through the generic `write` route

/// The usage section for the raw YAML writes, so its examples run verbatim.
fn yaml_usage() -> String {
    let usage =
        std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/usage.md"))
            .unwrap();
    let start = usage.find("### `add_key_yaml` and `set_key_yaml`").unwrap();
    let end = start + usage[start..].find("### Repairs").unwrap();
    usage[start..end].to_string()
}

/// Requests fed to `markrivet write` through the section's heredocs.
fn documented_writes(section: &str) -> Vec<Value> {
    section
        .split("markrivet write <<'EOF'\n")
        .skip(1)
        .map(|rest| serde_json::from_str(&rest[..rest.find("\nEOF").unwrap()]).unwrap())
        .collect()
}

/// `echo '<request>' | markrivet <command>` lines, as (command, request).
fn documented_reads(section: &str) -> Vec<(String, Value)> {
    section
        .lines()
        .filter_map(|l| {
            let rest = l.strip_prefix("echo '")?;
            let (request, command) = rest.split_once("' | markrivet ")?;
            Some((command.to_string(), serde_json::from_str(request).unwrap()))
        })
        .collect()
}

/// The op lines of the section's JSON block.
fn documented_ops(section: &str) -> Vec<Value> {
    section
        .lines()
        .filter(|l| l.starts_with("{\"op\": "))
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

fn with_guard(mut request: Value, guard: &Value) -> Value {
    for op in request["ops"].as_array_mut().unwrap() {
        assert!(op["guard"].as_str().unwrap().starts_with('<'));
        op["guard"] = guard.clone();
    }
    request
}

fn doc_revision(root: &Path) -> Value {
    let (code, map) = rivet(root, "map", json!({"path": FILE}));
    assert_eq!(code, 0, "{map}");
    map["revision"].clone()
}

/// Every file under `root`, so a refusal or preview can be shown to leave
/// the whole project untouched.
fn snapshot(root: &Path) -> Vec<(std::path::PathBuf, Vec<u8>)> {
    let mut out = vec![];
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                stack.push(path);
            } else {
                out.push((path.clone(), std::fs::read(&path).unwrap()));
            }
        }
    }
    out.sort();
    out
}

#[test]
fn yaml_documented_examples_run_through_write_and_keep_every_other_byte() {
    let section = yaml_usage();
    let writes = documented_writes(&section);
    assert_eq!(writes.len(), 2);
    let ops = documented_ops(&section);
    assert_eq!(ops.len(), 2);
    let reads = documented_reads(&section);
    assert_eq!(reads.len(), 2);
    let root = project("frontmatter-yaml-examples");
    for nl in ["\n", "\r\n"] {
        let source = "---\nscope: 'old scope' # old tail\nstatus: active\n---\n\n# Widgets\n\n- [ ] Synthetic action.\n".replace('\n', nl);
        std::fs::write(root.join(FILE), &source).unwrap();
        stamp(&root, FILE);
        let before = read(&root, FILE);
        for (command, request) in &reads {
            let (code, out) = rivet(&root, command, request.clone());
            assert_eq!(code, 0, "{command}: {out}");
        }
        // add_key_yaml, previewed first.
        let add = with_guard(writes[0].clone(), &doc_revision(&root));
        let mut preview = add.clone();
        preview["preview"] = json!(true);
        let files = snapshot(&root);
        let (code, out) = rivet(&root, "write", preview);
        assert_eq!(code, 0, "{out}");
        assert_eq!(out["status"], "preview");
        assert_eq!(snapshot(&root), files);
        let (code, out) = rivet(&root, "write", add);
        assert_eq!(code, 0, "{out}");
        let fence = format!("{nl}---{nl}{nl}# Widgets");
        let added = before.replacen(&fence, &format!("{nl}lenses: [a, b]{fence}"), 1);
        assert_eq!(read(&root, FILE), added);
        let (_, got) = rivet(
            &root,
            "get",
            json!({"path": FILE, "select": {"kind": "frontmatter_key", "key": "lenses"}}),
        );
        assert_eq!(got["value"], json!(["a", "b"]), "a list, not a string");
        assert_eq!(out["ops"][0]["revision"], got["revision"]);
        // set_key_yaml with replace_line.
        let set = with_guard(writes[1].clone(), &key_guard(&root, "scope"));
        let (code, out) = rivet(&root, "write", set.clone());
        assert_eq!(code, 0, "{out}");
        let after = added.replacen("scope: 'old scope' # old tail", "scope: 'new scope'", 1);
        assert_eq!(read(&root, FILE), after);
        assert_eq!(out["ops"][0]["revision"], key_guard(&root, "scope"));
        let (code, out) = rivet(&root, "write", set.clone());
        assert_eq!(code, 2, "{out}");
        assert_eq!(out["error"]["code"], "guard_conflict");
        assert_eq!(read(&root, FILE), after);
        let fresh = with_guard(writes[1].clone(), &key_guard(&root, "scope"));
        let (code, out) = rivet(&root, "write", fresh);
        assert_eq!(code, 0, "{out}");
        assert_eq!(out["status"], "unchanged");
        assert_eq!(read(&root, FILE), after);
        // Without replace_line the tail stays.
        std::fs::write(root.join(FILE), &before).unwrap();
        let mut keep = with_guard(writes[1].clone(), &key_guard(&root, "scope"));
        keep["ops"][0]
            .as_object_mut()
            .unwrap()
            .remove("replace_line");
        let (code, out) = rivet(&root, "write", keep);
        assert_eq!(code, 0, "{out}");
        assert_eq!(
            read(&root, FILE),
            before.replacen("'old scope'", "'new scope'", 1)
        );
        // The JSON block's op lines are the same operations.
        std::fs::write(root.join(FILE), &before).unwrap();
        for op in &ops {
            let guard = if op["op"] == "add_key_yaml" {
                doc_revision(&root)
            } else {
                key_guard(&root, op["key"].as_str().unwrap())
            };
            let request = with_guard(json!({"path": FILE, "ops": [op]}), &guard);
            let (code, out) = rivet(&root, "write", request);
            assert_eq!(code, 0, "{out}");
        }
        assert_eq!(read(&root, FILE), after);
    }
}

#[test]
fn yaml_invalid_shapes_exit_1_and_refusals_leave_the_project_untouched() {
    let root = project("frontmatter-yaml-refusals");
    let source = "---\nscope: 'v' # a\nlenses: [a, b]\ngenerated: {by: x}\nsources:\n  - one\n'k\u{feff}': plain\n---\n# Widgets\n\n- [ ] Synthetic action.\n";
    std::fs::write(root.join(FILE), source).unwrap();
    stamp(&root, FILE);
    let before = read(&root, FILE);
    let files = snapshot(&root);
    let doc = doc_revision(&root);
    let scope = key_guard(&root, "scope");
    let invalid = [
        json!({"op": "add_key_yaml", "key": "added", "yaml": 1, "guard": doc}),
        json!({"op": "add_key_yaml", "key": "added", "yaml": ["a"], "guard": doc}),
        json!({"op": "add_key_yaml", "key": "added", "guard": doc}),
        json!({"op": "add_key_yaml", "key": 1, "yaml": "x", "guard": doc}),
        json!({"op": "add_key_yaml", "key": "added", "yaml": "x", "guard": doc, "replace_line": true}),
        json!({"op": "add_key_yaml", "key": "added", "yaml": "x", "value": "x", "guard": doc}),
        json!({"op": "set_key_yaml", "select": {"kind": "frontmatter_key", "key": "scope"}, "yaml": "x", "guard": scope}),
        json!({"op": "set_key_yaml", "key": "scope", "yaml": "x", "guard": scope, "replace_line": "yes"}),
        json!({"op": "set_key_yaml", "key": "scope", "yaml": "x"}),
    ];
    for op in invalid {
        let (code, out) = rivet(&root, "write", json!({"path": FILE, "ops": [op]}));
        assert_eq!(code, 1, "{op}: {out}");
        assert_eq!(out["error"]["code"], "invalid_request", "{op}");
        assert_eq!(snapshot(&root), files, "{op}");
    }
    // No shorthand command exists; the generic write route carries both ops.
    for command in ["add-key-yaml", "set-key-yaml"] {
        let (code, out) = rivet(
            &root,
            command,
            json!({"path": FILE, "key": "scope", "yaml": "x", "guard": scope}),
        );
        assert_eq!(code, 1, "{out}");
        assert_eq!(snapshot(&root), files);
    }
    let lenses = key_guard(&root, "lenses");
    let mut refused = vec![
        (
            json!({"op": "set_key_yaml", "key": "absent", "yaml": "x", "guard": scope}),
            "missing_selector",
        ),
        (
            json!({"op": "add_key_yaml", "key": "scope", "yaml": "x", "guard": doc}),
            "already_exists",
        ),
        (
            json!({"op": "add_key_yaml", "key": "a b", "yaml": "x", "guard": doc}),
            "invalid_request",
        ),
        (
            json!({"op": "set_key_yaml", "key": "scope", "yaml": "x", "guard": doc}),
            "guard_conflict",
        ),
        (
            json!({"op": "set_key_yaml", "key": "scope", "yaml": "x", "guard": lenses}),
            "guard_conflict",
        ),
        (
            json!({"op": "add_key_yaml", "key": "added", "yaml": "x", "guard": scope}),
            "guard_conflict",
        ),
        (
            json!({"op": "set_key_yaml", "key": "k\u{feff}", "yaml": "[a]", "guard": key_guard(&root, "k\u{feff}")}),
            "unsupported_syntax",
        ),
    ];
    for key in ["generated", "sources"] {
        refused.push((
            json!({"op": "set_key_yaml", "key": key, "yaml": "x", "guard": key_guard(&root, key)}),
            "unsupported_syntax",
        ));
    }
    for token in [
        "", " a", "a # c", "[a] x", "{}", "[[a]]", "&x a", "a\nb", "~", "0x10",
    ] {
        refused.push((
            json!({"op": "set_key_yaml", "key": "scope", "yaml": token, "guard": scope}),
            "unsupported_syntax",
        ));
        refused.push((
            json!({"op": "add_key_yaml", "key": "added", "yaml": token, "guard": doc}),
            "unsupported_syntax",
        ));
    }
    for (op, expected) in refused {
        for preview in [false, true] {
            let (code, out) = rivet(
                &root,
                "write",
                json!({"path": FILE, "ops": [op], "preview": preview}),
            );
            assert_eq!(code, 2, "{op}: {out}");
            assert_eq!(out["error"]["code"], expected, "{op}");
            assert_eq!(snapshot(&root), files, "{op}");
        }
    }
    // The odd quoted key keeps a scalar.
    let (code, out) = rivet(
        &root,
        "write",
        json!({"path": FILE, "ops": [{"op": "set_key_yaml", "key": "k\u{feff}", "yaml": "'b'", "guard": key_guard(&root, "k\u{feff}")}]}),
    );
    assert_eq!(code, 0, "{out}");
    assert_eq!(read(&root, FILE), before.replace(": plain", ": 'b'"));
}

#[test]
fn yaml_standalone_probe_passes_against_the_built_binary() {
    let script =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tools/probe-frontmatter-yaml.py");
    let binary = std::fs::canonicalize(env!("CARGO_BIN_EXE_markrivet")).unwrap();
    let mut command = std::process::Command::new("python3");
    without_debug_hooks(&mut command);
    let out = command
        .arg(&script)
        .arg("--rivet")
        .arg(&binary)
        .arg("--json")
        .output()
        .unwrap();
    let report: Value = serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!(
            "probe printed no JSON ({e}): {}",
            String::from_utf8_lossy(&out.stderr)
        )
    });
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        serde_json::to_string(&report["failures"]).unwrap()
    );
    assert_eq!(report["ok"], true);
    assert!(report["assertions_executed"].as_u64().unwrap() > 0);
    assert_eq!(report["assertions_failed"], 0);
    assert_eq!(report["binary_before"], report["binary_after"]);
}

// append_key_item

fn append_usage() -> String {
    let usage =
        std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/usage.md"))
            .unwrap();
    let start = usage.find("### `append_key_item`").unwrap();
    let end = start + usage[start..].find("### `add_key_yaml`").unwrap();
    usage[start..end].to_string()
}

/// A synthetic tracker shaped like a store's OSS tracker: a flow `generated`
/// mapping and a block `verified` list of flow `{by, at}` items.
const VERIFIED: &str = "---\nentity: Synthetic project # e\ngenerated: {by: 'process:example', at: '2026-11-01T09:00:00+13:00'}\nverified:\n  - {by: 'claude/a', at: '2026-11-01T10:00:00+13:00'}\nstatus: active\n---\n\n# Widgets\n\n## Action\n\n- [ ] Synthetic action.\n";

#[test]
fn append_key_item_documented_example_appends_one_line_and_checks_its_guard() {
    let section = append_usage();
    let reads = documented_reads(&section);
    assert_eq!(reads.len(), 2);
    let ops = documented_ops(&section);
    assert_eq!(ops.len(), 1);
    let root = project("frontmatter-append-example");
    for nl in ["\n", "\r\n"] {
        std::fs::write(root.join(FILE), VERIFIED.replace('\n', nl)).unwrap();
        stamp(&root, FILE);
        let before = read(&root, FILE);
        let (command, get) = &reads[0];
        let (code, got) = rivet(&root, command, get.clone());
        assert_eq!(code, 0, "{got}");
        let (command, request) = &reads[1];
        assert_eq!(command, "append-key-item");
        let mut request = request.clone();
        assert!(request["guard"].as_str().unwrap().starts_with('<'));
        request["guard"] = got["revision"].clone();
        let mut preview = request.clone();
        preview["preview"] = json!(true);
        let files = snapshot(&root);
        let (code, out) = rivet(&root, command, preview);
        assert_eq!(code, 0, "{out}");
        assert_eq!(out["status"], "preview");
        assert_eq!(snapshot(&root), files);
        let (code, out) = rivet(&root, command, request.clone());
        assert_eq!(code, 0, "{out}");
        let first = "  - {by: 'claude/a', at: '2026-11-01T10:00:00+13:00'}";
        let after = before.replacen(
            &format!("{first}{nl}"),
            &format!("{first}{nl}  - {{by: 'claude/b', at: '2026-11-21T10:00:00+13:00'}}{nl}"),
            1,
        );
        assert_ne!(after, before);
        assert_eq!(read(&root, FILE), after);
        assert_eq!(out["ops"][0]["revision"], key_guard(&root, "verified"));
        let (_, got) = rivet(&root, "get", get.clone());
        assert_eq!(
            got["value"][1],
            json!({"by": "claude/b", "at": "2026-11-21T10:00:00+13:00"})
        );
        // A replay with the old guard is stale and changes nothing.
        let (code, out) = rivet(&root, command, request);
        assert_eq!(code, 2, "{out}");
        assert_eq!(out["error"]["code"], "guard_conflict");
        assert_eq!(read(&root, FILE), after);
        // The JSON block's op, through `write`, appends the same item again.
        let (code, out) = rivet(
            &root,
            "write",
            with_guard(
                json!({"path": FILE, "ops": [ops[0].clone()]}),
                &key_guard(&root, "verified"),
            ),
        );
        assert_eq!(code, 0, "{out}");
        assert_eq!(
            read(&root, FILE)
                .matches("{by: 'claude/example', at: '2026-11-21T10:00:00+13:00'}")
                .count(),
            1
        );
    }
}

#[test]
fn append_key_item_refusals_leave_the_project_untouched() {
    let root = project("frontmatter-append-refusals");
    let source = "---\nscalar: x\nmapping: {by: a}\nempty:\nscalars: [a, b]\nmixed:\n  - {by: a}\n  - b\nwrapped: [{by: a},\n  {by: b}]\nverified: [{by: a, at: b}]\n---\n# Widgets\n\n- [ ] Synthetic action.\n";
    std::fs::write(root.join(FILE), source).unwrap();
    stamp(&root, FILE);
    let files = snapshot(&root);
    let good = json!({"by": "c", "at": "d"});
    let refuse = |key: &str, item: Value, guard: Value, code: i32, want: &str| {
        let (got, out) = rivet(
            &root,
            "append-key-item",
            json!({"path": FILE, "key": key, "item": item, "guard": guard}),
        );
        assert_eq!(got, code, "{key}: {out}");
        if code == 2 {
            assert_eq!(out["error"]["code"], want, "{key}: {out}");
        }
        assert_eq!(snapshot(&root), files, "{key}");
    };
    for key in ["scalar", "mapping", "empty", "scalars", "mixed", "wrapped"] {
        refuse(
            key,
            good.clone(),
            key_guard(&root, key),
            2,
            "unsupported_syntax",
        );
    }
    for item in [json!({}), json!({"by": ["x"]}), json!({"true": "x"})] {
        refuse(
            "verified",
            item,
            key_guard(&root, "verified"),
            2,
            "invalid_request",
        );
    }
    refuse(
        "verified",
        json!({"by": "x\ny"}),
        key_guard(&root, "verified"),
        2,
        "unsupported_syntax",
    );
    // Not a mapping at all: the request does not parse, exit 1.
    refuse(
        "verified",
        json!("by: x"),
        key_guard(&root, "verified"),
        1,
        "",
    );
    refuse(
        "verified",
        json!([{"by": "x"}]),
        key_guard(&root, "verified"),
        1,
        "",
    );
    // Guards of the wrong kind or key.
    refuse(
        "verified",
        good.clone(),
        doc_revision(&root),
        2,
        "guard_conflict",
    );
    refuse(
        "verified",
        good.clone(),
        key_guard(&root, "scalar"),
        2,
        "guard_conflict",
    );
    refuse(
        "absent",
        good.clone(),
        key_guard(&root, "verified"),
        2,
        "guard_conflict",
    );
}

#[test]
fn append_key_item_commits_in_per_write_mode_like_other_key_writes() {
    let root = project("frontmatter-append-commit");
    let home = root.with_extension("home");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(root.join(FILE), VERIFIED).unwrap();
    stamp(&root, FILE);
    let git = |args: &[&str]| {
        let out = git_command(&root, &home, args).output().unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    };
    std::fs::write(
        root.join(".rivet/.gitignore"),
        "/lock\n/seq\n/pending/\n/commits.jsonl\n/push.jsonl\n",
    )
    .unwrap();
    let config_path = root.join(".rivet/project.json");
    let mut config: Value = serde_json::from_slice(&std::fs::read(&config_path).unwrap()).unwrap();
    config["git"] = json!({"commit": "per-write", "on_dirty": "refuse"});
    std::fs::write(&config_path, config.to_string()).unwrap();
    git(&["init", "-q", "-b", "main"]);
    git(&["config", "user.name", "Tester"]);
    git(&["config", "user.email", "tester@example.invalid"]);
    git(&["add", "-A"]);
    git(&["commit", "-qm", "synthetic fixture"]);
    let env = [
        ("HOME", home.display().to_string()),
        ("XDG_CONFIG_HOME", home.display().to_string()),
    ];
    let run = |cmd: &str, request: Value| finish(spawn(&root, cmd, &request, &env));
    let (_, got) = run(
        "get",
        json!({"path": FILE, "select": {"kind": "frontmatter_key", "key": "verified"}}),
    );
    let (code, out) = run(
        "append-key-item",
        json!({"path": FILE, "key": "verified", "item": {"by": "claude/b", "at": "2026-11-21T10:00:00+13:00"}, "guard": got["revision"], "message": "verify synthetic"}),
    );
    assert_eq!(code, 0, "{out}");
    let message = git(&["log", "-1", "--format=%B"]);
    assert!(message.starts_with("verify synthetic"), "{message}");
    assert!(message.contains("Rivet-Op:"), "{message}");
    assert_eq!(git(&["status", "--porcelain", "--", FILE]), "");
    assert!(
        git(&["show", &format!("HEAD:{FILE}")])
            .contains("  - {by: 'claude/b', at: '2026-11-21T10:00:00+13:00'}\n")
    );
}
