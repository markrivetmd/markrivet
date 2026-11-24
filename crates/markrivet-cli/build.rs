//! Records the commit the binary was built from, for `markrivet --version`. A
//! build whose sources differ from that commit says so with `-dirty`; one
//! built outside a git checkout names no commit.
use std::path::Path;
use std::process::Command;

fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn main() {
    let manifest = std::env::var("CARGO_MANIFEST_DIR").expect("cargo sets it");
    let dir = Path::new(&manifest);
    let workspace = dir.join("../..");
    // The paths `-dirty` is judged on, so it is never stale.
    for path in ["Cargo.toml", "Cargo.lock", "crates"] {
        println!("cargo:rerun-if-changed={}", workspace.join(path).display());
    }
    println!("cargo:rerun-if-changed=build.rs");
    let mut watched = vec![
        "HEAD".to_string(),
        "index".to_string(),
        "packed-refs".into(),
    ];
    if let Some(branch) = git(dir, &["symbolic-ref", "-q", "HEAD"]) {
        watched.push(branch);
    }
    for name in watched {
        if let Some(p) = git(dir, &["rev-parse", "--git-path", &name]) {
            println!("cargo:rerun-if-changed={}", dir.join(p).display());
        }
    }
    let commit = match git(dir, &["rev-parse", "--verify", "HEAD^{commit}"]) {
        Some(sha) => {
            // Untracked files count: a new source file changes the build.
            let clean = git(
                &workspace,
                &[
                    "status",
                    "--porcelain",
                    "--",
                    "Cargo.toml",
                    "Cargo.lock",
                    "crates",
                ],
            )
            .is_some_and(|s| s.is_empty());
            if clean { sha } else { format!("{sha}-dirty") }
        }
        None => "unknown".into(),
    };
    println!("cargo:rustc-env=RIVET_BUILD_COMMIT={commit}");
}
