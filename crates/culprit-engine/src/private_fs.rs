//! Connection metadata and reports (which contain query text) are readable by the current user only.

use std::fs;
use std::io;
use std::path::Path;

/// Create `path` and any missing parents, each readable by the current user only (not just the last one: a parent
/// left at the default 0755 lets other local users list connection names).
pub fn create_dir(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        fs::DirBuilder::new().recursive(true).mode(0o700).create(path)?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    #[cfg(not(unix))]
    fs::create_dir_all(path)?;
    Ok(())
}

/// Make an existing directory Culprit owns readable by the current user only (folders older versions created with
/// the default permissions). Missing directories are left alone.
pub fn tighten(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = fs::metadata(path)
            && meta.is_dir()
            && meta.permissions().mode() & 0o077 != 0
        {
            let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o700));
        }
    }
    #[cfg(not(unix))]
    let _ = path;
}

#[cfg(all(test, unix))]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn every_created_folder_is_private() {
        let root = std::env::temp_dir().join(format!("culprit-private-{}", std::process::id()));
        let leaf = root.join("reports").join("shop");
        super::create_dir(&leaf).unwrap();
        for dir in [&root, &root.join("reports"), &leaf] {
            let mode = std::fs::metadata(dir).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o700, "{}", dir.display());
        }
        std::fs::set_permissions(root.join("reports"), std::fs::Permissions::from_mode(0o755)).unwrap();
        super::tighten(&root.join("reports"));
        assert_eq!(std::fs::metadata(root.join("reports")).unwrap().permissions().mode() & 0o777, 0o700);
        let _ = std::fs::remove_dir_all(&root);
    }
}

/// Write through a unique, exclusively created sibling. Concurrent writers cannot truncate a published inode.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    for _ in 0..128 {
        let mut name = path.file_name().ok_or_else(|| io::Error::other("No file name"))?.to_os_string();
        name.push(format!(".{}.{}.tmp", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
        let tmp = path.with_file_name(name);
        let mut options = private_options();
        let mut file = match options.write(true).create_new(true).open(&tmp) {
            Ok(file) => file,
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        };
        let result = (|| {
            io::Write::write_all(&mut file, bytes)?;
            drop(file);
            fs::rename(&tmp, path)
        })();
        if result.is_err() {
            let _ = fs::remove_file(&tmp);
        }
        return result;
    }
    Err(io::Error::new(io::ErrorKind::AlreadyExists, "Cannot create a unique temporary file"))
}

fn private_options() -> fs::OpenOptions {
    #[cfg(unix)]
    let mut options = fs::OpenOptions::new();
    #[cfg(not(unix))]
    let options = fs::OpenOptions::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
}

/// Hold this file while reading and updating a store. Keep the lock file: deleting it could split concurrent locks
/// across two inodes. Readers need no lock because publication is atomic.
pub fn lock(path: &Path) -> io::Result<fs::File> {
    use std::time::{Duration, Instant};
    if let Some(parent) = path.parent() {
        create_dir(parent)?;
    }
    let mut name = path.file_name().ok_or_else(|| io::Error::other("No file name"))?.to_os_string();
    name.push(".lock");
    let file = private_options().read(true).write(true).create(true).truncate(false).open(path.with_file_name(name))?;
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(file),
            Err(fs::TryLockError::WouldBlock) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(fs::TryLockError::WouldBlock) => {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "Another Culprit process is still saving this file",
                ));
            }
            Err(fs::TryLockError::Error(e)) => return Err(e),
        }
    }
}

#[cfg(test)]
mod concurrent_tests {
    use super::*;
    use std::sync::{
        Arc, Barrier,
        atomic::{AtomicBool, Ordering},
    };

    #[test]
    fn concurrent_writers_publish_only_complete_files() {
        let root = std::env::temp_dir().join(format!("culprit-atomic-{}", std::process::id()));
        create_dir(&root).unwrap();
        let path = root.join("state.json");
        let done = Arc::new(AtomicBool::new(false));
        let reader = {
            let (path, done) = (path.clone(), done.clone());
            std::thread::spawn(move || {
                while !done.load(Ordering::Acquire) {
                    if let Ok(data) = fs::read(&path) {
                        assert_eq!(data.len(), 128 * 1024);
                        assert!(data.iter().all(|b| *b == data[0]));
                    }
                }
            })
        };
        let barrier = Arc::new(Barrier::new(8));
        let threads: Vec<_> = (0..8)
            .map(|i| {
                let (path, barrier) = (path.clone(), barrier.clone());
                std::thread::spawn(move || {
                    let data = vec![b'A' + i; 128 * 1024];
                    barrier.wait();
                    for _ in 0..20 {
                        write_atomic(&path, &data).unwrap();
                    }
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        done.store(true, Ordering::Release);
        reader.join().unwrap();
        assert_eq!(fs::read_dir(&root).unwrap().count(), 1, "temporary files are removed");
        fs::remove_dir_all(root).unwrap();
    }
}
