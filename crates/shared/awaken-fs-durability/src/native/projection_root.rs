//! Retained identity for a caller-owned quiescent projection. This is not a
//! publication capability or a replacement for the exact-dirfd mutation APIs.
#[cfg(any(target_os = "linux", target_os = "macos"))]
use super::{DirectoryIdentity, directory_identity_nofollow, open_exact_root};
use std::path::Path;

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub(super) fn reject_projection_link(metadata: &std::fs::Metadata) -> std::io::Result<()> {
    #[cfg(windows)]
    let link = {
        use std::os::windows::fs::MetadataExt;
        metadata.file_attributes() & 0x400 != 0
    };
    #[cfg(not(windows))]
    let link = metadata.file_type().is_symlink();
    if link {
        Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "projection contains a link or reparse point",
        ))
    } else {
        Ok(())
    }
}

pub struct ProjectionRoot {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    identity: DirectoryIdentity,
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    _directory: std::os::fd::OwnedFd,
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    handle: same_file::Handle,
}

/// Borrow the retained directory itself for descriptor-based consumers. They
/// must not reopen its name after validation: an alias or rename can change
/// which directory that name selects while this descriptor retains identity.
#[cfg(any(target_os = "linux", target_os = "macos"))]
impl std::os::fd::AsFd for ProjectionRoot {
    fn as_fd(&self) -> std::os::fd::BorrowedFd<'_> {
        self._directory.as_fd()
    }
}

impl ProjectionRoot {
    pub fn capture(path: &Path) -> std::io::Result<Self> {
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            let identity = directory_identity_nofollow(path)?;
            Ok(Self {
                identity,
                _directory: open_exact_root(path, identity)?,
            })
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            let metadata = std::fs::symlink_metadata(path)?;
            reject_projection_link(&metadata)?;
            if !metadata.is_dir() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "projection root is not a plain directory",
                ));
            }
            Ok(Self {
                handle: same_file::Handle::from_path(path)?,
            })
        }
    }

    /// Compare against the still-retained root, never capture a new baseline
    /// for a retry. Windows relies on the filesystem's stable file identifiers
    /// through same-file; quiescence is still required at path-based edges.
    pub fn verify(&self, path: &Path) -> std::io::Result<()> {
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            open_exact_root(path, self.identity).map(|_| ())
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            let current = Self::capture(path)?;
            if current.handle == self.handle {
                Ok(())
            } else {
                Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "projection root was replaced",
                ))
            }
        }
    }
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod tests {
    use super::super::descriptor_identity;
    use super::*;
    use std::os::fd::AsFd as _;

    /// P1 capture a plain directory -> retain its exact inode. P2 rename it and
    /// put another directory at the old name -> path verification rejects;
    /// the borrowed descriptor still names P1. This is the root-substitution
    /// condition a later descriptor bind must resist, not a path round-trip.
    #[test]
    fn borrowed_projection_descriptor_retains_the_captured_directory() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("runtime");
        std::fs::create_dir(&path).unwrap();
        let expected = directory_identity_nofollow(&path).unwrap();
        let retained = ProjectionRoot::capture(&path).unwrap();
        std::fs::rename(&path, temp.path().join("old-runtime")).unwrap();
        std::fs::create_dir(&path).unwrap();
        assert!(retained.verify(&path).is_err(), "P2 replacement rejected");
        assert_eq!(
            descriptor_identity(retained.as_fd()).unwrap(),
            expected,
            "P1 retained"
        );
    }
}
