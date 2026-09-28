// @formal entrypoint sandbox_control.single_publication_is_stale_fenced_non_wrapping_and_close_absorbing
//! Linux pathname socket addresses rooted in an opened directory.
use super::*;
use std::os::fd::AsRawFd as _;

/// A short socket pathname that retains the exact parent directory descriptor.
///
/// The address is process-local and valid only while this value is retained.
/// It names the same filesystem socket, without copying the caller's possibly
/// long directory path into `sockaddr_un`. This grants no publication, stale
/// replacement, or unlink authority; those remain with the provider. Linux
/// procfs and the kernel's descriptor/pathname semantics are trusted here.
pub struct PinnedUnixSocketPath {
    _directory: std::os::fd::OwnedFd,
    path: PathBuf,
}

impl PinnedUnixSocketPath {
    pub fn capture(socket: &Path) -> std::io::Result<Self> {
        let invalid = || {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "socket requires a parent directory and one leaf",
            )
        };
        let parent = socket.parent().ok_or_else(invalid)?;
        let leaf = validate_sibling_leaf(Path::new(socket.file_name().ok_or_else(invalid)?))?;
        let identity = directory_identity_nofollow(parent)?;
        let directory = open_exact_root(parent, identity)?;
        let path = PathBuf::from(format!("/proc/self/fd/{}", directory.as_raw_fd())).join(leaf);
        // Keep the standard library as the one socket-address length/NUL owner.
        std::os::unix::net::SocketAddr::from_pathname(&path)?;
        Ok(Self {
            _directory: directory,
            path,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}
