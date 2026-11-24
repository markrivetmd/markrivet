// Verify the dependency's actual boundary before adopting its runtime adapter.
#[test]
fn child_markers_never_become_container_ids() {
    for source in [
        "- one <!-- stay:item subhash=sha256:abcdef123456 -->\n- two\n\n<!-- stay:list -->\n",
        "| Key | Value |\n| --- | --- |\n| a | b<!-- stay:row subhash=sha256:abcdef123456 --> |\n<!-- stay:table -->\n",
    ] {
        let markers = markstay::find_markers(source, 0);
        assert!(markers.iter().any(|m| m.has_subhash && !m.is_block_stay()));
        let anchors = markstay::build_anchors(source);
        assert!(anchors.iter().all(|a| a.id != "row"));
        assert!(anchors.iter().all(|a| a.id != "item"));
    }
}

#[test]
fn unknown_keys_and_invalid_child_evidence_survive_scanning() {
    let raw = "<!-- stay:row subhash=invalid future=\"a b\" -->";
    let markers = markstay::find_markers(raw, 0);
    assert_eq!(markers.len(), 1);
    assert_eq!(markers[0].raw, raw);
    assert!(markers[0].has_subhash);
    assert!(!markers[0].is_block_stay());
}
