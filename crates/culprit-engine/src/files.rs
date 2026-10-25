//! Local export files: a bounded size, a plain filename and no replacement of an existing file.
use std::io::Write;
use std::path::{Path, PathBuf};

use culprit_core::{Error, Result};

pub const MAX_EXPORT_BYTES: usize = 64 * 1024 * 1024;

/// Save an export to Downloads with a plain document filename and no overwrite.
/// Windows inherits the folder's access rules; Unix files allow only owner read/write.
pub fn save_to_downloads(name: &str, contents: &str) -> Result<PathBuf> {
    let dir = dirs::download_dir()
        .or_else(|| dirs::home_dir().map(|home| home.join("Downloads")))
        .filter(|dir| dir.is_dir())
        .ok_or_else(|| Error::not_found("There is no Downloads folder to save into"))?;
    save_in(&dir, name, contents)
}

fn check_size(bytes: usize) -> Result<()> {
    if bytes > MAX_EXPORT_BYTES {
        return Err(Error::invalid(format!("Exports are limited to {} MiB", MAX_EXPORT_BYTES / (1024 * 1024)))
            .with_hint("Export fewer rows or columns, then try again"));
    }
    Ok(())
}

fn save_in(dir: &Path, name: &str, contents: &str) -> Result<PathBuf> {
    check_size(contents.len())?;
    let (stem, extension) = file_name(name);
    for n in 0..1000 {
        let file = if n == 0 { format!("{stem}.{extension}") } else { format!("{stem} ({n}).{extension}") };
        let path = dir.join(file);
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&path) {
            Ok(mut file) => {
                if let Err(error) = file.write_all(contents.as_bytes()) {
                    drop(file);
                    let _ = std::fs::remove_file(&path);
                    return Err(Error::internal(format!("Could not save the file: {error}")));
                }
                return Ok(path);
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(Error::internal(format!("Could not save the file: {error}"))),
        }
    }
    Err(Error::internal("Too many exported files with this name"))
}

fn file_name(name: &str) -> (String, &'static str) {
    let lower = name.to_ascii_lowercase();
    let extension = ["sqlplan", "json", "txt", "html", "md", "csv"]
        .into_iter()
        .find(|extension| lower.ends_with(&format!(".{extension}")))
        .unwrap_or("txt");
    let stem = if lower.ends_with(&format!(".{extension}")) { &name[..name.len() - extension.len() - 1] } else { name };
    let stem: String = stem
        .chars()
        .map(|char| if char.is_ascii_alphanumeric() || matches!(char, ' ' | '_' | '-' | '.') { char } else { '-' })
        .take(80)
        .collect();
    let stem = stem.trim_matches(|char: char| char == '.' || char == ' ' || char == '-').to_string();
    (if stem.is_empty() { "export".to_string() } else { crate::store::windows_safe(stem) }, extension)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Temp(PathBuf);
    impl Temp {
        fn new() -> Self {
            static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            let path = std::env::temp_dir().join(format!(
                "culprit-exports-{}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn file_names_stay_inside_downloads_and_keep_document_extensions() {
        for (input, stem, extension) in [
            ("Plan 815.sqlplan", "Plan 815", "sqlplan"),
            ("../../etc/passwd", "etc-passwd", "txt"),
            ("..\\..\\boot.ini.json", "boot.ini", "json"),
            (".hidden.txt", "hidden", "txt"),
            ("/abs/path/x.exe", "abs-path-x.exe", "txt"),
            ("...", "export", "txt"),
            ("con", "_con", "txt"),
            ("COM1.sqlplan", "_COM1", "sqlplan"),
            ("nul.json", "_nul", "json"),
            ("query-results.CSV", "query-results", "csv"),
        ] {
            assert_eq!(file_name(input), (stem.to_string(), extension));
        }
    }

    #[test]
    fn exports_preserve_contents_and_never_overwrite() {
        let dir = Temp::new();
        let first = save_in(&dir.0, "query-results.csv", "id,name\r\n1,é\r\n").unwrap();
        let second = save_in(&dir.0, "query-results.csv", "second").unwrap();
        assert_eq!(std::fs::read_to_string(&first).unwrap(), "id,name\r\n1,é\r\n");
        assert_eq!(std::fs::read_to_string(&second).unwrap(), "second");
        assert_ne!(first, second);
        assert_eq!(second.file_name().unwrap(), "query-results (1).csv");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(first).unwrap().permissions().mode() & 0o777, 0o600);
        }
    }

    #[test]
    fn export_size_policy_has_an_inclusive_byte_limit() {
        assert!(check_size(MAX_EXPORT_BYTES).is_ok());
        assert!(check_size(MAX_EXPORT_BYTES + 1).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn an_existing_symlink_is_not_followed_or_replaced() {
        let dir = Temp::new();
        let original = dir.0.join("original");
        std::fs::write(&original, b"keep").unwrap();
        let link = dir.0.join("query-results.json");
        std::os::unix::fs::symlink(&original, &link).unwrap();
        let export = save_in(&dir.0, "query-results.json", "[]").unwrap();
        assert_ne!(export, link);
        assert_eq!(std::fs::read_to_string(original).unwrap(), "keep");
    }
}
