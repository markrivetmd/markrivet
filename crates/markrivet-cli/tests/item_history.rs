//! Snapshot history is independent of receipts, live source and commit subjects.
mod common;
use common::*;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

const PATH: &str = "history.md";
const ID: &str = "abcdefgh";
const A: &str = "## Queue\n<!-- stay:sectionA -->\n\n- [ ] first <!-- stay:abcdefgh subhash=sha256:000000000000 -->\n- [ ] sibling <!-- stay:sibling1 subhash=sha256:000000000000 -->\n";

struct Store {
    root: PathBuf,
    home: PathBuf,
}
impl Store {
    fn new(name: &str) -> Self {
        let root = project(name);
        let home = root.with_extension("empty-home");
        std::fs::create_dir_all(&home).unwrap();
        let s = Self { root, home };
        s.git(&["init", "-q", "-b", "main"]);
        s.git(&["config", "user.name", "Synthetic Tester"]);
        s.git(&["config", "user.email", "tester@example.invalid"]);
        s
    }
    fn git(&self, args: &[&str]) -> String {
        let output = git_command(&self.root, &self.home, args).output().unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().into()
    }
    fn commit(&self, content: &str) -> String {
        self.commit_at(PATH, content.as_bytes(), "ordinary snapshot")
    }
    fn commit_at(&self, path: &str, bytes: &[u8], message: &str) -> String {
        std::fs::write(self.root.join(path), bytes).unwrap();
        self.git(&["add", "--", path]);
        self.git(&["commit", "-qm", message]);
        self.git(&["rev-parse", "HEAD"])
    }
    fn env(&self) -> Vec<(&'static str, String)> {
        vec![
            ("HOME", self.home.display().to_string()),
            ("XDG_CONFIG_HOME", self.home.display().to_string()),
        ]
    }
    fn log(&self) -> Value {
        self.log_at(PATH, &[])
    }
    fn log_at(&self, path: &str, extra: &[(&'static str, String)]) -> Value {
        let mut env = self.env();
        env.extend_from_slice(extra);
        let before = snapshot(&self.root);
        let (code, body) = finish(spawn(
            &self.root,
            "item-log",
            &json!({"path":path,"collection":"actions","id":ID}),
            &env,
        ));
        assert_unchanged(&self.root, &before);
        assert_eq!(code, 0, "{body}");
        assert_eq!(body["traversal"], "first_parent");
        body
    }
    fn script(&self, body: &str) -> String {
        let path = self.home.join("git-shim.py");
        std::fs::write(
            &path,
            format!("#!/usr/bin/python3\nimport os,sys,subprocess,time\n{body}\n"),
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path.display().to_string()
    }
}

fn snapshot(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<PathBuf, Vec<u8>>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            if entry.file_type().unwrap().is_dir() {
                out.insert(path.strip_prefix(root).unwrap().into(), vec![]);
                walk(root, &path, out);
            } else if entry.file_type().unwrap().is_symlink() {
                out.insert(
                    path.strip_prefix(root).unwrap().into(),
                    std::fs::read_link(&path)
                        .unwrap()
                        .as_os_str()
                        .as_encoded_bytes()
                        .to_vec(),
                );
            } else {
                out.insert(
                    path.strip_prefix(root).unwrap().into(),
                    std::fs::read(&path).unwrap(),
                );
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(root, root, &mut out);
    out
}

/// Fail naming each added, removed or changed entry, not two whole byte maps.
#[track_caller]
fn assert_unchanged(root: &Path, before: &BTreeMap<PathBuf, Vec<u8>>) {
    let after = snapshot(root);
    let mut diff = Vec::new();
    for (path, bytes) in before {
        match after.get(path) {
            None => diff.push(format!("removed {}", path.display())),
            Some(now) if now != bytes => diff.push(format!("changed {}", path.display())),
            Some(_) => {}
        }
    }
    for path in after.keys().filter(|p| !before.contains_key(*p)) {
        diff.push(format!("added {}", path.display()));
    }
    assert!(
        diff.is_empty(),
        "item-log changed project bytes or directory entries: {diff:?}"
    );
}
fn events(body: &Value) -> Vec<&str> {
    body["transitions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["event"].as_str().unwrap())
        .collect()
}

#[test]
fn history_root_update_delete_reappearance_and_no_receipts() {
    let s = Store::new("history-lifecycle");
    let root = s.commit(A);
    let update = s.commit(
        &A.replace("first", "second")
            .replace("sibling", "other sibling"),
    );
    s.commit("## Queue\n<!-- stay:sectionA -->\n");
    let tip = s.commit(A);
    // A dirty intermediate version is never a Git snapshot.
    std::fs::write(s.root.join(PATH), A.replace("first", "uncommitted")).unwrap();
    // Pending garbage and local bookkeeping are not inputs and are not settled.
    std::fs::create_dir_all(s.root.join(".rivet/pending")).unwrap();
    std::fs::write(s.root.join(".rivet/pending/unreadable.json"), "partial").unwrap();
    std::fs::write(s.root.join(".rivet/commits.jsonl"), "not a receipt\n").unwrap();
    let out = s.log();
    assert_eq!(
        events(&out),
        ["reappearance", "deletion", "content_update", "appearance"]
    );
    assert_eq!(out["history_tip"], tip);
    assert_eq!(out["coverage"]["complete"], true);
    assert_eq!(out["coverage"]["root_reached"], true);
    let rows = out["transitions"].as_array().unwrap();
    assert_eq!(rows[2]["commit"], update);
    assert_eq!(rows[2]["before_rev"], root);
    assert_eq!(rows[3]["before"]["state"]["status"], "absent");
    assert!(rows.iter().all(|r| r["writer_session"].is_null()));
    assert!(!out.to_string().contains("uncommitted"));
    assert!(!out.to_string().contains("other sibling"));
    // A fresh clone without local receipts observes the same recorded content.
    let clone = s.home.join("clone");
    s.git(&[
        "clone",
        "-q",
        s.root.to_str().unwrap(),
        clone.to_str().unwrap(),
    ]);
    assert_eq!(rivet(&clone, "init", json!({})).0, 0);
    let (code, cloned) = finish(spawn(
        &clone,
        "item-log",
        &json!({"path":PATH,"collection":"actions","id":ID}),
        &s.env(),
    ));
    assert_eq!(code, 0, "{cloned}");
    assert_eq!(cloned["transitions"], out["transitions"]);
}

#[test]
fn history_duplicate_parse_orphan_and_kind_states_are_not_deletions() {
    let s = Store::new("history-states");
    s.commit(A);
    s.commit(&format!(
        "{A}- [ ] duplicate <!-- stay:abcdefgh subhash=sha256:000000000000 -->\n"
    ));
    s.commit("unsupported\rline ending\r");
    s.commit("<!-- stay:abcdefgh -->\n");
    s.commit("## Heading\n<!-- stay:abcdefgh -->\n");
    s.commit(A);
    let out = s.log();
    assert!(!events(&out).contains(&"deletion"));
    assert!(events(&out).contains(&"kind_change"));
    assert_eq!(out["coverage"]["complete"], false);
    let text = out.to_string();
    for reason in [
        "history_identity_ambiguous",
        "unsupported_parser_input",
        "unaddressed_identity",
    ] {
        assert!(text.contains(reason), "{reason}: {out}");
    }
}

#[test]
fn history_location_and_marker_only_changes() {
    let s = Store::new("history-location");
    s.commit(A);
    s.commit(&A.replace("000000000000", "111111111111"));
    s.commit(&A.replace("sectionA", "sectionB"));
    let out = s.log();
    assert_eq!(
        events(&out),
        ["location_change", "structural_only", "appearance"]
    );
    assert_eq!(
        out["transitions"][0]["after"]["state"]["location"]["sections"][0][0],
        "sectionB"
    );
    assert_eq!(
        out["transitions"][0]["after"]["state"]["location"]["supported"],
        true
    );
}

#[test]
fn history_pre_stamp_text_other_item_edits_and_same_blob_are_skipped() {
    let s = Store::new("history-prestamp");
    s.commit("- [ ] first\n");
    let stamped = s.commit(A);
    s.commit(&A.replace("sibling", "a sibling"));
    s.git(&["commit", "--allow-empty", "-qm", "empty"]);
    let out = s.log();
    assert_eq!(events(&out), ["appearance"]);
    assert_eq!(out["transitions"][0]["commit"], stamped);
    assert_eq!(out["coverage"]["commits_scanned"], 4);
    assert_eq!(out["coverage"]["distinct_blobs"], 3);
}

#[test]
fn history_deleted_file_and_literal_strange_path() {
    let s = Store::new("history-literal");
    let path = ":(glob)* [x]\t odd.md";
    // Fixture git also needs literal pathspecs for the colon-leading name.
    std::fs::write(s.root.join(path), A).unwrap();
    s.git(&["--literal-pathspecs", "add", "--", path]);
    s.git(&["commit", "-qm", "unrelated subject"]);
    s.git(&["--literal-pathspecs", "rm", "--", path]);
    s.git(&["commit", "-qm", "removed"]);
    let out = s.log_at(path, &[]);
    assert_eq!(out["path"], path);
    assert_eq!(events(&out), ["deletion", "appearance"]);
}

#[test]
fn history_unborn_detached_dirty_index_and_no_current_schema() {
    let s = Store::new("history-eligibility");
    let unborn = s.log();
    assert!(unborn["history_tip"].is_null());
    assert_eq!(unborn["transitions"], json!([]));
    assert!(unborn["restore_guard"].is_null());
    assert!(unborn.get("restore_diagnostic").is_none());
    let tip = s.commit(A);
    s.git(&["checkout", "-q", "--detach"]);
    std::fs::write(s.root.join(PATH), "dirty live bytes").unwrap();
    s.git(&["add", "--", PATH]);
    std::fs::write(s.root.join(".rivet/project.json"), "invalid current config").unwrap();
    s.git(&["config", "--unset", "user.name"]);
    s.git(&["config", "--unset", "user.email"]);
    let out = s.log();
    assert_eq!(out["history_tip"], tip);
    assert!(out["branch"].is_null());
    assert!(out["restore_guard"].is_null());
    assert!(out.get("restore_diagnostic").is_none());
    assert_eq!(events(&out), ["appearance"]);
}

#[test]
fn history_first_parent_merge_omits_side_intermediates() {
    let s = Store::new("history-merge");
    let base = s.commit(A);
    s.git(&["checkout", "-qb", "side"]);
    let side = s.commit(&A.replace("first", "side intermediate"));
    s.commit(&A.replace("first", "integrated"));
    s.git(&["checkout", "-q", "main"]);
    s.git(&["commit", "--allow-empty", "-qm", "main unrelated"]);
    s.git(&["merge", "-q", "--no-ff", "side", "-m", "integration"]);
    let out = s.log();
    assert_eq!(events(&out), ["content_update", "appearance"]);
    assert_eq!(out["transitions"][0]["integration"], true);
    assert_eq!(out["transitions"][1]["commit"], base);
    assert!(!out.to_string().contains(&side));
    assert!(!out.to_string().contains("side intermediate"));
}

#[test]
fn history_invalid_utf8_size_and_unsupported_modes() {
    let s = Store::new("history-bytes");
    s.commit(A);
    s.commit_at(PATH, b"\xff\xfe", "invalid UTF-8");
    let out = s.log();
    assert!(out.to_string().contains("invalid_utf8"));
    assert!(!events(&out).contains(&"deletion"));
    s.commit_at(PATH, &vec![b'a'; 8 * 1024 * 1024 + 1], "large blob");
    let out = s.log();
    assert_eq!(out["coverage"]["boundary"], "blob_bytes");
    assert!(out.to_string().contains("history_resource_limit"));
    // A symlink\'s payload is not a Markdown snapshot.
    let s = Store::new("history-modes");
    s.commit(A);
    std::fs::remove_file(s.root.join(PATH)).unwrap();
    std::os::unix::fs::symlink("missing.md", s.root.join(PATH)).unwrap();
    s.git(&["add", "--", PATH]);
    s.git(&["commit", "-qm", "symlink"]);
    let out = s.log();
    assert!(out.to_string().contains("unsupported_snapshot_mode"));
}

#[test]
fn history_shallow_boundary_is_not_creation() {
    let s = Store::new("history-shallow");
    s.commit(A);
    s.commit(&A.replace("first", "new"));
    let clone = s.home.join("shallow");
    s.git(&[
        "clone",
        "-q",
        "--depth=1",
        &format!("file://{}", s.root.display()),
        clone.to_str().unwrap(),
    ]);
    assert_eq!(rivet(&clone, "init", json!({})).0, 0);
    let before = snapshot(&clone);
    let (code, out) = finish(spawn(
        &clone,
        "item-log",
        &json!({"path":PATH,"collection":"actions","id":ID}),
        &s.env(),
    ));
    assert_eq!(code, 0, "{out}");
    assert_unchanged(&clone, &before);
    assert_eq!(out["coverage"]["root_reached"], false);
    assert_eq!(out["coverage"]["complete"], false);
    assert!(out.to_string().contains("shallow_boundary"));
    assert!(!events(&out).contains(&"appearance"));
}

#[test]
fn history_missing_blob_and_parent_never_fetch_or_mutate() {
    let s = Store::new("history-missing");
    let first = s.commit(A);
    let blob = s.git(&["rev-parse", &format!("{first}:{PATH}")]);
    s.commit(&A.replace("first", "second"));
    let object = s
        .root
        .join(".git/objects")
        .join(&blob[..2])
        .join(&blob[2..]);
    std::fs::remove_file(object).unwrap();
    let calls = s.home.join("calls");
    let script = s.script(&format!(r#"
args=sys.argv[1:]
with open({calls:?},'a') as f: f.write(repr(args)+'\n')
assert os.environ.get('GIT_NO_LAZY_FETCH')=='1'
assert os.environ.get('GIT_NO_REPLACE_OBJECTS')=='1'
assert os.environ.get('GIT_OPTIONAL_LOCKS')=='0'
assert not any(a in ['checkout','fetch','reset','revert','update-ref','hash-object','add','commit'] for a in args)
os.execv('/usr/bin/git',['git']+args)
"#, calls=calls.to_str().unwrap()));
    let out = s.log_at(PATH, &[("RIVET_TEST_GIT", script)]);
    assert_eq!(out["coverage"]["complete"], false);
    assert!(out.to_string().contains("missing_object"));
    assert!(!events(&out).contains(&"deletion"));
    assert!(
        std::fs::read_to_string(&calls)
            .unwrap()
            .contains("cat-file")
    );
    // Missing commit parents still expose the readable tip as gap evidence.
    let object = s
        .root
        .join(".git/objects")
        .join(&first[..2])
        .join(&first[2..]);
    std::fs::remove_file(object).unwrap();
    let out = s.log();
    assert_eq!(out["coverage"]["root_reached"], false);
    assert_eq!(out["coverage"]["complete"], false);
    assert_eq!(
        out["transitions"][0]["after"]["state"]["content"],
        "[ ] second"
    );
}

#[test]
fn history_unavailable_git_and_promisor_clone_refuse_without_writes() {
    let s = Store::new("history-unavailable");
    s.commit(A);
    let before = snapshot(&s.root);
    let mut env = s.env();
    env.push(("RIVET_TEST_GIT", "/absent/git".into()));
    let (code, out) = finish(spawn(
        &s.root,
        "item-log",
        &json!({"path":PATH,"collection":"actions","id":ID}),
        &env,
    ));
    assert_eq!(code, 2, "{out}");
    assert_eq!(out["error"]["code"], "history_unavailable");
    assert_unchanged(&s.root, &before);
    s.git(&["config", "remote.origin.promisor", "true"]);
    let before = snapshot(&s.root);
    let (code, out) = finish(spawn(
        &s.root,
        "item-log",
        &json!({"path":PATH,"collection":"actions","id":ID}),
        &s.env(),
    ));
    assert_eq!(code, 2, "{out}");
    assert_eq!(out["error"]["code"], "history_unavailable");
    assert_unchanged(&s.root, &before);
}

fn process_dead(pid: i32) -> bool {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat"));
    match stat {
        Err(_) => true,
        Ok(s) => s
            .rsplit_once(") ")
            .is_some_and(|(_, tail)| tail.starts_with('Z')),
    }
}

#[test]
fn history_stdout_stderr_cutoffs_kill_and_reap_process_groups() {
    for (stream, boundary) in [(1, "git_stdout"), (2, "git_stderr")] {
        let s = Store::new(&format!("history-overflow-{stream}"));
        s.commit(A);
        let pids = s.home.join("pids");
        let script = s.script(&format!(
            r#"
if 'rev-list' in sys.argv:
    child=subprocess.Popen(['/bin/sleep','60'])
    with open({pids:?},'w') as f: f.write(str(os.getpid())+' '+str(child.pid))
    while True: os.write({stream},b'x'*8192)
os.execv('/usr/bin/git',['git']+sys.argv[1:])
"#,
            pids = pids.to_str().unwrap()
        ));
        let now = Instant::now();
        let out = s.log_at(PATH, &[("RIVET_TEST_GIT", script)]);
        assert!(now.elapsed() < Duration::from_secs(10));
        assert_eq!(out["coverage"]["boundary"], boundary);
        assert_eq!(out["coverage"]["complete"], false);
        let pids: Vec<i32> = std::fs::read_to_string(pids)
            .unwrap()
            .split_whitespace()
            .map(|p| p.parse().unwrap())
            .collect();
        assert!(process_dead(pids[0]), "Git leader not reaped");
        let deadline = Instant::now() + Duration::from_secs(2);
        while !process_dead(pids[1]) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(process_dead(pids[1]), "descendant survived cutoff");
    }
}

#[test]
fn history_repeated_cached_escaped_content_caps_complete_json_response() {
    let s = Store::new("history-response-budget");
    // Two cached blobs and small metadata, but repeated response details exceed
    // the cap. This assertion fails if only decoded-blob accounting remains.
    let text = "\\\"".repeat(1_563);
    let one = A.replace("first", &format!("one {text}"));
    let two = A.replace("first", &format!("two {text}"));
    let message = format!("escaped metadata {}", "\\\"".repeat(16));
    for i in 0..100 {
        s.commit_at(
            PATH,
            if i % 2 == 0 {
                one.as_bytes()
            } else {
                two.as_bytes()
            },
            &message,
        );
    }
    let branch_shim = s.script(
        r#"
if 'symbolic-ref' in sys.argv:
    print('refs/heads/'+'b'*6300)
    sys.exit(0)
os.execv('/usr/bin/git',['git']+sys.argv[1:])
"#,
    );
    for (pretty, long_branch) in [(false, false), (true, false), (false, true)] {
        let mut cmd = raw_command();
        cmd.arg("--root").arg(&s.root);
        if pretty {
            cmd.arg("--pretty");
        }
        if long_branch {
            cmd.env("RIVET_TEST_GIT", &branch_shim);
        }
        cmd.env("RIVET_TEST_HISTORY_BYTES", "1048576");
        cmd.arg("item-log")
            .envs(s.env())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = cmd.spawn().unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(
                json!({"path":PATH,"collection":"actions","id":ID})
                    .to_string()
                    .as_bytes(),
            )
            .unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stdout)
        );
        assert!(output.stdout.len() <= 1024 * 1024);
        let out: Value = serde_json::from_slice(&output.stdout).unwrap();
        if long_branch {
            assert!(out["branch"].as_str().unwrap().len() > 6000);
        }
        assert_eq!(out["coverage"]["boundary"], "response_bytes");
        assert_eq!(out["coverage"]["complete"], false);
        assert_eq!(out["coverage"]["distinct_blobs"], 2);
        assert!(out["coverage"]["decoded_bytes"].as_u64().unwrap() < 16 * 1024);
        let rows = out["transitions"].as_array().unwrap().len();
        assert!(rows > 10 && rows < 100, "retained {rows} rows");
        let field = if pretty {
            "pretty_response_bytes"
        } else {
            "compact_response_bytes"
        };
        assert_eq!(
            out["coverage"][field].as_u64().unwrap() as usize,
            output.stdout.len()
        );
        assert!(out.to_string().contains("history_resource_limit"));
    }
}

#[test]
fn history_documented_example() {
    let s = Store::new("history-documented");
    s.commit(A);
    // Exact stdin and command from docs/usage.md, with the project root supplied.
    let request: Value =
        serde_json::from_str(r#"{"path":"history.md","collection":"actions","id":"abcdefgh"}"#)
            .unwrap();
    let before = snapshot(&s.root);
    let (code, out) = finish(spawn(&s.root, "item-log", &request, &s.env()));
    assert_eq!(code, 0, "{out}");
    assert_unchanged(&s.root, &before);
    assert_eq!(events(&out), ["appearance"]);
}

#[test]
fn history_multi_op_write_and_whole_file_adopt_observe_each_changed_item() {
    let s = Store::new("history-native-writes");
    let mut config: Value = serde_json::from_str(&read(&s.root, ".rivet/project.json")).unwrap();
    config["git"] = json!({"commit":"per-write","on_dirty":"adopt"});
    std::fs::write(s.root.join(".rivet/project.json"), config.to_string()).unwrap();
    s.commit(A);
    let select = |id| json!({"kind":"id","id":id});
    let ops = [(ID, "[ ] changed"), ("sibling1", "[ ] changed sibling")].map(|(id, content)| {
        let (_, got) = finish(spawn(
            &s.root,
            "get",
            &json!({"path":PATH,"select":select(id)}),
            &s.env(),
        ));
        json!({"op":"set","select":select(id),"guard":got["revision"],"content":content})
    });
    let (code, out) = finish(spawn(
        &s.root,
        "write",
        &json!({"path":PATH,"ops":ops}),
        &s.env(),
    ));
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["commit"]["status"], "committed", "{out}");
    let multi = s.git(&["rev-parse", "HEAD"]);
    let source = read(&s.root, PATH).replace("changed", "adopted");
    std::fs::write(s.root.join(PATH), source).unwrap();
    let (code, out) = finish(spawn(&s.root, "adopt", &json!({"path":PATH}), &s.env()));
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["commit"]["status"], "committed", "{out}");
    let adopted = s.git(&["rev-parse", "HEAD"]);
    let history = s.log();
    assert_eq!(
        events(&history),
        ["content_update", "content_update", "appearance"]
    );
    assert_eq!(history["transitions"][0]["commit"], adopted);
    assert_eq!(history["transitions"][1]["commit"], multi);
    let before = snapshot(&s.root);
    let (code, sibling) = finish(spawn(
        &s.root,
        "item-log",
        &json!({"path":PATH,"collection":"actions","id":"sibling1"}),
        &s.env(),
    ));
    assert_eq!(code, 0, "{sibling}");
    assert_unchanged(&s.root, &before);
    assert_eq!(sibling["transitions"][0]["commit"], adopted);
    assert_eq!(sibling["transitions"][1]["commit"], multi);
}

#[test]
fn history_metadata_and_aggregate_received_limits_are_independent() {
    let s = Store::new("history-metadata");
    s.commit(A);
    let message = s.home.join("large-message");
    std::fs::write(&message, "m".repeat(4 * 1024)).unwrap();
    s.git(&[
        "commit",
        "--allow-empty",
        "-q",
        "-F",
        message.to_str().unwrap(),
    ]);
    let out = s.log_at(
        PATH,
        &[("RIVET_TEST_HISTORY_METADATA_BYTES", "4096".into())],
    );
    assert_eq!(out["coverage"]["boundary"], "metadata_bytes");
    assert!(out.to_string().contains("history_resource_limit"));

    let s = Store::new("history-received");
    s.commit(A);
    let text = "\\\"".repeat(500);
    for i in 0..6 {
        s.commit(&A.replace("first", &format!("{i} {text}")));
    }
    // A legal amount of stderr for each call, but more than the request cap
    // across all raw-object and repository inspection calls.
    let script = s.script(
        r#"
os.write(2,b'x'*16384)
os.execv('/usr/bin/git',['git']+sys.argv[1:])
"#,
    );
    // Cross the lowered request ceiling while each stderr pipe stays legal.
    for _ in 0..20 {
        s.git(&["commit", "--allow-empty", "-qm", "same blob"]);
    }
    let out = s.log_at(
        PATH,
        &[
            ("RIVET_TEST_GIT", script),
            ("RIVET_TEST_HISTORY_BYTES", "1048576".into()),
        ],
    );
    assert_eq!(out["coverage"]["boundary"], "git_received");
    assert!(out["coverage"]["commits_scanned"].as_u64().unwrap() > 0);
    assert!(out["coverage"]["distinct_blobs"].as_u64().unwrap() > 0);
    assert!(out["coverage"]["git_received_bytes"].as_u64().unwrap() >= 1024 * 1024);
    assert!(out["coverage"]["decoded_bytes"].as_u64().unwrap() < 16 * 1024);
}

#[test]
fn history_deadline_closes_pipes_and_kills_group() {
    let s = Store::new("history-deadline");
    s.commit(A);
    let pids = s.home.join("pids");
    let script = s.script(&format!(
        r#"
if 'rev-list' in sys.argv:
    child=subprocess.Popen(['/bin/sleep','60'])
    with open({pids:?},'w') as f: f.write(str(os.getpid())+' '+str(child.pid))
    time.sleep(60)
os.execv('/usr/bin/git',['git']+sys.argv[1:])
"#,
        pids = pids.to_str().unwrap()
    ));
    let now = Instant::now();
    let out = s.log_at(
        PATH,
        &[
            ("RIVET_TEST_GIT", script),
            ("RIVET_TEST_GIT_DEADLINE_MS", "500".into()),
        ],
    );
    assert!(now.elapsed() < Duration::from_secs(10));
    assert_eq!(out["coverage"]["complete"], false);
    let pids: Vec<i32> = std::fs::read_to_string(pids)
        .unwrap()
        .split_whitespace()
        .map(|p| p.parse().unwrap())
        .collect();
    assert!(process_dead(pids[0]));
    let until = Instant::now() + Duration::from_secs(2);
    while !process_dead(pids[1]) && Instant::now() < until {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(process_dead(pids[1]));
}

#[test]
fn history_sha256_oids_and_replace_objects_are_literal() {
    let s = Store::new("history-sha256");
    // Reinitialize an empty synthetic repository with the other supported format.
    std::fs::remove_dir_all(s.root.join(".git")).unwrap();
    s.git(&["init", "-q", "-b", "main", "--object-format=sha256"]);
    s.git(&["config", "user.name", "Synthetic Tester"]);
    s.git(&["config", "user.email", "tester@example.invalid"]);
    let first = s.commit(A);
    let tip = s.commit(&A.replace("first", "second"));
    s.git(&["replace", &tip, &first]);
    let out = s.log();
    assert_eq!(out["history_tip"].as_str().unwrap().len(), 64);
    assert_eq!(events(&out), ["content_update", "appearance"]);
    assert_eq!(
        out["transitions"][0]["after"]["state"]["content"],
        "[ ] second"
    );
}

#[test]
fn history_list_identity_moves_and_unsupported_location_are_visible() {
    let s = Store::new("history-list-location");
    let doc = "## Queue\n<!-- stay:sectionA -->\n\n- [ ] first <!-- stay:abcdefgh subhash=sha256:000000000000 -->\n<!-- stay:firstList -->\n\nseparator\n\n- sibling\n<!-- stay:otherList -->\n";
    s.commit(doc);
    s.commit(
        &doc.replace("firstList", "tempList")
            .replace("otherList", "firstList")
            .replace("tempList", "otherList"),
    );
    s.commit(
        &doc.replace("<!-- stay:firstList -->", "")
            .replace("<!-- stay:otherList -->", ""),
    );
    let out = s.log();
    assert_eq!(
        events(&out),
        ["location_change", "location_change", "appearance"]
    );
    assert_eq!(
        out["transitions"][0]["after"]["state"]["location"]["supported"],
        false
    );
    assert_eq!(
        out["transitions"][1]["after"]["state"]["location"]["parent_list"]["ids"][0],
        "otherList"
    );
    // Renaming a stayed heading is not a move or an item content change.
    let s = Store::new("history-heading-rename");
    s.commit(A);
    s.commit(&A.replace("Queue", "Renamed queue"));
    assert_eq!(events(&s.log()), ["appearance"]);
}

#[test]
fn history_directories_submodules_and_quoted_carriers_are_not_items() {
    let s = Store::new("history-tree-mode");
    std::fs::create_dir_all(s.root.join(PATH)).unwrap();
    s.commit_at(&format!("{PATH}/child.md"), A.as_bytes(), "tree entry");
    let out = s.log();
    assert_eq!(out["transitions"][0]["after"]["mode"], "040000");
    assert!(out.to_string().contains("unsupported_snapshot_mode"));
    let commit = s.git(&["rev-parse", "HEAD"]);
    s.git(&[
        "update-index",
        "--add",
        "--cacheinfo",
        &format!("160000,{commit},module.md"),
    ]);
    s.git(&["commit", "-qm", "gitlink"]);
    let out = s.log_at("module.md", &[]);
    assert_eq!(out["transitions"][0]["after"]["mode"], "160000");
    let s = Store::new("history-quoted-carriers");
    s.commit("- [ ] first `<!-- stay:abcdefgh subhash=sha256:000000000000 -->`\n");
    assert_eq!(s.log()["transitions"], json!([]));
}

#[test]
fn history_missing_tip_object_is_not_complete_empty_history() {
    let s = Store::new("history-missing-tip");
    let tip = s.commit(A);
    std::fs::remove_file(s.root.join(".git/objects").join(&tip[..2]).join(&tip[2..])).unwrap();
    let out = s.log();
    assert_eq!(out["history_tip"], tip);
    assert_eq!(out["coverage"]["complete"], false);
    assert_eq!(out["coverage"]["root_reached"], false);
    assert!(out.to_string().contains("missing_object"));
    let s = Store::new("history-unborn-control");
    let out = s.log();
    assert!(out["history_tip"].is_null());
    assert_eq!(out["coverage"]["complete"], true);
    assert_eq!(out["transitions"], json!([]));
}

#[test]
fn history_unborn_inspection_limits_and_deadlines_keep_their_reasons() {
    for call in ["raw_head", "show_ref"] {
        for (fault, boundary) in [
            ("stdout", "git_stdout"),
            ("stderr", "git_stderr"),
            ("deadline", "git_deadline"),
        ] {
            let s = Store::new(&format!("history-inspection-{call}-{fault}"));
            let script = s.script(&format!(
                r#"
args=sys.argv[1:]
hit=('rev-parse' in args and args[-1]=='HEAD') if {call:?}=='raw_head' else 'show-ref' in args
if hit:
    if {fault:?}=='deadline': time.sleep(60)
    else:
        while True: os.write(1 if {fault:?}=='stdout' else 2,b'x'*8192)
os.execv('/usr/bin/git',['git']+args)
"#
            ));
            let out = s.log_at(
                PATH,
                &[
                    ("RIVET_TEST_GIT", script),
                    ("RIVET_TEST_GIT_DEADLINE_MS", "1000".into()),
                ],
            );
            assert!(out["history_tip"].is_null());
            assert_eq!(out["coverage"]["complete"], false, "{call}/{fault}: {out}");
            assert_eq!(
                out["coverage"]["boundary"], boundary,
                "{call}/{fault}: {out}"
            );
            let reason = if fault == "deadline" {
                "git_deadline"
            } else {
                "history_resource_limit"
            };
            assert!(out.to_string().contains(reason));
        }
    }
}

impl Store {
    fn restoration(name: &str, before: &str, after: &str) -> (Self, String, Value) {
        let s = Self::new(name);
        let config_path = s.root.join(".rivet/project.json");
        let mut config: Value =
            serde_json::from_str(&read(&s.root, ".rivet/project.json")).unwrap();
        config["git"] = json!({"commit":"per-write","on_dirty":"refuse"});
        config["collections"]["actions"]["section"] = json!("Queue");
        config["collections"]["actions"]["files"] = json!([PATH]);
        std::fs::write(config_path, config.to_string()).unwrap();
        let old = s.commit(before);
        s.commit(after);
        let log = s.log();
        assert!(log["restore_guard"].is_string(), "{log}");
        let request = json!({"path":PATH,"collection":"actions","id":ID,
            "history_tip":log["history_tip"],"guard":log["restore_guard"],"rev":old});
        (s, old, request)
    }
    fn restore(&self, request: &Value) -> (i32, Value) {
        finish(spawn(&self.root, "revert-item", request, &self.env()))
    }
    fn refuses(&self, request: &Value, reason: &str) {
        let bytes = read(&self.root, PATH);
        let head = self.git(&["rev-parse", "HEAD"]);
        let (code, out) = self.restore(request);
        assert_eq!(code, 2, "{out}");
        assert_eq!(out["error"]["code"], reason, "{out}");
        assert_eq!(read(&self.root, PATH), bytes);
        assert_eq!(self.git(&["rev-parse", "HEAD"]), head);
    }
}

#[test]
fn revert_documented_example_preserves_bytes_and_attachment() {
    let changed = A.replace("first", "second");
    let (s, old, request) = Store::restoration("revert-example", A, &changed);
    let (code, out) = s.restore(&request);
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["resolved_rev"], old);
    assert_eq!(out["commit"]["status"], "committed");
    let doc = markrivet_core::Document::parse(&read(&s.root, PATH)).unwrap();
    let markrivet_core::document::Target::Item { list, item } = doc
        .resolve(&markrivet_core::Selector::Id { id: ID.into() })
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(doc.item_content(list, item), "[ ] first");
    // The own digest changes; the sibling's exact source and heading stay survive.
    assert!(read(&s.root, PATH).starts_with("## Queue\n<!-- stay:sectionA -->\n\n"));
    let source = read(&s.root, PATH);
    let span = doc.markers[doc.lists[list].items[item].markers[0]]
        .lexical
        .span;
    let expected = A.replace(
        "<!-- stay:abcdefgh subhash=sha256:000000000000 -->",
        &source[span.start..span.end],
    );
    assert_eq!(source, expected);
    let message = s.git(&["log", "-1", "--format=%B"]);
    assert!(message.contains(&old));
    assert!(!message.contains("Rivet-Delete:"));
    assert!(message.contains("Rivet-Op:"));
}

#[test]
fn revert_preview_without_lock_file_and_verified_noop_are_pure() {
    let (s, old, mut request) =
        Store::restoration("revert-purity", A, &A.replace("first", "second"));
    let _ = std::fs::remove_file(s.root.join(".rivet/lock"));
    request["preview"] = json!(true);
    let before = snapshot(&s.root);
    let (code, out) = s.restore(&request);
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["resolved_rev"], old);
    assert_unchanged(&s.root, &before);
    request["guard"] = json!("wrong");
    s.refuses(&request, "guard_conflict");
    assert_unchanged(&s.root, &before);
    let log = s.log();
    request["guard"] = log["restore_guard"].clone();
    request["rev"] = log["history_tip"].clone();
    request["preview"] = json!(false);
    // Actual no-op takes the lock, but makes no receipt, replacement or commit.
    let head = s.git(&["rev-parse", "HEAD"]);
    let bytes = read(&s.root, PATH);
    let (code, out) = s.restore(&request);
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["status"], "unchanged");
    assert_eq!(read(&s.root, PATH), bytes);
    assert_eq!(s.git(&["rev-parse", "HEAD"]), head);
    assert!(!s.root.join(".rivet/commits.jsonl").exists());
}

#[test]
fn revert_default_selects_previous_content_and_has_no_absent_preimage() {
    let (s, old, mut request) =
        Store::restoration("revert-default", A, &A.replace("first", "second"));
    request.as_object_mut().unwrap().remove("rev");
    request["preview"] = json!(true);
    let (code, out) = s.restore(&request);
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["resolved_rev"], old);
    let s = Store::new("revert-insertion");
    let config = json!({"git":{"commit":"per-write"},"collections":{"actions":{"schema":"tracker-action@1","files":[PATH],"section":"Queue"}}});
    std::fs::write(s.root.join(".rivet/project.json"), config.to_string()).unwrap();
    s.commit(A);
    let log = s.log();
    let request = json!({"path":PATH,"collection":"actions","id":ID,"history_tip":log["history_tip"],"guard":log["restore_guard"]});
    s.refuses(&request, "history_no_previous_version");
}

#[test]
fn revert_authored_session_default_is_separate_from_strict_fields() {
    let before = A.replace("first", "first\n  **Session**: s1");
    let (s, _, mut request) = Store::restoration("revert-session", A, &before);
    let p = s.root.join(".rivet/schemas/tracker-action.json");
    let mut schema: Value = serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
    schema["fields"]["session"] =
        json!({"type":"string","from":"label","label":"Session","default":"s1"});
    std::fs::write(p, schema.to_string()).unwrap();
    request["guard"] = s.log()["restore_guard"].clone();
    request["preview"] = json!(true);
    let all = snapshot(&s.root);
    let (code, out) = s.restore(&request);
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["current"]["authored"]["session"], "s1");
    assert_eq!(out["selected"]["authored"]["session"], Value::Null);
    assert_eq!(out["selected"]["fields"]["session"], "s1");
    assert_unchanged(&s.root, &all);
}

#[test]
fn revert_rejects_wrong_guard_oid_and_schema_changes() {
    let (s, _, mut request) =
        Store::restoration("revert-refusals", A, &A.replace("first", "second"));
    let good = request.clone();
    request["guard"] = json!(item_guard(&s.root, PATH, ID));
    s.refuses(&request, "guard_conflict");
    for bad in ["HEAD", "main", "-n", "abc", &"f".repeat(39)] {
        request = good.clone();
        request["rev"] = json!(bad);
        s.refuses(&request, "invalid_request");
    }
    request = good.clone();
    request["rev"] = json!("f".repeat(40));
    s.refuses(&request, "history_revision_missing");
    let p = s.root.join(".rivet/schemas/tracker-action.json");
    let mut schema: Value = serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
    schema["fields"]["gate"]["required"] = json!(true);
    std::fs::write(p, schema.to_string()).unwrap();
    s.refuses(&good, "guard_conflict");
    request = good;
    request["guard"] = s.log()["restore_guard"].clone();
    s.refuses(&request, "invalid_item");
}

#[test]
fn revert_refuses_foreign_stays_attributes_and_reused_identity() {
    for (name, before, reason) in [
        (
            "nested",
            A.replace(
                "- [ ] sibling",
                "  - [ ] child <!-- stay:nested01 subhash=sha256:000000000000 -->\n- [ ] sibling",
            ),
            "unsupported_syntax",
        ),
        (
            "attributes",
            A.replace("stay:abcdefgh subhash=", "stay:abcdefgh quote=old subhash="),
            "unsupported_syntax",
        ),
    ] {
        let (s, _, request) = Store::restoration(
            &format!("revert-{name}"),
            &before,
            &A.replace("first", "second"),
        );
        s.refuses(&request, reason);
    }
    let (s, old, _) = Store::restoration("revert-lineage", A, &A.replace("first", "second"));
    s.commit(&A.replace(
        "- [ ] first <!-- stay:abcdefgh subhash=sha256:000000000000 -->\n",
        "",
    ));
    s.commit(A);
    let log = s.log();
    let request = json!({"path":PATH,"collection":"actions","id":ID,"history_tip":log["history_tip"],"guard":log["restore_guard"],"rev":old});
    s.refuses(&request, "history_lineage_boundary");
}

#[test]
fn revert_preserves_current_newlines_prefix_and_carrier() {
    for (name, old, new) in [
        (
            "crlf",
            A.replace('\n', "\r\n"),
            A.replace("first", "second").replace('\n', "\r\n"),
        ),
        (
            "tab",
            A.replace("first", "first\n\tcontinued"),
            A.replace("first", "second"),
        ),
        (
            "lazy",
            A.replace("first", "first\nlazy text"),
            A.replace("first", "second"),
        ),
        (
            "quoted",
            A.replace("first", "first `<!-- stay:quoted01 -->`"),
            A.replace("first", "second"),
        ),
    ] {
        let (s, _, request) = Store::restoration(&format!("revert-{name}"), &old, &new);
        let selected = markrivet_core::Document::parse(&old).unwrap();
        let markrivet_core::document::Target::Item { list, item } = selected
            .resolve(&markrivet_core::Selector::Id { id: ID.into() })
            .unwrap()
        else {
            panic!()
        };
        let expect = selected.item_content(list, item).replace("\r\n", "\n");
        let (code, out) = s.restore(&request);
        assert_eq!(code, 0, "{name}: {out}");
        let source = read(&s.root, PATH);
        let doc = markrivet_core::Document::parse(&source).unwrap();
        let markrivet_core::document::Target::Item { list, item } = doc
            .resolve(&markrivet_core::Selector::Id { id: ID.into() })
            .unwrap()
        else {
            panic!()
        };
        assert_eq!(doc.item_content(list, item).replace("\r\n", "\n"), expect);
        assert!(
            source.ends_with(new.lines().last().unwrap().trim_end_matches('\r'))
                || source
                    .trim_end()
                    .ends_with(new.lines().last().unwrap().trim_end_matches('\r'))
        );
    }
}

#[test]
fn revert_rewritten_and_second_parent_only_selectors_are_unreachable() {
    let (s, old, request) = Store::restoration("revert-rewrite", A, &A.replace("first", "second"));
    let tip = s.git(&["rev-parse", "HEAD"]);
    let tree = s.git(&["rev-parse", "HEAD^{tree}"]);
    // A replacement commit retains tree and forged operation evidence, but has a new identity.
    let rewritten = s.git(&[
        "commit-tree",
        &tree,
        "-m",
        "rewritten\n\nRivet-Op: synthetic",
    ]);
    s.git(&["update-ref", "refs/heads/main", &rewritten, &tip]);
    s.refuses(&request, "history_revision_unreachable");
    s.git(&["cat-file", "-t", &old]); // Old object existence cannot repair the selector.
    let log = s.log();
    let mut r = request;
    r["history_tip"] = log["history_tip"].clone();
    r["guard"] = log["restore_guard"].clone();
    // Put the old history on a merge's second parent, which first-parent reads omit.
    let merge = s.git(&[
        "commit-tree",
        &tree,
        "-p",
        &rewritten,
        "-p",
        &tip,
        "-m",
        "integration",
    ]);
    s.git(&["update-ref", "refs/heads/main", &merge, &rewritten]);
    let log = s.log();
    r["history_tip"] = log["history_tip"].clone();
    r["guard"] = log["restore_guard"].clone();
    s.refuses(&r, "history_revision_unreachable");
}

#[test]
fn revert_absent_duplicate_kind_and_unsupported_location_refusals() {
    let changed = A.replace("first", "second");
    let (s, _, mut r) = Store::restoration("revert-absent", A, &changed);
    s.commit(&A.replace(
        "- [ ] first <!-- stay:abcdefgh subhash=sha256:000000000000 -->\n",
        "",
    ));
    s.refuses(&r, "history_target_absent");
    s.commit(
        &(changed.clone()
            + "\n- [ ] duplicate <!-- stay:abcdefgh subhash=sha256:000000000000 -->\n"),
    );
    s.refuses(&r, "history_identity_ambiguous");
    s.commit(&changed.replace(
        "- [ ] second <!-- stay:abcdefgh subhash=sha256:000000000000 -->",
        "Paragraph.\n<!-- stay:abcdefgh -->",
    ));
    s.refuses(&r, "history_kind_changed");
    s.commit(&(changed.clone() + "\nParagraph splitting lists.\n\n- [ ] another list.\n"));
    s.refuses(&r, "history_location_unsupported");
    s.commit(&changed);
    let p = s.root.join(".rivet/project.json");
    let mut c: Value = serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
    c["git"] = json!({"commit":"off"});
    std::fs::write(p, c.to_string()).unwrap();
    r["preview"] = json!(true);
    s.refuses(&r, "commit_refused");
}

#[test]
fn revert_stayed_parent_lists_and_section_title_changes() {
    let source = "## Queue\n<!-- stay:sectionA -->\n\n- [ ] first <!-- stay:abcdefgh subhash=sha256:000000000000 -->\n\n<!-- stay:list0001 -->\n\nProse between lists.\n\n- [ ] sibling <!-- stay:sibling1 subhash=sha256:000000000000 -->\n\n<!-- stay:list0002 -->\n";
    let (s, _, mut r) = Store::restoration(
        "revert-stayed-list",
        source,
        &source.replace("first", "second"),
    );
    let mut preview = r.clone();
    preview["preview"] = json!(true);
    let (code, out) = s.restore(&preview);
    assert_eq!(code, 0, "{out}");
    // Swap the parent-list identities while preserving all item bytes.
    let text = read(&s.root, PATH)
        .replace("list0001", "temporary")
        .replace("list0002", "list0001")
        .replace("temporary", "list0002");
    s.commit(&text);
    s.refuses(&r, "guard_conflict");
    let log = s.log();
    r["guard"] = log["restore_guard"].clone();
    r["history_tip"] = log["history_tip"].clone();
    s.refuses(&r, "history_location_changed");
}

#[test]
fn revert_same_content_preserves_supported_carrier_formatting() {
    let original = A.to_string();
    let current = original.replace(
        "first <!-- stay:abcdefgh subhash=sha256:000000000000 -->",
        "first\n  <!-- stay:abcdefgh subhash=sha256:000000000000 -->",
    );
    let (s, _, r) = Store::restoration("revert-format-noop", &original, &current);
    let head = s.git(&["rev-parse", "HEAD"]);
    let before = read(&s.root, PATH);
    let (code, out) = s.restore(&r);
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["status"], "unchanged");
    assert_eq!(read(&s.root, PATH), before);
    assert_eq!(s.git(&["rev-parse", "HEAD"]), head);
    assert!(!s.root.join(".rivet/commits.jsonl").exists());
}

#[test]
fn revert_section_rename_keeps_stable_location() {
    let (s, _, mut request) =
        Store::restoration("revert-title-rename", A, &A.replace("first", "second"));
    let renamed = read(&s.root, PATH).replace("## Queue", "## Renamed");
    s.commit(&renamed);
    let p = s.root.join(".rivet/project.json");
    let mut config: Value = serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
    config["collections"]["actions"]["section"] = json!("Renamed");
    std::fs::write(p, config.to_string()).unwrap();
    let log = s.log();
    request["history_tip"] = log["history_tip"].clone();
    request["guard"] = log["restore_guard"].clone();
    let (code, out) = s.restore(&request);
    assert_eq!(code, 0, "{out}");
    assert!(read(&s.root, PATH).starts_with("## Renamed\n<!-- stay:sectionA -->"));
    assert_eq!(
        read(&s.root, PATH).matches("<!-- stay:abcdefgh ").count(),
        1
    );
    let doc = markrivet_core::Document::parse(&read(&s.root, PATH)).unwrap();
    let markrivet_core::document::Target::Item { list, item } = doc
        .resolve(&markrivet_core::Selector::Id { id: ID.into() })
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(doc.item_content(list, item), "[ ] first");
}

#[test]
fn revert_selected_duplicate_and_missing_history_refuse_without_writing() {
    let duplicate =
        A.to_string() + "\n- [ ] repeated <!-- stay:abcdefgh subhash=sha256:000000000000 -->\n";
    let (s, _, request) = Store::restoration(
        "revert-selected-duplicate",
        &duplicate,
        &A.replace("first", "second"),
    );
    s.refuses(&request, "history_identity_ambiguous");
    let (s, _, request) =
        Store::restoration("revert-reserved-message", A, &A.replace("first", "second"));
    for key in ["Rivet-Delete", "Rivet-Op", "Rivet-Archive"] {
        let mut r = request.clone();
        r["message"] = json!(format!("Restore\n\n{key}: forged"));
        s.refuses(&r, "invalid_request");
    }
}

#[test]
fn revert_binding_input_changes_invalidate_guard_without_removing_membership() {
    for variant in ["files", "subsections", "exclude", "level", "archive"] {
        let (s, _, request) = Store::restoration(
            &format!("revert-binding-{variant}"),
            A,
            &A.replace("first", "second"),
        );
        let p = s.root.join(".rivet/project.json");
        let mut c: Value = serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        let b = &mut c["collections"]["actions"];
        match variant {
            "files" => b["files"] = json!([PATH, "elsewhere/*.md"]),
            "subsections" => b["subsections"] = json!(true),
            "exclude" => {
                b["subsections"] = json!(true);
                b["exclude"] = json!(["Excluded unrelated section"]);
            }
            "level" => b["level"] = Value::Null,
            "archive" => b["archive"] = json!(["archive/**/*.md"]),
            _ => unreachable!(),
        }
        std::fs::write(p, c.to_string()).unwrap();
        let log = s.log();
        assert!(log["restore_guard"].is_string(), "{variant}: {log}");
        s.refuses(&request, "guard_conflict");
    }
}

#[test]
fn revert_inspection_pipe_limits_are_resource_refusals_and_pure() {
    for channel in [1, 2] {
        let (s, _, mut request) = Store::restoration(
            &format!("revert-inspection-cap-{channel}"),
            A,
            &A.replace("first", "second"),
        );
        request["preview"] = json!(true);
        let shim = s.script(&format!("os.write({channel},b'x'*(2*1024*1024))"));
        let mut vars = s.env();
        vars.push(("RIVET_TEST_GIT", shim));
        let before = snapshot(&s.root);
        let (code, out) = finish(spawn(&s.root, "revert-item", &request, &vars));
        assert_eq!(code, 2, "{out}");
        assert_eq!(out["error"]["code"], "history_resource_limit", "{out}");
        assert_unchanged(&s.root, &before);
    }
}
