//! Fixtures shared by core unit/integration tests and CLI tests.
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

/// Prepare fixture Git with the production KEEP_ENV allowlist and an isolated home.
/// Return the command so tests can also inspect failures or hold a child open.
#[allow(dead_code)] // This module is compiled separately into each test binary.
pub fn git(dir: &Path, home: &Path, args: &[&str]) -> std::process::Command {
    let mut cmd = std::process::Command::new("git");
    // As production pins for its own calls (markrivet_core::git): a fixture commit must
    // not leave a detached auto-maintenance writing `.git` (git 2.56 holds
    // `objects/maintenance.lock`) while a later read-only assertion snapshots it.
    cmd.args(["-c", "gc.auto=0", "-c", "maintenance.auto=false"])
        .args(args)
        .current_dir(dir)
        .env_clear();
    // Keep in sync with markrivet_core::git::KEEP_ENV.
    for key in ["PATH", "HOME", "XDG_CONFIG_HOME", "LANG", "LC_ALL", "TZ"] {
        if let Some(value) = std::env::var_os(key) {
            cmd.env(key, value);
        }
    }
    cmd.env("HOME", home)
        .env("XDG_CONFIG_HOME", home)
        .env("GIT_CONFIG_NOSYSTEM", "1");
    cmd
}

/// Reserve a fresh directory without deleting any previous run's bytes.
pub fn scratch(name: &str) -> PathBuf {
    let parent = option_env!("CARGO_TARGET_TMPDIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    scratch_in(&parent, name)
}

pub fn scratch_in(parent: &Path, name: &str) -> PathBuf {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    std::fs::create_dir_all(parent).unwrap();
    // PIDs and local counters can repeat in sandbox namespaces and separate
    // test binaries. Only create_dir's exclusive reservation proves ownership.
    loop {
        let dir = parent.join(format!(
            "{name}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        match std::fs::create_dir(&dir) {
            Ok(()) => return dir,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => panic!("cannot allocate synthetic fixture: {e}"),
        }
    }
}
