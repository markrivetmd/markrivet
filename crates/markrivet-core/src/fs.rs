//! Project root, path confinement, the writer lock and durable replacement.
//!
//! Cooperating Rivet writers serialise on one advisory lock file that is
//! never unlinked, so its identity survives every target replacement. The
//! guarantee covers Rivet writers on a local Linux filesystem; an editor that
//! ignores the lock can still race the final rename. A live Emacs lock
//! beside the target refuses the write ([`crate::editor_lock`]).
//!
//! Every document access walks from the project root with `O_NOFOLLOW`
//! directory descriptors and then works relative to the parent descriptor, so
//! swapping a path component for a symlink after the check cannot redirect a
//! read, the temporary file or the rename.
use crate::{Diagnostic, ErrorCode};
use std::ffi::{CStr, CString};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub const MAX_FILE_BYTES: u64 = 8 * 1024 * 1024;
pub const DEFAULT_LOCK_TIMEOUT: Duration = Duration::from_secs(10);

/// The ignored directory under `.rivet/` that holds pending records and
/// stages every other bookkeeping file's replacement.
pub const STAGING: &str = "pending";

/// The largest whole log read or complete line in a backwards log scan.
pub const MAX_LOG_BYTES: u64 = 256 * 1024 * 1024;

/// Debug tests lower the read bound without creating a 256 MiB fixture.
pub(crate) fn log_read_limit() -> u64 {
    #[cfg(debug_assertions)]
    if let Some(limit) = std::env::var("RIVET_TEST_LOG_BYTES")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|n| *n > 0 && *n < MAX_LOG_BYTES)
    {
        return limit;
    }
    MAX_LOG_BYTES
}

const LOG_CHUNK_BYTES: usize = 64 * 1024;

/// Why a log append did not complete.
#[derive(Debug, Clone)]
pub enum LogFailure {
    /// The line may be partly written; the next append truncates it.
    Append(Diagnostic),
    /// The whole line is written but its sync failed.
    Sync(Diagnostic),
}

impl LogFailure {
    pub fn diagnostic(&self) -> &Diagnostic {
        match self {
            LogFailure::Append(d) | LogFailure::Sync(d) => d,
        }
    }
}

fn store_shown(sub: Option<&str>, name: &str) -> String {
    match sub {
        Some(sub) => format!(".rivet/{sub}/{name}"),
        None => format!(".rivet/{name}"),
    }
}

/// Lines `.rivet/.gitignore` carries: the lock and commit-mode bookkeeping
/// that must never be committed.
pub const RIVET_IGNORES: [&str; 5] = [
    "/lock",
    "/seq",
    "/pending/",
    "/commits.jsonl",
    "/push.jsonl",
];

/// The one spelling of a request path: `/`-separated components with empty
/// and `.` components dropped. `..` and absolute paths are refused. Bindings,
/// git lookups and reports only ever see this form, so `tracking//a.md` and
/// `tracking/./a.md` are the same document as `tracking/a.md`.
pub fn canonical_path(rel: &str) -> Result<String, Diagnostic> {
    let reject = |why: &str| Diagnostic::new(ErrorCode::PathRejected, format!("{rel:?}: {why}"));
    if rel.starts_with('/') {
        return Err(reject("paths are relative to the project root"));
    }
    let parts: Vec<&str> = rel
        .split('/')
        .filter(|p| !p.is_empty() && *p != ".")
        .collect();
    if parts.contains(&"..") {
        return Err(reject("only plain path components are allowed"));
    }
    if parts.is_empty() {
        return Err(reject("paths are relative to the project root"));
    }
    Ok(parts.join("/"))
}

/// Set once a rename has been attempted in this process. A crash handler
/// uses it to report a possibly-applied write instead of a safe refusal.
pub static COMMIT_STARTED: AtomicBool = AtomicBool::new(false);

/// A project root held open by descriptor for its lifetime, so renaming or
/// replacing the root path mid-write cannot redirect any access.
#[derive(Debug, Clone)]
pub struct Project {
    pub root: PathBuf,
    fd: Arc<OwnedFd>,
}

/// Held for the whole read-verify-replace sequence; dropping releases it.
pub struct WriterLock {
    _file: File,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Commit {
    Applied,
    /// The rename happened but the directory sync failed: the new bytes are
    /// visible and may not be durable. Never retry blindly.
    AppliedNotDurable,
}

/// A confined document: its parent directory descriptor and final name.
struct Located {
    dir: OwnedFd,
    name: CString,
}

fn io(code: ErrorCode, what: &str, e: std::io::Error) -> Diagnostic {
    Diagnostic::new(code, format!("{what}: {e}"))
}

fn check(ret: libc::c_int) -> std::io::Result<libc::c_int> {
    if ret < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(ret)
    }
}

fn open_at(
    dir: RawFd,
    name: &CStr,
    flags: libc::c_int,
    mode: libc::c_uint,
) -> std::io::Result<OwnedFd> {
    // SAFETY: `name` is NUL-terminated; the returned descriptor is owned here.
    let fd = check(unsafe { libc::openat(dir, name.as_ptr(), flags | libc::O_CLOEXEC, mode) })?;
    // SAFETY: openat returned a fresh descriptor.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn stat_at(dir: RawFd, name: &CStr) -> std::io::Result<libc::stat> {
    // SAFETY: a zeroed stat is a valid out-parameter.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: valid descriptor, NUL-terminated name, owned out-parameter.
    check(unsafe { libc::fstatat(dir, name.as_ptr(), &mut st, libc::AT_SYMLINK_NOFOLLOW) })?;
    Ok(st)
}

fn is_kind(st: &libc::stat, kind: libc::mode_t) -> bool {
    st.st_mode & libc::S_IFMT == kind
}

fn open_root(root: &Path) -> Result<Project, Diagnostic> {
    let path = CString::new(root.as_os_str().as_bytes())
        .map_err(|_| Diagnostic::new(ErrorCode::PathRejected, "project root contains NUL"))?;
    let fd = open_at(libc::AT_FDCWD, &path, libc::O_RDONLY | libc::O_DIRECTORY, 0)
        .map_err(|e| io(ErrorCode::NotFound, "project root", e))?;
    let rivet = CString::new(".rivet").expect("static");
    match stat_at(fd.as_raw_fd(), &rivet) {
        Ok(st) if is_kind(&st, libc::S_IFDIR) => Ok(Project {
            root: root.to_path_buf(),
            fd: Arc::new(fd),
        }),
        _ => Err(Diagnostic::new(
            ErrorCode::NotFound,
            format!(
                "{} has no .rivet directory; run `markrivet init`",
                root.display()
            ),
        )),
    }
}

/// The [`RIVET_IGNORES`] lines `.rivet/.gitignore` text lacks, compared line
/// by line as git reads a pattern.
pub fn missing_ignores(text: &str) -> Vec<&'static str> {
    RIVET_IGNORES
        .into_iter()
        // Git keeps leading whitespace in a pattern and drops trailing spaces.
        .filter(|want| {
            !text
                .lines()
                .any(|l| l.trim_end_matches('\r').trim_end_matches(' ') == *want)
        })
        .collect()
}

impl Project {
    /// The held root descriptor, for children that must start in the project
    /// this process opened rather than whatever the root path names now.
    pub(crate) fn root_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }

    /// The nearest ancestor of `start` holding a `.rivet` directory.
    pub fn discover(start: &Path) -> Result<Project, Diagnostic> {
        let start =
            fs::canonicalize(start).map_err(|e| io(ErrorCode::NotFound, "project start", e))?;
        for dir in start.ancestors() {
            if fs::symlink_metadata(dir.join(".rivet")).is_ok_and(|m| m.is_dir()) {
                return open_root(dir);
            }
        }
        Err(Diagnostic::new(
            ErrorCode::NotFound,
            "no .rivet directory here or above; run `markrivet init` or pass --root",
        ))
    }

    pub fn open(root: &Path) -> Result<Project, Diagnostic> {
        let root =
            fs::canonicalize(root).map_err(|e| io(ErrorCode::NotFound, "project root", e))?;
        open_root(&root)
    }

    /// Create `.rivet/` with its schema directory and an ignore file for the
    /// lock and commit bookkeeping. Re-run on a project, it appends any
    /// missing ignore line and leaves the rest of the file as it was.
    pub fn init(root: &Path) -> Result<Project, Diagnostic> {
        let root =
            fs::canonicalize(root).map_err(|e| io(ErrorCode::NotFound, "project root", e))?;
        let dir = root.join(".rivet");
        if fs::symlink_metadata(&dir).is_ok_and(|m| !m.is_dir()) {
            return Err(Diagnostic::new(
                ErrorCode::PathRejected,
                ".rivet exists and is not a directory",
            ));
        }
        fs::create_dir_all(dir.join("schemas"))
            .map_err(|e| io(ErrorCode::Io, "create .rivet", e))?;
        let project = open_root(&root)?;
        // Under the project lock, and append-only: the existing bytes,
        // permissions and attributes are never rewritten, so a crash leaves
        // at worst a partial last line after them.
        let lock = project.lock(DEFAULT_LOCK_TIMEOUT)?;
        let current = project.read_config(&[".gitignore"])?.unwrap_or_default();
        let missing = missing_ignores(&String::from_utf8_lossy(&current));
        if !missing.is_empty() {
            let mut tail = String::new();
            if current.last().is_some_and(|b| *b != b'\n') {
                tail.push('\n');
            }
            for line in missing {
                tail.push_str(line);
                tail.push('\n');
            }
            project.append_config(&lock, ".gitignore", tail.as_bytes())?;
        }
        drop(lock);
        Ok(project)
    }

    /// Append to `.rivet/<name>`, creating it if absent, relative to the held
    /// root without symlinks, then sync. A failure after the write is
    /// `commit_uncertain`: the bytes may be there.
    fn append_config(
        &self,
        _lock: &WriterLock,
        name: &str,
        bytes: &[u8],
    ) -> Result<(), Diagnostic> {
        let shown = format!(".rivet/{name}");
        let fail = |e: std::io::Error| io(ErrorCode::Io, &shown, e);
        let rivet = open_at(
            self.fd.as_raw_fd(),
            &CString::new(".rivet").expect("static"),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW,
            0,
        )
        .map_err(fail)?;
        let mut file = File::from(
            open_at(
                rivet.as_raw_fd(),
                &CString::new(name).expect("static name"),
                libc::O_WRONLY | libc::O_APPEND | libc::O_CREAT | libc::O_NOFOLLOW,
                0o644,
            )
            .map_err(fail)?,
        );
        if !file.metadata().map_err(fail)?.is_file() {
            return Err(Diagnostic::new(
                ErrorCode::PathRejected,
                format!("{shown}: not a regular file"),
            ));
        }
        // The document rules: a write must not reach another link, change
        // ownership, or lose special bits or attributes the kernel clears.
        check_metadata(&file, &shown)?;
        file.write_all(bytes)
            .and_then(|()| fault("config_sync"))
            .and_then(|()| file.sync_all())
            // SAFETY: fsync on an owned directory descriptor.
            .and_then(|()| check(unsafe { libc::fsync(rivet.as_raw_fd()) }).map(drop))
            .map_err(|e| {
                io(
                    ErrorCode::CommitUncertain,
                    &format!("{shown} may have been appended to; re-read it"),
                    e,
                )
            })
    }

    /// Open `.rivet/<parts...>` from the root descriptor without following
    /// symlinks. `None` when the file does not exist.
    pub fn read_config(&self, parts: &[&str]) -> Result<Option<Vec<u8>>, Diagnostic> {
        let shown = format!(".rivet/{}", parts.join("/"));
        let fail = |e: std::io::Error| match e.raw_os_error() {
            Some(libc::ELOOP | libc::ENOTDIR) => Diagnostic::new(
                ErrorCode::PathRejected,
                format!("{shown}: symlinks are not followed"),
            ),
            _ => io(ErrorCode::Io, &shown, e),
        };
        let mut dir = open_at(
            self.fd.as_raw_fd(),
            &CString::new(".rivet").expect("static"),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW,
            0,
        )
        .map_err(fail)?;
        let (name, dirs) = parts.split_last().expect("a config path");
        for part in dirs {
            let part = CString::new(*part).expect("static path");
            dir = open_at(
                dir.as_raw_fd(),
                &part,
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW,
                0,
            )
            .map_err(fail)?;
        }
        let name = CString::new(*name).map_err(|_| {
            Diagnostic::new(ErrorCode::PathRejected, format!("{shown}: contains NUL"))
        })?;
        match open_read(dir.as_raw_fd(), &name) {
            Ok(file) => {
                let (bytes, _) = read_regular(file, MAX_FILE_BYTES).map_err(|e| match e {
                    ReadFailure::Io(e) => fail(e),
                    e => e.diagnostic(
                        &shown,
                        format!("{shown}: not a regular file"),
                        format!("{shown} is larger than {MAX_FILE_BYTES} bytes"),
                    ),
                })?;
                Ok(Some(bytes))
            }
            Err(e) if e.raw_os_error() == Some(libc::ENOENT) => Ok(None),
            Err(e) => Err(fail(e)),
        }
    }

    /// `.rivet/` itself, or `.rivet/<sub>` when `sub` is given, opened from
    /// the root descriptor without following symlinks. With `create`, a
    /// missing `sub` is made and `.rivet/` synced so the new entry is durable.
    fn store_dir(&self, sub: Option<&str>, create: bool) -> std::io::Result<Option<OwnedFd>> {
        let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW;
        let rivet = open_at(
            self.fd.as_raw_fd(),
            &CString::new(".rivet").expect("static"),
            flags,
            0,
        )?;
        let Some(sub) = sub else {
            return Ok(Some(rivet));
        };
        let name = CString::new(sub).map_err(std::io::Error::other)?;
        match open_at(rivet.as_raw_fd(), &name, flags, 0) {
            Ok(fd) => Ok(Some(fd)),
            Err(e) if e.raw_os_error() == Some(libc::ENOENT) && create => {
                // SAFETY: held directory descriptor and a NUL-terminated name.
                let made = unsafe { libc::mkdirat(rivet.as_raw_fd(), name.as_ptr(), 0o755) };
                if made != 0 && std::io::Error::last_os_error().raw_os_error() != Some(libc::EEXIST)
                {
                    return Err(std::io::Error::last_os_error());
                }
                // SAFETY: fsync on a held directory descriptor.
                check(unsafe { libc::fsync(rivet.as_raw_fd()) })?;
                open_at(rivet.as_raw_fd(), &name, flags, 0).map(Some)
            }
            Err(e) if e.raw_os_error() == Some(libc::ENOENT) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Publish `.rivet/<sub>/<name>` (or `.rivet/<name>`) whole: a temporary
    /// file, synced, renamed over the name, then the directory synced. A
    /// reader sees the old bytes or the new ones, never a mixture.
    pub fn store_put(
        &self,
        _lock: &WriterLock,
        sub: Option<&str>,
        name: &str,
        bytes: &[u8],
    ) -> Result<(), Diagnostic> {
        let shown = store_shown(sub, name);
        let fail = |e: std::io::Error| io(ErrorCode::Io, &shown, e);
        let dir = self.store_dir(sub, true).map_err(fail)?.expect("created");
        // A file directly under `.rivet/` is staged in the ignored staging
        // directory, so a crash never leaves a temporary file git would see.
        let stage = match sub {
            Some(_) => None,
            None => Some(
                self.store_dir(Some(STAGING), true)
                    .map_err(fail)?
                    .expect("created"),
            ),
        };
        let from = stage.as_ref().unwrap_or(&dir).as_raw_fd();
        let target = CString::new(name).map_err(|e| fail(std::io::Error::other(e)))?;
        let temp = temp_name(&target);
        let written = (|| {
            let mut file = File::from(open_at(
                from,
                &temp,
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW,
                0o644,
            )?);
            file.write_all(bytes)?;
            file.sync_all()?;
            // SAFETY: both names are NUL-terminated, each relative to a held
            // directory descriptor on one filesystem.
            check(unsafe {
                libc::renameat(from, temp.as_ptr(), dir.as_raw_fd(), target.as_ptr())
            })?;
            // SAFETY: fsync on a held directory descriptor.
            check(unsafe { libc::fsync(dir.as_raw_fd()) }).map(drop)
        })();
        if let Err(e) = written {
            // SAFETY: held descriptor and NUL-terminated name.
            unsafe { libc::unlinkat(from, temp.as_ptr(), 0) };
            return Err(fail(e));
        }
        Ok(())
    }

    /// Remove `.rivet/<sub>/<name>` for each name (a missing one is fine),
    /// then sync the directory.
    pub fn store_remove(
        &self,
        _lock: &WriterLock,
        sub: &str,
        names: &[&str],
    ) -> Result<(), Diagnostic> {
        let shown = store_shown(Some(sub), "");
        let fail = |e: std::io::Error| io(ErrorCode::Io, &shown, e);
        let Some(dir) = self.store_dir(Some(sub), false).map_err(fail)? else {
            return Ok(());
        };
        for name in names {
            let name = CString::new(*name).map_err(|e| fail(std::io::Error::other(e)))?;
            // SAFETY: held directory descriptor and NUL-terminated name.
            if unsafe { libc::unlinkat(dir.as_raw_fd(), name.as_ptr(), 0) } != 0
                && std::io::Error::last_os_error().raw_os_error() != Some(libc::ENOENT)
            {
                return Err(fail(std::io::Error::last_os_error()));
            }
        }
        // SAFETY: fsync on a held directory descriptor.
        check(unsafe { libc::fsync(dir.as_raw_fd()) })
            .map(drop)
            .map_err(fail)
    }

    pub fn store_list(
        &self,
        sub: &str,
        suffix: &str,
    ) -> Result<Vec<(String, Vec<u8>)>, Diagnostic> {
        self.store_list_bounded(sub, suffix, MAX_FILE_BYTES)
    }

    /// Every regular file in `.rivet/<sub>` whose name ends in `suffix`, with
    /// its bytes, sorted by name. Empty when the directory does not exist.
    pub(crate) fn store_list_bounded(
        &self,
        sub: &str,
        suffix: &str,
        max_bytes: u64,
    ) -> Result<Vec<(String, Vec<u8>)>, Diagnostic> {
        self.store_list_bounded_checked(sub, suffix, max_bytes, false)
    }

    pub(crate) fn store_list_bounded_strict(
        &self,
        sub: &str,
        suffix: &str,
        max_bytes: u64,
    ) -> Result<Vec<(String, Vec<u8>)>, Diagnostic> {
        self.store_list_bounded_checked(sub, suffix, max_bytes, true)
    }

    fn store_list_bounded_checked(
        &self,
        sub: &str,
        suffix: &str,
        max_bytes: u64,
        strict: bool,
    ) -> Result<Vec<(String, Vec<u8>)>, Diagnostic> {
        let shown = store_shown(Some(sub), "");
        let fail = |e: std::io::Error| io(ErrorCode::Io, &shown, e);
        let Some(dir) = self.store_dir(Some(sub), false).map_err(fail)? else {
            return Ok(vec![]);
        };
        let mut out = vec![];
        for entry in fs::read_dir(format!("/proc/self/fd/{}", dir.as_raw_fd())).map_err(fail)? {
            let entry = entry.map_err(fail)?;
            let native = entry.file_name();
            if strict
                && native.as_bytes().ends_with(suffix.as_bytes())
                && (native.as_bytes().starts_with(b".") || native.to_str().is_none())
            {
                return Err(fail(std::io::Error::other(
                    "a hidden or non-UTF-8 bookkeeping name makes ownership unknown",
                )));
            }
            let name = native.to_string_lossy().into_owned();
            if name.starts_with('.') || !name.ends_with(suffix) {
                continue;
            }
            let cname =
                CString::new(name.as_bytes()).map_err(|e| fail(std::io::Error::other(e)))?;
            let file = match open_read(dir.as_raw_fd(), &cname) {
                Ok(file) => file,
                // Removed by its writer between the listing and the open.
                Err(e) if e.raw_os_error() == Some(libc::ENOENT) => continue,
                Err(e) => return Err(fail(e)),
            };
            let (bytes, _) = match read_regular(file, max_bytes) {
                Ok(capture) => capture,
                Err(ReadFailure::NotRegular) if !strict => continue,
                Err(e) => {
                    return Err(e.diagnostic(
                        &shown,
                        format!("{shown}{name}: not a regular file"),
                        format!("{shown}{name} exceeds the {max_bytes}-byte bookkeeping limit"),
                    ));
                }
            };
            out.push((name, bytes));
        }
        out.sort();
        Ok(out)
    }

    /// The next number from `.rivet/seq`, which only ever grows. Call it
    /// holding the writer lock.
    pub fn next_seq(&self, lock: &WriterLock) -> Result<u64, Diagnostic> {
        let current = match self.read_config(&["seq"])? {
            Some(bytes) => String::from_utf8_lossy(&bytes)
                .trim()
                .parse::<u64>()
                .map_err(|_| Diagnostic::new(ErrorCode::Io, ".rivet/seq is not a number"))?,
            None => 0,
        };
        let next = current + 1;
        self.store_put(lock, None, "seq", format!("{next}\n").as_bytes())?;
        Ok(next)
    }

    /// Append one whole line to the log `.rivet/<name>` and sync it. A last
    /// line left without its newline by an earlier failure is truncated
    /// first, so a reader never joins two events. The first time the log is
    /// created, `.rivet/` is synced too.
    pub fn log_append(
        &self,
        _lock: &WriterLock,
        name: &str,
        line: &[u8],
    ) -> Result<(), LogFailure> {
        let shown = store_shown(None, name);
        let fail = |e: std::io::Error| LogFailure::Append(io(ErrorCode::Io, &shown, e));
        let rivet = self.store_dir(None, false).map_err(fail)?.expect(".rivet");
        let cname = CString::new(name).expect("static log name");
        let created = match stat_at(rivet.as_raw_fd(), &cname) {
            Ok(_) => false,
            Err(e) if e.raw_os_error() == Some(libc::ENOENT) => true,
            Err(e) => return Err(fail(e)),
        };
        let mut file = File::from(
            open_at(
                rivet.as_raw_fd(),
                &cname,
                libc::O_RDWR | libc::O_APPEND | libc::O_CREAT | libc::O_NOFOLLOW,
                0o644,
            )
            .map_err(fail)?,
        );
        if !file.metadata().map_err(fail)?.is_file() {
            return Err(LogFailure::Append(Diagnostic::new(
                ErrorCode::PathRejected,
                format!("{shown}: not a regular file"),
            )));
        }
        if created {
            // SAFETY: fsync on a held directory descriptor.
            check(unsafe { libc::fsync(rivet.as_raw_fd()) }).map_err(fail)?;
        }
        // Find the complete prefix without loading an unterminated tail.
        let len = file.metadata().map_err(fail)?.len();
        let mut last = *b"\n";
        if len > 0 {
            use std::os::unix::fs::FileExt;
            file.read_exact_at(&mut last, len - 1).map_err(fail)?;
        }
        if last[0] != b'\n' {
            let keep = log_complete_len(&file, len, LOG_CHUNK_BYTES).map_err(fail)?;
            file.set_len(keep).map_err(fail)?;
        }
        let mut whole = line.to_vec();
        whole.push(b'\n');
        if fault("receipt_append").is_err() {
            let _ = file.write_all(&whole[..whole.len() / 2]);
            return Err(fail(std::io::Error::other("injected receipt_append fault")));
        }
        file.write_all(&whole).map_err(fail)?;
        fault("receipt_sync")
            .and_then(|()| file.sync_all())
            .map_err(|e| LogFailure::Sync(io(ErrorCode::Io, &shown, e)))
    }

    /// Sync the log `.rivet/<name>`, for an event already in it whose own
    /// sync failed.
    pub fn log_sync(&self, name: &str) -> Result<(), Diagnostic> {
        let shown = store_shown(None, name);
        let fail = |e: std::io::Error| io(ErrorCode::Io, &shown, e);
        let rivet = self.store_dir(None, false).map_err(fail)?.expect(".rivet");
        let file = File::from(
            open_at(
                rivet.as_raw_fd(),
                &CString::new(name).expect("static log name"),
                libc::O_RDONLY | libc::O_NOFOLLOW,
                0,
            )
            .map_err(fail)?,
        );
        fault("receipt_sync")
            .and_then(|()| file.sync_all())
            .map_err(fail)
    }

    /// The whole log `.rivet/<name>`, empty when it does not exist.
    pub fn log_read(&self, name: &str) -> Result<Vec<u8>, Diagnostic> {
        let shown = store_shown(None, name);
        let Some((file, _)) = self.log_open(name)? else {
            return Ok(vec![]);
        };
        let limit = log_read_limit();
        let (bytes, _) = read_regular(file, limit).map_err(|e| {
            e.diagnostic(
                &shown,
                format!("{shown}: not a regular file"),
                format!("{shown} is larger than {limit} bytes"),
            )
        })?;
        Ok(bytes)
    }

    /// Confined metadata inspection without loading the log's contents.
    pub(crate) fn log_size(&self, name: &str) -> Result<u64, Diagnostic> {
        Ok(self.log_open(name)?.map_or(0, |(_, meta)| meta.len()))
    }

    /// Visit complete lines newest first. `false` stops the scan. There is no
    /// total-size cap; the incomplete suffix is discarded without retaining it.
    pub(crate) fn log_visit_reverse(
        &self,
        name: &str,
        visit: impl FnMut(&[u8]) -> bool,
    ) -> Result<(), Diagnostic> {
        let Some((file, meta)) = self.log_open(name)? else {
            return Ok(());
        };
        let shown = store_shown(None, name);
        let limit = log_read_limit();
        hook("receipt_scan")?;
        visit_log_reverse(&file, meta.len(), LOG_CHUNK_BYTES, limit, visit).map_err(|e| {
            e.diagnostic(
                &shown,
                format!("{shown}: not a regular file"),
                format!("{shown}: complete log line is larger than {limit} bytes"),
            )
        })
    }

    fn log_open(&self, name: &str) -> Result<Option<(File, fs::Metadata)>, Diagnostic> {
        let shown = store_shown(None, name);
        let fail = |e: std::io::Error| io(ErrorCode::Io, &shown, e);
        let rivet = self.store_dir(None, false).map_err(fail)?.expect(".rivet");
        let file = match open_read(
            rivet.as_raw_fd(),
            &CString::new(name).expect("static log name"),
        ) {
            Ok(file) => file,
            Err(e) if e.raw_os_error() == Some(libc::ENOENT) => return Ok(None),
            Err(e) => return Err(fail(e)),
        };
        let meta = file.metadata().map_err(fail)?;
        if !meta.is_file() {
            return Err(Diagnostic::new(
                ErrorCode::PathRejected,
                format!("{shown}: not a regular file"),
            ));
        }
        Ok(Some((file, meta)))
    }

    /// Sync a document and its directory, so a write that landed without a
    /// durable rename is durable before git reads it.
    pub fn sync_document(&self, rel: &str) -> Result<(), Diagnostic> {
        let loc = self.locate(rel)?;
        let file = File::from(
            open_at(
                loc.dir.as_raw_fd(),
                &loc.name,
                libc::O_RDONLY | libc::O_NOFOLLOW,
                0,
            )
            .map_err(|e| io(ErrorCode::Io, rel, e))?,
        );
        file.sync_all().map_err(|e| io(ErrorCode::Io, rel, e))?;
        // SAFETY: fsync on a held directory descriptor.
        check(unsafe { libc::fsync(loc.dir.as_raw_fd()) })
            .map(drop)
            .map_err(|e| io(ErrorCode::Io, rel, e))
    }

    /// Confine a request path: relative, plain components, a Markdown
    /// extension, no symlinks on the way, a regular file at the end.
    fn locate(&self, rel: &str) -> Result<Located, Diagnostic> {
        let (loc, st) = self.locate_parent(rel)?;
        let reject =
            |why: &str| Diagnostic::new(ErrorCode::PathRejected, format!("{rel:?}: {why}"));
        let st = st.map_err(|e| match e.raw_os_error() {
            Some(libc::ENOENT) => {
                Diagnostic::new(ErrorCode::NotFound, format!("{rel:?} does not exist"))
            }
            Some(libc::ELOOP | libc::ENOTDIR) => {
                reject("symlinks and non-directories are not followed")
            }
            _ => io(ErrorCode::Io, rel, e),
        })?;
        if is_kind(&st, libc::S_IFLNK) {
            return Err(reject("symlinks are not followed"));
        }
        if !is_kind(&st, libc::S_IFREG) {
            return Err(reject("not a regular file"));
        }
        Ok(loc)
    }

    /// The confined parent directory of a document that must not exist yet.
    fn locate_new(&self, rel: &str) -> Result<Located, Diagnostic> {
        let (loc, st) = self.locate_parent(rel).map_err(|e| {
            if e.code == ErrorCode::NotFound {
                Diagnostic::new(
                    ErrorCode::NotFound,
                    format!("{rel:?}: its directory does not exist; Rivet creates documents, not directories"),
                )
            } else {
                e
            }
        })?;
        match st {
            Err(e) if e.raw_os_error() == Some(libc::ENOENT) => Ok(loc),
            Err(e) => Err(io(ErrorCode::Io, rel, e)),
            Ok(_) => Err(Diagnostic::new(
                ErrorCode::AlreadyExists,
                format!("{rel:?} already exists"),
            )),
        }
    }

    /// Walk to a document's parent through `O_NOFOLLOW` descriptors; the
    /// final name's `fstatat` result is left to the caller.
    fn locate_parent(
        &self,
        rel: &str,
    ) -> Result<(Located, std::io::Result<libc::stat>), Diagnostic> {
        let reject =
            |why: &str| Diagnostic::new(ErrorCode::PathRejected, format!("{rel:?}: {why}"));
        let path = Path::new(rel);
        if rel.is_empty() || path.is_absolute() {
            return Err(reject("paths are relative to the project root"));
        }
        if !matches!(
            path.extension().and_then(|e| e.to_str()),
            Some("md" | "markdown")
        ) {
            return Err(reject("only .md and .markdown files are documents"));
        }
        let mut parts = vec![];
        for component in path.components() {
            let Component::Normal(part) = component else {
                return Err(reject("only plain path components are allowed"));
            };
            parts.push(CString::new(part.as_bytes()).map_err(|_| reject("path contains NUL"))?);
        }
        if parts[0].as_bytes() == b".rivet" {
            return Err(reject(".rivet holds configuration, not documents"));
        }
        let walk_error = |e: std::io::Error| match e.raw_os_error() {
            Some(libc::ENOENT) => {
                Diagnostic::new(ErrorCode::NotFound, format!("{rel:?} does not exist"))
            }
            Some(libc::ELOOP | libc::ENOTDIR) => {
                reject("symlinks and non-directories are not followed")
            }
            _ => io(ErrorCode::Io, rel, e),
        };
        let mut dir = self.fd.try_clone().map_err(walk_error)?;
        let name = parts.pop().expect("at least one component");
        for part in &parts {
            dir = open_at(
                dir.as_raw_fd(),
                part,
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW,
                0,
            )
            .map_err(walk_error)?;
        }
        let st = stat_at(dir.as_raw_fd(), &name);
        Ok((Located { dir, name }, st))
    }

    /// Every regular file under the root, reached through descriptors and
    /// never through symlinks, as sorted relative paths; plus a diagnostic
    /// for each directory that could not be listed. Hidden entries, `target`
    /// and `node_modules` are skipped.
    fn open_dir(&self, rel: &str) -> std::io::Result<OwnedFd> {
        let mut dir = self.fd.try_clone()?;
        for part in rel.split('/').filter(|p| !p.is_empty()) {
            let part = CString::new(part).map_err(std::io::Error::other)?;
            dir = open_at(
                dir.as_raw_fd(),
                &part,
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW,
                0,
            )?;
        }
        Ok(dir)
    }

    pub fn list(&self) -> (Vec<String>, Vec<Diagnostic>) {
        self.list_with(&WalkFaults::default())
    }

    /// Create archive parents through held directory descriptors. Existing links
    /// and non-directories refuse; each new directory entry is synced.
    pub fn archive_mkdir(&self, _lock: &WriterLock, path: &str) -> Result<(), Diagnostic> {
        crate::delete_receipt::require_canonical(path)?;
        let config = crate::schema::load_config(self)?;
        if !config
            .archive_patterns()
            .iter()
            .any(|g| glob_reachable(g, path))
        {
            return Err(Diagnostic::new(
                ErrorCode::ArchiveCopyRefused,
                "directory does not lead to a configured archive",
            ));
        }
        let mut dir = self
            .fd
            .try_clone()
            .map_err(|e| io(ErrorCode::Io, path, e))?;
        for part in path.split('/') {
            let name = CString::new(part)
                .map_err(|e| Diagnostic::new(ErrorCode::PathRejected, e.to_string()))?;
            if unsafe { libc::mkdirat(dir.as_raw_fd(), name.as_ptr(), 0o755) } != 0 {
                let error = std::io::Error::last_os_error();
                if error.raw_os_error() != Some(libc::EEXIST) {
                    return Err(io(ErrorCode::Io, path, error));
                }
            }
            let next = open_at(
                dir.as_raw_fd(),
                &name,
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW,
                0,
            )
            .map_err(|e| io(ErrorCode::PathRejected, path, e))?;
            if unsafe { libc::fsync(dir.as_raw_fd()) } != 0 {
                return Err(io(ErrorCode::Io, path, std::io::Error::last_os_error()));
            }
            dir = next;
        }
        if unsafe { libc::fsync(dir.as_raw_fd()) } != 0 {
            return Err(io(ErrorCode::Io, path, std::io::Error::last_os_error()));
        }
        Ok(())
    }

    /// Snapshot enumeration refuses symlinks and incomplete names rather than omitting inputs.
    pub fn list_strict(&self) -> (Vec<String>, Vec<Diagnostic>) {
        self.list_with_policy(&WalkFaults::default(), true)
    }

    /// Snapshot enumeration restricted to the complete configured namespace.
    pub fn list_strict_scoped(&self, globs: &[String]) -> (Vec<String>, Vec<Diagnostic>) {
        self.list_strict_scoped_with(globs, &WalkFaults::default())
    }

    /// Explicit faults let coverage tests exercise unreadable paths even as root.
    pub fn list_strict_scoped_with(
        &self,
        globs: &[String],
        faults: &WalkFaults,
    ) -> (Vec<String>, Vec<Diagnostic>) {
        let (files, events) = self.observe_policy(globs, None, faults, true, None);
        let errors = events
            .iter()
            .filter_map(|e| e.strict_diagnostic(globs))
            .collect();
        (files, errors)
    }

    /// Bounded immutable policy discovery. Archive readers include skipped
    /// namespaces, so a relevant skipped entry is a refusal, never an absence.
    pub(crate) fn snapshot_files(
        &self,
        globs: &[String],
        archives: &[String],
    ) -> (Vec<String>, Vec<Diagnostic>) {
        self.snapshot_files_with(
            globs,
            archives,
            &WalkFaults::default(),
            50_000,
            8 * 1024 * 1024,
        )
    }

    fn snapshot_files_with(
        &self,
        globs: &[String],
        archives: &[String],
        faults: &WalkFaults,
        max_entries: usize,
        max_path_bytes: usize,
    ) -> (Vec<String>, Vec<Diagnostic>) {
        let (files, events) = self.observe_policy(
            globs,
            None,
            faults,
            true,
            Some(SnapshotWalk {
                archives,
                max_entries,
                max_path_bytes,
            }),
        );
        let errors = events
            .iter()
            .filter_map(|e| {
                let archive_gap = if e.native_name_hex.is_some() {
                    e.native_may_match_with(archives, true)
                } else {
                    e.matches(archives, e.path.as_deref().unwrap_or(""))
                };
                if e.reason == "limit"
                    || archive_gap
                        && (e.scope == "policy_excluded"
                            || e.is_gap(archives)
                            || e.native_name_hex.is_some())
                {
                    let mut d = e.diagnostic();
                    d.code = ErrorCode::PolicyConflict;
                    Some(d)
                } else {
                    e.strict_diagnostic(globs)
                }
            })
            .collect();
        (files, errors)
    }

    pub fn list_with(&self, faults: &WalkFaults) -> (Vec<String>, Vec<Diagnostic>) {
        self.list_with_policy(faults, false)
    }

    fn list_with_policy(
        &self,
        faults: &WalkFaults,
        strict: bool,
    ) -> (Vec<String>, Vec<Diagnostic>) {
        let globs = vec!["**".to_string()];
        if !strict {
            return self.documents(&globs, None, faults);
        }
        self.list_strict_scoped_with(&globs, faults)
    }

    /// Raw capture uses the same descriptor-confined document locator as writes.
    pub fn read_bytes(&self, rel: &str) -> Result<Vec<u8>, Diagnostic> {
        let loc = self.locate(rel)?;
        let file =
            open_read(loc.dir.as_raw_fd(), &loc.name).map_err(|e| io(ErrorCode::Io, rel, e))?;
        let (bytes, _) = read_regular(file, MAX_FILE_BYTES).map_err(|e| {
            e.diagnostic(
                rel,
                "not a regular file".into(),
                "document exceeds per-file capture cap".into(),
            )
            .with_path(rel)
        })?;
        Ok(bytes)
    }

    pub fn observe(
        &self,
        globs: &[String],
        paths: Option<&[String]>,
    ) -> (Vec<String>, Vec<WalkEvent>) {
        self.observe_with(globs, paths, &WalkFaults::default())
    }

    /// Observe matching Markdown documents with explicit local fault injection.
    pub fn observe_with(
        &self,
        globs: &[String],
        paths: Option<&[String]>,
        faults: &WalkFaults,
    ) -> (Vec<String>, Vec<WalkEvent>) {
        self.observe_policy(globs, paths, faults, false, None)
    }

    /// Matching documents and diagnostics for relevant coverage gaps and exclusions.
    pub fn documents(
        &self,
        globs: &[String],
        paths: Option<&[String]>,
        faults: &WalkFaults,
    ) -> (Vec<String>, Vec<Diagnostic>) {
        let (files, events) = self.observe_with(globs, paths, faults);
        let errors = events
            .iter()
            .filter_map(|e| e.document_diagnostic(globs))
            .collect();
        (files, errors)
    }

    /// Injection is explicit and local to this call, so deterministic coverage tests
    /// work as root without changing process environment or relying on chmod.
    fn observe_policy(
        &self,
        globs: &[String],
        paths: Option<&[String]>,
        faults: &WalkFaults,
        strict: bool,
        snapshot: Option<SnapshotWalk<'_>>,
    ) -> (Vec<String>, Vec<WalkEvent>) {
        let mut files = vec![];
        let mut events = vec![];
        let mut stack = vec![String::new()];
        let (mut entries_seen, mut path_bytes) = (0usize, 0usize);
        let reachable = |rel: &str| {
            if let Some(paths) = paths {
                return paths
                    .iter()
                    .any(|p| rel.is_empty() || p.starts_with(&format!("{rel}/")));
            }
            globs.iter().any(|g| glob_reachable(g, rel))
        };
        while let Some(rel) = stack.pop() {
            if !reachable(&rel) {
                events.push(WalkEvent::excluded(
                    &rel,
                    "glob_pruned",
                    true,
                    "out_of_scope",
                ));
                continue;
            }
            let dir = if faults.directories.contains(&rel) {
                Err(std::io::Error::other("injected enumeration failure"))
            } else {
                self.open_dir(&rel)
            };
            let dir = match dir {
                Ok(d) => d,
                Err(e) => {
                    events.push(WalkEvent::failed(
                        Some(rel),
                        "open_directory",
                        e,
                        "in_scope",
                    ));
                    continue;
                }
            };
            let entries = match fs::read_dir(format!("/proc/self/fd/{}", dir.as_raw_fd())) {
                Ok(e) => e,
                Err(e) => {
                    events.push(WalkEvent::failed(
                        Some(rel),
                        "read_directory",
                        e,
                        "in_scope",
                    ));
                    continue;
                }
            };
            if faults.entries.contains(&rel) {
                events.push(WalkEvent::failed(
                    Some(rel.clone()),
                    "read_entry",
                    std::io::Error::other("injected entry failure"),
                    "unknown",
                ));
            }
            for entry in entries {
                entries_seen = entries_seen.saturating_add(1);
                if snapshot
                    .as_ref()
                    .is_some_and(|s| entries_seen > s.max_entries)
                {
                    events.push(WalkEvent::snapshot_limit(&rel));
                    return (files, events);
                }

                if !strict
                    && (events.len() >= 160_000 || files.len() >= 10_000 || stack.len() >= 10_000)
                {
                    events.push(WalkEvent {
                        path: Some(rel.clone()),
                        operation: "enumerate".into(),
                        reason: "limit".into(),
                        scope: "in_scope".into(),
                        descendants_unknown: true,
                        native_name_hex: None,
                        error: Some(Diagnostic::new(
                            ErrorCode::LimitExceeded,
                            "walker entry cap exceeded",
                        )),
                    });
                    files.sort();
                    return (files, events);
                }
                let entry = match entry {
                    Ok(e) => e,
                    Err(e) => {
                        events.push(WalkEvent::failed(
                            Some(rel.clone()),
                            "read_entry",
                            e,
                            "unknown",
                        ));
                        continue;
                    }
                };
                let native = entry.file_name();
                path_bytes = path_bytes
                    .saturating_add(rel.len())
                    .saturating_add(usize::from(!rel.is_empty()))
                    .saturating_add(native.as_bytes().len());
                if snapshot
                    .as_ref()
                    .is_some_and(|s| path_bytes > s.max_path_bytes)
                {
                    events.push(WalkEvent::snapshot_limit(&rel));
                    return (files, events);
                }

                let Some(name) = native.to_str() else {
                    let mut e = WalkEvent::excluded(&rel, "non_utf8", true, "unknown");
                    match entry.file_type() {
                        Ok(kind) => e.descendants_unknown = kind.is_dir() || kind.is_symlink(),
                        Err(error) => {
                            e.operation = "file_type".into();
                            e.error = Some(io(ErrorCode::Io, "native entry file type", error));
                        }
                    }
                    e.native_name_hex = Some(
                        native
                            .as_bytes()
                            .iter()
                            .map(|b| format!("{b:02x}"))
                            .collect(),
                    );
                    events.push(e);
                    continue;
                };
                let child = if rel.is_empty() {
                    name.into()
                } else {
                    format!("{rel}/{name}")
                };
                // Strict discovery skips before stat; snapshot mode also observes
                // archive reachability of these excluded entries.
                if strict && let Some(reason) = skipped_component(name) {
                    if let Some(policy) = &snapshot {
                        let event = WalkEvent::excluded(&child, reason, true, "policy_excluded");
                        if event.matches(policy.archives, &child) {
                            events.push(event);
                        }
                    }
                    continue;
                }
                let kind = if faults.types.contains(&child) {
                    Err(std::io::Error::other("injected file type failure"))
                } else {
                    entry.file_type()
                };
                let kind = match kind {
                    Ok(k) => k,
                    Err(e) => {
                        events.push(WalkEvent::failed(
                            Some(child.clone()),
                            "file_type",
                            e,
                            if reachable(&child)
                                || (paths.is_none_or(|p| p.contains(&child))
                                    && globs.iter().any(|g| crate::schema::glob(g, &child)))
                            {
                                "in_scope"
                            } else {
                                "out_of_scope"
                            },
                        ));
                        continue;
                    }
                };
                let reason = if let Some(reason) = skipped_component(name) {
                    Some(reason)
                } else if kind.is_symlink() {
                    Some("symlink")
                } else if !kind.is_dir() && !kind.is_file() {
                    Some("nonregular")
                } else {
                    None
                };
                if let Some(reason) = reason {
                    // A symlink is never followed, so whatever it names is unknown.
                    events.push(WalkEvent::excluded(
                        &child,
                        reason,
                        kind.is_dir() || kind.is_symlink(),
                        "policy_excluded",
                    ));
                    continue;
                }
                if kind.is_dir() {
                    stack.push(child)
                } else if (strict || crate::analytics::safe_document_path(&child))
                    && globs.iter().any(|g| crate::schema::glob(g, &child))
                    && paths.is_none_or(|p| p.contains(&child))
                {
                    files.push(child)
                }
            }
        }
        files.sort();
        events.sort_by_key(|e| serde_json::to_string(e).expect("event JSON"));
        (files, events)
    }

    /// Atomically publish an immutable, bounded bundle under .rivet/exports.
    /// Every component is opened relative to held descriptors, without symlinks.
    pub fn publish_bundle(
        &self,
        name: &str,
        members: &[(String, Vec<u8>)],
    ) -> Result<(), Diagnostic> {
        if !crate::analytics::plain(name) {
            return Err(Diagnostic::new(
                ErrorCode::PathRejected,
                "bundle name must be plain",
            ));
        }
        let err = |e| io(ErrorCode::Io, "bundle", e);
        let rivet = open_at(
            self.fd.as_raw_fd(),
            &CString::new(".rivet").unwrap(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW,
            0,
        )
        .map_err(err)?;
        let mkdir = |fd: RawFd, name: &CStr| -> Result<(), Diagnostic> {
            // SAFETY: held directory descriptor and a terminated component.
            let ret = unsafe { libc::mkdirat(fd, name.as_ptr(), 0o700) };
            if ret != 0 && std::io::Error::last_os_error().raw_os_error() != Some(libc::EEXIST) {
                return Err(err(std::io::Error::last_os_error()));
            }
            Ok(())
        };
        let exports = CString::new("exports").unwrap();
        mkdir(rivet.as_raw_fd(), &exports)?;
        let exports = open_at(
            rivet.as_raw_fd(),
            &exports,
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW,
            0,
        )
        .map_err(err)?;
        let target = CString::new(name).unwrap();
        let temp = temp_name(&target);
        mkdir(exports.as_raw_fd(), &temp)?;
        let dir = open_at(
            exports.as_raw_fd(),
            &temp,
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW,
            0,
        )
        .map_err(err)?;
        for (path, bytes) in members {
            let parts: Vec<_> = path.split('/').collect();
            if parts
                .iter()
                .any(|p| p.is_empty() || *p == "." || *p == ".." || p.contains('\0'))
            {
                return Err(Diagnostic::new(
                    ErrorCode::PathRejected,
                    "invalid bundle member",
                ));
            }
            let mut parent = dir.try_clone().map_err(err)?;
            for part in &parts[..parts.len() - 1] {
                let part = CString::new(*part).unwrap();
                mkdir(parent.as_raw_fd(), &part)?;
                parent = open_at(
                    parent.as_raw_fd(),
                    &part,
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW,
                    0,
                )
                .map_err(err)?;
            }
            let part = CString::new(*parts.last().unwrap()).unwrap();
            let mut file = File::from(
                open_at(
                    parent.as_raw_fd(),
                    &part,
                    libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW,
                    0o600,
                )
                .map_err(err)?,
            );
            file.write_all(bytes)
                .and_then(|_| file.sync_all())
                .map_err(err)?;
            File::from(parent).sync_all().map_err(err)?;
        }
        File::from(dir).sync_all().map_err(err)?;
        // SAFETY: source and destination are components in the held export directory;
        // NOREPLACE prevents overwriting a previous immutable capture.
        let ret = unsafe {
            libc::renameat2(
                exports.as_raw_fd(),
                temp.as_ptr(),
                exports.as_raw_fd(),
                target.as_ptr(),
                libc::RENAME_NOREPLACE,
            )
        };
        check(ret).map_err(err)?;
        File::from(exports).sync_all().map_err(|e| {
            io(
                ErrorCode::CommitUncertain,
                "bundle published but directory sync failed",
                e,
            )
        })?;
        Ok(())
    }

    pub fn read(&self, rel: &str) -> Result<String, Diagnostic> {
        let loc = self.locate(rel)?;
        read_at(&loc, rel)
    }

    /// Raw bytes and permission bits of a document, through the same
    /// confinement as every read; `None` when it does not exist.
    pub fn read_raw(&self, rel: &str) -> Result<Option<(Vec<u8>, u32)>, Diagnostic> {
        let loc = match self.locate(rel) {
            Ok(loc) => loc,
            Err(e) if e.code == ErrorCode::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        let file =
            open_read(loc.dir.as_raw_fd(), &loc.name).map_err(|e| io(ErrorCode::Io, rel, e))?;
        let (bytes, meta) = read_regular(file, MAX_FILE_BYTES).map_err(|e| {
            let nonregular = matches!(e, ReadFailure::NotRegular);
            let d = e.diagnostic(
                rel,
                "not a regular file".into(),
                format!("{rel:?} is larger than {MAX_FILE_BYTES} bytes"),
            );
            if nonregular { d.with_path(rel) } else { d }
        })?;
        Ok(Some((bytes, meta.permissions().mode())))
    }

    pub fn lock(&self, timeout: Duration) -> Result<WriterLock, Diagnostic> {
        let fail = |e| io(ErrorCode::Io, "open .rivet/lock", e);
        let rivet = open_at(
            self.fd.as_raw_fd(),
            &CString::new(".rivet").expect("static"),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW,
            0,
        )
        .map_err(fail)?;
        let file = File::from(
            open_at(
                rivet.as_raw_fd(),
                &CString::new("lock").expect("static"),
                libc::O_RDWR | libc::O_CREAT | libc::O_NOFOLLOW,
                0o644,
            )
            .map_err(fail)?,
        );
        let deadline = Instant::now() + timeout;
        loop {
            match file.try_lock() {
                Ok(()) => return Ok(WriterLock { _file: file }),
                Err(fs::TryLockError::WouldBlock) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(fs::TryLockError::WouldBlock) => {
                    return Err(Diagnostic::new(
                        ErrorCode::LockTimeout,
                        "another Rivet writer holds the project lock",
                    ));
                }
                Err(fs::TryLockError::Error(e)) => return Err(io(ErrorCode::Io, "lock", e)),
            }
        }
    }

    /// Replace `rel` with `candidate` while holding the writer lock. `expected`
    /// is what the caller read under that lock; a difference means an outside
    /// writer changed the file and nothing is written.
    pub fn replace(
        &self,
        _lock: &WriterLock,
        rel: &str,
        expected: &str,
        candidate: &str,
    ) -> Result<Commit, Diagnostic> {
        let loc = self.locate(rel)?;
        let dir = loc.dir.as_raw_fd();
        let target = File::from(
            open_at(dir, &loc.name, libc::O_RDONLY | libc::O_NOFOLLOW, 0)
                .map_err(|e| io(ErrorCode::Io, rel, e))?,
        );
        let mode = check_metadata(&target, rel)?;
        drop(target);
        let temp = stage_document(&loc, rel, candidate, 0o600, |file| {
            file.set_permissions(fs::Permissions::from_mode(mode))
        })?;
        let unlink = || {
            // SAFETY: valid descriptor and NUL-terminated name.
            unsafe { libc::unlinkat(dir, temp.as_ptr(), 0) };
        };
        // Best-effort outside-edit check; the gap to rename remains.
        match read_at(&loc, rel) {
            Ok(now) if now == expected => {}
            Ok(_) => {
                unlink();
                return Err(Diagnostic::new(
                    ErrorCode::ExternalChange,
                    "the file changed outside Rivet while this write was prepared; nothing written",
                ));
            }
            Err(e) => {
                unlink();
                return Err(e);
            }
        }
        if let Err(e) = hook("rename_ready") {
            unlink();
            return Err(e);
        }
        COMMIT_STARTED.store(true, Ordering::SeqCst);
        let renamed = fault("rename").and_then(|()| {
            // SAFETY: both names are NUL-terminated and relative to one descriptor.
            check(unsafe { libc::renameat(dir, temp.as_ptr(), dir, loc.name.as_ptr()) }).map(drop)
        });
        if let Err(e) = renamed {
            unlink();
            return match read_at(&loc, rel) {
                Ok(now) if now == expected => {
                    COMMIT_STARTED.store(false, Ordering::SeqCst);
                    Err(io(ErrorCode::Io, "rename", e))
                }
                _ => Err(io(
                    ErrorCode::CommitUncertain,
                    "rename reported failure; re-read before retrying",
                    e,
                )),
            };
        }
        Ok(sync_document_directory(dir))
    }
}

impl Project {
    /// Create `rel` holding `candidate` while holding the writer lock. The
    /// parent directory must exist. Publication is `renameat2` with
    /// `RENAME_NOREPLACE`, so a file that appears meanwhile, from any
    /// writer, is never overwritten: the create is refused as
    /// `already_exists` with nothing written.
    pub fn create(
        &self,
        _lock: &WriterLock,
        rel: &str,
        candidate: &str,
    ) -> Result<Commit, Diagnostic> {
        let loc = self.locate_new(rel)?;
        let dir = loc.dir.as_raw_fd();
        let temp = stage_document(&loc, rel, candidate, 0o644, |_| Ok(()))?;
        let unlink = || {
            // SAFETY: valid descriptor and NUL-terminated name.
            unsafe { libc::unlinkat(dir, temp.as_ptr(), 0) };
        };
        COMMIT_STARTED.store(true, Ordering::SeqCst);
        let renamed = fault("rename").and_then(|()| {
            // SAFETY: both names are NUL-terminated and relative to one descriptor.
            check(unsafe {
                libc::renameat2(
                    dir,
                    temp.as_ptr(),
                    dir,
                    loc.name.as_ptr(),
                    libc::RENAME_NOREPLACE,
                )
            })
            .map(drop)
        });
        if let Err(e) = renamed {
            unlink();
            // EEXIST: the temporary file never took the name, and whatever is
            // there is someone else's. Otherwise the name is checked: still
            // absent means nothing of this write is visible.
            let absent = e.raw_os_error() == Some(libc::EEXIST)
                || stat_at(dir, &loc.name).is_err_and(|s| s.raw_os_error() == Some(libc::ENOENT));
            if !absent && e.raw_os_error() != Some(libc::EEXIST) {
                return Err(io(
                    ErrorCode::CommitUncertain,
                    "rename reported failure; re-read before retrying",
                    e,
                ));
            }
            COMMIT_STARTED.store(false, Ordering::SeqCst);
            return Err(if e.raw_os_error() == Some(libc::EEXIST) {
                Diagnostic::new(
                    ErrorCode::AlreadyExists,
                    format!("{rel:?} appeared while this create was prepared; nothing written"),
                )
            } else {
                io(ErrorCode::Io, "rename", e)
            });
        }
        Ok(sync_document_directory(dir))
    }
}

/// Write and sync a candidate, then check the editor lock before publication.
/// Callers choose the creation mode and any permission adjustment before writing.
/// On success they own cleanup of the returned temporary name.
fn stage_document(
    loc: &Located,
    rel: &str,
    candidate: &str,
    mode: libc::c_uint,
    permissions: impl FnOnce(&File) -> std::io::Result<()>,
) -> Result<CString, Diagnostic> {
    let dir = loc.dir.as_raw_fd();
    let temp = temp_name(&loc.name);
    let written = (|| {
        let mut file = File::from(open_at(
            dir,
            &temp,
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW,
            mode,
        )?);
        permissions(&file)?;
        file.write_all(candidate.as_bytes())?;
        file.sync_all()
    })();
    let ready = written
        .map_err(|e| io(ErrorCode::Io, "write temporary file", e))
        .and_then(|()| hook("before_rename"))
        // An Emacs buffer with unsaved changes would later save its stale
        // copy over this write, including when the file does not exist yet.
        .and_then(|()| crate::editor_lock::check(dir, &loc.name, rel));
    if let Err(e) = ready {
        // SAFETY: valid descriptor and NUL-terminated name.
        unsafe { libc::unlinkat(dir, temp.as_ptr(), 0) };
        return Err(e);
    }
    Ok(temp)
}

/// The new bytes are visible; a sync failure cannot be retry-safe.
fn sync_document_directory(dir: RawFd) -> Commit {
    let _ = hook("after_rename");
    // SAFETY: fsync on an owned, open directory descriptor.
    let synced = fault("dir_sync").and_then(|()| check(unsafe { libc::fsync(dir) }).map(drop));
    match synced {
        Ok(()) => Commit::Applied,
        Err(_) => Commit::AppliedNotDurable,
    }
}

/// Traverse a fixed descriptor snapshot in bounded blocks, newest first.
fn reverse_log_chunks(
    file: &File,
    mut end: u64,
    chunk_bytes: usize,
    mut visit: impl FnMut(u64, &[u8]) -> Result<bool, ReadFailure>,
) -> Result<(), ReadFailure> {
    use std::os::unix::fs::FileExt;
    let mut chunk = vec![0; chunk_bytes];
    while end > 0 {
        let count = end.min(chunk_bytes as u64) as usize;
        end -= count as u64;
        file.read_exact_at(&mut chunk[..count], end)
            .map_err(ReadFailure::Io)?;
        if !visit(end, &chunk[..count])? {
            break;
        }
    }
    Ok(())
}

/// The offset after the last newline, or zero for an entirely partial log.
fn log_complete_len(file: &File, len: u64, chunk_bytes: usize) -> std::io::Result<u64> {
    let mut keep = 0;
    reverse_log_chunks(file, len, chunk_bytes, |offset, chunk| {
        if let Some(i) = chunk.iter().rposition(|b| *b == b'\n') {
            keep = offset + i as u64 + 1;
            return Ok(false);
        }
        Ok(true)
    })
    .map_err(|e| match e {
        ReadFailure::Io(e) => e,
        _ => unreachable!("block traversal only fails on I/O"),
    })?;
    Ok(keep)
}

fn visit_log_reverse(
    file: &File,
    len: u64,
    chunk_bytes: usize,
    line_limit: u64,
    mut visit: impl FnMut(&[u8]) -> bool,
) -> Result<(), ReadFailure> {
    let mut complete = false;
    let mut stopped = false;
    let mut line = vec![];
    reverse_log_chunks(file, len, chunk_bytes, |_, chunk| {
        for &byte in chunk.iter().rev() {
            if !complete {
                // The final unterminated suffix is never a receipt, however long.
                complete = byte == b'\n';
            } else if byte == b'\n' {
                line.reverse();
                if !visit(&line) {
                    stopped = true;
                    return Ok(false);
                }
                line.clear();
            } else {
                if line.len() as u64 >= line_limit {
                    return Err(ReadFailure::TooLarge);
                }
                line.push(byte);
            }
        }
        Ok(true)
    })?;
    if complete && !stopped && !line.is_empty() {
        line.reverse();
        visit(&line);
    }
    Ok(())
}

/// All descriptor-relative reads refuse symlinks and avoid blocking on special files.
pub(crate) fn open_read(dir: RawFd, name: &CStr) -> std::io::Result<File> {
    open_at(
        dir,
        name,
        libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK,
        0,
    )
    .map(File::from)
}

#[derive(Debug)]
pub(crate) enum ReadFailure {
    Io(std::io::Error),
    NotRegular,
    TooLarge,
}

impl ReadFailure {
    fn diagnostic(self, path: &str, not_regular: String, too_large: String) -> Diagnostic {
        match self {
            Self::Io(e) => io(ErrorCode::Io, path, e),
            Self::NotRegular => Diagnostic::new(ErrorCode::PathRejected, not_regular),
            Self::TooLarge => Diagnostic::new(ErrorCode::LimitExceeded, too_large),
        }
    }
}

/// Check the opened descriptor, then capture at most cap + 1 bytes to detect overflow.
pub(crate) fn read_regular(file: File, cap: u64) -> Result<(Vec<u8>, fs::Metadata), ReadFailure> {
    let meta = file.metadata().map_err(ReadFailure::Io)?;
    if !meta.is_file() {
        return Err(ReadFailure::NotRegular);
    }
    let mut bytes = vec![];
    file.take(cap.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(ReadFailure::Io)?;
    if bytes.len() as u64 > cap {
        return Err(ReadFailure::TooLarge);
    }
    Ok((bytes, meta))
}

fn read_at(loc: &Located, rel: &str) -> Result<String, Diagnostic> {
    let file = open_read(loc.dir.as_raw_fd(), &loc.name).map_err(|e| io(ErrorCode::Io, rel, e))?;
    let (bytes, _) = read_regular(file, MAX_FILE_BYTES).map_err(|e| {
        e.diagnostic(
            rel,
            "not a regular file".into(),
            format!("{rel:?} is larger than {MAX_FILE_BYTES} bytes"),
        )
    })?;
    String::from_utf8(bytes).map_err(|_| {
        Diagnostic::new(
            ErrorCode::UnsupportedSyntax,
            format!("{rel:?} is not UTF-8"),
        )
    })
}

/// Refuse writes whose replacement could not preserve the file's identity.
/// Returns the permission bits to carry over.
fn check_metadata(file: &File, rel: &str) -> Result<u32, Diagnostic> {
    use std::os::unix::fs::MetadataExt;
    let refuse = |why: &str| Diagnostic::new(ErrorCode::PathRejected, format!("{rel:?}: {why}"));
    let meta = file.metadata().map_err(|e| io(ErrorCode::Io, rel, e))?;
    if meta.nlink() != 1 {
        return Err(refuse("hard-linked files are not written"));
    }
    // SAFETY: geteuid has no preconditions.
    if meta.uid() != unsafe { libc::geteuid() } {
        return Err(refuse(
            "owned by another user; replacement would change ownership",
        ));
    }
    if meta.mode() & 0o7000 != 0 {
        return Err(refuse("setuid, setgid or sticky bits are not preserved"));
    }
    // SAFETY: a null buffer with size 0 asks only for the list length.
    let len = unsafe { libc::flistxattr(file.as_raw_fd(), std::ptr::null_mut(), 0) };
    if len > 0 {
        return Err(refuse("extended attributes or ACLs are not preserved"));
    }
    Ok(meta.mode() & 0o777)
}

fn temp_name(target: &CStr) -> CString {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let name = String::from_utf8_lossy(target.to_bytes());
    CString::new(format!(
        ".{name}.rivet-{}-{nanos}-{}.tmp",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ))
    .expect("the target name has no NUL")
}

/// The initial pid namespace's inode number, fixed by the kernel
/// (`PROC_PID_INIT_INO`).
const INITIAL_PID_NAMESPACE: &str = "pid:[4026531836]";

/// Whether this process sees every pid on the host. In a child pid
/// namespace, as a sandbox makes, `ESRCH` proves nothing about a process
/// outside it. Unreadable counts as a child.
pub(crate) fn initial_pid_namespace() -> bool {
    std::fs::read_link("/proc/self/ns/pid")
        .is_ok_and(|ns| ns.as_os_str().as_encoded_bytes() == INITIAL_PID_NAMESPACE.as_bytes())
}

/// Debug-build test hook: `RIVET_TEST_PAUSE=<point>[,<point>...]:<dir>`
/// writes `<dir>/<point>.reached` and waits for `<dir>/<point>.go`.
/// `<point>@<n>` pauses only at this process's n-th call at that point, with
/// `<point>@<n>` in the file names.
pub fn hook(point: &str) -> Result<(), Diagnostic> {
    hook_with(point, &std::process::id().to_string())
}

/// Whether this call is the one a `<point>` or `<point>@<n>` entry selects;
/// counted entries count every call at their point, one counter per table.
fn selected<'a>(
    entries: impl Iterator<Item = &'a str>,
    point: &str,
    calls: &std::sync::Mutex<std::collections::BTreeMap<String, u64>>,
) -> Option<&'a str> {
    let mut counted = vec![];
    for entry in entries {
        match entry.split_once('@') {
            None if entry == point => return Some(entry),
            Some((p, n)) if p == point => {
                counted.extend(n.parse::<u64>().ok().map(|n| (entry, n)));
            }
            _ => {}
        }
    }
    if counted.is_empty() {
        return None;
    }
    let mut calls = calls.lock().unwrap_or_else(|e| e.into_inner());
    let seen = calls.entry(point.to_owned()).or_default();
    *seen += 1;
    let seen = *seen;
    counted
        .into_iter()
        .find(|&(_, n)| n == seen)
        .map(|(entry, _)| entry)
}

/// [`hook`] with the `.reached` file holding `note` instead of this
/// process's id, such as the id of a child it just started.
pub fn hook_with(point: &str, note: &str) -> Result<(), Diagnostic> {
    if !cfg!(debug_assertions) {
        return Ok(());
    }
    static CALLS: std::sync::Mutex<std::collections::BTreeMap<String, u64>> =
        std::sync::Mutex::new(std::collections::BTreeMap::new());
    if let Ok(spec) = std::env::var("RIVET_TEST_PAUSE")
        && let Some((p, dir)) = spec.split_once(':')
        && let Some(point) = selected(p.split(','), point, &CALLS)
    {
        let dir = Path::new(dir);
        let _ = fs::write(dir.join(format!("{point}.reached")), note);
        let deadline = Instant::now() + Duration::from_secs(60);
        while !dir.join(format!("{point}.go")).exists() {
            if Instant::now() > deadline {
                return Err(Diagnostic::new(ErrorCode::Io, "test pause timed out"));
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    Ok(())
}

/// Debug-build test hook: `RIVET_TEST_FAULT=<point>[,<point>...]` fails
/// those syscalls. `<point>@<n>` fails only this process's n-th call at that
/// point (1-based), for a point an earlier call in the same write passes first.
pub(crate) fn fault(point: &str) -> std::io::Result<()> {
    if !cfg!(debug_assertions) {
        return Ok(());
    }
    static CALLS: std::sync::Mutex<std::collections::BTreeMap<String, u64>> =
        std::sync::Mutex::new(std::collections::BTreeMap::new());
    if let Ok(spec) = std::env::var("RIVET_TEST_FAULT")
        && let Some(entry) = selected(spec.split(',').filter(|e| !e.contains('=')), point, &CALLS)
    {
        return Err(std::io::Error::other(format!("injected {entry} fault")));
    }
    Ok(())
}

/// Debug-build test hook for a syscall whose errno is classified:
/// `RIVET_TEST_FAULT=<point>[@<n>]=<ERRNO>` fails that call with a raw OS
/// error (`EINVAL`, `ENOSYS`, `EXDEV`, `EEXIST`, `EBUSY`, `EACCES` or `ENOENT`), counted as
/// [`fault`] counts. An entry without `=<ERRNO>` is invisible here, and one
/// with it is invisible to [`fault`].
pub(crate) fn fault_errno(point: &str) -> std::io::Result<()> {
    if !cfg!(debug_assertions) {
        return Ok(());
    }
    static CALLS: std::sync::Mutex<std::collections::BTreeMap<String, u64>> =
        std::sync::Mutex::new(std::collections::BTreeMap::new());
    let Ok(spec) = std::env::var("RIVET_TEST_FAULT") else {
        return Ok(());
    };
    let entries: Vec<(&str, &str)> = spec.split(',').filter_map(|e| e.split_once('=')).collect();
    let Some(entry) = selected(entries.iter().map(|&(e, _)| e), point, &CALLS) else {
        return Ok(());
    };
    let errno = entries
        .iter()
        .find(|&&(e, _)| e == entry)
        .map_or("", |&(_, n)| n);
    let code = match errno {
        "EINVAL" => libc::EINVAL,
        "ENOSYS" => libc::ENOSYS,
        "EXDEV" => libc::EXDEV,
        "EEXIST" => libc::EEXIST,
        "EBUSY" => libc::EBUSY,
        "EACCES" => libc::EACCES,
        "ENOENT" => libc::ENOENT,
        _ => {
            return Err(std::io::Error::other(format!(
                "unknown injected errno {errno}"
            )));
        }
    };
    Err(std::io::Error::from_raw_os_error(code))
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct WalkEvent {
    pub path: Option<String>,
    pub operation: String,
    pub reason: String,
    pub scope: String,
    pub descendants_unknown: bool,
    pub native_name_hex: Option<String>,
    pub error: Option<Diagnostic>,
}
impl WalkEvent {
    fn snapshot_limit(path: &str) -> Self {
        let mut e = Self::excluded(path, "limit", true, "in_scope");
        e.error = Some(Diagnostic::new(
            ErrorCode::PolicyConflict,
            "snapshot traversal exceeds entry or native-path byte cap",
        ));
        e
    }

    fn strict_diagnostic(&self, globs: &[String]) -> Option<Diagnostic> {
        (self.is_gap(globs)
            || matches!(self.reason.as_str(), "symlink" | "nonregular")
                && self.matches(globs, self.path.as_deref().unwrap_or(""))
            || self.reason == "non_utf8" && self.native_may_match_with(globs, true))
        .then(|| self.diagnostic())
    }

    /// A git path can contain an undecodable component even when the entry no
    /// longer exists in the tree. Keep the last known UTF-8 parent and use the
    /// same native-byte namespace test as working-tree observations.
    pub(crate) fn native_path(path: &[u8]) -> Self {
        let parts: Vec<_> = path.split(|b| *b == b'/').collect();
        let bad = parts
            .iter()
            .position(|p| std::str::from_utf8(p).is_err())
            .expect("unrepresentable git path");
        let parent = parts[..bad]
            .iter()
            .map(|p| std::str::from_utf8(p).expect("valid parent"))
            .collect::<Vec<_>>()
            .join("/");
        let suffix = &path[parent.len() + usize::from(!parent.is_empty())..];
        let mut event = Self::excluded(&parent, "non_utf8", false, "unknown");
        event.native_name_hex = Some(suffix.iter().map(|b| format!("{b:02x}")).collect());
        event
    }

    fn excluded(path: &str, reason: &str, directory: bool, scope: &str) -> Self {
        Self {
            path: Some(path.into()),
            operation: "exclude".into(),
            reason: reason.into(),
            scope: scope.into(),
            descendants_unknown: directory,
            native_name_hex: None,
            error: None,
        }
    }
    fn failed(path: Option<String>, operation: &str, error: std::io::Error, scope: &str) -> Self {
        Self {
            path,
            operation: operation.into(),
            reason: "io".into(),
            scope: scope.into(),
            descendants_unknown: true,
            native_name_hex: None,
            error: Some(io(ErrorCode::Io, operation, error)),
        }
    }

    /// Does this observation leave the given document namespace incomplete?
    /// Re-evaluate relevance, since one union walk can serve several scopes.
    pub fn is_gap(&self, globs: &[String]) -> bool {
        if !matches!(self.scope.as_str(), "in_scope" | "unknown") {
            return false;
        }
        if self.reason == "limit" {
            // A global cutoff can leave any pending prefix unvisited.
            return true;
        }
        if self.native_name_hex.is_some() {
            return self.native_may_match(globs);
        }
        self.path
            .as_deref()
            .is_none_or(|path| self.matches(globs, path))
    }

    fn matches(&self, globs: &[String], path: &str) -> bool {
        globs.iter().any(|g| {
            crate::schema::glob(g, path) || self.descendants_unknown && glob_reachable(g, path)
        })
    }

    /// Policy exclusions that would otherwise be reachable collection inputs.
    pub fn is_excluded(&self, globs: &[String]) -> bool {
        let path = self.path.as_deref().unwrap_or("");
        self.scope == "policy_excluded"
            && path != ".rivet"
            && path.rsplit('/').next() != Some(".git")
            && self.matches(globs, path)
    }

    /// Preserve collection discovery's diagnostics without treating policy
    /// exclusions as coverage gaps.
    pub fn document_diagnostic(&self, globs: &[String]) -> Option<Diagnostic> {
        (self.is_gap(globs) || self.is_excluded(globs)).then(|| self.diagnostic())
    }

    fn diagnostic(&self) -> Diagnostic {
        let path = self.path.as_deref().unwrap_or("");
        let mut d = self.error.clone().unwrap_or_else(|| {
            let message = if let Some(hex) = &self.native_name_hex {
                format!("entry with non-UTF-8 name (hex {hex}) cannot be matched against bindings")
            } else {
                format!(
                    "{} entry is not enumerated by traversal policy",
                    self.reason
                )
            };
            Diagnostic::new(ErrorCode::PathRejected, message)
        });
        if d.path.is_none() {
            d = d.with_path(path);
        }
        d
    }

    /// Whether an undecodable entry in `self.path` could be, or contain, a document
    /// selected by a glob. Matching runs on the native bytes, so a name is never
    /// guessed at through a lossy conversion; an entry of unknown type is both.
    fn native_may_match(&self, globs: &[String]) -> bool {
        self.native_may_match_with(globs, false)
    }

    pub(crate) fn native_may_match_with(&self, globs: &[String], all_files: bool) -> bool {
        fn walk(p: &[&[u8]], s: &[&[u8]], below: bool) -> bool {
            match (p.first(), s.first()) {
                (_, None) if below => !p.is_empty(),
                (_, None) => p.iter().all(|p| *p == b"**"),
                (None, Some(_)) => false,
                (Some(&b"**"), Some(_)) => walk(&p[1..], s, below) || walk(p, &s[1..], below),
                (Some(a), Some(b)) => {
                    crate::schema::glob_segment(a, b) && walk(&p[1..], &s[1..], below)
                }
            }
        }
        let Some(name) = self.native_name_hex.as_deref().and_then(|hex| {
            (0..hex.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(hex.get(i..i + 2)?, 16).ok())
                .collect::<Option<Vec<u8>>>()
        }) else {
            return true;
        };
        let parent = self.path.as_deref().unwrap_or("");
        let mut path: Vec<&[u8]> = parent
            .split('/')
            .filter(|s| !s.is_empty())
            .map(str::as_bytes)
            .collect();
        // A git observation retains the entire suffix after the first native
        // component; a directory observation carries just its entry name.
        path.extend(name.split(|b| *b == b'/'));
        // The event does not say whether a directory-like entry is a real directory
        // or an unfollowed symlink, so a Markdown-named entry is always a candidate.
        let maybe_dir = self.descendants_unknown;
        let maybe_file = all_files || name.ends_with(b".md") || name.ends_with(b".markdown");
        globs.iter().any(|g| {
            let p: Vec<&[u8]> = g.split('/').map(str::as_bytes).collect();
            maybe_file && walk(&p, &path, false) || maybe_dir && walk(&p, &path, true)
        })
    }
}
struct SnapshotWalk<'a> {
    archives: &'a [String],
    max_entries: usize,
    max_path_bytes: usize,
}

#[derive(Default)]
pub struct WalkFaults {
    pub directories: std::collections::BTreeSet<String>,
    pub entries: std::collections::BTreeSet<String>,
    pub types: std::collections::BTreeSet<String>,
}
const SKIPPED_COMPONENTS: [(&str, &str); 3] = [
    (".", "hidden"),
    ("target", "target"),
    ("node_modules", "node_modules"),
];

/// The namespace excluded by every document traversal and literal binding.
pub fn skipped_component(name: &str) -> Option<&'static str> {
    SKIPPED_COMPONENTS.iter().find_map(|(component, reason)| {
        (if *component == "." {
            name.starts_with('.')
        } else {
            name == *component
        })
        .then_some(*reason)
    })
}

/// Report the same exclusions used by traversal, followed by entry-kind exclusions.
pub(crate) fn traversal_exclusions() -> Vec<&'static str> {
    SKIPPED_COMPONENTS
        .iter()
        .filter_map(|(name, _)| skipped_component(name))
        .chain(["symlink", "nonregular", "non_utf8"])
        .collect()
}

/// Whether a glob can reach a file below a directory prefix. This is a
/// conservative prefix automaton: false proves exclusion, true permits descent.
pub fn glob_reachable(pattern: &str, dir: &str) -> bool {
    fn walk(p: &[&str], s: &[&str]) -> bool {
        if s.is_empty() {
            return !p.is_empty();
        }
        match p.first() {
            None => false,
            Some(&"**") => walk(&p[1..], s) || walk(p, &s[1..]),
            Some(segment) => crate::schema::glob(segment, s[0]) && walk(&p[1..], &s[1..]),
        }
    }
    let p: Vec<_> = pattern.split('/').collect();
    let s: Vec<_> = dir.split('/').filter(|s| !s.is_empty()).collect();
    walk(&p, &s)
}

#[cfg(test)]
mod read_tests {
    use super::*;

    #[test]
    fn backwards_log_reads_and_truncation_match_complete_prefix_at_every_boundary() {
        let root = crate::test_support::scratch("reverse-log");
        let path = root.join("log");
        for content in [
            b"".as_slice(),
            b"partial",
            b"\n",
            b"abc\n",
            b"abc\nxyz\n",
            b"a\n\nbc\ndef\npartial",
            b"a\n\nbc\ndef\n",
        ] {
            fs::write(&path, content).unwrap();
            let file = File::open(&path).unwrap();
            let keep = content
                .iter()
                .rposition(|b| *b == b'\n')
                .map_or(0, |i| i + 1);
            let expected: Vec<_> = content[..keep]
                .split(|b| *b == b'\n')
                .filter(|l| !l.is_empty())
                .rev()
                .map(<[u8]>::to_vec)
                .collect();
            for chunk in 1..=8 {
                let mut lines = vec![];
                visit_log_reverse(&file, content.len() as u64, chunk, 3, |line| {
                    if !line.is_empty() {
                        lines.push(line.to_vec());
                    }
                    true
                })
                .unwrap();
                assert_eq!(lines, expected, "content={content:?}, chunk={chunk}");
                assert_eq!(
                    log_complete_len(&file, content.len() as u64, chunk).unwrap(),
                    keep as u64
                );
                assert_eq!(fs::read(&path).unwrap(), content);
            }
        }
        // A suffix may exceed the line cap, since it is discarded, not parsed.
        let content = format!("abc\n{}", "x".repeat(16000));
        fs::write(&path, &content).unwrap();
        let file = File::open(&path).unwrap();
        let mut lines = vec![];
        visit_log_reverse(&file, content.len() as u64, 7, 3, |line| {
            lines.push(line.to_vec());
            true
        })
        .unwrap();
        assert_eq!(lines, [b"abc".to_vec()]);
        assert_eq!(log_complete_len(&file, content.len() as u64, 7).unwrap(), 4);
        fs::write(&path, b"abcd\nok\n").unwrap();
        let file = File::open(&path).unwrap();
        assert!(matches!(
            visit_log_reverse(&file, 8, 2, 3, |_| true),
            Err(ReadFailure::TooLarge)
        ));
        // Finding the selected terminal permits stopping before an older long line.
        let mut seen = vec![];
        visit_log_reverse(&file, 8, 2, 3, |line| {
            seen.push(line.to_vec());
            false
        })
        .unwrap();
        assert_eq!(seen, [b"ok".to_vec()]);
        assert_eq!(fs::read(&path).unwrap(), b"abcd\nok\n");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn log_readers_and_metadata_refuse_special_files_without_writing() {
        let root = crate::test_support::scratch("log-special");
        let p = Project::init(&root).unwrap();
        assert_eq!(p.log_size("missing").unwrap(), 0);
        assert!(p.log_read("missing").unwrap().is_empty());
        p.log_visit_reverse("missing", |_| panic!("missing log has no lines"))
            .unwrap();
        let path = root.join(".rivet/log");
        let target = root.join("target");
        fs::write(&target, b"keep\n").unwrap();
        for kind in ["symlink", "directory", "fifo"] {
            match kind {
                "symlink" => std::os::unix::fs::symlink(&target, &path).unwrap(),
                "directory" => fs::create_dir(&path).unwrap(),
                _ => {
                    let name = CString::new(path.as_os_str().as_bytes()).unwrap();
                    // SAFETY: a terminated synthetic fixture path.
                    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
                }
            }
            assert!(p.log_size("log").is_err(), "{kind}");
            assert!(p.log_read("log").is_err(), "{kind}");
            assert!(
                p.log_visit_reverse("log", |_| panic!("special file read"))
                    .is_err(),
                "{kind}"
            );
            assert_eq!(fs::read(&target).unwrap(), b"keep\n");
            if kind == "directory" {
                fs::remove_dir(&path).unwrap();
            } else {
                fs::remove_file(&path).unwrap();
            }
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn capped_reads_keep_boundaries_permissions_and_refuse_special_files() {
        let root = crate::test_support::scratch("capped-read");
        let p = Project::init(&root).unwrap();
        let path = root.join("bytes");
        let fd = p.root_fd();
        let name = CString::new("bytes").unwrap();
        for (content, cap, ok) in [
            (b"".as_slice(), 0, true),
            (b"abc", 3, true),
            (b"abcd", 3, false),
            (b"a", 0, false),
        ] {
            fs::write(&path, content).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
            let result = read_regular(open_read(fd, &name).unwrap(), cap);
            if ok {
                let (bytes, meta) = result.unwrap();
                assert_eq!(bytes, content);
                assert_eq!(meta.permissions().mode() & 0o777, 0o640);
            } else {
                assert!(matches!(result, Err(ReadFailure::TooLarge)));
            }
            assert_eq!(fs::read(&path).unwrap(), content);
        }
        std::os::unix::fs::symlink(&path, root.join("link")).unwrap();
        assert_eq!(
            open_read(fd, &CString::new("link").unwrap())
                .unwrap_err()
                .raw_os_error(),
            Some(libc::ELOOP)
        );
        fs::create_dir(root.join("directory")).unwrap();
        assert!(matches!(
            read_regular(
                open_read(fd, &CString::new("directory").unwrap()).unwrap(),
                8
            ),
            Err(ReadFailure::NotRegular)
        ));
        let fifo = CString::new(root.join("fifo").as_os_str().as_bytes()).unwrap();
        // SAFETY: a terminated path inside this test fixture.
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        assert!(matches!(
            read_regular(open_read(fd, &CString::new("fifo").unwrap()).unwrap(), 8),
            Err(ReadFailure::NotRegular)
        ));
        fs::remove_dir_all(root).unwrap();
    }
}

#[cfg(test)]
mod snapshot_walk_tests {
    use super::*;

    #[test]
    fn native_archive_relevance_uses_the_entry_not_its_parent_directory() {
        use std::os::unix::ffi::OsStringExt;
        let root = crate::test_support::scratch("snapshot-native-child");
        fs::create_dir_all(root.join("archive/month")).unwrap();
        fs::create_dir(root.join(".rivet")).unwrap();
        fs::write(root.join("archive/a.md"), "# A\n").unwrap();
        fs::write(
            root.join(std::ffi::OsString::from_vec(
                b"archive/month/\xff.txt".to_vec(),
            )),
            "helper",
        )
        .unwrap();
        let project = open_root(&root).unwrap();
        let globs = vec!["archive/**/*.md".into(), "archive/*".into()];
        let archives = vec!["archive/*".into()];
        let (files, errors) = project.snapshot_files(&globs, &archives);
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(files, vec!["archive/a.md"]);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn snapshot_entry_and_native_path_budgets_refuse_incomplete_results() {
        let root = crate::test_support::scratch("snapshot-walk-budget");
        fs::create_dir(root.join("archive")).unwrap();
        fs::write(root.join("archive/a.md"), "").unwrap();
        fs::write(root.join("archive/b.md"), "").unwrap();
        fs::create_dir(root.join(".rivet")).unwrap();
        let project = open_root(&root).unwrap();
        let globs = vec!["archive/**/*".into()];
        let (files, errors) =
            project.snapshot_files_with(&globs, &globs, &WalkFaults::default(), 100, 1024);
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(files.len(), 2);
        for (entries, bytes) in [(2, 1024), (100, 18)] {
            let (_, errors) =
                project.snapshot_files_with(&globs, &globs, &WalkFaults::default(), entries, bytes);
            assert!(
                errors
                    .iter()
                    .any(|e| e.code == ErrorCode::PolicyConflict && e.message.contains("cap")),
                "{errors:?}"
            );
        }
        // Unselected empty files still count before retaining observations.
        fs::write(root.join("archive/helper.txt"), "").unwrap();
        let narrow = vec!["archive/*.md".into()];
        let (_, errors) =
            project.snapshot_files_with(&narrow, &narrow, &WalkFaults::default(), 4, 1024);
        assert!(errors.iter().any(|e| e.code == ErrorCode::PolicyConflict));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn snapshot_archive_directory_failures_refuse_even_as_root() {
        let root = crate::test_support::scratch("snapshot-walk-fault");
        fs::create_dir_all(root.join("archive/subtree")).unwrap();
        fs::create_dir(root.join(".rivet")).unwrap();
        let project = open_root(&root).unwrap();
        let globs = vec!["archive/**/*.md".into()];
        let faults = WalkFaults {
            directories: ["archive/subtree".into()].into(),
            ..Default::default()
        };
        let (_, errors) = project.snapshot_files_with(&globs, &globs, &faults, 100, 1024);
        assert!(
            errors.iter().any(|e| e.code == ErrorCode::PolicyConflict
                && e.path.as_deref() == Some("archive/subtree")),
            "{errors:?}"
        );
        fs::remove_dir_all(root).unwrap();
    }
}

#[cfg(test)]
mod hook_selection_tests {
    use super::*;

    #[test]
    fn counted_entries_select_each_requested_call_and_plain_entries_every_call() {
        let calls = std::sync::Mutex::new(std::collections::BTreeMap::new());
        let spec = "x@1,x@3,y";
        let picked: Vec<_> = (0..4)
            .map(|_| selected(spec.split(','), "x", &calls))
            .collect();
        assert_eq!(picked, [Some("x@1"), None, Some("x@3"), None]);
        assert_eq!(selected(spec.split(','), "y", &calls), Some("y"));
        assert_eq!(selected(spec.split(','), "y", &calls), Some("y"));
        assert_eq!(selected(spec.split(','), "z", &calls), None);
    }
}
