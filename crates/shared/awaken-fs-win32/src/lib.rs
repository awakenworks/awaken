//! The narrow Win32 handle-inspection port for `awaken-sandbox-fs`.
//!
//! Filesystem policy, traversal, locking, and lifecycle stay with the existing
//! Sandbox filesystem crate. Only Win32 calls without a safe std or existing
//! dependency surface live here.
#![deny(unsafe_code)]

#[cfg(windows)]
#[allow(unsafe_code)]
mod sys;
#[cfg(windows)]
pub use sys::{
    NtfsFileInfo, delete_open, ntfs_file_information, rename_open_noreplace, rename_open_replace,
    require_local_fixed_drive,
};
