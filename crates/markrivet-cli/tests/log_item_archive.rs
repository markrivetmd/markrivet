//! Item sources outside collections: a stayed direct bullet of a heading named
//! by a `section_archives` binding's `items` key (usage.md § `section_archives`,
//! § `archive-copy`). Every fixture is synthetic, and none binds a collection
//! unless the case is about one.
mod common;
use common::*;
use markrivet_core::{Document, Selector, document::Target};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::PathBuf;

const LOG: &str = "tracking/TRACKING_LOG.md";
const HISTORY: &str = "archive/2026-10/history/LOG_HISTORY_2026-10.md";
const ENVELOPE: &str = "# Log history\n\n## Log\n";

fn binding(items: Value) -> Value {
    json!({"section_archives":{"history":{
        "files":["tracking/*.md"],
        "archive":["archive/*/history/*_HISTORY_*.md"],
        "items":items}}})
}

fn log_binding() -> Value {
    binding(json!({"headings":[["Log"]]}))
}

struct Fx {
    root: PathBuf,
    home: PathBuf,
}

impl Fx {
    /// Write, stamp every list and heading, then commit in per-write/refuse mode.
    fn new(name: &str, config: Value, files: &[(&str, &str)]) -> Fx {
        let root = scratch(name);
        let home = root.with_extension("home");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(root.join(".rivet")).unwrap();
        std::fs::write(
            root.join(".rivet/.gitignore"),
            "/lock\n/seq\n/pending/\n/commits.jsonl\n/push.jsonl\n",
        )
        .unwrap();
        let fx = Fx { root, home };
        fx.config(config, json!({"commit":"off"}));
        for (path, text) in files {
            fx.put(path, text);
            fx.stamp(path);
        }
        fx.git(&["init", "-q", "-b", "main"]);
        fx.git(&["config", "user.name", "Tester"]);
        fx.git(&["config", "user.email", "tester@example.invalid"]);
        fx.commit_mode();
        fx.commit("synthetic fixture");
        fx
    }

    fn config(&self, mut config: Value, git: Value) {
        config["git"] = git;
        std::fs::write(self.root.join(".rivet/project.json"), config.to_string()).unwrap();
    }

    fn set_config(&self, config: Value) {
        self.config(config, json!({"commit":"per-write","on_dirty":"refuse"}));
        self.commit("synthetic config");
    }

    fn commit_mode(&self) {
        let path = self.root.join(".rivet/project.json");
        let mut config: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        config["git"] = json!({"commit":"per-write","on_dirty":"refuse"});
        std::fs::write(path, config.to_string()).unwrap();
    }

    fn put(&self, path: &str, text: &str) {
        let full = self.root.join(path);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(full, text).unwrap();
    }

    fn read(&self, path: &str) -> String {
        read(&self.root, path)
    }

    fn stamp(&self, path: &str) {
        let (_, map) = self.run("map", json!({"path":path}));
        let (code, out) = self.run(
            "stamp",
            json!({"path":path,"guard":map["revision"],"select":{"kind":"document"}}),
        );
        assert_eq!(code, 0, "{path}: {out}");
    }

    fn git(&self, args: &[&str]) -> String {
        let out = git_command(&self.root, &self.home, args).output().unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    fn commit(&self, message: &str) {
        self.git(&["add", "-A"]);
        self.git(&["commit", "-qm", message]);
    }

    fn run(&self, cmd: &str, request: Value) -> (i32, Value) {
        let home = self.home.display().to_string();
        finish(spawn(
            &self.root,
            cmd,
            &request,
            &[("HOME", home.clone()), ("XDG_CONFIG_HOME", home)],
        ))
    }

    fn ok(&self, cmd: &str, request: Value) -> Value {
        let (code, out) = self.run(cmd, request);
        assert_eq!(code, 0, "{cmd}: {out}");
        out
    }

    fn doc(&self, path: &str) -> Document {
        Document::parse(&self.read(path)).unwrap()
    }

    /// The first stay of the one item whose bytes contain `needle`.
    fn item(&self, path: &str, needle: &str) -> String {
        let doc = self.doc(path);
        let found: Vec<String> = doc
            .lists
            .iter()
            .flat_map(|l| &l.items)
            .filter(|it| it.span.slice(&doc.source).unwrap().contains(needle))
            .map(|it| doc.stay_ids(&it.markers)[0].clone())
            .collect();
        assert_eq!(found.len(), 1, "{needle}: {found:?}");
        found[0].clone()
    }

    /// The stay of the heading at this exact title path.
    fn heading(&self, path: &str, titles: &[&str]) -> String {
        let doc = self.doc(path);
        let i = doc
            .sections
            .iter()
            .position(|s| s.path == titles)
            .unwrap_or_else(|| panic!("no heading {titles:?}"));
        doc.section_ids(i)[0].clone()
    }

    fn guard(&self, path: &str, id: &str) -> String {
        item_guard(&self.root, path, id)
    }

    fn copy_request(&self, id: &str, parent: Option<&str>, dest: &str) -> Value {
        let select = json!({"kind":"heading_path","path":["Log"]});
        let (_, d) = self.run("get", json!({"path":dest,"select":select}));
        let mut request = json!({"source":LOG,"id":id,"guard":self.guard(LOG,id),"path":dest,
            "select":select,"destination_guard":d["revision"]});
        if let Some(parent) = parent {
            request["parent"] = json!(parent);
        }
        request
    }

    fn copy(&self, id: &str, parent: &str, dest: &str) -> Value {
        let out = self.ok("archive-copy", self.copy_request(id, Some(parent), dest));
        assert_eq!(out["commit"]["status"], "committed", "{out}");
        out
    }

    fn delete_request(&self, id: &str, copy: &Value, parent: Option<&str>, dest: &str) -> Value {
        let mut request = json!({"path":LOG,"select":{"kind":"id","id":id},
            "guard":self.guard(LOG,id),"require_commit":true,
            "archive":{"path":dest,"copy_op":copy["commit"]["copy_op"],"commit":copy["commit"]["sha"]}});
        if let Some(parent) = parent {
            request["archive"]["parent"] = json!(parent);
        }
        request
    }

    fn head(&self) -> String {
        self.git(&["rev-parse", "HEAD"]).trim().to_string()
    }

    /// Every tracked byte surface a refused write must leave alone.
    fn surfaces(&self) -> (BTreeMap<String, String>, String, Vec<u8>, Vec<String>) {
        let mut files = BTreeMap::new();
        for dir in ["tracking", "archive"] {
            walk(&self.root, &self.root.join(dir), &mut files);
        }
        let pending = std::fs::read_dir(self.root.join(".rivet/pending"))
            .map(|d| {
                d.map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();
        (
            files,
            self.head(),
            std::fs::read(self.root.join(".git/index")).unwrap(),
            pending,
        )
    }

    /// The command refuses with `reason` and changes no byte, commit or record.
    fn refuses(&self, cmd: &str, request: Value, reason: &str) -> Value {
        let before = self.surfaces();
        let (code, out) = self.run(cmd, request);
        assert_eq!(code, 2, "{reason}: {out}");
        assert!(out.to_string().contains(reason), "expected {reason}: {out}");
        assert_eq!(self.surfaces(), before, "{reason}");
        out
    }
}

fn walk(root: &std::path::Path, dir: &std::path::Path, out: &mut BTreeMap<String, String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries {
        let p = e.unwrap().path();
        if p.is_dir() {
            walk(root, &p, out);
        } else {
            out.insert(
                p.strip_prefix(root).unwrap().display().to_string(),
                String::from_utf8_lossy(&std::fs::read(&p).unwrap()).into_owned(),
            );
        }
    }
}

/// Three dated entries, loose, the middle one with nested ordinary content.
const THREE: &str = "# Synthetic\n\n## Log\n\n- **2026-11-12: New** Body.\n\n- **2026-10-06: Old** Older body.\n  - nested child, kept verbatim\n\n  ```\n  fenced *example*\n  ```\n\n- **2026-09-01: Oldest** Third.\n";

fn three(name: &str) -> Fx {
    Fx::new(name, log_binding(), &[(LOG, THREE), (HISTORY, ENVELOPE)])
}

/// The list in `doc` that carries stay `id`, and its container marker text.
fn container(doc: &Document, item_id: &str) -> (usize, String, String) {
    let Target::Item { list, .. } = doc.resolve(&Selector::Id { id: item_id.into() }).unwrap()
    else {
        panic!("{item_id} is not an item");
    };
    let block = &doc.blocks[doc.lists[list].block];
    let [m] = block.markers.as_slice() else {
        panic!("list has no single container stay");
    };
    let marker = &doc.markers[*m];
    (
        list,
        marker.lexical.id.clone().unwrap(),
        marker.lexical.span.slice(&doc.source).unwrap().to_string(),
    )
}

fn item_text(doc: &Document, id: &str) -> String {
    let target = doc.resolve(&Selector::Id { id: id.into() }).unwrap();
    doc.target_span(target).0.slice(&doc.source).unwrap().into()
}

/// A container stay's stored digest is the fresh one: no drift was left behind.
fn fresh(doc: &Document, id: &str) {
    let m = doc
        .markers
        .iter()
        .position(|m| m.lexical.id.as_deref() == Some(id))
        .unwrap();
    let (fresh, stored) = doc.marker_evidence(m).unwrap();
    assert!(fresh.starts_with(&stored.unwrap()), "{id} drifted");
}

fn count(text: &str, id: &str) -> usize {
    text.matches(&format!("stay:{id} ")).count()
}

#[test]
fn a_stayed_log_bullet_copies_and_deletes_with_only_the_container_digest_refreshed() {
    let fx = three("log-item-basic");
    let id = fx.item(LOG, "2026-10-06");
    let parent = fx.heading(LOG, &["Synthetic", "Log"]);
    let original = fx.read(LOG);
    let before = Document::parse(&original).unwrap();
    let text = item_text(&before, &id);
    assert!(text.contains("nested child") && text.contains("fenced *example*"));
    let (_, container_id, old_marker) = container(&before, &id);

    let envelope = fx.read(HISTORY);
    let copy = fx.copy(&id, &parent, HISTORY);
    assert_eq!(fx.read(LOG), original, "the copy never touches its source");
    assert_eq!(
        copy["commit"]["archive_copy"]["copy"]["ids"],
        json!([id.clone()])
    );
    let history = fx.read(HISTORY);
    assert_eq!(history, format!("{envelope}\n{text}\n"));
    let archived = Document::parse(&history).unwrap();
    assert!(matches!(
        archived.resolve(&Selector::Id { id: id.clone() }),
        Ok(Target::Item { .. })
    ));
    assert_eq!(item_text(&archived, &id), text);
    assert!(
        archived
            .lists
            .iter()
            .all(|l| archived.blocks[l.block].markers.is_empty()),
        "item copy imports item stays only"
    );

    let deleted = fx.ok(
        "delete",
        fx.delete_request(&id, &copy, Some(&parent), HISTORY),
    );
    assert_eq!(deleted["commit"]["status"], "committed", "{deleted}");
    let after = fx.read(LOG);
    let doc = Document::parse(&after).unwrap();
    let (_, same, new_marker) = container(&doc, &fx.item(LOG, "2026-11-12"));
    assert_eq!(same, container_id, "the list keeps its container stay");
    assert_ne!(new_marker, old_marker, "the container digest is refreshed");
    fresh(&doc, &container_id);
    let expected = original.replacen(&format!("{text}\n\n"), "", 1);
    assert_eq!(after.replacen(&new_marker, &old_marker, 1), expected);
    // Exactly one occurrence of the moved id, owned by an item in history.
    assert_eq!(count(&after, &id) + count(&fx.read(HISTORY), &id), 1);
    assert_eq!(count(&fx.read(HISTORY), &id), 1);

    // The receipt is version 1 (no list authority) and verifies on its own.
    let events: Vec<Value> = fx
        .read(".rivet/commits.jsonl")
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .filter(|e: &Value| e["op"] == deleted["commit"]["op"])
        .collect();
    let receipt = &events[0]["delete_receipt"];
    assert_eq!(receipt["version"], 1, "{receipt}");
    assert_eq!(receipt["operation"]["dropped"], json!([id]));
    fx.ok(
        "verify-delete",
        json!({"parent":original,"candidate":after,"receipt":receipt}),
    );
}

#[test]
fn item_revisions_are_position_independent() {
    let fx = three("log-item-position");
    let id = fx.item(LOG, "2026-10-06");
    let guard = fx.guard(LOG, &id);
    let text = item_text(&fx.doc(LOG), &id);
    // A sibling before it and a changed neighbour leave its revision alone.
    fx.put(
        LOG,
        &fx.read(LOG)
            .replacen(
                &text,
                &format!("- **2026-11-20: Record** Inserted sibling.\n\n{text}"),
                1,
            )
            .replacen("Body.", "Neighbour edited.", 1),
    );
    assert_eq!(fx.guard(LOG, &id), guard);
}

#[test]
fn refusals_leave_every_surface_byte_identical() {
    // Each case: fixture text, the item to name, and the refusal it must give.
    let fx = Fx::new(
        "log-item-refusals",
        log_binding(),
        &[
            (
                LOG,
                "# Synthetic\n\n## Log\n\n- **2026-10-02: Plain** Kept.\n\n- [ ] **2026-10-03: Task** Not a log entry.\n\n- **2026-10-04: Drift** Original text.\n\n## Notes\n\n- **2026-10-05: Note** Elsewhere.\n- **2026-10-06: Note two** Elsewhere.\n",
            ),
            (HISTORY, ENVELOPE),
            ("archive/2026-10/OUTSIDE.md", ENVELOPE),
        ],
    );
    let parent = fx.heading(LOG, &["Synthetic", "Log"]);
    let task = fx.item(LOG, "2026-10-03");
    fx.refuses(
        "archive-copy",
        fx.copy_request(&task, Some(&parent), HISTORY),
        "task-list item",
    );
    let note = fx.item(LOG, "2026-10-05");
    let notes = fx.heading(LOG, &["Synthetic", "Notes"]);
    fx.refuses(
        "archive-copy",
        fx.copy_request(&note, Some(&notes), HISTORY),
        "is not named in section_archives",
    );
    let plain = fx.item(LOG, "2026-10-02");
    let mut outside = fx.copy_request(&plain, Some(&parent), HISTORY);
    outside["path"] = json!("archive/2026-10/OUTSIDE.md");
    fx.refuses(
        "archive-copy",
        outside,
        "exactly one section_archives binding",
    );
    // A list container is a block, not an item source.
    let doc = fx.doc(LOG);
    let (_, list_stay, _) = container(&doc, &plain);
    let mut request = fx.copy_request(&plain, None, HISTORY);
    request["id"] = json!(list_stay);
    request["guard"] = json!(fx.guard(LOG, &list_stay));
    fx.refuses("archive-copy", request, "action item");
    // An id that names nothing (an unstamped bullet has no id to name).
    let mut request = fx.copy_request(&plain, Some(&parent), HISTORY);
    request["id"] = json!("Unstampd");
    fx.refuses("archive-copy", request, "no stay with id Unstampd");

    // Drifted child evidence refuses until refreshed separately.
    let drift = fx.item(LOG, "2026-10-04");
    let request = fx.copy_request(&drift, Some(&parent), HISTORY);
    fx.put(LOG, &fx.read(LOG).replace("Original text.", "Edited text."));
    fx.commit("synthetic drift");
    let mut request = request;
    request["guard"] = json!(fx.guard(LOG, &drift));
    fx.refuses("archive-copy", request, "drifted");

    // Two bindings that both authorize this source into this destination.
    let mut two = log_binding();
    two["section_archives"]["second"] = two["section_archives"]["history"].clone();
    fx.set_config(two);
    fx.refuses(
        "archive-copy",
        fx.copy_request(&plain, Some(&parent), HISTORY),
        "exactly one section_archives binding",
    );
    let mut config = log_binding();
    config["section_archives"]["history"]
        .as_object_mut()
        .unwrap()
        .remove("items");
    fx.set_config(config);
    // With no `items` key anywhere, the item is judged as a collection item.
    fx.refuses(
        "archive-copy",
        fx.copy_request(&plain, None, HISTORY),
        "exactly one bound collection",
    );
}

#[test]
fn a_sole_item_and_list_authority_refuse_on_copy_and_delete() {
    let fx = Fx::new(
        "log-item-sole",
        log_binding(),
        &[
            (
                LOG,
                "# Synthetic\n\n## Log\n\n- **2026-10-01: Only** Sole entry.\n\nProse between lists.\n\n- **2026-09-02: A** One.\n- **2026-09-01: B** Two.\n",
            ),
            (HISTORY, ENVELOPE),
        ],
    );
    let parent = fx.heading(LOG, &["Synthetic", "Log"]);
    let only = fx.item(LOG, "2026-10-01");
    fx.refuses(
        "archive-copy",
        fx.copy_request(&only, Some(&parent), HISTORY),
        "sole item",
    );
    // Made sole after its copy: every delete shape refuses, with no receipt.
    let a = fx.item(LOG, "2026-09-02");
    let b = fx.item(LOG, "2026-09-01");
    let copy = fx.copy(&b, &parent, HISTORY);
    fx.ok(
        "delete",
        json!({"path":LOG,"select":{"kind":"id","id":a},"guard":fx.guard(LOG,&a)}),
    );
    let doc = fx.doc(LOG);
    let Target::Item { list, .. } = doc.resolve(&Selector::Id { id: b.clone() }).unwrap() else {
        panic!()
    };
    let list_guard = doc.target_revision(Target::Block(doc.lists[list].block));
    let parentless = fx.delete_request(&b, &copy, None, HISTORY);
    fx.refuses("delete", parentless.clone(), "list");
    let mut authorized = fx.delete_request(&b, &copy, Some(&parent), HISTORY);
    authorized["list"] = json!({"guard":list_guard});
    fx.refuses("delete", authorized, "list authority");
    let mut parentless_authorized = parentless;
    parentless_authorized["list"] = json!({"guard":list_guard});
    fx.refuses("delete", parentless_authorized, "list authority");
    fx.refuses(
        "delete",
        fx.delete_request(&b, &copy, Some(&parent), HISTORY),
        "list",
    );
    // A retry of the original copy now refuses the same way.
    fx.refuses(
        "archive-copy",
        fx.copy_request(&b, Some(&parent), HISTORY),
        "sole item",
    );
    // No commit carries a delete receipt for B; each refused attempt is logged
    // locally as not applied, whatever receipt it would have written.
    let log = fx.git(&["log", "--format=%B"]);
    assert_eq!(
        log.matches("Rivet-Delete").count(),
        1,
        "only A's delete: {log}"
    );
    for event in fx.read(".rivet/commits.jsonl").lines() {
        let event: Value = serde_json::from_str(event).unwrap();
        if event["delete_receipt"]["operation"]["dropped"]
            .as_array()
            .is_some_and(|d| d.contains(&json!(b)))
        {
            assert_eq!(event["outcome"], "not_applied", "{event}");
        }
    }
}

#[test]
fn parent_is_checked_for_non_sole_items_on_copy_retry_and_delete() {
    let config = binding(json!({"headings":[["Log"],["Notes"]]}));
    let fx = Fx::new(
        "log-item-parent",
        config,
        &[
            (
                LOG,
                "# Synthetic\n\n## Log\n\n- **2026-10-03: A** One.\n\n- **2026-10-02: B** Two.\n\n- **2026-10-01: C** Three.\n\n## Notes\n\n- **2026-09-02: N** Note.\n\n- **2026-09-01: M** Note two.\n",
            ),
            (HISTORY, ENVELOPE),
        ],
    );
    let log = fx.heading(LOG, &["Synthetic", "Log"]);
    let notes = fx.heading(LOG, &["Synthetic", "Notes"]);
    let title = fx.heading(LOG, &["Synthetic"]);
    let b = fx.item(LOG, "2026-10-02");
    let other = fx.item(LOG, "2026-10-03");
    for (parent, reason) in [
        (None, "requires a parent precondition"),
        (Some(notes.as_str()), "does not name the stamped heading"),
        (Some(title.as_str()), "does not name the stamped heading"),
        (Some(other.as_str()), "does not name the stamped heading"),
        (Some("Missing1"), "no stay with id Missing1"),
    ] {
        fx.refuses("archive-copy", fx.copy_request(&b, parent, HISTORY), reason);
    }
    let copy = fx.copy(&b, &log, HISTORY);
    for (parent, reason) in [
        (None, "requires a parent precondition"),
        (Some(notes.as_str()), "does not name the stamped heading"),
    ] {
        fx.refuses(
            "delete",
            fx.delete_request(&b, &copy, parent, HISTORY),
            reason,
        );
        fx.refuses("archive-copy", fx.copy_request(&b, parent, HISTORY), reason);
    }
    // Relocated under another authorized heading with its revision unchanged:
    // the frozen parent no longer holds on retry or delete.
    let guard = fx.guard(LOG, &b);
    let text = item_text(&fx.doc(LOG), &b);
    let moved = fx
        .read(LOG)
        .replacen(&format!("{text}\n\n"), "", 1)
        .replacen(
            "- **2026-09-01: M**",
            &format!("{text}\n\n- **2026-09-01: M**"),
            1,
        );
    fx.put(LOG, &moved);
    fx.commit("synthetic relocation");
    assert_eq!(fx.guard(LOG, &b), guard);
    let mut retry = fx.copy_request(&b, Some(&log), HISTORY);
    retry["guard"] = json!(guard);
    fx.refuses("archive-copy", retry, "does not name the stamped heading");
    fx.refuses(
        "delete",
        fx.delete_request(&b, &copy, Some(&log), HISTORY),
        "does not name the stamped heading",
    );
    // A correct-parent control on a sibling still copies.
    let c = fx.item(LOG, "2026-10-01");
    fx.copy(&c, &log, HISTORY);
}

/// Bind the example's checkbox collection over the same files, with its own
/// archive namespace, beside the `items`-enabled section binding.
fn with_actions(fx: &Fx, mut config: Value) -> Value {
    config["collections"] = json!({"actions":{"schema":"tracker-action@1",
        "files":["tracking/*.md"],"section":"Action","level":2,
        "archive":["items/*/*_CLOSED_*.md"]}});
    std::fs::create_dir_all(fx.root.join(".rivet/schemas")).unwrap();
    std::fs::copy(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../examples/tracker/.rivet/schemas/tracker-action.json"),
        fx.root.join(".rivet/schemas/tracker-action.json"),
    )
    .unwrap();
    config
}

#[test]
fn collection_items_beside_an_items_heading_keep_todays_rules() {
    const CLOSED: &str = "items/2026-10/LOG_CLOSED_2026-10.md";
    let fx = Fx::new(
        "log-item-collection-control",
        json!({}),
        &[
            (
                LOG,
                "# Synthetic\n\n## Action\n\n- [ ] **Do one.** First.\n- [ ] **Do two.** Second.\n\n## Log\n\n- **2026-10-02: A** One.\n\n- **2026-10-01: B** Two.\n",
            ),
            (HISTORY, ENVELOPE),
            (
                CLOSED,
                "# Closed\n\n## Log\n\n* [x] **Earlier.** Closed before.\n",
            ),
        ],
    );
    let earlier = item_text(&fx.doc(CLOSED), &fx.item(CLOSED, "Earlier."));
    let config = with_actions(&fx, log_binding());
    fx.set_config(config);
    let action = fx.item(LOG, "Do one.");
    let actions = fx.heading(LOG, &["Synthetic", "Action"]);
    let log = fx.heading(LOG, &["Synthetic", "Log"]);
    fx.refuses(
        "archive-copy",
        fx.copy_request(&action, Some(&actions), CLOSED),
        "only valid for a section",
    );
    // A collection member never takes a history destination, with or without parent.
    fx.refuses(
        "archive-copy",
        fx.copy_request(&action, None, HISTORY),
        "exactly one bound collection",
    );
    let copy = fx.ok("archive-copy", fx.copy_request(&action, None, CLOSED));
    assert_eq!(copy["commit"]["status"], "committed", "{copy}");
    // A `-` item never joins the `*` list: it starts its own list after it.
    let closed = fx.doc(CLOSED);
    assert_eq!(closed.lists.len(), 2, "{}", closed.source);
    assert_eq!(item_text(&closed, &fx.item(CLOSED, "Earlier.")), earlier);
    let Ok(Target::Item { list, .. }) = closed.resolve(&Selector::Id { id: action.clone() }) else {
        panic!("copied action is not an item")
    };
    assert_eq!(list, 1);
    assert_eq!(closed.lists[1].items.len(), 1);
    let mut with_parent = fx.delete_request(&action, &copy, Some(&actions), CLOSED);
    fx.refuses("delete", with_parent.clone(), "only valid for a section");
    with_parent["archive"]
        .as_object_mut()
        .unwrap()
        .remove("parent");
    fx.ok("delete", with_parent);
    // The Log bullet in the same file follows the heading rules.
    let b = fx.item(LOG, "2026-10-01");
    fx.refuses(
        "archive-copy",
        fx.copy_request(&b, Some(&log), CLOSED),
        "exactly one section_archives binding",
    );
    fx.copy(&b, &log, HISTORY);
}

#[test]
fn duplicates_are_searched_across_every_archive_namespace() {
    for other in ["history", "collection"] {
        let fx = three(&format!("log-item-duplicate-{other}"));
        let id = fx.item(LOG, "2026-10-06");
        let parent = fx.heading(LOG, &["Synthetic", "Log"]);
        let text = item_text(&fx.doc(LOG), &id);
        let mut config = log_binding();
        let carrier = if other == "history" {
            // Another history file under a second section binding.
            config["section_archives"]["older"] =
                json!({"files":["tracking/*.md"],"archive":["old/*/history/*_HISTORY_*.md"]});
            "old/2026-09/history/LOG_HISTORY_2026-09.md"
        } else {
            config = with_actions(&fx, config);
            "items/2026-09/LOG_CLOSED_2026-09.md"
        };
        fx.put(carrier, &format!("# Older\n\n## Log\n\n{text}\n"));
        fx.set_config(config);
        fx.refuses(
            "archive-copy",
            fx.copy_request(&id, Some(&parent), HISTORY),
            "another archive already carries",
        );
    }
}

#[test]
fn tightness_follows_the_parser_and_copied_bytes_never_change() {
    struct Case {
        name: &'static str,
        log: &'static str,
        take: &'static str,
        keep: &'static str,
        tight_after: bool,
    }
    for case in [
        Case {
            name: "asymmetric",
            log: "# Synthetic\n\n## Log\n\n- **2026-10-03: A** One.\n- **2026-10-02: B** Two.\n\n- **2026-10-01: C** Three.\n",
            take: "2026-10-01",
            keep: "2026-10-03",
            tight_after: true,
        },
        Case {
            name: "two-paragraph",
            log: "# Synthetic\n\n## Log\n\n- **2026-10-03: A** One.\n\n  Second paragraph.\n- **2026-10-02: B** Two.\n",
            take: "2026-10-02",
            keep: "2026-10-03",
            tight_after: false,
        },
        Case {
            name: "loose-two-to-one",
            log: "# Synthetic\n\n## Log\n\n- **2026-10-03: A** One.\n\n- **2026-10-02: B** Two.\n",
            take: "2026-10-02",
            keep: "2026-10-03",
            tight_after: true,
        },
    ] {
        let fx = Fx::new(
            &format!("log-item-tight-{}", case.name),
            log_binding(),
            &[(LOG, case.log), (HISTORY, ENVELOPE)],
        );
        let parent = fx.heading(LOG, &["Synthetic", "Log"]);
        let take = fx.item(LOG, case.take);
        let keep = fx.item(LOG, case.keep);
        let before = fx.doc(LOG);
        let kept = item_text(&before, &keep);
        let (_, container_id, _) = container(&before, &keep);
        let copy = fx.copy(&take, &parent, HISTORY);
        fx.ok(
            "delete",
            fx.delete_request(&take, &copy, Some(&parent), HISTORY),
        );
        let doc = fx.doc(LOG);
        let (list, same, _) = container(&doc, &keep);
        assert_eq!(same, container_id, "{}", case.name);
        assert_eq!(doc.lists[list].tight, case.tight_after, "{}", case.name);
        assert_eq!(item_text(&doc, &keep), kept, "{}", case.name);
        fresh(&doc, &container_id);
    }

    // Three copies into a fresh history list: tight after one, loose after two.
    let fx = Fx::new(
        "log-item-tight-history",
        log_binding(),
        &[
            (
                LOG,
                "# Synthetic\n\n## Log\n\n- **2026-10-04: K** Keep.\n- **2026-10-03: A** One.\n- **2026-10-02: B** Two.\n- **2026-10-01: C** Three.\n",
            ),
            (HISTORY, ENVELOPE),
        ],
    );
    let parent = fx.heading(LOG, &["Synthetic", "Log"]);
    let mut texts = vec![];
    for (n, date) in ["2026-10-03", "2026-10-02", "2026-10-01"]
        .iter()
        .enumerate()
    {
        let id = fx.item(LOG, date);
        texts.push((id.clone(), item_text(&fx.doc(LOG), &id)));
        let copy = fx.copy(&id, &parent, HISTORY);
        fx.ok(
            "delete",
            fx.delete_request(&id, &copy, Some(&parent), HISTORY),
        );
        let history = fx.doc(HISTORY);
        assert_eq!(history.lists.len(), 1);
        assert_eq!(history.lists[0].tight, n == 0, "after copy {}", n + 1);
        for (id, text) in &texts {
            assert_eq!(&item_text(&history, id), text);
        }
    }
}

#[test]
fn destination_placement_stays_in_the_history_log_own_body() {
    let entry = "### 2026-09-30: Archived entry\n\nEntry body.\n";
    for (name, history, crlf) in [
        ("new", ENVELOPE.to_string(), false),
        (
            "heading-only",
            format!("# Log history\n\n## Log\n\n{entry}"),
            false,
        ),
        (
            "trailing-item-stay",
            "# Log history\n\n## Log\n\n- **2026-10-20: Kept** Earlier copy.\n".to_string(),
            false,
        ),
        (
            "two-lists",
            format!(
                "# Log history\n\n## Log\n\n- **2026-10-21: First** list.\n\nProse.\n\n- **2026-10-20: Second** list.\n\n{entry}"
            ),
            false,
        ),
        (
            "other-marker",
            "# Log history\n\n## Log\n\n* **2026-10-20: Star** marker.\n".to_string(),
            false,
        ),
        ("crlf", ENVELOPE.to_string(), true),
        (
            "no-final-newline",
            "# Log history\n\n## Log".to_string(),
            false,
        ),
    ] {
        let nl = |t: &str| {
            if crlf {
                t.replace('\n', "\r\n")
            } else {
                t.to_string()
            }
        };
        let fx = Fx::new(
            &format!("log-item-place-{name}"),
            log_binding(),
            &[(LOG, &nl(THREE)), (HISTORY, &nl(&history))],
        );
        let parent = fx.heading(LOG, &["Synthetic", "Log"]);
        let id = fx.item(LOG, "2026-10-06");
        let text = item_text(&fx.doc(LOG), &id);
        let before = fx.read(HISTORY);
        fx.copy(&id, &parent, HISTORY);
        let after = fx.read(HISTORY);
        let doc = Document::parse(&after).unwrap();
        let target = doc.resolve(&Selector::Id { id: id.clone() }).unwrap();
        assert!(matches!(target, Target::Item { .. }), "{name}");
        assert_eq!(item_text(&doc, &id), text, "{name}");
        let at = doc.target_span(target).0.start;
        let log = doc
            .sections
            .iter()
            .position(|s| s.path == ["Log history", "Log"])
            .unwrap();
        assert_eq!(
            doc.section_of(at),
            Some(log),
            "{name}: never inside an archived ###"
        );
        assert!(at < doc.sections[log].own_end, "{name}");
        if let Some(i) = after.find("### 2026-09-30") {
            assert!(at < i, "{name}");
            assert_eq!(
                &after[i..],
                &before[before.find("### 2026-09-30").unwrap()..]
            );
        }
        if name == "two-lists" {
            assert!(
                at > after.find("Second").unwrap(),
                "appends to the last list"
            );
        }
        if name == "other-marker" {
            assert!(after.contains("* **2026-10-20: Star** marker."));
        }
        // Everything before the Log heading and every earlier item is unchanged.
        let i = before.find("## Log").unwrap();
        assert_eq!(&after[..i], &before[..i], "{name}");
        let old = Document::parse(&before).unwrap();
        for it in old.lists.iter().flat_map(|l| &l.items) {
            let bytes = it.span.slice(&old.source).unwrap();
            assert!(after.contains(bytes), "{name}: {bytes}");
        }
    }
}

#[test]
fn heading_authorization_is_an_exact_path_under_the_title() {
    let body = "- **2026-10-02: A** One.\n\n- **2026-10-01: B** Two.\n";
    for (name, items, text, expect) in [
        (
            "direct",
            json!([["Log"]]),
            format!("# T\n\n## Log\n\n{body}"),
            None,
        ),
        (
            "nested-only",
            json!([["Log"]]),
            format!("# T\n\n## Notes\n\n### Log\n\n{body}"),
            Some("is not named"),
        ),
        (
            "both-direct",
            json!([["Log"]]),
            format!(
                "# T\n\n## Log\n\n{body}\n## Notes\n\n### Log\n\n- **2026-09-02: N** x.\n\n- **2026-09-01: M** y.\n"
            ),
            None,
        ),
        (
            "both-nested",
            json!([["Log"]]),
            format!(
                "# T\n\n## Log\n\n- **2026-09-02: N** x.\n\n- **2026-09-01: M** y.\n\n## Notes\n\n### Log\n\n{body}"
            ),
            Some("is not named"),
        ),
        (
            "siblings",
            json!([["Log"]]),
            format!(
                "# T\n\n## Log\n\n{body}\n## Log\n\n- **2026-09-02: N** x.\n\n- **2026-09-01: M** y.\n"
            ),
            Some("2 headings match"),
        ),
        (
            "no-title",
            json!([["Log"]]),
            format!("## Log\n\n{body}"),
            Some("exactly one level-1 title"),
        ),
        (
            "two-titles",
            json!([["Log"]]),
            format!("# T\n\n## Log\n\n{body}\n# U\n\nOther.\n"),
            Some("exactly one level-1 title"),
        ),
        (
            "skipped-level",
            json!([["Log"]]),
            format!("# T\n\n### Log\n\n{body}"),
            Some("is not named"),
        ),
        (
            "deeper",
            json!([["Area", "Log"]]),
            format!("# T\n\n## Area\n\n### Log\n\n{body}"),
            None,
        ),
        (
            "deeper-gap",
            json!([["Area", "Log"]]),
            format!("# T\n\n## Area\n\n#### Log\n\n{body}"),
            Some("is not named"),
        ),
        (
            "deeper-direct-only",
            json!([["Area", "Log"]]),
            format!("# T\n\n## Log\n\n{body}"),
            Some("is not named"),
        ),
        (
            "inside-a-log-entry",
            json!([["Log"]]),
            format!(
                "# T\n\n## Log\n\n- **2026-09-02: N** x.\n\n- **2026-09-01: M** y.\n\n### 2026-09-30: Entry\n\n{body}"
            ),
            Some("is not named"),
        ),
    ] {
        let fx = Fx::new(
            &format!("log-item-heading-{name}"),
            binding(json!({"headings":items})),
            &[(LOG, &text), (HISTORY, ENVELOPE)],
        );
        let id = fx.item(LOG, "2026-10-01");
        let doc = fx.doc(LOG);
        let Target::Item { list, .. } = doc.resolve(&Selector::Id { id: id.clone() }).unwrap()
        else {
            panic!()
        };
        let section = doc
            .section_of(doc.blocks[doc.lists[list].block].span.start)
            .unwrap();
        let parent = doc.section_ids(section)[0].clone();
        let request = fx.copy_request(&id, Some(&parent), HISTORY);
        match expect {
            None => {
                fx.copy(&id, &parent, HISTORY);
            }
            Some(reason) => {
                fx.refuses("archive-copy", request, reason);
            }
        }
    }
}

#[test]
fn items_config_refuses_malformed_values_at_load() {
    let fx = three("log-item-config");
    for (items, reason) in [
        (json!({"headings":[]}), "at least one heading path"),
        (json!({"headings":[[]]}), "at least one heading"),
        (json!({"headings":[["Log",""]]}), "must not be empty"),
        (
            json!({"headings":[["Log"],["Log"]]}),
            "duplicate heading path",
        ),
        (json!({"headings":"Log"}), "invalid type"),
        (json!({"headings":[["Log"]],"extra":true}), "unknown field"),
        (json!(["Log"]), "must be an object"),
        (Value::Null, "must be an object"),
        // serde would read a struct from this positional array.
        (json!([[["Log"]]]), "must be an object"),
    ] {
        fx.config(
            binding(items.clone()),
            json!({"commit":"per-write","on_dirty":"refuse"}),
        );
        let (code, out) = fx.run(
            "archive-copy",
            json!({"source":LOG,"id":"x","guard":"x","path":HISTORY,
                "select":{"kind":"heading_path","path":["Log"]},"destination_guard":"x"}),
        );
        assert_eq!(code, 2, "{items}: {out}");
        assert!(out.to_string().contains(reason), "{items}: {out}");
        assert_eq!(out["error"]["code"], "invalid_schema", "{items}: {out}");
    }
}

/// No collection is configured here. Before deletion the live bullet carries
/// the cited id; a plain delete would leave the citation dangling, while the
/// archive-guarded delete leaves it carried by history.
#[test]
fn citations_resolve_live_before_deletion_and_archived_after() {
    let fx = three("log-item-citations");
    let id = fx.item(LOG, "2026-10-06");
    let parent = fx.heading(LOG, &["Synthetic", "Log"]);
    fx.put(
        "tracking/TRACKING_CITER.md",
        &format!("# Citer\n\n## Notes\n\nSee item:{id} for the background.\n"),
    );
    fx.commit("synthetic citer");
    let plain =
        json!({"path":LOG,"select":{"kind":"id","id":id},"guard":fx.guard(LOG,&id),"preview":true});
    let preview = fx.ok("delete", plain);
    assert_eq!(
        preview["dangling_citations"][0]["path"], "tracking/TRACKING_CITER.md",
        "a delete without a copy would strand the citation: {preview}"
    );
    let copy = fx.copy(&id, &parent, HISTORY);
    let mut preview = fx.delete_request(&id, &copy, Some(&parent), HISTORY);
    preview["preview"] = json!(true);
    let preview = fx.ok("delete", preview);
    assert!(preview.get("dangling_citations").is_none(), "{preview}");
    let deleted = fx.ok(
        "delete",
        fx.delete_request(&id, &copy, Some(&parent), HISTORY),
    );
    assert!(deleted.get("dangling_citations").is_none(), "{deleted}");
    assert!(matches!(
        fx.doc(HISTORY).resolve(&Selector::Id { id: id.clone() }),
        Ok(Target::Item { .. })
    ));
    assert!(fx.doc(LOG).resolve(&Selector::Id { id }).is_err());
}

/// The `items` configuration and the heading-item request in usage.md
/// § `section_archives` are the ones these tests run.
#[test]
fn usage_heading_item_example_runs() {
    let usage = include_str!("../../../docs/usage.md");
    let section = usage.split("### `section_archives`").nth(1).unwrap();
    let block = |text: &str| -> Value {
        let json = text.split("```json\n").nth(1).unwrap();
        serde_json::from_str(json.split("\n```").next().unwrap()).unwrap()
    };
    let config = block(section);
    assert_eq!(
        config["section_archives"]["history"]["items"],
        json!({"headings":[["Log"]]})
    );
    let documented = block(section.split("#### Heading items").nth(1).unwrap());
    let fx = Fx::new(
        "log-item-usage",
        config,
        &[(LOG, THREE), (HISTORY, ENVELOPE)],
    );
    let id = fx.item(LOG, "2026-10-06");
    let parent = fx.heading(LOG, &["Synthetic", "Log"]);
    let request = fx.copy_request(&id, Some(&parent), HISTORY);
    let keys = |v: &Value| v.as_object().unwrap().keys().cloned().collect::<Vec<_>>();
    assert_eq!(keys(&request), keys(&documented));
    assert_eq!(request["select"], documented["select"]);
    let copy = fx.copy(&id, &parent, HISTORY);
    fx.ok(
        "delete",
        fx.delete_request(&id, &copy, Some(&parent), HISTORY),
    );
}

/// A collection under a child heading of the Log does not block a Log bullet;
/// a plain bullet inside a collection's protected subtree still refuses.
#[test]
fn only_the_bullet_itself_must_lie_outside_collection_subtrees() {
    for (level, under_log) in [(3, true), (2, false)] {
        let log = if under_log {
            "# Synthetic\n\n## Log\n\n- **2026-10-02: A** One.\n\n- **2026-10-01: B** Two.\n\n### Action\n\n- [ ] **Do one.** First.\n- [ ] **Do two.** Second.\n"
        } else {
            "# Synthetic\n\n## Log\n\n- [ ] **Do one.** First.\n\n- **2026-10-02: A** One.\n\n- **2026-10-01: B** Two.\n"
        };
        let fx = Fx::new(
            &format!("log-item-overlap-{level}"),
            json!({}),
            &[(LOG, log), (HISTORY, ENVELOPE)],
        );
        let mut config = with_actions(&fx, log_binding());
        config["collections"]["actions"]["level"] = json!(level);
        if !under_log {
            config["collections"]["actions"]["section"] = json!("Log");
        }
        fx.set_config(config);
        let b = fx.item(LOG, "2026-10-01");
        let parent = fx.heading(LOG, &["Synthetic", "Log"]);
        if !under_log {
            fx.refuses(
                "archive-copy",
                fx.copy_request(&b, Some(&parent), HISTORY),
                "protected subtree",
            );
            continue;
        }
        let child = |fx: &Fx| fx.read(LOG)[fx.read(LOG).find("### Action").unwrap()..].to_string();
        let before = child(&fx);
        let copy = fx.copy(&b, &parent, HISTORY);
        fx.ok(
            "delete",
            fx.delete_request(&b, &copy, Some(&parent), HISTORY),
        );
        assert_eq!(child(&fx), before, "the child collection is untouched");
        let action = fx.item(LOG, "Do one.");
        assert!(matches!(
            fx.doc(LOG).resolve(&Selector::Id { id: action }),
            Ok(Target::Item { .. })
        ));
    }
}

/// The delete removes the native extent: the item through the whitespace
/// before the next item, or from the previous item's end for the last one.
/// Separator lines holding spaces and tabs go with it; nothing else changes
/// except the container digest.
#[test]
fn delete_extent_is_the_native_one_and_nothing_else_moves() {
    for which in ["2026-10-02", "2026-10-01"] {
        let fx = Fx::new(
            &format!("log-item-extent-{which}"),
            log_binding(),
            &[
                (
                    LOG,
                    "# Synthetic\n\n## Log\n\n- **2026-10-03: A** One.\n \t\n- **2026-10-02: B** Two.\n\t\n  \n- **2026-10-01: C** Three.\n\nTrailing prose.\n",
                ),
                (HISTORY, ENVELOPE),
            ],
        );
        let id = fx.item(LOG, which);
        let parent = fx.heading(LOG, &["Synthetic", "Log"]);
        let original = fx.read(LOG);
        let doc = Document::parse(&original).unwrap();
        let Target::Item { list, item } = doc.resolve(&Selector::Id { id: id.clone() }).unwrap()
        else {
            panic!()
        };
        let items = &doc.lists[list].items;
        let (start, end) = if item + 1 < items.len() {
            (items[item].span.start, items[item + 1].span.start)
        } else {
            (items[item - 1].span.end, items[item].span.end)
        };
        let gap = format!(
            "{}{}",
            &original[start..items[item].span.start.max(start)],
            &original[items[item].span.end.min(end)..end]
        );
        assert!(gap.contains('\t'), "the case must cover a tab separator");
        let (_, container_id, old_marker) = container(&doc, &id);
        let copy = fx.copy(&id, &parent, HISTORY);
        fx.ok(
            "delete",
            fx.delete_request(&id, &copy, Some(&parent), HISTORY),
        );
        let after = fx.read(LOG);
        let (_, same, new_marker) = container(&fx.doc(LOG), &fx.item(LOG, "2026-10-03"));
        assert_eq!(same, container_id);
        let expected = format!("{}{}", &original[..start], &original[end..]);
        assert_eq!(
            after.replacen(&new_marker, &old_marker, 1),
            expected,
            "{which}"
        );
    }
}

/// The marker text carrying stay `id`.
fn marker_text(doc: &Document, id: &str) -> String {
    let m = doc
        .markers
        .iter()
        .find(|m| m.lexical.id.as_deref() == Some(id))
        .unwrap_or_else(|| panic!("no marker {id}"));
    m.lexical.span.slice(&doc.source).unwrap().to_string()
}

/// `doctor`'s evidence findings for the Log file, from a complete report that
/// read it, so an incomplete or failed scan never passes as "no drift".
fn evidence_findings(fx: &Fx) -> Vec<Value> {
    let (code, report) = fx.run("doctor", json!({"paths":[LOG]}));
    assert_eq!(code, 0, "warnings only, not strict: {report}");
    assert_eq!(report["complete"], true, "{report}");
    assert_eq!(report["summary"]["files_parsed"], 1, "{report}");
    report["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|d| {
            d["code"]
                .as_str()
                .is_some_and(|c| c.starts_with("evidence_"))
        })
        .cloned()
        .collect()
}

/// Evidence refresh and heading-item archive together: a hand-edited Log bullet
/// refuses to archive, `doctor` names its drift, a `refresh` write rewrites only
/// the two digests, and the bullet then archives and deletes through Rivet alone.
#[test]
fn a_drifted_log_bullet_archives_after_a_refresh_write() {
    // `doctor` needs a bound collection, so the file has the tracker's shape:
    // an action collection beside the `items`-enabled Log.
    let tracker = THREE.replacen(
        "## Log",
        "## Action\n\n- [ ] **Do one.** First.\n\n## Log",
        1,
    );
    let fx = Fx::new(
        "log-item-refresh",
        json!({}),
        &[(LOG, &tracker), (HISTORY, ENVELOPE)],
    );
    fx.set_config(with_actions(&fx, log_binding()));
    let id = fx.item(LOG, "2026-10-06");
    let parent = fx.heading(LOG, &["Synthetic", "Log"]);
    let (_, container_id, _) = container(&fx.doc(LOG), &id);

    // A committed hand edit drifts the bullet's subhash and its list's hash.
    let edited = fx
        .read(LOG)
        .replacen("Older body.", "Older body, edited by hand.", 1);
    fx.put(LOG, &edited);
    fx.commit("synthetic hand edit");
    fx.refuses(
        "archive-copy",
        fx.copy_request(&id, Some(&parent), HISTORY),
        "drifted",
    );

    // `doctor` reports exactly those two stays as refreshable drift.
    let drift = evidence_findings(&fx);
    let mut named: Vec<&str> = drift.iter().map(|d| d["id"].as_str().unwrap()).collect();
    named.sort_unstable();
    let mut expected = vec![id.as_str(), container_id.as_str()];
    expected.sort_unstable();
    assert_eq!(named, expected, "{drift:?}");
    assert!(
        drift
            .iter()
            .all(|d| d["code"] == "evidence_drift" && d["evidence"]["refreshable"] == true),
        "{drift:?}"
    );
    let select: Vec<Value> = drift
        .iter()
        .map(|d| json!({"id": d["id"], "expect": d["evidence"]["stored"]}))
        .collect();

    // The refresh rewrites those two digest values and no other byte.
    let before = fx.doc(LOG);
    let (_, map) = fx.run("map", json!({"path":LOG}));
    let out = fx.ok(
        "refresh",
        json!({"path":LOG,"guard":map["revision"],"select":select,
            "message":"refresh: accept the hand edit","session":"test:refresh"}),
    );
    assert_eq!(out["commit"]["status"], "committed", "{out}");
    let refreshed = fx.read(LOG);
    let mut expected_text = edited.clone();
    for r in out["ops"][0]["refreshed"].as_array().unwrap() {
        assert_eq!(r["status"], "refreshed", "{out}");
        let old = marker_text(&before, r["id"].as_str().unwrap());
        let new = old.replacen(
            r["stored"].as_str().unwrap(),
            r["fresh"].as_str().unwrap(),
            1,
        );
        assert_ne!(old, new);
        expected_text = expected_text.replacen(&old, &new, 1);
    }
    assert_eq!(refreshed, expected_text, "only the named digests change");
    assert_eq!(
        evidence_findings(&fx),
        Vec::<Value>::new(),
        "no drift is left"
    );

    // The bullet, with its accepted edit, now archives and deletes.
    let text = item_text(&fx.doc(LOG), &id);
    assert!(text.contains("edited by hand"));
    let copy = fx.copy(&id, &parent, HISTORY);
    let deleted = fx.ok(
        "delete",
        fx.delete_request(&id, &copy, Some(&parent), HISTORY),
    );
    assert_eq!(deleted["commit"]["status"], "committed", "{deleted}");
    let history = fx.read(HISTORY);
    let after = fx.read(LOG);
    assert_eq!(item_text(&Document::parse(&history).unwrap(), &id), text);
    assert_eq!(count(&history, &id), 1);
    assert_eq!(count(&after, &id), 0);
    fresh(&fx.doc(LOG), &container_id);
}
