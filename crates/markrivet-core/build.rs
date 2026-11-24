use sha2::{Digest, Sha256};
use std::path::Path;

fn main() {
    // Hash the actual compiled core source, independent of uncommitted Git state.
    let mut paths = vec![
        "Cargo.toml".to_string(),
        "build.rs".to_string(),
        "../../Cargo.toml".to_string(),
        "../../Cargo.lock".to_string(),
        "../../docs/parser-contract.md".to_string(),
    ];
    fn collect(root: &Path, paths: &mut Vec<String>) {
        for entry in std::fs::read_dir(root).expect("core sources") {
            let path = entry.expect("source entry").path();
            if path.is_dir() {
                collect(&path, paths);
            } else if path.extension().is_some_and(|e| e == "rs") {
                paths.push(path.to_str().expect("source path UTF-8").into());
            }
        }
    }
    collect(Path::new("src"), &mut paths);
    paths.sort();
    for path in &paths {
        println!("cargo:rerun-if-changed={path}");
    }
    let hashes = paths
        .iter()
        .map(|path| {
            let bytes = std::fs::read(path).expect("source bytes");
            format!("{:x}", Sha256::digest(bytes))
        })
        .collect::<Vec<_>>();
    // Preserve the entire source manifest as a reproducible build identity.
    println!(
        "cargo:rustc-env=RIVET_BUILD_FINGERPRINT={}",
        hashes.join(".")
    );
}
