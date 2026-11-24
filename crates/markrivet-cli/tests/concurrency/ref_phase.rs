//! Ref publication under finite synthetic stalls of real Git.
//!
//! A Python proxy stands between Rivet and a genuine `git update-ref --stdin`
//! child. It forwards the transaction unchanged and only delays it: after
//! Git has prepared (its ref locks exist) and before the acknowledgment
//! reaches Rivet, and after Rivet sends `commit` and before Git receives it.
//! The HEAD reads Rivet makes while the transaction is prepared can be slowed
//! too. Locks in these cases are always Git's own; none is manufactured.
//!
//! Every phase budget is `BUDGET_MS` (`RIVET_TEST_GIT_DEADLINE_MS`). `STALL_MS`
//! fits one budget with `BUDGET_MS - STALL_MS` to spare, and two stalls exceed
//! one budget by `2 * STALL_MS - BUDGET_MS`. The proxy log records each phase
//! boundary and whether the child lived to exit.
//!
//! The split stimuli below are the ones that killed the single-clock engine
//! (prepare, validation and completion under one budget) in every run, with
//! Git's prepared locks left behind. The single-phase overruns are permanent
//! controls: a stall longer than its own budget is still killed, and what Git
//! leaves is still human recovery.
use super::*;
use std::time::{Duration, Instant};

/// Each phase budget, and the ordinary Git deadline, in this file's runs.
/// Wide, so interpreter and Git start-up under a loaded parallel test run
/// stay well inside one budget.
const BUDGET_MS: u64 = 4000;
/// One finite stall: below a budget by 1,600 ms; two exceed one budget by 800 ms.
const STALL_MS: u64 = 2400;
/// A small completion stall, so a kill that follows a late `commit` write
/// lands before the proxy forwards it.
const NUDGE_MS: u64 = 300;

/// What the proxy does around one prepared transaction.
#[derive(Default, Clone, Copy)]
struct Stall {
    /// After Git prepared, before Rivet sees the acknowledgment.
    prepare_ms: u64,
    /// After Rivet sent `commit`, before Git receives it.
    complete_ms: u64,
    /// Each `symbolic-ref`/`rev-parse` while the transaction is prepared.
    read_ms: u64,
    /// `deaf-commit`: fill its own stdin pipe before acknowledging and never
    /// read it again. `deaf-abort`: on EOF, keep Git's stdin open and hang.
    /// `trickle`: after `commit: ok`, write stderr slowly and never exit.
    mode: &'static str,
    /// Hold each HEAD read made while the transaction is prepared until
    /// the test creates `<home>/read.go`.
    hold_read: bool,
}

impl Stall {
    fn wrapper(self, s: &Store) -> Var {
        let body = format!(
            r#"#!/usr/bin/python3
import fcntl, os, subprocess, sys, time
HOME = {home:?}
PREPARE, COMPLETE, READ = {p} / 1000, {c} / 1000, {r} / 1000
MODE = {mode:?}
HOLD = {hold}
MARKER = HOME + '/ref-prepared'
def proxy_alive():
    # The marker names the proxy that wrote it; one killed before it could
    # remove the marker must not slow the reads that follow.
    try:
        pid = open(MARKER).read().strip()
        state = open('/proc/%s/stat' % pid).read().rsplit(') ', 1)[1][0]
    except (OSError, IndexError):
        return False
    return state != 'Z'
def log(message):
    with open(HOME + '/ref-proxy.log', 'a') as f:
        f.write('%.3f %s\n' % (time.monotonic(), message))
def fill_stdin():
    fcntl.fcntl(0, 1031, 4096)  # F_SETPIPE_SZ
    w = os.open('/proc/self/fd/0', os.O_WRONLY | os.O_NONBLOCK)
    try:
        while True:
            os.write(w, b'#' * 4096)
    except BlockingIOError:
        pass
    log('stdin full')
argv = sys.argv[1:]
if 'update-ref' not in argv:
    if (READ or HOLD) and proxy_alive() and ('symbolic-ref' in argv or 'rev-parse' in argv):
        log('slow head read')
        if HOLD:
            end = time.monotonic() + 30
            while not os.path.exists(HOME + '/read.go') and time.monotonic() < end:
                time.sleep(0.005)
        time.sleep(READ)
        log('head read done')
    os.execvp('git', ['git'] + argv)
inp, out = sys.stdin.buffer, sys.stdout.buffer
log('proxy pid %d' % os.getpid())
git = subprocess.Popen(['git'] + argv, stdin=subprocess.PIPE, stdout=subprocess.PIPE)
log('git pid %d' % git.pid)
git.stdin.write(b''.join(inp.readline() for _ in range(3)))
git.stdin.flush()
ack = git.stdout.readline() + git.stdout.readline()
log('prepared %r' % ack)
time.sleep(PREPARE)
if MODE == 'deaf-commit':
    fill_stdin()
with open(MARKER, 'w') as f:
    f.write(str(os.getpid()))
out.write(ack)
out.flush()
log('acknowledged')
if MODE == 'deaf-commit':
    time.sleep(30)
command = inp.readline()
os.unlink(MARKER)
log('received %r' % command)
if command:
    log('completion delay start %r' % COMPLETE)
    time.sleep(COMPLETE)
    log('completion delay done')
    git.stdin.write(command)
    git.stdin.flush()
    log('forwarded')
    out.write(git.stdout.readline())
    out.flush()
    if MODE == 'trickle':
        log('trickle')
        while True:
            sys.stderr.write('.' * 100)
            sys.stderr.flush()
            time.sleep(0.01)
    inp.read()
elif MODE == 'deaf-abort':
    log('deaf to abort')
    time.sleep(30)
git.stdin.close()
out.write(git.stdout.read())
out.flush()
code = git.wait()
log('exit %d' % code)
sys.exit(code)
"#,
            home = s.home.display().to_string(),
            p = self.prepare_ms,
            c = self.complete_ms,
            r = self.read_ms,
            mode = self.mode,
            hold = if self.hold_read { "True" } else { "False" },
        );
        let path = s.home.join("ref-proxy.py");
        std::fs::write(&path, body).unwrap();
        std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .unwrap();
        ("RIVET_TEST_GIT", path.display().to_string())
    }
}

fn budget() -> Var {
    ("RIVET_TEST_GIT_DEADLINE_MS", BUDGET_MS.to_string())
}

fn proxy_log(s: &Store) -> String {
    std::fs::read_to_string(s.home.join("ref-proxy.log")).unwrap_or_default()
}

/// Wait until the proxy has logged `line`.
fn logged(s: &Store, line: &str) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !proxy_log(s).contains(line) {
        assert!(Instant::now() < deadline, "never logged {line}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// The proxied Git shares the killed group but is reaped by init, not by
/// Rivet: wait for it to be gone before recovery looks for live Git.
fn proxied_git_gone(s: &Store) {
    let log = proxy_log(s);
    let Some(pid) = log
        .lines()
        .find_map(|l| l.split_once(" git pid ").map(|(_, p)| p.trim().to_string()))
    else {
        return;
    };
    let stat = format!("/proc/{pid}/stat");
    let deadline = Instant::now() + Duration::from_secs(10);
    while std::fs::read_to_string(&stat).is_ok_and(|text| {
        text.rsplit_once(") ")
            .is_some_and(|(_, rest)| !rest.starts_with('Z'))
    }) {
        assert!(
            Instant::now() < deadline,
            "proxied git {pid} outlived the kill"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Let held HEAD reads (`Stall::hold_read`) continue.
fn release_reads(s: &Store) {
    std::fs::write(s.home.join("read.go"), "").unwrap();
}

/// SIGKILL the transaction's whole process group from outside: the proxy
/// leads it and real Git is in it.
fn kill_transaction(s: &Store) {
    let leader = proxy_log(s)
        .lines()
        .find_map(|l| {
            l.split_once(" proxy pid ")
                .map(|(_, p)| p.trim().to_string())
        })
        .expect("proxy pid logged");
    let killed = std::process::Command::new("kill")
        .args(["-KILL", "--", &format!("-{leader}")])
        .status()
        .unwrap();
    assert!(killed.success());
}

/// Git was killed holding its prepared locks while HEAD moved: the write is
/// never settled as a `head_moved` skip. Publication fails, and observation
/// then reports HEAD on another branch (`branch_mismatch`); the record and
/// Git's locks stay for recovery.
fn kept_for_recovery(f: &Fixture, out: &Value) {
    let s = &f.s;
    assert_eq!(out["commit"]["status"], "failed", "{out}\n{}", proxy_log(s));
    assert_eq!(
        out["commit"]["reason"],
        "branch_mismatch",
        "{out}\n{}",
        proxy_log(s)
    );
    assert_eq!(s.git(&["rev-parse", "refs/heads/main"]).trim(), f.base);
    let op = s.record()["op"].as_str().unwrap().to_string();
    let locks = ref_locks(s);
    assert!(
        locks.iter().any(|(l, _)| l == ".git/refs/heads/main.lock"),
        "{locks:?}"
    );
    proxied_git_gone(s);
    let rec = s.recover(json!({}), &[]);
    assert_eq!(one_result(&rec)["reason"], "stale_git_lock", "{rec}");
    assert!(
        rec["records"][0]["locks"]
            .as_array()
            .unwrap()
            .iter()
            .all(|l| l["record"] == op),
        "{rec}"
    );
    f.untouched();
}

/// Every ref lock under `.git`, relative to the root, with its size.
fn ref_locks(s: &Store) -> Vec<(String, u64)> {
    fn walk(root: &Path, dir: &Path, out: &mut Vec<(String, u64)>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(root, &path, out);
            } else if path.extension().is_some_and(|x| x == "lock") {
                let rel = path.strip_prefix(root).unwrap().display().to_string();
                out.push((rel, std::fs::metadata(&path).unwrap().len()));
            }
        }
    }
    let mut out = vec![];
    walk(&s.root, &s.root.join(".git"), &mut out);
    out.sort();
    out
}

/// What must hold around every case: the unrelated staged entry, the other
/// tracker's bytes and the stay ids.
struct Fixture {
    s: Store,
    ids: Vec<String>,
    base: String,
    branch: String,
    index: Vec<u8>,
    docs: String,
    staged: String,
}

impl Fixture {
    fn new(name: &str) -> Fixture {
        Fixture::on(Store::new(name, "skip"))
    }

    fn on(s: Store) -> Fixture {
        std::fs::write(s.root.join("notes.txt"), "staged, not committed\n").unwrap();
        s.git(&["add", "notes.txt"]);
        let staged = s.git(&["ls-files", "-s", "notes.txt"]);
        let ids = s.items();
        Fixture {
            base: s.head(),
            branch: s.git(&["symbolic-ref", "HEAD"]).trim().to_string(),
            index: std::fs::read(s.root.join(".git/index")).unwrap(),
            docs: read(&s.root, DOCS),
            ids,
            staged,
            s,
        }
    }

    fn set(&self, content: &str, extra: &[Var]) -> Value {
        let (code, out) = self.s.set(&self.ids[0], content, extra);
        assert_eq!(code, 0, "{out}");
        assert_eq!(out["status"], "applied", "{out}");
        self.edited(content);
        out
    }

    /// The edited stay holds the requested content, whatever the commit did.
    fn edited(&self, content: &str) {
        let (code, got) = self.s.run(
            "get",
            json!({"path": WIDGETS, "select": {"kind": "id", "id": self.ids[0]}}),
            &[],
        );
        assert_eq!(code, 0, "{got}");
        let text = content.trim_start_matches("[x] ");
        assert!(got["content"].as_str().unwrap().contains(text), "{got}");
    }

    /// Unchanged by anything in this file: other bytes, staging, stay ids.
    fn untouched(&self) {
        let s = &self.s;
        assert_eq!(read(&s.root, DOCS), self.docs);
        assert_eq!(s.git(&["ls-files", "-s", "notes.txt"]), self.staged);
        assert_eq!(s.items(), self.ids);
    }

    /// Nothing published: HEAD and the index as before, the candidate on disk.
    fn unpublished(&self, out: &Value, reason: &str) -> String {
        let s = &self.s;
        assert_eq!(out["commit"]["status"], "failed", "{out}");
        assert_eq!(out["commit"]["reason"], reason, "{out}");
        assert_eq!(out["commit"]["published"], false, "{out}");
        assert_eq!(s.head(), self.base);
        assert_eq!(
            std::fs::read(s.root.join(".git/index")).unwrap(),
            self.index
        );
        self.untouched();
        s.record()["op"].as_str().unwrap().to_string()
    }

    /// A killed child: Git's prepared branch lock holds the unpublished commit.
    fn killed_with_locks(&self, out: &Value) -> String {
        let s = &self.s;
        let op = self.unpublished(out, "deadline");
        let file = format!(".git/{}.lock", self.branch);
        let locks = ref_locks(s);
        let branch = locks
            .iter()
            .find(|(f, _)| *f == file)
            .unwrap_or_else(|| panic!("no branch lock: {locks:?}\n{}", proxy_log(s)));
        assert_eq!(branch.1, 41, "{locks:?}");
        let unpublished = std::fs::read_to_string(s.root.join(&file))
            .unwrap()
            .trim()
            .to_string();
        assert!(s.message(&unpublished).contains(&format!("Rivet-Op: {op}")));
        op
    }

    /// A transaction abandoned before `commit`: Git saw EOF, aborted and
    /// exited by itself, so none of its ref locks is left.
    fn aborted_without_locks(&self, out: &Value) -> String {
        let s = &self.s;
        let op = self.unpublished(out, "deadline");
        let log = proxy_log(s);
        assert!(log.contains("received b''"), "{log}");
        assert!(!log.contains("received b'commit"), "{log}");
        assert!(log.contains("exit 0"), "{log}");
        assert!(ref_locks(s).is_empty(), "{:?}", ref_locks(s));
        op
    }

    /// Human recovery: blocked first, then exactly the reported locks cleared
    /// by identity, then one op-bearing commit with the candidate.
    fn recover_by_clearing(&self, op: &str, candidate: &str) {
        let s = &self.s;
        proxied_git_gone(s);
        let mut out = s.recover(json!({}), &[]);
        assert_eq!(one_result(&out)["reason"], "stale_git_lock", "{out}");
        for _ in 0..3 {
            let locks: Vec<Value> = out["records"]
                .as_array()
                .unwrap()
                .iter()
                .flat_map(|r| r["locks"].as_array().cloned().unwrap_or_default())
                .collect();
            let Some(lock) = locks.first() else { break };
            assert_eq!(lock["record"], op, "{out}");
            out = s.recover(
                json!({"op": op, "clear_lock": lock["file"], "identity": lock["identity"]}),
                &[],
            );
        }
        assert_eq!(out["unresolved"], 0, "{out}");
        self.committed_once(op, candidate);
    }

    /// Exactly one op-bearing commit over the base; raw HEAD and index blobs
    /// are the candidate's bytes; no record or ref lock remains.
    fn committed_once(&self, op: &str, candidate: &str) {
        let s = &self.s;
        assert_eq!(s.carrying(op).len(), 1, "{}", proxy_log(s));
        assert_eq!(s.since(&self.base).len(), 1);
        assert_eq!(s.changed(&s.head()), [WIDGETS]);
        assert_eq!(s.git(&["show", &format!("HEAD:{WIDGETS}")]), candidate);
        assert_eq!(s.git(&["show", &format!(":{WIDGETS}")]), candidate);
        assert_eq!(read(&s.root, WIDGETS), candidate);
        assert!(s.records().is_empty());
        assert!(ref_locks(s).is_empty(), "{:?}", ref_locks(s));
        self.untouched();
    }

    /// Committed by the write itself, with Git exiting on its own.
    fn committed_by_write(&self, out: &Value) {
        let log = proxy_log(&self.s);
        assert_eq!(out["commit"]["status"], "committed", "{out}\n{log}");
        assert!(log.contains("received b'commit\\n'"), "{log}");
        assert!(log.contains("exit 0"), "{log}");
        let op = out["commit"]["op"].as_str().unwrap();
        self.committed_once(op, &read(&self.s.root, WIDGETS));
    }
}

/// Preparation and completion each fit one budget; together they exceed it.
#[test]
fn ref_phase_prepare_plus_complete_split_stimulus() {
    let f = Fixture::new("ref-phase-prepare-complete");
    let stall = Stall {
        prepare_ms: STALL_MS,
        complete_ms: STALL_MS,
        ..Stall::default()
    };
    let out = f.set(
        "[x] **Ship the widget.** Split stimulus.",
        &[stall.wrapper(&f.s), budget()],
    );
    f.committed_by_write(&out);
}

/// Preparation fits one budget and the two HEAD reads fit another.
#[test]
fn ref_phase_prepare_plus_validation_split_stimulus() {
    let f = Fixture::new("ref-phase-prepare-validate");
    let stall = Stall {
        prepare_ms: STALL_MS,
        read_ms: STALL_MS / 3,
        complete_ms: NUDGE_MS,
        ..Stall::default()
    };
    let out = f.set(
        "[x] **Ship the widget.** Slow validation.",
        &[stall.wrapper(&f.s), budget()],
    );
    assert_eq!(proxy_log(&f.s).matches("head read done").count(), 2);
    f.committed_by_write(&out);
}

/// Each HEAD read fits one budget, both together do not: the second read is
/// cut at the shared end, no `commit` is sent, and Git aborts by itself.
#[test]
fn ref_phase_validation_shares_one_budget_across_head_reads() {
    let f = Fixture::new("ref-phase-shared-validation");
    let stall = Stall {
        read_ms: STALL_MS,
        complete_ms: NUDGE_MS,
        ..Stall::default()
    };
    let out = f.set(
        "[x] **Ship the widget.** Shared validation.",
        &[stall.wrapper(&f.s), budget()],
    );
    let candidate = read(&f.s.root, WIDGETS);
    let log = proxy_log(&f.s);
    // Both reads started; the second was cut at the shared end.
    assert_eq!(log.matches("slow head read").count(), 2, "{log}");
    assert_eq!(log.matches("head read done").count(), 1, "{log}");
    let op = f.aborted_without_locks(&out);
    // Nothing for a person to clear: ordinary recovery commits it.
    let rec = f.s.recover(json!({}), &[]);
    assert_eq!(one_result(&rec)["result"], "committed", "{rec}");
    f.committed_once(&op, &candidate);
}

/// The prepared callback returns after its budget with both reads correct.
#[test]
fn ref_phase_late_validation_return_never_commits() {
    let f = Fixture::new("ref-phase-late-return");
    let stall = Stall {
        complete_ms: NUDGE_MS,
        ..Stall::default()
    };
    let pause = Pause::new(&f.s.root, "ref_prepared");
    let request =
        f.s.set_request(&f.ids[0], "[x] **Ship the widget.** Late return.");
    let writer = f.s.spawn(
        "set",
        &request,
        &[stall.wrapper(&f.s), budget(), pause.env()],
    );
    pause.wait();
    std::thread::sleep(Duration::from_millis(BUDGET_MS + 1000));
    pause.release();
    let (code, out) = finish(writer);
    assert_eq!(code, 0, "{out}");
    let candidate = read(&f.s.root, WIDGETS);
    let op = f.aborted_without_locks(&out);
    let rec = f.s.recover(json!({}), &[]);
    assert_eq!(one_result(&rec)["result"], "committed", "{rec}");
    f.committed_once(&op, &candidate);
}

/// Without any proxy, real Git publishes the instant it reads `commit`: a
/// late validation must never send it.
#[test]
fn ref_phase_late_validation_sends_no_commit_to_real_git() {
    let f = Fixture::new("ref-phase-late-real-git");
    let pause = Pause::new(&f.s.root, "ref_prepared");
    let request =
        f.s.set_request(&f.ids[0], "[x] **Ship the widget.** Late, real Git.");
    let writer = f.s.spawn("set", &request, &[budget(), pause.env()]);
    pause.wait();
    std::thread::sleep(Duration::from_millis(BUDGET_MS + 1000));
    pause.release();
    let (code, out) = finish(writer);
    assert_eq!(code, 0, "{out}");
    let candidate = read(&f.s.root, WIDGETS);
    let op = f.unpublished(&out, "deadline");
    assert!(ref_locks(&f.s).is_empty(), "{:?}", ref_locks(&f.s));
    assert!(f.s.carrying(&op).is_empty());
    let rec = f.s.recover(json!({}), &[]);
    assert_eq!(one_result(&rec)["result"], "committed", "{rec}");
    f.committed_once(&op, &candidate);
}

/// Rewrite HEAD outside Git's locks, as only a lock-bypassing writer can.
fn move_head(s: &Store) {
    std::fs::write(s.root.join(".git/HEAD"), "ref: refs/heads/side\n").unwrap();
}

/// HEAD rewritten outside Git's locks while the transaction is prepared: the
/// recheck refuses, EOF aborts Git, and none of its locks is left.
#[test]
fn ref_phase_head_moved_during_validation_aborts_without_locks() {
    let f = Fixture::new("ref-phase-head-moved");
    f.s.git(&["branch", "side"]);
    let stall = Stall {
        hold_read: true,
        ..Stall::default()
    };
    let request =
        f.s.set_request(&f.ids[0], "[x] **Ship the widget.** HEAD moves.");
    let writer = f.s.spawn("set", &request, &[stall.wrapper(&f.s), budget()]);
    logged(&f.s, "slow head read");
    move_head(&f.s);
    release_reads(&f.s);
    let (code, out) = finish(writer);
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["commit"]["status"], "skipped", "{out}");
    assert_eq!(out["commit"]["reason"], "head_moved", "{out}");
    let log = proxy_log(&f.s);
    assert!(
        log.contains("received b''") && log.contains("exit 0"),
        "{log}"
    );
    assert!(ref_locks(&f.s).is_empty(), "{:?}", ref_locks(&f.s));
    assert_eq!(f.s.git(&["rev-parse", "refs/heads/main"]).trim(), f.base);
    assert!(f.s.records().is_empty());
    f.untouched();
}

/// HEAD moved during validation, and the abort that follows is never
/// finished: the child killed at the abort bound may leave its locks, so the
/// write is a deadline, not a skip, and its record stays for recovery.
#[test]
fn ref_phase_head_moved_with_an_unfinished_abort_keeps_its_record() {
    let f = Fixture::new("ref-phase-head-moved-deaf");
    f.s.git(&["branch", "side"]);
    let stall = Stall {
        hold_read: true,
        mode: "deaf-abort",
        ..Stall::default()
    };
    let request = f.s.set_request(
        &f.ids[0],
        "[x] **Ship the widget.** HEAD moves, abort hangs.",
    );
    let writer = f.s.spawn("set", &request, &[stall.wrapper(&f.s), budget()]);
    logged(&f.s, "slow head read");
    move_head(&f.s);
    release_reads(&f.s);
    let (code, out) = finish(writer);
    assert_eq!(code, 0, "{out}");
    let log = proxy_log(&f.s);
    assert!(
        log.contains("deaf to abort") && !log.contains("exit "),
        "{log}"
    );
    kept_for_recovery(&f, &out);
}

/// HEAD moved during validation while the prepared child is killed from
/// outside: a child reaped after a signal did not finish an abort.
#[test]
fn ref_phase_head_moved_with_a_child_killed_during_validation_keeps_its_record() {
    let f = Fixture::new("ref-phase-head-moved-killed");
    f.s.git(&["branch", "side"]);
    let stall = Stall {
        hold_read: true,
        ..Stall::default()
    };
    let request =
        f.s.set_request(&f.ids[0], "[x] **Ship the widget.** Killed in validation.");
    let writer = f.s.spawn("set", &request, &[stall.wrapper(&f.s), budget()]);
    logged(&f.s, "slow head read");
    move_head(&f.s);
    kill_transaction(&f.s);
    release_reads(&f.s);
    let (code, out) = finish(writer);
    assert_eq!(code, 0, "{out}");
    kept_for_recovery(&f, &out);
}

/// The prepared child killed before its acknowledgment reaches Rivet, with
/// HEAD moved: an incomplete handshake from a child that died by a signal
/// is not a lost comparison, so the write is not settled as a skip.
#[test]
fn ref_phase_head_moved_with_a_child_killed_before_acknowledging_keeps_its_record() {
    let f = Fixture::new("ref-phase-head-moved-unacked");
    f.s.git(&["branch", "side"]);
    // Longer than the preparation budget: the kill below lands first, with
    // that whole budget to spare.
    let stall = Stall {
        prepare_ms: 30_000,
        ..Stall::default()
    };
    let request = f.s.set_request(
        &f.ids[0],
        "[x] **Ship the widget.** Killed before acknowledging.",
    );
    let writer = f.s.spawn("set", &request, &[stall.wrapper(&f.s), budget()]);
    logged(&f.s, "prepared");
    move_head(&f.s);
    kill_transaction(&f.s);
    let (code, out) = finish(writer);
    assert_eq!(code, 0, "{out}");
    assert!(!proxy_log(&f.s).contains("acknowledged"));
    kept_for_recovery(&f, &out);
}

/// Controls: one stall longer than its own budget is still killed, leaves
/// Git's locks, and stays human recovery.
#[test]
fn ref_phase_single_phase_overrun_is_killed_and_stays_human_recovery() {
    for (name, stall) in [
        (
            "ref-phase-prepare-overrun",
            Stall {
                prepare_ms: 2 * STALL_MS,
                ..Stall::default()
            },
        ),
        (
            "ref-phase-complete-overrun",
            Stall {
                complete_ms: 2 * STALL_MS,
                ..Stall::default()
            },
        ),
    ] {
        let f = Fixture::new(name);
        let out = f.set(
            "[x] **Ship the widget.** Overrun.",
            &[stall.wrapper(&f.s), budget()],
        );
        let candidate = read(&f.s.root, WIDGETS);
        assert!(!proxy_log(&f.s).contains("exit "), "{name}");
        let op = f.killed_with_locks(&out);
        f.recover_by_clearing(&op, &candidate);
    }
}

/// A child that stops reading before `commit` costs the completion budget,
/// not an unbounded write; its prepared locks stay human recovery.
#[test]
fn ref_phase_blocked_commit_write_is_bounded_by_completion() {
    let f = Fixture::new("ref-phase-deaf-commit");
    let stall = Stall {
        mode: "deaf-commit",
        ..Stall::default()
    };
    let started = Instant::now();
    let out = f.set(
        "[x] **Ship the widget.** Deaf to commit.",
        &[stall.wrapper(&f.s), budget()],
    );
    assert!(
        started.elapsed() < Duration::from_millis(3 * BUDGET_MS),
        "{:?}",
        started.elapsed()
    );
    let candidate = read(&f.s.root, WIDGETS);
    let log = proxy_log(&f.s);
    assert!(
        log.contains("stdin full") && !log.contains("received"),
        "{log}"
    );
    let op = f.killed_with_locks(&out);
    f.recover_by_clearing(&op, &candidate);
}

/// An abort that Git never finishes is killed at its bound; its locks stay
/// human recovery.
#[test]
fn ref_phase_unresponsive_abort_is_bounded_and_stays_human_recovery() {
    let f = Fixture::new("ref-phase-deaf-abort");
    let stall = Stall {
        mode: "deaf-abort",
        ..Stall::default()
    };
    let pause = Pause::new(&f.s.root, "ref_prepared");
    let request =
        f.s.set_request(&f.ids[0], "[x] **Ship the widget.** Deaf to abort.");
    let writer = f.s.spawn(
        "set",
        &request,
        &[stall.wrapper(&f.s), budget(), pause.env()],
    );
    pause.wait();
    std::thread::sleep(Duration::from_millis(BUDGET_MS + 1000));
    let released = Instant::now();
    pause.release();
    let (code, out) = finish(writer);
    assert!(
        released.elapsed() < Duration::from_millis(2 * BUDGET_MS),
        "{:?}",
        released.elapsed()
    );
    assert_eq!(code, 0, "{out}");
    let candidate = read(&f.s.root, WIDGETS);
    let log = proxy_log(&f.s);
    assert!(
        log.contains("deaf to abort") && !log.contains("exit "),
        "{log}"
    );
    let op = f.killed_with_locks(&out);
    f.recover_by_clearing(&op, &candidate);
}

/// A child that publishes, then keeps its output open and never exits, is
/// killed at the completion bound; recovery finishes the index.
#[test]
fn ref_phase_endless_output_is_bounded_by_completion() {
    let f = Fixture::new("ref-phase-trickle");
    let stall = Stall {
        mode: "trickle",
        ..Stall::default()
    };
    let started = Instant::now();
    let out = f.set(
        "[x] **Ship the widget.** Endless output.",
        &[stall.wrapper(&f.s), budget()],
    );
    assert!(
        started.elapsed() < Duration::from_millis(3 * BUDGET_MS),
        "{:?}",
        started.elapsed()
    );
    let candidate = read(&f.s.root, WIDGETS);
    assert!(proxy_log(&f.s).contains("trickle"));
    assert_eq!(out["commit"]["status"], "failed", "{out}");
    assert_eq!(out["commit"]["published"], true, "{out}");
    let op = out["commit"]["op"].as_str().unwrap().to_string();
    assert_eq!(f.s.carrying(&op).len(), 1);
    let rec = f.s.recover(json!({}), &[]);
    assert_eq!(one_result(&rec)["result"], "committed", "{rec}");
    f.committed_once(&op, &candidate);
}

/// The same split stimulus on the other publication routes: an `adopt`
/// write, recovery of a pending record, a writer queued behind another on
/// the same file, and a long branch name.
#[test]
fn ref_phase_split_stimulus_on_adopt_recovery_queue_and_long_branch() {
    let split = Stall {
        prepare_ms: STALL_MS,
        complete_ms: STALL_MS,
        ..Stall::default()
    };

    // `adopt`: an outside edit is committed with the write.
    let f = Fixture::on(Store::new("ref-phase-adopt", "adopt"));
    let edited = read(&f.s.root, WIDGETS).replace("The release branch is cut", "Edited outside");
    std::fs::write(f.s.root.join(WIDGETS), edited).unwrap();
    let out = f.set(
        "[x] **Ship the widget.** Adopted.",
        &[split.wrapper(&f.s), budget()],
    );
    f.committed_by_write(&out);
    assert!(f.s.message(&f.s.head()).contains("Includes-outside-edit:"));

    // Recovery: a record left by an abandoned transaction commits under it.
    let f = Fixture::new("ref-phase-recovery");
    let pause = Pause::new(&f.s.root, "ref_prepared");
    let request =
        f.s.set_request(&f.ids[0], "[x] **Ship the widget.** Recovered.");
    let writer = f.s.spawn("set", &request, &[budget(), pause.env()]);
    pause.wait();
    std::thread::sleep(Duration::from_millis(BUDGET_MS + 1000));
    pause.release();
    let (_, out) = finish(writer);
    let op = f.unpublished(&out, "deadline");
    let candidate = read(&f.s.root, WIDGETS);
    let rec = f.s.recover(json!({}), &[split.wrapper(&f.s), budget()]);
    assert_eq!(one_result(&rec)["result"], "committed", "{rec}");
    assert!(proxy_log(&f.s).contains("exit 0"));
    f.committed_once(&op, &candidate);

    // A second writer on the same file waits for the project lock, then
    // publishes under the same stimulus.
    let f = Fixture::new("ref-phase-queued");
    let vars = [split.wrapper(&f.s), budget()];
    let first = f.s.spawn(
        "set",
        &f.s.set_request(&f.ids[0], "[x] **Ship the widget.** First."),
        &vars,
    );
    logged(&f.s, "prepared");
    let second = f.s.spawn(
        "set",
        &set_item(
            &f.ids[1],
            &item_guard(&f.s.root, WIDGETS, &f.ids[1]),
            "[x] **Refresh.** Second.",
        ),
        &vars,
    );
    let (_, one) = finish(first);
    let (_, two) = finish(second);
    assert_eq!(one["commit"]["status"], "committed", "{one}");
    assert_eq!(two["commit"]["status"], "committed", "{two}");
    let commits = f.s.since(&f.base);
    assert_eq!(commits.len(), 2);
    for (out, sha) in [(&one, &commits[0]), (&two, &commits[1])] {
        let op = out["commit"]["op"].as_str().unwrap();
        assert_eq!(f.s.carrying(op), std::slice::from_ref(sha));
    }
    assert_eq!(proxy_log(&f.s).matches("exit 0").count(), 2);
    assert!(f.s.records().is_empty() && ref_locks(&f.s).is_empty());
    f.untouched();

    // A long, supported branch name in the transaction input.
    let s = Store::new("ref-phase-long-branch", "skip");
    let long = format!("feature/{}/{}", "a".repeat(120), "b".repeat(120));
    s.git(&["checkout", "-q", "-b", &long]);
    let f = Fixture::on(s);
    let out = f.set(
        "[x] **Ship the widget.** Long branch.",
        &[split.wrapper(&f.s), budget()],
    );
    f.committed_by_write(&out);
    assert_eq!(
        f.s.git(&["symbolic-ref", "HEAD"]).trim(),
        format!("refs/heads/{long}")
    );
}

#[test]
fn pair_ref_phase_split_stimulus_commits_both_paths_without_orphan_locks() {
    let s = super::pair::fixture("pair-ref-phase");
    let wrapper = Stall {
        prepare_ms: STALL_MS,
        read_ms: STALL_MS / 3,
        complete_ms: STALL_MS,
        ..Stall::default()
    }
    .wrapper(&s);
    let (code, out) = s.run("__test-pair", json!({}), &[wrapper, budget()]);
    assert_eq!(code, 0, "{out}\n{}", proxy_log(&s));
    assert_eq!(s.changed(&s.head()), ["pair-a.md", "pair-b.md"]);
    let log = proxy_log(&s);
    assert_eq!(log.matches("slow head read").count(), 2, "{log}");
    assert_eq!(log.matches("head read done").count(), 2, "{log}");
    assert!(
        log.contains("completion delay done") && log.contains("exit 0"),
        "{log}"
    );
    assert!(s.records().is_empty());
    assert!(ref_locks(&s).is_empty());
    assert!(!s.root.join(".git/index.lock").exists());
}
