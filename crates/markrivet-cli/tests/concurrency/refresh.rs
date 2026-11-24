//! `refresh` under the lock, against outside edits, through publication faults
//! and recovery, and end to end with a section archive.
use super::*;
use markrivet_core::Document;

const HAND_EDIT: (&str, &str) = (
    "The release branch is cut;",
    "The release branch is cut by hand;",
);

/// A committed hand edit to the first action item drifts its own `subhash`
/// and its list's `hash`; `doctor` reports both.
fn drifted_store(name: &str) -> (Store, Value) {
    let s = Store::new(name, "refuse");
    let text = read(&s.root, WIDGETS);
    assert_eq!(text.matches(HAND_EDIT.0).count(), 1);
    std::fs::write(s.root.join(WIDGETS), text.replace(HAND_EDIT.0, HAND_EDIT.1)).unwrap();
    s.git(&["commit", "-qam", "hand edit without refreshing evidence"]);
    let select = drift(&s);
    assert_eq!(select.as_array().unwrap().len(), 2, "{select}");
    (s, select)
}

/// `doctor`'s `evidence_drift` findings as a `refresh` selection.
fn drift(s: &Store) -> Value {
    let (code, out) = s.run("doctor", json!({"paths": [WIDGETS]}), &[]);
    assert!(code == 0 || code == 4, "{out}");
    out["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|d| d["code"] == "evidence_drift")
        .map(|d| json!({"id": d["id"], "expect": d["evidence"]["stored"]}))
        .collect()
}

fn document_guard(s: &Store) -> String {
    let (code, out) = s.run("map", json!({"path": WIDGETS}), &[]);
    assert_eq!(code, 0, "{out}");
    out["revision"].as_str().unwrap().to_string()
}

fn refresh_request(guard: &str, select: &Value) -> Value {
    json!({"path": WIDGETS, "guard": guard, "select": select,
        "message": "refresh: accept the hand edit", "session": "test:refresh"})
}

fn current(text: &str, select: &Value) -> bool {
    let doc = Document::parse(text).unwrap();
    select.as_array().unwrap().iter().all(|sel| {
        let i = doc
            .markers
            .iter()
            .position(|m| m.lexical.id.as_deref() == sel["id"].as_str())
            .unwrap();
        let (fresh, stored) = doc.marker_evidence(i).unwrap();
        fresh.starts_with(&stored.unwrap())
    })
}

/// The edit B every interleaving uses: Log prose with no stay, so every stored
/// digest the preview saw is still the stored digest afterwards.
fn content_only_edit(s: &Store) -> Value {
    section_replace(
        &s.root,
        "Log",
        "Newest first, one entry per event.",
        "Newest first, one line per event.",
    )
}

#[test]
fn refresh_commits_its_digests_alone_with_attribution_and_no_delete_receipt() {
    let (s, select) = drifted_store("refresh-commit");
    let before = read(&s.root, WIDGETS);
    let head = s.head();
    let (code, out) = s.run(
        "refresh",
        refresh_request(&document_guard(&s), &select),
        &[],
    );
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["commit"]["status"], "committed", "{out}");
    let after = read(&s.root, WIDGETS);
    assert!(current(&after, &select));
    assert_eq!(after.len(), before.len(), "digests keep their precision");
    let sha = s.head();
    assert_eq!(s.since(&head), std::slice::from_ref(&sha));
    assert_eq!(s.changed(&sha), [WIDGETS]);
    let message = s.message(&sha);
    assert!(
        message.starts_with("refresh: accept the hand edit"),
        "{message}"
    );
    assert_eq!(message.matches("Rivet-Op: ").count(), 1, "{message}");
    assert!(!message.contains("Rivet-Delete"), "{message}");
    let event = s.receipts().last().unwrap().clone();
    assert_eq!(event["outcome"], "committed");
    assert_eq!(event["session"], "test:refresh");
    assert!(s.records().is_empty() && s.clean());
}

#[test]
fn a_content_edit_between_preview_and_apply_refuses_the_preview_guard() {
    let (s, select) = drifted_store("refresh-binding");
    let preimage = document_guard(&s);
    let mut preview = refresh_request(&preimage, &select);
    preview["preview"] = json!(true);
    let (code, out) = s.run("refresh", preview, &[]);
    assert_eq!(code, 0, "{out}");
    assert_ne!(
        out["revision"],
        preimage.as_str(),
        "a preview answers with the candidate"
    );
    let (code, out) = s.run("replace-in-section", content_only_edit(&s), &[]);
    assert_eq!(code, 0, "{out}");
    let b = read(&s.root, WIDGETS);
    assert_eq!(
        drift(&s),
        select,
        "B keeps every stored digest the preview saw"
    );
    let head = s.head();
    let (code, out) = s.run("refresh", refresh_request(&preimage, &select), &[]);
    assert_eq!(code, 2, "{out}");
    assert_eq!(out["error"]["code"], "guard_conflict", "{out}");
    assert_eq!(read(&s.root, WIDGETS), b, "B is preserved");
    assert_eq!(s.head(), head);
    assert!(s.records().is_empty());
}

#[test]
fn an_apply_queued_behind_a_writer_checks_its_guard_after_taking_the_lock() {
    let (s, select) = drifted_store("refresh-queued");
    let preimage = document_guard(&s);
    let first = Pause::new(&s.root, "locked");
    let writer =
        first.wait_for(s.spawn("replace-in-section", &content_only_edit(&s), &[first.env()]));
    // The refresh starts while the writer holds the lock, and can only reach
    // its own `locked` point once the writer has published B.
    let second = Pause::new(&s.home.join("refresh"), "locked");
    let refresher = s.spawn(
        "refresh",
        &refresh_request(&preimage, &select),
        &[second.env()],
    );
    wait_for_writer_lock(&s.root, &refresher);
    assert!(
        !second.dir.join("locked.reached").exists(),
        "the refresh waits for the lock"
    );
    first.release();
    committed_writer(writer);
    let b = read(&s.root, WIDGETS);
    let refresher = second.wait_for(refresher);
    second.release();
    stale_writer(refresher);
    assert_eq!(read(&s.root, WIDGETS), b, "B is preserved");
}

#[test]
fn an_outside_edit_during_refresh_preparation_is_detected() {
    let (s, select) = drifted_store("refresh-external");
    let pause = Pause::new(&s.root, "planned");
    let writer = pause.wait_for(s.spawn(
        "refresh",
        &refresh_request(&document_guard(&s), &select),
        &[pause.env()],
    ));
    let edited = read(&s.root, WIDGETS)
        .replace("Newest first, one entry per event.", "Edited in an editor.");
    std::fs::write(s.root.join(WIDGETS), &edited).unwrap();
    pause.release();
    let (code, out) = finish(writer);
    assert_eq!(code, 2, "{out}");
    assert_eq!(out["error"]["code"], "external_change", "{out}");
    assert_eq!(read(&s.root, WIDGETS), edited);
}

#[test]
fn publication_faults_and_recovery_keep_one_attributed_outcome() {
    // A failed rename publishes nothing.
    let (s, select) = drifted_store("refresh-fault-rename");
    let before = read(&s.root, WIDGETS);
    let head = s.head();
    let (code, out) = s.run(
        "refresh",
        refresh_request(&document_guard(&s), &select),
        &[("RIVET_TEST_FAULT", "rename".into())],
    );
    assert_eq!(code, 2, "{out}");
    assert_eq!(out["error"]["code"], "io", "{out}");
    assert_eq!(read(&s.root, WIDGETS), before);
    assert_eq!(s.head(), head);
    assert!(s.records().is_empty());
    assert_eq!(s.receipts().last().unwrap()["outcome"], "not_applied");

    // Killed after the rename: recovery commits the recorded candidate.
    let (s, select) = drifted_store("refresh-fault-recover");
    let request = refresh_request(&document_guard(&s), &select);
    s.kill_at("after_rename", "refresh", &request, &[]);
    let candidate = read(&s.root, WIDGETS);
    assert!(current(&candidate, &select));
    let op = s.record()["op"].as_str().unwrap().to_string();
    let out = s.recover(json!({}), &[]);
    assert_eq!(one_result(&out)["result"], "committed", "{out}");
    let sha = s.head();
    assert_eq!(s.carrying(&op), std::slice::from_ref(&sha));
    assert_eq!(s.changed(&sha), [WIDGETS]);
    assert!(!s.message(&sha).contains("Rivet-Delete"));
    assert_eq!(read(&s.root, WIDGETS), candidate);
    let events = s.events(&op);
    assert_eq!(events.last().unwrap()["outcome"], "committed");
    assert_eq!(events.last().unwrap()["session"], "test:refresh");
    assert!(s.records().is_empty() && s.clean());

    // Killed after the rename, then edited: recovery never commits later bytes.
    let (s, select) = drifted_store("refresh-fault-later");
    let request = refresh_request(&document_guard(&s), &select);
    s.kill_at("after_rename", "refresh", &request, &[]);
    let later = read(&s.root, WIDGETS).replace(
        "Newest first, one entry per event.",
        "Edited after the crash.",
    );
    std::fs::write(s.root.join(WIDGETS), &later).unwrap();
    let head = s.head();
    let out = s.recover(json!({}), &[]);
    assert_eq!(one_result(&out)["result"], "repair_required", "{out}");
    assert_eq!(one_result(&out)["reason"], "file_changed", "{out}");
    assert_eq!(s.head(), head, "{out}");
    assert_eq!(read(&s.root, WIDGETS), later);
}

fn archive_refused(s: &Store, id: &str) -> Value {
    let before = (read(&s.root, WIDGETS), s.head());
    let (code, out) = s.run("archive-copy", archive_request(s, id), &[]);
    assert_eq!(code, 2, "{out}");
    assert_eq!((read(&s.root, WIDGETS), s.head()), before);
    out
}

/// Ids and their attachment kinds inside the section stayed by `id`.
fn section_owners(text: &str, id: &str) -> Vec<(String, &'static str)> {
    use markrivet_core::{Selector, document::Attach, document::Target};
    let doc = Document::parse(text).unwrap();
    let Target::Section(sec) = doc.resolve(&Selector::Id { id: id.into() }).unwrap() else {
        panic!("{id} is not a section")
    };
    let span = doc.sections[sec].whole;
    doc.markers
        .iter()
        .filter(|m| span.start <= m.lexical.span.start && m.lexical.span.end <= span.end)
        .map(|m| {
            let kind = match m.attach {
                Attach::Item { .. } => "item",
                Attach::Block(b) if doc.sections.iter().any(|x| x.block == b) => "section",
                _ => "block",
            };
            (m.lexical.id.clone().unwrap(), kind)
        })
        .collect()
}

fn archived_owners(s: &Store, id: &str) -> Vec<(String, &'static str)> {
    section_owners(&read(&s.root, "archive/month/closed.md"), id)
}

#[test]
fn a_drifted_section_archives_after_a_refresh_through_rivet_alone() {
    let s = section_archive::matrix_store("refresh-archive", true);
    let id = section_archive::matrix_ids(&s, true)[1].clone();
    let text = read(&s.root, WIDGETS);
    std::fs::write(
        s.root.join(WIDGETS),
        text.replace("- A plain list", "- A plain list, edited by hand"),
    )
    .unwrap();
    s.git(&["commit", "-qam", "hand edit without refreshing evidence"]);
    let out = archive_refused(&s, &id);
    assert!(
        out["error"]["message"]
            .as_str()
            .unwrap()
            .contains("evidence has drifted; review the change, then refresh it"),
        "{out}"
    );
    let select = drift(&s);
    assert!(!select.as_array().unwrap().is_empty());
    let owners = section_owners(&read(&s.root, WIDGETS), &id);
    let (code, out) = s.run(
        "refresh",
        refresh_request(&document_guard(&s), &select),
        &[],
    );
    assert_eq!(code, 0, "{out}");
    assert_eq!(section_owners(&read(&s.root, WIDGETS), &id), owners);
    let (code, copy) = s.run("archive-copy", archive_request(&s, &id), &[]);
    assert_eq!(code, 0, "{copy}");
    assert_eq!(copy["commit"]["status"], "committed", "{copy}");
    assert_eq!(archived_owners(&s, &id), owners);
}

/// The REVIEW_CLI_FAMILY shape: a paragraph's block stay, a standalone
/// `subhash=sha256:0` marker line, then a glued heading whose stay holds the
/// §5.1 digest a default `markstay restamp` writes.
fn review_shape(name: &str, outside_copy: bool) -> (Store, String) {
    let s = section_archive::matrix_store(name, true);
    s.configure(json!({"commit": "off"}));
    let id = section_archive::matrix_ids(&s, true)[1].clone();
    let text = read(&s.root, WIDGETS);
    let doc = Document::parse(&text).unwrap();
    let i = doc
        .markers
        .iter()
        .position(|m| m.lexical.id.as_deref() == Some(id.as_str()))
        .unwrap();
    let marker = &text[doc.markers[i].lexical.span.start..doc.markers[i].lexical.span.end];
    let stored = doc.markers[i].lexical.hash.clone().unwrap();
    // Glue `### Entry` under the paragraph before it, with the two marker lines.
    let glued = text.replace(
        "Earlier history.\n\n### Entry",
        "Earlier history.\n<!-- stay:para0001 hash=sha256:000000000000 -->\n<!-- stay:debris01 subhash=sha256:0 -->\n### Entry",
    );
    assert_ne!(glued, text);
    let para = {
        let d = Document::parse(&glued).unwrap();
        let p = d
            .markers
            .iter()
            .position(|m| m.lexical.id.as_deref() == Some("para0001"))
            .unwrap();
        d.marker_evidence(p).unwrap().0[..12].to_string()
    };
    let mut glued = glued.replace(
        "para0001 hash=sha256:000000000000",
        &format!("para0001 hash=sha256:{para}"),
    );
    let blank = {
        let block = markstay::parse_document(&glued)
            .into_iter()
            .find(|b| {
                b.markers
                    .iter()
                    .any(|m| m.id.as_deref() == Some(id.as_str()))
            })
            .unwrap();
        markstay::body_hash(&block.content, None)[..stored.len()].to_string()
    };
    glued = glued.replace(marker, &marker.replace(&stored, &blank));
    if outside_copy {
        glued = glued.replace(
            "- [ ] **Keep.** A task.",
            &format!("- [ ] **Keep.** A task.\n\n<!-- stay:{id} -->"),
        );
    }
    std::fs::write(s.root.join(WIDGETS), &glued).unwrap();
    s.configure(json!({"commit": "per-write", "on_dirty": "refuse"}));
    s.git(&["add", "-A"]);
    s.git(&["commit", "-qm", "synthetic review shape"]);
    (s, id)
}

#[test]
fn the_review_family_shape_archives_after_refresh_and_an_outside_copy_still_refuses() {
    let (s, id) = review_shape("refresh-review", false);
    let select = drift(&s);
    assert_eq!(
        select
            .as_array()
            .unwrap()
            .iter()
            .map(|x| x["id"].clone())
            .collect::<Vec<_>>(),
        [json!(id)],
        "only the heading's §5.1 digest drifts under Rivet's profile"
    );
    archive_refused(&s, &id);
    let owners = section_owners(&read(&s.root, WIDGETS), &id);
    let (code, out) = s.run(
        "refresh",
        refresh_request(&document_guard(&s), &select),
        &[],
    );
    assert_eq!(code, 0, "{out}");
    assert!(read(&s.root, WIDGETS).contains("<!-- stay:debris01 subhash=sha256:0 -->"));
    let (code, copy) = s.run("archive-copy", archive_request(&s, &id), &[]);
    assert_eq!(code, 0, "{copy}");
    assert_eq!(archived_owners(&s, &id), owners);

    let (s, id) = review_shape("refresh-review-copy", true);
    let stored = Document::parse(&read(&s.root, WIDGETS))
        .unwrap()
        .markers
        .iter()
        .find(|m| m.lexical.id.as_deref() == Some(id.as_str()) && m.lexical.hash.is_some())
        .unwrap()
        .lexical
        .hash
        .clone()
        .unwrap();
    let before = read(&s.root, WIDGETS);
    let (code, out) = s.run(
        "refresh",
        refresh_request(&document_guard(&s), &json!([{"id": id, "expect": stored}])),
        &[],
    );
    assert_eq!(code, 2, "{out}");
    assert_eq!(out["error"]["code"], "duplicate_id", "{out}");
    assert_eq!(read(&s.root, WIDGETS), before);
    // The id no longer resolves, so take the section's guard by its title.
    let doc = Document::parse(&before).unwrap();
    let sec = doc
        .sections
        .iter()
        .position(|x| x.path.last().is_some_and(|t| t == "Entry"))
        .unwrap();
    let mut request = archive_request_unguarded(&s, &id);
    request["guard"] = json!(doc.target_revision(markrivet_core::document::Target::Section(sec)));
    let head = s.head();
    let (code, out) = s.run("archive-copy", request, &[]);
    assert_eq!(code, 2, "{out}");
    assert_eq!(out["error"]["code"], "duplicate_id", "{out}");
    assert_eq!(out["error"]["id"], id.as_str(), "{out}");
    assert_eq!((read(&s.root, WIDGETS), s.head()), (before, head));
}

/// `archive_request` without the source guard lookup by id.
fn archive_request_unguarded(s: &Store, id: &str) -> Value {
    let select = json!({"kind":"heading_path","path":["Closed"]});
    let (_, dest) = s.run(
        "get",
        json!({"path":"archive/month/closed.md","select":select}),
        &[],
    );
    let doc = Document::parse(&read(&s.root, WIDGETS)).unwrap();
    let log = doc
        .sections
        .iter()
        .position(|x| x.path.last().is_some_and(|t| t == "Log"))
        .map(|i| doc.section_ids(i)[0].clone())
        .unwrap();
    json!({"source":WIDGETS,"id":id,"guard":"","path":"archive/month/closed.md",
        "select":select,"destination_guard":dest["revision"],"parent":log})
}
