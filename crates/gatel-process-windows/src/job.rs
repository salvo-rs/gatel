use std::io;
use std::mem::size_of;
use std::os::windows::io::{AsRawHandle, BorrowedHandle, FromRawHandle, OwnedHandle};

use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_ACTIVE_PROCESS,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOBOBJECT_BASIC_ACCOUNTING_INFORMATION,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectBasicAccountingInformation,
    JobObjectExtendedLimitInformation, QueryInformationJobObject, SetInformationJobObject,
};

#[derive(Debug)]
pub struct Job(OwnedHandle);
impl Job {
    /// The limit includes the root process. No breakaway permissions are set.
    pub fn new(process_limit: u32) -> io::Result<Self> {
        if !(1..=1024).contains(&process_limit) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "process limit must be 1..1024",
            ));
        }
        // SAFETY: Null security attributes make the anonymous handle private
        // and non-inheritable. A successful API result is an owned job handle.
        let raw = unsafe { CreateJobObjectW(None, None) }.map_err(io::Error::other)?;
        // SAFETY: Transfer the sole owned, valid handle to RAII immediately;
        // no other owner closes it, including on subsequent setup failure.
        let job = Self(unsafe { OwnedHandle::from_raw_handle(raw.0) });
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags =
            JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_ACTIVE_PROCESS;
        limits.BasicLimitInformation.ActiveProcessLimit = process_limit;
        // SAFETY: The handle remains owned by job; pointer, size and info class
        // match a fully initialized structure kept alive throughout this call.
        unsafe {
            SetInformationJobObject(
                job.handle(),
                JobObjectExtendedLimitInformation,
                &limits as *const _ as _,
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        }
        .map_err(io::Error::other)?;
        Ok(job)
    }
    fn handle(&self) -> HANDLE {
        HANDLE(self.0.as_raw_handle())
    }

    /// The caller must supply a process handle, ideally a suspended child.
    /// A wrong handle type is rejected by Windows, not dereferenced in Rust.
    pub fn assign(&self, process: BorrowedHandle<'_>) -> io::Result<()> {
        // SAFETY: Both borrowed handles are valid for the entire syscall;
        // Windows validates their object types and granted access rights.
        unsafe { AssignProcessToJobObject(self.handle(), HANDLE(process.as_raw_handle())) }
            .map_err(io::Error::other)
    }
    /// Fixed-size accounting, independent of the number of current/exited PIDs.
    pub fn active_processes(&self) -> io::Result<u32> {
        let mut accounting = JOBOBJECT_BASIC_ACCOUNTING_INFORMATION::default();
        // SAFETY: The output is aligned, initialized and exclusively borrowed,
        // with the exact size/layout required by this information class.
        unsafe {
            QueryInformationJobObject(
                Some(self.handle()),
                JobObjectBasicAccountingInformation,
                &mut accounting as *mut _ as _,
                size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
                None,
            )
        }
        .map_err(io::Error::other)?;
        Ok(accounting.ActiveProcesses)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn validates_limits_and_empty_job_accounting() {
        assert!(Job::new(0).is_err());
        assert!(Job::new(1025).is_err());
        for limit in [1, 64, 1024] {
            assert_eq!(Job::new(limit).unwrap().active_processes().unwrap(), 0);
        }
    }
}
