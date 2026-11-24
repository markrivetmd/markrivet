mod common;
use common::*;
use serde_json::{Value, json};
use std::path::Path;

const SOURCE: &str = "# Sample\n\n## Action\n\n- [ ] **First**\n\n## Notes\n\nUntouched prose.\n";

fn configure(root: &Path, files: &[&str], archive: &[&str]) {
    std::fs::write(
        root.join(".rivet/project.json"),
        json!({"collections":{"actions":{
            "schema":"tracker-action@1", "files":files, "archive":archive,
            "section":"Action", "level":2
        }}})
        .to_string(),
    )
    .unwrap();
}

fn git(root: &Path, args: &[&str]) {
    let home = root.with_extension("isolated-home");
    std::fs::create_dir_all(&home).unwrap();
    let out = git_command(
        root,
        &home,
        &[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
        ],
    )
    .args(args)
    .output()
    .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn has_literal_error(out: &Value) -> bool {
    std::iter::once(&out["error"])
        .chain(out["diagnostics"].as_array().into_iter().flatten())
        .any(|d| {
            d["message"]
                .as_str()
                .is_some_and(|m| m.contains("literal binding"))
        })
}

#[test]
fn literal_bindings_are_refused_consistently_and_preserve_every_byte() {
    let root = project("literal-skipped-bindings");
    git(&root, &["init", "-q"]);
    git(&root, &["commit", "-qm", "fixture", "--allow-empty"]);
    for path in [
        ".hidden/tasks.md",
        "target/tasks.md",
        "node_modules/tasks.md",
        "records/.hidden/nested/tasks.md",
        "records/target/nested/tasks.md",
        "records/node_modules/nested/tasks.md",
        "records/.tasks.md",
    ] {
        let file = root.join(path);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, SOURCE).unwrap();
        // Mint real stays before installing the rejected binding.
        configure(&root, &["tracking/*.md"], &[]);
        stamp(&root, path);
        let (_, mapped) = rivet(&root, "map", json!({"path":path}));
        let (_, section) = rivet(
            &root,
            "get",
            json!({
                "path":path, "select":{"kind":"heading_path","path":["Sample","Notes"]}
            }),
        );
        let before = std::fs::read(&file).unwrap();
        git(&root, &["add", "-f", "--", path]);
        git(&root, &["commit", "-qm", "tracked sample"]);

        for archive in [false, true] {
            if archive {
                configure(&root, &["tracking/*.md"], &[path]);
            } else {
                configure(&root, &[path], &[]);
            }
            for (cmd, request) in [
                ("map", json!({"path":path})),
                ("get", json!({"path":path,"select":{"kind":"document"}})),
                (
                    "get",
                    json!({"path":path,"select":{"kind":"heading_path","path":["Sample","Action"]}}),
                ),
                ("stamp", json!({"path":path,"guard":mapped["revision"]})),
                (
                    "append",
                    json!({"path":path,
                        "select":{"kind":"heading_path","path":["Sample","Notes"]},
                        "guard":section["revision"],"content":"A real edit."
                    }),
                ),
                (
                    "write",
                    json!({"path":path,"ops":[{
                        "op":"append","select":{"kind":"heading_path","path":["Sample","Notes"]},
                        "guard":section["revision"],"content":"A batch edit."
                    }]}),
                ),
                ("query", json!({"collection":"actions"})),
            ] {
                let (code, out) = rivet(&root, cmd, request);
                assert_eq!(code, 2, "{cmd} {path}: {out}");
                assert_eq!(out["error"]["code"], "invalid_schema", "{out}");
                assert!(has_literal_error(&out), "{out}");
                assert_eq!(std::fs::read(&file).unwrap(), before, "{cmd} {path}");
            }
            for request in [json!({}), json!({"paths":[path]})] {
                let (code, status) = rivet(&root, "status", request);
                assert_eq!(code, 0, "{status}");
                assert_eq!(status["complete"], true, "{status}");
                assert!(has_literal_error(&status), "{status}");
                assert_eq!(
                    status["diagnostics"][0]["code"], "invalid_schema",
                    "{status}"
                );
            }
            let (code, doctor) = rivet(&root, "doctor", json!({}));
            assert_eq!(code, 2, "{doctor}");
            assert_eq!(doctor["health"], "unknown", "{doctor}");
            assert!(has_literal_error(&doctor), "{doctor}");
            assert_eq!(std::fs::read(&file).unwrap(), before);
        }
    }
}

#[test]
fn glob_siblings_keep_the_existing_traversal_policy() {
    let root = project("glob-skipped-bindings");
    git(&root, &["init", "-q"]);
    git(&root, &["commit", "-qm", "fixture", "--allow-empty"]);
    for pattern in [
        ".hidden/*.md",
        "target/*.md",
        "records/node_modules/**/*.md",
        "records/.hidden/task?.md",
    ] {
        let path = pattern
            .replace("**/", "nested/")
            .replace('*', "sample")
            .replace('?', "1");
        let file = root.join(&path);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, SOURCE).unwrap();
        configure(&root, &["tracking/*.md", pattern], &[pattern]);
        for (cmd, request) in [
            ("status", json!({})),
            ("doctor", json!({})),
            ("query", json!({"collection":"actions"})),
            ("map", json!({"path":path})),
        ] {
            let (code, out) = rivet(&root, cmd, request);
            assert_eq!(code, 0, "{pattern} {cmd}: {out}");
            assert!(!has_literal_error(&out), "{out}");
            if cmd == "status" {
                assert_eq!(out["complete"], true, "{out}");
                assert!(
                    out["paths"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .all(|p| p["path"] != path)
                );
            } else if cmd == "doctor" {
                assert!(
                    out["files"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .all(|p| p["path"] != path)
                );
            }
        }
    }
}

#[test]
fn missing_literals_and_unselected_archive_bindings_are_diagnosed() {
    let root = project("missing-literal-binding");
    configure(&root, &["records/target/missing.md"], &[]);
    let (code, out) = rivet(&root, "doctor", json!({}));
    assert_eq!(code, 2, "{out}");
    assert!(has_literal_error(&out), "{out}");

    let mut config: Value = serde_json::from_str(&read(&root, ".rivet/project.json")).unwrap();
    config["collections"]["other"] = config["collections"]["actions"].clone();
    config["collections"]["other"]["files"] = json!(["tracking/*.md"]);
    config["collections"]["actions"]["files"] = json!(["records/*.md"]);
    config["collections"]["actions"]["archive"] = json!(["records/target/missing.md"]);
    std::fs::write(root.join(".rivet/project.json"), config.to_string()).unwrap();
    let (code, out) = rivet(&root, "doctor", json!({"collection":"other"}));
    assert_eq!(code, 2, "{out}");
    assert!(has_literal_error(&out), "{out}");
}

#[test]
fn ordinary_literal_bindings_still_preserve_surrounding_bytes_and_stay_attachment() {
    let root = project("ordinary-literal-binding");
    let path = "records/targeted/task[1].md";
    std::fs::create_dir_all(root.join("records/targeted")).unwrap();
    std::fs::write(root.join(path), SOURCE).unwrap();
    configure(&root, &[path], &[]);
    stamp(&root, path);
    let (code, query) = rivet(&root, "query", json!({"collection":"actions"}));
    assert_eq!(code, 0, "{query}");
    let item = &query["items"][0];
    let id = item["id"].as_str().unwrap();
    let (_, before_item) = rivet(
        &root,
        "get",
        json!({
            "path":path, "select":{"kind":"id","id":id}
        }),
    );
    let before = read(&root, path);
    let (code, out) = rivet(
        &root,
        "update-item",
        json!({
            "path":path,"collection":"actions","id":id,
            "guard":item["revision"],"set":{"done":true}
        }),
    );
    assert_eq!(code, 0, "{out}");
    let (_, after_item) = rivet(
        &root,
        "get",
        json!({
            "path":path, "select":{"kind":"id","id":id}
        }),
    );
    assert_eq!(
        after_item["content"].as_str().unwrap(),
        before_item["content"]
            .as_str()
            .unwrap()
            .replacen("[ ]", "[x]", 1)
    );
    assert_eq!(after_item["checked"], true);
    let after = read(&root, path);
    let note_at = before.find("## Notes").unwrap();
    assert_eq!(
        &after[after.find("## Notes").unwrap()..],
        &before[note_at..]
    );
    assert_eq!(after.matches(&format!("stay:{id} ")).count(), 1);
}
