//! OS-level confinement for one sandbox process.
//!
//! The parent already enforces a wall-clock deadline, but a deadline cannot help
//! if the parent itself dies: the child would keep running with a closed pipe,
//! invisible and unkillable-by-anyone. Both platform implementations exist to
//! close that hole, and both are best-effort: if the OS refuses, the sandbox
//! still works and only loses the guarantee, so a failure here must not stop the
//! host.

#[cfg(windows)]
pub use imp::{Job, configure};

#[cfg(not(windows))]
pub use imp::{Job, configure};

#[cfg(windows)]
mod imp {
    use std::ffi::c_void;
    use std::process::Command;

    /// `JOB_OBJECT_LIMIT_PROCESS_MEMORY`
    const LIMIT_PROCESS_MEMORY: u32 = 0x0000_0100;
    /// `JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION`
    const LIMIT_DIE_ON_UNHANDLED_EXCEPTION: u32 = 0x0000_0400;
    /// `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`
    const LIMIT_KILL_ON_JOB_CLOSE: u32 = 0x0000_2000;

    /// `JobObjectExtendedLimitInformation`
    const EXTENDED_LIMIT_INFORMATION: i32 = 9;

    #[repr(C)]
    struct BasicLimitInformation {
        per_process_user_time_limit: i64,
        per_job_user_time_limit: i64,
        limit_flags: u32,
        minimum_working_set_size: usize,
        maximum_working_set_size: usize,
        active_process_limit: u32,
        affinity: usize,
        priority_class: u32,
        scheduling_class: u32,
    }

    #[repr(C)]
    struct IoCounters {
        read_operation_count: u64,
        write_operation_count: u64,
        other_operation_count: u64,
        read_transfer_count: u64,
        write_transfer_count: u64,
        other_transfer_count: u64,
    }

    #[repr(C)]
    struct ExtendedLimitInformation {
        basic: BasicLimitInformation,
        io: IoCounters,
        process_memory_limit: usize,
        job_memory_limit: usize,
        peak_process_memory_used: usize,
        peak_job_memory_used: usize,
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn CreateJobObjectW(attributes: *mut c_void, name: *const u16) -> *mut c_void;
        fn SetInformationJobObject(
            job: *mut c_void,
            class: i32,
            info: *const c_void,
            length: u32,
        ) -> i32;
        fn AssignProcessToJobObject(job: *mut c_void, process: *mut c_void) -> i32;
        fn OpenProcess(access: u32, inherit: i32, pid: u32) -> *mut c_void;
        fn CloseHandle(handle: *mut c_void) -> i32;
    }

    /// A Job Object holding exactly one sandbox process.
    pub struct Job {
        handle: *mut c_void,
    }

    // The handle is owned exclusively and only used through kernel calls that
    // accept it from any thread.
    unsafe impl Send for Job {}

    impl Drop for Job {
        fn drop(&mut self) {
            // KILL_ON_JOB_CLOSE means this is also the "reap the child" call.
            unsafe { CloseHandle(self.handle) };
        }
    }

    impl Job {
        /// Creates the job and assigns `pid` to it.
        ///
        /// The memory ceiling is a backstop, not the primary limit: the engine
        /// already refuses to grow its heap past `memory_bytes`. Headroom covers
        /// the process itself plus engine bookkeeping, so the OS only steps in
        /// when something has gone badly wrong.
        pub fn attach(pid: u32, memory_bytes: usize) -> Result<Option<Self>, String> {
            const PROCESS_SET_QUOTA: u32 = 0x0100;
            const PROCESS_TERMINATE: u32 = 0x0001;

            let handle = unsafe { CreateJobObjectW(std::ptr::null_mut(), std::ptr::null()) };
            if handle.is_null() {
                return Err(format!(
                    "CreateJobObject failed: {}",
                    std::io::Error::last_os_error()
                ));
            }
            let job = Job { handle };

            let ceiling = memory_bytes
                .saturating_mul(2)
                .saturating_add(128 * 1024 * 1024);
            let info = ExtendedLimitInformation {
                basic: BasicLimitInformation {
                    per_process_user_time_limit: 0,
                    per_job_user_time_limit: 0,
                    limit_flags: LIMIT_PROCESS_MEMORY
                        | LIMIT_DIE_ON_UNHANDLED_EXCEPTION
                        | LIMIT_KILL_ON_JOB_CLOSE,
                    minimum_working_set_size: 0,
                    maximum_working_set_size: 0,
                    active_process_limit: 0,
                    affinity: 0,
                    priority_class: 0,
                    scheduling_class: 0,
                },
                io: IoCounters {
                    read_operation_count: 0,
                    write_operation_count: 0,
                    other_operation_count: 0,
                    read_transfer_count: 0,
                    write_transfer_count: 0,
                    other_transfer_count: 0,
                },
                process_memory_limit: ceiling,
                job_memory_limit: 0,
                peak_process_memory_used: 0,
                peak_job_memory_used: 0,
            };
            let set = unsafe {
                SetInformationJobObject(
                    job.handle,
                    EXTENDED_LIMIT_INFORMATION,
                    &info as *const _ as *const c_void,
                    std::mem::size_of::<ExtendedLimitInformation>() as u32,
                )
            };
            if set == 0 {
                return Err(format!(
                    "SetInformationJobObject failed: {}",
                    std::io::Error::last_os_error()
                ));
            }

            let process = unsafe { OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, 0, pid) };
            if process.is_null() {
                return Err(format!(
                    "OpenProcess({pid}) failed: {}",
                    std::io::Error::last_os_error()
                ));
            }
            let assigned = unsafe { AssignProcessToJobObject(job.handle, process) };
            let error = std::io::Error::last_os_error();
            unsafe { CloseHandle(process) };
            if assigned == 0 {
                return Err(format!("AssignProcessToJobObject failed: {error}"));
            }
            Ok(Some(job))
        }
    }

    /// Platform hook applied before the child is spawned.
    pub fn configure(_command: &mut Command) {}
}

#[cfg(all(unix, not(target_os = "macos")))]
mod imp {
    use std::os::unix::process::CommandExt;
    use std::process::Command;

    /// The kernel enforces orphan cleanup on Linux; nothing to hold on to.
    pub struct Job;

    impl Job {
        /// Linux has no per-process memory ceiling this cheap; the engine's own
        /// heap limit applies and `PR_SET_PDEATHSIG` covers orphan cleanup.
        pub fn attach(_pid: u32, _memory_bytes: usize) -> Result<Option<Self>, String> {
            Ok(None)
        }
    }

    /// Asks the kernel to kill the child when its parent dies.
    ///
    /// `pre_exec` runs between fork and exec, so only async-signal-safe calls are
    /// allowed here: `prctl` is one.
    pub fn configure(command: &mut Command) {
        const PR_SET_PDEATHSIG: i32 = 1;
        const SIGKILL: i32 = 9;
        unsafe extern "C" {
            fn prctl(option: i32, ...) -> i32;
        }
        // SAFETY: `prctl` is async-signal-safe and the closure allocates nothing.
        unsafe {
            command.pre_exec(|| {
                prctl(PR_SET_PDEATHSIG, SIGKILL);
                Ok(())
            });
        }
    }
}

#[cfg(not(any(windows, unix)))]
mod imp {
    use std::process::Command;

    /// No OP on platforms without a confinement mechanism.
    pub struct Job;

    impl Job {
        /// Nothing available.
        pub fn attach(_pid: u32, _memory_bytes: usize) -> Result<Option<Self>, String> {
            Ok(None)
        }
    }

    /// Nothing available.
    pub fn configure(_command: &mut Command) {}
}

#[cfg(target_os = "macos")]
mod imp {
    use std::process::Command;

    /// macOS has no user-space orphan reaper that fits here.
    pub struct Job;

    impl Job {
        /// Unsupported; the parent's `Drop` still kills the child.
        pub fn attach(_pid: u32, _memory_bytes: usize) -> Result<Option<Self>, String> {
            Ok(None)
        }
    }

    /// Nothing to configure.
    pub fn configure(_command: &mut Command) {}
}
