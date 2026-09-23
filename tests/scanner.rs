use dsopt::{
    Candidate, CandidateKind, FileIdentity, deduplicate_candidates, format_size, scan, should_skip,
};

#[test]
fn finds_node_modules_and_target_with_recursive_sizes() {
    let fixture = tempfile::tempdir().unwrap();
    let modules = fixture.path().join("web/node_modules/pkg");
    let target = fixture.path().join("rust/target/debug");
    std::fs::create_dir_all(&modules).unwrap();
    std::fs::create_dir_all(&target).unwrap();
    std::fs::write(modules.join("index.js"), b"12345").unwrap();
    std::fs::write(target.join("app"), b"1234567").unwrap();

    let root = fixture.path().canonicalize().unwrap();
    let result = scan(&root, 2).unwrap();

    assert!(
        result.candidates.iter().any(|candidate| {
            candidate.kind == CandidateKind::NodeModules
                && candidate.path == root.join("web/node_modules")
                && candidate.size_bytes >= 5
        }),
        "candidates: {:?}",
        result.candidates
    );
    assert!(result.candidates.iter().any(|candidate| {
        candidate.kind == CandidateKind::RustTarget
            && candidate.path == root.join("rust/target")
            && candidate.size_bytes >= 7
    }));
}

#[test]
fn folds_nested_candidates_into_their_outermost_parent() {
    let fixture = tempfile::tempdir().unwrap();
    let nested = fixture
        .path()
        .join("app/node_modules/dep/node_modules/inner");
    std::fs::create_dir_all(&nested).unwrap();
    std::fs::write(nested.join("index.js"), b"1234567890").unwrap();

    let root = fixture.path().canonicalize().unwrap();
    let result = scan(&root, 2).unwrap();

    assert_eq!(
        result.candidates.len(),
        1,
        "nested node_modules should collapse: {:?}",
        result.candidates
    );
    assert_eq!(result.candidates[0].path, root.join("app/node_modules"));
}

#[test]
fn results_are_sorted_largest_first() {
    let fixture = tempfile::tempdir().unwrap();
    let small = fixture.path().join("small/node_modules");
    let large = fixture.path().join("large/target");
    std::fs::create_dir_all(&small).unwrap();
    std::fs::create_dir_all(&large).unwrap();
    std::fs::write(small.join("a"), b"1").unwrap();
    std::fs::write(large.join("a"), b"12345678901234567890").unwrap();

    let root = fixture.path().canonicalize().unwrap();
    let result = scan(&root, 2).unwrap();

    assert!(result.candidates.len() >= 2);
    assert!(result.candidates[0].size_bytes >= result.candidates[1].size_bytes);
}

#[test]
fn rejects_thread_counts_outside_the_supported_range() {
    assert!(scan(std::path::Path::new("/tmp"), 0).is_err());
    assert!(scan(std::path::Path::new("/tmp"), 9).is_err());
}

#[test]
fn reports_missing_roots_as_errors() {
    assert!(scan(std::path::Path::new("/definitely/not/a/real/path"), 2).is_err());
}

#[test]
fn skips_macos_data_volume_alias_when_scanning_root() {
    assert!(should_skip(std::path::Path::new("/System/Volumes/Data")));
}

#[test]
fn deduplicates_candidates_that_share_a_filesystem_identity() {
    let candidates = vec![
        (
            Candidate {
                kind: CandidateKind::RustTarget,
                path: "/Users/musichen/project/target".into(),
                size_bytes: 42,
            },
            Some(FileIdentity {
                device: 7,
                inode: 99,
            }),
        ),
        (
            Candidate {
                kind: CandidateKind::RustTarget,
                path: "/System/Volumes/Data/Users/musichen/project/target".into(),
                size_bytes: 42,
            },
            Some(FileIdentity {
                device: 7,
                inode: 99,
            }),
        ),
    ];

    let unique = deduplicate_candidates(candidates);

    assert_eq!(unique.len(), 1);
    assert_eq!(
        unique[0].path,
        std::path::PathBuf::from("/Users/musichen/project/target")
    );
}

#[test]
fn keeps_candidates_that_have_no_filesystem_identity() {
    // Windows and some filesystems report no (device, inode). Those candidates
    // must still be surfaced, not silently dropped.
    let candidates = vec![(
        Candidate {
            kind: CandidateKind::NodeModules,
            path: "/somewhere/node_modules".into(),
            size_bytes: 7,
        },
        None,
    )];

    let unique = deduplicate_candidates(candidates);

    assert_eq!(unique.len(), 1);
    assert_eq!(unique[0].size_bytes, 7);
    assert_eq!(unique[0].kind, CandidateKind::NodeModules);
}

#[test]
fn formats_human_readable_sizes() {
    assert_eq!(format_size(0), "0 B");
    assert_eq!(format_size(1024), "1.0 KB");
    assert_eq!(format_size(5 * 1024 * 1024 * 1024), "5.0 GB");
}
