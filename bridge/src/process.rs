use std::{
    io,
    process::{Child, Command, ExitStatus},
    time::Duration,
};

use wait_timeout::ChildExt;

/// Owns the tool4d process and its platform-specific process supervisor.
///
/// On Unix, tool4d is launched in a separate process group.
///
/// On Windows, tool4d is assigned to a Job Object configured with
/// JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE. Closing the adapter process therefore
/// terminates tool4d and every descendant that remains in the job.
pub struct ChildGuard {
    child: Child,
    shutdown_timeout: Duration,

    #[cfg(windows)]
    job: windows_job::WindowsJob,
}

impl ChildGuard {
    pub fn new(child: Child, shutdown_timeout: Duration) -> Result<Self, (Child, io::Error)> {
        #[cfg(windows)]
        let job = match windows_job::WindowsJob::assign(&child) {
            Ok(job) => job,
            Err(error) => return Err((child, error)),
        };

        Ok(Self {
            child,
            shutdown_timeout,

            #[cfg(windows)]
            job,
        })
    }

    pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        self.child.try_wait()
    }

    pub fn terminate(&mut self) {
        terminate_child(&mut self.child, self.shutdown_timeout);
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        self.terminate();

        /*
         * On Windows, `job` is dropped after this method returns. Closing its
         * handle enforces JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE as a final
         * process-tree cleanup mechanism.
         */
    }
}

/// Applies platform-specific process creation settings before spawning.
pub fn configure_process_supervision(command: &mut Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;

        // Create a dedicated process group with the child's PID as its PGID.
        command.process_group(0);
    }

    #[cfg(windows)]
    {
        /*
         * The child is assigned to a Job Object immediately after spawn.
         *
         * CREATE_BREAKAWAY_FROM_JOB is intentionally not used. Modern Windows
         * supports nested jobs, and requesting breakaway can fail when a
         * parent job does not explicitly permit it.
         */
        let _ = command;
    }

    #[cfg(not(any(unix, windows)))]
    {
        let _ = command;
    }
}

fn terminate_child(child: &mut Child, shutdown_timeout: Duration) {
    match child.try_wait() {
        Ok(Some(_)) => return,
        Ok(None) => {}
        Err(error) => {
            eprintln!(
                "tool4d-lsp-stdio: failed to inspect tool4d during shutdown: \
                 {error}"
            );
        }
    }

    eprintln!("tool4d-lsp-stdio: terminating tool4d");

    send_graceful_termination(child);

    match child.wait_timeout(shutdown_timeout) {
        Ok(Some(_)) => return,

        Ok(None) => {
            eprintln!(
                "tool4d-lsp-stdio: tool4d did not exit within {} seconds; \
                 forcing termination",
                shutdown_timeout.as_secs()
            );
        }

        Err(error) => {
            eprintln!("tool4d-lsp-stdio: failed while waiting for tool4d: {error}");
        }
    }

    force_terminate(child);

    if let Err(error) = child.wait() {
        eprintln!("tool4d-lsp-stdio: failed to reap tool4d: {error}");
    }
}

#[cfg(unix)]
fn send_graceful_termination(child: &mut Child) {
    let process_group = -(child.id() as libc::pid_t);

    // SAFETY: the child was placed in a dedicated process group at spawn.
    let result = unsafe { libc::kill(process_group, libc::SIGTERM) };

    if result != 0 {
        let error = io::Error::last_os_error();

        if error.raw_os_error() != Some(libc::ESRCH) {
            eprintln!(
                "tool4d-lsp-stdio: failed to terminate tool4d process group: \
                 {error}"
            );
        }
    }
}

#[cfg(windows)]
fn send_graceful_termination(_child: &mut Child) {
    /*
     * A console application can only receive CTRL_BREAK_EVENT reliably when
     * it has a suitable console and process group. Zed-launched language
     * servers do not necessarily meet those conditions.
     *
     * Leave tool4d running during the configured grace period. The Job Object
     * provides deterministic forced process-tree cleanup afterward.
     */
}

#[cfg(not(any(unix, windows)))]
fn send_graceful_termination(child: &mut Child) {
    if let Err(error) = child.kill() {
        eprintln!("tool4d-lsp-stdio: failed to terminate tool4d: {error}");
    }
}

#[cfg(unix)]
fn force_terminate(child: &mut Child) {
    let process_group = -(child.id() as libc::pid_t);

    // SAFETY: the child was placed in a dedicated process group at spawn.
    let result = unsafe { libc::kill(process_group, libc::SIGKILL) };

    if result != 0 {
        let error = io::Error::last_os_error();

        if error.raw_os_error() != Some(libc::ESRCH) {
            eprintln!(
                "tool4d-lsp-stdio: failed to kill tool4d process group: \
                 {error}"
            );
        }
    }
}

#[cfg(windows)]
fn force_terminate(child: &mut Child) {
    /*
     * Child::kill terminates the immediate process. ChildGuard still owns the
     * Job Object; closing that object subsequently terminates any descendants
     * left in the job.
     */
    if let Err(error) = child.kill()
        && error.kind() != io::ErrorKind::InvalidInput
    {
        eprintln!("tool4d-lsp-stdio: failed to kill tool4d: {error}");
    }
}

#[cfg(not(any(unix, windows)))]
fn force_terminate(child: &mut Child) {
    if let Err(error) = child.kill() {
        eprintln!("tool4d-lsp-stdio: failed to kill tool4d: {error}");
    }
}

#[cfg(windows)]
mod windows_job {
    use std::{
        ffi::c_void,
        io,
        mem::{size_of, zeroed},
        os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle, RawHandle},
        process::Child,
    };

    use windows_sys::Win32::{
        Foundation::HANDLE,
        System::JobObjects::{
            AssignProcessToJobObject, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
            SetInformationJobObject,
        },
    };

    /// A Windows Job Object owned by the adapter.
    ///
    /// `OwnedHandle` closes the Job Object automatically. Since the object is
    /// configured with KILL_ON_JOB_CLOSE, closing the adapter or dropping this
    /// value terminates all remaining processes assigned to the job.
    pub struct WindowsJob {
        _handle: OwnedHandle,
    }

    impl WindowsJob {
        pub fn assign(child: &Child) -> io::Result<Self> {
            let job = create_kill_on_close_job()?;

            let job_handle = raw_handle_to_windows(job.as_raw_handle());
            let process_handle = raw_handle_to_windows(child.as_raw_handle());

            // SAFETY:
            // - `job_handle` is a valid Job Object handle owned by `job`.
            // - `process_handle` is a valid process handle owned by `child`.
            // - both handles remain alive for the duration of the call.
            let assigned = unsafe { AssignProcessToJobObject(job_handle, process_handle) };

            if assigned == 0 {
                return Err(io::Error::last_os_error());
            }

            Ok(Self { _handle: job })
        }
    }

    fn create_kill_on_close_job() -> io::Result<OwnedHandle> {
        // SAFETY: a null security descriptor and null name create an unnamed
        // Job Object with default security.
        let raw_job = unsafe {
            windows_sys::Win32::System::JobObjects::CreateJobObjectW(
                std::ptr::null(),
                std::ptr::null(),
            )
        };

        if raw_job.is_null() {
            return Err(io::Error::last_os_error());
        }

        // SAFETY: `raw_job` is non-null and newly returned by
        // CreateJobObjectW, so ownership is transferred to OwnedHandle.
        let job = unsafe { OwnedHandle::from_raw_handle(raw_job as RawHandle) };

        // SAFETY: this structure consists of integer, pointer-sized, and POD
        // fields for which zero is a valid initial value.
        let mut information: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { zeroed() };

        information.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;

        let job_handle = raw_handle_to_windows(job.as_raw_handle());

        // SAFETY:
        // - `job_handle` identifies a valid Job Object.
        // - `information` points to a correctly sized extended-limit structure.
        // - the pointer remains valid for the duration of the call.
        let configured = unsafe {
            SetInformationJobObject(
                job_handle,
                JobObjectExtendedLimitInformation,
                std::ptr::addr_of!(information).cast::<c_void>(),
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };

        if configured == 0 {
            return Err(io::Error::last_os_error());
        }

        Ok(job)
    }

    fn raw_handle_to_windows(handle: RawHandle) -> HANDLE {
        handle.cast::<c_void>()
    }
}
