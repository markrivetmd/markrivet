//! Git children as real processes: the launch helper's PID handoff, death
//! with Rivet, the held root, and `markrivet status` over a real repository.
//! Every Rivet process gets HOME and XDG_CONFIG_HOME pointing at an empty
//! directory, so no user git configuration reaches it.
mod common;
use common::*;
use serde_json::{Value, json};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

struct Repo {
    root: PathBuf,
    home: PathBuf,
}

impl Repo {
    /// The tracker example, stamped and committed on `main`.
    fn new(name: &str) -> Repo {
        let root = project(name);
        let home = root.with_extension("home");
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        let r = Repo { root, home };
        r.git(&["init", "-q", "-b", "main"]);
        r.git(&["config", "user.name", "Tester"]);
        r.git(&["config", "user.email", "tester@example.invalid"]);
        stamp(&r.root, WIDGETS);
        stamp(&r.root, "tracking/TRACKING_DOCS.md");
        r.git(&["add", "-A"]);
        r.git(&["commit", "-qm", "init"]);
        r
    }

    fn git(&self, args: &[&str]) -> String {
        git_at(&self.root, &self.home, args)
    }

    fn env(&self) -> Vec<(&'static str, String)> {
        vec![
            ("HOME", self.home.display().to_string()),
            ("XDG_CONFIG_HOME", self.home.display().to_string()),
        ]
    }

    fn run(&self, cmd: &str, request: Value, extra: &[(&'static str, String)]) -> (i32, Value) {
        let mut env = self.env();
        env.extend_from_slice(extra);
        finish(spawn(&self.root, cmd, &request, &env))
    }

    fn status(&self) -> Value {
        let (code, out) = self.run("status", json!({}), &[]);
        assert_eq!(code, 0, "{out}");
        out
    }

    /// A shell script under the home directory, for `RIVET_TEST_GIT`.
    fn script(&self, name: &str, body: &str) -> String {
        let path = self.home.join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path.display().to_string()
    }
}

fn git_at(dir: &Path, home: &Path, args: &[&str]) -> String {
    let out = git_command(dir, home, args).output().unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

fn wait_for(path: &Path) -> String {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Ok(text) = std::fs::read_to_string(path)
            && !text.is_empty()
        {
            return text;
        }
        assert!(
            Instant::now() < deadline,
            "{} never appeared",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Running, not merely a zombie awaiting its new parent.
fn running(pid: u32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .ok()
        .and_then(|s| s.rsplit_once(") ").map(|(_, r)| !r.starts_with('Z')))
        .unwrap_or(false)
}

fn gone_soon(pid: u32) -> bool {
    let deadline = Instant::now() + Duration::from_secs(5);
    while running(pid) {
        if Instant::now() > deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    true
}

/// Kills a stray process if a test fails while it is still running.
struct Reap(u32);
impl Drop for Reap {
    fn drop(&mut self) {
        // SAFETY: signalling a process id this test started.
        unsafe { libc::kill(self.0 as libc::pid_t, libc::SIGKILL) };
    }
}

fn sleeper(r: &Repo, marker: &str) -> String {
    r.script(
        "sleeper",
        &format!(
            "echo $$ > {0}/{marker}\nwhile [ ! -e {0}/never ]; do sleep 0.05; done",
            r.home.display()
        ),
    )
}

#[test]
fn a_helper_launched_git_keeps_its_pid_and_dies_with_rivet() {
    let r = Repo::new("helper-pid");
    let git = sleeper(&r, "git.pid");
    let pause = Pause::new(&r.root, "git_spawned");
    let mut env = r.env();
    env.extend([("RIVET_TEST_GIT", git), pause.env()]);
    let mut rivet = spawn(
        &r.root,
        "__test-git",
        &json!({"args": ["status"], "mutate": true}),
        &env,
    );
    pause.wait();
    let helper: u32 = wait_for(&pause.dir.join("git_spawned.reached"))
        .trim()
        .parse()
        .unwrap();
    let _reap = Reap(helper);
    // Git has not started: the helper waits for the go byte.
    std::thread::sleep(Duration::from_millis(100));
    assert!(
        !r.home.join("git.pid").exists(),
        "git ran before the go byte"
    );
    pause.release();
    let git_pid: u32 = wait_for(&r.home.join("git.pid")).trim().parse().unwrap();
    assert_eq!(git_pid, helper, "git kept the helper's PID");
    rivet.kill().unwrap();
    rivet.wait().unwrap();
    assert!(gone_soon(git_pid), "git outlived Rivet");
}

#[test]
fn rivet_killed_before_the_death_signal_is_armed_leaves_no_child() {
    let r = Repo::new("pdeathsig");
    let git = sleeper(&r, "git.pid");
    let pause = Pause::new(&r.root, "git_child");
    let mut env = r.env();
    env.extend([("RIVET_TEST_GIT", git), pause.env()]);
    let mut rivet = spawn(&r.root, "__test-git", &json!({"args": ["status"]}), &env);
    pause.wait();
    let child: u32 = wait_for(&pause.dir.join("git_child.reached"))
        .trim()
        .parse()
        .unwrap();
    let _reap = Reap(child);
    // The child is between setpgid and PR_SET_PDEATHSIG when Rivet dies.
    rivet.kill().unwrap();
    rivet.wait().unwrap();
    pause.release();
    assert!(gone_soon(child), "the orphaned child kept running");
    std::thread::sleep(Duration::from_millis(100));
    assert!(!r.home.join("git.pid").exists(), "git ran with no parent");
}

#[test]
fn a_git_child_reads_the_held_root_after_the_path_is_swapped() {
    let r = Repo::new("root-swap");
    let original = r.git(&["rev-parse", "HEAD"]);
    let gate = r.home.display().to_string();
    let git = r.script(
        "gated",
        &format!(
            "touch {gate}/git.reached\nwhile [ ! -e {gate}/git.go ]; do sleep 0.01; done\nexec git \"$@\""
        ),
    );
    let mut env = r.env();
    env.push(("RIVET_TEST_GIT", git));
    let rivet = spawn(
        &r.root,
        "__test-git",
        &json!({"args": ["rev-parse", "HEAD"]}),
        &env,
    );
    wait_for_file(&r.home.join("git.reached"));
    // Replace the root path with a different repository.
    // The fixture home is cleared on allocation, including any earlier moved root.
    let moved = r.home.join("moved");
    std::fs::rename(&r.root, &moved).unwrap();
    std::fs::create_dir_all(r.root.join(".rivet")).unwrap();
    git_at(&r.root, &r.home, &["init", "-q", "-b", "main"]);
    std::fs::write(r.root.join("impostor.md"), "# impostor\n").unwrap();
    git_at(&r.root, &r.home, &["add", "-A"]);
    git_at(
        &r.root,
        &r.home,
        &[
            "-c",
            "user.name=x",
            "-c",
            "user.email=x@example.invalid",
            "commit",
            "-qm",
            "impostor",
        ],
    );
    let impostor = git_at(&r.root, &r.home, &["rev-parse", "HEAD"]);
    assert_ne!(impostor, original);
    std::fs::write(r.home.join("git.go"), "").unwrap();
    let (code, out) = finish(rivet);
    assert_eq!(code, 0, "{out}");
    assert_eq!(
        out["stdout"], original,
        "git read the repository Rivet opened"
    );
}

fn wait_for_file(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !path.exists() {
        assert!(
            Instant::now() < deadline,
            "{} never appeared",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn path_report<'a>(out: &'a Value, path: &str) -> &'a Value {
    out["paths"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["path"] == path)
        .unwrap_or_else(|| panic!("{path} missing from {out}"))
}

fn ids(list: &Value) -> Vec<String> {
    list.as_array()
        .unwrap()
        .iter()
        .map(|u| u["id"].as_str().unwrap().to_string())
        .collect()
}

#[test]
fn status_reports_a_clean_project_and_leaves_the_index_alone() {
    let r = Repo::new("status-clean");
    let index = r.root.join(".git/index");
    // Make the index's stat data stale, so a refreshing command would rewrite it.
    std::thread::sleep(Duration::from_millis(1100));
    for p in [WIDGETS, "tracking/TRACKING_DOCS.md"] {
        let text = read(&r.root, p);
        std::fs::write(r.root.join(p), text).unwrap();
    }
    let before = (
        std::fs::read(&index).unwrap(),
        std::fs::metadata(&index).unwrap().modified().unwrap(),
    );
    let out = r.status();
    let after = (
        std::fs::read(&index).unwrap(),
        std::fs::metadata(&index).unwrap().modified().unwrap(),
    );
    assert_eq!(before, after, "status wrote the index");
    assert_eq!(out["complete"], true);
    assert!(!r.root.join(".git/index.lock").exists());
    assert_eq!(out["git"]["branch"], "refs/heads/main");
    assert_eq!(out["git"]["ineligible"], Value::Null);
    let paths = out["paths"].as_array().unwrap();
    assert_eq!(paths.len(), 2);
    for p in paths {
        assert_eq!(p["base"], "known_clean", "{p}");
        assert_eq!(p["ineligible"], Value::Null);
        assert!(p.get("units").is_none());
        assert!(p["read_at"].as_u64().unwrap() >= out["git"]["read_at"].as_u64().unwrap());
    }
}

#[test]
fn status_reports_a_deleted_tracker() {
    let r = Repo::new("status-deleted");
    std::fs::remove_file(r.root.join("tracking/TRACKING_DOCS.md")).unwrap();
    let out = r.status();
    let p = path_report(&out, "tracking/TRACKING_DOCS.md");
    assert_eq!(p["base"], "known_dirty");
    assert_eq!(p["tree"]["exists"], false);
    assert!(!p["units"]["deleted"].as_array().unwrap().is_empty(), "{p}");
    assert_eq!(path_report(&out, WIDGETS)["base"], "known_clean");
}

#[test]
fn status_marks_failed_git_enumeration_incomplete_but_keeps_tree_paths() {
    let r = Repo::new("status-list-failed");
    let git = r.script(
        "nolist",
        "case \"$*\" in *ls-tree*) exit 128;; esac\nexec git \"$@\"",
    );
    let (code, out) = r.run("status", json!({}), &[("RIVET_TEST_GIT", git.clone())]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["complete"], false, "{out}");
    assert_eq!(out["paths"].as_array().unwrap().len(), 2);
    let (code, out) = r.run(
        "status",
        json!({"paths":[WIDGETS]}),
        &[("RIVET_TEST_GIT", git)],
    );
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["complete"], true, "{out}");
}

#[test]
fn status_accounts_for_unrepresentable_paths_only_in_head_or_index() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;
    let r = Repo::new("status-native-git");
    for (bytes, content) in [
        (&b"tracking/\xfe.md"[..], "## Action\n"),
        (&b"tracking/\xfe.txt"[..], "not a document\n"),
        (&b"outside/\xfe.md"[..], "## Action\n"),
    ] {
        let path = r.root.join(OsStr::from_bytes(bytes));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }
    r.git(&["add", "-A"]);
    // Index-only paths remain relevant after their working-tree entries vanish.
    for directory in ["tracking", "outside"] {
        std::fs::remove_file(r.root.join(directory).join(OsStr::from_bytes(b"\xfe.md"))).unwrap();
    }
    std::fs::remove_file(r.root.join("tracking").join(OsStr::from_bytes(b"\xfe.txt"))).unwrap();
    let out = r.status();
    assert_eq!(out["complete"], false, "{out}");
    assert!(
        out["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|d| d["code"] == "path_rejected" && d["path"] == "tracking"),
        "{out}"
    );
    assert!(
        !out["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|d| d["path"] == "outside"),
        "{out}"
    );
    r.git(&["commit", "-qm", "native paths"]);
    r.git(&["add", "-u"]);
    // HEAD-only names also leave a gap. The valid paths are still reported.
    let out = r.status();
    assert_eq!(out["complete"], false, "{out}");
    assert_eq!(out["paths"].as_array().unwrap().len(), 2);
    // Once Git no longer carries a matching native name, enumeration closes.
    r.git(&["commit", "-qm", "remove native paths"]);
    assert_eq!(r.status()["complete"], true);
}

#[test]
fn status_checks_the_entire_native_git_path_against_the_binding() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;
    let r = Repo::new("status-native-directory");
    std::fs::write(
        r.root.join(".rivet/project.json"),
        json!({"collections":{"actions":{"schema":"tracker-action@1","files":["tracking/*/one.md",".rivet/**/*.md"],"section":"Action","level":2}}}).to_string(),
    ).unwrap();
    for bytes in [
        &b"tracking/\xfe/other.md"[..],
        &b"tracking/\xfe/one.txt"[..],
        &b".rivet/\xfe/one.md"[..],
    ] {
        let path = r.root.join(OsStr::from_bytes(bytes));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "## Action\n").unwrap();
    }
    // Forced add includes .rivet, a namespace status deliberately omits.
    r.git(&["add", "-f", ".rivet", "tracking"]);
    for directory in ["tracking", ".rivet"] {
        std::fs::remove_dir_all(r.root.join(directory).join(OsStr::from_bytes(b"\xfe"))).unwrap();
    }
    assert_eq!(r.status()["complete"], true);
    let path = r.root.join(OsStr::from_bytes(b"tracking/\xfe/one.md"));
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, "## Action\n").unwrap();
    r.git(&["add", "tracking"]);
    std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    assert_eq!(r.status()["complete"], false);
}

#[test]
fn status_reports_a_moved_section() {
    let r = Repo::new("status-moved");
    let text = read(&r.root, WIDGETS);
    let start = text.find("### 2026-08-28").unwrap();
    let section = text[start..].to_string();
    let notes = text.find("## Log").unwrap();
    let moved = format!("{}{}\n{}", &text[..notes], section, &text[notes..start]);
    std::fs::write(r.root.join(WIDGETS), moved).unwrap();
    let out = r.status();
    let units = &path_report(&out, WIDGETS)["units"];
    let m = units["moved"]
        .as_array()
        .unwrap()
        .iter()
        .find(|u| u["kind"] == "section")
        .unwrap_or_else(|| panic!("{units}"));
    assert_eq!(m["from"], json!(["Widgets", "Log"]));
    assert_eq!(m["to"], json!(["Widgets", "Notes"]));
}

#[test]
fn status_reports_an_unstamped_item_and_residual_bytes() {
    let r = Repo::new("status-unstamped");
    let text = read(&r.root, WIDGETS)
        .replace("status: active", "status: paused")
        .replace(
            "## Notes",
            "- [ ] **A hand-added action. (2026-09-04)** Not stamped yet.\n\n## Notes",
        );
    std::fs::write(r.root.join(WIDGETS), text).unwrap();
    let out = r.status();
    let units = &path_report(&out, WIDGETS)["units"];
    let unstamped = units["unstamped"].as_array().unwrap();
    assert_eq!(unstamped.len(), 1, "{units}");
    assert_eq!(unstamped[0]["side"], "tree");
    assert!(units["residual"]["head_bytes"].is_u64(), "{units}");
    assert!(units["added"].as_array().unwrap().is_empty());
    assert!(units["deleted"].as_array().unwrap().is_empty());
}

#[test]
fn status_reports_a_reorder_that_keeps_every_revision() {
    let r = Repo::new("status-reorder");
    // Item stays carry a subhash; the first two are the two open actions.
    let items: Vec<String> = read(&r.root, WIDGETS)
        .split("<!-- stay:")
        .skip(1)
        .filter(|rest| rest[8..].starts_with(" subhash="))
        .map(|rest| rest[..8].to_string())
        .collect();
    let (a, b) = (&items[0], &items[1]);
    let before = (
        item_guard(&r.root, WIDGETS, a),
        item_guard(&r.root, WIDGETS, b),
    );
    let text = read(&r.root, WIDGETS);
    let ga = text.find("- [ ] **Ship").unwrap();
    let gb = text.find("- [ ] `agent`").unwrap();
    let gc = text.find("- [x]").unwrap();
    let swapped = format!(
        "{}{}{}{}",
        &text[..ga],
        &text[gb..gc],
        &text[ga..gb],
        &text[gc..]
    );
    std::fs::write(r.root.join(WIDGETS), swapped).unwrap();
    // Both items kept their revisions.
    let after = (
        item_guard(&r.root, WIDGETS, a),
        item_guard(&r.root, WIDGETS, b),
    );
    assert_eq!(before, after);
    let out = r.status();
    let units = &path_report(&out, WIDGETS)["units"];
    let moved = ids(&units["moved"]);
    assert_eq!(moved.len(), 1, "{units}");
    assert!(&moved[0] == a || &moved[0] == b);
    let changed = ids(&units["changed"]);
    assert!(!changed.contains(a) && !changed.contains(b), "{units}");
}

#[test]
fn status_reports_ineligibility_per_path() {
    let r = Repo::new("status-staged");
    let text = read(&r.root, WIDGETS).replace("status: active", "status: paused");
    std::fs::write(r.root.join(WIDGETS), text).unwrap();
    r.git(&["add", WIDGETS]);
    let out = r.status();
    let p = path_report(&out, WIDGETS);
    assert_eq!(p["base"], "known_dirty");
    assert_eq!(p["staged"], true);
    assert_eq!(p["ineligible"]["reason"], "staged");
    // Detached HEAD blocks every path, and the report says so once.
    r.git(&["checkout", "-q", "--detach"]);
    let out = r.status();
    assert_eq!(out["git"]["ineligible"]["reason"], "no_branch");
}

#[test]
fn status_outside_a_repository_reports_the_layout() {
    let root = project("status-norepo");
    let (code, out) = rivet(&root, "status", json!({}));
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["complete"], false);
    assert_eq!(out["git"]["ineligible"]["reason"], "unsupported_layout");
    for p in out["paths"].as_array().unwrap() {
        assert_eq!(p["base"], "unknown");
    }
}

#[test]
fn a_path_has_one_spelling() {
    let r = Repo::new("spelling");
    for spelling in [
        "tracking//TRACKING_WIDGETS.md",
        "tracking/./TRACKING_WIDGETS.md",
        "./tracking/TRACKING_WIDGETS.md",
    ] {
        let (code, out) = rivet(&r.root, "map", json!({"path": spelling}));
        assert_eq!(code, 0, "{spelling}: {out}");
        assert_eq!(out["path"], WIDGETS);
        // The binding sees the canonical path too.
        assert_eq!(out["collections"][0]["name"], "actions", "{spelling}");
        let (code, out) = r.run("status", json!({"paths": [spelling]}), &[]);
        assert_eq!(code, 0, "{out}");
        assert_eq!(out["paths"][0]["path"], WIDGETS);
        assert_eq!(out["paths"].as_array().unwrap().len(), 1);
    }
    let (code, out) = rivet(
        &r.root,
        "map",
        json!({"path": "tracking/../tracking/TRACKING_WIDGETS.md"}),
    );
    assert_eq!(code, 2);
    assert_eq!(out["error"]["code"], "path_rejected");
}

#[test]
fn doctor_reports_missing_ignore_lines() {
    let root = project("doctor-ignore");
    std::fs::write(root.join(".rivet/.gitignore"), "/lock\n").unwrap();
    let (_, out) = rivet(&root, "doctor", json!({}));
    let finding = out["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["code"] == "rivet_gitignore")
        .unwrap_or_else(|| panic!("{out}"));
    assert_eq!(finding["severity"], "info");
    assert!(finding["message"].as_str().unwrap().contains("/pending/"));
    // Under per-write commits the missing lines would let bookkeeping into git.
    let path = root.join(".rivet/project.json");
    let mut config: Value = serde_json::from_str(&read(&root, ".rivet/project.json")).unwrap();
    config["git"] = json!({"commit": "per-write"});
    std::fs::write(&path, config.to_string()).unwrap();
    let (_, out) = rivet(&root, "doctor", json!({}));
    let finding = out["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["code"] == "rivet_gitignore")
        .unwrap_or_else(|| panic!("{out}"));
    assert_eq!(finding["severity"], "warning");
}

#[test]
fn status_ignores_git_environment_rivet_inherits() {
    let r = Repo::new("status-env");
    let elsewhere = r.root.with_extension("elsewhere");
    let _ = std::fs::remove_dir_all(&elsewhere);
    std::fs::create_dir_all(&elsewhere).unwrap();
    git_at(&elsewhere, &r.home, &["init", "-q"]);
    let hostile = [
        ("GIT_DIR", elsewhere.join(".git").display().to_string()),
        ("GIT_WORK_TREE", elsewhere.display().to_string()),
        (
            "GIT_INDEX_FILE",
            elsewhere.join("index").display().to_string(),
        ),
        (
            "GIT_CONFIG_PARAMETERS",
            "'core.autocrlf'='true'".to_string(),
        ),
    ];
    let (code, out) = r.run("status", json!({}), &hostile);
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["git"]["ineligible"], Value::Null, "{out}");
    assert_eq!(path_report(&out, WIDGETS)["base"], "known_clean");
}

#[test]
fn status_never_compares_against_a_side_it_could_not_read() {
    let r = Repo::new("status-unread");
    let text = read(&r.root, WIDGETS).replace("status: active", "status: paused");
    std::fs::write(r.root.join(WIDGETS), text).unwrap();
    r.git(&["add", WIDGETS]);
    let git = r.script(
        "nocat",
        "case \"$*\" in *cat-file*) exit 128;; esac\nexec git \"$@\"",
    );
    let (code, out) = r.run("status", json!({}), &[("RIVET_TEST_GIT", git)]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["complete"], true, "detail errors do not lose paths");
    let p = path_report(&out, WIDGETS);
    assert_eq!(p["base"], "unknown");
    assert!(p.get("units").is_none(), "{p}");
    // What git did report stands.
    assert_eq!(p["staged"], true, "{p}");
    assert!(p["head"]["oid"].is_string(), "{p}");
    assert_eq!(p["index"].as_array().unwrap().len(), 1);
}

#[test]
fn status_keeps_residual_bytes_next_to_an_unaddressed_stay() {
    let r = Repo::new("status-orphan");
    let text = read(&r.root, WIDGETS) + "\n<!-- stay:Lone0001 subhash=sha256:000000000000 -->\n";
    std::fs::write(r.root.join(WIDGETS), text).unwrap();
    r.git(&["commit", "-qam", "a stay that addresses nothing"]);
    let text = read(&r.root, WIDGETS).replace("status: active", "status: paused");
    std::fs::write(r.root.join(WIDGETS), text).unwrap();
    let out = r.status();
    let units = &path_report(&out, WIDGETS)["units"];
    assert!(units["residual"].is_object(), "{units}");
    assert!(
        !ids(&units["changed"]).contains(&"Lone0001".to_string()),
        "{units}"
    );
}

#[test]
fn status_tells_two_sections_with_one_title_apart() {
    let r = Repo::new("status-same-title");
    let text = read(&r.root, WIDGETS)
        + "\n## Queue\n\n### Open\n\n- alpha\n\n- beta\n\n### Open\n\n- gamma\n";
    std::fs::write(r.root.join(WIDGETS), text).unwrap();
    let (_, map) = rivet(&r.root, "map", json!({"path": WIDGETS}));
    let (code, out) = rivet(
        &r.root,
        "stamp",
        json!({"path": WIDGETS, "select": {"kind": "document"}, "guard": map["revision"]}),
    );
    assert_eq!(code, 0, "{out}");
    // Drop the list stays (a marker line after a blank line), so the items'
    // flattened order is the same before and after the move and only the
    // enclosing section's identity can tell the two Open sections apart.
    let stamped = read(&r.root, WIDGETS);
    let (head, queue) = stamped.split_at(stamped.find("## Queue").unwrap());
    let mut kept: Vec<&str> = vec![];
    for line in queue.split_inclusive('\n') {
        let after_blank = kept.last().is_some_and(|l| l.trim().is_empty());
        if !(after_blank && line.starts_with("<!-- stay:")) {
            kept.push(line);
        }
    }
    std::fs::write(r.root.join(WIDGETS), format!("{head}{}", kept.concat())).unwrap();
    r.git(&["commit", "-qam", "two Open sections"]);
    // Move beta, unchanged, to the top of the second Open.
    let text = read(&r.root, WIDGETS);
    let beta_start = text.find("- beta").unwrap();
    let beta_end = beta_start + text[beta_start..].find('\n').unwrap() + 1;
    let beta = text[beta_start..beta_end].to_string();
    let without = format!("{}{}", &text[..beta_start], &text[beta_end..]);
    let gamma = without.find("- gamma").unwrap();
    let moved = format!("{}{}\n{}", &without[..gamma], beta, &without[gamma..]);
    std::fs::write(r.root.join(WIDGETS), moved).unwrap();
    let beta_id = beta.split("stay:").nth(1).unwrap()[..8].to_string();
    let out = r.status();
    let units = &path_report(&out, WIDGETS)["units"];
    assert!(ids(&units["moved"]).contains(&beta_id), "{units}");
}

/// A file-record write lands as one commit carrying its `Rivet-Op` trailer,
/// like any other write in commit mode.
#[test]
fn a_record_write_commits_with_its_rivet_op_trailer() {
    let r = Repo::new("git-record-write-commits");
    let config = r.root.join(".rivet/project.json");
    let mut c: Value = serde_json::from_slice(&std::fs::read(&config).unwrap()).unwrap();
    c["git"] = json!({"commit": "per-write", "on_dirty": "refuse"});
    c["collections"]["notes"] = json!({"schema": "note@1", "files": ["notes/*.md"]});
    std::fs::write(&config, c.to_string()).unwrap();
    std::fs::write(
        r.root.join(".rivet/schemas/note.json"),
        r#"{"id": "note", "version": 1, "item": "file", "fields": {
          "status": {"type": "enum", "from": "key", "key": "status", "values": ["open", "done"]}}}"#,
    )
    .unwrap();
    std::fs::create_dir_all(r.root.join("notes")).unwrap();
    std::fs::write(r.root.join("notes/a.md"), "---\nstatus: open\n---\n# A\n").unwrap();
    r.git(&["add", "-A"]);
    r.git(&["commit", "-qm", "notes"]);
    let (code, map) = r.run("map", json!({"path": "notes/a.md"}), &[]);
    assert_eq!(code, 0, "{map}");
    let guard = map["collections"][0]["revision"].clone();
    let (code, out) = r.run(
        "update-item",
        json!({"path": "notes/a.md", "collection": "notes", "guard": guard,
               "set": {"status": "done"}, "message": "close note a"}),
        &[],
    );
    assert_eq!(code, 0, "{out}");
    let message = r.git(&["log", "-1", "--format=%B"]);
    assert!(message.starts_with("close note a"), "{message}");
    assert_eq!(message.matches("Rivet-Op:").count(), 1, "{message}");
    assert_eq!(r.git(&["status", "--porcelain", "--", "notes/a.md"]), "");
    assert_eq!(
        r.git(&["show", "HEAD:notes/a.md"]),
        "---\nstatus: done\n---\n# A\n"
    );
}
