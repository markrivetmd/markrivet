//! Immutable bounded captured inputs; replay never opens the live Markdown/config.
use super::*;
use crate::fs::{MAX_FILE_BYTES, WalkEvent};

pub const MAX_BYTES: usize = 256 * 1024 * 1024;
pub const MAX_FILES: usize = 10_000;
pub const MAX_MANIFEST: usize = 8 * 1024 * 1024;
pub const MAX_RECORDS: usize = 8 * 1024 * 1024;
const ENGINE: &str = concat!(
    env!("CARGO_PKG_VERSION"),
    "/",
    env!("RIVET_BUILD_FINGERPRINT")
);
const PARSER: &str = "rivet-parser-1/comrak-0.55.0/markstay-0.14.0/yaml-rust2-0.13.0";
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Export {
    pub bundle: String,
    #[serde(default)]
    pub query: Option<Query>,
    #[serde(default)]
    pub view: Option<String>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    version: u32,
    engine: String,
    parser: String,
    query: Query,
    definitions: BTreeMap<String, Vec<u8>>,
    view: Option<String>,
    start: String,
    end: String,
    consistency: String,
    outcomes: Vec<Outcome>,
    events: Vec<WalkEvent>,
    coverage: Coverage,
    records_hash: String,
    fingerprint: String,
}
fn limited<T: Serialize>(value: &T, cap: usize) -> Result<Vec<u8>, Diagnostic> {
    let bytes = serde_json::to_vec(value).map_err(|e| bad(e.to_string()))?;
    if bytes.len() > cap {
        return Err(Diagnostic::new(
            ErrorCode::LimitExceeded,
            "bundle metadata cap exceeded",
        ));
    }
    Ok(bytes)
}
pub fn export(project: &Project, request: Export) -> Result<Value, Diagnostic> {
    if !plain(&request.bundle) {
        return Err(bad("bundle name must be plain"));
    }
    let (q, view) = match (request.query, request.view) {
        (Some(q), None) => (q, None),
        (None, Some(n)) => {
            let (q, b) = load_view(project, &n)?;
            (q, Some((n, b)))
        }
        _ => return Err(bad("export needs exactly one query or view")),
    };
    let ctx = Context::load(
        project,
        &q,
        view.as_ref().map(|(n, b)| (n.as_str(), b.clone())),
    )?;
    let mut c = Collector::new(&q, &ctx, true)?;
    let scan = scan(project, &q, &ctx, &mut c, true)?;
    publish_scan(project, &request.bundle, q, ctx, scan, c)
}

pub(super) fn publish_scan(
    project: &Project,
    name: &str,
    q: Query,
    ctx: Context,
    scan: Scan,
    c: Collector,
) -> Result<Value, Diagnostic> {
    check_budget(
        scan.captures.iter().map(|(_, b)| b.len()),
        scan.outcomes.len(),
    )?;
    let report = render(&q, &ctx, &scan, &c);
    let records = limited(&json!({"rows":c.rows,"report":report}), MAX_RECORDS)?;
    let manifest = Manifest {
        version: CONTRACT,
        engine: ENGINE.into(),
        parser: PARSER.into(),
        query: q,
        definitions: ctx.definitions,
        view: ctx.view,
        start: scan.start,
        end: scan.end,
        consistency: "per_file".into(),
        outcomes: scan.outcomes,
        events: scan.events,
        coverage: scan.coverage,
        records_hash: hash(&records),
        fingerprint: scan.fingerprint,
    };
    let bytes = limited(&manifest, MAX_MANIFEST)?;
    let mut members = scan.captures;
    members.push(("records.json".into(), records));
    members.push(("manifest.json".into(), bytes.clone()));
    let digest = bundle_digest(&bytes, &members);
    members.push(("digest.json".into(), serde_json::to_vec(&digest).unwrap()));
    project.publish_bundle(name, &members)?;
    Ok(
        json!({"status":"read","bundle":name,"bundle_digest":digest,"report":report,"consistency":"per_file","scan_start":manifest.start,"scan_end":manifest.end}),
    )
}
fn bundle_digest(manifest: &[u8], members: &[(String, Vec<u8>)]) -> String {
    let mut hashes: BTreeMap<_, _> = members
        .iter()
        .filter(|(p, _)| p != "manifest.json" && p != "digest.json")
        .map(|(p, b)| (p.clone(), hash(b)))
        .collect();
    hashes.insert("manifest.json".into(), hash(manifest));
    hash(&serde_json::to_vec(&hashes).unwrap())
}
fn member(project: &Project, bundle: &str, path: &str) -> Result<Vec<u8>, Diagnostic> {
    let parts: Vec<_> = path.split('/').collect();
    if parts
        .iter()
        .any(|p| p.is_empty() || *p == "." || *p == ".." || p.contains('\0'))
    {
        return Err(Diagnostic::new(
            ErrorCode::PathRejected,
            "malformed bundle member path",
        ));
    }
    let mut request = vec!["exports", bundle];
    request.extend(parts);
    project
        .read_config(&request)?
        .ok_or_else(|| bad(format!("missing bundle member {path}")))
}
pub(crate) fn captured(
    project: &Project,
    name: &str,
    expected: Option<&Query>,
) -> Result<(Context, Scan, Collector, Value), Diagnostic> {
    if !plain(name) {
        return Err(bad("bundle name must be plain"));
    }
    let bytes = member(project, name, "manifest.json")?;
    let manifest: Manifest =
        serde_json::from_slice(&bytes).map_err(|e| bad(format!("bundle manifest: {e}")))?;
    if manifest.version != CONTRACT
        || manifest.engine != ENGINE
        || manifest.parser != PARSER
        || manifest.consistency != "per_file"
    {
        return Err(bad("incompatible replay contract or engine version"));
    }
    if manifest.outcomes.len() > MAX_FILES || manifest.events.len() > MAX_FILES * 16 {
        return Err(Diagnostic::new(
            ErrorCode::LimitExceeded,
            "bundle file/event cap exceeded",
        ));
    }
    if let Some(expected) = expected
        && serde_json::to_value(expected).unwrap() != serde_json::to_value(&manifest.query).unwrap()
    {
        return Err(bad("drilldown query differs from captured query"));
    }

    let q = &manifest.query;
    let bound = schema::bind_with(&q.collection, |parts| {
        Ok(manifest.definitions.get(&parts.join("/")).cloned())
    })?;
    let declaration = if let Some(n) = &q.analytics {
        if !plain(n) {
            return Err(bad("malformed captured declaration name"));
        }
        let bytes = manifest
            .definitions
            .get(&format!("analytics/{n}.json"))
            .ok_or_else(|| bad("missing captured declaration"))?;
        let d: Declaration = serde_json::from_slice(bytes).map_err(|e| bad(e.to_string()))?;
        d.validate(&q.collection)?;
        Some(d)
    } else {
        None
    };
    if let Some(n) = &manifest.view {
        if !plain(n) {
            return Err(bad("malformed captured view name"));
        }
        let bytes = manifest
            .definitions
            .get(&format!("views/{n}.json"))
            .ok_or_else(|| bad("missing captured view"))?;
        let view: View = serde_json::from_slice(bytes).map_err(|e| bad(e.to_string()))?;
        if view.version != CONTRACT
            || serde_json::to_value(view.query).unwrap() != serde_json::to_value(q).unwrap()
        {
            return Err(bad("captured view differs from query"));
        }
    }
    let ctx = Context {
        bound,
        declaration,
        definitions: manifest.definitions,
        view: manifest.view,
    };
    ctx.check(q)?;
    let mut c = Collector::new(q, &ctx, true)?;
    let mut scan = Scan {
        coverage: Coverage {
            files_discovered: manifest.outcomes.len(),
            ..Coverage::default()
        },
        diagnostics: Diagnostics::default(),
        events: manifest.events,
        outcomes: manifest.outcomes,
        captures: vec![],
        fingerprint: String::new(),
        start: manifest.start,
        end: manifest.end,
    };
    let mut total = 0;
    let mut seen = BTreeSet::new();
    for (i, outcome) in scan.outcomes.iter().enumerate() {
        if !safe_document_path(&outcome.path)
            || !ctx
                .bound
                .config
                .files
                .iter()
                .any(|g| schema::glob(g, &outcome.path))
            || q.paths.as_ref().is_some_and(|p| !p.contains(&outcome.path))
            || !seen.insert(outcome.path.clone())
        {
            return Err(bad(
                "invalid, repeated or out-of-scope captured document path",
            ));
        }
        if i > 0 && scan.outcomes[i - 1].path >= outcome.path {
            return Err(bad("captured paths must be ordered"));
        }
        scan.coverage.reads += 1;
        let result = if let Some(path) = &outcome.member {
            if !path.starts_with("sources/")
                || !path.ends_with(".bin")
                || !path[8..path.len() - 4].bytes().all(|b| b.is_ascii_digit())
                || path[8..path.len() - 4].is_empty()
            {
                return Err(Diagnostic::new(
                    ErrorCode::PathRejected,
                    "invalid source member",
                ));
            }
            let bytes = member(project, name, path)?;
            total += bytes.len();
            if total > MAX_BYTES || bytes.len() as u64 > MAX_FILE_BYTES {
                return Err(Diagnostic::new(
                    ErrorCode::LimitExceeded,
                    "bundle source cap exceeded",
                ));
            }
            if outcome.source_hash.as_deref() != Some(hash(&bytes).as_str()) {
                return Err(bad("captured source hash mismatch"));
            }
            let derived = process(
                &outcome.path,
                Ok(&bytes),
                q,
                &ctx,
                &mut c,
                &mut scan.coverage,
                &mut scan.diagnostics,
            )?;
            scan.captures.push((path.clone(), bytes));
            derived
        } else {
            if outcome.status != "read_failed" || outcome.source_hash.is_some() {
                return Err(bad("complete capture missing for readable source"));
            }
            let e = outcome
                .error
                .clone()
                .ok_or_else(|| bad("failed capture needs an error"))?;
            process(
                &outcome.path,
                Err(e),
                q,
                &ctx,
                &mut c,
                &mut scan.coverage,
                &mut scan.diagnostics,
            )?
        };
        if result != (outcome.status.clone(), outcome.error.clone()) {
            return Err(bad("captured source outcome disagrees with reparsing"));
        }
    }
    finish_scan(q, &ctx, &mut scan)?;
    if scan.coverage != manifest.coverage || scan.fingerprint != manifest.fingerprint {
        return Err(bad("captured coverage or fingerprint mismatch"));
    }
    let records = member(project, name, "records.json")?;
    if records.len() > MAX_RECORDS || hash(&records) != manifest.records_hash {
        return Err(bad("records hash mismatch"));
    }
    let recorded: Value = serde_json::from_slice(&records).map_err(|e| bad(e.to_string()))?;
    if recorded != json!({"rows":c.rows,"report":render(q,&ctx,&scan,&c)}) {
        return Err(bad("captured records disagree with extraction"));
    }
    let mut members = scan.captures.clone();
    members.push(("records.json".into(), records));
    let digest: String = serde_json::from_slice(&member(project, name, "digest.json")?)
        .map_err(|e| bad(e.to_string()))?;
    if digest != bundle_digest(&bytes, &members) {
        return Err(bad("bundle digest mismatch"));
    }
    Ok((ctx, scan, c, recorded["report"].clone()))
}
pub fn replay(project: &Project, name: &str) -> Result<Value, Diagnostic> {
    let (_, _, _, report) = captured(project, name, None)?;
    crate::fs::hook("replay_verified")?;
    Ok(report)
}

fn check_budget(sizes: impl IntoIterator<Item = usize>, files: usize) -> Result<(), Diagnostic> {
    if files > MAX_FILES {
        return Err(Diagnostic::new(
            ErrorCode::LimitExceeded,
            "bundle file cap exceeded",
        ));
    }
    let mut total = 0usize;
    for size in sizes {
        if size as u64 > MAX_FILE_BYTES {
            return Err(Diagnostic::new(
                ErrorCode::LimitExceeded,
                "bundle per-file cap exceeded",
            ));
        }
        total = total.checked_add(size).ok_or_else(|| {
            Diagnostic::new(ErrorCode::LimitExceeded, "bundle corpus cap exceeded")
        })?;
        if total > MAX_BYTES {
            return Err(Diagnostic::new(
                ErrorCode::LimitExceeded,
                "bundle corpus cap exceeded",
            ));
        }
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn analytics_bundle_caps() {
        assert!(check_budget([8 * 1024 * 1024; 32], 32).is_ok());
        assert!(check_budget([8 * 1024 * 1024; 33], 33).is_err());
        assert!(check_budget([8 * 1024 * 1024 + 1], 1).is_err());
        assert!(check_budget([], 10_001).is_err());
        assert!(limited(&"x".repeat(MAX_RECORDS), MAX_RECORDS).is_err());
        assert!(limited(&"x".repeat(MAX_MANIFEST), MAX_MANIFEST).is_err());
    }
}
