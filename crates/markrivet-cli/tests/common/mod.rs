#![allow(dead_code)]
use serde_json::{Value, json};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
#[path = "../../../test-support/mod.rs"]
mod test_support;
use std::time::{Duration, Instant};
#[allow(unused_imports)] // Only the Git fixture binaries use this shared helper.
pub use test_support::git as git_command;
pub use test_support::scratch;

pub const WIDGETS: &str = "tracking/TRACKING_WIDGETS.md";

/// A fresh, exclusively allocated copy of the synthetic tracker example.
pub fn project(name: &str) -> PathBuf {
    let dir = scratch(name);
    copy(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/tracker"),
        &dir,
    );
    dir
}

fn copy(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), &target).unwrap();
        }
    }
}

/// Clear ambient hooks before a caller adds its explicit per-test overrides.
pub fn without_debug_hooks(command: &mut Command) -> &mut Command {
    for (key, _) in std::env::vars_os() {
        if key
            .to_str()
            .is_some_and(|key| key.starts_with("RIVET_TEST_"))
        {
            command.env_remove(key);
        }
    }
    command
}

/// Every CLI launch starts without ambient debug hooks; tests opt in explicitly.
pub fn raw_command() -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_markrivet"));
    without_debug_hooks(&mut c);
    c
}

pub fn command(root: &Path, cmd: &str, env: &[(&str, String)]) -> Command {
    let mut c = raw_command();
    c.arg("--root").arg(root).arg(cmd);
    for (k, v) in env {
        c.env(k, v);
    }
    c.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    c
}

pub fn spawn(root: &Path, cmd: &str, request: &Value, env: &[(&str, String)]) -> Child {
    let mut child = command(root, cmd, env).spawn().unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(request.to_string().as_bytes())
        .unwrap();
    child
}

pub fn finish(child: Child) -> (i32, Value) {
    let out = child.wait_with_output().unwrap();
    let body = serde_json::from_slice(&out.stdout).unwrap_or_else(|_| {
        json!({"raw": String::from_utf8_lossy(&out.stdout), "stderr": String::from_utf8_lossy(&out.stderr)})
    });
    (out.status.code().unwrap_or(-1), body)
}

pub fn rivet(root: &Path, cmd: &str, request: Value) -> (i32, Value) {
    finish(spawn(root, cmd, &request, &[]))
}

pub fn read(root: &Path, rel: &str) -> String {
    std::fs::read_to_string(root.join(rel)).unwrap()
}

pub fn stamp(root: &Path, rel: &str) {
    let (_, map) = rivet(root, "map", json!({"path": rel}));
    let (code, out) = rivet(
        root,
        "stamp",
        json!({"path": rel, "guard": map["revision"]}),
    );
    assert_eq!(code, 0, "{out}");
}

/// Open actions across the collection as (title, id, revision).
pub fn open_actions(root: &Path) -> Vec<(String, String, String)> {
    let (code, out) = rivet(
        root,
        "query",
        json!({"collection": "actions", "where": [{"field": "done", "op": "eq", "value": false}]}),
    );
    assert_eq!(code, 0, "{out}");
    out["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| {
            (
                i["fields"]["title"].as_str().unwrap().to_string(),
                i["id"].as_str().unwrap_or("").to_string(),
                i["revision"].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

pub fn item_guard(root: &Path, rel: &str, id: &str) -> String {
    let (_, out) = rivet(
        root,
        "get",
        json!({"path": rel, "select": {"kind": "id", "id": id}}),
    );
    out["revision"].as_str().unwrap().to_string()
}

/// Pause point handshake for `RIVET_TEST_PAUSE=<point>:<dir>`.
pub struct Pause {
    pub dir: PathBuf,
    pub point: &'static str,
}

impl Pause {
    pub fn new(root: &Path, point: &'static str) -> Self {
        let dir = root.with_extension(format!("pause-{point}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Pause { dir, point }
    }

    pub fn env(&self) -> (&'static str, String) {
        (
            "RIVET_TEST_PAUSE",
            format!("{}:{}", self.point, self.dir.display()),
        )
    }

    pub fn wait(&self) {
        assert!(
            self.wait_while(|| true),
            "writer never reached {} within the 20-second fixture pause bound",
            self.point
        );
    }

    /// Keep the writer's response when it exits before reaching the hook.
    /// A Git deadline must not turn into an unexplained missing pause marker.
    pub fn wait_for(&self, mut child: Child) -> Child {
        if !self.wait_while(|| child.try_wait().unwrap().is_none()) {
            let running = child.try_wait().unwrap().is_none();
            if running {
                child.kill().unwrap();
            }
            let (code, out) = finish(child);
            panic!(
                "writer {} before {}: exit {code}, response {out}; \
                 fixture pause bound is 20 seconds, each Git child deadline is {:?}",
                if running {
                    "was killed at the fixture bound"
                } else {
                    "exited"
                },
                self.point,
                markrivet_core::git::DEFAULT_DEADLINE
            );
        }
        child
    }

    fn wait_while(&self, mut running: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(20);
        while !self.dir.join(format!("{}.reached", self.point)).exists() {
            if !running() || Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        true
    }

    pub fn release(&self) {
        std::fs::write(self.dir.join(format!("{}.go", self.point)), "").unwrap();
    }
}

/// This host's name, as an Emacs lock written here would carry it.
pub fn host_name() -> String {
    let mut buf = [0u8; 256];
    // SAFETY: a buffer of the stated length, its last byte never written.
    assert_eq!(
        unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len() - 1) },
        0
    );
    let len = buf.iter().position(|b| *b == 0).unwrap();
    String::from_utf8_lossy(&buf[..len]).replace([' ', '\t'], "-")
}

/// `/proc/stat`'s boot time, in seconds since the epoch.
pub fn boot_time() -> u64 {
    std::fs::read_to_string("/proc/stat")
        .unwrap()
        .lines()
        .find_map(|l| l.strip_prefix("btime "))
        .unwrap()
        .trim()
        .parse()
        .unwrap()
}

/// A pid no process can have: `pid_max` is one more than the largest pid
/// the kernel allocates (proc(5)).
pub fn unused_pid() -> u64 {
    std::fs::read_to_string("/proc/sys/kernel/pid_max")
        .unwrap()
        .trim()
        .parse()
        .unwrap()
}

/// An Emacs lock text naming this test process on this host: live for as
/// long as the test runs.
pub fn live_lock_text() -> String {
    format!("ada@{}.{}:{}", host_name(), std::process::id(), boot_time())
}

/// Emacs's lock for `rel`: `.#<name>` beside it, a symlink holding `text`,
/// or a regular file holding it when `file` is set.
pub fn emacs_lock(root: &Path, rel: &str, text: &str, file: bool) -> PathBuf {
    let target = root.join(rel);
    let name = target.file_name().unwrap().to_string_lossy().into_owned();
    let lock = target.with_file_name(format!(".#{name}"));
    if file {
        std::fs::write(&lock, text).unwrap();
    } else {
        std::os::unix::fs::symlink(text, &lock).unwrap();
    }
    lock
}

/// Whether this test runs in the initial pid namespace, where a pid that
/// names no process proves an Emacs lock dead.
pub fn initial_pid_namespace() -> bool {
    std::fs::read_link("/proc/self/ns/pid")
        .is_ok_and(|ns| ns.to_string_lossy() == "pid:[4026531836]")
}

/// Set a lock's own mtime (not its target's), in seconds since the epoch.
pub fn set_lock_mtime(lock: &Path, secs: i64) {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(lock.as_os_str().as_bytes()).unwrap();
    let t = libc::timespec {
        tv_sec: secs,
        tv_nsec: 0,
    };
    // SAFETY: a terminated path and two valid timespecs.
    let ret = unsafe {
        libc::utimensat(
            libc::AT_FDCWD,
            path.as_ptr(),
            [t, t].as_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    assert_eq!(ret, 0, "{}", std::io::Error::last_os_error());
}

/// What a lock holds, read the way it was written.
pub fn lock_text(lock: &Path) -> String {
    match std::fs::read_link(lock) {
        Ok(text) => text.to_string_lossy().into_owned(),
        Err(_) => std::fs::read_to_string(lock).unwrap(),
    }
}
