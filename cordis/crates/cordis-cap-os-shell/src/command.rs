//! Bounded native subprocess execution for trusted host capabilities.

use std::io::{self, Read};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

const OUTPUT_LIMIT: usize = 256 * 1024;

pub(super) fn run(mut command: Command, timeout: Duration) -> io::Result<Value> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut scope = ProcessScope::prepare(&mut command)?;
    let mut child = command.spawn()?;
    let pid = child.id();
    if let Err(error) = scope.attach(&mut child) {
        let _ = child.kill();
        let _ = child.wait();
        return Err(error);
    }

    let stdout = child.stdout.take().expect("stdout configured as pipe");
    let stderr = child.stderr.take().expect("stderr configured as pipe");
    let exhausted = Arc::new(AtomicBool::new(false));
    let out_flag = Arc::clone(&exhausted);
    let err_flag = Arc::clone(&exhausted);
    let out_reader = std::thread::spawn(move || capture(stdout, &out_flag));
    let err_reader = std::thread::spawn(move || capture(stderr, &err_flag));
    let (sender, receiver) = mpsc::sync_channel(1);
    let (release, released) = mpsc::sync_channel(1);
    let waiter = std::thread::spawn(move || {
        let _ = sender.send(observe_exit(&mut child));
        let _ = released.recv();
        child.wait()
    });

    let deadline = Instant::now() + timeout;
    let observed = loop {
        if exhausted.load(Ordering::Acquire) {
            break Err(io::Error::other("command output quota exceeded"));
        }
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            break Err(io::Error::new(io::ErrorKind::TimedOut, "command timed out"));
        };
        match receiver.recv_timeout(remaining.min(Duration::from_millis(10))) {
            Ok(result) => break result,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(_) => break Err(io::Error::other("command wait failed")),
        }
    };

    // Stop descendants before reaping the leader or joining their output readers.
    scope.kill();
    let _ = release.send(());
    let status = waiter
        .join()
        .map_err(|_| io::Error::other("command waiter failed"))?;
    let stdout = out_reader
        .join()
        .map_err(|_| io::Error::other("stdout reader failed"))?;
    let stderr = err_reader
        .join()
        .map_err(|_| io::Error::other("stderr reader failed"))?;
    observed?;
    let status = status?;
    let stdout = stdout?;
    let stderr = stderr?;
    #[cfg(unix)]
    let signal = {
        use std::os::unix::process::ExitStatusExt;
        status.signal()
    };
    #[cfg(not(unix))]
    let signal: Option<i32> = None;

    Ok(json!({
        "exit_code": status.code(),
        "signal": signal,
        "pid": pid,
        "stdout": String::from_utf8_lossy(&stdout),
        "stderr": String::from_utf8_lossy(&stderr),
        "stdout_bytes": stdout.len(),
        "stderr_bytes": stderr.len(),
        "stdout_ref": null,
        "stderr_ref": null,
        "truncated": false,
        "termination_reason": "exited",
        "process_scope": scope.kind(),
    }))
}

fn capture(reader: impl Read, exhausted: &AtomicBool) -> io::Result<Vec<u8>> {
    let mut output = Vec::new();
    let limit = OUTPUT_LIMIT as u64 + 1;
    if let Err(error) = reader.take(limit).read_to_end(&mut output) {
        exhausted.store(true, Ordering::Release);
        return Err(error);
    }
    if output.len() > OUTPUT_LIMIT {
        exhausted.store(true, Ordering::Release);
        return Err(io::Error::other("command output quota exceeded"));
    }
    Ok(output)
}

#[cfg(unix)]
fn observe_exit(child: &mut std::process::Child) -> io::Result<()> {
    // Keep the exited leader unreaped until its group is stopped, preventing PID reuse.
    loop {
        let mut info = std::mem::MaybeUninit::<libc::siginfo_t>::zeroed();
        // SAFETY: waitid only writes the supplied siginfo for this process's child.
        let result = unsafe {
            libc::waitid(
                libc::P_PID,
                child.id(),
                info.as_mut_ptr(),
                libc::WEXITED | libc::WNOWAIT,
            )
        };
        if result == 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

#[cfg(windows)]
fn observe_exit(child: &mut std::process::Child) -> io::Result<()> {
    child.wait().map(|_| ())
}

#[cfg(unix)]
struct ProcessScope {
    pid: Option<i32>,
}
#[cfg(unix)]
impl ProcessScope {
    fn prepare(command: &mut Command) -> io::Result<Self> {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
        Ok(Self { pid: None })
    }
    fn attach(&mut self, child: &mut std::process::Child) -> io::Result<()> {
        self.pid =
            Some(i32::try_from(child.id()).map_err(|_| io::Error::other("pid out of range"))?);
        Ok(())
    }
    fn kill(&mut self) {
        if let Some(pid) = self.pid.take() {
            // SAFETY: only the new group created for this command is addressed.
            unsafe {
                libc::killpg(pid, libc::SIGKILL);
            }
        }
    }
    fn kind(&self) -> &'static str {
        "process_group"
    }
}

#[cfg(windows)]
struct ProcessScope {
    job: std::os::windows::io::OwnedHandle,
}
#[cfg(windows)]
impl ProcessScope {
    fn prepare(command: &mut Command) -> io::Result<Self> {
        use std::os::windows::{io::FromRawHandle, process::CommandExt};
        use windows_sys::Win32::System::{JobObjects::*, Threading::CREATE_SUSPENDED};
        // SAFETY: the job owns its handle and is configured before any child runs.
        unsafe {
            let handle = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if handle.is_null() {
                return Err(io::Error::last_os_error());
            }
            let job = std::os::windows::io::OwnedHandle::from_raw_handle(handle);
            let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            if SetInformationJobObject(
                handle,
                JobObjectExtendedLimitInformation,
                (&raw const limits).cast(),
                std::mem::size_of_val(&limits) as u32,
            ) == 0
            {
                return Err(io::Error::last_os_error());
            }
            command.creation_flags(CREATE_SUSPENDED);
            Ok(Self { job })
        }
    }
    fn attach(&mut self, child: &mut std::process::Child) -> io::Result<()> {
        use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
        use windows_sys::Win32::{
            Foundation::INVALID_HANDLE_VALUE,
            System::{
                Diagnostics::ToolHelp::*,
                JobObjects::AssignProcessToJobObject,
                Threading::{OpenThread, ResumeThread, THREAD_SUSPEND_RESUME},
            },
        };
        // SAFETY: child is still suspended; it joins the job before its initial thread resumes.
        unsafe {
            if AssignProcessToJobObject(self.job.as_raw_handle(), child.as_raw_handle()) == 0 {
                return Err(io::Error::last_os_error());
            }
            let raw = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0);
            if raw == INVALID_HANDLE_VALUE {
                return Err(io::Error::last_os_error());
            }
            let snapshot = OwnedHandle::from_raw_handle(raw);
            let mut entry: THREADENTRY32 = std::mem::zeroed();
            entry.dwSize = std::mem::size_of::<THREADENTRY32>() as u32;
            let mut found = Thread32First(snapshot.as_raw_handle(), &mut entry);
            while found != 0 {
                if entry.th32OwnerProcessID == child.id() {
                    let raw = OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID);
                    if raw.is_null() {
                        return Err(io::Error::last_os_error());
                    }
                    let thread = OwnedHandle::from_raw_handle(raw);
                    if ResumeThread(thread.as_raw_handle()) == u32::MAX {
                        return Err(io::Error::last_os_error());
                    }
                    return Ok(());
                }
                found = Thread32Next(snapshot.as_raw_handle(), &mut entry);
            }
            Err(io::Error::other("suspended command thread not found"))
        }
    }
    fn kill(&mut self) {
        use std::os::windows::io::AsRawHandle;
        // SAFETY: the job contains only processes owned by this command.
        unsafe {
            windows_sys::Win32::System::JobObjects::TerminateJobObject(self.job.as_raw_handle(), 1);
        }
    }
    fn kind(&self) -> &'static str {
        "windows_job"
    }
}

impl Drop for ProcessScope {
    fn drop(&mut self) {
        self.kill();
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn stops_process_on_timeout() {
        let mut command = Command::new("/bin/sleep");
        command.arg("10");
        let error = run(command, Duration::from_millis(30)).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    }

    #[test]
    fn rejects_output_over_the_quota() {
        let error = run(Command::new("/usr/bin/yes"), Duration::from_secs(5)).unwrap_err();
        assert_eq!(error.to_string(), "command output quota exceeded");
    }
}
