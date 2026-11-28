mod common;
use common::*;
use serde_json::json;

/// Checkbox guards from the example tracker, as literals, so no change to
/// the binding shape can move one unnoticed.
#[test]
fn checkbox_guards_are_pinned() {
    let root = project("file-records-pinned");
    let (code, out) = rivet(&root, "query", json!({"collection": "actions"}));
    assert_eq!(code, 0, "{out}");
    let first = &out["items"][0];
    assert_eq!(first["path"], "tracking/TRACKING_DOCS.md", "{out}");
    assert_eq!(
        first["revision"],
        "rivet:v1:action:3e2614e8fecf31ba2a3867c330e288e3"
    );
    let (code, map) = rivet(&root, "map", json!({"path": WIDGETS}));
    assert_eq!(code, 0, "{map}");
    assert_eq!(
        map["collections"][0]["revision"],
        "rivet:v1:collection:37611eeca9c98d8682550333d321476c"
    );
    let (code, doctor) = rivet(&root, "doctor", json!({}));
    assert!(code == 0 || code == 1, "{doctor}");
    assert_eq!(
        doctor["collections"][0]["binding_fingerprint"],
        "sha256:764524528fc5ca6effd508665b0fa909cef3bb824ca2854663c90b2ef51ae816"
    );
}
