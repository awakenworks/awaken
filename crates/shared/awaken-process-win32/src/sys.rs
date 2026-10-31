// @formal surface obligation=sandbox.local_process_spawn_policy_and_waiter_lock_ownership_are_preserved
// @formal reason: Original Windows Job assignment, independent control and retained member exit observation.
//! Narrow audited Win32 ownership and accounting for an original private Job.
use std::{
    io,
    mem::{MaybeUninit, size_of},
    os::windows::io::{AsHandle, AsRawHandle, BorrowedHandle, FromRawHandle, OwnedHandle},
    sync::Arc,
};

use awaken_sync::InvariantLock;
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_BASIC_ACCOUNTING_INFORMATION, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JobObjectBasicAccountingInformation, JobObjectExtendedLimitInformation,
    QueryInformationJobObject, SetInformationJobObject, TerminateJobObject,
};

#[derive(Clone, Debug)]
pub(super) struct Job(Arc<JobOwner>);

mod members;

#[derive(Debug)]
struct JobOwner {
    handle: OwnedHandle,
    termination: InvariantLock<Option<io::Result<members::Members>>>,
}

fn copy_error(error: &io::Error) -> io::Error {
    error.raw_os_error().map_or_else(
        || io::Error::other(error.to_string()),
        io::Error::from_raw_os_error,
    )
}

impl Job {
    pub(super) fn new() -> io::Result<Self> {
        // SAFETY: null security attributes create a non-inheritable handle;
        // null name creates a private unnamed Job. No pointer is retained.
        let raw = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if raw.is_null() {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: successful creation transfers this single valid owned handle.
        let handle = unsafe { OwnedHandle::from_raw_handle(raw) };
        // SAFETY: this Win32 configuration is numeric fields and pointers only;
        // zero is a valid empty configuration for every field, as required by
        // SetInformationJobObject before setting the one owned lifetime limit.
        let mut limits =
            unsafe { MaybeUninit::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>::zeroed().assume_init() };
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        // SAFETY: live owned Job; class, aligned input and exact size agree.
        // Win32 copies the structure during this call. Failure drops the Job.
        let ok = unsafe {
            SetInformationJobObject(
                handle.as_raw_handle(),
                JobObjectExtendedLimitInformation,
                std::ptr::from_ref(&limits).cast(),
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self(Arc::new(JobOwner {
            handle,
            termination: InvariantLock::new(None),
        })))
    }

    pub(super) fn assign(&self, process: BorrowedHandle<'_>) -> io::Result<()> {
        // SAFETY: both handles remain alive throughout this synchronous call.
        // The caller supplies the newly spawned still-suspended native child,
        // never a PID lookup or an externally supplied unowned handle.
        let ok = unsafe {
            AssignProcessToJobObject(self.0.handle.as_raw_handle(), process.as_raw_handle())
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    pub(super) fn terminate(&self) -> io::Result<()> {
        let mut retained = match self.0.termination.lock() {
            Ok(retained) => retained,
            Err(_) => {
                self.signal_termination()?;
                return Err(io::Error::other(
                    "original Job exit observation is poisoned",
                ));
            }
        };
        // Only the first termination captures membership, before signalling.
        // Later callers retain those same wait handles and any query refusal.
        let observation =
            retained.get_or_insert_with(|| members::Members::capture(self.0.handle.as_handle()));
        // Attempt termination even if observing its members was refused.
        self.signal_termination()?;
        observation.as_ref().map(|_| ()).map_err(copy_error)
    }

    fn signal_termination(&self) -> io::Result<()> {
        // SAFETY: retained original Job handle with creation-granted rights.
        if unsafe { TerminateJobObject(self.0.handle.as_raw_handle(), 1) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    pub(super) fn is_quiescent(&self) -> io::Result<bool> {
        let retained = self
            .0
            .termination
            .lock()
            .map_err(|_| io::Error::other("original Job exit observation is poisoned"))?;
        let Some(observation) = retained.as_ref() else {
            return Ok(false);
        };
        observation
            .as_ref()
            .map_err(copy_error)?
            .is_quiescent(query_accounting(self.0.handle.as_handle())?)
    }
}

fn query_accounting(job: BorrowedHandle<'_>) -> io::Result<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION> {
    let mut information = MaybeUninit::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>::uninit();
    // SAFETY: the borrowed original Job remains alive throughout the call.
    // The class, aligned output buffer and exact size agree. No pointer is
    // retained by Win32. On failure the uninitialized output is never read.
    let ok = unsafe {
        QueryInformationJobObject(
            job.as_raw_handle(),
            JobObjectBasicAccountingInformation,
            information.as_mut_ptr().cast(),
            size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a successful call initialized this exact numeric Win32 class.
    Ok(unsafe { information.assume_init() })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_object_query_preserves_the_native_error_instead_of_empty_job() {
        // A live file is the wrong kernel object, not a dangling fake handle.
        // Win32 must refuse it; the adapter must not synthesize active == 0.
        let file = tempfile::tempfile().unwrap();
        let error = query_accounting(file.as_handle()).err().unwrap();
        assert!(error.raw_os_error().is_some_and(|code| code != 0));
        let job = Job::new().unwrap();
        assert!(
            !job.is_quiescent().unwrap(),
            "no termination observation exists yet"
        );
        job.terminate().unwrap();
        assert!(job.is_quiescent().unwrap());
    }

    #[test]
    fn poisoned_member_observation_refuses_a_successful_stop() {
        let job = Job::new().unwrap();
        let panicking = job.clone();
        assert!(
            std::thread::spawn(move || {
                let _held = panicking.0.termination.lock().unwrap();
                panic!("injected observation failure");
            })
            .join()
            .is_err()
        );
        assert!(
            job.terminate()
                .unwrap_err()
                .to_string()
                .contains("poisoned")
        );
        assert!(
            job.is_quiescent()
                .unwrap_err()
                .to_string()
                .contains("poisoned")
        );
    }
}
