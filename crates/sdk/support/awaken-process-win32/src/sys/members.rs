// @formal surface obligation=sandbox.local_process_spawn_policy_and_waiter_lock_ownership_are_preserved
// @formal reason: Original Windows Job assignment, independent control and retained member exit observation.
//! Retained wait capabilities, never a PID-based termination mechanism.
use super::query_accounting;
use std::{
    io,
    mem::size_of,
    os::windows::io::{AsRawHandle, BorrowedHandle, FromRawHandle, OwnedHandle},
};
use windows_sys::Win32::{
    Foundation::{ERROR_INVALID_PARAMETER, ERROR_MORE_DATA, WAIT_OBJECT_0, WAIT_TIMEOUT},
    System::{
        JobObjects::{
            IsProcessInJob, JOBOBJECT_BASIC_ACCOUNTING_INFORMATION,
            JOBOBJECT_BASIC_PROCESS_ID_LIST, JobObjectBasicProcessIdList,
            QueryInformationJobObject,
        },
        Threading::{
            OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
            WaitForSingleObject,
        },
    },
};

#[derive(Debug)]
pub(super) struct Members {
    handles: Vec<OwnedHandle>,
    admitted: u32,
}

impl Members {
    pub(super) fn capture(job: BorrowedHandle<'_>) -> io::Result<Self> {
        let before = query_accounting(job)?;
        let mut handles = Vec::new();
        for pid in process_ids(job)? {
            let pid = u32::try_from(pid)
                .map_err(|_| io::Error::other("Job returned an invalid process identity"))?;
            // SAFETY: Win32 validates an ID read from the original private Job.
            // No signal uses this ID. Membership of the exact opened object is
            // checked below before retaining its non-inheritable wait handle.
            let raw = unsafe {
                OpenProcess(
                    PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
                    0,
                    pid,
                )
            };
            if raw.is_null() {
                let error = io::Error::last_os_error();
                // Invalid PID means the object is already gone, not merely
                // exiting. Access/inspection errors do not prove termination.
                if error.raw_os_error() == Some(ERROR_INVALID_PARAMETER as i32) {
                    continue;
                }
                return Err(error);
            }
            // SAFETY: successful OpenProcess transfers one valid owned handle.
            let handle = unsafe { OwnedHandle::from_raw_handle(raw) };
            let mut belongs = 0;
            // SAFETY: handles and writable BOOL remain alive throughout call.
            let ok = unsafe {
                IsProcessInJob(handle.as_raw_handle(), job.as_raw_handle(), &mut belongs)
            };
            if ok == 0 {
                return Err(io::Error::last_os_error());
            }
            if belongs == 0 {
                // A replaced or exiting member cannot be certified by a
                // foreign live object. Do not signal it or discard uncertainty.
                // SAFETY: this same retained handle grants SYNCHRONIZE.
                if unsafe { WaitForSingleObject(handle.as_raw_handle(), 0) } != WAIT_OBJECT_0 {
                    return Err(io::Error::other(
                        "original Job member identity changed before termination",
                    ));
                }
            } else {
                handles.push(handle);
            }
        }
        Ok(Self {
            handles,
            admitted: before.TotalProcesses,
        })
    }

    pub(super) fn is_quiescent(
        &self,
        now: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION,
    ) -> io::Result<bool> {
        if now.TotalProcesses != self.admitted {
            // A concurrently admitted process could disappear before another
            // snapshot retains it. Keep the original owner and uncertainty.
            return Err(io::Error::other(
                "original Job membership changed during termination; physical exit remains unconfirmed",
            ));
        }
        for handle in &self.handles {
            // SAFETY: retained exact process object grants SYNCHRONIZE. This
            // nonblocking wait consumes neither identity nor completion proof.
            match unsafe { WaitForSingleObject(handle.as_raw_handle(), 0) } {
                WAIT_OBJECT_0 => (),
                WAIT_TIMEOUT => return Ok(false),
                _ => return Err(io::Error::last_os_error()),
            }
        }
        Ok(now.ActiveProcesses == 0)
    }
}

fn process_ids(job: BorrowedHandle<'_>) -> io::Result<Vec<usize>> {
    // usize provides alignment for the flexible array. Every byte is initialized,
    // including a partially written MORE_DATA response. Bound native allocation.
    let mut storage = vec![0usize; 65];
    loop {
        let bytes = u32::try_from(storage.len() * size_of::<usize>())
            .map_err(|_| io::Error::other("Job membership buffer exceeds Win32 size"))?;
        // SAFETY: correct class, aligned live writable buffer and exact size;
        // Win32 retains no pointer and writes no more than the supplied bytes.
        let ok = unsafe {
            QueryInformationJobObject(
                job.as_raw_handle(),
                JobObjectBasicProcessIdList,
                storage.as_mut_ptr().cast(),
                bytes,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(ERROR_MORE_DATA as i32) {
                return Err(error);
            }
            if storage.len() >= 1_048_576 {
                return Err(io::Error::other(
                    "original Job is too large to confirm termination",
                ));
            }
            storage.resize(storage.len() * 2, 0);
            continue;
        }
        // SAFETY: successful query initialized the header. Check its count
        // against the real allocation before reading the initialized tail.
        let list = unsafe { &*storage.as_ptr().cast::<JOBOBJECT_BASIC_PROCESS_ID_LIST>() };
        let count = list.NumberOfProcessIdsInList as usize;
        let offset = std::mem::offset_of!(JOBOBJECT_BASIC_PROCESS_ID_LIST, ProcessIdList);
        if list.NumberOfAssignedProcesses as usize != count {
            return Err(io::Error::other(
                "original Job membership snapshot is incomplete",
            ));
        }
        if count
            .checked_mul(size_of::<usize>())
            .and_then(|tail| offset.checked_add(tail))
            .is_none_or(|end| end > bytes as usize)
        {
            return Err(io::Error::other("Job returned an oversized process list"));
        }
        // SAFETY: initialized tail is bounded by the checked allocation.
        return Ok(
            unsafe { std::slice::from_raw_parts(storage.as_ptr().byte_add(offset), count) }
                .to_vec(),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn active_members_and_changed_membership_cannot_be_confirmed_as_quiescent() {
        let retained = Members {
            handles: Vec::new(),
            admitted: 2,
        };
        for active in [0, 1, 2] {
            // SAFETY: numeric Win32 accounting fields admit the all-zero value.
            let mut now = unsafe { std::mem::zeroed::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() };
            now.TotalProcesses = 2;
            now.ActiveProcesses = active;
            assert_eq!(retained.is_quiescent(now).unwrap(), active == 0);
        }
        // SAFETY: same numeric structure; a new admission is never ignored even
        // if both the snapshot and current active count appear empty.
        let mut now = unsafe { std::mem::zeroed::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() };
        now.TotalProcesses = 3;
        assert!(
            retained
                .is_quiescent(now)
                .unwrap_err()
                .to_string()
                .contains("membership changed")
        );
    }
}
