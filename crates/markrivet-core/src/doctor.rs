//! Read-only collection diagnosis over immutable config and one read per file,
//! plus one per other collection or archive file when a citation needs them.
use crate::api::Response;
use crate::document::{Document, Target};
use crate::edit;
use crate::fs::{Project, WalkEvent, WalkFaults};
use crate::schema::{self, Bound, ProjectConfig, Source};
use crate::{CarrierBlocker, Diagnostic, ErrorCode, Selector};
use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

pub const MAX_DIAGNOSTICS: usize = 10_000;
/// Debug tests lower this bound without changing the release constant.
fn max_diagnostics() -> usize {
    #[cfg(debug_assertions)]
    if let Some(limit) = std::env::var("RIVET_TEST_DOCTOR_DIAGNOSTICS")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .filter(|n| *n > 0 && *n < MAX_DIAGNOSTICS)
    {
        return limit;
    }
    MAX_DIAGNOSTICS
}
pub const MAX_REPORT_BYTES: usize = 8 * 1024 * 1024;
/// Debug tests lower this bound without changing the release constant.
fn max_report_bytes() -> usize {
    #[cfg(debug_assertions)]
    if let Some(limit) = std::env::var("RIVET_TEST_DOCTOR_BYTES")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .filter(|n| *n > ENVELOPE_RESERVE && *n < MAX_REPORT_BYTES)
    {
        return limit;
    }
    MAX_REPORT_BYTES
}
const ENVELOPE_RESERVE: usize = 128 * 1024;

#[derive(Default, Serialize)]
pub struct Counts {
    pub files_discovered: usize,
    pub files_requested: usize,
    pub files_selected: usize,
    pub files_read: usize,
    pub files_parsed: usize,
    pub files_failed: usize,
    pub items: usize,
    pub valid: usize,
    pub invalid: usize,
    pub missing_ids: usize,
    pub unresolvable_ids: usize,
    pub excluded_items: usize,
    pub unselected_items: usize,
    pub errors: usize,
    pub warnings: usize,
    pub informational: usize,
}

#[derive(Serialize)]
pub struct CollectionReport {
    pub name: String,
    pub binding_fingerprint: String,
    pub schema_fingerprint: Option<String>,
    pub coverage: Counts,
}

#[derive(Serialize)]
pub struct Finding {
    pub code: String,
    pub severity: &'static str,
    pub collection: Option<String>,
    pub path: Option<String>,
    pub id: Option<String>,
    pub line: Option<usize>,
    pub column: Option<usize>,
    pub revision: Option<String>,
    pub message: String,
    pub next_action: &'static str,
    pub blocker: Option<CarrierBlocker>,
    /// Only on `evidence_drift` and `evidence_unrefreshable`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub evidence: Option<EvidenceFinding>,
}

/// A live stay whose stored evidence would refuse an archive or a move.
#[derive(Serialize)]
pub struct EvidenceFinding {
    pub key: &'static str,
    pub stored: Option<String>,
    /// The unit's current digest at the stored precision.
    pub fresh: Option<String>,
    pub owner: &'static str,
    pub refreshable: bool,
}

#[derive(Serialize)]
pub struct Readiness {
    pub stamp: &'static str,
    pub current_carrier: &'static str,
    pub rewrite_prerequisites: &'static str,
    pub checkbox: &'static str,
    pub checkbox_field: Option<String>,
    pub document_prerequisite: &'static str,
    pub typed_update: &'static str,
    pub typed_update_reason: &'static str,
}

#[derive(Serialize)]
pub struct ItemReport {
    pub collection: String,
    pub path: String,
    pub list_index: usize,
    pub item_index: usize,
    pub line: usize,
    pub id: Option<String>,
    pub checked: bool,
    pub valid: bool,
    pub addressing: &'static str,
    pub readiness: Readiness,
}

#[derive(Serialize)]
pub struct Membership {
    pub collection: String,
    pub heading: &'static str,
    pub member_sections: Vec<usize>,
    pub excluded_sections: Vec<usize>,
    pub unselected_sections: Vec<usize>,
}

#[derive(Serialize)]
pub struct FileReport {
    pub path: String,
    pub status: &'static str,
    pub revision: Option<String>,
    pub memberships: Vec<Membership>,
}

#[derive(Serialize)]
pub struct CarrierGroup {
    pub path: String,
    pub offset: usize,
    pub affected_memberships: usize,
    pub affected_items: usize,
}

#[derive(Serialize)]
pub struct Report {
    pub report_version: u32,
    pub root: String,
    pub scope: Value,
    pub config_fingerprint: Option<String>,
    pub consistency: &'static str,
    pub complete: bool,
    pub health: &'static str,
    pub counts: &'static str,
    pub summary: Counts,
    pub unique_items: usize,
    pub collections: Vec<CollectionReport>,
    pub files: Vec<FileReport>,
    pub traversal_events: Vec<WalkEvent>,
    pub items: Vec<ItemReport>,
    pub diagnostics: Vec<Finding>,
    pub carrier_groups: Vec<CarrierGroup>,
    pub omitted_detail: Option<String>,
    pub ok: bool,
    pub status: &'static str,
}

type MembershipKey = (String, usize, usize);
type GroupKey = (String, usize);
struct ObservedDocument<'a> {
    document: &'a Document,
    revision: &'a str,
}
impl ObservedDocument<'_> {
    fn file_line(&self, offset: usize) -> usize {
        self.document.file_line(offset)
    }
}
type FindingContext<'a> = (
    Option<&'a str>,
    Option<&'a str>,
    Option<&'a ObservedDocument<'a>>,
);

struct Scan {
    report: Report,
    used: usize,
    capped: bool,
    errors: usize,
    warnings: usize,
    physical: BTreeSet<MembershipKey>,
    groups: BTreeMap<GroupKey, BTreeSet<MembershipKey>>,
}

fn fingerprint(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}

// Charge escaped strings, pretty whitespace and a conservative allowance for
// enclosing indentation, commas and future counter growth before retaining detail.
#[derive(Default)]
struct SizeCounter {
    bytes: usize,
    lines: usize,
}
impl std::io::Write for SizeCounter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.bytes = self.bytes.saturating_add(bytes.len());
        self.lines = self
            .lines
            .saturating_add(bytes.iter().filter(|&&b| b == b'\n').count());
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
fn serialized_size<T: Serialize>(value: &T) -> SizeCounter {
    let mut size = SizeCounter::default();
    serde_json::to_writer_pretty(&mut size, value).expect("report detail serialises");
    size
}
fn cost<T: Serialize>(value: &T) -> usize {
    let size = serialized_size(value);
    size.bytes
        .saturating_add(size.lines.saturating_mul(8))
        .saturating_add(64)
}

impl Scan {
    fn new(root: String, scope: Value) -> Self {
        let mut scan = Self {
            report: Report {
                report_version: 1,
                root,
                scope,
                config_fingerprint: None,
                consistency: "per_file",
                complete: true,
                health: "unknown",
                counts: "lower_bound",
                summary: Counts::default(),
                unique_items: 0,
                collections: vec![],
                files: vec![],
                traversal_events: vec![],
                items: vec![],
                diagnostics: vec![],
                carrier_groups: vec![],
                omitted_detail: None,
                ok: false,
                status: "incomplete",
            },
            used: 0,
            capped: false,
            errors: 0,
            warnings: 0,
            physical: BTreeSet::new(),
            groups: BTreeMap::new(),
        };
        // Every variable-size envelope field is charged, including an unavailable
        // project's arbitrary caller-supplied root and escaped scope strings.
        scan.used = cost(&scan.report);
        if scan.used > max_report_bytes() - ENVELOPE_RESERVE {
            scan.cap(
                "serialized_response_bytes",
                "variable envelope metadata omitted",
            );
            scan.report.scope = json!({"omitted": "scope exceeds response budget"});
            if cost(&scan.report) > max_report_bytes() - ENVELOPE_RESERVE {
                scan.report.root = "<omitted: root exceeds response budget>".to_string();
            }
        }
        scan
    }

    fn charge<T: Serialize>(&mut self, value: &T) -> bool {
        if self.capped {
            return false;
        }
        let size = cost(value);
        if self.used.saturating_add(size) > max_report_bytes() - ENVELOPE_RESERVE {
            self.cap(
                "serialized_response_bytes",
                "further detail and scan work omitted",
            );
            return false;
        }
        self.used += size;
        true
    }

    fn cap(&mut self, limit: &str, reason: &str) {
        self.capped = true;
        self.report.complete = false;
        self.report.omitted_detail = Some(format!(
            "{limit}: {reason}; diagnostic limit {}, response budget {} bytes",
            max_diagnostics(),
            max_report_bytes()
        ));
    }

    fn finding(
        &mut self,
        code: &str,
        severity: &'static str,
        context: FindingContext<'_>,
        d: Diagnostic,
        next_action: &'static str,
    ) {
        self.finding_with(code, severity, context, d, next_action, None);
    }

    fn finding_with(
        &mut self,
        code: &str,
        severity: &'static str,
        context: FindingContext<'_>,
        d: Diagnostic,
        next_action: &'static str,
        evidence: Option<EvidenceFinding>,
    ) {
        let (collection, path, observed) = context;
        let doc = observed.map(|o| o.document);
        if self.capped {
            return;
        }
        if self.report.diagnostics.len() == max_diagnostics() {
            self.cap(
                "diagnostic_count",
                "additional findings and scan work omitted",
            );
            return;
        }
        let offset = d
            .blocker
            .as_ref()
            .map(|b| b.offset)
            .or_else(|| d.span.map(|s| s.start));
        let (line, column) = if let Some(b) = &d.blocker {
            // An affected item's own line keeps findings behind one blocker apart;
            // the blocker field still carries the blocker's coordinates.
            match d.line {
                Some(line) => (Some(line), None),
                None => (Some(b.line), Some(b.column)),
            }
        } else if let (Some(doc), Some(at)) = (doc, offset) {
            let start = doc.source.as_bytes()[..at]
                .iter()
                .rposition(|&b| b == b'\n')
                .map_or(0, |p| p + 1);
            (
                Some(observed.expect("document context").file_line(at)),
                Some(doc.source[start..at].chars().count() + 1),
            )
        } else {
            (d.line, None)
        };
        let finding = Finding {
            code: code.to_string(),
            severity,
            collection: collection.map(str::to_string),
            path: path.map(str::to_string).or(d.path),
            id: d.id,
            line,
            column,
            revision: observed.map(|o| o.revision.to_string()).or(d.current),
            message: d.message,
            next_action,
            blocker: d.blocker,
            evidence,
        };
        if self.charge(&finding) {
            self.report.summary.errors += usize::from(severity == "error");
            self.report.summary.warnings += usize::from(severity == "warning");
            self.report.summary.informational += usize::from(severity == "info");
            if let Some(name) = collection
                && let Some(col) = self.report.collections.iter_mut().find(|c| c.name == name)
            {
                col.coverage.errors += usize::from(severity == "error");
                col.coverage.warnings += usize::from(severity == "warning");
                col.coverage.informational += usize::from(severity == "info");
            }
            self.errors += usize::from(severity == "error");
            self.warnings += usize::from(severity == "warning");
            self.report.diagnostics.push(finding);
        }
    }

    fn group(&mut self, path: &str, offset: usize, collection: &str, list: usize, item: usize) {
        // Charge each contribution, bounding both retained group keys and member sets.
        if self.charge(&(path, offset, collection, list, item)) {
            self.groups
                .entry((path.to_string(), offset))
                .or_default()
                .insert((collection.to_string(), list, item));
        }
    }

    fn finish(mut self, strict: bool) -> Response {
        self.report.unique_items = self.physical.len();
        for ((path, offset), affected) in self.groups {
            let unique: BTreeSet<_> = affected.iter().map(|(_, l, i)| (*l, *i)).collect();
            self.report.carrier_groups.push(CarrierGroup {
                path,
                offset,
                affected_memberships: affected.len(),
                affected_items: unique.len(),
            });
        }
        self.report.diagnostics.sort_by(|a, b| {
            (&a.collection, &a.path, a.line, a.column, &a.code).cmp(&(
                &b.collection,
                &b.path,
                b.line,
                b.column,
                &b.code,
            ))
        });
        self.report.counts = if self.report.complete {
            "exact"
        } else {
            "lower_bound"
        };
        self.report.health = if !self.report.complete {
            "unknown"
        } else if self.errors > 0 {
            "unhealthy"
        } else if self.warnings > 0 {
            "warning"
        } else {
            "healthy"
        };
        self.report.ok = self.report.complete;
        self.report.status = if self.report.complete {
            "read"
        } else {
            "incomplete"
        };
        // A final streaming check covers the actual pretty envelope too. The
        // incremental ledger normally leaves this reserve unused; a mismatch
        // remains an explicit incomplete report rather than oversized output.
        if serialized_size(&self.report).bytes.saturating_add(1) > max_report_bytes() {
            self.report.complete = false;
            self.report.ok = false;
            self.report.status = "incomplete";
            self.report.health = "unknown";
            self.report.counts = "lower_bound";
            self.report.omitted_detail = Some("serialized_response_bytes: final envelope exceeds budget; detail arrays and scope omitted".to_string());
            self.report.items.clear();
            self.report.files.clear();
            self.report.traversal_events.clear();
            self.report.collections.clear();
            self.report.diagnostics.clear();
            self.report.carrier_groups.clear();
            self.report.scope = json!({"omitted": "response budget"});
            if serialized_size(&self.report).bytes.saturating_add(1) > max_report_bytes() {
                self.report.root = "<omitted: root exceeds response budget>".to_string();
            }
        }
        let exit = if !self.report.complete {
            2
        } else if self.errors > 0 || strict && self.warnings > 0 {
            4
        } else {
            0
        };
        Response {
            body: serde_json::to_value(self.report).expect("report serialises"),
            exit,
        }
    }
}

// The affected item owns this identity; a blocker can point into another unit.
fn item_diagnostic(mut diagnostic: Diagnostic, id: Option<&str>) -> Diagnostic {
    diagnostic.id = id.map(str::to_string);
    diagnostic
}

fn invalid(message: &str) -> Response {
    Response {
        body: json!({"ok":false,"status":"invalid","error":Diagnostic::new(ErrorCode::InvalidRequest, message)}),
        exit: 1,
    }
}

fn invalid_paths(paths: Option<&[String]>) -> Option<&'static str> {
    let paths = paths?;
    if paths.is_empty() {
        return Some("paths must be a nonempty array when supplied");
    }
    if paths.iter().any(|path| {
        path.split('/')
            .any(|part| part.is_empty() || matches!(part, "." | ".."))
            || path.split('/').next() == Some(".rivet")
            || !matches!(
                std::path::Path::new(path)
                    .extension()
                    .and_then(|e| e.to_str()),
                Some("md" | "markdown")
            )
    }) {
        return Some(
            "paths must name Markdown documents using exact plain project-relative components",
        );
    }
    None
}

/// A doctor project-open failure still uses the partial-report contract.
pub fn unavailable(
    root: &std::path::Path,
    request: &crate::api::Request,
    diagnostic: Diagnostic,
) -> Response {
    if let crate::api::Request::Doctor { paths, .. } = request
        && let Some(message) = invalid_paths(paths.as_deref())
    {
        return invalid(message);
    }
    let mut scope = serde_json::to_value(request).expect("request serialises");
    if let Some(scope) = scope.as_object_mut() {
        scope.remove("op");
    }
    let mut scan = Scan::new(root.display().to_string(), scope);
    scan.report.complete = false;
    scan.finding(
        "configuration",
        "error",
        (None, None, None),
        diagnostic,
        "open a project containing a readable .rivet directory",
    );
    scan.finish(false)
}

pub fn run(
    project: &Project,
    collection: Option<String>,
    paths: Option<Vec<String>>,
    strict: bool,
) -> Response {
    run_with(project, collection, paths, strict, &WalkFaults::default())
}

/// `faults` reaches only the directory walker, so tests can place enumeration
/// failures deterministically without chmod or process environment.
pub fn run_with(
    project: &Project,
    collection: Option<String>,
    paths: Option<Vec<String>>,
    strict: bool,
    faults: &WalkFaults,
) -> Response {
    if let Some(message) = invalid_paths(paths.as_deref()) {
        return invalid(message);
    }
    let mut scan = Scan::new(
        project.root.display().to_string(),
        json!({"collection": collection, "paths": paths, "strict": strict}),
    );
    if scan.capped {
        return scan.finish(strict);
    }
    let log_limit = crate::fs::log_read_limit();
    match project.log_size(crate::commit::RECEIPTS) {
        Ok(size) if size >= (log_limit / 2).max(1) => scan.finding(
            "receipt_log_size",
            "warning",
            (None, Some(".rivet/commits.jsonl"), None),
            Diagnostic::new(
                ErrorCode::LimitExceeded,
                format!(
                    "receipt log is {size} bytes; full-history status reads are limited to {log_limit} bytes; recovery scans in bounded chunks without a total-size limit"
                ),
            ),
            "plan receipt-history retention before the status read limit is reached; retain the log for external readers",
        ),
        Ok(_) => {}
        Err(d) => {
            scan.report.complete = false;
            scan.finding(
                "receipt_log_read",
                "error",
                (None, Some(".rivet/commits.jsonl"), None),
                d,
                "make the receipt log a readable regular file without symlinks",
            );
        }
    }
    let ignored = project
        .read_config(&[".gitignore"])
        .map(|b| String::from_utf8_lossy(&b.unwrap_or_default()).into_owned());
    if let Ok(text) = ignored {
        let missing = crate::fs::missing_ignores(&text);
        // Commit mode writes the bookkeeping these lines keep out of git.
        let per_write = schema::load_config(project)
            .ok()
            .and_then(|c| crate::commit::per_write(c.git.as_ref()))
            .is_some();
        if !missing.is_empty() {
            scan.finding(
                "rivet_gitignore",
                if per_write { "warning" } else { "info" },
                (None, Some(".rivet/.gitignore"), None),
                Diagnostic::new(
                    ErrorCode::NotFound,
                    format!(".rivet/.gitignore lacks {}", missing.join(", ")),
                ),
                "run `markrivet init` again to append the missing lines",
            );
        }
    }
    let config = match project
        .read_config(&["project.json"])
        .and_then(|bytes| {
            bytes.ok_or_else(|| {
                Diagnostic::new(ErrorCode::InvalidSchema, "missing .rivet/project.json")
            })
        })
        .and_then(|bytes| {
            scan.report.config_fingerprint = Some(fingerprint(&bytes));
            serde_json::from_slice::<ProjectConfig>(&bytes).map_err(|e| {
                Diagnostic::new(
                    ErrorCode::InvalidSchema,
                    format!(".rivet/project.json: {e}"),
                )
            })
        }) {
        Ok(c) if !c.collections.is_empty() => c,
        result => {
            scan.report.complete = false;
            let d = result.err().unwrap_or_else(|| {
                Diagnostic::new(ErrorCode::InvalidSchema, "no configured collections")
            });
            scan.finding(
                "configuration",
                "error",
                (None, None, None),
                d,
                "provide a valid project binding and schema",
            );
            return scan.finish(strict);
        }
    };
    if collection
        .as_ref()
        .is_some_and(|c| !config.collections.contains_key(c))
    {
        return invalid("unknown collection");
    }
    // Citations resolve against every collection, not only the selected ones.
    let whole = config.clone();
    let mut selected: BTreeMap<_, _> = config
        .collections
        .into_iter()
        .filter(|(name, _)| collection.as_ref().is_none_or(|c| c == name))
        .collect();
    if let Some(paths) = &paths {
        for path in paths {
            if !selected
                .values()
                .any(|c| schema::matches_file_patterns(&c.files, path))
            {
                return invalid("requested path does not match any selected collection binding");
            }
        }
        selected.retain(|_, config| {
            paths
                .iter()
                .any(|path| schema::matches_file_patterns(&config.files, path))
        });
    }
    let mut bindings: BTreeMap<String, Bound> = BTreeMap::new();
    let mut schema_bytes: BTreeMap<String, Result<Vec<u8>, Diagnostic>> = BTreeMap::new();
    for (name, config) in &selected {
        let bound = schema::schema_id(name, config).and_then(|id| {
            let bytes = schema_bytes.entry(id.to_string()).or_insert_with(|| {
                project
                    .read_config(&["schemas", &format!("{id}.json")])
                    .and_then(|b| {
                        b.ok_or_else(|| {
                            Diagnostic::new(
                                ErrorCode::InvalidSchema,
                                format!("missing schema {id}"),
                            )
                        })
                    })
            });
            bytes
                .as_ref()
                .map_err(Clone::clone)
                .and_then(|bytes| schema::bind_bytes(name, config.clone(), bytes))
        });
        let report = CollectionReport {
            name: name.clone(),
            binding_fingerprint: fingerprint(
                &serde_json::to_vec(config).expect("binding serialises"),
            ),
            schema_fingerprint: schema::schema_id(name, config)
                .ok()
                .and_then(|id| schema_bytes.get(id))
                .and_then(|bytes| bytes.as_ref().ok())
                .map(|bytes| fingerprint(bytes)),
            coverage: Counts::default(),
        };
        if !scan.charge(&report) {
            return scan.finish(strict);
        }
        scan.report.collections.push(report);
        match bound {
            Ok(b) => {
                bindings.insert(name.clone(), b);
            }
            Err(d) => {
                scan.report.complete = false;
                scan.finding(
                    "schema",
                    "error",
                    (Some(name), None, None),
                    d,
                    "correct the binding and supported schema version",
                );
            }
        }
    }
    // One walk covers citation scope too. Partition its results by selected
    // live globs, keeping files_discovered independent of requested paths.
    let globs: Vec<String> = selected
        .values()
        .flat_map(|c| c.files.iter().cloned())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let (scope_files, events, binding_errors) = crate::citations::scope(project, &whole, faults);
    for d in binding_errors {
        // Selected bindings already reported the same configuration failure.
        if !scan
            .report
            .diagnostics
            .iter()
            .any(|f| f.message == d.message)
        {
            scan.report.complete = false;
            scan.finding(
                "schema",
                "error",
                (None, None, None),
                d,
                "move the literal binding into the traversed namespace",
            );
        }
    }
    let all_globs: Vec<String> = whole
        .live_patterns()
        .into_iter()
        .chain(whole.archive_patterns())
        .collect();
    let discovered: Vec<String> = scope_files
        .iter()
        .filter(|(path, _)| schema::matches_file_patterns(&globs, path))
        .map(|(path, _)| path.clone())
        .collect();
    // Coverage gaps are decided before any file is read; informational
    // exclusions and the raw events are retained after the files, so a large
    // walk cannot crowd out file evidence.
    for event in &events {
        traversal_gap(&mut scan, event, &globs);
    }
    let matching =
        |path: &str, name: &str| schema::matches_file_patterns(&selected[name].files, path);
    let discovered: BTreeSet<_> = discovered.into_iter().collect();
    let requested: BTreeSet<_> = paths.clone().unwrap_or_default().into_iter().collect();
    let files: BTreeSet<_> = if paths.is_some() {
        requested.clone()
    } else {
        discovered.clone()
    };
    scan.report.summary.files_discovered = discovered.len();
    scan.report.summary.files_requested = requested.len();
    scan.report.summary.files_selected = files.len();
    for col in &mut scan.report.collections {
        col.coverage.files_discovered =
            discovered.iter().filter(|p| matching(p, &col.name)).count();
        col.coverage.files_requested = requested.iter().filter(|p| matching(p, &col.name)).count();
        col.coverage.files_selected = files.iter().filter(|p| matching(p, &col.name)).count();
    }
    for name in selected.keys() {
        if !files.iter().any(|p| matching(p, name)) {
            scan.finding(
                "zero_files",
                "error",
                (Some(name), None, None),
                Diagnostic::new(ErrorCode::NotFound, "collection selects zero files"),
                "check binding globs and requested scope",
            );
        }
    }
    // Citations are read from the selected files; the ids they name resolve
    // across every collection and archive file, whatever the scope.
    let mut ids = crate::citations::IdMap::default();
    let mut citing: Vec<(String, Document, String)> = vec![];
    let seen = files.clone();
    for path in files {
        if scan.capped {
            break;
        }
        let names: Vec<_> = selected
            .keys()
            .filter(|n| matching(&path, n))
            .cloned()
            .collect();
        let mut file = FileReport {
            path: path.clone(),
            status: "failed",
            revision: None,
            memberships: vec![],
        };
        if !scan.charge(&file) {
            break;
        }
        let read = project.read(&path);
        file.revision = read
            .as_ref()
            .ok()
            .map(|source| crate::revision("document", "", source.as_bytes(), &[]));
        if read.is_ok() {
            scan.report.summary.files_read += 1;
            for col in &mut scan.report.collections {
                if names.contains(&col.name) {
                    col.coverage.files_read += 1;
                }
            }
        }
        let parsed = read.and_then(|source| Document::parse(&source));
        match parsed {
            Err(mut d) => {
                d.current = file.revision.clone();
                scan.report.complete = false;
                scan.report.summary.files_failed += 1;
                for name in &names {
                    scan.report
                        .collections
                        .iter_mut()
                        .find(|c| c.name == *name)
                        .expect("selected collection")
                        .coverage
                        .files_failed += 1;
                    scan.finding(
                        "file_read_or_parse",
                        "error",
                        (Some(name), Some(&path), None),
                        d.clone(),
                        "correct the confined Markdown path, bytes or read permissions",
                    );
                }
            }
            Ok(doc) => {
                file.status = "parsed";
                let observed = ObservedDocument {
                    document: &doc,
                    revision: file.revision.as_deref().expect("read source revision"),
                };
                scan.report.summary.files_parsed += 1;
                for name in &names {
                    let ci = scan
                        .report
                        .collections
                        .iter()
                        .position(|c| c.name == *name)
                        .expect("selected collection");
                    scan.report.collections[ci].coverage.files_parsed += 1;
                    let Some(bound) = bindings.get(name) else {
                        continue;
                    };
                    let frontmatter = doc.frontmatter.fields.as_ref().err();
                    if let Some(d) = frontmatter {
                        // Unsupported frontmatter refuses key reads and writes only;
                        // collection items and body writes are unaffected.
                        scan.finding(
                            "unsupported_frontmatter",
                            "info",
                            (Some(name), Some(&path), Some(&observed)),
                            d.clone(),
                            "frontmatter key reads and writes are refused; collection items and body writes are unaffected",
                        );
                    }
                    for d in doc.diagnostics.iter().filter(|d| Some(*d) != frontmatter) {
                        let severity = if d.code == ErrorCode::UnaddressedMarker {
                            "warning"
                        } else {
                            "error"
                        };
                        let code = serde_json::to_value(d.code).expect("code serialises");
                        scan.finding(
                            code.as_str().expect("string code"),
                            severity,
                            (Some(name), Some(&path), Some(&observed)),
                            d.clone(),
                            "correct marker attachment, identity or unsupported source syntax",
                        );
                    }
                    let membership = inspect_membership(&mut scan, &observed, bound, &path, ci);
                    if let Some(m) = membership
                        && scan.charge(&m)
                    {
                        file.memberships.push(m);
                    }
                }
                evidence_findings(&mut scan, &path, &observed);
                ids.add(&path, &doc, false);
                if !crate::citations::citations(&doc).is_empty() {
                    let revision = file.revision.clone().expect("read source revision");
                    citing.push((path.clone(), doc, revision));
                }
            }
        }
        scan.report.files.push(file);
    }
    // Before other files join the map: only the selected live files compete.
    // Each finding names at most a few other carriers, so many copies of one
    // id cost linear work, and the loops stop once the report is capped.
    const NAMED: usize = 8;
    'ids: for (id, carriers) in ids.live_in_several_files() {
        if scan.capped {
            break;
        }
        // Up to NAMED carriers from each file: skipping a carrier's own file
        // in this sample costs at most NAMED steps.
        let mut per_file: BTreeMap<&str, usize> = BTreeMap::new();
        let mut sample = vec![];
        for (path, line) in &carriers {
            let seen = per_file.entry(path.as_str()).or_default();
            *seen += 1;
            if *seen <= NAMED {
                sample.push((path.as_str(), *line));
            }
        }
        for (path, line) in &carriers {
            if scan.capped {
                break 'ids;
            }
            let mut elsewhere: Vec<String> = sample
                .iter()
                .filter(|(p, _)| p != path)
                .take(NAMED)
                .map(|(p, l)| format!("{p}:{l}"))
                .collect();
            let others = carriers.len() - per_file[path.as_str()];
            let more = others.saturating_sub(NAMED);
            if more > 0 {
                elsewhere.push(format!("{more} more"));
            }
            let mut d = Diagnostic::new(
                ErrorCode::DuplicateId,
                format!(
                    "stay {id} was read live here and in {}",
                    elsewhere.join(", ")
                ),
            );
            d.id = Some(id.clone());
            d.line = Some(*line);
            scan.finding(
                "cross_file_duplicate_id",
                "error",
                (None, Some(path), None),
                d,
                "if a move is pending, run commit-pending; recheck the current files before choosing which copy keeps the id",
            );
        }
    }
    if !citing.is_empty() && !scan.capped {
        let other_files: Vec<_> = scope_files
            .into_iter()
            .filter(|(p, _)| !seen.contains(p))
            .collect();
        let other_gaps: Vec<_> = events
            .iter()
            .filter(|e| e.is_gap(&all_globs) && !e.is_gap(&globs))
            .collect();
        check_citations(&mut scan, project, ids, &other_files, &other_gaps, &citing);
    }
    for event in &events {
        excluded_entry(&mut scan, event, &globs);
    }
    for event in events {
        if !scan.charge(&event) {
            break;
        }
        scan.report.traversal_events.push(event);
    }
    scan.finish(strict)
}

/// File-level evidence findings over every live stay, once per file whatever
/// collections select it. The classification is the one archive and move use.
fn evidence_findings(scan: &mut Scan, path: &str, observed: &ObservedDocument<'_>) {
    use crate::document::Attach;
    use crate::native::{EvidenceBlock, evidence_block};
    let doc = observed.document;
    for i in 0..doc.markers.len() {
        let Some(block) = evidence_block(doc, i) else {
            continue;
        };
        let m = &doc.markers[i];
        let id = m.lexical.id.clone().unwrap_or_default();
        let (key, owner) = match m.attach {
            Attach::Item { .. } => ("subhash", "item"),
            Attach::Block(b) if doc.sections.iter().any(|s| s.block == b) => ("hash", "section"),
            Attach::Block(b) if doc.blocks[b].list.is_some() => ("hash", "list"),
            _ => ("hash", "block"),
        };
        // What the marker stores and what its unit hashes to now, whenever
        // both are readable, whatever makes the evidence unusable.
        let (stored, fresh) = match doc.marker_evidence(i) {
            Some((fresh, Some(stored))) => {
                let cut = fresh.get(..stored.len()).unwrap_or(&fresh).to_string();
                (Some(stored), Some(cut))
            }
            _ => (None, None),
        };
        let reason = match &block {
            EvidenceBlock::InvalidChild => Some(format!(
                "stay {id} is an item stay without supported attributes or a stored subhash"
            )),
            EvidenceBlock::Drift { .. } => crate::edit::unrefreshable(doc, i).map(|(_, why)| why),
        };
        let span = m.lexical.span;
        let mut finding = |code, message: String, next_action, refreshable| {
            scan.finding_with(
                code,
                "warning",
                (None, Some(path), Some(observed)),
                Diagnostic::new(ErrorCode::InvalidContent, message)
                    .with_id(id.clone())
                    .with_span(span),
                next_action,
                Some(EvidenceFinding {
                    key,
                    stored: stored.clone(),
                    fresh: fresh.clone(),
                    owner,
                    refreshable,
                }),
            );
        };
        match reason {
            None => finding(
                "evidence_drift",
                format!(
                    "stay {id} {key} is stored as {} but the {owner} now hashes to {}; \
                     archive and move refuse it",
                    stored.clone().unwrap_or_default(),
                    fresh.clone().unwrap_or_default()
                ),
                "review what changed in the unit, then rewrite its digest with a guarded refresh write",
                true,
            ),
            Some(why) => finding(
                "evidence_unrefreshable",
                format!("{why}; archive and move refuse it and refresh cannot rewrite it"),
                "repair the marker by a reviewed write; its evidence cannot be rewritten in place",
                false,
            ),
        }
    }
}

/// Report each citation in `citing` that does not name exactly one unit of its
/// kind, once the id map holds every collection and archive file. A file or
/// walk gap outside the selection is named, since a citation of an id only it
/// carries then reads as unresolved.
fn check_citations(
    scan: &mut Scan,
    project: &Project,
    mut ids: crate::citations::IdMap,
    files: &[(String, bool)],
    gaps: &[&WalkEvent],
    citing: &[(String, Document, String)],
) {
    let unchecked = "a citation of an id only it carries reads as unresolved";
    for gap in gaps {
        let path = gap.path.clone().unwrap_or_default();
        if scan.capped {
            continue;
        }
        let reason = gap
            .error
            .as_ref()
            .map_or(gap.reason.as_str(), |e| &e.message);
        scan.finding(
            "citation_scope",
            "info",
            (None, Some(&path), None),
            Diagnostic::new(ErrorCode::Io, format!("{reason}; {unchecked}")),
            "make the named directory or entry listable",
        );
    }
    for (path, archived) in files {
        if scan.capped {
            return;
        }
        match project.read(path).and_then(|s| Document::parse(&s)) {
            Ok(doc) => ids.add(path, &doc, *archived),
            Err(d) => scan.finding(
                "citation_scope",
                "info",
                (None, Some(path), None),
                Diagnostic::new(d.code, format!("{}; {unchecked}", d.message)),
                "correct the file's bytes or read permissions",
            ),
        }
    }
    for (path, doc, revision) in citing {
        let observed = ObservedDocument {
            document: doc,
            revision,
        };
        for citation in crate::citations::citations(doc) {
            if scan.capped {
                return;
            }
            let Some(problem) = ids.check(&citation) else {
                continue;
            };
            let mut d = Diagnostic::new(ErrorCode::MissingSelector, problem.message)
                .with_span(citation.span);
            d.id = Some(citation.id.clone());
            scan.finding(
                problem.code,
                "warning",
                (None, Some(path), Some(&observed)),
                d,
                "correct the cited id, or write the citation without its item: or section: prefix",
            );
        }
    }
}

/// A walk observation that could hide a matching document is a coverage gap:
/// errors and undecodable names the walker could not place outside the checked
/// bindings. Pruned prefixes and proven unrelated errors stay in
/// `traversal_events` only.
fn traversal_gap(scan: &mut Scan, event: &WalkEvent, globs: &[String]) {
    let path = event.path.as_deref().unwrap_or("");
    if !event.is_gap(globs) {
        return;
    }
    scan.report.complete = false;
    if let Some(hex) = &event.native_name_hex {
        let mut message =
            format!("entry with non-UTF-8 name (hex {hex}) cannot be matched against bindings");
        if let Some(e) = &event.error {
            message = format!("{message}; {}", e.message);
        }
        scan.finding(
            "malformed_name",
            "error",
            (None, None, None),
            Diagnostic::new(ErrorCode::PathRejected, message).with_path(path),
            "rename the entry to valid UTF-8 and scan again",
        );
    } else {
        let mut d = event.error.clone().unwrap_or_else(|| {
            Diagnostic::new(ErrorCode::Io, format!("{} not enumerated", event.reason))
        });
        if d.path.is_none() {
            d = d.with_path(path);
        }
        let next_action = if event.reason == "limit" {
            "narrow the binding globs or split the collection, then scan again"
        } else {
            "make the named directory or entry listable and scan again"
        };
        scan.finding("enumeration", "error", (None, None, None), d, next_action);
    }
}

/// Policy exclusions a binding would otherwise reach are reported, never counted.
/// Rivet's own `.rivet` and version-control `.git` entries are never documents.
fn excluded_entry(scan: &mut Scan, event: &WalkEvent, globs: &[String]) {
    let path = event.path.as_deref().unwrap_or("");
    if !event.is_excluded(globs) {
        return;
    }
    scan.finding(
        "excluded_entry",
        "info",
        (None, None, None),
        Diagnostic::new(
            ErrorCode::PathRejected,
            format!(
                "{} entry is skipped by traversal policy; its contents are not counted",
                event.reason
            ),
        )
        .with_path(path),
        "move or rename the entry if it should be a collection document",
    );
}

fn inspect_membership(
    scan: &mut Scan,
    observed: &ObservedDocument<'_>,
    bound: &Bound,
    path: &str,
    ci: usize,
) -> Option<Membership> {
    let doc = observed.document;
    let name = bound.name.as_str();
    let inst = match schema::instance(doc, bound) {
        Ok(Some(i)) => i,
        result => {
            let (heading, code, d) = match result {
                Ok(None) => (
                    "missing",
                    "missing_heading",
                    Diagnostic::new(
                        ErrorCode::NotFound,
                        format!("bound heading {:?} is missing", bound.config.section),
                    ),
                ),
                Err(d) => ("ambiguous", "ambiguous_heading", d),
                _ => unreachable!(),
            };
            scan.finding(
                code,
                "error",
                (Some(name), Some(path), Some(observed)),
                d,
                "restore one unambiguous heading matching the binding",
            );
            return Some(Membership {
                collection: name.to_string(),
                heading,
                member_sections: vec![],
                excluded_sections: vec![],
                unselected_sections: vec![],
            });
        }
    };
    let whole = doc.sections[inst.section].whole;
    let mut excluded = vec![];
    let mut unselected = vec![];
    for (i, section) in doc.sections.iter().enumerate() {
        if whole.contains(section.whole.start) && !inst.members.contains(&i) {
            if bound.config.subsections {
                excluded.push(i);
            } else {
                unselected.push(i);
            }
        }
    }
    for (sections, code, severity) in [
        (&excluded, "excluded_subtree", "info"),
        (&unselected, "unselected_subsection", "warning"),
    ] {
        for &i in sections {
            let section = &doc.sections[i];
            let count = doc
                .lists
                .iter()
                .filter(|l| {
                    let at = doc.blocks[l.block].span.start;
                    section.body.start <= at && at < section.own_end
                })
                .flat_map(|l| &l.items)
                .filter(|it| it.checkbox.is_some())
                .count();
            if code == "excluded_subtree" {
                scan.report.collections[ci].coverage.excluded_items += count;
                scan.report.summary.excluded_items += count;
            } else {
                scan.report.collections[ci].coverage.unselected_items += count;
                scan.report.summary.unselected_items += count;
            }
            if count > 0 {
                scan.finding(code, severity, (Some(name), Some(path), Some(observed)), Diagnostic::new(ErrorCode::MissingSelector, format!("{count} checkbox items in descendant section {:?} are outside membership", section.path)).with_span(section.heading), "confirm the intended subsection binding and exclusions");
            }
        }
    }
    // One §3.4 index per list of this document, built on first use.
    let carriers = edit::Carriers::new(doc);
    for (li, ii) in inst.items {
        if scan.capped {
            break;
        }
        let item = &doc.lists[li].items[ii];
        let ids = doc.stay_ids(&item.markers);
        let mut addressing = "resolvable";
        let mut readiness = Readiness {
            stamp: "not_applicable",
            current_carrier: "not_applicable",
            rewrite_prerequisites: "clear",
            checkbox: "not_applicable",
            checkbox_field: bound
                .schema
                .fields
                .iter()
                .find(|(_, f)| f.from == Source::Checkbox)
                .map(|(n, _)| n.clone()),
            document_prerequisite: if doc.write_blocker.is_some() {
                "blocked"
            } else {
                "clear"
            },
            typed_update: "not_checked",
            typed_update_reason: "no replacement fields supplied; rendering, provenance, read-back and final candidate carrier verification are outside this structural scan",
        };
        if readiness.checkbox_field.is_some() {
            readiness.checkbox = "clear";
        }
        let id = ids.first().cloned();
        scan.physical.insert((path.to_string(), li, ii));
        scan.report.summary.items += 1;
        scan.report.collections[ci].coverage.items += 1;
        let valid = match schema::extract(&doc.item_content(li, ii), &bound.schema) {
            Ok(_) => {
                scan.report.summary.valid += 1;
                scan.report.collections[ci].coverage.valid += 1;
                true
            }
            Err(errors) => {
                scan.report.summary.invalid += 1;
                scan.report.collections[ci].coverage.invalid += 1;
                for message in errors {
                    scan.finding(
                        "invalid_item",
                        "error",
                        (Some(name), Some(path), Some(observed)),
                        item_diagnostic(
                            Diagnostic::new(ErrorCode::InvalidItem, message).with_span(item.span),
                            id.as_deref(),
                        ),
                        "supply values that satisfy the bound schema",
                    );
                }
                false
            }
        };
        // Stamp skips an item carrying its own hand-written stay: adopting that
        // stay keeps the id anyone already cites, so stamping does not apply.
        let hand = item
            .markers
            .is_empty()
            .then(|| edit::hand_written_stay(&carriers, li, ii))
            .flatten();
        if ids.is_empty() {
            addressing = "missing_id";
            scan.report.summary.missing_ids += 1;
            scan.report.collections[ci].coverage.missing_ids += 1;
            let (message, next) = match &hand {
                Some((d, _)) => (
                    d.message.clone(),
                    "make any hand repair the message names, then adopt the stay with adopt_stay; \
                     stamp skips the item",
                ),
                None => (
                    "item has no stable id".to_string(),
                    "review stamp eligibility, then explicitly stamp with a fresh guard",
                ),
            };
            scan.finding(
                "missing_id",
                "warning",
                (Some(name), Some(path), Some(observed)),
                Diagnostic::new(ErrorCode::MissingSelector, message).with_span(item.span),
                next,
            );
        } else {
            for id in &ids {
                if doc.resolve(&Selector::Id { id: id.clone() })
                    != Ok(Target::Item { list: li, item: ii })
                {
                    addressing = "unresolvable";
                }
            }
            if addressing == "unresolvable" {
                scan.report.summary.unresolvable_ids += 1;
                scan.report.collections[ci].coverage.unresolvable_ids += 1;
                scan.finding(
                    "unresolvable_id",
                    "error",
                    (Some(name), Some(path), Some(observed)),
                    Diagnostic::new(
                        ErrorCode::AmbiguousSelector,
                        "item ids do not resolve uniquely to this item",
                    )
                    .with_span(item.span)
                    .with_id(id.clone().expect("nonempty ids")),
                    "correct duplicated or incorrectly attached identities",
                );
            }
        }
        // A hand-written stay of its own leaves stamp `not_applicable`, as an
        // owned stay does, and `current_carrier` too: it owns no stay yet.
        if item.markers.is_empty() && hand.is_none() {
            readiness.stamp = "clear";
            if let Err(d) = edit::stamp_item_carrier(&carriers, li, ii) {
                readiness.stamp = "blocked";
                if let Some(b) = &d.blocker {
                    scan.group(path, b.offset, name, li, ii);
                }
                scan.finding(
                    "stamp_blocked",
                    "error",
                    (Some(name), Some(path), Some(observed)),
                    d.with_line(observed.file_line(item.span.start)),
                    "correct the original carrier or paragraph before an explicit stamp",
                );
            }
        } else if !item.markers.is_empty() {
            readiness.current_carrier = "clear";
            for &m in &item.markers {
                if let Some(d) =
                    edit::current_carrier(&carriers, li, doc.markers[m].lexical.span.start)
                {
                    readiness.current_carrier = "blocked";
                    if let Some(b) = &d.blocker {
                        scan.group(path, b.offset, name, li, ii);
                    }
                    scan.finding("carrier_blocked", "error", (Some(name), Some(path), Some(observed)), { let mut d = d; d.id = doc.markers[m].lexical.id.clone(); d }, "correct current carrier text before a whole-item rewrite; checkbox checks remain separate");
                }
            }
        }
        if let Err(d) = edit::rewrite_prerequisite(doc, li, ii) {
            readiness.rewrite_prerequisites = "blocked";
            scan.finding(
                "rewrite_blocked",
                "error",
                (Some(name), Some(path), Some(observed)),
                item_diagnostic(d, id.as_deref()),
                if hand.is_some() {
                    "make any hand repair its missing_id finding names, then adopt the item's \
                     hand-written stay with adopt_stay so the item owns it"
                } else {
                    "edit the owning unit separately or use the checkbox-only path where applicable"
                },
            );
        }
        if doc.write_blocker.is_some() {
            readiness.checkbox = "blocked";
        }
        let report = ItemReport {
            collection: name.to_string(),
            path: path.to_string(),
            list_index: li,
            item_index: ii,
            line: observed.file_line(item.span.start),
            id,
            checked: item.checkbox.expect("collection shape").checked,
            valid,
            addressing,
            readiness,
        };
        if scan.charge(&report) {
            scan.report.items.push(report);
        }
    }
    Some(Membership {
        collection: name.to_string(),
        heading: "found",
        member_sections: inst.members,
        excluded_sections: excluded,
        unselected_sections: unselected,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analytics;
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;

    const GLOB: &str = "records/**/*.md";
    fn project() -> Project {
        let root = crate::test_support::scratch("rivet-doctor-walk");
        for dir in ["records/deep", "records/.drafts", "unrelated", ".hidden"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        let project = Project::init(&root).unwrap();
        std::fs::write(
            root.join(".rivet/project.json"),
            json!({"collections":{"tasks":{"schema":"task@1","files":[GLOB],"section":"Tasks","level":2}}}).to_string(),
        )
        .unwrap();
        std::fs::write(root.join(".rivet/schemas/task.json"), r#"{"id":"task","version":1,"item":"checkbox","fields":{"done":{"type":"boolean","from":"checkbox"}}}"#).unwrap();
        for file in [
            "records/one.md",
            "records/deep/two.md",
            "unrelated/three.md",
        ] {
            std::fs::write(root.join(file), "## Tasks\n").unwrap();
        }
        project
    }
    fn doctor(p: &Project, paths: Option<Vec<String>>, faults: &WalkFaults) -> Response {
        let response = run_with(p, None, paths, false, faults);
        assert!(serialized_size(&response.body).bytes < MAX_REPORT_BYTES);
        response
    }
    fn codes(r: &Response, code: &str) -> Vec<String> {
        r.body["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|d| d["code"] == code)
            .map(|d| d["path"].as_str().unwrap().to_string())
            .collect()
    }
    fn faults(directories: &[&str], entries: &[&str], types: &[&str]) -> WalkFaults {
        let set = |s: &[&str]| s.iter().map(|s| s.to_string()).collect();
        WalkFaults {
            directories: set(directories),
            entries: set(entries),
            types: set(types),
        }
    }

    #[test]
    fn unlistable_matching_directory_is_incomplete_and_keeps_evidence() {
        let p = project();
        // An injected entry failure is reported alongside the listing it interrupts.
        for (fault, parsed) in [
            (faults(&["records/deep"], &[], &[]), 1),
            (faults(&[], &["records"], &[]), 2),
            (faults(&[], &[], &["records/deep"]), 1),
        ] {
            let r = doctor(&p, None, &fault);
            assert_eq!(r.exit, 2, "{}", r.body);
            assert_eq!(r.body["complete"], false);
            assert_eq!(r.body["health"], "unknown");
            assert_eq!(r.body["counts"], "lower_bound");
            assert_eq!(codes(&r, "enumeration").len(), 1, "{}", r.body);
            assert_eq!(r.body["summary"]["files_parsed"], parsed, "{}", r.body);
        }
        // Requested paths are read independently, but the checked binding's
        // discovery gap still bounds files_discovered.
        let r = doctor(
            &p,
            Some(vec!["records/one.md".into()]),
            &faults(&["records/deep"], &[], &[]),
        );
        assert_eq!(r.exit, 2, "{}", r.body);
        assert_eq!(r.body["summary"]["files_parsed"], 1);
        assert_eq!(codes(&r, "enumeration"), ["records/deep"]);
        std::fs::remove_dir_all(&p.root).unwrap();
    }

    #[test]
    fn unrelated_pruned_and_policy_entries_do_not_impair_coverage() {
        let p = project();
        // The unrelated directory is pruned before it is opened, so a fault
        // there never fires; an unrelated type failure is proven out of scope.
        let r = doctor(
            &p,
            None,
            &faults(&["unrelated"], &[], &["unrelated", ".hidden"]),
        );
        assert_eq!(r.exit, 0, "{}", r.body);
        assert_eq!(r.body["complete"], true);
        assert_eq!(r.body["health"], "healthy");
        assert_eq!(r.body["summary"]["files_discovered"], 2);
        assert!(codes(&r, "enumeration").is_empty());
        let events = r.body["traversal_events"].as_array().unwrap();
        assert!(events.iter().any(|e| e["path"] == "unrelated"
            && e["scope"] == "out_of_scope"
            && e["error"].is_object()));
        assert!(
            !events
                .iter()
                .any(|e| e["path"] == "unrelated" && e["reason"] == "glob_pruned"),
            "a type failure leaves nothing to descend into"
        );
        let r = doctor(&p, None, &WalkFaults::default());
        assert_eq!(r.exit, 0, "{}", r.body);
        let events = r.body["traversal_events"].as_array().unwrap();
        assert!(events.iter().any(|e| e["path"] == "unrelated"
            && e["reason"] == "glob_pruned"
            && e["error"].is_null()));
        // Policy exclusions a binding would reach are reported, never counted.
        assert_eq!(codes(&r, "excluded_entry"), ["records/.drafts"]);
        assert!(
            events
                .iter()
                .any(|e| e["path"] == ".hidden" && e["reason"] == "hidden")
        );
        assert_eq!(r.body["summary"]["informational"], 1);
        // A symlinked document the glob matches is named, not silently healthy.
        std::os::unix::fs::symlink(
            p.root.join("unrelated/three.md"),
            p.root.join("records/link.md"),
        )
        .unwrap();
        let r = doctor(&p, None, &WalkFaults::default());
        assert_eq!(r.exit, 0, "{}", r.body);
        assert_eq!(
            codes(&r, "excluded_entry"),
            ["records/.drafts", "records/link.md"]
        );
        assert_eq!(r.body["summary"]["files_discovered"], 2);
        assert!(r.body["diagnostics"].as_array().unwrap().iter().any(|d| {
            d["path"] == "records/link.md"
                && d["severity"] == "info"
                && d["message"].as_str().unwrap().starts_with("symlink")
        }));
        // So is a symlinked directory the glob would descend into, and an
        // unrelated one stays an event only.
        std::os::unix::fs::symlink(p.root.join("unrelated"), p.root.join("records/more")).unwrap();
        // A link whose name trips an earlier policy rule is still never followed.
        std::os::unix::fs::symlink(p.root.join("unrelated"), p.root.join("records/.linked"))
            .unwrap();
        // A root link no glob can descend through stays an event only, as do
        // version-control directories under the binding.
        std::os::unix::fs::symlink(p.root.join("records"), p.root.join("back")).unwrap();
        std::fs::create_dir(p.root.join("records/.git")).unwrap();
        let r = doctor(&p, None, &WalkFaults::default());
        assert_eq!(r.exit, 0, "{}", r.body);
        assert_eq!(r.body["complete"], true);
        assert_eq!(
            codes(&r, "excluded_entry"),
            [
                "records/.drafts",
                "records/.linked",
                "records/link.md",
                "records/more"
            ]
        );
        assert!(
            r.body["traversal_events"]
                .as_array()
                .unwrap()
                .iter()
                .any(|e| {
                    e["path"] == "records/more"
                        && e["reason"] == "symlink"
                        && e["descendants_unknown"] == true
                })
        );
        let events = r.body["traversal_events"].as_array().unwrap();
        for path in ["back", "records/.git"] {
            assert!(
                events
                    .iter()
                    .any(|e| e["path"] == path && e["descendants_unknown"] == true)
            );
        }
        std::fs::remove_dir_all(&p.root).unwrap();
    }

    #[test]
    fn non_utf8_symlink_under_a_file_glob_is_a_coverage_gap() {
        let p = project();
        std::fs::write(
            p.root.join(".rivet/project.json"),
            json!({"collections":{"tasks":{"schema":"task@1","files":["records/*.md"],"section":"Tasks","level":2}}}).to_string(),
        )
        .unwrap();
        let link = p.root.join("records").join(OsStr::from_bytes(b"\xfe.md"));
        std::os::unix::fs::symlink(p.root.join("unrelated/three.md"), &link).unwrap();
        let r = doctor(&p, None, &WalkFaults::default());
        assert_eq!(r.exit, 2, "{}", r.body);
        assert_eq!(codes(&r, "malformed_name"), ["records"]);
        // The walker cannot follow it, so its descendants are unknown too.
        assert!(
            r.body["traversal_events"]
                .as_array()
                .unwrap()
                .iter()
                .any(|e| {
                    e["native_name_hex"] == "fe2e6d64" && e["descendants_unknown"] == true
                })
        );
        std::fs::remove_dir_all(&p.root).unwrap();
    }

    #[test]
    fn many_policy_exclusions_cannot_displace_file_evidence() {
        let p = project();
        // Every hidden directory is reachable under records/**, so each is an
        // informational finding, and there are more than the diagnostic limit.
        for n in 0..=MAX_DIAGNOSTICS {
            std::fs::create_dir(p.root.join(format!("records/.d{n}"))).unwrap();
        }
        let r = doctor(&p, None, &WalkFaults::default());
        assert_eq!(
            r.exit, 2,
            "the diagnostic limit leaves the report incomplete"
        );
        assert!(
            r.body["omitted_detail"]
                .as_str()
                .unwrap()
                .starts_with("diagnostic_count")
        );
        assert_eq!(
            r.body["summary"]["files_parsed"], 2,
            "{}",
            r.body["summary"]
        );
        assert_eq!(r.body["files"].as_array().unwrap().len(), 2);
        std::fs::remove_dir_all(&p.root).unwrap();
    }

    #[test]
    fn glob_matched_non_markdown_files_are_not_documents() {
        let p = project();
        std::fs::write(
            p.root.join(".rivet/project.json"),
            json!({"collections":{"tasks":{"schema":"task@1","files":["records/**"],"section":"Tasks","level":2}}}).to_string(),
        )
        .unwrap();
        std::fs::write(
            p.root.join("records/notes.txt"),
            "## Tasks\n\n- [ ] **One**\n",
        )
        .unwrap();
        let r = doctor(&p, None, &WalkFaults::default());
        assert_eq!(r.exit, 0, "{}", r.body);
        assert_eq!(r.body["summary"]["files_discovered"], 2);
        assert!(
            !r.body["files"]
                .as_array()
                .unwrap()
                .iter()
                .any(|f| f["path"] == "records/notes.txt")
        );
        std::fs::remove_dir_all(&p.root).unwrap();
    }

    #[test]
    fn malformed_native_names_are_reported_reversibly_and_never_guessed() {
        let p = project();
        let native = |parent: &str, name: &[u8]| p.root.join(parent).join(OsStr::from_bytes(name));
        // Provably unrelated: a root entry the literal `records` cannot match,
        // and a non-Markdown file in a matching directory.
        std::fs::write(native("", b"\xff.md"), "## Tasks\n").unwrap();
        std::fs::write(native("records", b"\xff.txt"), "").unwrap();
        let r = doctor(&p, None, &WalkFaults::default());
        assert_eq!(r.exit, 0, "{}", r.body);
        assert!(codes(&r, "malformed_name").is_empty());
        let hex: Vec<_> = r.body["traversal_events"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["reason"] == "non_utf8")
            .map(|e| e["native_name_hex"].as_str().unwrap())
            .collect();
        assert_eq!(hex.len(), 2);
        assert!(hex.contains(&"ff2e6d64") && hex.contains(&"ff2e747874"));
        // A Markdown file or a directory under the binding could hold a document.
        for (parent, name, dir) in [
            ("records", &b"\xfe.md"[..], false),
            ("records", &b"\xfe\xfe"[..], true),
        ] {
            let path = native(parent, name);
            if dir {
                std::fs::create_dir(&path).unwrap();
            } else {
                std::fs::write(&path, "## Tasks\n").unwrap();
            }
            let r = doctor(&p, None, &WalkFaults::default());
            assert_eq!(r.exit, 2, "{}", r.body);
            assert_eq!(r.body["health"], "unknown");
            assert_eq!(codes(&r, "malformed_name"), ["records"]);
            let message = r.body["diagnostics"]
                .as_array()
                .unwrap()
                .iter()
                .find(|d| d["code"] == "malformed_name")
                .unwrap()["message"]
                .as_str()
                .unwrap()
                .to_string();
            let hex: String = name.iter().map(|b| format!("{b:02x}")).collect();
            assert!(message.contains(&hex), "{message}");
            // No lossy document key is invented for it.
            assert!(
                !r.body["files"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|f| f["path"].as_str().unwrap().contains('\u{fffd}'))
            );
            if dir {
                std::fs::remove_dir(&path).unwrap();
            } else {
                std::fs::remove_file(&path).unwrap();
            }
        }
        std::fs::remove_dir_all(&p.root).unwrap();
    }

    #[test]
    fn doctor_and_analyze_agree_on_discovery_and_completeness() {
        let p = project();
        let q = analytics::Query {
            collection: "tasks".into(),
            ..analytics::Query::default()
        };
        let live = analytics::execute(&p, q.clone()).unwrap();
        let r = doctor(&p, None, &WalkFaults::default());
        assert_eq!(r.body["traversal_events"], live["events"]);
        assert_eq!(
            r.body["summary"]["files_discovered"],
            live["coverage"]["files_discovered"]
        );
        let ctx = analytics::Context::load(&p, &q, None).unwrap();
        for (fault, complete) in [
            (WalkFaults::default(), true),
            (faults(&["records/deep"], &[], &[]), false),
            (faults(&[], &["records"], &[]), false),
            (faults(&[], &[], &["records/one.md"]), false),
            (faults(&["unrelated"], &[], &["unrelated"]), true),
        ] {
            let (files, events) = p.observe_with(&ctx.bound.config.files, None, &fault);
            let r = doctor(&p, None, &fault);
            assert_eq!(r.body["traversal_events"], json!(events));
            assert_eq!(r.body["summary"]["files_discovered"], files.len());
            let mut s = analytics::Scan {
                coverage: analytics::Coverage::default(),
                diagnostics: Default::default(),
                events,
                outcomes: vec![],
                captures: vec![],
                fingerprint: String::new(),
                start: "1".into(),
                end: "2".into(),
            };
            assert_eq!(analytics::finish_scan(&q, &ctx, &mut s).is_ok(), complete);
            assert_eq!(r.body["complete"], complete, "{}", r.body);
        }
        // Matching native names leave both strict analytics and doctor incomplete.
        std::fs::write(
            p.root.join("records").join(OsStr::from_bytes(b"\xfe.md")),
            "## Tasks\n",
        )
        .unwrap();
        assert!(analytics::execute(&p, q.clone()).is_err());
        let live = analytics::execute(
            &p,
            analytics::Query {
                mode: analytics::Mode::Diagnostic,
                ..q.clone()
            },
        )
        .unwrap();
        let r = doctor(&p, None, &WalkFaults::default());
        assert_eq!(r.body["traversal_events"], live["events"]);
        assert_eq!(
            r.body["summary"]["files_discovered"],
            live["coverage"]["files_discovered"]
        );
        assert_eq!(r.body["complete"], false);
        assert_eq!(codes(&r, "malformed_name"), ["records"]);
        std::fs::remove_dir_all(&p.root).unwrap();
    }

    #[test]
    fn indexed_coordinates_match_file_byte_prefixes() {
        for source in [
            "",
            "# Tasks\n- [ ] café 界\n",
            "\u{feff}# Tasks\r\n- [ ] café 界\r\n",
            "---\ntitle: 界\n---\n# Tasks\n- [ ] café\n",
            "\u{feff}---\r\ntitle: 界\r\n---\r\n# Tasks\r\n- [ ] café",
        ] {
            let doc = Document::parse(source).unwrap();
            let revision = doc.revision();
            let observed = ObservedDocument {
                document: &doc,
                revision: &revision,
            };
            // Include continuation bytes, frontmatter, terminators and EOF.
            for offset in 0..=source.len() {
                assert_eq!(
                    observed.file_line(offset),
                    1 + source.as_bytes()[..offset]
                        .iter()
                        .filter(|b| **b == b'\n')
                        .count(),
                    "source {source:?}, byte {offset}"
                );
            }
        }
    }

    #[test]
    fn release_resource_limits() {
        assert_eq!(MAX_DIAGNOSTICS, 10_000);
        assert_eq!(MAX_REPORT_BYTES, 8 * 1024 * 1024);
    }

    #[test]
    fn unavailable_root_metadata_and_final_envelope_are_bounded() {
        let scan = Scan::new("界".repeat(MAX_REPORT_BYTES / 3 + 1), json!({}));
        let response = scan.finish(false);
        assert_eq!(response.exit, 2);
        assert!(response.body["root"].as_str().unwrap().contains("omitted"));
        assert!(serialized_size(&response.body).bytes < MAX_REPORT_BYTES);

        // Negative control for the backstop independent of the incremental ledger.
        let mut scan = Scan::new(".".into(), json!({}));
        scan.report.root = "\u{1}".repeat(MAX_REPORT_BYTES / 6 + 1);
        let response = scan.finish(false);
        assert_eq!(response.exit, 2);
        assert_eq!(response.body["complete"], false);
        assert_eq!(response.body["health"], "unknown");
        assert_eq!(response.body["counts"], "lower_bound");
        assert!(
            response.body["omitted_detail"]
                .as_str()
                .unwrap()
                .contains("final envelope")
        );
        assert!(serialized_size(&response.body).bytes < MAX_REPORT_BYTES);
    }
}
