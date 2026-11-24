//! Git for Rivet: one invocation policy for every git child, the snapshot a
//! commit-mode write pins (base state and eligibility), and the stale-lock
//! rule recovery applies after a git child was killed.
//!
//! Every child starts in the held project root with a cleared environment,
//! literal pathspecs, and hooks, fsmonitor, signing and automatic maintenance
//! off, so git cannot call back into Rivet or reach the network while Rivet
//! holds its lock. Each child leads its own process group, dies with Rivet,
//! and is killed and reaped by a guard on every exit path, a panic included.
use crate::Diagnostic;
use crate::contract::{ErrorCode, LockClearReason};
use crate::fs::Project;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::ffi::{CStr, CString, OsStr, OsString};
use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// How long one git child may run before its process group is killed.
pub const DEFAULT_DEADLINE: Duration = Duration::from_secs(5);
/// `core.fsync`, set on every call, needs git 2.36.
pub const MIN_VERSION: (u32, u32) = (2, 36);
/// The argument that turns the `markrivet` binary into the launch helper.
pub const HELPER: &str = "__git-exec";

const CONFIG: [&str; 6] = [
    "core.hooksPath=/dev/null",
    "core.fsmonitor=false",
    "commit.gpgsign=false",
    "gc.auto=0",
    "maintenance.auto=false",
    "core.fsync=committed,index",
];
const KEEP_ENV: [&str; 6] = ["PATH", "HOME", "XDG_CONFIG_HOME", "LANG", "LC_ALL", "TZ"];
const CONVERSION_ATTRIBUTES: [&str; 6] = [
    "filter",
    "ident",
    "working-tree-encoding",
    "text",
    "eol",
    "crlf",
];
const BUSY: [&str; 6] = [
    "MERGE_HEAD",
    "CHERRY_PICK_HEAD",
    "REVERT_HEAD",
    "rebase-merge",
    "rebase-apply",
    "sequencer",
];

/// Milliseconds since the epoch, the stamp every observation carries.
pub fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GitError {
    /// The child could not be started, or its pipes failed.
    Spawn(String),
    /// The deadline passed; the process group was killed and reaped.
    Deadline,
    /// The caller's `spawned` callback refused, so git never ran.
    Aborted(String),
    /// A bounded read exceeded its stream or request budget.
    OutputLimit(&'static str),
}

impl std::fmt::Display for GitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GitError::Spawn(e) => write!(f, "git could not run: {e}"),
            GitError::Deadline => write!(f, "git did not finish before its deadline"),
            GitError::Aborted(e) => write!(f, "git was not started: {e}"),
            GitError::OutputLimit(budget) => write!(f, "git read exceeded {budget}"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Output {
    /// `None` when the child was ended by a signal.
    pub code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub millis: u64,
}

impl Output {
    pub fn ok(&self) -> bool {
        self.code == Some(0)
    }

    pub fn failure(&self, what: &str) -> String {
        let stderr = String::from_utf8_lossy(&self.stderr);
        let first = stderr.lines().next().unwrap_or("").trim();
        match self.code {
            Some(c) => format!("{what} exited {c}: {first}"),
            None => format!("{what} was killed by a signal"),
        }
    }
}

/// Runs git under the invocation policy, from the held root of one project.
#[derive(Debug, Clone)]
pub struct Git {
    project: Project,
    program: OsString,
    pub deadline: Duration,
    /// A budget shared with other calls, such as the prepared HEAD recheck's
    /// two reads; each child ends by the earlier of it and its own deadline.
    cutoff: Option<Deadline>,
    read_budget: Option<std::sync::Arc<ReadBudget>>,
    stdout_limit: usize,
    index: Option<OsString>,
}

/// The instant one finite budget runs out. Every Git wait asks it the same
/// question, so a budget shared by several waits is never reset by one of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Deadline(Instant);

impl Deadline {
    /// A budget of `length` that starts at `start`.
    pub fn after(start: Instant, length: Duration) -> Deadline {
        Deadline(start + length)
    }

    /// Time left at `now`, or `None` once `now` has reached the end.
    pub fn remaining_at(self, now: Instant) -> Option<Duration> {
        self.0.checked_duration_since(now).filter(|d| !d.is_zero())
    }

    pub fn remaining(self) -> Option<Duration> {
        self.remaining_at(Instant::now())
    }

    pub fn expired(self) -> bool {
        self.remaining().is_none()
    }

    /// The earlier of the two.
    pub fn min(self, other: Option<Deadline>) -> Deadline {
        other.map_or(self, |o| Deadline(self.0.min(o.0)))
    }

    pub fn instant(self) -> Instant {
        self.0
    }
}

/// Shared across every Git child in one history request, including inspection.
#[derive(Debug)]
pub struct ReadBudget {
    pub received: std::sync::atomic::AtomicUsize,
    pub exceeded: std::sync::Mutex<Option<&'static str>>,
    pub limit: usize,
}

impl ReadBudget {
    pub fn new(limit: usize) -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self {
            received: std::sync::atomic::AtomicUsize::new(0),
            exceeded: std::sync::Mutex::new(None),
            limit,
        })
    }
}

impl Git {
    pub fn new(project: &Project) -> Git {
        let program = if cfg!(debug_assertions) {
            std::env::var_os("RIVET_TEST_GIT")
        } else {
            None
        };
        let mut git = Git::with_program(project, program.unwrap_or_else(|| "git".into()));
        // Debug builds only: a short deadline, so a deadline test need not
        // wait out the real one.
        if cfg!(debug_assertions)
            && let Some(ms) = std::env::var("RIVET_TEST_GIT_DEADLINE_MS")
                .ok()
                .and_then(|v| v.parse().ok())
        {
            git.deadline = Duration::from_millis(ms);
        }
        git
    }

    /// Name the git executable explicitly. Tests pass a wrapper script so a
    /// fault lands in one call without touching process environment.
    pub fn with_program(project: &Project, program: impl Into<OsString>) -> Git {
        Git {
            project: project.clone(),
            program: program.into(),
            deadline: DEFAULT_DEADLINE,
            cutoff: None,
            read_budget: None,
            stdout_limit: 1024 * 1024,
            index: None,
        }
    }

    /// The same runner, with every later child also ending by `cutoff`. A
    /// child is not started once the cutoff has passed.
    pub(crate) fn within(&self, cutoff: Deadline) -> Self {
        let mut git = self.clone();
        git.cutoff = Some(cutoff.min(self.cutoff));
        git
    }

    /// When a child started at `started` must be done: its own deadline, or
    /// the shared cutoff when that comes first.
    fn ends(&self, started: Instant) -> Deadline {
        Deadline::after(started, self.deadline).min(self.cutoff)
    }

    /// Opt into bounded pipe reads without changing existing callers.
    pub fn bounded(mut self, budget: std::sync::Arc<ReadBudget>) -> Self {
        self.read_budget = Some(budget);
        self
    }

    pub fn inspect_limit<S: AsRef<OsStr>>(
        &self,
        args: &[S],
        limit: usize,
    ) -> Result<Output, GitError> {
        self.inspect_input_limit(args, None, limit)
    }

    /// The same ceiling override for an inspection that also supplies stdin.
    pub(crate) fn inspect_input_limit<S: AsRef<OsStr>>(
        &self,
        args: &[S],
        input: Option<&[u8]>,
        limit: usize,
    ) -> Result<Output, GitError> {
        let mut git = self.clone();
        git.stdout_limit = limit;
        git.inspect(args, input)
    }

    /// Machine-readable history uses a fixed encoding, independent of the
    /// user's log display preference. All history log readers share this call.
    pub(crate) fn log(&self, args: &[&str]) -> Result<Output, GitError> {
        let mut command = vec!["log", "--encoding=UTF-8"];
        command.extend_from_slice(args);
        self.inspect(&command, None)
    }

    /// A read-only call, started directly, with `GIT_OPTIONAL_LOCKS=0`.
    pub fn inspect<S: AsRef<OsStr>>(
        &self,
        args: &[S],
        input: Option<&[u8]>,
    ) -> Result<Output, GitError> {
        self.run(args, input, None, true, None)
    }

    /// Whether git ignores `path`, by `git check-ignore`, which refuses
    /// `GIT_LITERAL_PATHSPECS` but matches its argument as a name, not a
    /// pattern. A `./` prefix keeps a leading `:` from reading as magic.
    pub fn ignored(&self, path: &str) -> Result<bool, String> {
        let arg = format!("./{path}");
        match self.run(
            &["check-ignore", "-q", "--", arg.as_str()],
            None,
            None,
            false,
            None,
        ) {
            Ok(out) if out.code == Some(1) => Ok(false),
            Ok(out) if out.ok() => Ok(true),
            Ok(out) => Err(out.failure("git check-ignore")),
            Err(e) => Err(e.to_string()),
        }
    }

    /// A mutating call, started through the launch helper. `spawned` gets the
    /// PID git will have and the time just before the spawn; git starts only
    /// once it returns `Ok`, so the caller can record both durably first.
    pub fn mutate<S, F>(&self, args: &[S], spawned: F) -> Result<Output, GitError>
    where
        S: AsRef<OsStr>,
        F: FnOnce(u32, SystemTime) -> Result<(), Diagnostic>,
    {
        let mut once = Some(spawned);
        let mut call = |pid, at| (once.take().expect("called once"))(pid, at);
        self.run(args, None, Some(&mut call), true, None)
    }

    fn run<S: AsRef<OsStr>>(
        &self,
        args: &[S],
        input: Option<&[u8]>,
        spawned: Option<&mut dyn FnMut(u32, SystemTime) -> Result<(), Diagnostic>>,
        literal: bool,
        prepared: Option<&mut dyn FnMut(Deadline) -> Result<(), Diagnostic>>,
    ) -> Result<Output, GitError> {
        if let Some(budget) = &self.read_budget
            && let Some(name) = *budget.exceeded.lock().expect("budget mutex")
        {
            return Err(GitError::OutputLimit(name));
        }
        if self.cutoff.is_some_and(Deadline::expired) {
            return Err(GitError::Deadline);
        }
        let mut started = Instant::now();
        let spawn_err = |e: std::io::Error| GitError::Spawn(e.to_string());
        let go = match spawned {
            Some(_) => Some(pipe().map_err(spawn_err)?),
            None => None,
        };
        let mut cmd = match &go {
            Some((read, _)) => {
                let mut c = Command::new("/proc/self/exe");
                c.arg(HELPER)
                    .arg(read.as_raw_fd().to_string())
                    .arg(&self.program);
                c
            }
            None => Command::new(&self.program),
        };
        for setting in CONFIG {
            cmd.arg("-c").arg(setting);
        }
        cmd.args(args).env_clear();
        for key in KEEP_ENV {
            if let Some(value) = std::env::var_os(key) {
                cmd.env(key, value);
            }
        }
        if literal {
            cmd.env("GIT_LITERAL_PATHSPECS", "1");
        }
        if let Some(index) = &self.index {
            cmd.env("GIT_INDEX_FILE", index);
        }
        cmd.env("GIT_NO_REPLACE_OBJECTS", "1");
        if self.read_budget.is_some() {
            cmd.env("GIT_NO_LAZY_FETCH", "1");
        }
        if go.is_none() {
            cmd.env("GIT_OPTIONAL_LOCKS", "0");
        }
        cmd.stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
        let parent = std::process::id() as libc::pid_t;
        let root = self.project.root_fd();
        let helper_fd = go.as_ref().map(|(read, _)| read.as_raw_fd());
        let pause = ChildPause::from_env();
        // SAFETY: the closure runs between fork and exec and makes only
        // async-signal-safe calls on values prepared before the fork.
        unsafe {
            cmd.pre_exec(move || {
                if libc::setpgid(0, 0) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if let Some(pause) = &pause {
                    pause.wait();
                }
                if libc::prctl(
                    libc::PR_SET_PDEATHSIG,
                    libc::SIGKILL as libc::c_ulong,
                    0,
                    0,
                    0,
                ) != 0
                {
                    return Err(std::io::Error::last_os_error());
                }
                // Rivet died before the signal was armed: nobody will kill
                // this child, so it must not run git.
                if libc::getppid() != parent {
                    libc::_exit(125);
                }
                if libc::fchdir(root) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if let Some(fd) = helper_fd
                    && libc::fcntl(fd, libc::F_SETFD, 0) != 0
                {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let spawn_time = SystemTime::now();
        let child = cmd.spawn().map_err(spawn_err)?;
        let mut guard = ChildGuard { child: Some(child) };
        if let (Some(spawned), Some((read, write))) = (spawned, go) {
            drop(read);
            spawned(guard.pid(), spawn_time).map_err(|d| GitError::Aborted(d.message))?;
            File::from(write).write_all(&[1]).map_err(spawn_err)?;
            // The deadline covers git, not the caller's durable bookkeeping.
            started = Instant::now();
            // Debug builds only: unwind while git runs, so a test can see the
            // guard kill it before the Rivet lock is released.
            if cfg!(debug_assertions)
                && std::env::var("RIVET_TEST_FAULT").is_ok_and(|f| f == "git_panic")
            {
                let _ = crate::fs::hook("git_panic");
                panic!("injected git_panic fault");
            }
        }
        let child = guard.child.as_mut().expect("held until reaped");
        // The ordinary deadline. A ref transaction replaces it phase by phase.
        let mut ends = self.ends(started);
        let mut transaction_stdout: Option<Box<dyn Read + Send>> = None;
        // Why a prepared transaction was abandoned without `commit`.
        let mut abandoned: Option<GitError> = None;
        if let Some(prepared) = prepared {
            // Preparation: the input and Git's acknowledgment, from the
            // recorded start.
            let stdin = child.stdin.take().expect("transaction stdin");
            let stdin = write_by(stdin, input.unwrap_or_default(), ends)?;
            let stream = child.stdout.take().expect("transaction stdout");
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let mut reader = std::io::BufReader::new(stream);
                let mut prefix = Vec::new();
                // Bound the handshake as well as its wall time. Ordinary output
                // draining below owns the rest of the stream after preparation.
                for _ in 0..4096 {
                    let mut byte = [0];
                    if reader.read_exact(&mut byte).is_err() {
                        break;
                    }
                    prefix.push(byte[0]);
                    if prefix.iter().filter(|b| **b == b'\n').count() == 2 {
                        break;
                    }
                }
                let _ = tx.send((reader, prefix, Instant::now()));
            });
            let (reader, prefix, acknowledged) = rx
                .recv_timeout(ends.remaining().unwrap_or_default())
                .map_err(|_| GitError::Deadline)?;
            // An acknowledgment queued after the budget ran out is too late.
            if acknowledged >= ends.instant() {
                return Err(GitError::Deadline);
            }
            if prefix != b"start: ok\nprepare: ok\n" {
                // Git did not prepare. Only its own exit within the
                // preparation budget confirms it released what it locked; a
                // child that died by a signal or is still running does not.
                drop(stdin);
                return Err(match guard.wait_until(ends.instant()) {
                    Ok(exit) if exit.code().is_some() => {
                        GitError::Aborted("git ref transaction did not prepare".into())
                    }
                    Ok(exit) => GitError::Spawn(format!(
                        "git ref transaction ended {exit} before preparing"
                    )),
                    Err(e) => e,
                });
            }
            // Validation: the caller's checks share one budget, and a check
            // that returns after it cannot authorize publication.
            let validate = Deadline::after(acknowledged, self.deadline);
            let checked = prepared(validate);
            // Completion or abort: one budget, started once, never reset.
            ends = Deadline::after(Instant::now(), self.deadline);
            // Without `commit`, EOF makes Git abort the prepared transaction
            // and release its locks: the pipe is closed, the child not killed.
            match checked {
                _ if validate.expired() => {
                    drop(stdin);
                    abandoned = Some(GitError::Deadline);
                }
                Err(d) => {
                    drop(stdin);
                    abandoned = Some(GitError::Aborted(d.message));
                }
                Ok(()) => drop(write_by(stdin, b"commit\n", ends)?),
            }
            transaction_stdout = Some(Box::new(std::io::Cursor::new(prefix).chain(reader)));
        }
        let writer = match (input, child.stdin.take()) {
            (Some(bytes), Some(mut stdin)) => {
                let bytes = bytes.to_vec();
                Some(std::thread::spawn(move || {
                    let _ = stdin.write_all(&bytes);
                }))
            }
            _ => None,
        };
        if let Some(budget) = &self.read_budget {
            return capped_output(&mut guard, started, ends, self.stdout_limit, budget);
        }
        // Readers report through channels, so a descendant that escaped the
        // process group and kept a pipe open cannot hold Rivet past the
        // deadline.
        let drain = |r: Option<Box<dyn Read + Send>>| {
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let mut buf = vec![];
                if let Some(mut r) = r {
                    let _ = r.read_to_end(&mut buf);
                }
                let _ = tx.send(buf);
            });
            rx
        };
        let stdout = drain(transaction_stdout.or_else(|| {
            child
                .stdout
                .take()
                .map(|s| Box::new(s) as Box<dyn Read + Send>)
        }));
        let stderr = drain(
            child
                .stderr
                .take()
                .map(|s| Box::new(s) as Box<dyn Read + Send>),
        );
        let status = guard.wait_until(ends.instant());
        if let Some(why) = abandoned {
            // Only an abort Git finished, exiting 0 after EOF, reports why the
            // transaction was abandoned. A child killed at its bound may have
            // left its locks and is a deadline; one that died by a signal or
            // failed, or whose reap failed, is a Git error.
            return Err(match status {
                Ok(exit) if exit.success() => why,
                Ok(exit) => GitError::Spawn(format!("git ref transaction abort ended {exit}")),
                Err(GitError::Spawn(e)) => GitError::Spawn(e),
                Err(_) => GitError::Deadline,
            });
        }
        let status = status?;
        // The stdin writer is left detached: git has exited, and a
        // descendant still holding the pipe must not hold Rivet.
        drop(writer);
        let collect = |rx: std::sync::mpsc::Receiver<Vec<u8>>| {
            rx.recv_timeout(ends.remaining().unwrap_or_default())
                .map_err(|_| GitError::Deadline)
        };
        let stdout = collect(stdout)?;
        let stderr = collect(stderr)?;
        Ok(Output {
            code: status.code(),
            stdout,
            stderr,
            millis: started.elapsed().as_millis() as u64,
        })
    }
}

impl Git {
    /// Only this invocation-owned override survives environment clearing.
    pub(crate) fn with_index(&self, index: &PrivateIndex) -> Self {
        let mut git = self.clone();
        git.index = Some(index.path().into_os_string());
        git
    }

    /// Immutable object storage does not stage paths or publish refs. It runs
    /// before a pending record exists, under the same deadline/process policy.
    pub(crate) fn store_blob(&self, bytes: &[u8]) -> Result<Output, GitError> {
        self.run(
            &["hash-object", "-w", "--no-filters", "--stdin"],
            Some(bytes),
            None,
            true,
            None,
        )
    }

    /// A recorded ref transaction: recheck HEAD after Git has prepared and
    /// locked its ref updates, before allowing it to publish anything.
    ///
    /// Unlike every other call, this child has three budgets of `deadline`
    /// each: preparation (from the recorded start to Git's acknowledgment),
    /// validation (`prepared`, which receives its end and must share it across
    /// its own Git reads) and completion or abort (from just before `commit`,
    /// or the EOF that aborts, to Git's exit and output). A validation that
    /// fails or returns late sends no `commit`.
    pub(crate) fn publish<F, P>(
        &self,
        input: &[u8],
        spawned: F,
        mut prepared: P,
    ) -> Result<Output, GitError>
    where
        F: FnOnce(u32, SystemTime) -> Result<(), Diagnostic>,
        P: FnMut(Deadline) -> Result<(), Diagnostic>,
    {
        let mut once = Some(spawned);
        let mut call = |pid, at| (once.take().expect("called once"))(pid, at);
        self.run(
            &["update-ref", "--stdin", "--no-deref"],
            Some(input),
            Some(&mut call),
            true,
            Some(&mut prepared),
        )
    }
}

/// Git's racy-stat detection depends on the index timestamp as well as its
/// bytes. Both copies must retain that timestamp, including its nanoseconds.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct IndexImage {
    bytes: Vec<u8>,
    modified: SystemTime,
}

impl IndexImage {
    fn copy_to(&self, file: &mut File) -> std::io::Result<()> {
        self.copy_to_with_hook(file, "private_index_partial_write")
    }

    fn copy_to_with_hook(&self, file: &mut File, point: &str) -> std::io::Result<()> {
        use std::io::{Seek, SeekFrom};
        file.set_len(0)?;
        file.seek(SeekFrom::Start(0))?;
        let split = self.bytes.len() / 2;
        file.write_all(&self.bytes[..split])?;
        crate::fs::hook(point).map_err(|e| std::io::Error::other(e.detail()))?;
        file.write_all(&self.bytes[split..])?;
        file.set_times(std::fs::FileTimes::new().set_modified(self.modified))?;
        file.sync_all()
    }
}

/// A private index under the held pending directory. Its name is derived
/// from the record, never from a document path or an ambient environment.
pub(crate) struct PrivateIndex {
    dir: OwnedFd,
    name: String,
}

impl PrivateIndex {
    pub(crate) fn new(project: &Project, key: &str) -> std::io::Result<Self> {
        let rivet = open_dir_at(project.root_fd(), c".rivet")?;
        let dir = open_dir_at(rivet.as_raw_fd(), c"pending")?;
        let this = Self {
            dir,
            name: format!("index-{key}"),
        };
        this.remove();
        Ok(this)
    }

    fn path(&self) -> std::path::PathBuf {
        format!(
            "/proc/{}/fd/{}/{}",
            std::process::id(),
            self.dir.as_raw_fd(),
            self.name
        )
        .into()
    }

    pub(crate) fn seed(&self, image: &IndexImage) -> std::io::Result<()> {
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(self.path())?;
        image.copy_to(&mut f)
    }

    pub(crate) fn bytes(&self) -> std::io::Result<IndexImage> {
        read_plain(
            self.dir.as_raw_fd(),
            &CString::new(self.name.as_str()).expect("derived name"),
        )?
        .ok_or_else(|| std::io::Error::other("private index missing"))
    }

    fn remove(&self) {
        for name in [&self.name, &format!("{}.lock", self.name)] {
            if let Ok(name) = CString::new(name.as_str()) {
                // SAFETY: held directory, leaf name; no link is followed.
                unsafe {
                    libc::unlinkat(self.dir.as_raw_fd(), name.as_ptr(), 0);
                }
            }
        }
    }
}

impl Drop for PrivateIndex {
    fn drop(&mut self) {
        self.remove();
    }
}

/// Stable evidence for Rivet's index lock, which can span several Git children.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexLease {
    pub dev: u64,
    pub ino: u64,
    pub acquired_ns: u64,
}

impl IndexLease {
    pub(crate) fn matches(&self, file: &str, identity: &str) -> bool {
        file == ".git/index.lock" && identity.starts_with(&format!("{}:{}:", self.dev, self.ino))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Fence {
    pub dev: u64,
    pub ino: u64,
    pub btime_ns: u64,
}

impl Fence {
    fn of(file: &File) -> std::io::Result<Self> {
        use std::os::unix::fs::MetadataExt;
        let meta = file.metadata()?;
        if !meta.is_file() {
            return Err(std::io::Error::other("fence is not a regular file"));
        }
        let unsupported = |e| std::io::Error::new(std::io::ErrorKind::Unsupported, e);
        crate::fs::fault("move_fence_birth").map_err(unsupported)?;
        let birth = meta
            .created()
            .map_err(unsupported)?
            .duration_since(UNIX_EPOCH)
            .map_err(|e| unsupported(std::io::Error::other(e)))?;
        Ok(Self {
            dev: meta.dev(),
            ino: meta.ino(),
            btime_ns: u64::try_from(birth.as_nanos())
                .map_err(|e| unsupported(std::io::Error::other(e)))?,
        })
    }

    fn at(dir: RawFd, name: &CStr) -> std::io::Result<Self> {
        // SAFETY: fixed leaf in a held directory; never follow a symlink or
        // block on a non-regular file swapped in during the listing.
        let fd = unsafe {
            libc::openat(
                dir,
                name.as_ptr(),
                libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: the fresh descriptor is owned by this File.
        Self::of(&unsafe { File::from_raw_fd(fd) })
    }
}

/// Why a pair fence was not taken. `Unsupported` and `Io` mean no staging
/// entry was created; `Staged` and `Named` mean one was, so the
/// record must stay pending and the entry stays for the operator.
#[derive(Debug)]
pub(crate) enum FenceError {
    /// No birth time, or no atomic no-replace rename; nothing created.
    Unsupported(std::io::Error),
    /// Failed before any staging entry was created.
    Io(std::io::Error),
    /// A `.git/rivet-fence-<op>` entry exists and `index.lock` was not named:
    /// one found from an earlier attempt, or this attempt's, left after a later
    /// step failed. Rivet never removes it; the detail says what the operator
    /// does.
    Staged(String),
    /// `index.lock` is, or may be, named and retained: the published record
    /// owns it, so the record must stay for recovery to take the fence over.
    Named(std::io::Error),
}

/// Rivet's own staging name for a fence, derived from its op.
fn fence_staging(op: &str) -> std::io::Result<CString> {
    if op.is_empty() || !op.bytes().all(|b| b.is_ascii_alphanumeric()) {
        return Err(std::io::Error::other("op is not a plain name"));
    }
    CString::new(format!("rivet-fence-{op}")).map_err(std::io::Error::other)
}

/// The operator step for a staging entry Rivet leaves in place.
fn staged(name: &CStr, why: &str) -> FenceError {
    FenceError::Staged(staged_detail(name, why))
}

fn staged_detail(name: &CStr, why: &str) -> String {
    format!(
        ".git/{} is present ({why}); with Git and Rivet writers stopped, check \
         it and remove it, then run commit-pending",
        name.to_string_lossy()
    )
}

/// The operator step when this op's staging entry is present, checked before
/// any fence takeover or acquisition: while it exists no fence is taken, and
/// it is reported whatever else holds `index.lock`.
pub(crate) fn staging_present(project: &Project, op: &str) -> std::io::Result<Option<String>> {
    let staging = fence_staging(op)?;
    let dir = open_dir_at(project.root_fd(), c".git")?;
    match stat_at(dir.as_raw_fd(), &staging) {
        Ok(_) => {
            let held = stat_at(dir.as_raw_fd(), c"index.lock").is_ok();
            let why = if held {
                "left by an earlier attempt; .git/index.lock is held too and is not this record's fence"
            } else {
                "left by an earlier attempt"
            };
            Ok(Some(staged_detail(&staging, why)))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// The two names of the persistent no-replace probe, exactly one of which
/// exists between probes. It is never removed: a check then a removal by name
/// is the race the staging rule forbids.
const PROBE_A: &CStr = c"rivet-noreplace-a";
const PROBE_B: &CStr = c"rivet-noreplace-b";

/// Proof that this `.git` renamed a regular file without replacing, holding
/// the directory descriptor it was proved on. A fence acquisition takes one
/// and uses its descriptor, so each acquisition probes exactly once.
pub(crate) struct NoReplace {
    dir: OwnedFd,
}

impl NoReplace {
    /// Prove no-replace rename on `.git` by renaming the empty probe file
    /// between its two names. The probe never unlinks, truncates, writes,
    /// copies or replaces anything; it renames only what is at its own name,
    /// and an entry swapped in there is moved with its bytes and reported.
    /// `Unsupported`: the rename is refused as unsupported. `Io`: obstructed,
    /// or another error, with the operator step in the detail.
    pub(crate) fn prove(project: &Project) -> Result<Self, FenceError> {
        let dir = open_dir_at(project.root_fd(), c".git").map_err(FenceError::Io)?;
        Self::prove_on(dir)
    }

    fn prove_on(dir: OwnedFd) -> Result<Self, FenceError> {
        let at = dir.as_raw_fd();
        let obstructed = |why: String| {
            FenceError::Io(std::io::Error::other(format!(
                "the no-replace probe at .git/rivet-noreplace-a and .git/rivet-noreplace-b \
                 is obstructed ({why}); with Git and Rivet writers stopped, check them, leave \
                 at most one empty regular file under those names, and retry"
            )))
        };
        let present = |name: &CStr| match stat_at(at, name) {
            Ok(_) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(FenceError::Io(e)),
        };
        let (source, target) = match (present(PROBE_A)?, present(PROBE_B)?) {
            (true, false) => (PROBE_A, PROBE_B),
            (false, true) => (PROBE_B, PROBE_A),
            (true, true) => return Err(obstructed("both names are present".into())),
            (false, false) => {
                // SAFETY: held directory, Rivet-owned leaf, exclusive creation.
                let fd = unsafe {
                    libc::openat(
                        at,
                        PROBE_A.as_ptr(),
                        libc::O_WRONLY
                            | libc::O_CREAT
                            | libc::O_EXCL
                            | libc::O_NOFOLLOW
                            | libc::O_CLOEXEC,
                        0o600,
                    )
                };
                if fd < 0 {
                    let e = std::io::Error::last_os_error();
                    return Err(if e.raw_os_error() == Some(libc::EEXIST) {
                        obstructed("another process created the probe".into())
                    } else {
                        FenceError::Io(e)
                    });
                }
                // SAFETY: openat returned an owned descriptor; dropping closes it.
                drop(unsafe { File::from_raw_fd(fd) });
                (PROBE_A, PROBE_B)
            }
        };
        let shown = |name: &CStr| format!(".git/{}", name.to_string_lossy());
        // SAFETY: fixed leaf in a held directory; no symlink is followed and
        // no special file blocks the open.
        let fd = unsafe {
            libc::openat(
                at,
                source.as_ptr(),
                libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            let e = std::io::Error::last_os_error();
            return Err(obstructed(format!(
                "{} cannot be opened: {e}",
                shown(source)
            )));
        }
        // SAFETY: openat returned an owned descriptor.
        let file = unsafe { File::from_raw_fd(fd) };
        let empty_file =
            |st: &libc::stat| st.st_mode & libc::S_IFMT == libc::S_IFREG && st.st_size == 0;
        let held = fstat_fd(file.as_raw_fd()).map_err(FenceError::Io)?;
        if !empty_file(&held) {
            return Err(obstructed(format!(
                "{} is not an empty regular file",
                shown(source)
            )));
        }
        let kept = (held.st_dev, held.st_ino);
        crate::fs::hook("move_noreplace_probe")
            .map_err(|d| FenceError::Io(std::io::Error::other(d.detail().to_string())))?;
        let renamed = crate::fs::fault_errno("move_noreplace_rename").and_then(|()| {
            // SAFETY: both names are fixed leaves in one held directory.
            let ret = unsafe {
                libc::renameat2(
                    at,
                    source.as_ptr(),
                    at,
                    target.as_ptr(),
                    libc::RENAME_NOREPLACE,
                )
            };
            if ret != 0 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
        match renamed.map_err(|e| (e.raw_os_error(), e)) {
            Ok(()) => {}
            Err((Some(libc::EINVAL | libc::ENOSYS), e)) => {
                return Err(FenceError::Unsupported(std::io::Error::new(
                    std::io::ErrorKind::Unsupported,
                    format!("no-replace rename is unsupported on .git: {e}"),
                )));
            }
            Err((Some(libc::EEXIST), _)) => {
                return Err(obstructed(format!("{} appeared", shown(target))));
            }
            Err((Some(libc::EXDEV | libc::EBUSY), e)) => {
                return Err(obstructed(format!("a mount at either name: {e}")));
            }
            Err((_, e)) => return Err(obstructed(format!("the probe rename failed: {e}"))),
        }
        // The rename names a path: whatever was at the source in the gap is
        // what moved. It must be the inode checked above, still empty. The
        // held descriptor's identity is compared too, as defence in depth.
        let moved = stat_at(at, target).map_err(FenceError::Io)?;
        let held = fstat_fd(file.as_raw_fd()).map_err(FenceError::Io)?;
        let same = (moved.st_dev, moved.st_ino) == kept;
        let empty = empty_file(&moved);
        if !same || !empty || (held.st_dev, held.st_ino) != kept {
            return Err(obstructed(format!(
                "the entry renamed from {} to {} is not the empty probe checked before it",
                shown(source),
                shown(target)
            )));
        }
        Ok(Self { dir })
    }
}

fn fstat_fd(fd: RawFd) -> std::io::Result<libc::stat> {
    // SAFETY: a zeroed stat is a valid out-parameter.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: a held descriptor and an owned out-parameter.
    if unsafe { libc::fstat(fd, &mut st) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(st)
}

/// Birth time on the held `.git` directory, checked before creating anything:
/// a filesystem without it cannot host a fence.
fn birth_supported(dir: RawFd) -> std::io::Result<()> {
    let unsupported = |e| std::io::Error::new(std::io::ErrorKind::Unsupported, e);
    crate::fs::fault("move_fence_birth").map_err(unsupported)?;
    std::fs::metadata(format!("/proc/self/fd/{dir}"))?
        .created()
        .map_err(unsupported)
        .map(drop)
}

impl IndexLock {
    /// A pair fence whose identity is durably published before `index.lock`
    /// exists, so no crash can leave a fence no record names. The inode is
    /// created as `.git/rivet-fence-<op>`, `publish` records its identity,
    /// and a no-replace rename then takes `index.lock`, keeping the inode
    /// and birth time the record names. The fence is retained from the rename
    /// on: only an explicit, synced `release` removes it. `proof` is the
    /// caller's [`NoReplace`] for this acquisition; its `.git` is used here.
    pub(crate) fn acquire_published(
        op: &str,
        proof: NoReplace,
        publish: impl FnOnce(&Fence) -> Result<(), Diagnostic>,
    ) -> Result<Self, FenceError> {
        use std::os::unix::fs::MetadataExt;
        let io = FenceError::Io;
        let staging = fence_staging(op).map_err(io)?;
        // The `.git` the no-replace proof was made on; no second probe.
        let dir = proof.dir;
        let at = dir.as_raw_fd();
        // Rivet never unlinks a staging entry: a check by name and an unlink by
        // name leave a gap in which a replacement would be deleted. An entry
        // leaves its name only through this call's own rename below.
        match stat_at(at, &staging) {
            Ok(_) => return Err(staged(&staging, "left by an earlier attempt")),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(io(e)),
        }
        birth_supported(at).map_err(|e| {
            if e.kind() == std::io::ErrorKind::Unsupported {
                FenceError::Unsupported(e)
            } else {
                io(e)
            }
        })?;
        // SAFETY: held directory, Rivet-owned leaf, exclusive creation.
        let fd = unsafe {
            libc::openat(
                at,
                staging.as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0o666,
            )
        };
        if fd < 0 {
            let e = std::io::Error::last_os_error();
            return Err(if e.raw_os_error() == Some(libc::EEXIST) {
                staged(&staging, "appeared while this fence was prepared")
            } else {
                io(e)
            });
        }
        // SAFETY: openat returned an owned descriptor.
        let file = unsafe { File::from_raw_fd(fd) };
        // From here an entry exists, and every failure leaves it in place.
        // Each pause hook sits beside the fault that follows it, so a test can
        // replace the entry before that step fails.
        let paused = |point| {
            crate::fs::hook(point).map_err(|d| std::io::Error::other(d.detail().to_string()))
        };
        let fence = paused("move_fence_created")
            .and_then(|()| Fence::of(&file))
            .and_then(|fence| {
                paused("move_fence_identified")?;
                crate::fs::fault("move_fence_staging_sync")?;
                file.sync_all().map(|()| fence)
            })
            .map_err(|e| staged(&staging, &format!("its fence could not be prepared: {e}")))?;
        if let Err(d) = publish(&fence) {
            return Err(staged(
                &staging,
                &format!("its identity could not be published: {}", d.detail()),
            ));
        }
        // A path rename cannot be bound to the held inode, so check the name
        // on both sides of it. Whatever another process put there is never
        // removed or adopted here.
        let ours = |name: &CStr| Fence::at(at, name).is_ok_and(|found| found == fence);
        if !ours(&staging) {
            return Err(staged(&staging, "it no longer holds the published inode"));
        }
        let renamed = crate::fs::fault("move_fence_noreplace")
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Unsupported, e))
            .and_then(|()| crate::fs::fault("move_fence_rename"))
            .and_then(|()| {
                // SAFETY: both names are fixed leaves in one held directory.
                let ret = unsafe {
                    libc::renameat2(
                        at,
                        staging.as_ptr(),
                        at,
                        c"index.lock".as_ptr(),
                        libc::RENAME_NOREPLACE,
                    )
                };
                if ret != 0 {
                    Err(std::io::Error::last_os_error())
                } else {
                    Ok(())
                }
            });
        if let Err(e) = renamed {
            // Where cleanup once ran: nothing is removed after this point.
            let _ = crate::fs::hook("move_fence_refused");
            let why = if e.kind() == std::io::ErrorKind::Unsupported {
                "no-replace rename is unsupported here"
            } else {
                match e.raw_os_error() {
                    Some(libc::EEXIST) => ".git/index.lock is held by another writer",
                    Some(libc::EINVAL | libc::ENOSYS) => "no-replace rename is unsupported here",
                    // Not proven unrenamed: index.lock may be this fence.
                    _ => return Err(FenceError::Named(e)),
                }
            };
            return Err(staged(
                &staging,
                &format!("the rename onto index.lock failed: {why}"),
            ));
        }
        let named = FenceError::Named;
        if !ours(c"index.lock") {
            // Swapped in after the check above: the record names an inode
            // that index.lock is not, so recovery treats this lock as foreign.
            return Err(named(std::io::Error::other(
                "index.lock is not the published fence",
            )));
        }
        let meta = file.metadata().map_err(named)?;
        let acquired_ns = meta
            .modified()
            .map_err(named)?
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
            .min(u64::MAX as u128) as u64;
        let mut this = Self {
            dir,
            file,
            lease: IndexLease {
                dev: meta.dev(),
                ino: meta.ino(),
                acquired_ns,
            },
            installed: false,
            original: None,
            retained: true,
        };
        crate::fs::fault("move_fence_renamed").map_err(named)?;
        this.sync_fence_directory().map_err(named)?;
        crate::fs::fault("move_fence_index_read").map_err(named)?;
        this.original = this.bytes().map_err(named)?;
        Ok(this)
    }

    /// Adopt the same inode without unlinking the fence, even if an earlier
    /// install partially wrote it. The next install truncates before writing.
    pub(crate) fn take_over(project: &Project, expected: &Fence) -> std::io::Result<Self> {
        use std::os::unix::fs::MetadataExt;
        let dir = open_dir_at(project.root_fd(), c".git")?;
        // SAFETY: fixed leaf in a held directory, no symlinks or blocking opens.
        let fd = unsafe {
            libc::openat(
                dir.as_raw_fd(),
                c"index.lock".as_ptr(),
                libc::O_WRONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: the descriptor returned above is owned.
        let file = unsafe { File::from_raw_fd(fd) };
        if &Fence::of(&file)? != expected {
            return Err(std::io::Error::other("foreign index fence"));
        }
        let meta = file.metadata()?;
        let mut this = Self {
            dir,
            file,
            lease: IndexLease {
                dev: meta.dev(),
                ino: meta.ino(),
                acquired_ns: expected.btime_ns,
            },
            installed: false,
            original: None,
            retained: true,
        };
        if !this.owns_name() {
            return Err(std::io::Error::other("fence replaced"));
        }
        this.original = this.bytes()?;
        Ok(this)
    }

    pub(crate) fn retain(&mut self) {
        self.retained = true;
    }

    /// Call only after proving that every owned path equals HEAD and the index.
    pub(crate) fn release(mut self) -> std::io::Result<()> {
        if !self.owns_name() {
            return Err(std::io::Error::other("fence replaced"));
        }
        // SAFETY: fixed leaf whose inode is still the one held by this descriptor.
        if unsafe { libc::unlinkat(self.dir.as_raw_fd(), c"index.lock".as_ptr(), 0) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        self.installed = true;
        self.sync_fence_directory()
    }

    fn sync_fence_directory(&self) -> std::io::Result<()> {
        crate::fs::fault("move_fence_sync")?;
        // SAFETY: held directory descriptor.
        if unsafe { libc::fsync(self.dir.as_raw_fd()) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }
}

/// The real index stays locked from its base-entry check through ref publication
/// and durable installation. Other staged paths are copied, never rebuilt from HEAD.
pub(crate) struct IndexLock {
    dir: OwnedFd,
    file: File,
    pub lease: IndexLease,
    installed: bool,
    original: Option<IndexImage>,
    retained: bool,
}

impl IndexLock {
    pub(crate) fn acquire(project: &Project) -> std::io::Result<Self> {
        use std::os::unix::fs::MetadataExt;
        let dir = open_dir_at(project.root_fd(), c".git")?;
        // SAFETY: held directory, fixed leaf, exclusive creation.
        let fd = unsafe {
            libc::openat(
                dir.as_raw_fd(),
                c"index.lock".as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0o666,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: openat returned an owned descriptor.
        let file = unsafe { File::from_raw_fd(fd) };
        let meta = file.metadata()?;
        let acquired_ns = meta
            .modified()?
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
            .min(u64::MAX as u128) as u64;
        let mut this = Self {
            dir,
            file,
            lease: IndexLease {
                dev: meta.dev(),
                ino: meta.ino(),
                acquired_ns,
            },
            installed: false,
            original: None,
            retained: false,
        };
        this.original = this.bytes()?;
        Ok(this)
    }

    pub(crate) fn bytes(&self) -> std::io::Result<Option<IndexImage>> {
        read_plain(self.dir.as_raw_fd(), c"index")
    }

    pub(crate) fn install(self, image: &IndexImage) -> std::io::Result<()> {
        self.install_checked(image, || Ok(()))
    }

    /// Validate after all blocking index I/O, immediately before consuming the fence.
    pub(crate) fn install_checked(
        mut self,
        image: &IndexImage,
        validate: impl FnOnce() -> std::io::Result<()>,
    ) -> std::io::Result<()> {
        if self.bytes()? != self.original {
            return Err(std::io::Error::other("index changed outside its lock"));
        }
        image.copy_to_with_hook(&mut self.file, "index_partial_write")?;
        crate::fs::fault("index_sync")?;
        if !self.owns_name() {
            return Err(std::io::Error::other("index lock replaced"));
        }
        validate()?;
        // SAFETY: fixed names within a held directory.
        if unsafe {
            libc::renameat(
                self.dir.as_raw_fd(),
                c"index.lock".as_ptr(),
                self.dir.as_raw_fd(),
                c"index".as_ptr(),
            )
        } != 0
        {
            return Err(std::io::Error::last_os_error());
        }
        self.installed = true;
        self.sync_directory()
    }

    fn sync_directory(&self) -> std::io::Result<()> {
        crate::fs::fault("index_directory_sync")?;
        // SAFETY: held directory.
        if unsafe { libc::fsync(self.dir.as_raw_fd()) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }

    /// Retry durability without rewriting an index already installed at HEAD.
    pub(crate) fn sync_existing(&self) -> std::io::Result<()> {
        use std::os::unix::fs::OpenOptionsExt;
        let path = format!("/proc/self/fd/{}/index", self.dir.as_raw_fd());
        match std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(path)
        {
            Ok(file) if file.metadata()?.is_file() => file.sync_all()?,
            Ok(_) => return Err(std::io::Error::other("index is not a regular file")),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && self.original.is_none() => {}
            Err(e) => return Err(e),
        }
        self.sync_directory()
    }

    fn owns_name(&self) -> bool {
        stat_at(self.dir.as_raw_fd(), c"index.lock")
            .is_ok_and(|st| st.st_dev == self.lease.dev && st.st_ino == self.lease.ino)
    }
}

impl Drop for IndexLock {
    fn drop(&mut self) {
        if !self.installed && !self.retained && self.owns_name() {
            // SAFETY: remove only the fixed leaf whose inode we still own.
            unsafe {
                libc::unlinkat(self.dir.as_raw_fd(), c"index.lock".as_ptr(), 0);
            }
        }
    }
}

fn read_plain(dir: RawFd, name: &CStr) -> std::io::Result<Option<IndexImage>> {
    // SAFETY: held directory and fixed/derived leaf name, no symlink traversal.
    let fd = unsafe {
        libc::openat(
            dir,
            name.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        let err = std::io::Error::last_os_error();
        return if err.kind() == std::io::ErrorKind::NotFound {
            Ok(None)
        } else {
            Err(err)
        };
    }
    // SAFETY: openat returned an owned descriptor.
    let mut f = unsafe { File::from_raw_fd(fd) };
    let meta = f.metadata()?;
    if !meta.is_file() {
        return Err(std::io::Error::other("index is not a regular file"));
    }
    let mut bytes = Vec::new();
    f.read_to_end(&mut bytes)?;
    Ok(Some(IndexImage {
        bytes,
        modified: meta.modified()?,
    }))
}

fn pipe() -> std::io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0; 2];
    // SAFETY: a two-element array is the out-parameter pipe2 expects.
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: pipe2 returned two fresh descriptors owned from here on.
    Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}

/// Write `bytes` to a transaction's stdin by `ends`, on this thread, and hand
/// the pipe back so the caller decides when Git sees EOF. The descriptor is
/// nonblocking and every write is preceded by a check of the absolute end, so
/// a child that stops reading costs the phase its budget and nothing is
/// written once the phase is over.
fn write_by(
    stdin: std::process::ChildStdin,
    bytes: &[u8],
    ends: Deadline,
) -> Result<std::process::ChildStdin, GitError> {
    let fd = stdin.as_raw_fd();
    let os_err = || GitError::Spawn(std::io::Error::last_os_error().to_string());
    // SAFETY: a valid pipe descriptor owned by `stdin`.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    // SAFETY: as above; only this writer's file status flags change.
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(os_err());
    }
    let mut rest = bytes;
    while !rest.is_empty() {
        let Some(left) = ends.remaining() else {
            return Err(GitError::Deadline);
        };
        // SAFETY: `rest` is a valid initialized buffer of `rest.len()` bytes.
        let n = unsafe { libc::write(fd, rest.as_ptr().cast(), rest.len()) };
        if n >= 0 {
            rest = &rest[n as usize..];
            continue;
        }
        let err = std::io::Error::last_os_error();
        match err.kind() {
            std::io::ErrorKind::Interrupted => {}
            std::io::ErrorKind::WouldBlock => {
                let mut poll = libc::pollfd {
                    fd,
                    events: libc::POLLOUT,
                    revents: 0,
                };
                let ms = left.as_millis().clamp(1, i32::MAX as u128) as libc::c_int;
                // SAFETY: one valid pollfd.
                if unsafe { libc::poll(&mut poll, 1, ms) } < 0
                    && std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted
                {
                    return Err(os_err());
                }
            }
            _ => return Err(GitError::Spawn(err.to_string())),
        }
    }
    Ok(stdin)
}

/// Receive both pipes on this thread, retaining at most the declared ceilings.
/// Nonblocking descriptors and a deadline also cover descendants holding pipes.
fn capped_output(
    guard: &mut ChildGuard,
    started: Instant,
    ends: Deadline,
    stdout_limit: usize,
    budget: &ReadBudget,
) -> Result<Output, GitError> {
    use std::sync::atomic::Ordering;
    let child = guard.child.as_mut().expect("held until reaped");
    let mut pipes = [
        child.stdout.take().map(|p| File::from(OwnedFd::from(p))),
        child.stderr.take().map(|p| File::from(OwnedFd::from(p))),
    ];
    for pipe in pipes.iter().flatten() {
        // SAFETY: valid pipe descriptor, nonblocking only affects this reader.
        if unsafe { libc::fcntl(pipe.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK) } < 0 {
            return Err(GitError::Spawn(std::io::Error::last_os_error().to_string()));
        }
    }
    let deadline = ends.instant();
    let mut buffers = [Vec::new(), Vec::new()];
    let limits = [stdout_limit, 64 * 1024];
    let names = ["git_stdout", "git_stderr"];
    while pipes.iter().any(Option::is_some) {
        if let Some(name) = *budget.exceeded.lock().expect("budget mutex") {
            return Err(GitError::OutputLimit(name));
        }
        if Instant::now() >= deadline {
            return Err(GitError::Deadline);
        }
        let mut polls = pipes.each_ref().map(|p| libc::pollfd {
            fd: p.as_ref().map_or(-1, AsRawFd::as_raw_fd),
            events: libc::POLLIN,
            revents: 0,
        });
        let ms = deadline
            .saturating_duration_since(Instant::now())
            .as_millis()
            .clamp(1, 50) as i32;
        // SAFETY: two valid pollfd slots; negative fds are ignored.
        if unsafe { libc::poll(polls.as_mut_ptr(), 2, ms) } < 0 {
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(GitError::Spawn(std::io::Error::last_os_error().to_string()));
        }
        for i in 0..2 {
            let Some(pipe) = &mut pipes[i] else { continue };
            if polls[i].revents == 0 {
                continue;
            }
            let mut chunk = [0u8; 8192];
            let room = (limits[i] - buffers[i].len() + 1)
                .min(
                    budget
                        .limit
                        .saturating_sub(budget.received.load(Ordering::Relaxed))
                        + 1,
                )
                .min(chunk.len());
            match pipe.read(&mut chunk[..room]) {
                Ok(0) => pipes[i] = None,
                Ok(n) => {
                    let total = budget.received.fetch_add(n, Ordering::Relaxed) + n;
                    let boundary = if total > budget.limit {
                        Some("git_received")
                    } else if buffers[i].len() + n > limits[i] {
                        Some(names[i])
                    } else {
                        None
                    };
                    if let Some(name) = boundary {
                        *budget.exceeded.lock().expect("budget mutex") = Some(name);
                        return Err(GitError::OutputLimit(name));
                    }
                    buffers[i].extend_from_slice(&chunk[..n]);
                }
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                    ) => {}
                Err(e) => return Err(GitError::Spawn(e.to_string())),
            }
        }
    }
    let status = guard.wait_until(deadline)?;
    let [stdout, stderr] = buffers;
    Ok(Output {
        code: status.code(),
        stdout,
        stderr,
        millis: started.elapsed().as_millis() as u64,
    })
}

/// Owns a git child until it is reaped. Dropping it kills the child's whole
/// process group first, so no exit path, a panic included, leaves git running.
struct ChildGuard {
    child: Option<Child>,
}

impl ChildGuard {
    fn pid(&self) -> u32 {
        self.child.as_ref().expect("held until reaped").id()
    }

    fn kill_group(&self) {
        // SAFETY: the child leads its own group and is not yet reaped, so the
        // group id cannot have been reused.
        unsafe { libc::killpg(self.pid() as libc::pid_t, libc::SIGKILL) };
    }

    fn wait_until(&mut self, deadline: Instant) -> Result<ExitStatus, GitError> {
        let pid = self.pid() as libc::pid_t;
        // SAFETY: pidfd_open takes a pid and flags and returns a descriptor.
        let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) } as RawFd;
        if raw >= 0 {
            // SAFETY: a fresh descriptor from pidfd_open.
            let pidfd = unsafe { OwnedFd::from_raw_fd(raw) };
            loop {
                let now = Instant::now();
                if now >= deadline {
                    return Err(self.expire());
                }
                let ms = (deadline - now).as_millis().clamp(1, i32::MAX as u128) as libc::c_int;
                let mut poll = libc::pollfd {
                    fd: pidfd.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                };
                // SAFETY: one valid pollfd.
                let ready = unsafe { libc::poll(&mut poll, 1, ms) };
                if ready > 0 {
                    break;
                }
                if ready < 0 && std::io::Error::last_os_error().raw_os_error() != Some(libc::EINTR)
                {
                    return Err(GitError::Spawn(format!(
                        "waiting for git: {}",
                        std::io::Error::last_os_error()
                    )));
                }
            }
        } else {
            loop {
                // WNOWAIT leaves the child unreaped, so its group id stays ours.
                // SAFETY: a zeroed siginfo is a valid out-parameter.
                let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
                // SAFETY: valid pid and out-parameter.
                let r = unsafe {
                    libc::waitid(
                        libc::P_PID,
                        pid as libc::id_t,
                        &mut info,
                        libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
                    )
                };
                // SAFETY: si_pid is set by waitid when a child changed state.
                if r == 0 && unsafe { info.si_pid() } == pid {
                    break;
                }
                if Instant::now() >= deadline {
                    return Err(self.expire());
                }
                std::thread::sleep(Duration::from_millis(1));
            }
        }
        // Git has exited but is unreaped: sweep anything it left in its group.
        self.kill_group();
        let mut child = self.child.take().expect("held until reaped");
        child
            .wait()
            .map_err(|e| GitError::Spawn(format!("reaping git: {e}")))
    }

    fn expire(&mut self) -> GitError {
        self.kill_group();
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        GitError::Deadline
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if self.child.is_some() {
            self.kill_group();
            if let Some(mut child) = self.child.take() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }
}

/// Debug-build test hook inside the child, between `setpgid` and arming the
/// parent-death signal: `RIVET_TEST_PAUSE=git_child:<dir>`. It writes the
/// child's PID to `<dir>/git_child.reached` and waits for `git_child.go`.
struct ChildPause {
    reached: CString,
    go: CString,
}

impl ChildPause {
    fn from_env() -> Option<ChildPause> {
        if !cfg!(debug_assertions) {
            return None;
        }
        let spec = std::env::var("RIVET_TEST_PAUSE").ok()?;
        let (point, dir) = spec.split_once(':')?;
        if point != "git_child" {
            return None;
        }
        Some(ChildPause {
            reached: CString::new(format!("{dir}/git_child.reached")).ok()?,
            go: CString::new(format!("{dir}/git_child.go")).ok()?,
        })
    }

    /// Async-signal-safe: open, write, close, access and nanosleep only.
    fn wait(&self) {
        // SAFETY: only async-signal-safe calls on prepared, NUL-terminated paths.
        unsafe {
            let fd = libc::open(
                self.reached.as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_TRUNC | libc::O_CLOEXEC,
                0o644,
            );
            if fd >= 0 {
                let mut digits = [0u8; 20];
                let mut n = libc::getpid() as u64;
                let mut at = digits.len();
                loop {
                    at -= 1;
                    digits[at] = b'0' + (n % 10) as u8;
                    n /= 10;
                    if n == 0 {
                        break;
                    }
                }
                libc::write(fd, digits[at..].as_ptr().cast(), digits.len() - at);
                libc::close(fd);
            }
            let pause = libc::timespec {
                tv_sec: 0,
                tv_nsec: 5_000_000,
            };
            for _ in 0..12_000 {
                if libc::access(self.go.as_ptr(), libc::F_OK) == 0 {
                    return;
                }
                libc::nanosleep(&pause, std::ptr::null_mut());
            }
        }
    }
}

/// The launch helper: `markrivet __git-exec <fd> <git> <args...>`. The process
/// group, parent-death signal and working directory were set up in the fork;
/// the helper waits for Rivet's go byte on `fd`, then execs git, which keeps
/// this PID and the parent-death setting. No go byte (Rivet died or refused)
/// means git never runs.
pub fn exec_helper(args: &[OsString]) -> ! {
    // SAFETY: _exit ends the process without running destructors.
    let fail = |code| -> ! { unsafe { libc::_exit(code) } };
    let Some(fd) = args
        .first()
        .and_then(|a| a.to_str())
        .and_then(|s| s.parse::<RawFd>().ok())
    else {
        fail(125)
    };
    let Some(program) = args.get(1) else {
        fail(125)
    };
    let mut byte = [0u8; 1];
    loop {
        // SAFETY: a one-byte buffer on the inherited pipe descriptor.
        let n = unsafe { libc::read(fd, byte.as_mut_ptr().cast(), 1) };
        if n == 1 {
            break;
        }
        if n < 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
            continue;
        }
        fail(125);
    }
    // SAFETY: the descriptor was inherited for this read and is not reused.
    unsafe { libc::close(fd) };
    let error = Command::new(program).args(&args[2..]).exec();
    eprintln!("markrivet: cannot exec {program:?}: {error}");
    fail(126)
}

// ---------------------------------------------------------------------------
// Snapshot, base state and eligibility.

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Base {
    KnownClean,
    KnownDirty,
    Absent,
    Unknown,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OnDirty {
    #[default]
    Skip,
    Adopt,
    Refuse,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Entry {
    pub oid: String,
    pub mode: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct IndexEntry {
    pub oid: String,
    pub mode: String,
    pub stage: u8,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub assume_unchanged: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub skip_worktree: bool,
}

impl IndexEntry {
    pub fn is(&self, entry: &Entry) -> bool {
        self.stage == 0 && self.oid == entry.oid && self.mode == entry.mode
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Ineligible {
    pub reason: &'static str,
    pub detail: String,
}

fn ineligible(reason: &'static str, detail: impl Into<String>) -> Ineligible {
    Ineligible {
        reason,
        detail: detail.into(),
    }
}

/// What the working tree holds at a path, read through the project's
/// confinement.
#[derive(Debug, Clone)]
pub enum FileState {
    Missing,
    Present { bytes: Vec<u8>, mode: u32 },
    Failed(String),
}

impl FileState {
    pub fn read(project: &Project, path: &str) -> FileState {
        match project.read_raw(path) {
            Ok(None) => FileState::Missing,
            Ok(Some((bytes, mode))) => FileState::Present { bytes, mode },
            Err(e) => FileState::Failed(e.message),
        }
    }

    /// The mode git records for this file: executable when the owner may run it.
    pub fn git_mode(&self) -> Option<&'static str> {
        match self {
            FileState::Present { mode, .. } if mode & 0o100 != 0 => Some("100755"),
            FileState::Present { .. } => Some("100644"),
            _ => None,
        }
    }
}

/// Repository-wide facts, read once per report or write.
#[derive(Debug, Clone)]
pub struct Repo {
    pub git: Git,
    pub version: Option<String>,
    pub too_old: bool,
    /// The first inspection that failed or timed out.
    pub error: Option<String>,
    /// Why the layout is unsupported, if it is.
    pub layout: Option<String>,
    /// The pinned HEAD commit.
    pub head: Option<String>,
    /// The symbolic ref HEAD names, such as `refs/heads/main`.
    pub branch: Option<String>,
    pub busy: Option<String>,
    pub file_mode: bool,
    pub conversion: Option<String>,
    pub identity: Option<String>,
    pub read_at: u64,
}

/// Git's view of one path at the pinned HEAD and in the index.
#[derive(Debug, Clone, Default)]
pub struct PathGit {
    pub head: Option<Entry>,
    pub head_blob: Option<Vec<u8>>,
    pub index: Vec<IndexEntry>,
    /// Conversion attributes set for the path, as `name=value`.
    pub attributes: Vec<String>,
    pub error: Option<String>,
}

/// Configuration as `git config --list -z` gives it: `None` for a key set
/// with no value, which git reads as true.
type Config = BTreeMap<String, Option<String>>;

/// A boolean as git itself parses it (`git config --type=bool`), so no
/// spelling git accepts is read differently. `Ok(None)` when the key is unset.
fn git_bool(git: &Git, args: &[&str]) -> Result<Option<Vec<String>>, String> {
    let mut call = vec!["config", "--type=bool"];
    call.extend_from_slice(args);
    match git.inspect(&call, None) {
        Ok(out) if out.ok() => Ok(Some(lines(&out.stdout))),
        Ok(out) if out.code == Some(1) => Ok(None),
        Ok(out) => Err(out.failure("git config --type=bool")),
        Err(e) => Err(e.to_string()),
    }
}

fn lines(bytes: &[u8]) -> Vec<String> {
    String::from_utf8_lossy(bytes)
        .lines()
        .map(str::to_string)
        .collect()
}

fn nul_fields(bytes: &[u8]) -> impl Iterator<Item = &[u8]> {
    bytes.split(|b| *b == 0).filter(|f| !f.is_empty())
}

/// Decode listing records without inventing lossy document keys. Both listing
/// formats put ASCII metadata before a tab and the native path after it.
fn listing_text<'a>(record: &'a [u8], native_name: &mut impl FnMut(&[u8])) -> Option<&'a str> {
    match std::str::from_utf8(record) {
        Ok(text) => Some(text),
        Err(_) => {
            if let Some(path) = record.splitn(2, |b| *b == b'\t').nth(1)
                && std::str::from_utf8(path).is_err()
            {
                native_name(path);
            }
            None
        }
    }
}

/// `git ls-tree -r -z` records as (path, entry). Documents have UTF-8 paths;
/// a record that is not UTF-8 is skipped, since a lossy decode could merge
/// two different git paths into one.
fn parse_ls_tree(stdout: &[u8]) -> Result<Vec<(String, Entry)>, String> {
    parse_ls_tree_observed(stdout, &mut |_| {})
}

pub(crate) fn parse_ls_tree_observed(
    stdout: &[u8],
    native_name: &mut impl FnMut(&[u8]),
) -> Result<Vec<(String, Entry)>, String> {
    let mut out = vec![];
    for record in nul_fields(stdout) {
        let Some(text) = listing_text(record, native_name) else {
            continue;
        };
        let Some((meta, path)) = text.split_once('\t') else {
            return Err(format!("unreadable ls-tree record {text:?}"));
        };
        let mut meta = meta.split(' ');
        let (Some(mode), Some(_kind), Some(oid)) = (meta.next(), meta.next(), meta.next()) else {
            return Err(format!("unreadable ls-tree record {text:?}"));
        };
        out.push((
            path.to_string(),
            Entry {
                oid: oid.into(),
                mode: mode.into(),
            },
        ));
    }
    Ok(out)
}

/// `git ls-files -s -v -z` records as (path, entry).
fn parse_ls_files(stdout: &[u8]) -> Result<Vec<(String, IndexEntry)>, String> {
    parse_ls_files_observed(stdout, &mut |_| {})
}

fn parse_ls_files_observed(
    stdout: &[u8],
    native_name: &mut impl FnMut(&[u8]),
) -> Result<Vec<(String, IndexEntry)>, String> {
    let mut out = vec![];
    for record in nul_fields(stdout) {
        let Some(text) = listing_text(record, native_name) else {
            continue;
        };
        let parsed = text.split_once('\t').and_then(|(meta, path)| {
            let mut meta = meta.split(' ');
            let tag = meta.next()?.chars().next()?;
            let mode = meta.next()?;
            let oid = meta.next()?;
            let stage = meta.next()?.parse().ok()?;
            Some((tag, mode, oid, stage, path))
        });
        let Some((tag, mode, oid, stage, path)) = parsed else {
            return Err(format!("unreadable ls-files record {text:?}"));
        };
        out.push((
            path.to_string(),
            IndexEntry {
                oid: oid.into(),
                mode: mode.into(),
                stage,
                assume_unchanged: tag.is_ascii_lowercase(),
                skip_worktree: tag.eq_ignore_ascii_case(&'s'),
            },
        ));
    }
    Ok(out)
}

pub(crate) fn tree_entry(git: &Git, rev: &str, path: &str) -> Result<Option<Entry>, String> {
    let out = git
        .inspect(&["ls-tree", "-z", "--full-tree", rev, "--", path], None)
        .map_err(|e| e.to_string())?;
    if !out.ok() {
        return Err(out.failure("git ls-tree"));
    }
    Ok(parse_ls_tree(&out.stdout)?
        .into_iter()
        .find(|(p, _)| p == path)
        .map(|(_, e)| e))
}

/// One path's entry at `rev` and its index entries, read directly rather
/// than through a [`Repo`] snapshot.
pub fn path_entries(
    git: &Git,
    rev: &str,
    path: &str,
) -> Result<(Option<Entry>, Vec<IndexEntry>), String> {
    let head = tree_entry(git, rev, path)?;
    let out = git
        .inspect(&["ls-files", "-s", "-v", "-z", "--", path], None)
        .map_err(|e| e.to_string())?;
    if !out.ok() {
        return Err(out.failure("git ls-files"));
    }
    let index = parse_ls_files(&out.stdout)?
        .into_iter()
        .filter(|(p, _)| p == path)
        .map(|(_, e)| e)
        .collect();
    Ok((head, index))
}

/// The branch HEAD names (`None` when detached) and the commit it points
/// at (`None` when unborn).
pub fn head_state(git: &Git) -> Result<(Option<String>, Option<String>), String> {
    let branch = match git.inspect(&["symbolic-ref", "-q", "HEAD"], None) {
        Ok(out) if out.ok() => lines(&out.stdout).into_iter().next(),
        Ok(out) if out.code == Some(1) => None,
        Ok(out) => return Err(out.failure("git symbolic-ref HEAD")),
        Err(e) => return Err(e.to_string()),
    };
    let head = match git.inspect(&["rev-parse", "-q", "--verify", "HEAD^{commit}"], None) {
        Ok(out) if out.ok() => lines(&out.stdout).into_iter().next(),
        Ok(out) if out.code == Some(1) => None,
        Ok(out) => return Err(out.failure("git rev-parse HEAD")),
        Err(e) => return Err(e.to_string()),
    };
    Ok((branch, head))
}

impl Repo {
    pub fn inspect(project: &Project) -> Repo {
        Repo::inspect_with(Git::new(project), project)
    }

    pub fn inspect_with(git: Git, project: &Project) -> Repo {
        let mut repo = Repo {
            git,
            version: None,
            too_old: false,
            error: None,
            layout: None,
            head: None,
            branch: None,
            busy: None,
            file_mode: true,
            conversion: None,
            identity: None,
            read_at: now_millis(),
        };
        let fail = |repo: &mut Repo, message: String| {
            repo.error.get_or_insert(message);
        };
        match repo.git.inspect(&["version"], None) {
            Ok(out) if out.ok() => {
                let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
                let version = text
                    .strip_prefix("git version ")
                    .unwrap_or(&text)
                    .to_string();
                let mut parts = version.split(|c: char| !c.is_ascii_digit());
                let major = parts.next().and_then(|p| p.parse::<u32>().ok());
                let minor = parts.next().and_then(|p| p.parse::<u32>().ok());
                match (major, minor) {
                    (Some(a), Some(b)) => repo.too_old = (a, b) < MIN_VERSION,
                    _ => fail(&mut repo, format!("unreadable git version {text:?}")),
                }
                repo.version = Some(version);
            }
            Ok(out) => fail(&mut repo, out.failure("git version")),
            Err(e) => fail(&mut repo, e.to_string()),
        }
        let config = match repo.git.inspect(&["config", "--list", "-z"], None) {
            Ok(out) if out.ok() => {
                let mut map = Config::new();
                for field in nul_fields(&out.stdout) {
                    let text = String::from_utf8_lossy(field);
                    match text.split_once('\n') {
                        Some((key, value)) => map.insert(key.into(), Some(value.into())),
                        None => map.insert(text.into_owned(), None),
                    };
                }
                map
            }
            Ok(out) => {
                fail(&mut repo, out.failure("git config --list"));
                Config::new()
            }
            Err(e) => {
                fail(&mut repo, e.to_string());
                Config::new()
            }
        };
        if config.contains_key("core.filemode") {
            match git_bool(&repo.git, &["--get", "core.filemode"]) {
                Ok(Some(v)) => repo.file_mode = v.first().map(String::as_str) != Some("false"),
                Ok(None) => {}
                Err(e) => fail(&mut repo, e),
            }
        }
        let mut conversions = vec![];
        if let Some(v) = config.get("core.autocrlf") {
            // `input` is not a boolean; anything but a clean false converts.
            let off = git_bool(&repo.git, &["--get", "core.autocrlf"])
                .is_ok_and(|v| v.is_some_and(|v| v.first().map(String::as_str) == Some("false")));
            if !off {
                conversions.push(format!("core.autocrlf={}", v.as_deref().unwrap_or("true")));
            }
        }
        // check-attr prints a driver literally named `unset` or `unspecified`
        // exactly as it prints no driver, so such a driver blocks every path.
        if let Some(k) = config
            .keys()
            .find(|k| k.starts_with("filter.unset.") || k.starts_with("filter.unspecified."))
        {
            conversions.push(format!("{k} is configured"));
        }
        if let Some(v) = config.get("core.eol") {
            conversions.push(format!("core.eol={}", v.as_deref().unwrap_or("")));
        }
        if !conversions.is_empty() {
            repo.conversion = Some(conversions.join(", "));
        }
        let mut missing = vec![];
        for key in ["user.name", "user.email"] {
            if config
                .get(key)
                .is_none_or(|v| v.as_deref().is_none_or(|v| v.trim().is_empty()))
            {
                missing.push(key);
            }
        }
        if !missing.is_empty() {
            repo.identity = Some(format!("{} not set", missing.join(" and ")));
        }
        repo.layout = layout(&repo, project, &config);
        if repo.layout.is_none() {
            match repo
                .git
                .inspect(&["rev-parse", "-q", "--verify", "HEAD^{commit}"], None)
            {
                Ok(out) if out.ok() => repo.head = lines(&out.stdout).into_iter().next(),
                Ok(out) if out.code == Some(1) => {}
                Ok(out) => fail(&mut repo, out.failure("git rev-parse HEAD")),
                Err(e) => fail(&mut repo, e.to_string()),
            }
            match repo.git.inspect(&["symbolic-ref", "-q", "HEAD"], None) {
                Ok(out) if out.ok() => repo.branch = lines(&out.stdout).into_iter().next(),
                Ok(out) if out.code == Some(1) => {}
                Ok(out) => fail(&mut repo, out.failure("git symbolic-ref HEAD")),
                Err(e) => fail(&mut repo, e.to_string()),
            }
            repo.busy = busy_marker(project);
            if repo.busy.is_none() {
                match repo.git.inspect(&["ls-files", "-u", "-z"], None) {
                    Ok(out) if out.ok() => {
                        if !out.stdout.is_empty() {
                            repo.busy = Some("the index has unmerged entries".into());
                        }
                    }
                    Ok(out) => fail(&mut repo, out.failure("git ls-files -u")),
                    Err(e) => fail(&mut repo, e.to_string()),
                }
            }
        }
        repo
    }

    /// The reason a commit could not be made at all, whatever the path.
    pub fn blocked(&self) -> Option<Ineligible> {
        if self.too_old {
            return Some(ineligible(
                "git_too_old",
                format!(
                    "git {} is older than {}.{}",
                    self.version.as_deref().unwrap_or("?"),
                    MIN_VERSION.0,
                    MIN_VERSION.1
                ),
            ));
        }
        if let Some(e) = &self.error {
            return Some(ineligible("git_error", e.clone()));
        }
        if let Some(l) = &self.layout {
            return Some(ineligible("unsupported_layout", l.clone()));
        }
        if self.head.is_none() {
            return Some(ineligible("no_branch", "HEAD is unborn"));
        }
        if self.branch.is_none() {
            return Some(ineligible("no_branch", "HEAD is detached"));
        }
        if let Some(b) = &self.busy {
            return Some(ineligible("repo_busy", b.clone()));
        }
        None
    }

    /// Git's view of `paths` (or of every path, for `None`) at the pinned
    /// HEAD and in the index, plus conversion attributes and HEAD blobs for
    /// the paths in `detail`.
    pub fn paths(
        &self,
        scope: Option<&[String]>,
        detail: impl Fn(&str) -> bool,
    ) -> Result<BTreeMap<String, PathGit>, String> {
        self.paths_observed(scope, detail, |_| {})
    }

    /// Read the same git facts while reporting paths that cannot become UTF-8
    /// document keys. Status uses these observations for enumeration coverage;
    /// preflight retains its existing explicit-path behavior through `paths`.
    pub(crate) fn paths_observed(
        &self,
        scope: Option<&[String]>,
        detail: impl Fn(&str) -> bool,
        mut native_name: impl FnMut(&[u8]),
    ) -> Result<BTreeMap<String, PathGit>, String> {
        let mut found: BTreeMap<String, PathGit> = BTreeMap::new();
        if let Some(paths) = scope {
            for p in paths {
                found.entry(p.clone()).or_default();
            }
        }
        if let Some(head) = &self.head {
            let mut args: Vec<&str> = vec!["ls-tree", "-r", "-z", "--full-tree", head];
            if let Some(paths) = scope {
                args.push("--");
                args.extend(paths.iter().map(String::as_str));
            }
            let out = self.git.inspect(&args, None).map_err(|e| e.to_string())?;
            if !out.ok() {
                return Err(out.failure("git ls-tree"));
            }
            for (path, entry) in parse_ls_tree_observed(&out.stdout, &mut native_name)? {
                if scope.is_some_and(|s| !s.contains(&path)) {
                    continue;
                }
                found.entry(path).or_default().head = Some(entry);
            }
        }
        let mut args: Vec<&str> = vec!["ls-files", "-s", "-v", "-z"];
        if let Some(paths) = scope {
            args.push("--");
            args.extend(paths.iter().map(String::as_str));
        }
        let out = self.git.inspect(&args, None).map_err(|e| e.to_string())?;
        if !out.ok() {
            return Err(out.failure("git ls-files"));
        }
        for (path, entry) in parse_ls_files_observed(&out.stdout, &mut native_name)? {
            if scope.is_some_and(|s| !s.contains(&path)) {
                continue;
            }
            found.entry(path).or_default().index.push(entry);
        }
        let wanted: Vec<String> = found.keys().filter(|p| detail(p)).cloned().collect();
        // A failure here leaves the HEAD and index facts already read, and
        // marks the paths it could not finish.
        if let Err(e) = self.detail(&mut found, &wanted) {
            for p in &wanted {
                if let Some(entry) = found.get_mut(p) {
                    entry.error = Some(e.clone());
                }
            }
        }
        Ok(found)
    }

    /// Conversion attributes and HEAD blobs for `wanted`.
    fn detail(
        &self,
        found: &mut BTreeMap<String, PathGit>,
        wanted: &[String],
    ) -> Result<(), String> {
        if wanted.is_empty() {
            return Ok(());
        }
        let mut input = vec![];
        for p in wanted {
            input.extend_from_slice(p.as_bytes());
            input.push(0);
        }
        let mut args = vec!["check-attr", "--stdin", "-z"];
        args.extend(CONVERSION_ATTRIBUTES);
        let out = self
            .git
            .inspect(&args, Some(&input))
            .map_err(|e| e.to_string())?;
        if !out.ok() {
            return Err(out.failure("git check-attr"));
        }
        let fields: Vec<&[u8]> = out.stdout.split(|b| *b == 0).collect();
        for triple in fields.chunks(3) {
            if let [path, attr, info] = triple {
                let path = String::from_utf8_lossy(path);
                let info = String::from_utf8_lossy(info);
                // `unset` (`-text`, `-filter`) turns a conversion off.
                if info != "unspecified"
                    && info != "unset"
                    && let Some(entry) = found.get_mut(path.as_ref())
                {
                    entry
                        .attributes
                        .push(format!("{}={info}", String::from_utf8_lossy(attr)));
                }
            }
        }
        let oids: Vec<(String, String)> = wanted
            .iter()
            .filter_map(|p| {
                let head = found[p].head.as_ref()?;
                matches!(head.mode.as_str(), "100644" | "100755")
                    .then(|| (p.clone(), head.oid.clone()))
            })
            .collect();
        if oids.is_empty() {
            return Ok(());
        }
        let mut input = vec![];
        for (_, oid) in &oids {
            input.extend_from_slice(oid.as_bytes());
            input.push(b'\n');
        }
        let out = self
            .git
            .inspect(&["cat-file", "--batch"], Some(&input))
            .map_err(|e| e.to_string())?;
        if !out.ok() {
            return Err(out.failure("git cat-file"));
        }
        let mut rest = out.stdout.as_slice();
        for (path, oid) in oids {
            let header_end = rest
                .iter()
                .position(|b| *b == b'\n')
                .ok_or("truncated cat-file output")?;
            let header = String::from_utf8_lossy(&rest[..header_end]).to_string();
            let mut parts = header.split(' ');
            let (Some(got), Some("blob"), Some(size)) = (parts.next(), parts.next(), parts.next())
            else {
                return Err(format!("cat-file could not read {oid}: {header}"));
            };
            let size: usize = size.parse().map_err(|_| "unreadable cat-file size")?;
            if got != oid || rest.len() < header_end + 1 + size + 1 {
                return Err("truncated cat-file output".into());
            }
            let body = rest[header_end + 1..header_end + 1 + size].to_vec();
            rest = &rest[header_end + 1 + size + 1..];
            if let Some(entry) = found.get_mut(&path) {
                entry.head_blob = Some(body);
            }
        }
        Ok(())
    }

    /// Base state and the first ineligibility reason for one path. A
    /// creation compares against absence: no HEAD entry, no index entry.
    pub fn assess(
        &self,
        path: &PathGit,
        file: &FileState,
        creation: Creation,
    ) -> (Base, Option<Ineligible>) {
        let base = self.base(path, file, creation);
        (base, self.ineligible(path, file, creation))
    }

    fn base(&self, path: &PathGit, file: &FileState, creation: Creation) -> Base {
        if self.error.is_some()
            || self.layout.is_some()
            || self.head.is_none()
            || path.error.is_some()
            || matches!(file, FileState::Failed(_))
        {
            return Base::Unknown;
        }
        let Some(head) = &path.head else {
            return if creation.index_ok(&path.index)
                && match creation {
                    Creation::Fresh => matches!(file, FileState::Missing),
                    Creation::Untracked | Creation::Recovering(_) => {
                        matches!(file, FileState::Present { .. })
                    }
                    Creation::No => false,
                } {
                Base::Absent
            } else {
                Base::Unknown
            };
        };
        let index_clean = matches!(path.index.as_slice(), [only] if only.is(head));
        let file_clean = match file {
            FileState::Present { bytes, .. } => {
                path.head_blob.as_deref() == Some(bytes.as_slice())
                    && (!self.file_mode || file.git_mode() == Some(head.mode.as_str()))
            }
            _ => false,
        };
        if index_clean && file_clean {
            Base::KnownClean
        } else {
            Base::KnownDirty
        }
    }

    fn ineligible(
        &self,
        path: &PathGit,
        file: &FileState,
        creation: Creation,
    ) -> Option<Ineligible> {
        if !self.too_old {
            if let Some(e) = self.error.as_ref().or(path.error.as_ref()) {
                return Some(ineligible("git_error", e.clone()));
            }
            if let FileState::Failed(e) = file {
                return Some(ineligible("git_error", e.clone()));
            }
        }
        if let Some(blocked) = self.blocked() {
            return Some(blocked);
        }
        match &path.head {
            None if creation == Creation::No => {
                return Some(ineligible("untracked", "the path is not at HEAD"));
            }
            None if !creation.index_ok(&path.index) => {
                return Some(ineligible(
                    "staged",
                    "the index already holds an entry for a path absent from HEAD",
                ));
            }
            Some(head) if !matches!(path.index.as_slice(), [only] if only.is(head)) => {
                return Some(ineligible(
                    "staged",
                    "the index entry differs from the blob at HEAD",
                ));
            }
            _ => {}
        }
        if let Some(e) = path
            .index
            .iter()
            .find(|e| e.assume_unchanged || e.skip_worktree)
        {
            let flag = if e.skip_worktree {
                "skip-worktree"
            } else {
                "assume-unchanged"
            };
            return Some(ineligible("index_flags", format!("{flag} is set")));
        }
        let mut conversions = path.attributes.clone();
        conversions.extend(self.conversion.clone());
        if !conversions.is_empty() {
            return Some(ineligible("git_conversion", conversions.join(", ")));
        }
        if let Some(i) = &self.identity {
            return Some(ineligible("no_identity", i.clone()));
        }
        None
    }
}

fn layout(repo: &Repo, project: &Project, config: &Config) -> Option<String> {
    let outside = || Some("the project root is not in a git work tree".to_string());
    let bare = match repo
        .git
        .inspect(&["rev-parse", "--is-bare-repository"], None)
    {
        Ok(out) if out.ok() => lines(&out.stdout).first().map(String::as_str) == Some("true"),
        _ => return outside(),
    };
    if bare {
        return Some("the repository is bare".into());
    }
    match stat_at(project.root_fd(), &CString::new(".git").expect("static")) {
        Ok(st) if st.st_mode & libc::S_IFMT == libc::S_IFDIR => {}
        Ok(_) => return Some(".git is not a directory (a linked worktree or submodule)".into()),
        Err(_) => return Some("the project root is not the work tree's top level".into()),
    }
    let found = match repo.git.inspect(
        &[
            "rev-parse",
            "--git-dir",
            "--git-common-dir",
            "--show-toplevel",
        ],
        None,
    ) {
        Ok(out) if out.ok() => lines(&out.stdout),
        _ => return outside(),
    };
    let [git_dir, common_dir, top, ..] = found.as_slice() else {
        return outside();
    };
    if git_dir != common_dir {
        return Some("the project root is a linked worktree".into());
    }
    use std::os::unix::fs::MetadataExt;
    // SAFETY: a zeroed stat is a valid out-parameter.
    let mut root: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: held descriptor and owned out-parameter.
    let held = unsafe { libc::fstat(project.root_fd(), &mut root) } == 0;
    let same = std::fs::metadata(top)
        .is_ok_and(|m| held && m.dev() == root.st_dev && m.ino() == root.st_ino);
    if !same {
        return Some("the project root is not the work tree's top level".into());
    }
    if let Some(w) = config.get("core.worktree") {
        return Some(format!(
            "core.worktree is set ({})",
            w.as_deref().unwrap_or("")
        ));
    }
    if config.contains_key("extensions.partialclone") {
        return Some("partial clones are not supported".into());
    }
    if config
        .keys()
        .any(|k| k.starts_with("remote.") && k.ends_with(".promisor"))
    {
        // A value git cannot read as a boolean is treated as a promisor.
        let promisor = match git_bool(&repo.git, &["--get-regexp", r"^remote\..*\.promisor$"]) {
            Ok(Some(found)) => found.iter().any(|l| l.ends_with(" true")),
            Ok(None) => false,
            Err(_) => true,
        };
        if promisor {
            return Some("partial clones are not supported".into());
        }
    }
    None
}

fn open_dir_at(dir: RawFd, name: &CStr) -> std::io::Result<OwnedFd> {
    // SAFETY: held descriptor, NUL-terminated name; the result is owned here.
    let fd = unsafe {
        libc::openat(
            dir,
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: openat returned a fresh descriptor.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn stat_at(dir: RawFd, name: &CStr) -> std::io::Result<libc::stat> {
    // SAFETY: a zeroed stat is a valid out-parameter.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: valid descriptor, NUL-terminated name, owned out-parameter.
    if unsafe { libc::fstatat(dir, name.as_ptr(), &mut st, libc::AT_SYMLINK_NOFOLLOW) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(st)
}

fn busy_marker(project: &Project) -> Option<String> {
    let git = open_dir_at(project.root_fd(), &CString::new(".git").expect("static")).ok()?;
    BUSY.iter()
        .find(|name| stat_at(git.as_raw_fd(), &CString::new(**name).expect("static")).is_ok())
        .map(|name| format!(".git/{name} is present"))
}

/// Which preflight a path gets: an existing tracked file, a document being
/// created (absent from HEAD, the index and the tree), an untracked file in
/// the tree that `markrivet adopt` adds, or recovery of a creation whose file is
/// in the tree and whose index entry may already be the recorded candidate
/// (a legacy writer staged it before its commit).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Creation<'a> {
    No,
    Fresh,
    Untracked,
    Recovering(&'a Entry),
}

impl Creation<'_> {
    /// The index entries a creation accepts: none, or in recovery exactly
    /// the candidate at stage 0.
    fn index_ok(self, index: &[IndexEntry]) -> bool {
        match (self, index) {
            (Creation::No, _) => false,
            (_, []) => true,
            (Creation::Recovering(c), [only]) => only.is(c),
            _ => false,
        }
    }
}

impl From<bool> for Creation<'_> {
    fn from(fresh: bool) -> Self {
        if fresh { Creation::Fresh } else { Creation::No }
    }
}

/// Base state, eligibility and the snapshot they came from, for one path.
#[derive(Debug, Clone, Serialize)]
pub struct Preflight {
    pub base: Base,
    pub ineligible: Option<Ineligible>,
    pub head: Option<String>,
    pub branch: Option<String>,
    pub head_entry: Option<Entry>,
    pub index: Vec<IndexEntry>,
    #[serde(skip)]
    pub head_blob: Option<Vec<u8>>,
}

/// One snapshot for one canonical path: repository facts, the path's HEAD
/// and index entries, and the bytes and mode read from the tree.
pub fn preflight<'a>(
    repo: &Repo,
    path: &str,
    file: &FileState,
    creation: impl Into<Creation<'a>>,
) -> Preflight {
    let creation = creation.into();
    let scope = [path.to_string()];
    let snapshot = repo.paths(Some(&scope), |_| true);
    let entry = match snapshot {
        Ok(mut found) => found.remove(path).unwrap_or_default(),
        Err(e) => PathGit {
            error: Some(e),
            ..PathGit::default()
        },
    };
    let (base, mut ineligible) = repo.assess(&entry, file, creation);
    // Ignored creations stay ineligible even though plumbing can store any blob.
    if ineligible.is_none() && creation != Creation::No {
        match repo.git.ignored(path) {
            Ok(false) => {}
            Ok(true) => {
                ineligible = Some(self::ineligible("ignored", "the path is ignored by git"))
            }
            Err(e) => ineligible = Some(self::ineligible("git_error", e)),
        }
    }
    Preflight {
        base,
        ineligible,
        head: repo.head.clone(),
        branch: repo.branch.clone(),
        head_entry: entry.head,
        index: entry.index,
        head_blob: entry.head_blob,
    }
}

/// What a commit-mode write does with a preflight.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// `on_dirty: refuse` and the file is not known clean: write nothing.
    Refuse(Base),
    /// The write lands; no commit, for this reason.
    Ineligible(Ineligible),
    /// The write lands on a dirty file; the dirty policy decides.
    Dirty,
    Commit,
}

/// The dirty-first rule: `refuse` refuses `known_dirty` and `unknown` before
/// any ineligibility, so a refused write never depends on why git could not
/// commit it.
pub fn decide(base: Base, ineligible: Option<&Ineligible>, on_dirty: OnDirty) -> Decision {
    if on_dirty == OnDirty::Refuse && matches!(base, Base::KnownDirty | Base::Unknown) {
        return Decision::Refuse(base);
    }
    if let Some(i) = ineligible {
        return Decision::Ineligible(i.clone());
    }
    match base {
        Base::KnownDirty => Decision::Dirty,
        Base::KnownClean | Base::Absent => Decision::Commit,
        Base::Unknown => Decision::Ineligible(ineligible_unknown()),
    }
}

fn ineligible_unknown() -> Ineligible {
    ineligible("git_error", "the path's git state could not be established")
}

// ---------------------------------------------------------------------------
// Git lock files.
//
// Automatic clearing requires evidence that Rivet's own git child left it.
// Explicit unowned clearing also requires the operator to assert quiescence.
// `next-index-<pid>.lock` carries the PID of the git that took it, so once
// that recorded child is dead the file is removed automatically. `index.lock`, `HEAD.lock` and the recorded branch's
// lock carry no owner: they are reported with their identity and removed
// only by an explicit `clear_lock` that repeats it. Anything else is
// reported and left for the person.

/// The git child a pending record names. `pid` is `None` when git never ran.
#[derive(Debug, Clone)]
pub struct GitChild {
    pub pid: Option<u32>,
    pub spawned: SystemTime,
    pub identity: Option<ProcessIdentity>,
}

/// Linux process identity, stable across exec but distinct across PID reuse.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessIdentity {
    pub boot_id: String,
    /// Field 22 of /proc/<pid>/stat, in clock ticks since boot.
    pub start_time: u64,
}

impl ProcessIdentity {
    pub fn read(pid: u32) -> Option<Self> {
        crate::fs::fault("child_identity").ok()?;
        let boot_id = std::fs::read_to_string("/proc/sys/kernel/random/boot_id").ok()?;
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        // comm (field 2) can contain spaces and closing parentheses.
        let (_, fields) = stat.rsplit_once(") ")?;
        let start_time = fields.split_whitespace().nth(19)?.parse().ok()?;
        Some(Self {
            boot_id: boot_id.trim().to_owned(),
            start_time,
        })
    }
}

impl GitChild {
    /// Missing identity or unreadable proc data is not evidence of death.
    /// Legacy records retain the PID existence and zombie checks.
    pub fn dead(&self) -> bool {
        let Some(pid) = self.pid else {
            return true;
        };
        if let Some(recorded) = &self.identity {
            if let Ok(boot) = std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
                && recorded.boot_id != boot.trim()
            {
                return true;
            }
            if let Some(current) = ProcessIdentity::read(pid)
                && *recorded != current
            {
                return true;
            }
        }
        // SAFETY: signal 0 only checks for existence.
        if unsafe { libc::kill(pid as libc::pid_t, 0) } != 0 {
            return std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH);
        }
        // A zombie no longer holds anything.
        std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .ok()
            .and_then(|s| s.rsplit_once(") ").map(|(_, r)| r.starts_with('Z')))
            .unwrap_or(false)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LockKind {
    /// `.git/index.lock`
    Index,
    /// `.git/HEAD.lock`
    Head,
    /// The recorded branch's lock under `.git/refs/heads/`.
    Branch,
    /// `.git/next-index-<pid>.lock`
    NextIndex,
    /// Any other lock file in `.git/` or under `.git/refs/`.
    Other,
}

/// A `.lock` file in `.git/` or anywhere under `.git/refs/`.
#[derive(Debug, Clone, Serialize)]
pub struct LockFile {
    /// Strong fence identity is internal evidence; the clear protocol still
    /// repeats the listed inode/mtime identity.
    #[serde(skip)]
    pub(crate) fence: Option<Fence>,
    pub file: String,
    pub kind: LockKind,
    /// `<dev>:<ino>:<mtime ns>`, which a `clear_lock` request repeats.
    pub identity: String,
    /// Modification time, in milliseconds since the epoch.
    pub mtime: u64,
    /// Size in bytes, required by an unowned clear request.
    pub size: u64,
    pub regular: bool,
    /// The PID a `next-index-<pid>.lock` names.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    #[serde(skip)]
    pub modified: SystemTime,
}

impl LockFile {
    /// One of the three locks that stop a commit on the recorded branch.
    pub fn blocking(&self) -> bool {
        matches!(
            self.kind,
            LockKind::Index | LockKind::Head | LockKind::Branch
        )
    }
}

/// A lock file Rivet leaves in place: `repair_required: stale_git_lock`,
/// naming the file, with the evidence behind the decision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StaleLock {
    pub reason: &'static str,
    pub file: String,
    pub detail: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub identity: Option<String>,
    /// What the veto found, when it fired.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub veto: Option<String>,
}

fn stale(file: impl Into<String>, detail: impl Into<String>) -> StaleLock {
    StaleLock {
        reason: "stale_git_lock",
        file: file.into(),
        detail: detail.into(),
        identity: None,
        veto: None,
    }
}

impl StaleLock {
    fn of(lock: &LockFile, detail: impl Into<String>) -> StaleLock {
        StaleLock {
            identity: Some(lock.identity.clone()),
            ..stale(&lock.file, detail)
        }
    }
}

fn mtime(st: &libc::stat) -> SystemTime {
    UNIX_EPOCH + Duration::new(st.st_mtime as u64, st.st_mtime_nsec as u32)
}

/// Why a lock might still belong to a live process: some process holds it
/// open, or a git process runs with its working directory in the
/// repository. A lock git prepared and closed (`update-ref --stdin` before
/// `commit`) is caught by the second rule. An inaccessible descriptor directory
/// (EACCES on open) or a departed process (ENOENT on open or iteration) is
/// skipped as a best-effort visibility boundary. Root lookup, global enumeration
/// and other descriptor enumeration errors veto removal.
fn in_use(project: &Project, wanted: &[(u64, u64)]) -> Option<(usize, String)> {
    use std::os::unix::fs::MetadataExt;
    let inspect = || -> std::io::Result<Option<(usize, String)>> {
        crate::fs::fault("lock_proc_self_fd")?;
        let root = std::fs::read_link(format!("/proc/self/fd/{}", project.root_fd()))?;
        crate::fs::fault("lock_proc_list")?;
        let procs = std::fs::read_dir("/proc")?;
        let departed = |error: &std::io::Error| error.raw_os_error() == Some(libc::ENOENT);
        'processes: for proc_entry in procs {
            crate::fs::fault("lock_proc_entry")?;
            let proc_entry = crate::fs::fault_errno("lock_proc_entry").and(proc_entry)?;
            let Some(pid) = proc_entry
                .file_name()
                .to_str()
                .and_then(|n| n.parse::<u32>().ok())
            else {
                continue;
            };
            let dir = proc_entry.path();
            crate::fs::fault("lock_proc_fds")?;
            let fds = match crate::fs::fault_errno("lock_proc_fds")
                .and_then(|()| std::fs::read_dir(dir.join("fd")))
            {
                Ok(fds) => fds,
                Err(error) if departed(&error) || error.raw_os_error() == Some(libc::EACCES) => {
                    continue;
                }
                Err(error) => return Err(error),
            };
            for fd in fds {
                crate::fs::fault("lock_proc_fd_entry")?;
                let fd = match crate::fs::fault_errno("lock_proc_fd_entry").and(fd) {
                    Ok(fd) => fd,
                    Err(error) if departed(&error) => continue 'processes,
                    Err(error) => return Err(error),
                };
                if let Ok(meta) = std::fs::metadata(fd.path())
                    && let Some(i) = wanted.iter().position(|k| *k == (meta.dev(), meta.ino()))
                {
                    return Ok(Some((i, format!("held open by process {pid}"))));
                }
            }
            let is_git = std::fs::read_link(dir.join("exe")).is_ok_and(|exe| {
                exe.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n == "git" || n.starts_with("git-"))
            });
            if is_git && targets(&dir, &root) {
                return Ok(Some((
                    0,
                    format!("git process {pid} is running in this repository"),
                )));
            }
        }
        Ok(None)
    };
    inspect()
        .unwrap_or_else(|error| Some((0, format!("cannot inspect /proc for lock use: {error}"))))
}

/// Whether the process at `/proc/<pid>` works in `root`: its working
/// directory, a path argument (`--git-dir=`, `-C`, ...), or `GIT_DIR`,
/// `GIT_WORK_TREE` or `GIT_COMMON_DIR` resolves inside it.
fn targets(dir: &std::path::Path, root: &std::path::Path) -> bool {
    let Ok(cwd) = std::fs::read_link(dir.join("cwd")) else {
        return false;
    };
    if cwd.starts_with(root) {
        return true;
    }
    let inside = |value: &[u8]| {
        let value = std::path::Path::new(OsStr::from_bytes(value));
        std::fs::canonicalize(cwd.join(value)).is_ok_and(|p| p.starts_with(root))
    };
    let args = std::fs::read(dir.join("cmdline")).unwrap_or_default();
    let env = std::fs::read(dir.join("environ")).unwrap_or_default();
    // Each argument whole, and an option's value after its first `=`, so a
    // path that itself contains `=` is still read in full.
    args.split(|b| *b == 0).filter(|a| !a.is_empty()).any(|a| {
        inside(a)
            || (a.starts_with(b"--")
                && a.iter()
                    .position(|b| *b == b'=')
                    .is_some_and(|i| inside(&a[i + 1..])))
    }) || env.split(|b| *b == 0).any(|e| {
        [&b"GIT_DIR="[..], b"GIT_WORK_TREE=", b"GIT_COMMON_DIR="]
            .iter()
            .any(|k| e.strip_prefix(*k).is_some_and(inside))
    })
}

/// The shown path of the recorded branch's lock, when the branch name is a
/// plain one.
fn branch_lock(branch: &str) -> Option<String> {
    let short = branch.strip_prefix("refs/heads/").unwrap_or(branch);
    let plain = !short.is_empty()
        && !short
            .split('/')
            .any(|p| p.is_empty() || p == "." || p == ".." || p.contains('\0'));
    plain.then(|| format!(".git/refs/heads/{short}.lock"))
}

/// Every `.lock` file in `.git/` (no descent) and anywhere under
/// `.git/refs/`, reached from the held root one `O_NOFOLLOW` directory at a
/// time. A symlink under `.git/refs/` refuses the whole listing.
pub fn list_locks(project: &Project, branch: Option<&str>) -> Result<Vec<LockFile>, StaleLock> {
    let branch_lock = branch.and_then(branch_lock);
    let git = open_dir_at(project.root_fd(), &CString::new(".git").expect("static"))
        .map_err(|e| stale(".git", format!("cannot open .git as a directory: {e}")))?;
    let mut dirs: Vec<OwnedFd> = vec![git];
    let mut found = vec![];
    // Depth-first over `.git` itself (no descent) and the whole `refs` tree.
    let mut stack: Vec<(usize, String, bool)> = vec![(0, ".git".into(), false)];
    while let Some((dir, shown_dir, descend)) = stack.pop() {
        let entries = std::fs::read_dir(format!("/proc/self/fd/{}", dirs[dir].as_raw_fd()))
            .map_err(|e| stale(&shown_dir, format!("cannot list: {e}")))?;
        for entry in entries {
            let entry = entry.map_err(|e| stale(&shown_dir, format!("cannot list: {e}")))?;
            let name = entry.file_name();
            let bytes = name.as_bytes();
            let shown = format!("{shown_dir}/{}", name.to_string_lossy());
            let cname = CString::new(bytes).expect("directory entries hold no NUL");
            let kind = entry
                .file_type()
                .map_err(|e| stale(&shown, e.to_string()))?;
            if kind.is_dir() && (descend || (dir == 0 && bytes == b"refs")) {
                let fd = open_dir_at(dirs[dir].as_raw_fd(), &cname)
                    .map_err(|e| stale(&shown, format!("not a plain directory: {e}")))?;
                dirs.push(fd);
                stack.push((dirs.len() - 1, shown, true));
                continue;
            }
            if (dir == 0 && bytes == b"refs") || (descend && kind.is_symlink()) {
                return Err(stale(shown, "symlinks under .git/refs are not followed"));
            }
            if !bytes.ends_with(b".lock") {
                continue;
            }
            let st = match stat_at(dirs[dir].as_raw_fd(), &cname) {
                Ok(st) => st,
                // Its git finished between the listing and the stat.
                Err(e) if e.raw_os_error() == Some(libc::ENOENT) => continue,
                Err(e) => return Err(stale(&shown, e.to_string())),
            };
            let mut pid = None;
            let kind = if dir == 0 {
                match bytes {
                    b"index.lock" => LockKind::Index,
                    b"HEAD.lock" => LockKind::Head,
                    _ => {
                        pid = bytes
                            .strip_prefix(b"next-index-")
                            .and_then(|r| r.strip_suffix(b".lock"))
                            .and_then(|d| std::str::from_utf8(d).ok())
                            .and_then(|d| d.parse::<u32>().ok());
                        if pid.is_some() {
                            LockKind::NextIndex
                        } else {
                            LockKind::Other
                        }
                    }
                }
            } else if branch_lock.as_deref() == Some(shown.as_str()) {
                LockKind::Branch
            } else {
                LockKind::Other
            };
            let modified = mtime(&st);
            let fence = matches!(kind, LockKind::Index)
                .then(|| Fence::at(dirs[dir].as_raw_fd(), &cname).ok())
                .flatten()
                .filter(|f| f.dev == st.st_dev && f.ino == st.st_ino);
            found.push(LockFile {
                fence,
                file: shown,
                kind,
                identity: format!(
                    "{}:{}:{}",
                    st.st_dev,
                    st.st_ino,
                    st.st_mtime as i128 * 1_000_000_000 + st.st_mtime_nsec as i128
                ),
                mtime: modified
                    .duration_since(UNIX_EPOCH)
                    .map_or(0, |d| d.as_millis() as u64),
                size: st.st_size as u64,
                regular: st.st_mode & libc::S_IFMT == libc::S_IFREG,
                pid,
                modified,
            });
        }
    }
    found.sort_by(|a, b| a.file.cmp(&b.file));
    Ok(found)
}

/// Remove one listed lock, provided the file at its name is still the one
/// listed: the directory is reopened from the held root without following
/// symlinks and the identity re-read through it just before `unlinkat`.
fn remove_lock(project: &Project, lock: &LockFile) -> Result<(), StaleLock> {
    remove_lock_checked(project, lock, false)
}

fn remove_lock_checked(
    project: &Project,
    lock: &LockFile,
    check_size: bool,
) -> Result<(), StaleLock> {
    let parts: Vec<&str> = lock.file.split('/').collect();
    let (name, dirs) = parts.split_last().expect("a lock path");
    let mut dir = open_dir_at(project.root_fd(), &CString::new(".git").expect("static"))
        .map_err(|e| StaleLock::of(lock, format!("cannot open .git: {e}")))?;
    for part in &dirs[1..] {
        let part = CString::new(*part).expect("listed names hold no NUL");
        dir = open_dir_at(dir.as_raw_fd(), &part)
            .map_err(|e| StaleLock::of(lock, format!("not a plain directory: {e}")))?;
    }
    let cname = CString::new(*name).expect("listed names hold no NUL");
    if check_size {
        crate::fs::hook("clear_unowned_lock").map_err(|e| StaleLock::of(lock, e.message))?;
    }
    let st = stat_at(dir.as_raw_fd(), &cname)
        .map_err(|e| StaleLock::of(lock, format!("cannot stat: {e}")))?;
    let identity = format!(
        "{}:{}:{}",
        st.st_dev,
        st.st_ino,
        st.st_mtime as i128 * 1_000_000_000 + st.st_mtime_nsec as i128
    );
    if identity != lock.identity
        || st.st_mode & libc::S_IFMT != libc::S_IFREG
        || (check_size && st.st_size as u64 != lock.size)
    {
        return Err(StaleLock::of(
            lock,
            "the file was replaced since it was read",
        ));
    }
    // SAFETY: held directory descriptor and NUL-terminated name.
    if unsafe { libc::unlinkat(dir.as_raw_fd(), cname.as_ptr(), 0) } != 0 {
        return Err(StaleLock::of(
            lock,
            format!("cannot remove: {}", std::io::Error::last_os_error()),
        ));
    }
    // SAFETY: fsync on a held directory descriptor.
    unsafe { libc::fsync(dir.as_raw_fd()) };
    Ok(())
}

/// What the veto finds for one lock, if anything: a process holding it
/// open, or a git process working in this repository.
pub fn lock_veto(project: &Project, lock: &LockFile) -> Option<String> {
    veto(project, lock)
}

fn veto(project: &Project, lock: &LockFile) -> Option<String> {
    let key = lock
        .identity
        .split(':')
        .take(2)
        .map(|n| n.parse::<u64>().unwrap_or(u64::MAX))
        .collect::<Vec<_>>();
    in_use(project, &[(key[0], key[1])]).map(|(_, why)| why)
}

/// The automatic half of the lock rule: remove `next-index-<pid>.lock`
/// whose name carries the recorded child's PID, once that child is dead.
/// Returns what was removed, and each such file the veto kept, as
/// `stale_git_lock`. Call it holding the Rivet lock.
pub fn clear_next_index(
    project: &Project,
    branch: Option<&str>,
    child: &GitChild,
) -> (Vec<String>, Vec<StaleLock>) {
    let Some(pid) = child.pid else {
        return (vec![], vec![]);
    };
    if !child.dead() {
        return (vec![], vec![]);
    }
    let locks = match list_locks(project, branch) {
        Ok(locks) => locks,
        Err(e) => return (vec![], vec![e]),
    };
    let mut removed = vec![];
    let mut kept = vec![];
    for lock in locks
        .iter()
        .filter(|l| l.kind == LockKind::NextIndex && l.pid == Some(pid))
    {
        if !lock.regular {
            kept.push(StaleLock::of(lock, "not a regular file"));
            continue;
        }
        if let Some(why) = veto(project, lock) {
            kept.push(StaleLock {
                veto: Some(why.clone()),
                ..StaleLock::of(lock, why)
            });
            continue;
        }
        match remove_lock(project, lock) {
            Ok(()) => removed.push(lock.file.clone()),
            Err(e) => kept.push(e),
        }
    }
    (removed, kept)
}

/// The requested half of the lock rule: remove `file`, one of `index.lock`,
/// `HEAD.lock` or the recorded branch's lock, for the record whose child is
/// `child`. Refused unless the child ran and is dead, the file is regular,
/// no older than the child's spawn, still has the reported `identity`, and
/// the veto finds nothing. Call it holding the Rivet lock.
pub fn clear_lock(
    project: &Project,
    branch: Option<&str>,
    child: &GitChild,
    file: &str,
    identity: &str,
) -> Result<String, StaleLock> {
    let locks = list_locks(project, branch)?;
    let Some(lock) = locks.iter().find(|l| l.file == file) else {
        return Err(stale(file, "no such lock file"));
    };
    if !lock.blocking() {
        return Err(StaleLock::of(
            lock,
            "only index.lock, HEAD.lock and the recorded branch's lock are cleared on request",
        ));
    }
    let Some(pid) = child.pid else {
        return Err(StaleLock::of(lock, "no git child ran for this record"));
    };
    if !child.dead() {
        return Err(StaleLock::of(
            lock,
            format!("git child {pid} may still be running"),
        ));
    }
    if !lock.regular {
        return Err(StaleLock::of(lock, "not a regular file"));
    }
    if lock.modified < child.spawned {
        return Err(StaleLock::of(lock, "older than the recorded git child"));
    }
    if lock.identity != identity {
        return Err(StaleLock::of(
            lock,
            "the lock is not the file that was reported; read the new report",
        ));
    }
    if let Some(why) = veto(project, lock) {
        return Err(StaleLock {
            veto: Some(why.clone()),
            ..StaleLock::of(lock, why)
        });
    }
    remove_lock(project, lock)?;
    Ok(lock.file.clone())
}

/// Explicit removal without an owning pending record. The caller holds the
/// project lock, has checked pending ownership, and received the operator's
/// explicit assertion that this repository and its worktrees are quiescent.
pub fn clear_unowned_lock(
    project: &Project,
    branch: Option<&str>,
    file: &str,
    identity: &str,
    size: u64,
    may_own: impl Fn(&LockFile) -> bool,
) -> Result<String, Diagnostic> {
    let refused = |reason, detail: String| {
        Diagnostic::new(ErrorCode::ClearLockRefused, detail).with_lock_reason(reason)
    };
    let locks =
        list_locks(project, branch).map_err(|e| refused(LockClearReason::LockUnknown, e.detail))?;
    let lock = locks
        .iter()
        .find(|l| l.file == file)
        .ok_or_else(|| refused(LockClearReason::LockMissing, "no such reported lock".into()))?;
    if may_own(lock) {
        return Err(refused(
            LockClearReason::PendingRecords,
            "a pending record may own this lock or has a live git child; use its op".into(),
        ));
    }
    if !lock.blocking() || !lock.regular {
        return Err(refused(
            LockClearReason::UnsupportedLock,
            "expected a regular index.lock, HEAD.lock or current branch lock".into(),
        ));
    }
    if lock.identity != identity || lock.size != size {
        return Err(refused(
            LockClearReason::IdentityMismatch,
            "the lock differs from the report; read status again".into(),
        ));
    }
    if SystemTime::now()
        .duration_since(lock.modified)
        .map_or(true, |age| age < Duration::from_secs(30))
    {
        return Err(refused(
            LockClearReason::LockRecent,
            "unowned locks must be at least 30 seconds old".into(),
        ));
    }
    crate::fs::fault("unowned_lock_processes")
        .map_err(|e| refused(LockClearReason::ProcessUnknown, e.to_string()))?;
    if !crate::fs::initial_pid_namespace() {
        return Err(refused(
            LockClearReason::ProcessUnknown,
            "requires the host PID namespace for the process veto".into(),
        ));
    }
    if let Some(why) = veto(project, lock) {
        let reason = if why.starts_with("git process ") {
            LockClearReason::LiveGitProcess
        } else {
            LockClearReason::LockInUse
        };
        return Err(refused(reason, why));
    }
    remove_lock_checked(project, lock, true)
        .map_err(|e| refused(LockClearReason::IdentityOrRemoveFailed, e.detail))?;
    Ok(lock.file.clone())
}

/// A blocking lock present after a short bounded retry, for preflight's
/// `git_locked`: a lock its git is about to drop gets a moment to go.
pub fn lingering_lock(project: &Project, branch: Option<&str>) -> Option<String> {
    let mut last = None;
    for attempt in 0..5 {
        if attempt > 0 {
            std::thread::sleep(Duration::from_millis(20));
        }
        last = Some(match list_locks(project, branch) {
            Ok(locks) => locks.iter().find(|l| l.blocking())?.file.clone(),
            Err(e) => format!("{}: {}", e.file, e.detail),
        });
    }
    last
}

#[cfg(test)]
mod phase_tests {
    use super::*;

    #[test]
    fn a_deadline_is_spent_at_its_instant_not_after_it() {
        let start = Instant::now();
        let d = Deadline::after(start, Duration::from_millis(100));
        let end = start + Duration::from_millis(100);
        let before = end - Duration::from_nanos(1);
        assert_eq!(d.remaining_at(before), Some(Duration::from_nanos(1)));
        assert_eq!(d.remaining_at(end), None);
        assert_eq!(d.remaining_at(end + Duration::from_millis(1)), None);
        let sooner = Deadline::after(start, Duration::from_millis(10));
        assert_eq!(d.min(Some(sooner)), sooner);
        assert_eq!(sooner.min(Some(d)), sooner);
        assert_eq!(d.min(None), d);
    }

    /// A child that never reads its stdin: the write ends with the phase.
    #[test]
    fn a_write_to_a_non_reading_child_ends_with_its_budget() {
        let mut child = Command::new("sleep")
            .arg("10")
            .stdin(Stdio::piped())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let started = Instant::now();
        let ends = Deadline::after(started, Duration::from_millis(300));
        let result = write_by(stdin, &vec![b'#'; 4 << 20], ends);
        let took = started.elapsed();
        let _ = child.kill();
        let _ = child.wait();
        assert_eq!(result.err(), Some(GitError::Deadline));
        assert!(took >= Duration::from_millis(300), "{took:?}");
        assert!(took < Duration::from_secs(3), "{took:?}");
        // A spent budget writes nothing at all.
        let mut child = Command::new("cat")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let spent = Deadline::after(Instant::now() - Duration::from_secs(1), Duration::ZERO);
        assert_eq!(
            write_by(stdin, b"commit\n", spent).err(),
            Some(GitError::Deadline)
        );
        let out = child.wait_with_output().unwrap();
        assert!(out.stdout.is_empty());
    }
}
