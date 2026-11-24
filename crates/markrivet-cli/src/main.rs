use markrivet_core::api::{Request, Response, execute};
use markrivet_core::fs::Project;
use markrivet_core::{Diagnostic, ErrorCode};
use serde_json::{Value, json};
use std::io::{self, Read};
use std::path::PathBuf;

const MAX_REQUEST: u64 = 16 * 1024 * 1024;

const USAGE: &str = "usage: markrivet [--root DIR] [--pretty] <command> < request.json
       markrivet --version

Every command reads one JSON object on stdin and prints one JSON object.
Exit status: 0 ok, 1 unusable request, 2 refused with nothing changed,
3 a write that may have been applied (re-read before any retry),
4 complete doctor findings (errors, or warnings under strict).
Doctor exit 2 carries an incomplete report; doctor never writes.

  init                         create .rivet/ in --root (default: .)
  inspect                      {\"source\": \"...\"}  parser view, no project needed
  verify-delete                {\"parent\", \"candidate\", \"receipt\"} pure guarded replay
  prune-snapshot               {} immutable policy inputs under the writer lock
  move                         guarded item transfer: source, destination, id, guard, collection, typed_guard
  archive-mkdir                {\"path\"} confined configured archive parents
  archive-copy                 {\"source\", \"id\", \"guard\", \"path\", \"select\", \"destination_guard\"}
  map                          {\"path\"} outline with ids and revisions
  get                          {\"path\", \"select\"} one unit with its revision
  find                         {\"text\", \"path\"?} units whose text holds a literal match
  doctor                       {\"collection\"?, \"paths\"?, \"strict\"?} collection health and coverage
  revert-item                  {\"path\", \"collection\", \"id\", \"history_tip\", \"guard\", \"rev\"?} restore one live item
  item-log                     {\"path\", \"collection\", \"id\"} first-parent item snapshots
  status                       {\"paths\"?} git state of bound documents against the tree
  query                        {\"collection\", \"where\"?, \"fields\"?, \"ready\"?} typed items across files
  analyze                      typed metadata rows, grouped metrics and enum facets
  run-view                     {\"view\": \"name\"} run a versioned saved query
  export                       {\"bundle\", \"query\" | \"view\"} capture immutable inputs
  replay                       {\"bundle\"} replay without live source/config reads
  drilldown                    {\"query\", \"fingerprint\", \"group\", \"measure\"}
  write                        {\"path\", \"ops\": [...], \"preview\"?, \"message\"?, \"session\"?}
                               single-file batch
  set | insert | delete | stamp | update-item | insert-item | append | add-key
  set-key-field | delete-key | append-key-item
  | rejoin | adopt-stay | refresh | replace-in-section | remove-paragraph-in-section
                               {\"path\", ...op fields} a write with one op;
                               replace-in-section also takes \"top_level\"?
  create                       {\"path\", \"content\", \"stamp\"?, \"preview\"?, \"message\"?, \"session\"?}
                               a new document; never replaces one
  adopt                        {\"path\", \"message\"?, \"session\"?} commit mode: stamp and
                               commit a dirty file whole
  commit-pending               {} | {\"op\"} | {\"abandon\", \"reason\"} | {\"clear_lock\", \"identity\", \"op\"}
                               | {\"clear_lock\", \"identity\", \"size\", \"quiescent\": true}
                               settle per-write commit records

Run from inside a project or pass --root. See docs/usage.md.";

fn main() {
    let raw: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    if raw
        .first()
        .is_some_and(|a| a == markrivet_core::git::HELPER)
    {
        markrivet_core::git::exec_helper(&raw[1..]);
    }
    let mut args = std::env::args().skip(1);
    let mut root: Option<PathBuf> = None;
    let mut pretty = false;
    let mut command = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--root" => {
                // An option in the directory's place is a missing value, not a directory:
                // a directory whose name starts with `-` is reached as `./-name`.
                let dir = args
                    .next()
                    .filter(|d| !d.starts_with('-'))
                    .unwrap_or_else(|| usage_error("--root requires a directory"));
                root = Some(PathBuf::from(dir));
            }
            "--pretty" => pretty = true,
            "-h" | "--help" | "help" => {
                println!("{USAGE}");
                return;
            }
            "-V" | "--version" => {
                println!(
                    "markrivet {} {}",
                    env!("CARGO_PKG_VERSION"),
                    env!("RIVET_BUILD_COMMIT")
                );
                return;
            }
            _ if command.is_none() && !arg.starts_with('-') => command = Some(arg),
            _ => usage_error(&format!("unexpected argument {arg:?}")),
        }
    }
    let Some(command) = command else {
        usage_error("missing command")
    };
    let response = std::panic::catch_unwind(|| run(&command, root)).unwrap_or_else(|_| {
        // A panic is a Rivet defect. Before a rename nothing was written; after
        // one started, the write may have landed.
        let started = markrivet_core::fs::COMMIT_STARTED.load(std::sync::atomic::Ordering::SeqCst);
        let message = if started {
            "internal error after a rename started; re-read the file before any retry"
        } else {
            "internal error before any write; nothing changed"
        };
        Response {
            body: json!({
                "ok": false,
                "status": if started { "uncertain" } else { "refused" },
                "error": Diagnostic::new(ErrorCode::Internal, message),
            }),
            exit: if started { 3 } else { 2 },
        }
    });
    let text = if pretty {
        serde_json::to_string_pretty(&response.body)
    } else {
        serde_json::to_string(&response.body)
    }
    .expect("responses serialise");
    println!("{text}");
    std::process::exit(response.exit);
}

fn usage_error(message: &str) -> ! {
    eprintln!("{message}\n\n{USAGE}");
    std::process::exit(1);
}

fn invalid(message: String) -> Response {
    Response {
        body: json!({
            "ok": false,
            "status": "invalid",
            "error": Diagnostic::new(ErrorCode::InvalidRequest, message),
        }),
        exit: 1,
    }
}

fn run(command: &str, root: Option<PathBuf>) -> Response {
    let root_dir = root.clone().unwrap_or_else(|| PathBuf::from("."));
    if command == "init" {
        return match Project::init(&root_dir) {
            Ok(p) => Response {
                body: json!({"ok": true, "status": "applied", "root": p.root}),
                exit: 0,
            },
            Err(e) if e.code == ErrorCode::CommitUncertain => Response {
                body: json!({"ok": false, "status": "uncertain", "error": e}),
                exit: 3,
            },
            Err(e) => Response {
                body: json!({"ok": false, "status": "refused", "error": e}),
                exit: 2,
            },
        };
    }
    let max_request = if command == "verify-delete" {
        markrivet_core::delete_receipt::MAX_VERIFY_REQUEST
    } else {
        MAX_REQUEST
    };
    let mut input = Vec::new();
    if let Err(e) = io::stdin().take(max_request + 1).read_to_end(&mut input) {
        return invalid(format!("reading stdin: {e}"));
    }
    if input.len() as u64 > max_request {
        return invalid(format!("request exceeds {} MiB", max_request / 1024 / 1024));
    }
    if command == "verify-delete" {
        return match markrivet_core::delete_receipt::parse_request(&input) {
            Ok(request) => execute(None, Request::VerifyDelete { request }),
            Err(e) => invalid(e.message),
        };
    }
    if let Err(e) = markrivet_core::delete_receipt::reject_duplicates(&input) {
        return invalid(e.message);
    }
    let value: Value = match serde_json::from_slice(&input) {
        Ok(Value::Object(map)) => Value::Object(map),
        Ok(_) => return invalid("the request must be a JSON object".into()),
        Err(e) => return invalid(format!("request is not JSON: {e}")),
    };
    if cfg!(debug_assertions) && command == "__test-git" {
        return test_git(root, &root_dir, value);
    }
    #[cfg(debug_assertions)]
    if command == "__test-pair" {
        if value != json!({}) {
            return invalid("__test-pair takes an empty object".into());
        }
        return match Project::open(&root_dir) {
            Ok(p) => markrivet_core::relocate::test_pair(&p),
            Err(e) => Response {
                body: json!({"ok":false, "status":"refused", "error":e}),
                exit: 2,
            },
        };
    }
    let request = match build(command, value) {
        Ok(r) => r,
        Err(message) => return invalid(message),
    };
    let project = match &request {
        Request::Inspect { .. } => None,
        _ => {
            let found = match &root {
                Some(r) => Project::open(r),
                None => Project::discover(&root_dir),
            };
            match found {
                Ok(p) => Some(p),
                Err(e) => {
                    if matches!(request, Request::Doctor { .. }) {
                        return markrivet_core::doctor::unavailable(&root_dir, &request, e);
                    }
                    return Response {
                        body: json!({"ok": false, "status": "refused", "error": e}),
                        exit: 2,
                    };
                }
            }
        }
    };
    execute(project.as_ref(), request)
}

/// Map a command and its stdin object to a request. Single-op write commands
/// lift `path`, `preview`, `lock_timeout_ms`, `message` and `session` into
/// the enclosing write.
fn build(command: &str, mut value: Value) -> Result<Request, String> {
    if command == "commit-pending" {
        return serde_json::from_value(json!({"op": "commit_pending", "request": value}))
            .map_err(|e| format!("{command}: {e}"));
    }
    if matches!(
        command,
        "analyze" | "export" | "drilldown" | "archive-copy" | "move"
    ) {
        if value.get("op").is_some() {
            return Err("the command names the op".into());
        }
        let key = if command == "analyze" {
            "query"
        } else {
            "request"
        };
        return serde_json::from_value(json!({"op":command.replace('-', "_"),key:value}))
            .map_err(|e| format!("{command}: {e}"));
    }
    let op = match command {
        "inspect" | "map" | "get" | "find" | "write" | "query" | "replay" | "run-view"
        | "doctor" | "status" | "create" | "adopt" | "item-log" | "prune-snapshot"
        | "archive-mkdir" | "revert-item" => {
            if value.get("op").is_some() {
                return Err("the command names the op; drop \"op\" from the request".into());
            }
            value["op"] = json!(command.replace('-', "_"));
            return serde_json::from_value(value).map_err(|e| format!("{command}: {e}"));
        }
        "set" | "insert" | "delete" | "stamp" => command.to_string(),
        "update-item"
        | "insert-item"
        | "add-key"
        | "set-key-field"
        | "delete-key"
        | "append-key-item"
        | "adopt-stay"
        | "refresh"
        | "replace-in-section"
        | "remove-paragraph-in-section" => command.replace('-', "_"),
        "append" | "rejoin" => command.to_string(),
        other => return Err(format!("unknown command {other:?}")),
    };
    let map = value.as_object_mut().expect("checked object");
    if map.contains_key("op") {
        return Err("the command names the op; drop \"op\" from the request".into());
    }
    let mut write = serde_json::Map::new();
    write.insert("op".into(), json!("write"));
    for key in [
        "path",
        "preview",
        "lock_timeout_ms",
        "message",
        "session",
        "require_commit",
        "policy_guard",
        "archive",
    ] {
        if let Some(v) = map.remove(key) {
            write.insert(key.into(), v);
        }
    }
    map.insert("op".into(), json!(op));
    write.insert("ops".into(), Value::Array(vec![value]));
    serde_json::from_value(Value::Object(write)).map_err(|e| format!("{command}: {e}"))
}

/// Debug builds only: run one git call under the invocation policy, so the
/// process tests can watch a child's lifecycle. `{"args": [...], "mutate":
/// bool}`; a mutating call reports the helper's PID at the `git_spawned`
/// pause point before git starts.
fn test_git(root: Option<PathBuf>, root_dir: &std::path::Path, value: Value) -> Response {
    let project = match &root {
        Some(r) => Project::open(r),
        None => Project::discover(root_dir),
    };
    let project = match project {
        Ok(p) => p,
        Err(e) => return invalid(e.message),
    };
    let args: Vec<String> = match serde_json::from_value(value["args"].clone()) {
        Ok(a) => a,
        Err(e) => return invalid(format!("args: {e}")),
    };
    let git = markrivet_core::git::Git::new(&project);
    let out = if value["mutate"] == json!(true) {
        git.mutate(&args, |pid, _| {
            markrivet_core::fs::hook_with("git_spawned", &pid.to_string())
        })
    } else {
        git.inspect(&args, None)
    };
    match out {
        Ok(out) => Response {
            body: json!({
                "ok": true,
                "code": out.code,
                "stdout": String::from_utf8_lossy(&out.stdout),
                "stderr": String::from_utf8_lossy(&out.stderr),
            }),
            exit: 0,
        },
        Err(e) => Response {
            body: json!({"ok": false, "error": e.to_string()}),
            exit: 2,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    // Serde supplies the actual variants, so adding one cannot silently leave
    // this command table or the documented error list behind.
    fn variants(error: serde_json::Error) -> BTreeSet<String> {
        error
            .to_string()
            .split("expected one of ")
            .nth(1)
            .unwrap()
            .split('`')
            .skip(1)
            .step_by(2)
            .map(str::to_owned)
            .collect()
    }

    #[test]
    fn every_request_has_a_cli_command_and_documented_error_codes() {
        let cases: Vec<Value> = serde_json::from_str(include_str!(
            "../../markrivet-core/tests/fixtures/request-contract.json"
        ))
        .unwrap();
        let expected =
            variants(serde_json::from_value::<Request>(json!({"op":"__unknown"})).unwrap_err());
        let usage: BTreeSet<_> = USAGE.split_whitespace().collect();
        let mut seen = BTreeSet::new();
        for case in cases {
            let command = case["command"].as_str().unwrap();
            assert!(usage.contains(command), "missing USAGE entry: {command}");
            let input = case["input"].clone();
            // run() deliberately uses a bounded, duplicate-aware transport
            // parser for verify-delete, before the general build() path.
            let request = if command == "verify-delete" {
                Request::VerifyDelete {
                    request: markrivet_core::delete_receipt::parse_request(
                        &serde_json::to_vec(&input).unwrap(),
                    )
                    .unwrap(),
                }
            } else {
                build(command, input).unwrap_or_else(|e| panic!("{command}: {e}"))
            };
            let op = serde_json::to_value(request).unwrap()["op"]
                .as_str()
                .unwrap()
                .to_owned();
            assert_eq!(op, command.replace('-', "_"));
            assert!(seen.insert(op));
        }
        assert_eq!(seen, expected, "update the shared request contract table");
        let errors = variants(serde_json::from_value::<ErrorCode>(json!("__unknown")).unwrap_err());
        let docs = include_str!("../../../docs/usage.md");
        for code in errors {
            assert!(
                docs.contains(&format!("`{code}`")),
                "undocumented ErrorCode: {code}"
            );
        }
    }
}
