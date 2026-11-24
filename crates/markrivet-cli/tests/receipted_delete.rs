mod common;
use common::*;
use markrivet_core::{
    Document, Selector,
    delete_receipt::{DeleteReceipt, VerifyRequest},
};
use serde_json::{Value, json};
use std::path::PathBuf;

const ARCHIVE: &str = "archive/month/closed.md";
const SOURCE: &str = "# Synthetic project\n\nPrefix text.\n\n## Action\n\n- [ ] Keep this open.\n- [x] Archive this completed item.\n  Ordinary nested content.\n- [x] Another completed item.\n\n## Log\n\nUnrelated suffix text.\n";

// Keep the large transport/marker stress workloads from consuming the Git
// deadlines of unrelated functional cases in this test process. Parallel
// writers and fault interleavings are exercised separately in concurrency.rs.
static FIXTURE_WORKLOAD: std::sync::Mutex<()> = std::sync::Mutex::new(());

const SOLE_SOURCE: &str = "# Synthetic project\n\nPrefix text.\n\n## Action\n\n- [x] Archive this completed item.\n  Ordinary nested content.\n\n## Log\n\nUnrelated suffix text.\n";

struct Fixture {
    root: PathBuf,
    home: PathBuf,
    _workload: std::sync::MutexGuard<'static, ()>,
}
impl Fixture {
    fn new(name: &str, crlf: bool) -> Self {
        Self::with_source(name, crlf, SOURCE)
    }
    fn sole(name: &str, crlf: bool) -> Self {
        Self::with_source(name, crlf, SOLE_SOURCE)
    }
    fn with_source(name: &str, crlf: bool, input: &str) -> Self {
        // The mutex contains no data, so a failed case must not poison the
        // independent fixtures that follow it.
        let workload = FIXTURE_WORKLOAD.lock().unwrap_or_else(|p| p.into_inner());
        let root = project(name);
        let home = root.with_extension("isolated-home");
        std::fs::create_dir_all(&home).unwrap();
        let s = Self {
            root,
            home,
            _workload: workload,
        };
        let source = if crlf {
            input.replace('\n', "\r\n")
        } else {
            input.into()
        };
        std::fs::write(s.root.join(WIDGETS), source).unwrap();
        std::fs::create_dir_all(s.root.join("archive/month")).unwrap();
        std::fs::write(
            s.root.join(ARCHIVE),
            if crlf {
                "# Archive\r\n\r\n## Closed\r\n\r\nPreserved archive prefix.\r\n"
            } else {
                "# Archive\n\n## Closed\n\nPreserved archive prefix.\n"
            },
        )
        .unwrap();
        stamp(&s.root, WIDGETS);
        stamp(&s.root, ARCHIVE);
        let config_path = s.root.join(".rivet/project.json");
        let mut config: Value =
            serde_json::from_slice(&std::fs::read(&config_path).unwrap()).unwrap();
        config["collections"]["actions"]["archive"] = json!(["archive/**/*.md"]);
        config["git"] = json!({"commit":"per-write", "on_dirty":"refuse"});
        config["prune"] = json!({"files":["tracking/*.md"]});
        std::fs::write(config_path, serde_json::to_vec(&config).unwrap()).unwrap();
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
        self.run_env(cmd, input, &[])
    }
    fn run_env(&self, cmd: &str, input: Value, extra: &[(&str, String)]) -> (i32, Value) {
        let mut env = vec![
            ("HOME", self.home.to_string_lossy().into()),
            ("XDG_CONFIG_HOME", self.home.to_string_lossy().into()),
            ("GIT_CONFIG_NOSYSTEM", "1".into()),
            ("GIT_CONFIG_GLOBAL", "/dev/null".into()),
        ];
        env.extend_from_slice(extra);
        finish(spawn(&self.root, cmd, &input, &env))
    }
    fn id(&self, item: usize) -> String {
        let doc = Document::parse(&read(&self.root, WIDGETS)).unwrap();
        doc.target_ids(markrivet_core::document::Target::Item { list: 0, item })[0].clone()
    }
    fn delete(&self, item: usize) -> Value {
        let id = self.id(item);
        json!({"path":WIDGETS,"select":{"kind":"id","id":id},"guard":item_guard(&self.root,WIDGETS,&id),"require_commit":true})
    }

    fn list_delete(&self) -> Value {
        let doc = Document::parse(&read(&self.root, WIDGETS)).unwrap();
        let mut request = self.delete(0);
        request.as_object_mut().unwrap().remove("require_commit");
        request["list"] = json!({"guard":doc.target_revision(
            markrivet_core::document::Target::Block(doc.lists[0].block))});
        request
    }
    fn commit_source(&self, source: &str) {
        std::fs::write(self.root.join(WIDGETS), source).unwrap();
        self.git(&["add", WIDGETS]);
        self.git(&["commit", "-qm", "synthetic source change"]);
    }
    fn copy(&self, item: usize) -> Value {
        let id = self.id(item);
        let select = json!({"kind":"heading_path","path":["Closed"]});
        let (_, dest) = self.run("get", json!({"path":ARCHIVE,"select":select}));
        json!({"source":WIDGETS,"id":id,"guard":item_guard(&self.root,WIDGETS,&id),"path":ARCHIVE,"select":select,"destination_guard":dest["revision"]})
    }
    fn state(&self) -> (String, String, String, Vec<u8>) {
        (
            read(&self.root, WIDGETS),
            read(&self.root, ARCHIVE),
            self.git(&["rev-parse", "HEAD"]),
            std::fs::read(self.root.join(".git/index")).unwrap(),
        )
    }
    fn receipt(&self, out: &Value) -> DeleteReceipt {
        assert_eq!(
            out["commit"]["status"], "committed",
            "delete fixture commit: {out}"
        );
        let msg = self.git(&["log", "-1", "--format=%B"]);
        DeleteReceipt::decode(
            msg.lines()
                .find_map(|l| l.strip_prefix("Rivet-Delete: "))
                .unwrap(),
        )
        .unwrap()
    }
}

fn old_verifier(request: &Value) -> Option<(i32, Value)> {
    let old = std::env::var_os("RIVET_V1_VERIFIER")?;
    let version = std::process::Command::new(&old)
        .arg("--version")
        .output()
        .unwrap();
    assert!(
        String::from_utf8_lossy(&version.stdout)
            .contains("11bdd47b15079ccbc5f1e29113258a046c9cbfd5")
    );
    let mut command = std::process::Command::new(old);
    command
        .arg("verify-delete")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped());
    let mut child = command.spawn().unwrap();
    use std::io::Write;
    child
        .stdin
        .take()
        .unwrap()
        .write_all(request.to_string().as_bytes())
        .unwrap();
    Some(finish(child))
}

#[test]
fn receipt_fixture_reports_the_full_failed_commit_before_reading_the_trailer() {
    use std::os::unix::fs::PermissionsExt;

    let s = Fixture::new("receipt-commit-deadline", false);
    let slow = s.home.join("slow-git");
    std::fs::write(
        &slow,
        "#!/bin/sh\ncase \"$*\" in *\" update-ref --stdin \"*) exec sleep 30;; esac\nexec git \"$@\"\n",
    )
    .unwrap();
    std::fs::set_permissions(&slow, std::fs::Permissions::from_mode(0o755)).unwrap();
    let head = s.git(&["rev-parse", "HEAD"]);
    let (code, out) = s.run_env(
        "delete",
        s.delete(1),
        &[
            ("RIVET_TEST_GIT", slow.to_string_lossy().into()),
            ("RIVET_TEST_GIT_DEADLINE_MS", "1000".into()),
        ],
    );
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["commit"]["status"], "failed", "{out}");
    assert_eq!(out["commit"]["reason"], "deadline", "{out}");
    assert_eq!(s.git(&["rev-parse", "HEAD"]), head);
    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| s.receipt(&out)))
        .expect_err("the fixture must reject a failed commit before reading its trailer");
    let message = panic
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| panic.downcast_ref::<&str>().copied())
        .expect("assertion panic must contain a diagnostic");
    assert!(
        message.contains(&format!("delete fixture commit: {out}")),
        "{message}"
    );
}

#[test]
fn delete_receipts_replay_exact_bytes_and_preserve_survivor_attachment() {
    for crlf in [false, true] {
        for item in [1, 2] {
            let s = Fixture::new("receipt-exact", crlf);
            let before = read(&s.root, WIDGETS);
            let original = Document::parse(&before).unwrap();
            let parent = s.git(&["rev-parse", "HEAD"]).trim().to_string();
            let request = s.delete(item);
            let id = s.id(item);
            let (code, out) = s.run("delete", request);
            assert_eq!(code, 0, "{out}");
            assert_eq!(out["commit"]["status"], "committed", "{out}");
            let receipt = s.receipt(&out);
            assert_eq!(receipt.parent, parent);
            assert_eq!(receipt.operation.dropped(), std::slice::from_ref(&id));
            assert_eq!(out["ops"][0]["dropped"], json!([id.clone()]));
            let after = read(&s.root, WIDGETS);
            let parsed = Document::parse(&after).unwrap();
            for (stay, targets) in &original.ids {
                if stay != &id {
                    assert_eq!(targets.len(), parsed.ids[stay].len());
                    let target = parsed.resolve(&Selector::Id { id: stay.clone() }).unwrap();
                    assert_eq!(
                        std::mem::discriminant(&targets[0]),
                        std::mem::discriminant(&target)
                    );
                    if matches!(targets[0], markrivet_core::document::Target::Item { .. }) {
                        assert_eq!(
                            original.target_span(targets[0]).0.slice(&before),
                            parsed.target_span(target).0.slice(&after)
                        );
                        assert_eq!(original.target_ids(targets[0]), parsed.target_ids(target));
                    }
                }
            }
            assert!(after.starts_with(before.split("## Action").next().unwrap()));
            assert!(after.ends_with(before.split("## Log").nth(1).unwrap()));
            assert_eq!(parsed.newline, original.newline);
            let (code, verified) = s.run(
                "verify-delete",
                json!({"parent":before,"candidate":after,"receipt":receipt}),
            );
            assert_eq!(code, 0, "{verified}");
            assert_eq!(verified["dropped"], json!([id]));
            if let Some((code, old)) =
                old_verifier(&json!({"parent":before,"candidate":after,"receipt":receipt}))
            {
                assert_eq!(code, 0, "v1 control: {old}");
                assert_eq!(old["version"], 1);
            }
            let events = read(&s.root, ".rivet/commits.jsonl");
            let event: Value = serde_json::from_str(events.lines().last().unwrap()).unwrap();
            assert_eq!(
                event["delete_receipt"],
                serde_json::to_value(&receipt).unwrap()
            );
        }
    }
}

#[test]
fn receipted_section_and_list_block_deletes_preserve_exact_outside_bytes() {
    use markrivet_core::document::Target;
    for section in [false, true] {
        let s = Fixture::new("receipt-structural-target", false);
        let source = read(&s.root, WIDGETS);
        let doc = Document::parse(&source).unwrap();
        let log = doc
            .resolve(&Selector::HeadingPath {
                path: vec!["Log".into()],
            })
            .unwrap();
        let (select, target, expected) = if section {
            (
                Selector::HeadingPath {
                    path: vec!["Log".into()],
                },
                log,
                source[..doc.target_span(log).0.start].to_string(),
            )
        } else {
            let block = doc.lists[0].block;
            let id = doc.stay_ids(&doc.blocks[block].markers)[0].clone();
            (
                Selector::Id { id },
                Target::Block(block),
                format!(
                    "{}{}",
                    &source[..doc.blocks[block].span.start],
                    &source[doc.target_span(log).0.start..]
                ),
            )
        };
        let guard = doc.target_revision(target);
        let archive = read(&s.root, ARCHIVE);
        let (code, out) = s.run(
            "delete",
            json!({"path":WIDGETS,"select":select,"guard":guard,"require_commit":true}),
        );
        assert_eq!(code, 0, "{out}");
        let candidate = read(&s.root, WIDGETS);
        assert_eq!(candidate, expected);
        assert_eq!(read(&s.root, ARCHIVE), archive);
        let receipt = s.receipt(&out);
        let response: Vec<String> =
            serde_json::from_value(out["ops"][0]["dropped"].clone()).unwrap();
        assert_eq!(
            receipt
                .operation
                .dropped()
                .iter()
                .collect::<std::collections::BTreeSet<_>>(),
            response.iter().collect()
        );
        let (code, out) = s.run(
            "verify-delete",
            json!({"parent":source,"candidate":candidate,"receipt":receipt}),
        );
        assert_eq!(code, 0, "{out}");
    }
}

fn unguarded_reference_delete_refusal(verify: bool) {
    use markrivet_core::{delete_receipt::hash, document::Target};
    for block in ["- first\n- second\n", "Paragraph.\n"] {
        for suffix in ["", "Uses [ref] here.\n"] {
            for newline in ["\n", "\r\n"] {
                let s = Fixture::new("unguarded-delete", newline == "\r\n");
                let source = format!(
                    "# Page\n\n{block}<!-- stay:delete01 -->\n\n[ref]: https://example.com/x\n\n{suffix}"
                ).replace('\n', newline);
                std::fs::write(s.root.join(WIDGETS), &source).unwrap();
                s.git(&["add", WIDGETS]);
                s.git(&["commit", "-qm", "synthetic unguarded reference"]);
                let doc = Document::parse(&source).unwrap();
                let select = Selector::Id {
                    id: "delete01".into(),
                };
                let target = doc.resolve(&select).unwrap();
                assert!(matches!(target, Target::Block(_)));
                let guard = doc.target_revision(target);
                let before = s.state();
                if !verify {
                    let (code, out) = s.run(
                        "delete",
                        json!({
                            "path":WIDGETS, "select":select, "guard":guard, "require_commit":true
                        }),
                    );
                    assert_eq!(code, 2, "{out}");
                    assert_eq!(out["error"]["code"], "unsupported_syntax", "{out}");
                    assert_eq!(s.state(), before);
                }

                // A well-formed legacy receipt describes exactly the unsafe old
                // deletion, including byte hashes, so refusal must come from replay.
                let start = doc.target_span(target).0.start;
                let candidate = format!("{}{}", &source[..start], suffix.replace('\n', newline));
                let oid = "0".repeat(40);
                let receipt = json!({
                    "version":1, "op":"0123456789abcdef", "path":WIDGETS,
                    "parent":oid, "base":{"oid":oid,"mode":"100644"},
                    "candidate":{"oid":oid,"mode":"100644"},
                    "read_sha256":hash(source.as_bytes()),
                    "candidate_sha256":hash(candidate.as_bytes()),
                    "operation":{"op":"delete","select":select,"guard":guard,"dropped":["delete01"]}
                });
                if verify {
                    let (code, out) = s.run(
                        "verify-delete",
                        json!({
                            "parent":source, "candidate":candidate, "receipt":receipt
                        }),
                    );
                    assert_eq!(code, 2, "{out}");
                    assert_eq!(out["error"]["code"], "unsupported_syntax", "{out}");
                    assert_eq!(s.state(), before);
                }

                // Block archive-copy is already unsupported; it cannot copy a
                // narrower unit and then authorize this larger deletion.
                let (_, dest) = s.run(
                    "get",
                    json!({
                        "path":ARCHIVE, "select":{"kind":"heading_path","path":["Closed"]}
                    }),
                );
                let (code, out) = s.run(
                    "archive-copy",
                    json!({
                        "source":WIDGETS, "id":"delete01", "guard":guard,
                        "path":ARCHIVE, "select":{"kind":"heading_path","path":["Closed"]},
                        "destination_guard":dest["revision"]
                    }),
                );
                assert_eq!(code, 2, "{out}");
                assert_eq!(out["error"]["code"], "archive_copy_refused", "{out}");
                assert_eq!(s.state(), before);
            }
        }
    }
}

#[test]
fn deletes_refuse_unguarded_reference_definitions_without_writing() {
    unguarded_reference_delete_refusal(false);
}

#[test]
fn receipt_replay_refuses_unguarded_reference_definitions_without_writing() {
    unguarded_reference_delete_refusal(true);
}

#[test]
fn archive_copy_and_receipted_item_delete_agree_on_guarded_reference_definitions() {
    for crlf in [false, true] {
        let s = Fixture::new("guarded-item-reference", crlf);
        let nl = if crlf { "\r\n" } else { "\n" };
        let source = SOURCE
            .replace(
                "  Ordinary nested content.\n",
                "  Ordinary nested content.\n\n  [ref]: https://example.com/x\n",
            )
            .replace('\n', nl);
        std::fs::write(s.root.join(WIDGETS), &source).unwrap();
        s.git(&["add", WIDGETS]);
        s.git(&["commit", "-qm", "synthetic guarded reference"]);
        stamp(&s.root, WIDGETS);
        let source = read(&s.root, WIDGETS);
        let doc = Document::parse(&source).unwrap();
        let target = doc.resolve(&Selector::Id { id: s.id(1) }).unwrap();
        let span = doc.target_span(target).0;
        assert!(span.slice(&source).unwrap().contains("[ref]:"));
        let archive_before = read(&s.root, ARCHIVE);
        let (code, copied) = s.run("archive-copy", s.copy(1));
        assert_eq!(code, 0, "{copied}");
        let archived = read(&s.root, ARCHIVE);
        assert!(archived.contains("[ref]: https://example.com/x"));
        assert!(archived.starts_with(&archive_before));
        assert_eq!(read(&s.root, WIDGETS), source);
        let (code, deleted) = s.run("delete", s.delete(1));
        assert_eq!(code, 0, "{deleted}");
        let candidate = read(&s.root, WIDGETS);
        assert!(!candidate.contains("[ref]:"));
        assert_eq!(read(&s.root, ARCHIVE), archived);
        let receipt = s.receipt(&deleted);
        let (code, verified) = s.run(
            "verify-delete",
            json!({
                "parent":source, "candidate":candidate, "receipt":receipt
            }),
        );
        assert_eq!(code, 0, "{verified}");
    }
}

#[test]
fn archive_copy_refuses_conflicting_or_unproven_ids_and_source_evidence_drift() {
    for variant in ["conflict", "another_archive", "drift"] {
        let s = Fixture::new("archive-id-provenance", false);
        let source = read(&s.root, WIDGETS);
        let doc = Document::parse(&source).unwrap();
        let id = s.id(1);
        let target = doc.resolve(&Selector::Id { id: id.clone() }).unwrap();
        let item = doc.target_span(target).0.slice(&source).unwrap();
        if variant == "drift" {
            let markrivet_core::document::Target::Item { list, item: index } = target else {
                unreachable!()
            };
            let marker = &doc.markers[doc.lists[list].items[index].markers[0]];
            let stored = marker.lexical.subhash.as_ref().unwrap();
            let raw = marker.lexical.span.slice(&source).unwrap();
            let replacement = raw.replace(stored, &"0".repeat(stored.len()));
            let mut changed = source.clone();
            changed.replace_range(
                marker.lexical.span.start..marker.lexical.span.end,
                &replacement,
            );
            std::fs::write(s.root.join(WIDGETS), changed).unwrap();
        } else {
            let path = if variant == "conflict" {
                ARCHIVE
            } else {
                "archive/month/other.md"
            };
            let copied = if variant == "conflict" {
                item.replace("Archive this completed item.", "Conflicting bytes.")
            } else {
                item.into()
            };
            std::fs::write(
                s.root.join(path),
                format!("# Synthetic archive\n\n## Closed\n\n{copied}\n"),
            )
            .unwrap();
        }
        s.git(&["add", "-A"]);
        s.git(&["commit", "-qm", "synthetic conflicting archive input"]);
        let request = s.copy(1);
        let state = s.state();
        let (code, out) = s.run("archive-copy", request);
        assert_eq!(code, 2, "{variant}: {out}");
        assert_eq!(s.state(), state);
        if variant == "drift" {
            assert_eq!(out["error"]["code"], "archive_copy_refused", "{out}");
            assert!(out["error"]["message"].as_str().unwrap().contains("drift"));
        }
    }
}

#[test]
fn archive_discovery_refuses_excluded_inputs_without_changing_documents_or_git() {
    let s = Fixture::new("archive-excluded-discovery", false);
    std::os::unix::fs::symlink(s.root.join(ARCHIVE), s.root.join("archive/month/link.md")).unwrap();
    s.git(&["add", "archive/month/link.md"]);
    s.git(&["commit", "-qm", "synthetic excluded archive"]);
    let source = read(&s.root, WIDGETS);
    let archive = read(&s.root, ARCHIVE);
    let head = s.git(&["rev-parse", "HEAD"]);
    let index = std::fs::read(s.root.join(".git/index")).unwrap();
    let (code, out) = s.run("archive-copy", s.copy(1));
    assert_eq!(code, 2, "{out}");
    assert_eq!(out["error"]["code"], "path_rejected");
    assert_eq!(out["error"]["path"], "archive/month/link.md");
    assert_eq!(read(&s.root, WIDGETS), source);
    assert_eq!(read(&s.root, ARCHIVE), archive);
    assert_eq!(s.git(&["rev-parse", "HEAD"]), head);
    assert_eq!(std::fs::read(s.root.join(".git/index")).unwrap(), index);
}

#[test]
fn verifier_rejects_extra_edits_and_receipt_tampering_without_a_project() {
    let s = Fixture::new("receipt-tamper", false);
    let parent = read(&s.root, WIDGETS);
    let (code, out) = s.run("delete", s.delete(1));
    assert_eq!(code, 0, "{out}");
    let candidate = read(&s.root, WIDGETS);
    let receipt = s.receipt(&out);
    let base = json!({"parent":parent,"candidate":candidate,"receipt":receipt});
    let absent = s.root.join("absent-project");
    let (code, verified) = rivet(&absent, "verify-delete", base.clone());
    assert_eq!(code, 0, "{verified}");
    assert!(!absent.exists());
    for field in [
        "version",
        "op",
        "path",
        "parent",
        "base",
        "candidate",
        "read_sha256",
        "candidate_sha256",
        "operation",
    ] {
        let mut request = base.clone();
        request["receipt"][field] = match field {
            "version" => json!(2),
            "base" | "candidate" => json!({"oid":"0".repeat(40),"mode":"120000"}),
            "operation" => {
                let mut op = request["receipt"]["operation"].clone();
                op["dropped"] = json!(["unrelated"]);
                op
            }
            "path" => json!("tracking/./other.md"),
            _ => json!("bad"),
        };
        assert_ne!(s.run("verify-delete", request).0, 0, "{field}");
    }
    let mut extra = base.clone();
    extra["candidate"] = json!(candidate.replace("suffix", "changed suffix"));
    extra["receipt"]["candidate_sha256"] = json!(markrivet_core::delete_receipt::hash(
        extra["candidate"].as_str().unwrap().as_bytes()
    ));
    assert_ne!(s.run("verify-delete", extra).0, 0);
    let mut duplicate = base.clone();
    duplicate["receipt"]["operation"]["dropped"] = json!([
        receipt.operation.dropped()[0],
        receipt.operation.dropped()[0]
    ]);
    assert_ne!(s.run("verify-delete", duplicate).0, 0);
    let state = s.state();
    for line in [
        "Rivet-Delete: forged",
        " rivet-delete = forged",
        "RIVET-DELETE : forged",
        "Rivet-Archive: forged",
    ] {
        let mut request = s.delete(1);
        request["message"] = json!(format!("subject\n\n{line}"));
        assert_ne!(s.run("delete", request).0, 0);
        assert_eq!(s.state(), state);
    }
    let text = serde_json::to_string(&base).unwrap().replacen(
        "\"version\":1",
        "\"version\":1,\"version\":1",
        1,
    );
    let mut child = command(&s.root, "verify-delete", &[]).spawn().unwrap();
    use std::io::Write;
    child
        .stdin
        .take()
        .unwrap()
        .write_all(text.as_bytes())
        .unwrap();
    assert_ne!(finish(child).0, 0);
}

#[test]
fn archive_copy_is_exact_idempotent_and_supports_later_appends_before_delete() {
    for crlf in [false, true] {
        let s = Fixture::new("archive-exact", crlf);
        let before = read(&s.root, WIDGETS);
        let id = s.id(1);
        let original = Document::parse(&before).unwrap();
        let target = original.resolve(&Selector::Id { id: id.clone() }).unwrap();
        let item = original.target_span(target).0.slice(&before).unwrap();
        let request = s.copy(1);
        let destination_before = read(&s.root, ARCHIVE);
        let (code, first) = s.run("archive-copy", request.clone());
        assert_eq!(code, 0, "{first}");
        assert_eq!(first["commit"]["status"], "committed", "{first}");
        let journal = read(&s.root, ".rivet/commits.jsonl");
        let event: Value = serde_json::from_str(journal.lines().last().unwrap()).unwrap();
        assert_eq!(first["commit"]["op"], event["op"]);
        assert_eq!(first["commit"]["op"], first["commit"]["copy_op"]);
        assert_eq!(read(&s.root, WIDGETS), before);
        let archived = read(&s.root, ARCHIVE);
        assert!(archived.starts_with(&destination_before));
        assert_eq!(archived.matches(item).count(), 1);
        let head = s.git(&["rev-parse", "HEAD"]);
        let (code, retry) = s.run("archive-copy", request);
        assert_eq!(code, 0, "{retry}");
        assert_eq!(retry["status"], "unchanged");
        assert_eq!(retry["commit"]["copy_op"], first["commit"]["copy_op"]);
        assert_eq!(retry["commit"]["op"], event["op"]);
        assert_eq!(read(&s.root, ".rivet/commits.jsonl"), journal);
        assert_eq!(retry["commit"]["sha"], first["commit"]["sha"]);
        assert_eq!(s.git(&["rev-parse", "HEAD"]), head);
        let (code, second) = s.run("archive-copy", s.copy(2));
        assert_eq!(code, 0, "{second}");
        assert_eq!(second["commit"]["status"], "committed", "{second}");
        let mut deletion = s.delete(1);
        deletion["archive"] = json!({"path":ARCHIVE,"copy_op":retry["commit"]["copy_op"],"commit":retry["commit"]["sha"]});
        let (_, snapshot) = s.run("prune-snapshot", json!({}));
        deletion["policy_guard"] = snapshot["snapshot"]["revision"].clone();
        let (code, deleted) = s.run("delete", deletion);
        assert_eq!(code, 0, "{deleted}");
        assert_eq!(deleted["commit"]["status"], "committed", "{deleted}");
        let mut second_deletion = s.delete(1);
        second_deletion["archive"] = json!({"path":ARCHIVE,"copy_op":second["commit"]["copy_op"],"commit":second["commit"]["sha"]});
        let (code, deleted) = s.run("delete", second_deletion);
        assert_eq!(code, 0, "{deleted}");
        assert_eq!(read(&s.root, ARCHIVE).matches(item).count(), 1);
    }
}

#[test]
fn archive_requests_normalize_paths_and_refusals_preserve_bytes() {
    for crlf in [false, true] {
        let s = Fixture::new("archive-path-spellings", crlf);
        let before = s.state();
        let (code, made) = s.run("archive-mkdir", json!({"path":"./archive//./next"}));
        assert_eq!(code, 0, "{made}");
        assert_eq!(made["path"], "archive/next");
        assert!(s.root.join("archive/next").is_dir());
        assert_eq!(s.state(), before);
        let (code, refused) = s.run("archive-mkdir", json!({"path":"archive/../escape"}));
        assert_eq!(code, 2, "{refused}");
        assert_eq!(refused["error"]["code"], "path_rejected");
        assert!(!s.root.join("escape").exists());
        assert_eq!(s.state(), before);

        for field in ["source", "path"] {
            let mut bad = s.copy(1);
            bad[field] = json!("../escape.md");
            let (code, refused) = s.run("archive-copy", bad);
            assert_eq!(code, 2, "{refused}");
            assert_eq!(refused["error"]["code"], "path_rejected");
            assert_eq!(s.state(), before);
        }
        let source = Document::parse(&before.0).unwrap();
        let id = s.id(1);
        let target = source.resolve(&Selector::Id { id: id.clone() }).unwrap();
        let bytes = source.target_span(target).0.slice(&before.0).unwrap();
        let mut request = s.copy(1);
        request["source"] = json!(format!("./{}", WIDGETS.replacen('/', "//./", 1)));
        request["path"] = json!(format!("./{}", ARCHIVE.replacen('/', "/./", 1)));
        let (code, copied) = s.run("archive-copy", request.clone());
        assert_eq!(code, 0, "{copied}");
        assert_eq!(copied["path"], ARCHIVE);
        assert_eq!(read(&s.root, WIDGETS), before.0);
        let after = read(&s.root, ARCHIVE);
        assert!(after.starts_with(&before.1));
        assert_eq!(after.matches(bytes).count(), 1);
        let archived = Document::parse(&after).unwrap();
        let target = archived.resolve(&Selector::Id { id }).unwrap();
        assert_eq!(archived.target_span(target).0.slice(&after), Some(bytes));
        let (_, retry) = s.run("archive-copy", request);
        assert_eq!(retry["status"], "unchanged", "{retry}");
        assert_eq!(read(&s.root, ARCHIVE), after);
    }
}

#[test]
fn receipt_path_assertions_keep_the_receipt_diagnostic() {
    let s = Fixture::new("receipt-path-spelling", false);
    let (code, out) = s.run("delete", s.delete(1));
    assert_eq!(code, 0, "{out}");
    let mut receipt = s.receipt(&out);
    receipt.path = format!("./{}", receipt.path);
    let error = receipt.validate().unwrap_err();
    assert_eq!(error.code, markrivet_core::ErrorCode::InvalidDeleteReceipt);
}

#[test]
fn archive_and_commit_precondition_refusals_preserve_source_archive_index_and_head() {
    for variant in [
        "source_guard",
        "destination_guard",
        "missing_archive",
        "newline",
        "dirty_archive",
        "supplied_content",
        "symlink",
        "assume_unchanged",
        "skip_worktree",
        "lock",
        "conversion",
    ] {
        let s = Fixture::new("archive-refuse", false);
        let mut request = s.copy(1);
        match variant {
            "source_guard" => request["guard"] = json!("stale"),
            "destination_guard" => request["destination_guard"] = json!("stale"),
            "missing_archive" => {
                let p = s.root.join(".rivet/project.json");
                let mut c: Value = serde_json::from_slice(&std::fs::read(&p).unwrap()).unwrap();
                c["collections"]["actions"]["archive"] = json!([]);
                std::fs::write(p, serde_json::to_vec(&c).unwrap()).unwrap();
            }
            "newline" => std::fs::write(
                s.root.join(ARCHIVE),
                read(&s.root, ARCHIVE).replace('\n', "\r\n"),
            )
            .unwrap(),
            "dirty_archive" => {
                std::fs::write(
                    s.root.join(ARCHIVE),
                    format!("{}outside\n", read(&s.root, ARCHIVE)),
                )
                .unwrap();
            }
            "supplied_content" => {
                request["content"] =
                    json!("- [x] forged <!-- stay:forged subhash=sha256:000000000000 -->")
            }
            "symlink" => {
                std::os::unix::fs::symlink(s.root.join("archive"), s.root.join("linked")).unwrap();
                request["path"] = json!("linked/month/closed.md");
            }
            "assume_unchanged" => {
                s.git(&["update-index", "--assume-unchanged", ARCHIVE]);
            }
            "skip_worktree" => {
                s.git(&["update-index", "--skip-worktree", ARCHIVE]);
            }
            "lock" => std::fs::write(s.root.join(".git/index.lock"), "").unwrap(),
            "conversion" => {
                std::fs::write(
                    s.root.join(".gitattributes"),
                    format!("{ARCHIVE} filter=custom\n"),
                )
                .unwrap();
            }
            _ => unreachable!(),
        }
        let state = s.state();
        let (code, out) = s.run("archive-copy", request);
        assert_ne!(code, 0, "{variant}: {out}");
        assert_eq!(s.state(), state, "{variant}");
    }
}

#[test]
fn archive_copy_refuses_sole_items_and_nested_foreign_stays_without_writing() {
    for variant in ["sole", "foreign"] {
        let s = Fixture::new("archive-unsupported-source", false);
        if variant == "sole" {
            for _ in 0..2 {
                let (code, out) = s.run("delete", s.delete(1));
                assert_eq!(code, 0, "{out}");
            }
        } else {
            let source = read(&s.root, WIDGETS);
            let doc = Document::parse(&source).unwrap();
            let id = s.id(0);
            let target = doc.resolve(&Selector::Id { id }).unwrap();
            let mut changed = source.clone();
            changed.insert_str(
                doc.target_span(target).0.end,
                "\n  - [x] Nested <!-- stay:foreignItem subhash=sha256:000000000000 -->",
            );
            std::fs::write(s.root.join(WIDGETS), changed).unwrap();
            s.git(&["add", WIDGETS]);
            s.git(&["commit", "-qm", "synthetic foreign child"]);
        }
        let request = s.copy(0);
        let state = s.state();
        let (code, out) = s.run("archive-copy", request);
        if variant == "sole" {
            assert_eq!(code, 0, "{out}");
            assert_eq!(out["commit"]["status"], "committed", "{out}");
            assert_eq!(read(&s.root, WIDGETS), state.0);
        } else {
            assert_eq!(code, 2, "{out}");
            assert_eq!(out["error"]["code"], "unsupported_syntax", "{out}");
            assert_eq!(s.state(), state);
        }
    }
}

#[test]
fn receipted_control_character_paths_refuse_explicitly_without_silent_downgrade() {
    let s = Fixture::new("delete-control-path", false);
    let path = "tracking/control\nname.md";
    std::fs::write(s.root.join(path), read(&s.root, WIDGETS)).unwrap();
    s.git(&["add", "--", path]);
    s.git(&["commit", "-qm", "synthetic unusual path"]);
    let id = s.id(1);
    let guard = item_guard(&s.root, path, &id);
    let before = read(&s.root, path);
    let state = s.state();
    let (code, out) = s.run(
        "delete",
        json!({"path":path,"select":{"kind":"id","id":id},"guard":guard,"require_commit":true}),
    );
    assert_eq!(code, 2, "{out}");
    assert_eq!(out["error"]["code"], "path_rejected");
    assert_eq!(read(&s.root, path), before);
    assert_eq!(s.state(), state);
}

#[test]
fn policy_snapshot_ignores_helper_symlinks_through_check_and_preserves_the_delete_footprint() {
    for crlf in [false, true] {
        let s = Fixture::new("policy-helper-symlink", crlf);
        let (code, original) = s.run("prune-snapshot", json!({}));
        assert_eq!(code, 0, "{original}");
        std::os::unix::fs::symlink("unavailable-helper", s.root.join("save.sh")).unwrap();
        // The same parent directory can contain an unrelated symlink.
        std::os::unix::fs::symlink("unavailable-helper", s.root.join("tracking/helper.sh"))
            .unwrap();
        let state = s.state();
        let (code, out) = s.run("prune-snapshot", json!({}));
        assert_eq!(code, 0, "{out}");
        assert_eq!(
            out["snapshot"]["revision"],
            original["snapshot"]["revision"]
        );
        assert_eq!(s.state(), state);
        let before = read(&s.root, WIDGETS);
        let archive = read(&s.root, ARCHIVE);
        let original = Document::parse(&before).unwrap();
        let deleted = s.id(1);
        let mut request = s.delete(1);
        request["policy_guard"] = out["snapshot"]["revision"].clone();
        let (code, out) = s.run("delete", request);
        assert_eq!(code, 0, "{out}");
        let receipt = s.receipt(&out);
        let after = read(&s.root, WIDGETS);
        let parsed = Document::parse(&after).unwrap();
        for (stay, targets) in &original.ids {
            if stay != &deleted {
                let target = parsed.resolve(&Selector::Id { id: stay.clone() }).unwrap();
                assert_eq!(
                    std::mem::discriminant(&targets[0]),
                    std::mem::discriminant(&target)
                );
                if matches!(targets[0], markrivet_core::document::Target::Item { .. }) {
                    assert_eq!(
                        original.target_span(targets[0]).0.slice(&before),
                        parsed.target_span(target).0.slice(&after)
                    );
                    assert_eq!(original.target_ids(targets[0]), parsed.target_ids(target));
                }
            }
        }
        assert!(after.starts_with(before.split("## Action").next().unwrap()));
        assert!(after.ends_with(before.split("## Log").nth(1).unwrap()));
        assert_eq!(read(&s.root, ARCHIVE), archive);
        let (code, verified) = s.run(
            "verify-delete",
            json!({"parent":before,"candidate":after,"receipt":receipt}),
        );
        assert_eq!(code, 0, "{verified}");
        assert_eq!(
            std::fs::read_link(s.root.join("save.sh")).unwrap(),
            PathBuf::from("unavailable-helper")
        );
    }
}

#[test]
fn policy_snapshot_and_guarded_delete_refuse_in_scope_symlinks_and_unreadable_directories() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    for problem in ["file-symlink", "directory-symlink", "unreadable"] {
        if problem == "unreadable" && unsafe { libc::geteuid() } == 0 {
            // Root bypasses mode bits; deterministic walker-fault coverage is in core.
            continue;
        }
        let s = Fixture::new(problem, false);
        let config_path = s.root.join(".rivet/project.json");
        let mut config: Value =
            serde_json::from_slice(&std::fs::read(&config_path).unwrap()).unwrap();
        config["prune"]["files"] = json!(["tracking/**/*.md"]);
        config["collections"]["actions"]["files"] = json!(["tracking/**/*.md"]);
        std::fs::write(config_path, serde_json::to_vec(&config).unwrap()).unwrap();
        s.git(&["add", ".rivet/project.json"]);
        s.git(&["commit", "-qm", "recursive synthetic namespace"]);
        let (code, snapshot) = s.run("prune-snapshot", json!({}));
        assert_eq!(code, 0, "{snapshot}");
        let mut request = s.delete(1);
        request["policy_guard"] = snapshot["snapshot"]["revision"].clone();
        let bad = if problem == "file-symlink" {
            "tracking/link.md"
        } else {
            "tracking/subtree"
        };
        if problem == "unreadable" {
            std::fs::create_dir(s.root.join(bad)).unwrap();
            std::fs::write(s.root.join(bad).join("hidden.md"), "# Must not disappear\n").unwrap();
            std::fs::set_permissions(s.root.join(bad), std::fs::Permissions::from_mode(0o0))
                .unwrap();
        } else {
            symlink("unavailable", s.root.join(bad)).unwrap();
        }
        let state = s.state();
        for (command, input) in [("prune-snapshot", json!({})), ("delete", request)] {
            let (code, out) = s.run(command, input);
            assert_eq!(code, 2, "{out}");
            assert_eq!(
                out["error"]["code"],
                if problem == "unreadable" {
                    "io"
                } else {
                    "path_rejected"
                }
            );
            assert_eq!(
                out["error"]["path"],
                if command == "prune-snapshot" {
                    bad
                } else {
                    WIDGETS
                }
            );
            assert_eq!(s.state(), state);
        }
        if problem == "unreadable" {
            std::fs::set_permissions(s.root.join(bad), std::fs::Permissions::from_mode(0o755))
                .unwrap();
        }
    }
}

#[test]
fn policy_snapshot_covers_all_live_bytes_and_refuses_unbound_policy_sources() {
    let s = Fixture::new("policy-scope", false);
    let (code, out) = s.run("prune-snapshot", json!({}));
    assert_eq!(code, 0, "{out}");
    let snapshot: markrivet_core::prune_snapshot::Snapshot =
        serde_json::from_value(out["snapshot"].clone()).unwrap();
    snapshot.verify().unwrap();
    assert!(snapshot.inputs.contains_key(WIDGETS));
    assert!(snapshot.inputs.contains_key(".rivet/project.json"));
    assert!(
        snapshot
            .inputs
            .keys()
            .any(|p| p.starts_with(".rivet/schemas/"))
    );
    let mut deletion = s.delete(1);
    deletion["policy_guard"] = json!(snapshot.revision);
    std::fs::write(
        s.root.join("tracking/new.md"),
        "# Newly discovered policy input\n",
    )
    .unwrap();
    let state = s.state();
    let (code, out) = s.run("delete", deletion);
    assert_eq!(code, 2, "{out}");
    assert_eq!(out["error"]["code"], "policy_conflict");
    assert_eq!(s.state(), state);
    let p = s.root.join(".rivet/project.json");
    let mut config: Value = serde_json::from_slice(&std::fs::read(&p).unwrap()).unwrap();
    config["collections"]["actions"]["files"] = json!([WIDGETS]);
    std::fs::write(p, serde_json::to_vec(&config).unwrap()).unwrap();
    let (code, out) = s.run("prune-snapshot", json!({}));
    assert_eq!(code, 2, "{out}");
    assert!(
        out["error"]["message"]
            .as_str()
            .unwrap()
            .contains("outside live bound")
    );
    assert_eq!(out["error"]["path"], "tracking/TRACKING_DOCS.md", "{out}");
}

#[test]
fn confined_archive_parents_refuse_symlinks_and_unconfigured_paths() {
    let s = Fixture::new("archive-mkdir", false);
    let source = s.state();
    let (code, out) = s.run("archive-mkdir", json!({"path":"archive/new/month"}));
    assert_eq!(code, 0, "{out}");
    assert!(s.root.join("archive/new/month").is_dir());
    assert_eq!(s.state(), source);
    std::os::unix::fs::symlink(s.root.join("archive/month"), s.root.join("archive/link")).unwrap();
    for path in ["archive/link/new", "../escape", "unconfigured/new"] {
        let (code, out) = s.run("archive-mkdir", json!({"path":path}));
        assert_eq!(code, 2, "{out}");
        assert_eq!(s.state(), source);
    }
    assert!(!s.root.join("archive/month/new").exists());
}

#[test]
fn snapshot_reports_synthetic_lock_and_refusal_latency_with_coverage() {
    let s = Fixture::new("snapshot-measurement", false);
    for n in 0..128 {
        std::fs::write(
            s.root.join(format!("tracking/synthetic-{n}.md")),
            format!(
                "# Synthetic {n}\n\n## Action\n\n{}\n",
                "safe text ".repeat(820)
            ),
        )
        .unwrap();
    }
    let (code, out) = s.run("prune-snapshot", json!({}));
    assert_eq!(code, 0, "{out}");
    let snapshot: markrivet_core::prune_snapshot::Snapshot =
        serde_json::from_value(out["snapshot"].clone()).unwrap();
    snapshot.verify().unwrap();
    assert_eq!(snapshot.live.len(), 130);
    assert!(out["capture_us"].as_u64().unwrap() > 0);
    let bytes = snapshot.manifest.values().map(|i| i.bytes).sum::<usize>();
    let mut deletion = s.delete(1);
    deletion["policy_guard"] = json!(snapshot.revision);
    std::fs::write(s.root.join("tracking/added.md"), "# New dependency input\n").unwrap();
    let start = std::time::Instant::now();
    let (code, refused) = s.run("delete", deletion);
    let elapsed = start.elapsed().as_micros();
    assert_eq!(code, 2, "{refused}");
    assert_eq!(refused["error"]["code"], "policy_conflict");
    println!(
        "synthetic snapshot: {} live files, {} raw bytes, capture under lock {} us; changed-membership refusal {} us wall clock (debug CLI)",
        snapshot.live.len(),
        bytes,
        out["capture_us"],
        elapsed
    );
}

#[test]
fn receipt_canonical_budget_has_an_exact_boundary() {
    let s = Fixture::new("receipt-budget", false);
    let (code, out) = s.run("delete", s.delete(1));
    assert_eq!(code, 0, "{out}");
    let mut receipt = s.receipt(&out);
    let length = receipt.bytes().unwrap().len();
    receipt
        .path
        .push_str(&"x".repeat(markrivet_core::delete_receipt::MAX_RECEIPT - length - 1));
    assert_eq!(receipt.bytes().unwrap().len(), 65_535);
    receipt.path.push('x');
    assert_eq!(receipt.bytes().unwrap().len(), 65_536);
    assert!(receipt.encode().is_ok());
    receipt.path.push('x');
    assert_eq!(
        receipt.bytes().unwrap_err().code,
        markrivet_core::ErrorCode::DeleteReceiptTooLarge
    );
    let request = VerifyRequest {
        parent: String::new(),
        candidate: String::new(),
        receipt,
    };
    assert!(markrivet_core::delete_receipt::verify(&request).is_err());
}

#[test]
fn previews_off_mode_and_mixed_batches_publish_no_delete_authority() {
    for mode in ["preview", "off", "mixed"] {
        let s = Fixture::new("unreceipted-mode", false);
        let before = s.state();
        let mut request = s.delete(1);
        if mode == "preview" {
            request["preview"] = json!(true);
        }
        if mode == "off" {
            let path = s.root.join(".rivet/project.json");
            let mut config: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
            config["git"] = json!({"commit":"off"});
            std::fs::write(path, serde_json::to_vec(&config).unwrap()).unwrap();
            request.as_object_mut().unwrap().remove("require_commit");
        }
        let (code, out) = if mode == "mixed" {
            let (_, section) = s.run(
                "get",
                json!({"path":WIDGETS,"select":{"kind":"heading_path","path":["Log"]}}),
            );
            let mut delete = request.clone();
            delete.as_object_mut().unwrap().remove("path");
            delete.as_object_mut().unwrap().remove("require_commit");
            delete["op"] = json!("delete");
            s.run("write",json!({"path":WIDGETS,"ops":[delete,{"op":"append","select":{"kind":"heading_path","path":["Log"]},"guard":section["revision"],"content":"Extra log paragraph."}]}))
        } else {
            s.run("delete", request)
        };
        assert_eq!(code, 0, "{mode}: {out}");
        if mode == "preview" {
            assert_eq!(s.state(), before);
        }
        assert!(
            !s.git(&["log", "-1", "--format=%B"])
                .contains("Rivet-Delete:")
        );
        let events =
            std::fs::read_to_string(s.root.join(".rivet/commits.jsonl")).unwrap_or_default();
        assert!(!events.contains("delete_receipt"));
    }
}

#[test]
fn emitted_receipts_at_the_budget_boundary_replay_and_oversize_refuses_before_publication() {
    use markrivet_core::{
        delete_receipt::{DeleteIntent, hash},
        git::Entry,
    };
    for limit in [65_535, 65_536, 65_537] {
        let s = Fixture::new("producer-budget", false);
        let render = |title: &str| {
            format!("## {title}\n\nDiscard this section.\n\n## Keep\n\nPreserved suffix.\n")
        };
        let source = render("Cut");
        let doc = Document::parse(&source).unwrap();
        let select = Selector::HeadingPath {
            path: vec!["Cut".into()],
        };
        let guard = doc.target_revision(doc.resolve(&select).unwrap());
        let template = DeleteReceipt {
            version: 1,
            op: "0".repeat(16),
            path: WIDGETS.into(),
            parent: "0".repeat(40),
            base: Entry {
                oid: "0".repeat(40),
                mode: "100644".into(),
            },
            candidate: Entry {
                oid: "0".repeat(40),
                mode: "100644".into(),
            },
            read_sha256: hash(b""),
            candidate_sha256: hash(b""),
            operation: DeleteIntent::Delete {
                list: None,
                select,
                guard,
                dropped: vec![],
            },
        };
        let length = serde_json::to_vec(&template).unwrap().len();
        let title = "x".repeat(3 + limit - length);
        let source = render(&title);
        std::fs::write(s.root.join(WIDGETS), &source).unwrap();
        s.git(&["add", WIDGETS]);
        s.git(&["commit", "-qm", "synthetic large selector"]);
        let doc = Document::parse(&source).unwrap();
        let select = Selector::HeadingPath { path: vec![title] };
        let guard = doc.target_revision(doc.resolve(&select).unwrap());
        let state = s.state();
        let (code, out) = s.run(
            "delete",
            json!({"path":WIDGETS,"select":select,"guard":guard,"require_commit":true}),
        );
        if limit > 65_536 {
            assert_eq!(code, 2, "{out}");
            assert_eq!(out["error"]["code"], "delete_receipt_too_large");
            assert_eq!(s.state(), state);
        } else {
            assert_eq!(code, 0, "{out}");
            let receipt = s.receipt(&out);
            assert_eq!(receipt.bytes().unwrap().len(), limit);
            let clone = s.root.with_extension("clone");
            let cloned = git_command(&s.root, &s.home, &["clone", "--quiet", "--no-hardlinks"])
                .arg(&s.root)
                .arg(&clone)
                .output()
                .unwrap();
            assert!(cloned.status.success());
            let (code, verified) = rivet(
                &clone,
                "verify-delete",
                json!({"parent":source,"candidate":read(&s.root,WIDGETS),"receipt":receipt}),
            );
            assert_eq!(code, 0, "{verified}");
        }
    }
}

#[test]
fn dropped_item_ids_share_the_complete_receipt_budget() {
    for count in [120, 240] {
        let s = Fixture::new("many-dropped-ids", false);
        let mut source = String::from("## Remove\n\n");
        for n in 0..count {
            source.push_str(&format!(
                "- [x] Synthetic {n} <!-- stay:item{n:06}abcdefghijklmnopqrstuvwx subhash=sha256:000000000000 -->\n"
            ));
        }
        source.push_str("\n## Retain\n\nUnrelated suffix.\n");
        std::fs::write(s.root.join(WIDGETS), &source).unwrap();
        s.git(&["add", WIDGETS]);
        s.git(&["commit", "-qm", "synthetic many-item section"]);
        let doc = Document::parse(&source).unwrap();
        let select = Selector::HeadingPath {
            path: vec!["Remove".into()],
        };
        let guard = doc.target_revision(doc.resolve(&select).unwrap());
        let state = s.state();
        let env = [("RIVET_TEST_RECEIPT_BYTES", "8192".into())];
        let (code, out) = s.run_env(
            "delete",
            json!({"path":WIDGETS,"select":select,"guard":guard,"require_commit":true}),
            &env,
        );
        if count == 240 {
            assert_eq!(code, 2, "{out}");
            assert_eq!(out["error"]["code"], "delete_receipt_too_large");
            assert_eq!(s.state(), state);
        } else {
            assert_eq!(code, 0, "{out}");
            let receipt = s.receipt(&out);
            assert_eq!(receipt.operation.dropped().len(), count);
            assert!(receipt.bytes().unwrap().len() <= 8192);
            assert!(read(&s.root, WIDGETS).ends_with("## Retain\n\nUnrelated suffix.\n"));
            let (code, out) = s.run_env(
                "verify-delete",
                json!({"parent":source,"candidate":read(&s.root,WIDGETS),"receipt":receipt}),
                &env,
            );
            assert_eq!(code, 0, "{out}");
        }
    }
}

#[test]
fn verifier_transport_accepts_escaped_near_eight_mib_inputs_and_refuses_raw_decoded_and_framing_limits()
 {
    use markrivet_core::{
        delete_receipt::{DeleteIntent, hash},
        edit::{self, Op, Planner},
        git::Entry,
    };
    let s = Fixture::new("transport-budget", false);
    let header = "## Drop\n\nDelete.\n\n## Keep\n\n```text\n";
    let tail = "\n```\n";
    let source = format!(
        "{header}{}{tail}",
        "\u{1}".repeat(markrivet_core::fs::MAX_FILE_BYTES as usize - header.len() - tail.len())
    );
    let doc = Document::parse(&source).unwrap();
    let select = Selector::HeadingPath {
        path: vec!["Drop".into()],
    };
    let guard = doc.target_revision(doc.resolve(&select).unwrap());
    let op = Op::Delete {
        list: None,
        collection: None,
        select: select.clone(),
        guard: guard.clone(),
    };
    let outcome = edit::plan(&doc, vec![Planner::new(&doc).structural(&op).unwrap()]).unwrap();
    let receipt = DeleteReceipt {
        version: 1,
        op: "0".repeat(16),
        path: WIDGETS.into(),
        parent: "0".repeat(40),
        base: Entry {
            oid: "0".repeat(40),
            mode: "100644".into(),
        },
        candidate: Entry {
            oid: "0".repeat(40),
            mode: "100644".into(),
        },
        read_sha256: hash(source.as_bytes()),
        candidate_sha256: hash(outcome.candidate.as_bytes()),
        operation: DeleteIntent::Delete {
            list: None,
            select,
            guard,
            dropped: vec![],
        },
    };
    let request = json!({"parent":source,"candidate":outcome.candidate,"receipt":receipt});
    assert!(serde_json::to_vec(&request).unwrap().len() > 96 * 1024 * 1024 - 1024);
    let (code, out) = s.run("verify-delete", request.clone());
    assert_eq!(code, 0, "{out}");
    let mut oversized = request;
    oversized["parent"].as_str().unwrap();
    oversized["parent"] = json!(format!("{}x", oversized["parent"].as_str().unwrap()));
    let (code, out) = s.run("verify-delete", oversized);
    assert_eq!(code, 2, "{out}");
    assert_eq!(out["error"]["code"], "limit_exceeded");

    use std::io::Write;
    for (cmd, bytes) in [
        ("verify-delete", vec![b' '; 100 * 1024 * 1024 + 1]),
        ("inspect", vec![b' '; 16 * 1024 * 1024 + 1]),
    ] {
        let mut child = command(&s.root, cmd, &[]).spawn().unwrap();
        child.stdin.take().unwrap().write_all(&bytes).unwrap();
        let (code, out) = finish(child);
        assert_eq!(code, 1, "{out}");
        assert!(
            out["error"]["message"]
                .as_str()
                .unwrap()
                .contains("request exceeds")
        );
    }
    let tiny = json!({"parent":"","candidate":"","receipt":receipt}).to_string();
    let framed = format!("{}{}", " ".repeat(1024 * 1024 + 1), tiny);
    let mut child = command(&s.root, "verify-delete", &[]).spawn().unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(framed.as_bytes())
        .unwrap();
    let (code, out) = finish(child);
    assert_ne!(code, 0);
    assert!(
        out["error"]["message"]
            .as_str()
            .unwrap()
            .contains("framing")
    );
}

#[test]
fn receipt_budget_uses_canonical_bytes_when_transport_escapes_unicode() {
    let s = Fixture::new("unicode-receipt-transport", false);
    let title = "é".repeat(20_000);
    let source = format!("## {title}\n\nRemove.\n\n## Keep\n\nPreserved suffix.\n");
    std::fs::write(s.root.join(WIDGETS), &source).unwrap();
    s.git(&["add", WIDGETS]);
    s.git(&["commit", "-qm", "synthetic unicode selector"]);
    let doc = Document::parse(&source).unwrap();
    let select = Selector::HeadingPath { path: vec![title] };
    let guard = doc.target_revision(doc.resolve(&select).unwrap());
    let (code, out) = s.run(
        "delete",
        json!({"path":WIDGETS,"select":select,"guard":guard,"require_commit":true}),
    );
    assert_eq!(code, 0, "{out}");
    let receipt = s.receipt(&out);
    assert!(receipt.bytes().unwrap().len() < 65_536);
    assert!(
        serde_json::to_string(&receipt)
            .unwrap()
            .replace('é', "\\u00e9")
            .len()
            > 65_536
    );
    let wire = json!({"parent":source,"candidate":read(&s.root,WIDGETS),"receipt":receipt})
        .to_string()
        .replace('é', "\\u00e9");
    use std::io::Write;
    let mut child = command(&s.root, "verify-delete", &[]).spawn().unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(wire.as_bytes())
        .unwrap();
    let (code, verified) = finish(child);
    assert_eq!(code, 0, "{verified}");
    assert_eq!(verified["status"], "verified");
}

#[test]
fn require_commit_checks_source_flags_and_late_hash_failures_before_replacement() {
    for variant in [
        "assume",
        "skip",
        "dirty",
        "staged",
        "conversion",
        "lock",
        "off",
    ] {
        let s = Fixture::new("delete-eligibility", false);
        let request = s.delete(1);
        match variant {
            "assume" => {
                s.git(&["update-index", "--assume-unchanged", WIDGETS]);
            }
            "skip" => {
                s.git(&["update-index", "--skip-worktree", WIDGETS]);
            }
            "dirty" | "staged" => {
                std::fs::write(
                    s.root.join(WIDGETS),
                    format!("{}outside\n", read(&s.root, WIDGETS)),
                )
                .unwrap();
                if variant == "staged" {
                    s.git(&["add", WIDGETS]);
                }
            }
            "conversion" => std::fs::write(
                s.root.join(".gitattributes"),
                format!("{WIDGETS} filter=custom\n"),
            )
            .unwrap(),
            "lock" => std::fs::write(s.root.join(".git/index.lock"), "").unwrap(),
            "off" => {
                let p = s.root.join(".rivet/project.json");
                let mut c: Value = serde_json::from_slice(&std::fs::read(&p).unwrap()).unwrap();
                c["git"] = json!({"commit":"off"});
                std::fs::write(p, serde_json::to_vec(&c).unwrap()).unwrap();
            }
            _ => unreachable!(),
        }
        let state = s.state();
        let (code, out) = s.run("delete", request);
        assert_eq!(code, 2, "{variant}: {out}");
        assert_eq!(s.state(), state, "{variant}");
    }
}

#[test]
fn history_reads_receipted_delete_and_archive_copy_together() {
    let s = Fixture::new("history-delete-archive", false);
    let id = s.id(1);
    let original = read(&s.root, WIDGETS);
    let (code, copy) = s.run("archive-copy", s.copy(1));
    assert_eq!(code, 0, "{copy}");
    let (code, deleted) = s.run("delete", s.delete(1));
    assert_eq!(code, 0, "{deleted}");
    assert_eq!(deleted["commit"]["status"], "committed");
    let state = s.state();
    let (code, log) = s.run(
        "item-log",
        json!({"path":WIDGETS,"collection":"actions","id":id}),
    );
    assert_eq!(code, 0, "{log}");
    assert_eq!(log["coverage"]["complete"], true);
    let rows = log["transitions"].as_array().unwrap();
    assert_eq!(rows[0]["event"], "deletion");
    assert!(
        rows[0]["message"]
            .as_str()
            .unwrap()
            .contains("Rivet-Delete:")
    );
    assert!(rows.iter().any(|r| {
        r["after"]["state"]["content"]
            .as_str()
            .is_some_and(|s| s.contains("Archive this completed item."))
    }));
    assert!(log["restore_guard"].is_null());
    assert_eq!(log["restore_diagnostic"]["code"], "history_target_absent");
    assert_eq!(s.state(), state);
    let archive = read(&s.root, ARCHIVE);
    let doc = Document::parse(&archive).unwrap();
    let original = Document::parse(&original).unwrap();
    let target = original.resolve(&Selector::Id { id: id.clone() }).unwrap();
    // Copying retains the original item identity and content in its destination.
    let copied = doc.resolve(&Selector::Id { id: id.clone() }).unwrap();
    assert_eq!(original.target_ids(target), doc.target_ids(copied));
    let (code, log) = s.run(
        "item-log",
        json!({"path":ARCHIVE,"collection":"actions","id":id}),
    );
    assert_eq!(code, 0, "{log}");
    assert!(
        log["transitions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["message"].as_str().unwrap().contains("Rivet-Archive:"))
    );
    assert_eq!(s.state(), state);
}

#[test]
fn list_delete_exact_bytes_receipt_v2_and_portable_replay() {
    use markrivet_core::document::Target;
    for crlf in [false, true] {
        let s = Fixture::sole("list-exact", crlf);
        let before = read(&s.root, WIDGETS);
        let doc = Document::parse(&before).unwrap();
        let b = doc.lists[0].block;
        let item_ids = doc.target_ids(Target::Item { list: 0, item: 0 });
        let list_ids = doc.target_ids(Target::Block(b));
        assert!(!list_ids.is_empty());
        let mut dropped = item_ids.clone();
        dropped.extend(list_ids.clone());
        dropped.sort();
        let request = s.list_delete();
        let (code, out) = s.run("delete", request);
        assert_eq!(code, 0, "{out}");
        let receipt = s.receipt(&out);
        assert_eq!(receipt.version, 2);
        assert_eq!(receipt.operation.dropped(), dropped);
        let response_dropped: std::collections::BTreeSet<_> =
            serde_json::from_value::<Vec<String>>(out["ops"][0]["dropped"].clone())
                .unwrap()
                .into_iter()
                .collect();
        assert_eq!(response_dropped, dropped.iter().cloned().collect());
        let after = read(&s.root, WIDGETS);
        let parsed = Document::parse(&after).unwrap();
        let end = doc.blocks.get(b + 1).map_or(before.len(), |b| b.span.start);
        let mut expected = before[..doc.blocks[b].span.start].to_string() + &before[end..];
        // Only the Action heading's evidence is refreshed outside the deletion.
        for (i, m) in doc.markers.iter().enumerate() {
            if let Some(id) = &m.lexical.id {
                if dropped.contains(id) {
                    continue;
                }
                let target = parsed.resolve(&Selector::Id { id: id.clone() }).unwrap();
                let original = doc.resolve(&Selector::Id { id: id.clone() }).unwrap();
                assert_eq!(
                    std::mem::discriminant(&target),
                    std::mem::discriminant(&original)
                );
                if let Some((fresh, Some(stored))) = parsed
                    .markers
                    .iter()
                    .position(|n| n.lexical.id.as_ref() == Some(id))
                    .and_then(|j| parsed.marker_evidence(j))
                    && fresh.starts_with(&stored)
                {
                    let old = doc.marker_evidence(i).unwrap().1.unwrap();
                    expected = expected.replace(&old, &stored);
                }
            }
        }
        assert_eq!(after, expected);
        assert!(!after.contains("Archive this"));
        let event: Value = read(&s.root, ".rivet/commits.jsonl")
            .lines()
            .last()
            .map(|l| serde_json::from_str(l).unwrap())
            .unwrap();
        assert_eq!(event["delete_receipt"], json!(receipt));
        assert_eq!(
            event["delete_receipt"]["operation"]["dropped"],
            json!(dropped)
        );
        let request = json!({"parent":before,"candidate":after,"receipt":receipt});
        let (code, verified) = s.run("verify-delete", request.clone());
        assert_eq!(code, 0, "{verified}");
        assert_eq!(verified["version"], 2);
        let clone = scratch("list-replay-clone");
        s.git(&[
            "clone",
            "-q",
            s.root.to_str().unwrap(),
            clone.to_str().unwrap(),
        ]);
        let (code, verified) = rivet(&clone, "verify-delete", request.clone());
        assert_eq!(code, 0, "{verified}");
        if let Some((code, out)) = old_verifier(&request) {
            assert_ne!(code, 0, "old verifier must hold v2: {out}");
            assert_eq!(out["error"]["code"], "invalid_request", "{out}");
        }
        let operation = serde_json::to_string(&receipt.operation).unwrap();
        assert!(operation.starts_with("{\"op\":\"delete\",\"select\":"));
        assert!(operation.find("\"guard\":").unwrap() < operation.find("\"list\":").unwrap());
        assert!(operation.find("\"list\":").unwrap() < operation.find("\"dropped\":").unwrap());
        for (version, remove_list) in [(1, false), (2, true)] {
            let mut bad = request.clone();
            bad["receipt"]["version"] = json!(version);
            if remove_list {
                bad["receipt"]["operation"]
                    .as_object_mut()
                    .unwrap()
                    .remove("list");
            }
            let (code, out) = s.run("verify-delete", bad);
            assert_eq!(code, 2, "{out}");
            assert_eq!(out["error"]["code"], "invalid_delete_receipt");
        }
        let mut v1 = request.clone();
        v1["receipt"]["version"] = json!(1);
        v1["receipt"]["operation"]
            .as_object_mut()
            .unwrap()
            .remove("list");
        let (code, out) = s.run("verify-delete", v1);
        assert_eq!(code, 2, "{out}");
        assert_eq!(out["error"]["code"], "invalid_request");
    }
}

#[test]
fn list_delete_refuses_non_items_multi_items_and_stale_trailers() {
    use markrivet_core::document::Target;
    for variant in [
        "section",
        "block",
        "multi",
        "digest",
        "alias",
        "rename",
        "collection",
    ] {
        let s = if variant == "multi" {
            Fixture::new("list-refuse", false)
        } else {
            Fixture::sole("list-refuse", false)
        };
        let mut request = s.list_delete();
        let source = read(&s.root, WIDGETS);
        let doc = Document::parse(&source).unwrap();
        if ["digest", "alias", "rename"].contains(&variant) {
            let b = &doc.blocks[doc.lists[0].block];
            let m = &doc.markers[b.markers[0]];
            let text = m.lexical.span.slice(&source).unwrap();
            let new = match variant {
                "digest" => text.replace("hash=sha256:", "hash=sha256:0"),
                "rename" => text.replace(m.lexical.id.as_ref().unwrap(), "RenamedList"),
                _ => format!("{text}\n<!-- stay:ExtraList hash=sha256:000000000000 -->"),
            };
            s.commit_source(&source.replace(text, &new));
            assert_eq!(
                request["guard"],
                json!(item_guard(&s.root, WIDGETS, &s.id(0)))
            );
        } else if variant == "section" {
            let t = Target::Section(1);
            request["select"] = json!({"kind":"id","id":doc.target_ids(t)[0]});
            request["guard"] = json!(doc.target_revision(t));
        } else if variant == "block" {
            let t = Target::Block(doc.lists[0].block);
            request["select"] = json!({"kind":"id","id":doc.target_ids(t)[0]});
            request["guard"] = json!(doc.target_revision(t));
        } else if variant == "collection" {
            request["collection"] = json!("missing");
        }
        let state = s.state();
        let (code, out) = s.run("delete", request);
        assert_eq!(code, 2, "{variant}: {out}");
        assert_eq!(
            out["error"]["code"],
            match variant {
                "digest" | "alias" | "rename" => "guard_conflict",
                "collection" => "collection_conflict",
                _ => "invalid_request",
            },
            "{variant}: {out}"
        );
        assert_eq!(s.state(), state, "{variant}");
    }
}

#[test]
fn list_delete_and_archive_copy_refuse_foreign_items_and_trailers() {
    use markrivet_core::document::Target;
    for variant in [
        "nested-alias",
        "nested-subhash",
        "trailer-subhash",
        "trailer-malformed",
    ] {
        // Put the unsupported syntax in the fixture before stamping.
        let input = match variant {
            "nested-alias" => SOLE_SOURCE.replace("  Ordinary nested content.", "  Ordinary nested content.\n  - nested <!-- stay:ForeignAlias -->"),
            "nested-subhash" => SOLE_SOURCE.replace("  Ordinary nested content.", "  Ordinary nested content.\n  - nested <!-- stay:ForeignChild subhash=sha256:000000000000 -->"),
            "trailer-subhash" => SOLE_SOURCE.replace("\n\n## Log", "\n<!-- stay:ForeignTrailer subhash=sha256:000000000000 -->\n\n## Log"),
            _ => SOLE_SOURCE.replace("\n\n## Log", "\n<!-- stay:hash=sha256:000000000000 -->\n\n## Log"),
        };
        // Unsupported markers cannot be stamped; use explicit item/list stays,
        // then refresh only valid evidence through the parser's native guards.
        let s = Fixture::sole("list-foreign", false);
        let source = input.replace("- [x] Archive this completed item.",
            "- [x] Archive this completed item. <!-- stay:OwnItem subhash=sha256:000000000000 -->")
            .replace("\n\n## Log", "\n<!-- stay:OwnList -->\n\n## Log");
        s.commit_source(&source);
        let doc = Document::parse(&source).unwrap();
        let request = json!({"path":WIDGETS,"select":{"kind":"id","id":"OwnItem"},
            "guard":doc.target_revision(Target::Item{list:0,item:0}),
            "list":{"guard":doc.target_revision(Target::Block(doc.lists[0].block))}});
        let state = s.state();
        let (code, out) = s.run("delete", request);
        assert_eq!(code, 2, "{variant}: {out}");
        let expected = if variant.starts_with("nested") {
            "unsupported_syntax"
        } else {
            "list_trailer_refused"
        };
        assert_eq!(out["error"]["code"], expected, "{variant}: {out}");
        if variant.starts_with("trailer") {
            assert!(out["error"]["message"].as_str().unwrap().contains("line"));
        }
        assert_eq!(s.state(), state);
        let (code, out) = s.run("archive-copy", s.copy(0));
        assert_eq!(code, 2, "{variant}: {out}");
        assert_eq!(out["error"]["code"], expected, "{variant}: {out}");
        assert_eq!(s.state(), state);
    }
}

#[test]
fn list_delete_gap_reference_definitions_refuse_and_blank_lines_delete() {
    for suffix in [
        "[ref]: /unselected\n\n",
        "[ref]: /unselected\n\nUse [ref].\n\n",
        "\n\n\n",
    ] {
        let s = Fixture::sole("list-gap", false);
        // Stamping mints a list trailer, so add the reference after that trailer.
        s.commit_source(&read(&s.root, WIDGETS).replace("## Log", &format!("{suffix}## Log")));
        let state = s.state();
        let (code, out) = s.run("delete", s.list_delete());
        if suffix.starts_with("[ref]") {
            assert_eq!(code, 2, "{out}");
            assert_eq!(out["error"]["code"], "unsupported_syntax");
            assert_eq!(s.state(), state);
        } else {
            assert_eq!(code, 0, "{out}");
            s.receipt(&out);
        }
    }
}

#[test]
fn list_delete_requires_clean_receipted_commit_but_previews_without_git() {
    for variant in [
        "no-git",
        "skip",
        "adopt",
        "dirty",
        "untracked",
        "git-lock",
        "multi-op",
        "preview",
    ] {
        let s = Fixture::sole("list-commit-only", false);
        let mut request = s.list_delete();
        let config_path = s.root.join(".rivet/project.json");
        let mut config: Value =
            serde_json::from_str(&read(&s.root, ".rivet/project.json")).unwrap();
        match variant {
            "no-git" | "preview" => {
                config.as_object_mut().unwrap().remove("git");
            }
            "skip" | "adopt" => config["git"]["on_dirty"] = json!(variant),
            "dirty" => std::fs::write(s.root.join(WIDGETS), read(&s.root, WIDGETS) + "\n").unwrap(),
            "untracked" => {
                s.git(&["rm", "--cached", WIDGETS]);
                s.git(&["commit", "-qm", "synthetic untrack"]);
            }
            "git-lock" => std::fs::write(s.root.join(".git/index.lock"), "").unwrap(),
            "multi-op" => {
                let mut op = request.clone();
                op.as_object_mut().unwrap().remove("path");
                op["op"] = json!("delete");
                request = json!({"path":WIDGETS,"ops":[op.clone(), op]});
            }
            _ => unreachable!(),
        }
        std::fs::write(config_path, serde_json::to_vec(&config).unwrap()).unwrap();
        if variant == "preview" {
            request["preview"] = json!(true);
        }
        let state = s.state();
        let (code, out) = s.run(
            if variant == "multi-op" {
                "write"
            } else {
                "delete"
            },
            request,
        );
        if variant == "preview" {
            assert_eq!(code, 0, "{out}");
            assert_eq!(out["status"], "preview");
        } else {
            assert_eq!(code, 2, "{variant}: {out}");
            assert_eq!(
                out["error"]["code"],
                if variant == "multi-op" {
                    "invalid_request"
                } else {
                    "commit_refused"
                },
                "{out}"
            );
        }
        assert_eq!(s.state(), state, "{variant}");
    }
}

#[test]
fn list_delete_archive_policy_collection_and_explicit_opt_in() {
    for collection in [false, true] {
        let s = Fixture::sole("list-archive-policy", false);
        let (code, copy) = s.run("archive-copy", s.copy(0));
        assert_eq!(code, 0, "{copy}");
        assert_eq!(copy["commit"]["status"], "committed");
        let (_, snapshot) = s.run("prune-snapshot", json!({}));
        let mut request = s.list_delete();
        request["archive"] =
            json!({"path":ARCHIVE,"commit":copy["commit"]["sha"],"copy_op":copy["commit"]["op"]});
        request["policy_guard"] = snapshot["snapshot"]["revision"].clone();
        if collection {
            request["collection"] = json!("actions");
        }
        let mut plain = request.clone();
        plain.as_object_mut().unwrap().remove("list");
        let state = s.state();
        for bare in [false, true] {
            let mut plain = plain.clone();
            if bare {
                plain.as_object_mut().unwrap().remove("archive");
                plain.as_object_mut().unwrap().remove("policy_guard");
            }
            let (code, out) = s.run("delete", plain);
            assert_eq!(code, 2, "{out}");
            assert_eq!(out["error"]["code"], "invalid_request");
            assert_eq!(s.state(), state);
        }
        let (code, out) = s.run("delete", request);
        assert_eq!(code, 0, "{out}");
        assert_eq!(s.receipt(&out).version, 2);
    }
}

#[test]
fn list_delete_citation_scope_is_complete_and_candidate_only() {
    use markrivet_core::{
        delete_receipt::DeleteIntent,
        document::Target,
        edit::Op,
        fs::{Project, WalkFaults},
    };
    use std::os::unix::{
        ffi::OsStringExt,
        fs::{PermissionsExt, symlink},
    };
    for variant in [
        "collection",
        "collection-archive",
        "section-only",
        "existing-copy",
        "self",
        "item-id",
        "duplicate",
        "lone-cr",
        "symlink",
        "non-utf8",
        "unreadable",
        "read-fault",
        "walk-fault",
        "unrelated",
        "outside-target",
        "outside-citation",
        "verify-pure",
        "preview",
        "preview-no-git",
    ] {
        let s = Fixture::sole("list-citations", false);
        let source = read(&s.root, WIDGETS);
        let doc = Document::parse(&source).unwrap();
        let list = doc.target_ids(Target::Block(doc.lists[0].block))[0].clone();
        let item = s.id(0);
        let config_path = s.root.join(".rivet/project.json");
        let mut config: Value =
            serde_json::from_str(&read(&s.root, ".rivet/project.json")).unwrap();
        config["section_archives"] =
            json!({"history":{"files":["other/*.md"],"archive":["history/**/*.md"]}});
        if variant.starts_with("outside") {
            config["collections"]["actions"]["files"] = json!(["other-live/*.md"]);
        }
        if variant == "preview-no-git" {
            config.as_object_mut().unwrap().remove("git");
        }
        std::fs::write(config_path, serde_json::to_vec(&config).unwrap()).unwrap();
        std::fs::create_dir_all(s.root.join("history/month")).unwrap();
        let history = "history/month/history.md";
        let citing = format!("# History\n\nSee list:{list}.\n");
        let mut unreadable = None;
        match variant {
            "collection" => {
                std::fs::write(s.root.join("tracking/other.md"), &citing).unwrap();
            }
            "collection-archive" | "existing-copy" => {
                std::fs::write(s.root.join("archive/month/other.md"), &citing).unwrap();
            }
            "section-only" | "preview" | "preview-no-git" => {
                std::fs::write(s.root.join(history), &citing).unwrap();
            }
            "lone-cr" => {
                std::fs::write(s.root.join(history), "# History\rBroken\n").unwrap();
            }
            "symlink" => {
                symlink("missing", s.root.join(history)).unwrap();
            }
            "non-utf8" => {
                let bad = std::ffi::OsString::from_vec(b"history/month/bad\xff.md".to_vec());
                std::fs::write(s.root.join(bad), "# History\n").unwrap();
            }
            "unreadable" => {
                // Root bypasses chmod. The read-fault row exercises the refusal
                // and its positive control under root too.
                if unsafe { libc::geteuid() } == 0 {
                    continue;
                }
                let path = s.root.join(history);
                std::fs::write(&path, "# History\n").unwrap();
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
                unreadable = Some(path);
            }
            "read-fault" | "walk-fault" => {
                std::fs::write(s.root.join(history), "# History\n").unwrap();
            }
            "self" => s.commit_source(
                &source.replace("Ordinary nested content.", &format!("See list:{list}.")),
            ),
            "item-id" => {
                std::fs::write(
                    s.root.join(history),
                    format!("# History\n\nSee item:{item}.\n"),
                )
                .unwrap();
            }
            "duplicate" => {
                std::fs::write(
                    s.root.join(history),
                    format!("{citing}\n<!-- stay:{list} -->\n"),
                )
                .unwrap();
            }
            "outside-citation" => {
                s.commit_source(&(source.clone() + &format!("\nSee list:{list}.\n")))
            }
            "unrelated" => {
                std::fs::create_dir_all(s.root.join("unrelated")).unwrap();
                symlink("missing", s.root.join("unrelated/ignored.md")).unwrap();
                std::fs::write(s.root.join(".rivet/ignored.md"), "# Broken\r").unwrap();
            }
            _ => {}
        }
        let mut request = s.list_delete();
        if variant.starts_with("preview") {
            request["preview"] = json!(true);
        }
        let state = s.state();
        if variant == "walk-fault" {
            let project = Project::open(&s.root).unwrap();
            let mut faults = WalkFaults::default();
            faults.directories.insert("history".into());
            let intent = DeleteIntent::from_ops(&[serde_json::from_value::<Op>(json!({"op":"delete","select":request["select"],"guard":request["guard"],"list":request["list"]})).unwrap()], vec![item, list]).unwrap();
            let e = markrivet_core::delete_receipt::check_list_citations(
                &project, WIDGETS, &doc, "", &intent, &faults,
            )
            .unwrap_err();
            assert_eq!(e.code, markrivet_core::ErrorCode::ListCitationRefused);
            assert!(
                e.message.contains("path_rejected") || e.message.contains("io"),
                "{e:?}"
            );
            assert_eq!(s.state(), state);
            continue;
        }
        let env = if variant == "read-fault" {
            vec![("RIVET_TEST_FAULT", "list_citation_read".into())]
        } else {
            vec![]
        };
        let (code, out) = s.run_env("delete", request, &env);
        let positive = [
            "self",
            "item-id",
            "unrelated",
            "outside-target",
            "verify-pure",
        ]
        .contains(&variant);
        if positive {
            assert_eq!(code, 0, "{variant}: {out}");
            let receipt = s.receipt(&out);
            if variant == "verify-pure" {
                std::fs::write(s.root.join(history), &citing).unwrap();
                let (code, verified) = s.run(
                    "verify-delete",
                    json!({"parent":source,"candidate":read(&s.root,WIDGETS),"receipt":receipt}),
                );
                assert_eq!(code, 0, "{verified}");
            }
        } else {
            assert_eq!(code, 2, "{variant}: {out}");
            assert_eq!(
                out["error"]["code"], "list_citation_refused",
                "{variant}: {out}"
            );
            if ["lone-cr", "unreadable", "read-fault", "symlink", "non-utf8"].contains(&variant) {
                assert!(
                    out["error"]["message"]
                        .as_str()
                        .unwrap()
                        .contains("incomplete"),
                    "{out}"
                );
                assert!(out["error"]["path"].as_str().is_some(), "{out}");
            }
            if let Some(p) = unreadable {
                std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o644)).unwrap();
            }
            assert_eq!(s.state(), state, "{variant}");
            if variant == "read-fault" {
                let (code, out) = s.run("delete", s.list_delete());
                assert_eq!(code, 0, "fault control: {out}");
            }
        }
    }
}

#[test]
fn list_delete_collection_checks_membership_of_the_selected_item() {
    use markrivet_core::document::Target;
    let s = Fixture::with_source(
        "list-outside-queue",
        false,
        "# Synthetic\n\n## Action\n\n- [ ] Keep this task.\n\n## Other\n\n- [x] A completed item outside the queue. <!-- stay:Outside01 subhash=sha256:000000000000 -->\n",
    );
    let doc = Document::parse(&read(&s.root, WIDGETS)).unwrap();
    let target = Target::Item { list: 1, item: 0 };
    let request = json!({"path":WIDGETS, "select":{"kind":"id","id":doc.target_ids(target)[0]},
        "guard":doc.target_revision(target), "list":{"guard":doc.target_revision(Target::Block(doc.lists[1].block))},
        "collection":"actions"});
    let state = s.state();
    let (code, out) = s.run("delete", request);
    assert_eq!(code, 2, "{out}");
    assert_eq!(out["error"]["code"], "collection_conflict", "{out}");
    assert_eq!(s.state(), state);
}

/// A position names an item by place, which a receipt replayed against
/// other bytes cannot carry; archive destinations are sections. Both refuse
/// a position before anything else, the committed-copy retry included.
#[test]
fn archive_copy_and_receipts_refuse_a_position_selector() {
    let s = Fixture::new("position-archive", false);
    let position = json!({"kind": "position", "heading_path": ["Closed"], "index": 0});
    let mut request = s.copy(1);
    request["select"] = position.clone();
    let state = s.state();
    let journal = std::fs::read_to_string(s.root.join(".rivet/commits.jsonl")).unwrap_or_default();
    let (code, out) = s.run("archive-copy", request.clone());
    assert_eq!(
        (code, out["error"]["code"].as_str()),
        (2, Some("invalid_request")),
        "{out}"
    );
    assert_eq!(s.state(), state);
    assert_eq!(
        std::fs::read_to_string(s.root.join(".rivet/commits.jsonl")).unwrap_or_default(),
        journal
    );
    // After a committed copy, the retry shortcut would answer `unchanged`
    // before ever resolving the destination; the position is refused first.
    let (code, first) = s.run("archive-copy", s.copy(1));
    assert_eq!(code, 0, "{first}");
    let state = s.state();
    let journal_after = read(&s.root, ".rivet/commits.jsonl");
    assert_ne!(journal_after, journal);
    let mut retry = s.copy(1);
    retry["select"] = position.clone();
    let (code, out) = s.run("archive-copy", retry);
    assert_eq!(
        (code, out["error"]["code"].as_str()),
        (2, Some("invalid_request")),
        "{out}"
    );
    assert_eq!(s.state(), state);
    assert_eq!(read(&s.root, ".rivet/commits.jsonl"), journal_after);

    // A hand-made receipt whose delete names a position.
    let parent = read(&s.root, WIDGETS);
    let (code, out) = s.run("delete", s.delete(1));
    assert_eq!(code, 0, "{out}");
    let candidate = read(&s.root, WIDGETS);
    let receipt = s.receipt(&out);
    let mut request = json!({"parent": parent, "candidate": candidate, "receipt": receipt});
    let (code, verified) = rivet(&s.root, "verify-delete", request.clone());
    assert_eq!(code, 0, "{verified}");
    request["receipt"]["operation"]["select"] =
        json!({"kind": "position", "heading_path": ["Action"], "index": 1});
    let (code, out) = rivet(&s.root, "verify-delete", request);
    assert_eq!(
        (code, out["error"]["code"].as_str()),
        (2, Some("invalid_delete_receipt")),
        "{out}"
    );
}
