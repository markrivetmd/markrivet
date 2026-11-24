//! Emacs's file lock, honoured before a write publishes.
//!
//! Emacs marks a buffer with unsaved changes by creating `.#<name>` beside
//! the file: a symlink whose text is `USER@HOST.PID:BOOT`, or a regular file
//! holding the same text where symlinks fail. A write that renames over the
//! file while such a buffer is open leaves Emacs to save its stale copy over
//! the change later, so Rivet refuses while the owner looks live.
//!
//! The owner is parsed as Emacs's own `current_lock_owner` parses it. It is
//! dead only when it is on this host and its pid names no process: out of
//! range, not positive, or `kill(pid, 0)` failing with `ESRCH` while Rivet
//! runs in the initial pid namespace. Everything else is live, including a
//! pid that now names some other process.
//!
//! Emacs also calls a lock dead when its boot field is more than a second
//! from the current boot time. Rivet does not, and does not compare any
//! other clock either: the boot field, the lock's mtime and `/proc/stat`'s
//! boot time are all wall-clock values, which a clock step moves apart
//! (`/proc/stat` follows the step, the others do not), and Emacs's boot
//! source (utmp, a file's mtime) already sits tens of seconds from
//! `/proc/stat` on a systemd host. A stale lock whose pid was reused refuses
//! until someone removes it, which the message says. Anything that cannot
//! be read or parsed is refused, as Emacs itself refuses it.
use crate::fs::{ReadFailure, open_read, read_regular};
use crate::{Diagnostic, ErrorCode};
use std::ffi::{CStr, CString};
use std::os::fd::RawFd;

/// The longest lock text read, as Emacs's `MAX_LFINFO`.
const MAX_LOCK_BYTES: usize = 8 * 1024;

/// A lock's text, parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Owner {
    pub user: String,
    pub host: String,
    /// `None` when the text holds a pid no process can have (out of range).
    pub pid: Option<libc::pid_t>,
}

/// Parse `USER@HOST.PID[:BOOT]` as Emacs does: the user is everything
/// before the last `@`, the host runs to the last `.` after it, and the
/// boot time, when present, is digits after `:` (or after U+F022, which a
/// CIFS client can substitute for it). `None` for anything else.
pub fn parse(text: &[u8]) -> Option<Owner> {
    let at = text.iter().rposition(|b| *b == b'@')?;
    let dot = at + 1 + text[at + 1..].iter().rposition(|b| *b == b'.')?;
    let rest = &text[dot + 1..];
    let digits = rest.iter().take_while(|b| b.is_ascii_digit()).count();
    if digits == 0 {
        return None;
    }
    let boot = match &rest[digits..] {
        [] => &[][..],
        [b':', boot @ ..] => boot,
        [0xef, 0x80, 0xa2, boot @ ..] => boot,
        _ => return None,
    };
    if !rest[digits..].is_empty() && (boot.is_empty() || !boot.iter().all(u8::is_ascii_digit)) {
        return None;
    }
    let pid = std::str::from_utf8(&rest[..digits])
        .ok()?
        .parse::<libc::pid_t>()
        .ok();
    Some(Owner {
        user: String::from_utf8_lossy(&text[..at]).into_owned(),
        host: String::from_utf8_lossy(&text[at + 1..dot]).into_owned(),
        pid,
    })
}

/// Refuse when `.#<name>` in `dir` holds a live Emacs lock, or exists and
/// cannot be read as one. `rel` is the document's request path.
pub(crate) fn check(dir: RawFd, name: &CStr, rel: &str) -> Result<(), Diagnostic> {
    let mut lock_name = b".#".to_vec();
    lock_name.extend_from_slice(name.to_bytes());
    let shown = match rel.rsplit_once('/') {
        Some((parent, _)) => format!("{parent}/{}", String::from_utf8_lossy(&lock_name)),
        None => String::from_utf8_lossy(&lock_name).into_owned(),
    };
    let lock_name = CString::new(lock_name).expect("a document name has no NUL");
    let unreadable = |why: String| {
        Diagnostic::new(
            ErrorCode::EditorLock,
            format!(
                "{shown} is not an Emacs lock Rivet can read ({why}); if no editor has {rel} open, remove it and retry; nothing written"
            ),
        )
    };
    let Some(text) = read_lock(dir, &lock_name).map_err(|e| unreadable(e.to_string()))? else {
        return Ok(());
    };
    let Some(owner) = parse(&text) else {
        return Err(unreadable(format!(
            "text {:?} is not USER@HOST.PID:BOOT",
            String::from_utf8_lossy(&text)
        )));
    };
    let live = if owner.host != host_name() {
        Some(format!("on another host, {}", owner.host))
    } else {
        owner.pid.filter(|pid| *pid > 0).and_then(|pid| {
            if !crate::fs::initial_pid_namespace() {
                Some(format!(
                    "pid {pid} on this host, which this pid namespace cannot see"
                ))
            } else {
                exists(pid).then(|| format!("pid {pid} on this host"))
            }
        })
    };
    match live {
        Some(where_) => Err(Diagnostic::new(
            ErrorCode::EditorLock,
            format!(
                "{rel} has unsaved changes in Emacs ({}@{}, {where_}; lock {shown}); save or revert the buffer there and retry, or, if no Emacs has it open, remove the lock and retry; nothing written",
                owner.user, owner.host
            ),
        )),
        None => Ok(()),
    }
}

/// The lock's text. `None` when there is no lock.
fn read_lock(dir: RawFd, name: &CStr) -> std::io::Result<Option<Vec<u8>>> {
    let gone = |e: &std::io::Error| matches!(e.raw_os_error(), Some(libc::ENOENT | libc::ENOTDIR));
    // SAFETY: a zeroed stat is a valid out-parameter.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: valid descriptor, NUL-terminated name, owned out-parameter.
    if unsafe { libc::fstatat(dir, name.as_ptr(), &mut st, libc::AT_SYMLINK_NOFOLLOW) } != 0 {
        let e = std::io::Error::last_os_error();
        return if gone(&e) { Ok(None) } else { Err(e) };
    }
    let kind = st.st_mode & libc::S_IFMT;
    if kind == libc::S_IFLNK {
        let mut buf = vec![0u8; MAX_LOCK_BYTES + 1];
        // SAFETY: valid descriptor, NUL-terminated name, a buffer of the
        // stated length.
        let n = unsafe {
            libc::readlinkat(
                dir,
                name.as_ptr(),
                buf.as_mut_ptr().cast::<libc::c_char>(),
                buf.len(),
            )
        };
        if n < 0 {
            let e = std::io::Error::last_os_error();
            return if gone(&e) { Ok(None) } else { Err(e) };
        }
        buf.truncate(n as usize);
        return bounded(buf).map(Some);
    }
    if kind != libc::S_IFREG {
        return Err(std::io::Error::other("not a symlink or a regular file"));
    }
    let file = match open_read(dir, name) {
        Ok(file) => file,
        Err(e) if gone(&e) => return Ok(None),
        Err(e) => return Err(e),
    };
    read_regular(file, MAX_LOCK_BYTES as u64)
        .map(|(text, _)| Some(text))
        .map_err(|e| match e {
            ReadFailure::Io(e) => e,
            ReadFailure::NotRegular => std::io::Error::other("not a symlink or a regular file"),
            ReadFailure::TooLarge => {
                std::io::Error::other(format!("longer than {MAX_LOCK_BYTES} bytes"))
            }
        })
}

fn bounded(text: Vec<u8>) -> std::io::Result<Vec<u8>> {
    if text.len() > MAX_LOCK_BYTES {
        return Err(std::io::Error::other(format!(
            "longer than {MAX_LOCK_BYTES} bytes"
        )));
    }
    Ok(text)
}

/// This host's name as Emacs's `system-name` spells it: `gethostname` with
/// spaces and tabs turned into `-`.
fn host_name() -> String {
    let mut buf = [0u8; 256];
    // SAFETY: a buffer of the stated length; the result is NUL-terminated
    // when it fits, and the last byte is never written.
    if unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len() - 1) } != 0 {
        return String::new();
    }
    let len = buf.iter().position(|b| *b == 0).unwrap_or(buf.len());
    String::from_utf8_lossy(&buf[..len]).replace([' ', '\t'], "-")
}

/// Whether a process with `pid` exists: `kill(pid, 0)` succeeds, or is
/// refused with `EPERM` because another user owns it.
fn exists(pid: libc::pid_t) -> bool {
    // SAFETY: signal 0 only checks that the process exists.
    let sent = unsafe { libc::kill(pid, 0) } == 0;
    sent || std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn owner(user: &str, host: &str, pid: Option<libc::pid_t>) -> Option<Owner> {
        Some(Owner {
            user: user.into(),
            host: host.into(),
            pid,
        })
    }

    #[test]
    fn lock_text_parses_as_emacs_reads_it() {
        assert_eq!(
            parse(b"ada@desk.4242:1700000000"),
            owner("ada", "desk", Some(4242))
        );
        // The boot time is optional, the host may hold dots, the user `@`s.
        assert_eq!(parse(b"ada@desk.4242"), owner("ada", "desk", Some(4242)));
        assert_eq!(
            parse(b"a@b@desk.example.org.7:1"),
            owner("a@b", "desk.example.org", Some(7))
        );
        // U+F022 stands in for `:` on CIFS.
        assert_eq!(
            parse("ada@desk.4242\u{f022}1700000000".as_bytes()),
            owner("ada", "desk", Some(4242))
        );
        // A pid out of range is parsed, and names no process.
        assert_eq!(
            parse(b"ada@desk.99999999999999:1"),
            owner("ada", "desk", None)
        );
    }

    #[test]
    fn lock_text_that_emacs_would_not_read_is_none() {
        for text in [
            &b""[..],
            b"no-at-sign.42:1",
            b"ada@nodot",
            b"ada@desk.:1",
            b"ada@desk.x42",
            b"ada@desk.42:",
            b"ada@desk.42:1x",
            b"ada@desk.42;1",
            b"ada@desk.42 ",
        ] {
            assert_eq!(parse(text), None, "{}", String::from_utf8_lossy(text));
        }
    }

    #[test]
    fn this_process_exists_and_an_unallocated_pid_does_not() {
        assert!(exists(std::process::id() as libc::pid_t));
        // pid_max is one more than the largest pid the kernel allocates.
        let max: libc::pid_t = std::fs::read_to_string("/proc/sys/kernel/pid_max")
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert!(!exists(max));
    }
}
