use std::fs::File;
use std::io;
use std::mem::MaybeUninit;
use std::os::windows::ffi::OsStrExt as _;
use std::os::windows::io::AsRawHandle as _;
use std::path::Path;

use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, FILE_DISPOSITION_FLAG_DELETE,
    FILE_DISPOSITION_FLAG_IGNORE_READONLY_ATTRIBUTE, FILE_DISPOSITION_FLAG_POSIX_SEMANTICS,
    FILE_DISPOSITION_INFO_EX, FILE_RENAME_INFO, FILE_RENAME_INFO_0, FileDispositionInfoEx,
    FileRenameInfo, FileRenameInfoEx, GetDriveTypeW, GetFileInformationByHandle,
    GetVolumeInformationByHandleW, SetFileInformationByHandle,
};
use windows_sys::Win32::System::WindowsProgramming::DRIVE_FIXED;

/// Require a local fixed drive before the v1 NTFS identity or handle-relative
/// mutation path is used. UNC paths and mapped network drives are not part of
/// the qualified Windows sandbox filesystem contract.
pub fn require_local_fixed_drive(root: &Path) -> io::Result<()> {
    let mut wide = root.as_os_str().encode_wide().collect::<Vec<_>>();
    wide.push(0);
    // SAFETY: `wide` is a live NUL-terminated UTF-16 drive-root pathname;
    // GetDriveTypeW borrows it for this synchronous call only.
    if unsafe { GetDriveTypeW(wide.as_ptr()) } == DRIVE_FIXED {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "exact Windows sandbox filesystem requires a local fixed drive",
        ))
    }
}

/// The existing v1 Sandbox marker's 64-bit NTFS identity and link evidence.
/// ReFS, SMB, and other filesystems are refused before these values are used.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NtfsFileInfo {
    pub volume_serial: u64,
    pub file_index: u64,
    pub number_of_links: u32,
}

/// Inspect a retained handle, accepting only local NTFS. This ensures the v1
/// `(device, inode)` marker never truncates a ReFS 128-bit file identifier.
pub fn ntfs_file_information(file: &File) -> io::Result<NtfsFileInfo> {
    let handle = file.as_raw_handle() as HANDLE;
    let mut info = MaybeUninit::<BY_HANDLE_FILE_INFORMATION>::zeroed();
    // SAFETY: `file` retains a live handle; `info` points to a writable buffer
    // of the exact Win32 structure size, and the API retains no pointer.
    if unsafe { GetFileInformationByHandle(handle, info.as_mut_ptr()) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: successful GetFileInformationByHandle initializes every field.
    let info = unsafe { info.assume_init() };

    let mut filesystem = [0_u16; 32];
    // SAFETY: optional outputs are null; the filesystem name points to a
    // writable UTF-16 buffer with its exact element count.
    let status = unsafe {
        GetVolumeInformationByHandleW(
            handle,
            std::ptr::null_mut(),
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            filesystem.as_mut_ptr(),
            filesystem.len() as u32,
        )
    };
    if status == 0 {
        return Err(io::Error::last_os_error());
    }
    let end = filesystem
        .iter()
        .position(|character| *character == 0)
        .unwrap_or(filesystem.len());
    if !String::from_utf16_lossy(&filesystem[..end]).eq_ignore_ascii_case("NTFS") {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "exact Windows sandbox identity requires local NTFS",
        ));
    }
    Ok(NtfsFileInfo {
        volume_serial: u64::from(info.dwVolumeSerialNumber),
        file_index: (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
        number_of_links: info.nNumberOfLinks,
    })
}

/// Atomically move the already-open source to an absent absolute pathname.
/// The source handle must have DELETE access. The caller validates and pins
/// each destination directory. Win32's wrapper rejects non-null RootDirectory
/// on current Windows, so the checked absolute name is used instead.
pub fn rename_open_noreplace(source: &File, destination: &Path) -> io::Result<()> {
    rename_open(source, destination, false)
}

/// Replace an existing, caller-validated regular-file name using the same
/// complete source-handle rename boundary as no-replace publication.
pub fn rename_open_replace(source: &File, destination: &Path) -> io::Result<()> {
    rename_open(source, destination, true)
}

fn rename_open(source: &File, destination: &Path, replace: bool) -> io::Result<()> {
    if !destination.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Windows rename destination must be absolute",
        ));
    }
    let wide = destination.as_os_str().encode_wide().collect::<Vec<_>>();
    if wide.is_empty() || wide.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid Windows rename path",
        ));
    }
    let bytes = wide.len().checked_mul(2).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "Windows rename path is too long",
        )
    })?;
    let bytes = u32::try_from(bytes).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "Windows rename path is too long",
        )
    })?;
    let name_offset = std::mem::offset_of!(FILE_RENAME_INFO, FileName);
    // The Win32 wrapper uses the zero-terminated name even though the length
    // is supplied separately. Reserve one UTF-16 NUL beyond FileNameLength.
    let allocation = name_offset
        .checked_add(bytes as usize)
        .and_then(|size| size.checked_add(2))
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "Windows rename buffer is too large",
            )
        })?;
    let words = allocation.div_ceil(std::mem::size_of::<u64>());
    let mut buffer = vec![0_u64; words];
    let header = buffer.as_mut_ptr().cast::<FILE_RENAME_INFO>();
    // SAFETY: the u64 allocation is aligned for FILE_RENAME_INFO, contains its
    // fixed header plus the complete zero-terminated UTF-16 name, and remains live for the
    // synchronous API call. Neither Win32 function retains the buffer.
    unsafe {
        // FileRenameInfoEx accepts IGNORE_READONLY_ATTRIBUTE only together
        // with REPLACE_IF_EXISTS. The ordinary class remains the no-replace
        // path so a missing/occupied destination has its original semantics.
        (*header).Anonymous = FILE_RENAME_INFO_0 {
            Flags: if replace {
                0x0000_0001 | 0x0000_0040
            } else {
                0
            },
        };
        (*header).RootDirectory = std::ptr::null_mut();
        (*header).FileNameLength = bytes;
        std::ptr::copy_nonoverlapping(
            wide.as_ptr(),
            buffer
                .as_mut_ptr()
                .cast::<u8>()
                .add(name_offset)
                .cast::<u16>(),
            wide.len(),
        );
        if SetFileInformationByHandle(
            source.as_raw_handle() as HANDLE,
            if replace {
                FileRenameInfoEx
            } else {
                FileRenameInfo
            },
            buffer.as_ptr().cast(),
            u32::try_from(allocation).map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "Windows rename buffer is too large",
                )
            })?,
        ) == 0
        {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

/// Mark exactly this already-open name for deletion on close. The handle must
/// have DELETE access and the caller must validate its kind and link count.
pub fn delete_open(file: &File) -> io::Result<()> {
    let disposition = FILE_DISPOSITION_INFO_EX {
        Flags: FILE_DISPOSITION_FLAG_DELETE
            | FILE_DISPOSITION_FLAG_IGNORE_READONLY_ATTRIBUTE
            | FILE_DISPOSITION_FLAG_POSIX_SEMANTICS,
    };
    // SAFETY: the live handle has caller-proven DELETE access, and the fixed
    // structure is passed by pointer for one synchronous call only.
    if unsafe {
        SetFileInformationByHandle(
            file.as_raw_handle() as HANDLE,
            FileDispositionInfoEx,
            (&raw const disposition).cast(),
            std::mem::size_of::<FILE_DISPOSITION_INFO_EX>() as u32,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
