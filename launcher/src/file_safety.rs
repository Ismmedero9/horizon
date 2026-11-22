//! Small file helpers for the launcher's backend: owner-only and unique temporary files.
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// A new file at `path` (any old one there removed first) only its owner can read, holding `bytes`.
pub(crate) fn write_owner_only(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let _ = fs::remove_file(path);
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    io::Write::write_all(&mut options.open(path)?, bytes)
}

/// Where to copy `name` into `dir` before renaming it into place: its own name per copy, so two
/// threads completing the same file never write into the one the other just renamed.
pub(crate) fn unique_partial(dir: &Path, name: &str) -> PathBuf {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static COPIES: AtomicUsize = AtomicUsize::new(0);
    let n = COPIES.fetch_add(1, Ordering::Relaxed);
    dir.join(format!(".{name}.partial-{}-{n}", std::process::id()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn owner_only_files_and_unique_partials() {
        use std::os::unix::fs::PermissionsExt;
        let dir =
            std::env::temp_dir().join(format!("horizon-file-safety-owner-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.tmp");
        fs::write(&path, b"old").unwrap();
        write_owner_only(&path, b"new").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"new");
        assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o077, 0);
        assert_ne!(
            unique_partial(&dir, "a.json"),
            unique_partial(&dir, "a.json")
        );
        let _ = fs::remove_dir_all(&dir);
    }
}
