//! Immutable policy inputs. This module discovers bytes, never evaluates queue policy.
use crate::{
    Diagnostic, ErrorCode,
    delete_receipt::hash,
    fs::{Project, WriterLock},
    git::{Entry, Git, ReadBudget},
    schema,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

pub const MAX_SNAPSHOT_BYTES: usize = 64 * 1024 * 1024;

/// The trusted policy reader's independently configured discovery namespace.
/// It must cover the reader's complete input set, including excluded batches.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PruneConfig {
    pub files: Vec<String>,
    #[serde(default)]
    pub inputs: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct InputHash {
    pub sha256: String,
    pub bytes: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Snapshot {
    pub version: u32,
    pub revision: String,
    pub head: String,
    pub live: Vec<String>,
    pub policy: Vec<String>,
    pub archives: Vec<String>,
    pub manifest: BTreeMap<String, InputHash>,
    pub inputs: BTreeMap<String, String>,
}

fn conflict(message: impl Into<String>) -> Diagnostic {
    Diagnostic::new(ErrorCode::PolicyConflict, message)
}

/// A policy refusal names the input that failed, when it is representable.
fn conflict_at(path: &str, message: impl Into<String>) -> Diagnostic {
    conflict(message).with_path(path)
}

fn canonical_policy_path(path: &str) -> Result<(), Diagnostic> {
    crate::delete_receipt::require_canonical(path).map_err(|error| conflict_at(path, error.message))
}

fn utf8(bytes: Vec<u8>, path: &str) -> Result<String, Diagnostic> {
    String::from_utf8(bytes)
        .map_err(|_| conflict_at(path, format!("policy input is not UTF-8: {path}")))
}

fn read(project: &Project, path: &str) -> Result<Vec<u8>, Diagnostic> {
    crate::delete_receipt::require_canonical(path)?;
    if let Some(rest) = path.strip_prefix(".rivet/") {
        project
            .read_config(&rest.split('/').collect::<Vec<_>>())?
            .ok_or_else(|| conflict(format!("missing policy input {path}")))
    } else {
        project.read_bytes(path)
    }
}

fn revision(
    head: &str,
    live: &[String],
    policy: &[String],
    archives: &[String],
    manifest: &BTreeMap<String, InputHash>,
) -> String {
    let bytes = serde_json::to_vec(&(2_u32, head, live, policy, archives, manifest))
        .expect("manifest serialises");
    format!("prune:sha256:{}", hash(&bytes))
}

/// Complete literal directory components are a superset of matching files.
/// One unscoped pattern requires the full tree for the entire union.
fn archive_prefixes(globs: &[String]) -> Option<BTreeSet<String>> {
    globs
        .iter()
        .map(|glob| {
            // Terminal ** components can consume zero segments, so the
            // preceding literal component may itself be a file.
            let glob = glob.trim_end_matches("/**");
            let (directory, _) = glob.rsplit_once('/')?;
            let components: Vec<_> = directory
                .split('/')
                .take_while(|part| !part.is_empty() && !part.contains(['*', '?']))
                .collect();
            if components.is_empty() {
                None
            } else {
                Some(format!("{}/", components.join("/")))
            }
        })
        .collect()
}

/// Read the pinned tree before decoding document names. Git's observed parser
/// reports native names, including entries absent from the working tree.
fn archive_tree(
    git: &Git,
    head: &str,
    globs: &[String],
) -> Result<BTreeMap<String, Entry>, Diagnostic> {
    if globs.is_empty() {
        return Ok(BTreeMap::new());
    }
    let prefixes = archive_prefixes(globs);
    let mut args = vec![
        "--literal-pathspecs",
        "ls-tree",
        "-r",
        "-z",
        "--full-tree",
        head,
    ];
    if let Some(prefixes) = &prefixes {
        args.push("--");
        args.extend(prefixes.iter().map(String::as_str));
    }
    let out = git
        .inspect_limit(&args, 1024 * 1024)
        .map_err(|e| conflict(e.to_string()))?;
    if !out.ok() {
        return Err(conflict(out.failure("git ls-tree")));
    }
    let mut native_gap = None;
    let entries = crate::git::parse_ls_tree_observed(&out.stdout, &mut |path| {
        let event = crate::fs::WalkEvent::native_path(path);
        if native_gap.is_none() && event.native_may_match_with(globs, true) {
            native_gap = event.path;
        }
    })
    .map_err(conflict)?;
    if let Some(parent) = native_gap {
        return Err(conflict_at(
            &parent,
            "committed archive has a relevant non-UTF-8 name",
        ));
    }
    let mut selected = BTreeMap::new();
    for (path, entry) in entries {
        if !schema::matches_file_patterns(globs, &path) {
            continue;
        }
        canonical_policy_path(&path)?;
        if !matches!(entry.mode.as_str(), "100644" | "100755")
            || path
                .split('/')
                .any(|c| crate::fs::skipped_component(c).is_some())
        {
            return Err(conflict_at(
                &path,
                "archive tree entry has unsupported mode or excluded path",
            ));
        }
        selected.insert(path, entry);
    }
    Ok(selected)
}

fn archive_blob(git: &Git, entry: &Entry, path: &str) -> Result<Vec<u8>, Diagnostic> {
    let result = (|| {
        let out = git
            .inspect_limit(&["cat-file", "-s", &entry.oid], 128)
            .map_err(|e| conflict(e.to_string()))?;
        if !out.ok() {
            return Err(conflict(out.failure("git cat-file size")));
        }
        let size = std::str::from_utf8(&out.stdout)
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
            .ok_or_else(|| conflict("invalid archive blob size"))?;
        if size > crate::fs::MAX_FILE_BYTES {
            return Err(conflict("archive exceeds 8 MiB"));
        }
        let out = git
            .inspect_limit(
                &["cat-file", "blob", &entry.oid],
                crate::fs::MAX_FILE_BYTES as usize,
            )
            .map_err(|e| conflict(e.to_string()))?;
        if !out.ok() {
            return Err(conflict(out.failure("git cat-file blob")));
        }
        if out.stdout.len() as u64 != size {
            return Err(conflict("archive blob size changed"));
        }
        Ok(out.stdout)
    })();
    result.map_err(|e| conflict_at(path, e.message))
}

impl Snapshot {
    pub fn verify(&self) -> Result<(), Diagnostic> {
        if self.version != 2
            || self.inputs.len() != self.manifest.len()
            || self.revision
                != revision(
                    &self.head,
                    &self.live,
                    &self.policy,
                    &self.archives,
                    &self.manifest,
                )
        {
            return Err(conflict("invalid snapshot manifest"));
        }
        let mut total = 0usize;
        for (path, expected) in &self.manifest {
            canonical_policy_path(path)?;
            let input = self
                .inputs
                .get(path)
                .ok_or_else(|| conflict_at(path, "snapshot input missing"))?;
            total = total.saturating_add(input.len());
            if expected.bytes != input.len() || expected.sha256 != hash(input.as_bytes()) {
                return Err(conflict_at(
                    path,
                    format!("snapshot input hash mismatch: {path}"),
                ));
            }
        }
        if total > MAX_SNAPSHOT_BYTES {
            return Err(conflict("snapshot exceeds 64 MiB"));
        }
        Ok(())
    }
}

/// The lock serializes cooperating writers across discovery and all input reads.
/// The optional source substitution is recovery-only, after checking actual bytes.
pub(crate) fn capture(
    project: &Project,
    _lock: &WriterLock,
    source: Option<(&str, &str)>,
) -> Result<Snapshot, Diagnostic> {
    crate::commit::pair_barrier(project, None, None, false)?;
    let git = Git::new(project).bounded(ReadBudget::new(MAX_SNAPSHOT_BYTES));
    let (branch, head) = crate::git::head_state(&git).map_err(conflict)?;
    let head = head.ok_or_else(|| conflict("prune snapshot requires an existing Git HEAD"))?;
    let config_bytes = read(project, ".rivet/project.json")?;
    let config: schema::ProjectConfig = serde_json::from_slice(&config_bytes)
        .map_err(|e| conflict(format!("invalid project configuration: {e}")))?;
    let prune = config.prune.as_ref().ok_or_else(|| conflict("prune-snapshot needs configured prune.files naming the policy reader's complete namespace"))?;
    if prune.files.is_empty() {
        return Err(conflict("prune.files must not be empty"));
    }
    let archive_globs: Vec<_> = config
        .collections
        .values()
        .flat_map(|c| c.archive.iter())
        .cloned()
        .collect();
    let globs: Vec<_> = prune
        .files
        .iter()
        .chain(config.collections.values().flat_map(|c| c.files.iter()))
        .chain(archive_globs.iter())
        .cloned()
        .collect();
    let (all, errors) = project.snapshot_files(&globs, &archive_globs);
    if let Some(e) = errors.into_iter().next() {
        return Err(e);
    }
    let live: Vec<_> = all
        .iter()
        .filter(|p| {
            config
                .collections
                .values()
                .any(|c| c.files.iter().any(|g| schema::glob(g, p)))
        })
        .cloned()
        .collect();
    let policy: Vec<_> = all
        .iter()
        .filter(|p| prune.files.iter().any(|g| schema::glob(g, p)))
        .cloned()
        .collect();
    let bound: BTreeSet<_> = live.iter().collect();
    if let Some(path) = policy.iter().find(|p| !bound.contains(p)) {
        return Err(conflict_at(
            path,
            format!("policy source {path} lies outside live bound scope"),
        ));
    }
    let archives: Vec<_> = all
        .iter()
        .filter(|p| schema::matches_file_patterns(&archive_globs, p))
        .cloned()
        .collect();
    let tree = archive_tree(&git, &head, &archive_globs)?;
    // Both sequences are sorted. Report the first differing member, including
    // a trailing addition/removal when their common prefix matches.
    let differing = archives
        .iter()
        .zip(tree.keys())
        .find_map(|(working, committed)| (working != committed).then_some(working.min(committed)))
        .or_else(|| {
            if archives.len() > tree.len() {
                archives.get(tree.len())
            } else {
                tree.keys().nth(archives.len())
            }
        });
    if let Some(path) = differing {
        return Err(conflict_at(
            path,
            "archive membership differs between HEAD and working tree",
        ));
    }
    let mut inputs = BTreeMap::new();
    inputs.insert(
        ".rivet/project.json".into(),
        utf8(config_bytes, ".rivet/project.json")?,
    );
    let mut paths: BTreeSet<String> = live
        .iter()
        .chain(archives.iter())
        .chain(prune.inputs.iter())
        .cloned()
        .collect();
    for (name, collection) in &config.collections {
        let id = schema::schema_id(name, collection)?;
        paths.insert(format!(".rivet/schemas/{id}.json"));
    }
    let mut total = inputs.values().map(String::len).sum::<usize>();
    for path in paths {
        if inputs.contains_key(&path) {
            continue;
        }
        let bytes = match source {
            Some((source_path, bytes)) if source_path == path => bytes.as_bytes().to_vec(),
            _ => read(project, &path).map_err(|e| {
                if tree.contains_key(&path) {
                    conflict_at(&path, e.message)
                } else {
                    e.with_path(&path)
                }
            })?,
        };
        if let Some(entry) = tree.get(&path)
            && archive_blob(&git, entry, &path)? != bytes
        {
            return Err(conflict_at(
                &path,
                "archive bytes differ between HEAD and working tree",
            ));
        }
        total = total.saturating_add(bytes.len());
        if total > MAX_SNAPSHOT_BYTES {
            return Err(conflict("prune-snapshot raw inputs exceed 64 MiB"));
        }
        let text = utf8(bytes, &path)?;
        inputs.insert(path, text);
    }
    for (name, collection) in &config.collections {
        let id = schema::schema_id(name, collection)?;
        schema::bind_bytes(
            name,
            collection.clone(),
            inputs[&format!(".rivet/schemas/{id}.json")].as_bytes(),
        )?;
    }
    let manifest = inputs
        .iter()
        .map(|(p, bytes)| {
            (
                p.clone(),
                InputHash {
                    bytes: bytes.len(),
                    sha256: hash(bytes.as_bytes()),
                },
            )
        })
        .collect();
    let end = crate::git::head_state(&git).map_err(conflict)?;
    if end != (branch, Some(head.clone())) {
        return Err(conflict(
            "HEAD or branch moved while capturing policy inputs",
        ));
    }
    Ok(Snapshot {
        version: 2,
        revision: revision(&head, &live, &policy, &archives, &manifest),
        head,
        live,
        policy,
        archives,
        manifest,
        inputs,
    })
}

pub(crate) fn check(
    project: &Project,
    lock: &WriterLock,
    expected: &str,
    source: Option<(&str, &str)>,
) -> Result<(), Diagnostic> {
    crate::fs::hook("policy_before_read")?;
    let now = capture(project, lock, source)?;
    if now.revision != expected {
        return Err(conflict(
            "policy inputs or membership changed; capture a new bundle and reconsider policy",
        ));
    }
    crate::fs::hook("policy_after_read")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_two_verification_binds_head_archive_membership_and_bytes() {
        let inputs: BTreeMap<String, String> =
            [("archive/a.md".into(), "# Archived\n".to_string())].into();
        let manifest: BTreeMap<_, _> = inputs
            .iter()
            .map(|(path, text)| {
                (
                    path.clone(),
                    InputHash {
                        sha256: hash(text.as_bytes()),
                        bytes: text.len(),
                    },
                )
            })
            .collect();
        let mut bundle = Snapshot {
            version: 2,
            revision: String::new(),
            head: "a".repeat(40),
            live: vec![],
            policy: vec![],
            archives: vec!["archive/a.md".into()],
            manifest,
            inputs,
        };
        bundle.revision = revision(
            &bundle.head,
            &bundle.live,
            &bundle.policy,
            &bundle.archives,
            &bundle.manifest,
        );
        bundle.verify().unwrap();
        for shape in ["noncanonical", "missing"] {
            let mut changed = bundle.clone();
            let expected_path = if shape == "noncanonical" {
                let value = changed.inputs.remove("archive/a.md").unwrap();
                let expected = changed.manifest.remove("archive/a.md").unwrap();
                changed.inputs.insert("archive/./a.md".into(), value);
                changed.manifest.insert("archive/./a.md".into(), expected);
                changed.revision = revision(
                    &changed.head,
                    &changed.live,
                    &changed.policy,
                    &changed.archives,
                    &changed.manifest,
                );
                "archive/./a.md"
            } else {
                let value = changed.inputs.remove("archive/a.md").unwrap();
                changed.inputs.insert("archive/b.md".into(), value);
                "archive/a.md"
            };
            let error = changed.verify().unwrap_err();
            assert_eq!(error.code, ErrorCode::PolicyConflict, "{shape}: {error:?}");
            assert_eq!(
                error.path.as_deref(),
                Some(expected_path),
                "{shape}: {error:?}"
            );
            if shape == "missing" {
                assert_eq!(error.message, "snapshot input missing");
            }
        }
        for field in ["version", "head", "archives", "bytes"] {
            let mut changed = bundle.clone();
            match field {
                "version" => changed.version = 1,
                "head" => changed.head = "b".repeat(40),
                "archives" => changed.archives.clear(),
                "bytes" => {
                    changed
                        .inputs
                        .insert("archive/a.md".into(), "different".into());
                }
                _ => unreachable!(),
            }
            let error = changed.verify().unwrap_err();
            assert_eq!(error.code, ErrorCode::PolicyConflict, "{field}");
            if field == "bytes" {
                assert_eq!(error.path.as_deref(), Some("archive/a.md"));
            }
        }
    }

    #[test]
    fn archive_prefixes_use_only_complete_literal_directories() {
        for (globs, expected) in [
            (
                vec!["archive/*/*.md", "archive/year/**/*.md", "archive/*/a.md"],
                Some(vec!["archive/", "archive/year/"]),
            ),
            (vec!["archive[box]/a.md"], Some(vec!["archive[box]/"])),
            (vec!["archive/month/a.md"], Some(vec!["archive/month/"])),
            (vec!["archive/month/a.md/**"], Some(vec!["archive/month/"])),
            (
                vec!["archive/month/a.md/**/**"],
                Some(vec!["archive/month/"]),
            ),
            (vec!["archive/month/**"], Some(vec!["archive/"])),
            (vec!["archive/**"], None),
            (vec!["archive/year?/a.md"], Some(vec!["archive/"])),
            (vec!["archive/*.md", "a.md"], None),
            (vec!["archive/*.md", "?rchive/a.md"], None),
            (vec!["**/a.md"], None),
        ] {
            let globs: Vec<String> = globs.into_iter().map(str::to_string).collect();
            let expected: Option<BTreeSet<String>> =
                expected.map(|paths| paths.into_iter().map(str::to_string).collect());
            assert_eq!(archive_prefixes(&globs), expected, "{globs:?}");
        }
    }
}
