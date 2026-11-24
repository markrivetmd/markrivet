#[path = "../../test-support/mod.rs"]
mod test_support;

#[test]
fn occupied_names_and_parallel_allocations_keep_their_bytes() {
    let parent = test_support::scratch("allocator-collisions");
    // Simulate earlier binaries or sandbox namespaces with the same PID.
    let occupied: Vec<_> = (0..16)
        .map(|i| {
            let dir = parent.join(format!("same-{}-{i}", std::process::id()));
            std::fs::create_dir(&dir).unwrap();
            std::fs::write(dir.join("sentinel"), b"previous run").unwrap();
            dir
        })
        .collect();
    let fresh = test_support::scratch_in(&parent, "same");
    assert!(!occupied.contains(&fresh));
    let dirs = std::thread::scope(|s| {
        let handles: Vec<_> = (0..16)
            .map(|i| {
                let parent = &parent;
                s.spawn(move || {
                    let dir = test_support::scratch_in(parent, "same");
                    std::fs::write(dir.join("owner"), i.to_string()).unwrap();
                    (i, dir)
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().unwrap())
            .collect::<Vec<_>>()
    });
    let unique: std::collections::BTreeSet<_> = dirs.iter().map(|(_, d)| d).collect();
    assert_eq!(unique.len(), dirs.len());
    for (i, dir) in dirs {
        assert_eq!(
            std::fs::read_to_string(dir.join("owner")).unwrap(),
            i.to_string()
        );
    }
    for dir in occupied {
        assert_eq!(
            std::fs::read(dir.join("sentinel")).unwrap(),
            b"previous run"
        );
    }
    std::fs::remove_dir_all(parent).unwrap();
}
