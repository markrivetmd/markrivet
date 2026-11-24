//! Complexity guard: `map` and `update-item` on a stamped
//! tracker ten times longer must cost at most fifteen times as much.
//!
//! The statistic is the minimum of interleaved repetitions, which estimates the
//! uncontended cost on a shared host, and the assertion is a ratio, not a time:
//! it guards how cost grows with the document, in whatever build runs it. The
//! release latencies live in `benchmark/latency/`. This is its own test binary so
//! no other test in the same process competes for the CPU.
//!
//! `RIVET_SCALING_REPS` (default 7) sets the repetitions; it is read here only
//! and changes nothing in the binary under test.
mod common;
use common::*;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const PATH: &str = "tracking/TRACKING_SYNTH.md";
const SIZES: [usize; 2] = [3_000, 30_000];
const BOUND: f64 = 15.0;

/// A synthetic tracker of about `lines` lines: half Action items, half Log
/// sections, the shape of the audit's `bench.py tracker()`.
fn tracker(lines: usize) -> String {
    let mut out: Vec<String> = [
        "---",
        "status: active",
        "entity: synth",
        "owner: owner",
        "---",
        "",
        "# Synth",
        "",
        "## Action",
        "",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    for i in 0..(lines / 2 / 4).max(3) {
        let tag = if i % 3 == 0 { "`agent` " } else { "" };
        out.push(format!(
            "- [ ] {tag}**Action {i} title. (2026-09-{:02})** Context line for item {i}",
            1 + i % 28
        ));
        out.push(format!("      with a continuation about widget {i}."));
        out.push(format!("      **Gate**: check {i} passes."));
        out.push(String::new());
    }
    out.push("## Log".into());
    out.push(String::new());
    for j in 0..(lines / 2 / 6).max(1) {
        out.push(format!("### 2026-08-{:02}: event {j}", 1 + j % 28));
        out.push(String::new());
        out.push(format!(
            "Entry {j} paragraph one with some words about the change."
        ));
        out.push("A second line in the same paragraph.".into());
        out.push(String::new());
        out.push(String::new());
    }
    out.join("\n") + "\n"
}

fn timed(root: &Path, cmd: &str, request: Value) -> (Value, Duration) {
    let started = Instant::now();
    let (code, out) = rivet(root, cmd, request);
    let took = started.elapsed();
    assert_eq!(code, 0, "{cmd}: {out}");
    (out, took)
}

struct Subject {
    lines: usize,
    root: PathBuf,
    id: String,
    guard: Value,
    done: bool,
}

/// Untimed setup: write, map, stamp, then find the last open action's id.
fn subject(lines: usize) -> Subject {
    let root = project(&format!("scaling-{lines}"));
    for entry in std::fs::read_dir(root.join("tracking")).unwrap() {
        std::fs::remove_file(entry.unwrap().path()).unwrap();
    }
    std::fs::write(root.join(PATH), tracker(lines)).unwrap();
    let (map, _) = timed(&root, "map", json!({"path": PATH}));
    let (_, took) = timed(
        &root,
        "stamp",
        json!({"path": PATH, "guard": map["revision"]}),
    );
    println!("setup {lines} lines: stamp {} ms", took.as_millis());
    let mut cursor = Value::Null;
    let mut last = Value::Null;
    loop {
        let (q, _) = timed(
            &root,
            "query",
            json!({"collection": "actions", "paths": [PATH], "limit": 1000, "cursor": cursor}),
        );
        if let Some(item) = q["items"].as_array().and_then(|a| a.last()) {
            last = item.clone();
        }
        cursor = q["next_cursor"].clone();
        if cursor.is_null() {
            break;
        }
    }
    Subject {
        lines,
        root,
        id: last["id"].as_str().expect("stamped").to_string(),
        guard: last["revision"].clone(),
        done: true,
    }
}

/// One real write: tick or untick the subject's action with the guard the
/// previous write returned.
fn update(s: &mut Subject) -> Duration {
    let (out, took) = timed(
        &s.root,
        "update-item",
        json!({"path": PATH, "collection": "actions", "id": s.id, "guard": s.guard, "set": {"done": s.done}}),
    );
    s.guard = out["ops"]
        .as_array()
        .and_then(|ops| ops.iter().find(|op| op["id"] == s.id.as_str()))
        .map(|op| op["revision"].clone())
        .unwrap_or(Value::Null);
    assert!(!s.guard.is_null(), "no fresh guard: {out}");
    s.done = !s.done;
    took
}

fn loadavg() -> String {
    std::fs::read_to_string("/proc/loadavg")
        .unwrap_or_default()
        .trim()
        .to_string()
}

#[test]
fn map_and_update_item_grow_at_most_fifteenfold_over_tenfold_lines() {
    let reps: usize = std::env::var("RIVET_SCALING_REPS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(7)
        .max(1);
    let started = Instant::now();
    let load_before = loadavg();
    let mut subjects: Vec<Subject> = SIZES.iter().map(|&l| subject(l)).collect();
    // samples[size][op] in milliseconds; op 0 is map, 1 is update-item.
    let mut samples = vec![[Vec::new(), Vec::new()]; SIZES.len()];
    for _ in 0..reps {
        for (s, sample) in subjects.iter_mut().zip(&mut samples) {
            let (_, took) = timed(&s.root, "map", json!({"path": PATH}));
            sample[0].push(took.as_secs_f64() * 1000.0);
            sample[1].push(update(s).as_secs_f64() * 1000.0);
        }
    }
    let load_after = loadavg();
    let min = |v: &Vec<f64>| v.iter().copied().fold(f64::INFINITY, f64::min);
    let mut report = format!(
        "reps {reps}, wall {:.1} s, loadavg before [{load_before}] after [{load_after}]\n",
        started.elapsed().as_secs_f64()
    );
    let mut failed = vec![];
    for (op, name) in ["map", "update-item"].iter().enumerate() {
        for (s, sample) in subjects.iter().zip(&samples) {
            report.push_str(&format!(
                "{name} {} lines: min {:.1} ms, samples {:.1?}\n",
                s.lines,
                min(&sample[op]),
                sample[op]
            ));
        }
        let ratio = min(&samples[1][op]) / min(&samples[0][op]);
        report.push_str(&format!(
            "{name} ratio {}->{} lines: {ratio:.1} (bound {BOUND})\n",
            SIZES[0], SIZES[1]
        ));
        if ratio > BOUND {
            failed.push(format!("{name} {ratio:.1}"));
        }
    }
    println!("{report}");
    assert!(
        failed.is_empty(),
        "ratio above {BOUND}: {}\n{report}",
        failed.join(", ")
    );
}
