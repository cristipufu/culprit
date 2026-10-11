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

/// Write to a temporary sibling and rename, so readers never see a half-written file.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let tmp = path.with_extension("tmp");
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    io::Write::write_all(&mut options.open(&tmp)?, bytes)?;
    fs::rename(&tmp, path)
}
