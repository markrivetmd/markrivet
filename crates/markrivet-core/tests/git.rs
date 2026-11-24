//! The git invocation policy, base state, eligibility and the stale-lock
//! rule, in real temporary repositories. Rivet's git calls run through a
//! wrapper that points HOME at an empty directory, so no global or user
//! configuration reaches them.
#[path = "../../test-support/mod.rs"]
mod test_support;
use markrivet_core::fs::Project;
use markrivet_core::git::{
    Base, Decision, FileState, Git, GitChild, LockKind, OnDirty, Preflight, Repo, StaleLock,
    clear_lock, clear_next_index, decide, lingering_lock, list_locks, preflight,
};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime};

const DOC: &str = "tracking/a.md";

struct Fixture {
    dir: PathBuf,
    home: PathBuf,
}

impl Fixture {
    /// A repository whose root is a Rivet project with one committed document.
    fn new(name: &str) -> Fixture {
        let base = test_support::scratch(&format!("git-{name}"));
        let dir = base.join("repo");
        let home = base.join("home");
        std::fs::create_dir_all(dir.join(".rivet/schemas")).unwrap();
        std::fs::create_dir_all(dir.join("tracking")).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        let f = Fixture { dir, home };
        f.wrapper("git", "exec git \"$@\"");
        f.git(&["init", "-q", "-b", "main"]);
        f.git(&["config", "user.name", "Tester"]);
        f.git(&["config", "user.email", "tester@example.invalid"]);
        f.write(DOC, "# A\n\n## Action\n\n- [ ] one\n");
        f.git(&["add", "-A"]);
        f.git(&["commit", "-qm", "init"]);
        f
    }

    /// Write `<home>/<name>`, a git wrapper with HOME pinned to the empty home.
    fn wrapper(&self, name: &str, body: &str) -> PathBuf {
        let path = self.home.join(name);
        use std::io::Write;
        use std::process::Stdio;
        // Keep writable executable descriptors out of this multi-threaded process:
        // a concurrent Git::run forks before exec and can inherit one, keeping
        // the inode busy even after this thread closes it. The isolated writer
        // and its children have all exited before we execute the script.
        let mut child = Command::new("/bin/sh")
            .args([
                "-c",
                "cat > \"$1.tmp\" && chmod 755 \"$1.tmp\" && mv \"$1.tmp\" \"$1\"",
                "fixture",
            ])
            .arg(&path)
            .stdin(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(format!(
            "#!/bin/sh\nexport HOME={0} XDG_CONFIG_HOME={0} GIT_CONFIG_NOSYSTEM=1\n{body}\n",
            self.home.display()
        ).as_bytes()).unwrap();
        assert!(child.wait().unwrap().success());
        path
    }

    /// The test's own git, isolated the same way.
    fn git(&self, args: &[&str]) -> String {
        self.git_in(&self.dir, args)
    }

    fn git_in(&self, dir: &Path, args: &[&str]) -> String {
        let out = test_support::git(dir, &self.home, args).output().unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    }

    fn write(&self, rel: &str, text: &str) {
        std::fs::write(self.dir.join(rel), text).unwrap();
    }

    fn project(&self) -> Project {
        Project::open(&self.dir).unwrap()
    }

    fn repo_with(&self, program: &str) -> Repo {
        let project = self.project();
        Repo::inspect_with(
            Git::with_program(&project, self.home.join(program)),
            &project,
        )
    }

    fn check(&self, path: &str, creation: bool) -> Preflight {
        self.check_with("git", path, creation)
    }

    fn check_with(&self, program: &str, path: &str, creation: bool) -> Preflight {
        let repo = self.repo_with(program);
        let file = FileState::read(&self.project(), path);
        preflight(&repo, path, &file, creation)
    }
}

fn reason(p: &Preflight) -> Option<&'static str> {
    p.ineligible.as_ref().map(|i| i.reason)
}

#[test]
fn a_clean_tracked_file_commits() {
    let f = Fixture::new("clean");
    let p = f.check(DOC, false);
    assert_eq!(p.base, Base::KnownClean, "{p:?}");
    assert_eq!(p.ineligible, None);
    assert_eq!(p.branch.as_deref(), Some("refs/heads/main"));
    for policy in [OnDirty::Skip, OnDirty::Adopt, OnDirty::Refuse] {
        assert_eq!(decide(p.base, None, policy), Decision::Commit);
    }
}

#[test]
fn changed_bytes_make_a_file_known_dirty() {
    let f = Fixture::new("dirty");
    f.write(DOC, "# A\n\n## Action\n\n- [ ] one\n- [ ] two\n");
    let p = f.check(DOC, false);
    assert_eq!(p.base, Base::KnownDirty);
    assert_eq!(p.ineligible, None);
    assert_eq!(decide(p.base, None, OnDirty::Skip), Decision::Dirty);
    assert_eq!(decide(p.base, None, OnDirty::Adopt), Decision::Dirty);
}

#[test]
fn a_mode_only_change_makes_a_file_known_dirty() {
    let f = Fixture::new("mode");
    let path = f.dir.join(DOC);
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    let p = f.check(DOC, false);
    assert_eq!(p.base, Base::KnownDirty, "same bytes, executable bit set");
    // With core.fileMode off git ignores the bit, and so does Rivet.
    f.git(&["config", "core.fileMode", "false"]);
    assert_eq!(f.check(DOC, false).base, Base::KnownClean);
}

#[test]
fn a_creation_into_nothing_is_absent() {
    let f = Fixture::new("absent");
    let p = f.check("tracking/new.md", true);
    assert_eq!(p.base, Base::Absent);
    assert_eq!(p.ineligible, None);
    assert_eq!(decide(p.base, None, OnDirty::Refuse), Decision::Commit);
}

#[test]
fn unknown_base_states() {
    // An existing file git does not track.
    let f = Fixture::new("unknown");
    f.write("tracking/loose.md", "# Loose\n");
    let p = f.check("tracking/loose.md", false);
    assert_eq!((p.base, reason(&p)), (Base::Unknown, Some("untracked")));

    // Absent from HEAD and from disk, but staged.
    f.write("tracking/staged.md", "# Staged\n");
    f.git(&["add", "tracking/staged.md"]);
    std::fs::remove_file(f.dir.join("tracking/staged.md")).unwrap();
    let p = f.check("tracking/staged.md", true);
    assert_eq!((p.base, reason(&p)), (Base::Unknown, Some("staged")));

    // An unborn HEAD.
    let u = Fixture::new("unborn");
    u.git(&["checkout", "-q", "--orphan", "fresh"]);
    u.git(&["rm", "-rq", "--cached", "."]);
    let p = u.check(DOC, false);
    assert_eq!((p.base, reason(&p)), (Base::Unknown, Some("no_branch")));

    // An inspection that fails.
    let e = Fixture::new("inspect-fails");
    e.wrapper(
        "broken",
        "case \"$*\" in *ls-files*) echo boom >&2; exit 2;; esac\nexec git \"$@\"",
    );
    let p = e.check_with("broken", DOC, false);
    assert_eq!((p.base, reason(&p)), (Base::Unknown, Some("git_error")));
}

#[test]
fn refuse_refuses_dirty_and_unknown_before_any_ineligibility() {
    let f = Fixture::new("refuse");
    // Dirty and staged: the dirty-first rule refuses rather than reporting
    // why git could not commit it.
    f.write(DOC, "# A\n\nchanged\n");
    f.git(&["add", DOC]);
    let p = f.check(DOC, false);
    assert_eq!((p.base, reason(&p)), (Base::KnownDirty, Some("staged")));
    assert_eq!(
        decide(p.base, p.ineligible.as_ref(), OnDirty::Refuse),
        Decision::Refuse(Base::KnownDirty)
    );
    assert!(matches!(
        decide(p.base, p.ineligible.as_ref(), OnDirty::Skip),
        Decision::Ineligible(i) if i.reason == "staged"
    ));
    // A creation over a path that is staged but absent is not `absent`.
    f.write("tracking/new.md", "# New\n");
    f.git(&["add", "tracking/new.md"]);
    std::fs::remove_file(f.dir.join("tracking/new.md")).unwrap();
    let p = f.check("tracking/new.md", true);
    assert_eq!(p.base, Base::Unknown);
    assert_eq!(
        decide(p.base, p.ineligible.as_ref(), OnDirty::Refuse),
        Decision::Refuse(Base::Unknown)
    );
}

#[test]
fn each_ineligible_row() {
    type Setup = Box<dyn Fn(&Fixture)>;
    let cases: Vec<(&str, Setup, &str)> = vec![
        (
            "too-old",
            Box::new(|f: &Fixture| {
                f.wrapper(
                    "git",
                    "case \"$*\" in *' version') echo 'git version 2.30.1'; exit 0;; esac\nexec git \"$@\"",
                );
            }),
            "git_too_old",
        ),
        (
            "git-error",
            Box::new(|f: &Fixture| {
                f.wrapper(
                    "git",
                    "case \"$*\" in *ls-tree*) exit 128;; esac\nexec git \"$@\"",
                );
            }),
            "git_error",
        ),
        (
            "core-worktree",
            Box::new(|f: &Fixture| {
                let top = f.dir.canonicalize().unwrap();
                f.git(&["config", "core.worktree", top.to_str().unwrap()]);
            }),
            "unsupported_layout",
        ),
        (
            "partial",
            Box::new(|f: &Fixture| {
                f.git(&["config", "extensions.partialClone", "origin"]);
            }),
            "unsupported_layout",
        ),
        (
            "promisor",
            Box::new(|f: &Fixture| {
                f.git(&["config", "remote.origin.promisor", "true"]);
            }),
            "unsupported_layout",
        ),
        (
            "detached",
            Box::new(|f: &Fixture| {
                f.git(&["checkout", "-q", "--detach"]);
            }),
            "no_branch",
        ),
        (
            "merge-head",
            Box::new(|f: &Fixture| {
                let head = f.git(&["rev-parse", "HEAD"]);
                f.write(".git/MERGE_HEAD", &head);
            }),
            "repo_busy",
        ),
        (
            "unmerged",
            Box::new(|f: &Fixture| {
                f.write("other.md", "base\n");
                f.git(&["add", "other.md"]);
                f.git(&["commit", "-qm", "base"]);
                f.git(&["checkout", "-qb", "side"]);
                f.write("other.md", "side\n");
                f.git(&["commit", "-qam", "side"]);
                f.git(&["checkout", "-q", "main"]);
                f.write("other.md", "main\n");
                f.git(&["commit", "-qam", "main"]);
                let _ = test_support::git(&f.dir, &f.home, &["merge", "-q", "side"]).output();
                // The conflict is real; the marker file would mask the row.
                std::fs::remove_file(f.dir.join(".git/MERGE_HEAD")).unwrap();
            }),
            "repo_busy",
        ),
        (
            "staged",
            Box::new(|f: &Fixture| {
                f.write(DOC, "# A\n\nstaged\n");
                f.git(&["add", DOC]);
            }),
            "staged",
        ),
        (
            "assume-unchanged",
            Box::new(|f: &Fixture| {
                f.git(&["update-index", "--assume-unchanged", DOC]);
            }),
            "index_flags",
        ),
        (
            "skip-worktree",
            Box::new(|f: &Fixture| {
                f.git(&["update-index", "--skip-worktree", DOC]);
            }),
            "index_flags",
        ),
        (
            "filter",
            Box::new(|f: &Fixture| {
                f.write(".gitattributes", "*.md filter=lfs\n");
                f.git(&["add", ".gitattributes"]);
                f.git(&["commit", "-qm", "attributes"]);
            }),
            "git_conversion",
        ),
        (
            "autocrlf",
            Box::new(|f: &Fixture| {
                f.git(&["config", "core.autocrlf", "input"]);
            }),
            "git_conversion",
        ),
        (
            "identity",
            Box::new(|f: &Fixture| {
                f.git(&["config", "--unset", "user.email"]);
            }),
            "no_identity",
        ),
    ];
    for (name, setup, want) in cases {
        let f = Fixture::new(name);
        setup(&f);
        let p = f.check(DOC, false);
        assert_eq!(reason(&p), Some(want), "{name}: {p:?}");
        assert!(
            matches!(decide(p.base, p.ineligible.as_ref(), OnDirty::Skip), Decision::Ineligible(ref i) if i.reason == want),
            "{name}"
        );
    }
    // An attribute turned off is no conversion either.
    let f = Fixture::new("attributes-unset");
    f.write(".gitattributes", "*.md -text -filter -crlf -ident\n");
    f.git(&["add", ".gitattributes"]);
    f.git(&["commit", "-qm", "attributes"]);
    assert_eq!(reason(&f.check(DOC, false)), None);
    // A driver literally named `unset` prints like no driver at all.
    let f = Fixture::new("filter-named-unset");
    f.write(".gitattributes", "*.md filter=unset\n");
    f.git(&["add", ".gitattributes"]);
    f.git(&["commit", "-qm", "attributes"]);
    f.git(&["config", "filter.unset.clean", "tr a-z A-Z"]);
    assert_eq!(reason(&f.check(DOC, false)), Some("git_conversion"));
    // `core.autocrlf=false` is no conversion.
    let f = Fixture::new("autocrlf-false");
    f.git(&["config", "core.autocrlf", "false"]);
    assert_eq!(reason(&f.check(DOC, false)), None);
}

#[test]
fn unsupported_layouts() {
    // The project root is a subdirectory of the work tree.
    let f = Fixture::new("nested");
    std::fs::create_dir_all(f.dir.join("sub/.rivet")).unwrap();
    std::fs::create_dir_all(f.dir.join("sub/tracking")).unwrap();
    f.write("sub/tracking/b.md", "# B\n");
    f.git(&["add", "-A"]);
    f.git(&["commit", "-qm", "nested"]);
    let project = Project::open(&f.dir.join("sub")).unwrap();
    let repo = Repo::inspect_with(Git::with_program(&project, f.home.join("git")), &project);
    let p = preflight(
        &repo,
        "tracking/b.md",
        &FileState::read(&project, "tracking/b.md"),
        false,
    );
    assert_eq!(p.base, Base::Unknown);
    assert_eq!(reason(&p), Some("unsupported_layout"), "{p:?}");

    // A linked worktree.
    let f = Fixture::new("linked");
    let linked = f.dir.with_file_name("linked");
    f.git(&["worktree", "add", "-q", linked.to_str().unwrap()]);
    std::fs::create_dir_all(linked.join(".rivet")).unwrap();
    let project = Project::open(&linked).unwrap();
    let repo = Repo::inspect_with(Git::with_program(&project, f.home.join("git")), &project);
    assert!(
        repo.layout.as_deref().unwrap().contains("worktree"),
        "{:?}",
        repo.layout
    );

    // A bare repository holding a .rivet directory.
    let f = Fixture::new("bare");
    let bare = f.dir.with_file_name("bare.git");
    f.git(&["clone", "-q", "--bare", ".", bare.to_str().unwrap()]);
    std::fs::create_dir_all(bare.join(".rivet")).unwrap();
    let project = Project::open(&bare).unwrap();
    let repo = Repo::inspect_with(Git::with_program(&project, f.home.join("git")), &project);
    assert_eq!(repo.layout.as_deref(), Some("the repository is bare"));

    // No repository at all.
    let f = Fixture::new("none");
    std::fs::remove_dir_all(f.dir.join(".git")).unwrap();
    let repo = f.repo_with("git");
    assert!(repo.layout.is_some());
    assert_eq!(repo.blocked().unwrap().reason, "unsupported_layout");
}

#[test]
fn literal_pathspecs_touch_only_the_named_file() {
    let f = Fixture::new("literal");
    for name in ["a*.md", "ab.md", "[x].md", "x.md"] {
        f.write(&format!("tracking/{name}"), &format!("# {name}\n"));
    }
    f.git(&["add", "-A"]);
    f.git(&["commit", "-qm", "globs"]);
    let project = f.project();
    let git = Git::with_program(&project, f.home.join("git"));
    for (spec, only) in [
        ("tracking/a*.md", "tracking/a*.md"),
        ("tracking/[x].md", "tracking/[x].md"),
    ] {
        let out = git.inspect(&["ls-files", "--", spec], None).unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout), format!("{only}\n"));
    }
    // A change to the files a glob would match leaves the named one clean.
    f.write("tracking/ab.md", "# changed\n");
    f.write("tracking/x.md", "# changed\n");
    f.git(&["add", "tracking/ab.md", "tracking/x.md"]);
    for name in ["tracking/a*.md", "tracking/[x].md"] {
        let p = f.check(name, false);
        assert_eq!((p.base, reason(&p)), (Base::KnownClean, None), "{name}");
        assert_eq!(p.index.len(), 1, "{name}");
    }
}

#[test]
fn a_deadline_kills_and_reaps_the_child() {
    let f = Fixture::new("deadline");
    f.wrapper("slow", "exec sleep 30");
    let project = f.project();
    let mut git = Git::with_program(&project, f.home.join("slow"));
    git.deadline = Duration::from_millis(200);
    let started = std::time::Instant::now();
    let out = git.inspect(&["status"], None);
    assert_eq!(out.unwrap_err(), markrivet_core::git::GitError::Deadline);
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[test]
fn every_git_child_runs_under_the_policy() {
    let f = Fixture::new("policy");
    f.wrapper("env", "pwd; env | sort | grep -E '^GIT_' ; echo \"$*\"");
    // A hook that would run on commit, were hooks not off.
    std::fs::write(f.dir.join(".git/hooks/pre-commit"), "#!/bin/sh\nexit 1\n").unwrap();
    let project = f.project();
    let git = Git::with_program(&project, f.home.join("env"));
    let out = git.inspect(&["status"], None).unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.starts_with(&format!("{}\n", f.dir.canonicalize().unwrap().display())),
        "{text}"
    );
    for want in [
        "GIT_LITERAL_PATHSPECS=1",
        "GIT_NO_REPLACE_OBJECTS=1",
        "GIT_OPTIONAL_LOCKS=0",
        "-c core.hooksPath=/dev/null -c core.fsmonitor=false -c commit.gpgsign=false -c gc.auto=0 -c maintenance.auto=false -c core.fsync=committed,index status",
    ] {
        assert!(text.contains(want), "{want}: {text}");
    }
}

fn dead_pid() -> u32 {
    let mut child = Command::new("true").spawn().unwrap();
    let pid = child.id();
    child.wait().unwrap();
    pid
}

fn touch(path: &Path) {
    std::fs::write(path, "").unwrap();
}

/// `clear_lock` with the identity a report would carry for `file` now.
fn clear(
    project: &Project,
    branch: &str,
    child: &GitChild,
    file: &str,
) -> Result<String, StaleLock> {
    let identity = list_locks(project, Some(branch))
        .unwrap()
        .into_iter()
        .find(|l| l.file == file)
        .map(|l| l.identity)
        .unwrap_or_default();
    clear_lock(project, Some(branch), child, file, &identity)
}

#[test]
fn locks_are_listed_by_kind() {
    let f = Fixture::new("lock-kinds");
    let project = f.project();
    let git = f.dir.join(".git");
    for name in [
        "index.lock",
        "HEAD.lock",
        "next-index-42.lock",
        "config.lock",
    ] {
        touch(&git.join(name));
    }
    std::fs::create_dir_all(git.join("refs/heads/feat")).unwrap();
    touch(&git.join("refs/heads/feat/x.lock"));
    touch(&git.join("refs/heads/other.lock"));
    let kinds: Vec<(String, LockKind)> = list_locks(&project, Some("refs/heads/feat/x"))
        .unwrap()
        .into_iter()
        .map(|l| (l.file, l.kind))
        .collect();
    assert_eq!(
        kinds,
        [
            (".git/HEAD.lock".to_string(), LockKind::Head),
            (".git/config.lock".to_string(), LockKind::Other),
            (".git/index.lock".to_string(), LockKind::Index),
            (".git/next-index-42.lock".to_string(), LockKind::NextIndex),
            (".git/refs/heads/feat/x.lock".to_string(), LockKind::Branch),
            (".git/refs/heads/other.lock".to_string(), LockKind::Other),
        ]
    );
    assert_eq!(
        lingering_lock(&project, Some("refs/heads/feat/x")).as_deref(),
        Some(".git/HEAD.lock")
    );
    for name in ["index.lock", "HEAD.lock"] {
        std::fs::remove_file(git.join(name)).unwrap();
    }
    std::fs::remove_file(git.join("refs/heads/feat/x.lock")).unwrap();
    assert_eq!(lingering_lock(&project, Some("refs/heads/feat/x")), None);
}

#[test]
fn only_next_index_is_removed_automatically() {
    let f = Fixture::new("stale-auto");
    let project = f.project();
    let git = f.dir.join(".git");
    let pid = dead_pid();
    touch(&git.join("index.lock"));
    touch(&git.join(format!("next-index-{pid}.lock")));
    touch(&git.join("next-index-1.lock"));
    // The name is the evidence: a next-index lock older than the spawn goes.
    let child = GitChild {
        identity: None,
        pid: Some(pid),
        spawned: SystemTime::now() + Duration::from_secs(60),
    };
    let (removed, kept) = clear_next_index(&project, Some("main"), &child);
    assert_eq!(removed, [format!(".git/next-index-{pid}.lock")]);
    assert!(kept.is_empty(), "{kept:?}");
    assert!(
        git.join("index.lock").exists(),
        "index.lock needs a request"
    );
    assert!(git.join("next-index-1.lock").exists(), "another process's");
    // A live child's lock, or a record with no child, is left alone.
    touch(&git.join(format!("next-index-{}.lock", std::process::id())));
    for child in [
        GitChild {
            identity: None,
            pid: Some(std::process::id()),
            spawned: SystemTime::now(),
        },
        GitChild {
            identity: None,
            pid: None,
            spawned: SystemTime::now(),
        },
    ] {
        assert_eq!(
            clear_next_index(&project, Some("main"), &child),
            (vec![], vec![])
        );
    }
    // A vetoed one is reported and left.
    touch(&git.join(format!("next-index-{pid}.lock")));
    let held = std::fs::File::open(git.join(format!("next-index-{pid}.lock"))).unwrap();
    let (removed, kept) = clear_next_index(&project, Some("main"), &child);
    assert!(removed.is_empty());
    assert_eq!(kept[0].reason, "stale_git_lock");
    assert!(
        kept[0].veto.as_deref().unwrap().contains("held open"),
        "{kept:?}"
    );
    drop(held);
}

#[test]
fn requested_locks_from_a_dead_child_are_removed() {
    let f = Fixture::new("stale");
    let project = f.project();
    let spawned = SystemTime::now() - Duration::from_secs(1);
    let git = f.dir.join(".git");
    touch(&git.join("index.lock"));
    touch(&git.join("HEAD.lock"));
    touch(&git.join("refs/heads/main.lock"));
    let child = GitChild {
        identity: None,
        pid: Some(dead_pid()),
        spawned,
    };
    for file in [
        ".git/index.lock",
        ".git/HEAD.lock",
        ".git/refs/heads/main.lock",
    ] {
        assert_eq!(
            clear(&project, "refs/heads/main", &child, file).unwrap(),
            file
        );
    }
    assert!(list_locks(&project, Some("main")).unwrap().is_empty());
    // A slashed branch is walked one component at a time.
    std::fs::create_dir_all(git.join("refs/heads/feat")).unwrap();
    touch(&git.join("refs/heads/feat/x.lock"));
    assert_eq!(
        clear(
            &project,
            "refs/heads/feat/x",
            &child,
            ".git/refs/heads/feat/x.lock"
        )
        .unwrap(),
        ".git/refs/heads/feat/x.lock"
    );
}

#[test]
fn stale_lock_rule_refuses_what_it_cannot_attribute() {
    let f = Fixture::new("stale-refuse");
    let project = f.project();
    let git = f.dir.join(".git");
    let dead = GitChild {
        identity: None,
        pid: Some(dead_pid()),
        spawned: SystemTime::now() - Duration::from_secs(1),
    };
    touch(&git.join("index.lock"));
    let index = ".git/index.lock";
    // A live child.
    let live = GitChild {
        identity: None,
        pid: Some(std::process::id()),
        spawned: dead.spawned,
    };
    let e = clear(&project, "main", &live, index).unwrap_err();
    assert_eq!((e.reason, e.file.as_str()), ("stale_git_lock", index));
    // No git child ran for the record.
    let none = GitChild {
        identity: None,
        pid: None,
        spawned: dead.spawned,
    };
    assert!(clear(&project, "main", &none, index).is_err());
    // The lock predates the child.
    let late = GitChild {
        identity: None,
        pid: dead.pid,
        spawned: SystemTime::now() + Duration::from_secs(60),
    };
    assert!(clear(&project, "main", &late, index).is_err());
    // A request carrying another file's identity, as after a replacement.
    let reported = list_locks(&project, Some("main")).unwrap()[0]
        .identity
        .clone();
    std::fs::remove_file(git.join("index.lock")).unwrap();
    std::thread::sleep(Duration::from_millis(10));
    touch(&git.join("index.lock"));
    let e = clear_lock(&project, Some("main"), &dead, index, &reported).unwrap_err();
    assert!(e.detail.contains("not the file that was reported"), "{e:?}");
    assert!(git.join("index.lock").exists());
    // Another lock is never cleared on request, and does not block one that is.
    touch(&git.join("config.lock"));
    let e = clear(&project, "main", &dead, ".git/config.lock").unwrap_err();
    assert!(e.detail.contains("only index.lock"), "{e:?}");
    assert_eq!(clear(&project, "main", &dead, index).unwrap(), index);
    assert!(git.join("config.lock").exists());
    // A symlinked refs directory is not followed.
    touch(&git.join("index.lock"));
    let elsewhere = f.dir.with_file_name("elsewhere");
    std::fs::create_dir_all(elsewhere.join("heads")).unwrap();
    std::fs::rename(git.join("refs"), f.dir.with_file_name("refs-real")).unwrap();
    std::os::unix::fs::symlink(&elsewhere, git.join("refs")).unwrap();
    let e = list_locks(&project, Some("main")).unwrap_err();
    assert_eq!(e.file, ".git/refs");
    let e = clear_lock(&project, Some("main"), &dead, index, "x").unwrap_err();
    assert_eq!(e.file, ".git/refs");
    assert!(git.join("index.lock").exists());
}

#[test]
fn git_booleans_are_read_the_way_git_reads_them() {
    // core.fileMode=2 is true to git, so the executable bit still counts.
    let f = Fixture::new("filemode-2");
    f.git(&["config", "core.fileMode", "2"]);
    std::fs::set_permissions(f.dir.join(DOC), std::fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(f.check(DOC, false).base, Base::KnownDirty);
    f.git(&["config", "core.fileMode", "0x1"]);
    assert_eq!(
        f.check(DOC, false).base,
        Base::KnownDirty,
        "hex is an integer to git"
    );
    f.git(&["config", "core.fileMode", "0"]);
    assert_eq!(f.check(DOC, false).base, Base::KnownClean);
    // A value git would reject is an inspection failure, not a guess.
    f.git(&["config", "core.fileMode", "maybe"]);
    let p = f.check(DOC, false);
    assert_eq!((p.base, reason(&p)), (Base::Unknown, Some("git_error")));
    // A numeric promisor flag is true.
    let f = Fixture::new("promisor-2");
    f.git(&["config", "remote.origin.promisor", "2"]);
    assert_eq!(reason(&f.check(DOC, false)), Some("unsupported_layout"));
}

#[test]
fn absent_is_only_for_a_creation() {
    let f = Fixture::new("absent-read");
    let p = f.check("tracking/none.md", false);
    assert_eq!((p.base, reason(&p)), (Base::Unknown, Some("untracked")));
}

#[test]
fn a_non_utf8_name_never_stands_in_for_another_path() {
    use std::os::unix::ffi::OsStrExt;
    let f = Fixture::new("non-utf8");
    let raw = f
        .dir
        .join("tracking")
        .join(std::ffi::OsStr::from_bytes(b"\xff.md"));
    std::fs::write(&raw, "# Raw\n").unwrap();
    f.git(&["add", "-A"]);
    f.git(&["commit", "-qm", "raw name"]);
    // Rename to the name a lossy decode of the old one would produce.
    let lossy = "tracking/\u{FFFD}.md";
    std::fs::rename(&raw, f.dir.join(lossy)).unwrap();
    f.git(&["add", "-A"]);
    let repo = f.repo_with("git");
    let all = repo.paths(None, |_| true).unwrap();
    let entry = &all[lossy];
    assert_eq!(entry.head, None, "HEAD never had this path");
    assert_eq!(entry.index.len(), 1);
    let file = FileState::read(&f.project(), lossy);
    let (base, why) = repo.assess(entry, &file, markrivet_core::git::Creation::No);
    assert_eq!(
        (base, why.map(|i| i.reason)),
        (Base::Unknown, Some("untracked"))
    );
}

/// Start a shell wrapper that leaves `sleep 30` behind and record its PID.
fn leaves_a_sleeper(f: &Fixture, name: &str, rest: &str) -> PathBuf {
    let pid = f.home.join(format!("{name}.pid"));
    f.wrapper(
        name,
        &format!("sleep 30 &\necho $! > {}\n{rest}", pid.display()),
    );
    pid
}

fn alive(pid: u32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .ok()
        .and_then(|s| s.rsplit_once(") ").map(|(_, r)| !r.starts_with('Z')))
        .unwrap_or(false)
}

fn gone_soon(pid: u32) -> bool {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while alive(pid) {
        if std::time::Instant::now() > deadline {
            // SAFETY: signalling a process this test started.
            unsafe { libc::kill(pid as i32, libc::SIGKILL) };
            return false;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    true
}

#[test]
fn the_process_group_is_swept_after_git_exits_and_at_the_deadline() {
    let f = Fixture::new("sweep");
    let project = f.project();
    // Git exits normally, leaving a descendant in its group.
    let pid = leaves_a_sleeper(&f, "exits", "exit 0");
    let out = Git::with_program(&project, f.home.join("exits"))
        .inspect(&["status"], None)
        .unwrap();
    assert!(out.ok());
    let left: u32 = std::fs::read_to_string(pid)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(gone_soon(left), "a descendant outlived the call");
    // Git runs past its deadline.
    let pid = leaves_a_sleeper(&f, "hangs", "exec sleep 30");
    let mut git = Git::with_program(&project, f.home.join("hangs"));
    git.deadline = Duration::from_millis(300);
    assert_eq!(
        git.inspect(&["status"], None).unwrap_err(),
        markrivet_core::git::GitError::Deadline
    );
    let left: u32 = std::fs::read_to_string(pid)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(gone_soon(left), "a descendant outlived the deadline");
}

#[test]
fn executable_fixture_replaces_an_inode_with_an_inherited_writer() {
    use std::os::fd::AsRawFd;
    let f = Fixture::new("inherited-writer");
    let path = f.wrapper("script", "exit 0");
    let writer = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
    let mut gate = [0; 2];
    // SAFETY: a pipe for the child to wait on; the child only calls libc.
    assert_eq!(
        unsafe { libc::pipe2(gate.as_mut_ptr(), libc::O_CLOEXEC) },
        0
    );
    let fd = writer.as_raw_fd();
    // SAFETY: after fork the child uses only async-signal-safe calls and _exit.
    let pid = unsafe { libc::fork() };
    assert!(pid >= 0);
    if pid == 0 {
        unsafe {
            libc::close(gate[1]);
            let mut byte = 0u8;
            libc::read(gate[0], (&mut byte as *mut u8).cast(), 1);
            libc::close(fd);
            libc::_exit(0);
        }
    }
    drop(writer);
    // Negative control: closing the parent's writer does not make this inode executable.
    let busy = Command::new(&path).status();
    // A replacement fixture must execute while the child still holds the old inode.
    let replaced = f.wrapper("script", "exit 0");
    let result = Command::new(replaced).status();
    // Release and reap our child before asserting, including when the fixture regresses.
    unsafe {
        libc::close(gate[0]);
        libc::close(gate[1]);
        libc::waitpid(pid, std::ptr::null_mut(), 0);
    }
    assert_eq!(busy.unwrap_err().raw_os_error(), Some(libc::ETXTBSY));
    assert!(result.unwrap().success());
}

#[test]
fn a_descendant_that_escapes_the_group_cannot_hold_rivet_past_the_deadline() {
    let f = Fixture::new("escape");
    let pid = f.home.join("escaped.pid");
    f.wrapper(
        "escapes",
        // The pause lets setsid leave the group before git exits.
        &format!(
            "setsid sleep 30 &\necho $! > {}\nsleep 0.2\nexit 0",
            pid.display()
        ),
    );
    let project = f.project();
    let mut git = Git::with_program(&project, f.home.join("escapes"));
    git.deadline = Duration::from_millis(500);
    let started = std::time::Instant::now();
    let out = git.inspect(&["status"], None);
    let took = started.elapsed();
    if let Ok(text) = std::fs::read_to_string(&pid) {
        // SAFETY: signalling a process this test started.
        unsafe { libc::kill(text.trim().parse::<i32>().unwrap(), libc::SIGKILL) };
    }
    assert_eq!(out.unwrap_err(), markrivet_core::git::GitError::Deadline);
    assert!(took < Duration::from_secs(3), "{took:?}");
}

#[test]
fn stale_lock_rule_refuses_locks_it_does_not_own() {
    let f = Fixture::new("stale-owners");
    let project = f.project();
    let git = f.dir.join(".git");
    let dead = GitChild {
        identity: None,
        pid: Some(dead_pid()),
        spawned: SystemTime::now() - Duration::from_secs(1),
    };
    // A next-index lock naming another process.
    let other = format!(".git/next-index-{}.lock", std::process::id());
    touch(&f.dir.join(&other));
    assert!(clear(&project, "main", &dead, &other).is_err());
    assert!(f.dir.join(&other).exists());
    // Another branch's lock.
    touch(&git.join("refs/heads/other.lock"));
    let e = clear(&project, "main", &dead, ".git/refs/heads/other.lock").unwrap_err();
    assert_eq!(e.file, ".git/refs/heads/other.lock");
    // A lock some live process holds open.
    touch(&git.join("index.lock"));
    let held = std::fs::File::open(git.join("index.lock")).unwrap();
    let e = clear(&project, "main", &dead, ".git/index.lock").unwrap_err();
    assert!(e.detail.contains("held open"), "{e:?}");
    assert!(e.veto.is_some());
    drop(held);
    // A lock that is a symlink.
    std::fs::remove_file(git.join("index.lock")).unwrap();
    std::os::unix::fs::symlink("/dev/null", git.join("index.lock")).unwrap();
    let e = clear(&project, "main", &dead, ".git/index.lock").unwrap_err();
    assert_eq!(e.detail, "not a regular file");
    std::fs::remove_file(git.join("index.lock")).unwrap();
    touch(&git.join("index.lock"));
    assert_eq!(
        clear(&project, "main", &dead, ".git/index.lock").unwrap(),
        ".git/index.lock"
    );
}

#[test]
fn stale_lock_rule_refuses_a_live_transaction_and_symlinked_refs() {
    let f = Fixture::new("stale-live");
    let project = f.project();
    let git = f.dir.join(".git");
    let dead = GitChild {
        identity: None,
        pid: Some(dead_pid()),
        spawned: SystemTime::now() - Duration::from_secs(1),
    };
    // A prepared update-ref transaction has closed its lock and waits.
    let head = f.git(&["rev-parse", "HEAD"]);
    let mut live = test_support::git(&f.dir, &f.home, &["update-ref", "--stdin"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    {
        use std::io::{BufRead, Write};
        let stdin = live.stdin.as_mut().unwrap();
        write!(stdin, "start\nupdate refs/heads/main {}prepare\n", head).unwrap();
        stdin.flush().unwrap();
        let mut line = String::new();
        let mut out = std::io::BufReader::new(live.stdout.as_mut().unwrap());
        out.read_line(&mut line).unwrap();
        out.read_line(&mut line).unwrap();
        assert!(line.contains("prepare"), "{line}");
    }
    assert!(git.join("refs/heads/main.lock").exists());
    let e = clear(&project, "main", &dead, ".git/refs/heads/main.lock").unwrap_err();
    assert!(e.detail.starts_with("git process"), "{e:?}");
    assert!(git.join("refs/heads/main.lock").exists());
    drop(live.stdin.take());
    live.wait().unwrap();

    // A symlinked directory under refs/ is refused, not skipped.
    let elsewhere = f.dir.with_file_name("feat-real");
    std::fs::create_dir_all(&elsewhere).unwrap();
    touch(&elsewhere.join("x.lock"));
    std::os::unix::fs::symlink(&elsewhere, git.join("refs/heads/feat")).unwrap();
    touch(&git.join("index.lock"));
    let e = clear_lock(&project, Some("main"), &dead, ".git/index.lock", "x").unwrap_err();
    assert_eq!(e.file, ".git/refs/heads/feat");
    assert!(git.join("index.lock").exists(), "nothing removed");
}

#[test]
fn a_descendant_holding_only_stdin_cannot_hold_rivet() {
    let f = Fixture::new("escape-stdin");
    let pid = f.home.join("escaped.pid");
    f.wrapper(
        "keeps-stdin",
        &format!(
            // A background job's stdin is /dev/null, so pass the pipe on fd 3.
            "exec 3<&0\nsetsid sleep 30 <&3 >/dev/null 2>&1 &\necho $! > {}\nsleep 0.2\nexit 0",
            pid.display()
        ),
    );
    let project = f.project();
    let mut git = Git::with_program(&project, f.home.join("keeps-stdin"));
    git.deadline = Duration::from_millis(2000);
    let input = vec![b'x'; 4 * 1024 * 1024];
    let started = std::time::Instant::now();
    let out = git.inspect(&["hash-object", "--stdin"], Some(&input));
    let took = started.elapsed();
    let escaped: i32 = std::fs::read_to_string(&pid)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let stdin = std::fs::read_link(format!("/proc/{escaped}/fd/0"));
    // SAFETY: signalling a process this test started.
    unsafe { libc::kill(escaped, libc::SIGKILL) };
    assert!(
        stdin.unwrap().to_string_lossy().starts_with("pipe:"),
        "the escapee holds the input pipe"
    );
    assert!(out.unwrap().ok());
    assert!(took < Duration::from_millis(1500), "{took:?}");
}

#[test]
fn stale_lock_rule_refuses_git_started_elsewhere_on_this_repository() {
    // An `=` in the repository's own path must not cut the argument short.
    let f = Fixture::new("stale=git-dir");
    let project = f.project();
    let git = f.dir.join(".git");
    let dead = GitChild {
        identity: None,
        pid: Some(dead_pid()),
        spawned: SystemTime::now() - Duration::from_secs(1),
    };
    let head = f.git(&["rev-parse", "HEAD"]);
    let git_dir = format!("--git-dir={}", git.display());
    let mut live = test_support::git(
        Path::new("/"),
        &f.home,
        &[git_dir.as_str(), "update-ref", "--stdin"],
    )
    .stdin(std::process::Stdio::piped())
    .stdout(std::process::Stdio::piped())
    .spawn()
    .unwrap();
    {
        use std::io::{BufRead, Write};
        let stdin = live.stdin.as_mut().unwrap();
        write!(stdin, "start\nupdate refs/heads/main {}prepare\n", head).unwrap();
        stdin.flush().unwrap();
        let mut line = String::new();
        let mut out = std::io::BufReader::new(live.stdout.as_mut().unwrap());
        out.read_line(&mut line).unwrap();
        out.read_line(&mut line).unwrap();
    }
    let e = clear(&project, "main", &dead, ".git/refs/heads/main.lock").unwrap_err();
    assert!(e.detail.starts_with("git process"), "{e:?}");
    assert!(git.join("refs/heads/main.lock").exists());
    drop(live.stdin.take());
    live.wait().unwrap();
}

#[test]
fn snapshot_git_stream_and_shared_budgets_refuse_instead_of_truncating() {
    use markrivet_core::git::{GitError, ReadBudget};
    let f = Fixture::new("snapshot-read-caps");
    let project = f.project();
    let git = Git::with_program(&project, f.home.join("git")).bounded(ReadBudget::new(1024 * 1024));
    let full = git.inspect(&["ls-tree", "-rz", "HEAD"], None).unwrap();
    assert!(full.ok());
    assert!(full.stdout.len() > 8);
    assert!(matches!(
        git.inspect_limit(&["ls-tree", "-rz", "HEAD"], 8),
        Err(GitError::OutputLimit(_))
    ));
    let shared = ReadBudget::new(full.stdout.len());
    let git = Git::with_program(&project, f.home.join("git")).bounded(shared.clone());
    assert!(git.inspect(&["ls-tree", "-rz", "HEAD"], None).unwrap().ok());
    assert!(matches!(
        git.inspect(&["ls-tree", "-rz", "HEAD"], None),
        Err(GitError::OutputLimit(_))
    ));
    assert!(shared.exceeded.lock().unwrap().is_some());
}
