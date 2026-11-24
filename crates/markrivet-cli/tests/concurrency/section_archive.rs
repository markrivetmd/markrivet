use super::*;
use markrivet_core::{Document, Selector, document::Target};

const ARCHIVE: &str = "archive/month/closed.md";

fn config(s: &Store, change: impl FnOnce(&mut Value)) {
    let path = s.root.join(".rivet/project.json");
    let mut value: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    change(&mut value);
    std::fs::write(path, value.to_string()).unwrap();
}

fn commit_fixture(s: &Store) {
    s.git(&["add", "-A"]);
    s.git(&["commit", "-qm", "synthetic section fixture"]);
}

fn source(s: &Store, text: &str) {
    s.configure(json!({"commit":"off"}));
    std::fs::write(s.root.join(WIDGETS), text).unwrap();
    let (_, mapped) = s.run("map", json!({"path":WIDGETS}), &[]);
    let (code, out) = s.run(
        "stamp",
        json!({"path":WIDGETS,"guard":mapped["revision"],"select":{"kind":"document"}}),
        &[],
    );
    assert_eq!(code, 0, "{out}");
    s.configure(json!({"commit":"per-write","on_dirty":"refuse"}));
    commit_fixture(s);
}

pub(super) fn matrix_store(name: &str, section: bool) -> Store {
    let s = archive_store(name);
    if section {
        config(&s, |c| {
            c["collections"]["actions"]["archive"] = json!(["items/**/*.md"]);
            c["section_archives"] =
                json!({"history":{"files":["tracking/*.md"],"archive":["archive/**/*.md"]}});
        });
        source(
            &s,
            "# Synthetic\n\n## Action\n\n- [ ] **Keep.** A task.\n\n## Log\n\n### Earlier\n\nEarlier history.\n\n### Entry\n\nRefresh the fixtures.\n\n- A plain list\n  - A nested plain item\n\n#### Detail\n\nNested prose.\n",
        );
    }
    s
}

fn section_id(s: &Store, title: &str) -> String {
    let doc = Document::parse(&read(&s.root, WIDGETS)).unwrap();
    let i = doc
        .sections
        .iter()
        .position(|sec| sec.path.last().is_some_and(|t| t == title))
        .unwrap();
    doc.section_ids(i)[0].clone()
}

pub(super) fn matrix_ids(s: &Store, section: bool) -> Vec<String> {
    if section {
        vec![section_id(s, "Earlier"), section_id(s, "Entry")]
    } else {
        s.items()
    }
}

pub(super) fn parent_stay(s: &Store, id: &str) -> Option<String> {
    let doc = Document::parse(&read(&s.root, WIDGETS)).unwrap();
    let target = doc.resolve(&Selector::Id { id: id.into() }).unwrap();
    match target {
        Target::Section(i) => doc.sections[i]
            .parent
            .map(|p| doc.section_ids(p)[0].clone()),
        _ => None,
    }
}

pub(super) fn add_parent(s: &Store, id: &str, request: &mut Value) {
    if let Some(parent) = parent_stay(s, id) {
        request["archive"]["parent"] = json!(parent);
    }
}

fn deletion(s: &Store, id: &str, copy: &Value) -> Value {
    let mut request = receipted_request(s, id);
    request["archive"] =
        json!({"path":ARCHIVE,"copy_op":copy["commit"]["copy_op"],"commit":copy["commit"]["sha"]});
    add_parent(s, id, &mut request);
    request
}

fn unchanged(s: &Store, command: &str, request: Value, reason: &str) -> Value {
    let before = (
        read(&s.root, WIDGETS),
        read(&s.root, ARCHIVE),
        s.head(),
        std::fs::read(s.root.join(".git/index")).unwrap(),
        s.records(),
    );
    let (code, out) = s.run(command, request, &[]);
    assert_eq!(code, 2, "{reason}: {out}");
    assert!(out.to_string().contains(reason), "expected {reason}: {out}");
    assert_eq!(read(&s.root, WIDGETS), before.0);
    assert_eq!(read(&s.root, ARCHIVE), before.1);
    assert_eq!(s.head(), before.2);
    assert_eq!(std::fs::read(s.root.join(".git/index")).unwrap(), before.3);
    assert_eq!(s.records(), before.4);
    out
}

fn attachments(doc: &Document, ids: &[String]) -> Vec<&'static str> {
    ids.iter()
        .map(
            |id| match doc.resolve(&Selector::Id { id: id.clone() }).unwrap() {
                Target::Section(_) => "section",
                Target::Item { .. } => "item",
                Target::Block(_) => "block",
                _ => "other",
            },
        )
        .collect()
}

#[test]
fn identity_content_and_attachments_match_the_native_delete() {
    let s = matrix_store("section-identity", true);
    source(
        &s,
        "# Synthetic\n\n## Log\n\n### Entry {#Entry001}\n\nStayed prose. <!-- stay:Para0001 -->\n\n- A plain list\n  - A nested plain item\n\n#### Detail\n\nNested prose.\n\n```\n<!-- stay:Code0001 -->\n```\n",
    );
    let id = section_id(&s, "Entry");
    let source = read(&s.root, WIDGETS);
    let doc = Document::parse(&source).unwrap();
    let (code, preview) = s.run(
        "delete",
        {
            let mut r = receipted_request(&s, &id);
            r["preview"] = json!(true);
            r
        },
        &[],
    );
    assert_eq!(code, 0, "{preview}");
    let (code, copy) = s.run("archive-copy", archive_request(&s, &id), &[]);
    assert_eq!(code, 0, "{copy}");
    let ids: Vec<String> =
        serde_json::from_value(copy["commit"]["archive_copy"]["copy"]["ids"].clone()).unwrap();
    let dropped = preview["ops"][0]["dropped"].clone();
    assert_eq!(json!(ids), dropped, "{preview}");
    let archive = Document::parse(&read(&s.root, ARCHIVE)).unwrap();
    assert_eq!(attachments(&doc, &ids), attachments(&archive, &ids));
    assert!(ids.len() >= 4, "{ids:?}");
    assert_eq!(read(&s.root, WIDGETS), source);
    let (code, deleted) = s.run("delete", deletion(&s, &id, &copy), &[]);
    assert_eq!(code, 0, "{deleted}");
    let events = s.events(deleted["commit"]["op"].as_str().unwrap());
    let receipt = &events[0]["delete_receipt"];
    let mut canonical_ids = ids.clone();
    canonical_ids.sort();
    assert_eq!(
        receipt["operation"]["dropped"],
        json!(canonical_ids),
        "{deleted}"
    );
    let (code, verified) = s.run(
        "verify-delete",
        json!({"parent":source,"candidate":read(&s.root,WIDGETS),"receipt":receipt}),
        &[],
    );
    assert_eq!(code, 0, "{verified}");
}

#[test]
fn separators_reference_definitions_and_recovery_keep_original_provenance() {
    for nl in ["\n", "\r\n"] {
        for blanks in [0, 1, 3] {
            for sibling in [false, true] {
                for ending in [
                    "Body.\n",
                    "[reference]: /synthetic\n",
                    "[reference]: /synthetic\n  \"multiline title\"\n",
                    "```\ncode\n```\n",
                    "Body.\n\u{a0}\n",
                ] {
                    let s = matrix_store("section-separators", true);
                    let text = format!(
                        "# Synthetic\n\n## Log\n\n### A\n\n{ending}{}### B\n\nSecond.\n",
                        " \n\t\n".repeat(blanks)
                    );
                    source(&s, &text.replace('\n', nl));
                    // Exercise EOF and an existing following equal-level destination.
                    s.configure(json!({"commit":"off"}));
                    std::fs::write(
                        s.root.join(ARCHIVE),
                        format!(
                            "# Archive\n\n## Closed\n{}",
                            if sibling {
                                "\n## Following\n\nKeep.\n"
                            } else {
                                ""
                            }
                        )
                        .replace('\n', nl),
                    )
                    .unwrap();
                    stamp(&s.root, ARCHIVE);
                    s.configure(json!({"commit":"per-write","on_dirty":"refuse"}));
                    commit_fixture(&s);
                    let a = section_id(&s, "A");
                    let b = section_id(&s, "B");
                    let request = archive_request(&s, &a);
                    let original = read(&s.root, WIDGETS);
                    let (code, copy) = s.run("archive-copy", request.clone(), &[]);
                    assert_eq!(code, 0, "{nl:?} {blanks} {sibling} {ending:?}: {copy}");
                    let (code, appended) = s.run("archive-copy", archive_request(&s, &b), &[]);
                    assert_eq!(code, 0, "{appended}");
                    let (code, retry) = s.run("archive-copy", request, &[]);
                    assert_eq!(code, 0, "{retry}");
                    assert_eq!(retry["status"], "unchanged");
                    assert_eq!(retry["commit"]["sha"], copy["commit"]["sha"]);
                    let archive = read(&s.root, ARCHIVE);
                    assert!(archive.contains(&ending.replace('\n', nl)));
                    let delete = deletion(&s, &a, &copy);
                    s.kill_at("after_rename", "delete", &delete, &[]);
                    let candidate = read(&s.root, WIDGETS);
                    let op = s.record()["op"].as_str().unwrap().to_string();
                    let recovered = s.recover(json!({}), &[]);
                    assert_eq!(one_result(&recovered)["result"], "committed", "{recovered}");
                    assert_eq!(read(&s.root, ARCHIVE), archive);
                    assert_eq!(read(&s.root, WIDGETS), candidate);
                    assert!(s.records().is_empty());
                    let events = s.events(&op);
                    let receipt = &events[0]["delete_receipt"];
                    let (code, verified) = s.run(
                        "verify-delete",
                        json!({"parent":original,"candidate":candidate,"receipt":receipt}),
                        &[],
                    );
                    assert_eq!(code, 0, "{recovered}: {verified}");
                }
            }
        }
    }
}

/// A copy that lands at end of file ends with the source unit's own trailing
/// blank lines, so the archived unit reads with the frozen source guard. A copy
/// before a following section keeps the single separator newline.
#[test]
fn eof_copy_reads_with_the_frozen_source_guard_and_mid_file_bytes_are_unchanged() {
    for nl in ["\n", "\r\n"] {
        for tail in ["", "\n", " \n\t\n\n"] {
            for source_last in [true, false] {
                for sibling in [false, true] {
                    let case = format!("{nl:?} {tail:?} last={source_last} sibling={sibling}");
                    let s = matrix_store("section-eof-revision", true);
                    let text = if source_last {
                        format!(
                            "# Synthetic\n\n## Log\n\n### B\n\nSecond.\n\n### A\n\nBody.\n{tail}"
                        )
                    } else {
                        format!("# Synthetic\n\n## Log\n\n### A\n\nBody.\n{tail}### B\n\nSecond.\n")
                    };
                    source(&s, &text.replace('\n', nl));
                    s.configure(json!({"commit":"off"}));
                    std::fs::write(
                        s.root.join(ARCHIVE),
                        format!(
                            "# Archive\n\n## Closed\n{}",
                            if sibling {
                                "\n## Following\n\nKeep.\n"
                            } else {
                                ""
                            }
                        )
                        .replace('\n', nl),
                    )
                    .unwrap();
                    stamp(&s.root, ARCHIVE);
                    s.configure(json!({"commit":"per-write","on_dirty":"refuse"}));
                    commit_fixture(&s);
                    let a = section_id(&s, "A");
                    let request = archive_request(&s, &a);
                    let source_doc = Document::parse(&read(&s.root, WIDGETS)).unwrap();
                    let Target::Section(i) =
                        source_doc.resolve(&Selector::Id { id: a.clone() }).unwrap()
                    else {
                        panic!("{case}: A is a section");
                    };
                    let whole = source_doc.sections[i]
                        .whole
                        .slice(&source_doc.source)
                        .unwrap();
                    // Every tail line is unowned blank, so the content extent excludes it.
                    let content = whole.strip_suffix(&tail.replace('\n', nl)).unwrap();
                    assert!(content.ends_with(&format!("Body.{nl}")), "{case}");
                    let before = read(&s.root, ARCHIVE);
                    let (code, copy) = s.run("archive-copy", request.clone(), &[]);
                    assert_eq!(code, 0, "{case}: {copy}");
                    let after = read(&s.root, ARCHIVE);
                    if sibling {
                        // The pre-fix shape: blank line, content, one separator newline.
                        let at = before.find("## Following").unwrap();
                        let mut expected = before[..at].to_string();
                        if !expected.ends_with('\n') {
                            expected.push_str(nl);
                        }
                        if !expected.ends_with(&format!("{nl}{nl}")) {
                            expected.push_str(nl);
                        }
                        expected.push_str(content);
                        expected.push_str(nl);
                        expected.push_str(&before[at..]);
                        assert_eq!(after, expected, "{case}");
                    } else {
                        assert!(after.ends_with(whole), "{case}: {after:?}");
                        assert_eq!(
                            item_guard(&s.root, ARCHIVE, &a),
                            request["guard"].as_str().unwrap(),
                            "{case}: archived unit revision differs from its frozen guard"
                        );
                    }
                    let (code, retry) = s.run("archive-copy", request, &[]);
                    assert_eq!(code, 0, "{case}: {retry}");
                    assert_eq!(retry["status"], "unchanged", "{case}");
                    let (code, deleted) = s.run("delete", deletion(&s, &a, &copy), &[]);
                    assert_eq!(code, 0, "{case}: {deleted}");
                    assert_eq!(read(&s.root, ARCHIVE), after, "{case}");
                }
            }
        }
    }
}

#[test]
fn item_copy_to_end_of_file_reads_with_the_frozen_source_guard() {
    let s = archive_store("item-eof-revision");
    let ids = s.items();
    // The fixture's Closed section ends the archive file.
    let request = archive_request(&s, &ids[1]);
    let (code, copy) = s.run("archive-copy", request.clone(), &[]);
    assert_eq!(code, 0, "{copy}");
    assert_eq!(
        item_guard(&s.root, ARCHIVE, &ids[1]),
        request["guard"].as_str().unwrap()
    );
}

#[test]
fn terminal_owned_content_and_consumed_indentation_refuse_without_publication() {
    for nl in ["\n", "\r\n"] {
        for ending in [
            "body",
            "- <?\n ",
            "```\ncontent\n\n",
            "```\ncontent\n \n",
            "```\ncontent\n\t\n",
            "<pre>\ncontent\n\n",
            "<pre>\ncontent\n \n",
            "<pre>\ncontent\n\t\n",
        ] {
            let s = matrix_store("section-owned-eof", true);
            source(
                &s,
                &format!("# Synthetic\n\n## Log\n\n### Entry\n\n{ending}").replace('\n', nl),
            );
            let request = archive_request(&s, &section_id(&s, "Entry"));
            unchanged(
                &s,
                "archive-copy",
                request,
                if ending.ends_with('\n') {
                    "parser-owned"
                } else {
                    "terminated final physical line"
                },
            );
        }
    }
}

#[test]
fn placement_and_parent_preconditions_are_explicit() {
    let s = matrix_store("section-placement", true);
    let id = section_id(&s, "Entry");
    let mut request = archive_request(&s, &id);
    let parent = request["parent"].clone();
    request.as_object_mut().unwrap().remove("parent");
    unchanged(&s, "archive-copy", request.clone(), "requires a parent");
    request["parent"] = json!(id);
    unchanged(&s, "archive-copy", request.clone(), "direct parent");
    request["parent"] = parent;
    // Existing descendants close when the correctly leveled incoming heading arrives.
    s.configure(json!({"commit":"off"}));
    std::fs::write(
        s.root.join(ARCHIVE),
        "# Archive\n\n## Closed\n\n### Existing\n\n#### Deep\n\nKeep.\n",
    )
    .unwrap();
    stamp(&s.root, ARCHIVE);
    s.configure(json!({"commit":"per-write","on_dirty":"refuse"}));
    commit_fixture(&s);
    let (code, out) = s.run("archive-copy", archive_request(&s, &id), &[]);
    assert_eq!(code, 0, "{out}");
    let doc = Document::parse(&read(&s.root, ARCHIVE)).unwrap();
    let Target::Section(i) = doc.resolve(&Selector::Id { id }).unwrap() else {
        panic!()
    };
    assert_eq!(
        doc.sections[doc.sections[i].parent.unwrap()]
            .path
            .last()
            .unwrap(),
        "Closed"
    );
    for heading in ["##", "####"] {
        let s = matrix_store("section-level", true);
        source(
            &s,
            &format!("# Synthetic\n\n## Log\n\n{heading} Entry\n\nBody.\n"),
        );
        let request = archive_request(&s, &section_id(&s, "Entry"));
        unchanged(&s, "archive-copy", request, "one heading level");
    }
    let s = matrix_store("section-legacy-parent", true);
    let doc = Document::parse(&read(&s.root, WIDGETS)).unwrap();
    let p = doc
        .sections
        .iter()
        .position(|s| s.path.last().unwrap() == "Log")
        .unwrap();
    let mut text = doc.source.clone();
    text.replace_range(
        doc.sections[p].heading.start..doc.sections[p].heading.end,
        "## Log {#parent01}",
    );
    std::fs::write(s.root.join(WIDGETS), text).unwrap();
    commit_fixture(&s);
    let request = archive_request(&s, &section_id(&s, "Entry"));
    unchanged(&s, "archive-copy", request, "stamped direct parent");
}

#[test]
fn eligibility_refusals_leave_every_write_surface_identical() {
    for variant in [
        "unaddressed",
        "duplicate-outside",
        "legacy-child",
        "drift",
        "title",
        "unstamped",
        "destination-id",
        "other-copy",
        "item-parent",
        "item-delete-parent",
        "protected-heading",
        "protected-excluded",
    ] {
        let s = matrix_store("section-eligibility", true);
        let mut id = section_id(&s, "Entry");
        let mut request = archive_request(&s, &id);
        let reason = match variant {
            "unaddressed" => {
                source(
                    &s,
                    "# Synthetic\n\n## Log\n\n### Entry\n\n- outer\n  - inner <!-- stay:Loose001 subhash=sha256:111111111111 -->\n",
                );
                id = section_id(&s, "Entry");
                request = archive_request(&s, &id);
                "unaddressed stay Loose001"
            }
            "duplicate-outside" => {
                let text = format!(
                    "{}\n## Outside\n\n- outer\n  - inner <!-- stay:{id} subhash=sha256:111111111111 -->\n",
                    read(&s.root, WIDGETS)
                );
                std::fs::write(s.root.join(WIDGETS), text).unwrap();
                commit_fixture(&s);
                let doc = Document::parse(&read(&s.root, WIDGETS)).unwrap();
                let entry = doc
                    .sections
                    .iter()
                    .position(|s| s.path.last().unwrap() == "Entry")
                    .unwrap();
                request["guard"] = json!(doc.target_revision(Target::Section(entry)));
                "not unique"
            }
            "legacy-child" => {
                let doc = Document::parse(&read(&s.root, WIDGETS)).unwrap();
                let child = doc
                    .sections
                    .iter()
                    .find(|s| s.path.last().unwrap() == "Detail")
                    .unwrap();
                let mut text = doc.source.clone();
                text.replace_range(
                    child.heading.start..child.heading.end,
                    "#### Detail {#Legacy01}",
                );
                std::fs::write(s.root.join(WIDGETS), text).unwrap();
                commit_fixture(&s);
                request = archive_request(&s, &id);
                "legacy-only heading stay Legacy01"
            }
            "drift" => {
                let text = read(&s.root, WIDGETS).replace("Nested prose.", "Changed nested prose.");
                // Give a nested paragraph its own stale addressed marker.
                let text = text.replace(
                    "Changed nested prose.",
                    "Changed nested prose. <!-- stay:Drift001 hash=sha256:111111111111 -->",
                );
                std::fs::write(s.root.join(WIDGETS), text).unwrap();
                commit_fixture(&s);
                request = archive_request(&s, &id);
                "Drift001 evidence has drifted"
            }
            "title" => {
                id = section_id(&s, "Synthetic");
                request = archive_request(&s, &id);
                "level-1 title"
            }
            "unstamped" => {
                let doc = Document::parse(&read(&s.root, WIDGETS)).unwrap();
                let entry = doc
                    .sections
                    .iter()
                    .find(|s| s.path.last().unwrap() == "Entry")
                    .unwrap();
                let mut text = doc.source.clone();
                text.replace_range(
                    entry.heading.start..entry.heading.end,
                    "### Entry {#Entry001}",
                );
                std::fs::write(s.root.join(WIDGETS), text).unwrap();
                commit_fixture(&s);
                request = archive_request(&s, "Entry001");
                "own heading stay"
            }
            "destination-id" => {
                let text = format!(
                    "{}\n### Existing\n<!-- stay:{id} -->\n\nBody.\n",
                    read(&s.root, ARCHIVE)
                );
                std::fs::write(s.root.join(ARCHIVE), text).unwrap();
                commit_fixture(&s);
                "marker ownership differ"
            }
            "other-copy" => {
                let other = section_id(&s, "Earlier");
                let (code, copy) = s.run("archive-copy", archive_request(&s, &other), &[]);
                assert_eq!(code, 0, "{copy}");
                let request = deletion(&s, &id, &copy);
                unchanged(&s, "delete", request, "provenance does not describe");
                continue;
            }
            "item-parent" | "item-delete-parent" => {
                let s = archive_store("item-parent-refusal");
                let id = &s.items()[1];
                let mut request = archive_request(&s, id);
                if variant == "item-parent" {
                    request["parent"] = json!("Parent01");
                    unchanged(&s, "archive-copy", request, "only valid for a section");
                } else {
                    let (code, copy) = s.run("archive-copy", request, &[]);
                    assert_eq!(code, 0, "{copy}");
                    let mut request = deletion(&s, id, &copy);
                    request["archive"]["parent"] = json!("Parent01");
                    unchanged(&s, "delete", request, "only valid for a section");
                }
                continue;
            }
            "protected-heading" | "protected-excluded" => {
                config(&s, |c| {
                    c["collections"]["actions"]["subsections"] = json!(true);
                    c["collections"]["actions"]["exclude"] = json!(["Plans"]);
                });
                source(
                    &s,
                    "# Synthetic\n\n## Action\n\n### Plans\n\nPlain excluded content.\n\n## Log\n\nKeep.\n",
                );
                id = section_id(
                    &s,
                    if variant == "protected-heading" {
                        "Action"
                    } else {
                        "Plans"
                    },
                );
                request = archive_request(&s, &id);
                "protected subtree"
            }
            _ => unreachable!(),
        };
        unchanged(&s, "archive-copy", request, reason);
    }
}

#[test]
fn section_task_refusal_keeps_readiness_unchanged_even_for_nested_checked_tasks() {
    for task in [
        "- [x] **Finished.** <!-- stay:Checked1 subhash=sha256:111111111111 -->",
        "- plain\n  - [x] checked nested task",
    ] {
        let s = matrix_store("section-readiness", true);
        let schema_path = s.root.join(".rivet/schemas/tracker-action.json");
        let mut schema: Value =
            serde_json::from_str(&std::fs::read_to_string(&schema_path).unwrap()).unwrap();
        schema["fields"]["waits"] = json!({"type":"string","from":"label","label":"Waits"});
        std::fs::write(schema_path, schema.to_string()).unwrap();
        source(
            &s,
            &format!(
                "# Synthetic\n\n## Action\n\n- [ ] **Waiter.**\n  **Waits**: item:Checked1\n\n## Log\n\n### Entry\n\n{task}\n"
            ),
        );
        if task.contains("Checked1") {
            let doc = Document::parse(&read(&s.root, WIDGETS)).unwrap();
            let i = doc
                .markers
                .iter()
                .position(|m| m.lexical.id.as_deref() == Some("Checked1"))
                .unwrap();
            let (fresh, _) = doc.marker_evidence(i).unwrap();
            let text = doc.source.replace("111111111111", &fresh[..12]);
            std::fs::write(s.root.join(WIDGETS), text).unwrap();
            commit_fixture(&s);
        }
        let query = json!({"collection":"actions","paths":[WIDGETS],"ready":{"field":"waits"}});
        let before = s.run("query", query.clone(), &[]);
        assert_eq!(before.0, 0, "{}", before.1);
        if task.contains("Checked1") {
            assert_eq!(before.1["readiness"]["ready"], 1, "{}", before.1);
            assert_eq!(before.1["items"][0]["wait"]["on"]["id"], "Checked1");
        } else {
            assert_eq!(before.1["readiness"]["unresolved"], 1, "{}", before.1);
        }
        let request = archive_request(&s, &section_id(&s, "Entry"));
        unchanged(&s, "archive-copy", request, "task-list item");
        assert_eq!(s.run("query", query, &[]), before);
    }
}

#[test]
fn shared_section_namespaces_work_without_collections_and_keep_live_precedence() {
    let s = matrix_store("section-only", true);
    config(&s, |c| {
        c["collections"] = json!({});
    });
    commit_fixture(&s);
    let (code, out) = s.run("archive-mkdir", json!({"path":"archive/new-month"}), &[]);
    assert_eq!(code, 0, "{out}");
    assert!(s.root.join("archive/new-month").is_dir());
    let id = section_id(&s, "Entry");
    let (code, copy) = s.run("archive-copy", archive_request(&s, &id), &[]);
    assert_eq!(code, 0, "{copy}");
    let project = markrivet_core::fs::Project::open(&s.root).unwrap();
    let config = markrivet_core::schema::load_config(&project).unwrap();
    let (files, _, errors) =
        markrivet_core::citations::scope(&project, &config, &Default::default());
    assert!(errors.is_empty(), "{errors:?}");
    assert!(files.contains(&(WIDGETS.into(), false)));
    assert!(files.contains(&(ARCHIVE.into(), true)));
    let mut carriers = markrivet_core::citations::IdMap::default();
    for (path, archived) in files {
        carriers.add(
            &path,
            &Document::parse(&project.read(&path).unwrap()).unwrap(),
            archived,
        );
    }
    let citation_doc = Document::parse(&format!("See section:{id}.\n")).unwrap();
    let citation = markrivet_core::citations::citations(&citation_doc)
        .pop()
        .unwrap();
    assert!(carriers.check(&citation).is_none());
    let (code, out) = s.run("delete", deletion(&s, &id, &copy), &[]);
    assert_eq!(code, 0, "{out}");
    let mut carriers = markrivet_core::citations::IdMap::default();
    for (path, archived) in
        markrivet_core::citations::scope(&project, &config, &Default::default()).0
    {
        carriers.add(
            &path,
            &Document::parse(&project.read(&path).unwrap()).unwrap(),
            archived,
        );
    }
    assert!(carriers.check(&citation).is_none());
}

#[test]
fn section_archive_glob_overlap_refuses_before_any_file_exists() {
    for (live, history, item_archive) in [
        ("tracking/*.md", "tracking/HISTORY_*.md", "items/*.md"),
        ("tracking/**/LOG_?.md", "**/LOG_a.md", "items/*.md"),
        (
            "tracking/*.md",
            "archive/*/history.md",
            "archive/**/h?story.md",
        ),
    ] {
        let root = scratch("section-glob-overlap");
        std::fs::create_dir(root.join(".rivet")).unwrap();
        std::fs::write(root.join(".rivet/project.json"),json!({"collections":{"tasks":{"schema":"not-loaded@1","files":[live],"section":"Action","archive":[item_archive]}},"section_archives":{"history":{"files":["other/*.md"],"archive":[history]}}}).to_string()).unwrap();
        let project = markrivet_core::fs::Project::open(&root).unwrap();
        let error = markrivet_core::schema::load_config(&project).unwrap_err();
        assert!(error.message.contains("overlaps"), "{error:?}");
    }
    let root = scratch("section-other-source-overlap");
    std::fs::create_dir(root.join(".rivet")).unwrap();
    std::fs::write(root.join(".rivet/project.json"),json!({"section_archives":{"one":{"files":["one/*.md"],"archive":["two/*.md"]},"two":{"files":["two/*.md"],"archive":["archive/two/*.md"]}}}).to_string()).unwrap();
    let project = markrivet_core::fs::Project::open(&root).unwrap();
    assert!(
        markrivet_core::schema::load_config(&project)
            .unwrap_err()
            .message
            .contains("overlaps")
    );
}

#[test]
fn section_duplicate_search_covers_other_history_and_collection_archives() {
    for other in ["items/month/other.md", "history/month/other.md"] {
        let s = matrix_store("section-global-duplicate", true);
        config(&s, |c| {
            c["section_archives"]["other"] =
                json!({"files":["other/*.md"],"archive":["history/**/*.md"]});
        });
        let id = section_id(&s, "Entry");
        std::fs::create_dir_all(s.root.join(other).parent().unwrap()).unwrap();
        std::fs::write(
            s.root.join(other),
            format!("# Other\n\n### Entry\n<!-- stay:{id} -->\n\nCopy.\n"),
        )
        .unwrap();
        // A live citation does not excuse a second archive carrier.
        let text = format!(
            "{}\n## Citation\n\nSee section:{id}.\n",
            read(&s.root, WIDGETS)
        );
        std::fs::write(s.root.join(WIDGETS), text).unwrap();
        commit_fixture(&s);
        let request = archive_request(&s, &id);
        unchanged(
            &s,
            "archive-copy",
            request,
            "another archive already carries",
        );
    }
}

#[test]
fn item_copy_and_retry_ignore_unrelated_conflicting_or_unreadable_archives() {
    for bad in ["conflicting", "symlink"] {
        let s = archive_store("item-unrelated-archive");
        config(&s, |c| {
            c["section_archives"] =
                json!({"history":{"files":["tracking/*.md"],"archive":["history/**/*.md"]}});
        });
        let id = &s.items()[1];
        std::fs::create_dir_all(s.root.join("history/month")).unwrap();
        if bad == "symlink" {
            std::os::unix::fs::symlink("missing", s.root.join("history/month/history.md")).unwrap();
        } else {
            std::fs::write(
                s.root.join("history/month/history.md"),
                format!(
                    "# Other\n\n- [x] conflicting <!-- stay:{id} subhash=sha256:111111111111 -->\n"
                ),
            )
            .unwrap();
        }
        // The collection's broad archive namespace must be narrowed away from history.
        config(&s, |c| {
            c["collections"]["actions"]["archive"] = json!(["archive/**/*.md"]);
        });
        commit_fixture(&s);
        let request = archive_request(&s, id);
        let (code, copy) = s.run("archive-copy", request.clone(), &[]);
        assert_eq!(code, 0, "{copy}");
        let (code, retry) = s.run("archive-copy", request, &[]);
        assert_eq!(code, 0, "{retry}");
        assert_eq!(retry["status"], "unchanged");
    }
}

#[test]
fn ancestry_is_rechecked_under_the_writer_lock_for_copy_delete_and_retry() {
    for command in ["archive-copy", "delete"] {
        let s = matrix_store("section-ancestry-race", true);
        // A skipped source level lets a native insert become the existing entry's
        // new direct parent while leaving all entry bytes and its revision intact.
        source(&s, "# Synthetic\n\n## Log\n\n#### Entry\n\nBody.\n");
        s.configure(json!({"commit":"off"}));
        std::fs::write(
            s.root.join(ARCHIVE),
            "# Archive\n\n## Container\n\n### Closed\n",
        )
        .unwrap();
        stamp(&s.root, ARCHIVE);
        s.configure(json!({"commit":"per-write","on_dirty":"refuse"}));
        commit_fixture(&s);
        let id = section_id(&s, "Entry");
        let mut copy_request = archive_request(&s, &id);
        copy_request["select"] = json!({"kind":"heading_path","path":["Container","Closed"]});
        let (_, dest) = s.run(
            "get",
            json!({"path":ARCHIVE,"select":copy_request["select"]}),
            &[],
        );
        copy_request["destination_guard"] = dest["revision"].clone();
        let request = if command == "delete" {
            let (code, copy) = s.run("archive-copy", copy_request.clone(), &[]);
            assert_eq!(code, 0, "{copy}");
            deletion(&s, &id, &copy)
        } else {
            copy_request.clone()
        };
        let parent = section_id(&s, "Log");
        let insert = json!({"path":WIDGETS,"select":{"kind":"id","id":parent},"guard":item_guard(&s.root,WIDGETS,&parent),"position":"start","content":"### New parent\n","stamp":true});
        let pause = Pause::new(&s.root, "before_rename");
        let published = Pause {
            dir: pause.dir.clone(),
            point: "after_rename",
        };
        let both = (
            "RIVET_TEST_PAUSE",
            format!("before_rename,after_rename:{}", pause.dir.display()),
        );
        let mover = s.spawn("insert", &insert, &[both]);
        pause.wait();
        let waiting = s.spawn(command, &request, &[]);
        wait_for_writer_lock(&s.root, &waiting);
        pause.release();
        published.wait();
        let moved = read(&s.root, WIDGETS);
        assert_eq!(
            item_guard(&s.root, WIDGETS, &id),
            request["guard"].as_str().unwrap()
        );
        published.release();
        let (code, out) = finish(mover);
        assert_eq!(code, 0, "{out}");
        let archive = read(&s.root, ARCHIVE);
        let head = s.head();
        let index = std::fs::read(s.root.join(".git/index")).unwrap();
        let (code, out) = finish(waiting);
        assert_eq!(code, 2, "{out}");
        assert!(out.to_string().contains("direct parent"), "{out}");
        assert_eq!(read(&s.root, WIDGETS), moved);
        assert_eq!(read(&s.root, ARCHIVE), archive);
        assert_eq!(s.head(), head);
        assert_eq!(std::fs::read(s.root.join(".git/index")).unwrap(), index);
        assert!(s.records().is_empty());
        unchanged(&s, command, request, "direct parent");
        // A wrapper resume of the frozen copy also retains the original parent.
        unchanged(&s, "archive-copy", copy_request, "direct parent");
    }
}

#[test]
fn corrupted_nested_archived_stay_requires_repair_and_keeps_the_delete_candidate() {
    let s = matrix_store("section-corrupt-recovery", true);
    let id = section_id(&s, "Entry");
    let (code, copy) = s.run("archive-copy", archive_request(&s, &id), &[]);
    assert_eq!(code, 0, "{copy}");
    let request = deletion(&s, &id, &copy);
    s.kill_at("after_rename", "delete", &request, &[]);
    let candidate = read(&s.root, WIDGETS);
    let archive = read(&s.root, ARCHIVE).replace("Nested prose.", "Corrupted nested prose.");
    std::fs::write(s.root.join(ARCHIVE), &archive).unwrap();
    s.git(&[
        "commit",
        "-qm",
        "synthetic nested archive corruption",
        "--",
        ARCHIVE,
    ]);
    let head = s.head();
    let index = std::fs::read(s.root.join(".git/index")).unwrap();
    let recovered = s.recover(json!({}), &[]);
    assert_eq!(
        one_result(&recovered)["result"],
        "repair_required",
        "{recovered}"
    );
    assert_eq!(read(&s.root, WIDGETS), candidate);
    assert_eq!(read(&s.root, ARCHIVE), archive);
    assert_eq!(s.head(), head);
    assert_eq!(std::fs::read(s.root.join(".git/index")).unwrap(), index);
    assert_eq!(s.records().len(), 1);
}

#[test]
fn pending_copy_retains_parent_for_every_recovery_source_check() {
    for mutation in ["missing-parent", "wrong-parent"] {
        let s = matrix_store("section-pending-parent", true);
        let id = section_id(&s, "Entry");
        let request = archive_request(&s, &id);
        let original = read(&s.root, WIDGETS);
        s.kill_at("after_rename", "archive-copy", &request, &[]);
        let mut record = s.record();
        assert_eq!(record["archive_parent"], request["parent"]);
        let path = s.root.join(format!(
            ".rivet/pending/{}.json",
            record["op"].as_str().unwrap()
        ));
        if mutation == "missing-parent" {
            record.as_object_mut().unwrap().remove("archive_parent");
        } else {
            record["archive_parent"] = json!(id);
        }
        std::fs::write(path, record.to_string()).unwrap();
        let archive = read(&s.root, ARCHIVE);
        let head = s.head();
        let index = std::fs::read(s.root.join(".git/index")).unwrap();
        let recovered = s.recover(json!({}), &[]);
        assert_eq!(
            one_result(&recovered)["result"],
            "repair_required",
            "{recovered}"
        );
        assert_eq!(read(&s.root, WIDGETS), original);
        assert_eq!(read(&s.root, ARCHIVE), archive);
        assert_eq!(s.head(), head);
        assert_eq!(std::fs::read(s.root.join(".git/index")).unwrap(), index);
        assert_eq!(s.records().len(), 1);
    }
}

#[test]
fn doctor_reports_a_history_namespace_walk_gap_for_live_citations() {
    let s = matrix_store("section-citation-gap", true);
    let id = section_id(&s, "Entry");
    let text = read(&s.root, WIDGETS).replace("A task.", &format!("See section:{id}."));
    std::fs::write(s.root.join(WIDGETS), text).unwrap();
    commit_fixture(&s);
    let project = markrivet_core::fs::Project::open(&s.root).unwrap();
    let faults = markrivet_core::fs::WalkFaults {
        directories: ["archive/month".into()].into(),
        ..Default::default()
    };
    let out = markrivet_core::doctor::run_with(&project, None, None, false, &faults).body;
    assert!(
        out["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|d| d["code"] == "citation_scope" && d["path"] == "archive/month"),
        "{out}"
    );
}

#[test]
fn reference_context_cannot_move_a_copied_stay_to_another_paragraph() {
    let s = matrix_store("section-reference-owner", true);
    source(
        &s,
        "# Synthetic\n\n[ref]: /synthetic\n\n## Log\n\n### Entry\n\nFirst [foo][ref](<!-- stay:Nested01 -->)\n\nSecond [bar][other](<!-- stay:Nested01 -->)\n",
    );
    let doc = Document::parse(&read(&s.root, WIDGETS)).unwrap();
    let Target::Block(block) = doc
        .resolve(&Selector::Id {
            id: "Nested01".into(),
        })
        .unwrap()
    else {
        panic!()
    };
    assert!(
        doc.blocks[block]
            .span
            .slice(&doc.source)
            .unwrap()
            .starts_with("First")
    );
    s.configure(json!({"commit":"off"}));
    std::fs::write(
        s.root.join(ARCHIVE),
        "# Archive\n\n[other]: /synthetic\n\n## Closed\n",
    )
    .unwrap();
    stamp(&s.root, ARCHIVE);
    s.configure(json!({"commit":"per-write","on_dirty":"refuse"}));
    commit_fixture(&s);
    let request = archive_request(&s, &section_id(&s, "Entry"));
    unchanged(&s, "archive-copy", request, "marker ownership differ");
}

#[test]
fn descendant_identity_ambiguity_refuses_retry_delete_and_delete_recovery() {
    for legacy_conflict in [true, false] {
        for operation in ["retry", "delete", "recovery"] {
            let s = matrix_store("section-descendant-ambiguity", true);
            let id = section_id(&s, "Entry");
            let request = archive_request(&s, &id);
            let (code, copy) = s.run("archive-copy", request.clone(), &[]);
            assert_eq!(code, 0, "{copy}");
            assert_eq!(copy["commit"]["status"], "committed", "{copy}");
            let ids: Vec<String> =
                serde_json::from_value(copy["commit"]["archive_copy"]["copy"]["ids"].clone())
                    .unwrap();
            let descendant = ids.iter().find(|v| **v != id).unwrap();
            let delete = deletion(&s, &id, &copy);
            if operation == "recovery" {
                s.kill_at("after_rename", "delete", &delete, &[]);
            }
            let candidate = read(&s.root, WIDGETS);
            let suffix = if legacy_conflict {
                format!(
                    "\n## Later {{#{descendant}}}\n<!-- stay:Foreign1 -->\n\nConflicting heading identity.\n"
                )
            } else {
                format!("\n## Later\n\nDuplicate <!-- stay:{descendant} -->\n")
            };
            let archive = format!("{}{suffix}", read(&s.root, ARCHIVE));
            std::fs::write(s.root.join(ARCHIVE), &archive).unwrap();
            s.git(&[
                "commit",
                "-qm",
                "synthetic descendant duplicate",
                "--",
                ARCHIVE,
            ]);
            if operation == "recovery" {
                let head = s.head();
                let index = std::fs::read(s.root.join(".git/index")).unwrap();
                let out = s.recover(json!({}), &[]);
                assert_eq!(one_result(&out)["result"], "repair_required", "{out}");
                assert!(
                    s.record()["repair"]["detail"]
                        .as_str()
                        .unwrap()
                        .contains("uniquely"),
                    "{}",
                    s.record()
                );
                assert_eq!(read(&s.root, WIDGETS), candidate);
                assert_eq!(read(&s.root, ARCHIVE), archive);
                assert_eq!(s.head(), head);
                assert_eq!(std::fs::read(s.root.join(".git/index")).unwrap(), index);
                assert_eq!(s.records().len(), 1);
            } else {
                unchanged(
                    &s,
                    if operation == "retry" {
                        "archive-copy"
                    } else {
                        "delete"
                    },
                    if operation == "retry" {
                        request
                    } else {
                        delete
                    },
                    "uniquely",
                );
            }
        }
    }
}

#[test]
fn source_rechecks_compare_marker_owners_with_the_frozen_copy_candidate() {
    for recovery in [false, true] {
        let s = matrix_store("section-source-context", true);
        source(
            &s,
            "# Synthetic\n\n[ref]: /synthetic\n\n## Log\n\n### Entry\n\nFirst [foo][ref](<!-- stay:Nested01 -->)\n\nSecond [bar][other](<!-- stay:Nested01 -->)\n",
        );
        s.configure(json!({"commit":"off"}));
        std::fs::write(
            s.root.join(ARCHIVE),
            "# Archive\n\n[ref]: /synthetic\n\n## Closed\n",
        )
        .unwrap();
        stamp(&s.root, ARCHIVE);
        s.configure(json!({"commit":"per-write","on_dirty":"refuse"}));
        commit_fixture(&s);
        let id = section_id(&s, "Entry");
        let request = archive_request(&s, &id);
        let archive = read(&s.root, ARCHIVE);
        let pause = Pause::new(&s.root, "planned");
        let child = if recovery {
            s.kill_at("after_rename", "archive-copy", &request, &[]);
            None
        } else {
            let c = s.spawn("archive-copy", &request, &[pause.env()]);
            pause.wait();
            Some(c)
        };
        // An editor ignoring Rivet's lock can change reference context outside
        // the guarded subtree and commit it while the copy waits.
        let changed = read(&s.root, WIDGETS).replace("[ref]: /synthetic", "[other]: /synthetic");
        std::fs::write(s.root.join(WIDGETS), &changed).unwrap();
        s.git(&[
            "commit",
            "-qm",
            "synthetic source reference context",
            "--",
            WIDGETS,
        ]);
        assert_eq!(item_guard(&s.root, WIDGETS, &id), request["guard"]);
        let head = s.head();
        let index = std::fs::read(s.root.join(".git/index")).unwrap();
        if let Some(child) = child {
            pause.release();
            let (code, out) = finish(child);
            assert_eq!(code, 2, "{out}");
            assert!(out.to_string().contains("marker ownership differ"), "{out}");
            assert_eq!(read(&s.root, ARCHIVE), archive);
            assert!(s.records().is_empty());
        } else {
            let archive = read(&s.root, ARCHIVE);
            let out = s.recover(json!({}), &[]);
            assert_eq!(one_result(&out)["result"], "repair_required", "{out}");
            assert_eq!(read(&s.root, ARCHIVE), archive);
            assert_eq!(s.records().len(), 1);
        }
        assert_eq!(read(&s.root, WIDGETS), changed);
        assert_eq!(s.head(), head);
        assert_eq!(std::fs::read(s.root.join(".git/index")).unwrap(), index);
    }
}

#[test]
fn retry_rechecks_both_marker_owners_against_verified_provenance() {
    for (change_source, change_archive) in
        [(true, true), (true, false), (false, true), (false, false)]
    {
        let s = matrix_store("section-retry-context", true);
        source(
            &s,
            "# Synthetic\n\n[ref]: /synthetic\n\n## Log\n\n### Entry\n\nFirst [foo][ref](<!-- stay:Nested01 -->)\n\nSecond [bar][other](<!-- stay:Nested01 -->)\n",
        );
        s.configure(json!({"commit":"off"}));
        std::fs::write(
            s.root.join(ARCHIVE),
            "# Archive\n\n[ref]: /synthetic\n\n## Closed\n",
        )
        .unwrap();
        stamp(&s.root, ARCHIVE);
        s.configure(json!({"commit":"per-write","on_dirty":"refuse"}));
        commit_fixture(&s);
        let id = section_id(&s, "Entry");
        let request = archive_request(&s, &id);
        let (code, copy) = s.run("archive-copy", request.clone(), &[]);
        assert_eq!(code, 0, "{copy}");
        assert_eq!(copy["commit"]["status"], "committed", "{copy}");
        let pause = Pause::new(&s.root, "archive_retry_checked");
        let child = s.spawn("archive-copy", &request, &[pause.env()]);
        let child = pause.wait_for(child);
        // An editor outside Rivet's lock can commit reference definitions that
        // switch a live stay's paragraph without touching the guarded subtree.
        for (path, changed) in [(WIDGETS, change_source), (ARCHIVE, change_archive)] {
            let before = read(&s.root, path);
            let after = if changed {
                before.replace("[ref]: /synthetic", "[other]: /synthetic")
            } else {
                before.replace(
                    "[ref]: /synthetic",
                    "[ref]: /synthetic\n[unused]: /synthetic",
                )
            };
            let old = Document::parse(&before).unwrap();
            let new = Document::parse(&after).unwrap();
            let select = Selector::Id { id: id.clone() };
            let old_target = old.resolve(&select).unwrap();
            let new_target = new.resolve(&select).unwrap();
            assert_eq!(
                old.target_revision(old_target),
                new.target_revision(new_target)
            );
            let stay = Selector::Id {
                id: "Nested01".into(),
            };
            let owner = new.target_span(new.resolve(&stay).unwrap()).0;
            assert!(owner.slice(&after).unwrap().starts_with(if changed {
                "Second"
            } else {
                "First"
            }));
            std::fs::write(s.root.join(path), after).unwrap();
        }
        s.git(&[
            "commit",
            "-qm",
            "synthetic retry reference context",
            "--",
            WIDGETS,
            ARCHIVE,
        ]);
        assert_eq!(item_guard(&s.root, WIDGETS, &id), request["guard"]);
        let source = read(&s.root, WIDGETS);
        let archive = read(&s.root, ARCHIVE);
        let head = s.head();
        let index = std::fs::read(s.root.join(".git/index")).unwrap();
        let records = s.records();
        pause.release();
        let (code, out) = finish(child);
        if change_source || change_archive {
            assert_eq!(
                code, 2,
                "source={change_source} archive={change_archive}: {out}"
            );
            assert!(out.to_string().contains("marker ownership differ"), "{out}");
        } else {
            assert_eq!(code, 0, "{out}");
            assert_eq!(out["status"], "unchanged");
            assert_eq!(out["commit"]["sha"], copy["commit"]["sha"]);
            assert_eq!(out["copy_op"], copy["commit"]["copy_op"]);
        }
        assert_eq!(read(&s.root, WIDGETS), source);
        assert_eq!(read(&s.root, ARCHIVE), archive);
        assert_eq!(s.head(), head);
        assert_eq!(std::fs::read(s.root.join(".git/index")).unwrap(), index);
        assert_eq!(s.records(), records);
    }
}
