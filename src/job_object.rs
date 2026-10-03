//! Win32 job object with KILL_ON_JOB_CLOSE, hand-rolled FFI (the project
//! deliberately has no windows/winapi crate). A spawned child is assigned to
//! a job for the lifetime of its owner, which fixes two orphan classes at once:
//! - "отменить" used to `Child::kill()` only the direct child; yt-dlp's own
//!   children (ffmpeg mid-merge, aria2c as external downloader) survived and
//!   kept writing to the output folder;
//! - if the app itself dies (crash, task-manager kill, Ctrl+C), the OS closes
//!   the last job handle and takes the whole child tree with it.
//!
//! `TerminateJobObject` on cancel kills the tree in one call.

#[cfg(windows)]
mod imp {
    use std::ffi::c_void;

    type Handle = *mut c_void;

    #[repr(C)]
    struct IoCounters {
        read_ops: u64,
        write_ops: u64,
        other_ops: u64,
        read_bytes: u64,
        write_bytes: u64,
        other_bytes: u64,
    }

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
    struct ExtendedLimitInformation {
        basic: BasicLimitInformation,
        io: IoCounters,
        process_memory_limit: usize,
        job_memory_limit: usize,
        peak_process_memory_used: usize,
        peak_job_memory_used: usize,
    }

    const JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE: u32 = 0x2000;
    const JOB_OBJECT_EXTENDED_LIMIT_INFORMATION: i32 = 9;

    extern "system" {
        fn CreateJobObjectW(attrs: *mut c_void, name: *const u16) -> Handle;
        fn SetInformationJobObject(job: Handle, class: i32, info: *const c_void, len: u32) -> i32;
        fn AssignProcessToJobObject(job: Handle, process: Handle) -> i32;
        fn TerminateJobObject(job: Handle, exit_code: u32) -> i32;
        fn CloseHandle(h: Handle) -> i32;
    }

    pub struct Job {
        handle: Handle,
    }

    // The handle is process-global state; it is created on one thread and
    // then owned exclusively by the thread that waits on the child.
    unsafe impl Send for Job {}

    impl Job {
        /// Capture Win32 failures and reap the child: an unprotected tree
        /// could orphan descendants and leave pipe reader joins hung forever.
        pub fn create_and_assign(child: &mut std::process::Child) -> std::io::Result<Option<Self>> {
            use std::os::windows::io::AsRawHandle;
            let process: Handle = child.as_raw_handle();
            unsafe {
                let handle = CreateJobObjectW(std::ptr::null_mut(), std::ptr::null());
                if handle.is_null() {
                    let error = std::io::Error::last_os_error();
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(std::io::Error::new(error.kind(), format!("CreateJobObjectW: {error}")));
                }
                let mut info: ExtendedLimitInformation = std::mem::zeroed();
                info.basic.limit_flags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                let ok = SetInformationJobObject(
                    handle,
                    JOB_OBJECT_EXTENDED_LIMIT_INFORMATION,
                    &info as *const _ as *const c_void,
                    std::mem::size_of::<ExtendedLimitInformation>() as u32,
                );
                let failed = if ok == 0 { Some("SetInformationJobObject") }
                    else if AssignProcessToJobObject(handle, process) == 0 { Some("AssignProcessToJobObject") }
                    else { None };
                if let Some(operation) = failed {
                    let error = std::io::Error::last_os_error();
                    CloseHandle(handle);
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(std::io::Error::new(error.kind(), format!("{operation}: {error}")));
                }
                Ok(Some(Self { handle }))
            }
        }

        pub fn terminate(&self) {
            unsafe {
                TerminateJobObject(self.handle, 1);
            }
        }
    }

    impl Drop for Job {
        fn drop(&mut self) {
            unsafe {
                // Last handle closes -> KILL_ON_JOB_CLOSE reaps whatever of
                // the tree is still alive.
                CloseHandle(self.handle);
            }
        }
    }
}

/// Non-Windows placeholder so callers stay cfg-free.
#[cfg(not(windows))]
mod imp {
    pub struct Job;

    impl Job {
        pub fn create_and_assign(_child: &mut std::process::Child) -> std::io::Result<Option<Self>> {
            Ok(None)
        }
        pub fn terminate(&self) {}
    }
}

pub use imp::Job;

/// Assign before the child executes any user code: a fast loader must not
/// spawn descendants (or exit) in the CreateProcess -> AssignProcess window.
pub fn spawn(command: &mut std::process::Command, hidden: bool)
    -> std::io::Result<(std::process::Child, Option<Job>)>
{
    #[cfg(windows)]
    {
        use std::os::windows::{io::AsRawHandle, process::CommandExt};
        #[link(name = "ntdll")]
        extern "system" {
            fn NtResumeProcess(process: *mut std::ffi::c_void) -> i32;
            fn RtlNtStatusToDosError(status: i32) -> u32;
        }
        // std does not expose the primary thread handle on stable Rust.
        // NtResumeProcess resumes that suspended process through its owned
        // process handle, without PID/thread enumeration or reuse races.
        command.creation_flags(0x0000_0004 | if hidden { 0x0800_0000 } else { 0 });
        let mut child = command.spawn()?;
        let job = Job::create_and_assign(&mut child)?;
        let status = unsafe { NtResumeProcess(child.as_raw_handle()) };
        if status < 0 {
            let error = std::io::Error::from_raw_os_error(unsafe { RtlNtStatusToDosError(status) } as i32);
            let _ = child.kill();
            let _ = child.wait();
            return Err(std::io::Error::new(error.kind(), format!("NtResumeProcess: {error}")));
        }
        Ok((child, job))
    }
    #[cfg(not(windows))]
    {
        let _ = hidden;
        Ok((command.spawn()?, None))
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use std::process::{Command, Stdio};

    #[test]
    fn closing_job_reaps_child_without_an_explicit_kill() {
        let system = std::env::var_os("SystemRoot").map(std::path::PathBuf::from).unwrap_or_else(|| r"C:\Windows".into());
        let cwd = std::env::current_exe().unwrap();
        let mut command = Command::new(system.join("System32/PING.EXE"));
        command.args(["-n", "30", "127.0.0.1"])
            .current_dir(cwd.parent().unwrap())
            .stdout(Stdio::null()).stderr(Stdio::null());
        let (mut child, job) = spawn(&mut command, true).unwrap();
        let job = job.unwrap();
        drop(job);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        loop {
            if child.try_wait().unwrap().is_some() { break; }
            if std::time::Instant::now() > deadline {
                child.kill().ok(); child.wait().ok(); panic!("job close did not reap child");
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    #[test]
    fn assignment_failure_reports_the_win32_operation() {
        let system = std::env::var_os("SystemRoot").map(std::path::PathBuf::from).unwrap_or_else(|| r"C:\Windows".into());
        let cwd = std::env::current_exe().unwrap();
        let mut child = Command::new(system.join("System32/cmd.exe")).args(["/C", "exit", "0"])
            .current_dir(cwd.parent().unwrap())
            .stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap();
        child.wait().unwrap();
        let error = match Job::create_and_assign(&mut child) { Err(e) => e, Ok(_) => panic!("exited process was assigned") };
        assert!(error.to_string().contains("AssignProcessToJobObject"), "{error}");
    }

    #[test]
    fn terminating_job_closes_pipes_inherited_by_a_surviving_grandchild() {
        use std::io::Read;
        use std::io::Write;
        use std::time::Duration;
        let cwd = std::env::current_exe().unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let mut command = Command::new(&cwd);
        command.args(["--exact", "job_object::tests::pipe_tree_fixture", "--nocapture"])
            .env("SNATCH_JOB_PIPE_FIXTURE", listener.local_addr().unwrap().to_string())
            .current_dir(cwd.parent().unwrap())
            .stdout(Stdio::piped()).stderr(Stdio::piped());
        let (mut child, job) = spawn(&mut command, true).unwrap();
        let job = job.unwrap();
        // The fixture cannot spawn its grandchild until assignment completes.
        let (mut ready, _) = listener.accept().unwrap();
        ready.write_all(&[1]).unwrap();
        let mut stdout = child.stdout.take().unwrap();
        let mut stderr = child.stderr.take().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let out = std::thread::spawn(move || {
            stdout.read_to_end(&mut Vec::new()).unwrap(); tx.send(()).unwrap();
        });
        let err = std::thread::spawn(move || stderr.read_to_end(&mut Vec::new()).unwrap());
        child.wait().unwrap();
        assert!(rx.recv_timeout(Duration::from_millis(100)).is_err(), "grandchild should still hold stdout");
        job.terminate();
        rx.recv_timeout(Duration::from_secs(3)).unwrap();
        out.join().unwrap(); err.join().unwrap();
    }

    #[test]
    fn pipe_tree_fixture() {
        use std::io::Read;
        let Ok(address) = std::env::var("SNATCH_JOB_PIPE_FIXTURE") else { return };
        let mut ready = std::net::TcpStream::connect(address).unwrap();
        ready.read_exact(&mut [0u8; 1]).unwrap();
        let system = std::env::var_os("SystemRoot").map(std::path::PathBuf::from).unwrap_or_else(|| r"C:\Windows".into());
        let child = Command::new(system.join("System32/PING.EXE"))
            .args(["-n", "30", "127.0.0.1"]).stdout(Stdio::inherit()).stderr(Stdio::inherit()).spawn().unwrap();
        // std::process::Child does not kill on Drop: the grandchild keeps
        // both inherited pipe handles after this test process exits.
        drop(child);
    }
}
