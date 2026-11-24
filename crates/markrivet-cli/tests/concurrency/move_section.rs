use super::move_fixture::{A, B, MoveStore};
use super::move_item::{fence, killed, recovered};
use super::*;

#[test]
fn move_section_crash_boundaries_preserve_subtree_and_recover_both_paths() {
    for point in [
        "move_pending_written",
        "move_destination_replaced",
        "move_applied",
        "ref_updated",
    ] {
        let s = MoveStore::section(false);
        let mut request = s.section_request();
        request["before"] = json!("dchild01");
        let reads = (s.read(A), s.read(B));
        killed(&s, point, &request, &[]);
        fence(&s);
        let receipt = &s.records()[0]["move_receipt"];
        assert_eq!(receipt["kind"], "section");
        assert_eq!(receipt["destination"]["before"], "dchild01");
        assert_eq!(receipt["ids"].as_array().unwrap().len(), 6);
        if point == "move_destination_replaced" {
            assert!(s.read(A).contains("stay:section1"));
            assert!(s.read(B).contains("stay:section1"));
        }
        recovered(&s, point != "move_pending_written");
        if point == "move_pending_written" {
            assert_eq!((s.read(A), s.read(B)), reads);
        } else {
            assert!(!s.read(A).contains("stay:section1"));
            assert!(s.read(B).contains("stay:detail01"));
            assert!(
                s.read(B).find("### Payload").unwrap() < s.read(B).find("### Existing").unwrap()
            );
            assert_eq!(s.ok("move", request)["current"], "matches");
        }
    }
}

#[test]
fn move_section_compensation_and_destination_durability_keep_every_byte() {
    for fault in ["move_source_rename", "dir_sync"] {
        let s = MoveStore::section(false);
        let reads = (s.read(A), s.read(B));
        let (code, out) = s.run(
            "move",
            s.section_request(),
            &[("RIVET_TEST_FAULT", fault.into())],
        );
        if fault == "move_source_rename" {
            assert_eq!(code, 2, "{out}");
            assert_eq!((s.read(A), s.read(B)), reads);
            assert!(s.records().is_empty());
            assert!(!s.root.join(".git/index.lock").exists());
        } else {
            assert_eq!(code, 3, "{out}");
            assert_eq!(s.read(A), reads.0);
            fence(&s);
            recovered(&s, true);
        }
    }
}

#[test]
fn move_section_recovery_keeps_an_outside_edit_and_blocks_both_paths() {
    for path in [A, B] {
        let s = MoveStore::section(false);
        let request = s.section_request();
        killed(&s, "move_destination_replaced", &request, &[]);
        let text = format!("{}\nOutside edit.\n", s.read(path));
        s.put(path, &text);
        let out = s.ok("commit-pending", json!({}));
        assert_eq!(out["unresolved"], 1, "{out}");
        assert_eq!(s.read(path), text);
        fence(&s);
        let (code, out) = s.run("move", request, &[]);
        assert_eq!(code, 2, "{out}");
        assert_eq!(out["error"]["code"], "pending_unresolved");
        assert_eq!(s.read(path), text);
    }
}

#[test]
fn move_section_waits_for_destination_writer_and_keeps_its_edit() {
    let s = MoveStore::section(false);
    let request = s.section_request();
    let guard = item_guard(&s.root, B, "dlog0001");
    let pause = Pause::new(&s.root, "planned");
    let writer = pause.wait_for(s.spawn(
        "write",
        &json!({"path":B,"ops":[{
            "op":"replace_in_section","select":{"kind":"id","id":"dlog0001"},
            "guard":guard,"old":"Destination history.","new":"Concurrent history."
        }]}),
        &[pause.env()],
    ));
    let mover = s.spawn("move", &request, &[]);
    pause.release();
    let (code, out) = finish(writer);
    assert_eq!(code, 0, "{out}");
    let (code, out) = finish(mover);
    assert_eq!(code, 0, "{out}");
    assert!(s.read(B).contains("Concurrent history."));
    assert!(s.read(B).contains("stay:section1"));
}
