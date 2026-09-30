// @formal surface requirement=resource.files.resource_lifecycle
// @formal reason: Provider-neutral filesystem durability port used after each
// @formal reason: owning store or Sandbox adapter has decided what to publish or
// @formal reason: remove. It owns only native directory-handle flush mechanics,
// @formal reason: not resource identity or authorization, and claims no proof.
//! One native directory-flush primitive for durable filesystem adapters.

#[cfg(not(windows))]
use std::fs::File;
use std::io;
use std::path::Path;

/// Flush directory metadata after an owning adapter creates, renames, or
/// removes an entry. A file or absent path is never accepted as a directory.
pub fn sync_directory(path: &Path) -> io::Result<()> {
    #[cfg(windows)]
    let directory = {
        use std::os::windows::fs::OpenOptionsExt as _;
        use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_BACKUP_SEMANTICS;

        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(path)?
    };
    #[cfg(not(windows))]
    let directory = File::open(path)?;

    if !directory.metadata()?.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "directory durability requires a directory handle",
        ));
    }
    directory.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_directory_flush_accepts_only_a_real_directory() {
        // Causes: C1 an existing directory; C2 an existing regular file; C3
        // an absent path. Effects: R1 native metadata flush succeeds on C1;
        // R2 a file is rejected as InvalidInput; R3 absence remains NotFound.
        // The owning adapter, not this leaf, decides which tree is authorized.
        let temp = tempfile::tempdir().unwrap();
        assert!(sync_directory(temp.path()).is_ok(), "R1");
        let file = temp.path().join("file");
        std::fs::write(&file, b"data").unwrap();
        assert_eq!(
            sync_directory(&file).unwrap_err().kind(),
            io::ErrorKind::InvalidInput,
            "R2"
        );
        assert_eq!(
            sync_directory(&temp.path().join("absent"))
                .unwrap_err()
                .kind(),
            io::ErrorKind::NotFound,
            "R3"
        );
    }
}
