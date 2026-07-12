use std::fs::{File, OpenOptions};
use std::io;
use std::path::Path;

use fs4::fs_std::FileExt as _;

const OPERATION_LOCK_FILE: &str = ".language-operations.lock";

/// Holds one advisory data-directory operation lock until drop.
pub struct LanguageOperationGuard {
    _file: File,
}

impl LanguageOperationGuard {
    pub fn shared(data_dir: &Path) -> io::Result<Self> {
        let file = open_lock_file(data_dir)?;
        file.lock_shared()?;
        Ok(Self { _file: file })
    }

    pub fn exclusive(data_dir: &Path) -> io::Result<Self> {
        let file = open_lock_file(data_dir)?;
        file.lock_exclusive()?;
        Ok(Self { _file: file })
    }
}

fn open_lock_file(data_dir: &Path) -> io::Result<File> {
    std::fs::create_dir_all(data_dir)?;
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(data_dir.join(OPERATION_LOCK_FILE))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    #[test]
    fn exclusive_waits_for_shared_operation() {
        let temp = tempfile::tempdir().unwrap();
        let shared = LanguageOperationGuard::shared(temp.path()).unwrap();
        let path = temp.path().to_path_buf();
        let (acquired_tx, acquired_rx) = mpsc::channel();

        let waiter = std::thread::spawn(move || {
            let exclusive = LanguageOperationGuard::exclusive(&path).unwrap();
            acquired_tx.send(()).unwrap();
            exclusive
        });

        assert_eq!(
            acquired_rx.recv_timeout(Duration::from_millis(50)),
            Err(mpsc::RecvTimeoutError::Timeout)
        );
        drop(shared);
        acquired_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        drop(waiter.join().unwrap());
    }
}
