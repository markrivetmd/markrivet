//! Rivet's marker adapter against markstay's own vectors: the §5.2 tree tier,
//! §8 hash vectors, and child vectors generated from the Python reference.
use markrivet_core::Document;
use markrivet_core::document::Attach;
use serde_json::{Value, json};

fn load(name: &str) -> Value {
    let path = format!(
        "{}/tests/fixtures/markstay/{name}",
        env!("CARGO_MANIFEST_DIR")
    );
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

fn trimmed(s: &str) -> &str {
    s.trim_matches([' ', '\t', '\n', '\r', '\x0c', '\x0b'])
}

/// Content blocks and orphan chunks in the shape markstay's vectors use.
fn blocks(doc: &Document) -> Vec<Value> {
    let mut out: Vec<(usize, Value)> = vec![];
    for (i, b) in doc.blocks.iter().enumerate() {
        let content = doc.strip_markers(b.span);
        out.push((
            b.span.start,
            json!({
                "content": trimmed(&content),
                "index": i,
                "ids": doc.stay_ids(&b.markers),
                "line": doc.file_line(b.span.start),
                "orphan": false,
            }),
        ));
    }
    let mut orphans: Vec<(usize, Vec<String>)> = vec![];
    for m in doc.markers.iter().filter(|m| m.attach == Attach::Orphan) {
        let line = doc.file_line(m.lexical.span.start);
        match orphans.last_mut() {
            Some((l, ids)) if *l == line => ids.extend(m.lexical.id.clone()),
            _ => orphans.push((line, m.lexical.id.clone().into_iter().collect())),
        }
    }
    for (line, ids) in orphans {
        let pos = doc.lines[doc.lines.partition_point(|s| doc.file_line(*s) < line)];
        out.push((
            pos,
            json!({"content": "", "index": -1, "ids": ids, "line": line, "orphan": true}),
        ));
    }
    out.sort_by_key(|(pos, v)| (*pos, v["orphan"] == json!(false)));
    out.into_iter().map(|(_, v)| v).collect()
}

#[test]
fn tree_tier_vectors() {
    let corpus = load("tree-parse52.json");
    for v in corpus["vectors"].as_array().unwrap() {
        let doc = Document::parse(v["doc"].as_str().unwrap()).unwrap();
        assert_eq!(Value::Array(blocks(&doc)), v["treeBlocks"], "{}", v["name"]);
    }
}

#[test]
fn hash_vectors_match_the_pinned_dependency() {
    let corpus = load("spec-hash.json");
    for v in corpus["vectors"].as_array().unwrap() {
        let body = v["body"].as_str().unwrap();
        assert_eq!(
            markstay::normalize_body(body),
            v["normalized"].as_str().unwrap()
        );
        assert_eq!(
            markstay::body_hash(body, None),
            v["sha256"].as_str().unwrap()
        );
    }
}

#[test]
fn reference_child_vectors() {
    let corpus = load("reference-children.json");
    for v in corpus["vectors"].as_array().unwrap() {
        let name = v["name"].as_str().unwrap();
        let doc = Document::parse(v["doc"].as_str().unwrap()).unwrap();
        let expected: Vec<&Value> = v["blocks"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|b| b["orphan"] == json!(false))
            .collect();
        assert_eq!(doc.blocks.len(), expected.len(), "{name}: block count");
        for (i, (b, want)) in doc.blocks.iter().zip(expected).enumerate() {
            let content = doc.strip_markers(b.span).replace("\r\n", "\n");
            assert_eq!(
                trimmed(&content),
                want["content"].as_str().unwrap(),
                "{name} block {i}"
            );
            assert_eq!(
                doc.file_line(b.span.start) as u64,
                want["line"].as_u64().unwrap(),
                "{name} block {i}"
            );
            assert_eq!(
                json!(doc.stay_ids(&b.markers)),
                want["ids"],
                "{name} block {i} ids"
            );
            assert_eq!(
                doc.block_hash(i),
                want["hash"].as_str().unwrap(),
                "{name} block {i} hash"
            );
            let rows: Vec<&Value> = want["children"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|c| c["kind"] == json!("row"))
                .collect();
            for row in rows {
                for id in row["ids"].as_array().unwrap() {
                    let id = id.as_str().unwrap();
                    assert!(
                        doc.markers
                            .iter()
                            .any(|m| m.lexical.id.as_deref() == Some(id)
                                && m.attach == Attach::Unaddressed),
                        "{name}: row marker {id} must address nothing in Rivet"
                    );
                }
            }
            let children: Vec<&Value> = want["children"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|c| c["kind"] == json!("list"))
                .collect();
            let items = b.list.map_or(&[][..], |l| &doc.lists[l].items[..]);
            assert_eq!(items.len(), children.len(), "{name} block {i} children");
            for (n, (item, want)) in items.iter().zip(children).enumerate() {
                let list = b.list.unwrap();
                assert_eq!(
                    doc.item_body(list, n),
                    want["content"].as_str().unwrap(),
                    "{name} item {n}"
                );
                assert_eq!(
                    doc.item_hash(list, n),
                    want["subhash"].as_str().unwrap(),
                    "{name} item {n}"
                );
                assert_eq!(
                    doc.file_line(item.span.start) as u64,
                    want["line"].as_u64().unwrap()
                );
                assert_eq!(
                    json!(doc.stay_ids(&item.markers)),
                    want["ids"],
                    "{name} item {n} ids"
                );
            }
        }
        let orphan_ids: Vec<Value> = v["blocks"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|b| b["orphan"] == json!(true))
            .flat_map(|b| b["ids"].as_array().unwrap().clone())
            .collect();
        let ours: Vec<Value> = doc
            .markers
            .iter()
            .filter(|m| m.attach == Attach::Orphan)
            .map(|m| json!(m.lexical.id))
            .collect();
        assert_eq!(ours, orphan_ids, "{name}: orphans");
    }
}

/// Rivet's block digests are the §5.2 profile's, not §5.1's.
///
/// Outside §5.4's agreement subset one stay has two correct digests, one per
/// segmenter. markstay's CLI and pre-commit hook default to §5.1, so a stay
/// Rivet refreshed reads as drift to a default `markstay lint`, and one a
/// default `markstay restamp` wrote reads as drift to Rivet. Neither tool is
/// computing wrongly. `marker_evidence` is the value Rivet both checks and
/// writes, so it must equal `tree` on every stay, and the split stays must stay
/// split, or the vectors have stopped showing the divergence they pin.
#[test]
fn block_digests_follow_the_tree_profile() {
    let corpus = load("profile-divergence.json");
    let mut split = 0;
    for v in corpus["vectors"].as_array().unwrap() {
        let name = v["name"].as_str().unwrap();
        let doc = Document::parse(v["doc"].as_str().unwrap()).unwrap();
        for s in v["stays"].as_array().unwrap() {
            let id = s["id"].as_str().unwrap();
            let marker = doc
                .markers
                .iter()
                .position(|m| m.lexical.id.as_deref() == Some(id))
                .unwrap_or_else(|| panic!("{name}: no marker {id}"));
            let (computed, _) = doc
                .marker_evidence(marker)
                .unwrap_or_else(|| panic!("{name}: {id} binds nothing"));
            assert_eq!(computed, s["tree"].as_str().unwrap(), "{name}: {id}");
            if s["agrees"] == json!(false) {
                assert_ne!(computed, s["blank_line"].as_str().unwrap(), "{name}: {id}");
                split += 1;
            }
        }
    }
    assert_eq!(split, 3, "the vectors no longer split the two profiles");
}

/// The spec revision these vectors were reviewed against. Bumping it is a
/// deliberate act: regenerate with `fixtures/markstay/generate.py`, read what
/// moved, and only then pin the new hash. markstay v1.10 is the revision pinned
/// here: it adds a linter rule (a bare stay adopted in place as its item's child
/// stay is kept, reported as `ADOPTED_ID`) and regenerating moved no vector, nor
/// did v1.9's (a dropped child stay). v1.8, spec §3.4's code-span mask, is the
/// last that moved them.
#[test]
fn vendored_vectors_pin_the_reviewed_spec() {
    for name in [
        "reference-children.json",
        "carrier-state.json",
        "profile-divergence.json",
    ] {
        assert_eq!(
            load(name)["spec_sha256"],
            "9dcbc452f10120af3c97ca20dd79b4d98eb77dcabc363f8abaad5a3f902cba1a",
            "{name} was generated against a different spec revision than the \
             other vectors; regenerate both together"
        );
    }
}

/// §3.4's carrier rule against markstay's own writer, including v1.8's
/// code-span mask.
///
/// The vectors carry `presence_rule` as well as `accepts`: the pre-v1.8 answer
/// for the same carrier. A port that simply masked everything between the first
/// and last backtick would satisfy `accepts` on the cases that moved and fail
/// the ones that did not, so both columns are asserted and the moved cases are
/// counted. If that count reaches zero the vectors have stopped discriminating,
/// which is a broken test rather than a passing one.
///
/// The corpus was checked by building the wrong ports rather than by reasoning
/// about it. Seven were written against this implementation and every one is
/// rejected, each by the vector added for it: pairing across lines
/// (`cross-line-pairing`), continuing past a line whose runs do not balance
/// (`continues-past-an-unbalanced-line`), keeping the spans found on such a
/// line (`tentative-spans-on-an-unbalanced-line`), pairing runs inside a fence
/// (`tilde-fence-is-not-scanned`), and splitting lines on LF alone
/// (`bare-cr-line-endings`). A later round added two: a closer merely as long
/// as its opener rather than equal to it (`closer-longer-than-opener`), and
/// code-point offsets used as byte indices, which agrees on every verdict and
/// is caught only by the masked-text comparison below. The same round added
/// `fence-indented-four-is-scanned`, which is not a wrong port but a boundary:
/// §3.3 does not recognise a fence indented four spaces, so the scan does
/// enter it. Each wrong port masks MORE than the canonical rule,
/// which is the direction that silently rewrites someone's document; a port
/// that masks less only refuses. Removing one of those vectors puts its
/// mistake back in reach.
#[test]
fn carrier_state_matches_markstay() {
    let corpus = load("carrier-state.json");
    let mut moved = 0;
    for vector in corpus["vectors"].as_array().unwrap() {
        let name = vector["name"].as_str().unwrap();
        let text = vector["carrier_text"].as_str().unwrap();
        let expected = vector["accepts"].as_bool().unwrap();
        let presence = vector["presence_rule"].as_bool().unwrap();
        let actual = markrivet_core::edit::carrier_refusal(text, &[]).is_none();
        assert_eq!(
            actual, expected,
            "carrier {name:?}: markstay accepts={expected}, rivet accepts={actual}\n\
             carrier text: {text:?}"
        );
        // The masked text itself, not just the verdict, for every vector.
        // markstay writes one space per code point and a byte-range mask writes
        // one per byte, so the generator widens each masked character to its
        // UTF-8 length in `masked_bytes`. Comparing that instead of skipping
        // the non-ASCII vectors is what catches a port using code-point offsets
        // as byte indices, which agrees on the verdict and masks the wrong run.
        assert_eq!(
            markrivet_core::edit::mask_for_carrier(text, &[]),
            vector["masked_bytes"].as_str().unwrap(),
            "carrier {name:?}: masked text diverges from markstay's"
        );
        if expected != presence {
            moved += 1;
        }
    }
    assert!(
        moved >= 4,
        "only {moved} vectors differ between v1.7's presence rule and v1.8; \
         the corpus no longer exercises the clause it exists to pin"
    );
}

/// What markstay's own diff linter reports for an `adopt_stay` write: §16's
/// in-place rule (v1.10) keeps a stay that shares its line with the item's
/// content as `ADOPTED_ID`, and fails closed on one on its own line, which
/// `adopt_stay` accepts all the same and the linter still calls `DROPPED_ID`.
/// The parser contract states both; this holds it to the pinned crate.
#[test]
fn adopt_stay_reads_as_markstay_reports_it() {
    use markrivet_core::Selector;
    use markrivet_core::edit::{self, Op, Planner};
    let adopt = |src: &str| {
        let doc = Document::parse(src).unwrap();
        let target = doc
            .resolve(&Selector::Id {
                id: "handID01".into(),
            })
            .unwrap();
        let op = Op::AdoptStay {
            id: "handID01".into(),
            guard: doc.target_revision(target),
        };
        let change = Planner::new(&doc).structural(&op).unwrap();
        edit::plan(&doc, vec![change]).unwrap().candidate
    };
    let codes = |before: &str| -> Vec<(&'static str, &'static str, Option<String>)> {
        markstay::lint::lint_diff(before, &adopt(before))
            .into_iter()
            .map(|f| (f.level.as_str(), f.code, f.id))
            .collect()
    };
    let same_line = "# T\n\n## Action\n\n- [ ] **One.** Written by hand. <!-- stay:handID01 -->\n- [ ] **Two.** Plain.\n";
    assert_eq!(
        codes(same_line),
        [("warn", "ADOPTED_ID", Some("handID01".into()))]
    );
    let own_line = "# T\n\n## Action\n\n- [ ] **One.** Written by hand.\n  <!-- stay:handID01 -->\n- [ ] **Two.** Plain.\n";
    assert_eq!(
        codes(own_line),
        [("error", "DROPPED_ID", Some("handID01".into()))]
    );
}
