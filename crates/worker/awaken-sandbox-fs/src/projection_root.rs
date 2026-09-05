//! Retained identity for a caller-owned quiescent projection. This is not a
//! publication capability or a replacement for the exact-dirfd mutation APIs.
use super::*;

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
