use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Unique per-test directory, removed on drop.
pub struct TempDir(PathBuf);

impl TempDir {
    pub fn new(prefix: &str) -> Self {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        Self::with_sequence(prefix, &SEQ)
    }

    fn with_sequence(prefix: &str, sequence: &AtomicU64) -> Self {
        loop {
            let dir = std::env::temp_dir().join(format!(
                "{prefix}-{}-{}",
                std::process::id(),
                sequence.fetch_add(1, Ordering::Relaxed)
            ));
            match std::fs::create_dir(&dir) {
                Ok(()) => return Self(dir),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("cannot create test directory: {error}"),
            }
        }
    }
}

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
    let stale = PathBuf::from(format!("{prefix}-{}-0", std::process::id()));
    std::fs::create_dir(&stale).unwrap();
    std::fs::write(stale.join("previous-run"), b"keep").unwrap();
    let sequence = AtomicU64::new(0);
    let first = TempDir::with_sequence(&prefix, &sequence);
    let second = TempDir::with_sequence(&prefix, &sequence);
    assert_ne!(&*first, stale);
    assert_ne!(&*first, &*second);
    let owned = first.to_path_buf();
    drop(first);
    assert!(!owned.exists());
    assert!(second.is_dir());
    assert_eq!(std::fs::read(stale.join("previous-run")).unwrap(), b"keep");
}

impl std::ops::Deref for TempDir {
    type Target = Path;

    fn deref(&self) -> &Path {
        &self.0
    }
}

impl AsRef<Path> for TempDir {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
