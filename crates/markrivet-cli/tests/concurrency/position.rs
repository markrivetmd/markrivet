//! A positional write waiting on the project lock while the holder inserts
//! an item above its target: the waiter rereads under the lock and refuses
//! instead of writing the item that moved into its place.
use super::*;

const SOURCE: &str = "\
# Places

## Action

- [ ] **Alpha. (2026-09-01)** a
- [ ] **Bravo. (2026-09-02)** b
- [ ] **Charlie. (2026-09-03)** c
";

#[test]
fn a_waiting_positional_update_refuses_after_an_insert_above_its_target() {
    let root = project("position-race");
    std::fs::write(root.join(WIDGETS), SOURCE).unwrap();
    let place = json!({"kind": "position", "heading_path": ["Action"], "index": 1});
    let (_, got) = rivet(&root, "get", json!({"path": WIDGETS, "select": place}));
    assert_eq!(
        got["typed"]["actions"]["fields"]["title"], "Bravo.",
        "{got}"
    );
    let waiting_request = json!({
        "path": WIDGETS, "collection": "actions", "select": place,
        "guard": got["typed"]["actions"]["revision"], "set": {"done": true},
        "lock_timeout_ms": 60000,
    });
    // The holder rewrites the section body with a new item above Bravo.
    let section = json!({"kind": "heading_path", "path": ["Action"]});
    let (_, body) = rivet(&root, "get", json!({"path": WIDGETS, "select": section}));
    let inserted = body["body"].as_str().unwrap().replace(
        "- [ ] **Alpha",
        "- [ ] **Zulu. (2026-09-09)** z\n- [ ] **Alpha",
    );
    let pause = Pause::new(&root, "planned");
    let holder = spawn(
        &root,
        "set",
        &json!({"path": WIDGETS, "select": section, "part": "body",
                "guard": body["revision"], "content": inserted}),
        &[pause.env()],
    );
    pause.wait();
    let waiter = spawn(&root, "update-item", &waiting_request, &[]);
    wait_for_writer_lock(&root, &waiter);
    pause.release();
    let (code, out) = finish(holder);
    assert_eq!(code, 0, "{out}");
    let published = read(&root, WIDGETS);
    assert!(published.contains("**Zulu."));
    let (code, out) = finish(waiter);
    assert_eq!(code, 2, "{out}");
    assert_eq!(out["error"]["code"], "guard_conflict", "{out}");
    assert!(out["error"].get("current").is_none(), "{out}");
    assert_eq!(
        out["error"]["relocated"]["select"],
        json!({"kind": "position", "heading_path": ["Places", "Action"], "index": 2}),
        "{out}"
    );
    assert_eq!(
        read(&root, WIDGETS),
        published,
        "the holder's bytes, exactly"
    );
}
