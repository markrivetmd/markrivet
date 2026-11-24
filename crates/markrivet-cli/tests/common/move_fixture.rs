//! Literal, independently framed two-document move fixtures.
#![allow(dead_code)]
use super::common::*;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::Child;

pub const A: &str = "tracking/A.md";
pub const B: &str = "tracking/B.md";
pub const ID: &str = "unit0001";
pub const IDS: [&str; 3] = [ID, "child001", "child002"];

pub struct MoveStore {
    pub root: PathBuf,
    pub home: PathBuf,
}

impl MoveStore {
    pub fn new(shape: &str, crlf: bool) -> Self {
        let root = project(&format!(
            "move-{shape}-{}",
            if crlf { "crlf" } else { "lf" }
        ));
        let home = root.join(".test-home");
        std::fs::create_dir(&home).unwrap();
        let s = Self { root, home };
        s.put(A, &Self::literal(shape, "source", crlf));
        s.put(B, &Self::literal(shape, "destination", crlf));
        let mut config = s.config();
        config["git"] = json!({"commit":"per-write","on_dirty":"refuse"});
        s.set_config(config);
        s.git(&["init", "-q", "-b", "main"]);
        s.git(&["config", "user.name", "Synthetic Tester"]);
        s.git(&["config", "user.email", "tester@example.invalid"]);
        s.commit();
        s
    }

    pub fn literal(shape: &str, which: &str, crlf: bool) -> String {
        let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/move");
        let shape = if shape == "empty" && which.contains("source") {
            "tight"
        } else {
            shape
        };
        let text = std::fs::read_to_string(base.join(format!("{shape}-{which}.md"))).unwrap();
        if crlf {
            text.replace('\n', "\r\n")
        } else {
            text
        }
    }

    pub fn put(&self, path: &str, text: &str) {
        std::fs::write(self.root.join(path), text).unwrap();
    }
    pub fn read(&self, path: &str) -> String {
        read(&self.root, path)
    }
    pub fn config(&self) -> Value {
        serde_json::from_str(&self.read(".rivet/project.json")).unwrap()
    }
    pub fn set_config(&self, config: Value) {
        self.put(
            ".rivet/project.json",
            &serde_json::to_string_pretty(&config).unwrap(),
        );
    }
    pub fn git_try(&self, args: &[&str]) -> (bool, String) {
        let out = git_command(&self.root, &self.home, args).output().unwrap();
        (
            out.status.success(),
            String::from_utf8_lossy(&out.stdout).into_owned()
                + &String::from_utf8_lossy(&out.stderr),
        )
    }
    pub fn git(&self, args: &[&str]) -> String {
        let (ok, out) = self.git_try(args);
        assert!(ok, "git {args:?}: {out}");
        out
    }
    pub fn commit(&self) {
        self.git(&[
            "add",
            "-A",
            "--",
            "tracking",
            ".rivet/project.json",
            ".rivet/schemas",
        ]);
        self.git(&["commit", "--allow-empty", "-qm", "synthetic fixture"]);
    }
    pub fn vars(&self, extra: &[(&'static str, String)]) -> Vec<(&'static str, String)> {
        let mut vars = vec![
            ("HOME", self.home.display().to_string()),
            ("XDG_CONFIG_HOME", self.home.display().to_string()),
        ];
        vars.extend_from_slice(extra);
        vars
    }
    pub fn spawn(&self, cmd: &str, request: &Value, extra: &[(&'static str, String)]) -> Child {
        spawn(&self.root, cmd, request, &self.vars(extra))
    }
    pub fn run(&self, cmd: &str, request: Value, extra: &[(&'static str, String)]) -> (i32, Value) {
        finish(self.spawn(cmd, &request, extra))
    }
    pub fn ok(&self, cmd: &str, request: Value) -> Value {
        let (code, out) = self.run(cmd, request, &[]);
        assert_eq!(code, 0, "{out}");
        out
    }
    pub fn request(&self) -> Value {
        let typed = self.ok("query", json!({"collection":"actions","paths":[A]}));
        let item = typed["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["id"] == ID)
            .unwrap();
        json!({"source":A,"destination":B,"id":ID,"guard":item_guard(&self.root,A,ID),"typed_guard":item["revision"],"collection":"actions"})
    }
    pub fn records(&self) -> Vec<Value> {
        let Ok(dir) = std::fs::read_dir(self.root.join(".rivet/pending")) else {
            return vec![];
        };
        let mut records: Vec<Value> = dir
            .map(|e| e.unwrap().path())
            .filter(|p| p.extension().is_some_and(|x| x == "json"))
            .map(|p| serde_json::from_slice(&std::fs::read(p).unwrap()).unwrap())
            .collect();
        records.sort_by_key(|r| r["op"].as_str().unwrap().to_string());
        records
    }
    pub fn receipts(&self) -> Vec<u8> {
        std::fs::read(self.root.join(".rivet/commits.jsonl")).unwrap_or_default()
    }
    pub fn snapshot(&self) -> (String, String, String, Vec<u8>, Vec<u8>, Vec<Value>, bool) {
        (
            self.read(A),
            self.read(B),
            self.git(&["rev-parse", "HEAD"]),
            std::fs::read(self.root.join(".git/index")).unwrap(),
            self.receipts(),
            self.records(),
            self.root.join(".git/index.lock").exists(),
        )
    }
    pub fn refuses(&self, request: Value, code: &str, reason: Option<&str>) -> Value {
        let before = self.snapshot();
        let (exit, out) = self.run("move", request, &[]);
        assert_eq!(exit, 2, "{out}");
        assert_eq!(out["error"]["code"], code, "{out}");
        if let Some(reason) = reason {
            assert_eq!(out["error"]["move_reason"], reason, "{out}");
        }
        assert_eq!(self.snapshot(), before, "refusal changed state: {out}");
        out
    }
    pub fn stamp(&self, path: &str) -> Value {
        let map = self.ok("map", json!({"path":path}));
        self.ok("stamp", json!({"path":path,"guard":map["revision"]}))
    }

    /// Prepare evidence on intentionally edited synthetic inputs, outside the
    /// operation under test. Literal layout expectations never use this helper.
    pub fn refresh_fixture(&self, path: &str) {
        let source = self.read(path);
        let doc = markrivet_core::Document::parse(&source).unwrap();
        let mut candidate = source.clone();
        for (i, marker) in doc.markers.iter().enumerate().rev() {
            if let Some((fresh, Some(_))) = doc.marker_evidence(i) {
                let span = marker.lexical.span;
                let raw = span.slice(&source).unwrap();
                let value = marker
                    .lexical
                    .subhash_value
                    .or(marker.lexical.hash_value)
                    .unwrap();
                if let Some(replacement) =
                    markrivet_core::markers::refresh_digest(raw, value, &fresh)
                {
                    candidate.replace_range(span.start..span.end, &replacement);
                }
            }
        }
        self.put(path, &candidate);
    }

    pub fn section(crlf: bool) -> Self {
        let s = Self::new("tight", crlf);
        s.put(A, &Self::literal("section", "source", crlf));
        s.put(B, &Self::literal("section", "destination", crlf));
        s.refresh_fixture(A);
        s.refresh_fixture(B);
        let mut config = s.config();
        config["collections"]["actions"]["subsections"] = json!(true);
        s.set_config(config);
        s.commit();
        s
    }

    pub fn section_request(&self) -> Value {
        self.section_request_from(&self.read(A))
    }

    pub fn section_request_from(&self, source: &str) -> Value {
        let doc = markrivet_core::Document::parse(source).unwrap();
        let target = doc
            .resolve(&markrivet_core::Selector::Id {
                id: "section1".into(),
            })
            .unwrap();
        json!({"source":A,"destination":B,"id":"section1","guard":doc.target_revision(target),
            "source_parent":"sparent1","destination_parent":"dparent1"})
    }
}
