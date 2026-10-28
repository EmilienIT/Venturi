use super::test_support::TempDir;
use std::sync::atomic::AtomicU64;

#[test]
fn temporary_directories_do_not_adopt_or_delete_stale_candidates() {
    let root = TempDir::new(&format!(
        "vv-exclusive-control-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let prefix = root.join("candidate").display().to_string();
    let stale = std::path::PathBuf::from(format!("{prefix}-{}-0", std::process::id()));
    std::fs::create_dir(&stale).unwrap();
    std::fs::write(stale.join("previous-run"), b"keep").unwrap();
    let sequence = AtomicU64::new(0);
    let first = TempDir::with_sequence(&prefix, &sequence);
    let second = TempDir::with_sequence(&prefix, &sequence);
    assert_ne!(first.path(), stale);
    assert_ne!(first.path(), second.path());
    let owned = first.path().to_path_buf();
    drop(first);
    assert!(!owned.exists());
    assert!(second.path().is_dir());
    assert_eq!(std::fs::read(stale.join("previous-run")).unwrap(), b"keep");
}
