//! Windows process-tree kill via a Job Object (SIRF-56).
//!
//! Windows has no process groups and no `kill -KILL <tree>`: `Child::kill()`
//! ends only the process it was given. The supervised command is `cmd.exe /C
//! "claude -p …"` (or a POSIX `sh`), so killing just that root left the real
//! agent and everything it started running in the worktree — two writers, and
//! `commit_all`/release/reset ran underneath a live agent.
//!
//! A Job Object is the OS-level answer: every process the root starts is a
//! member of its job automatically (members cannot break away unless the job
//! says so, and ours does not), and `TerminateJobObject` kills all members at
//! once. No process snapshot is walked, so a grandchild forked "just now"
//! cannot be missed the way it can with `taskkill /T`.
//!
//! The FFI is a handful of `kernel32` calls, declared by hand so the crate set
//! stays as small as AGENTS.md requires (no `windows-sys`).
//!
//! # Breakaway is deliberately NOT allowed
//!
//! The job does not set `JOB_OBJECT_LIMIT_BREAKAWAY_OK`. A process in the
//! agent's tree that calls `CreateProcess` with `CREATE_BREAKAWAY_FROM_JOB`
//! therefore gets `ERROR_ACCESS_DENIED` (outside a job that flag is simply
//! ignored). That is a behaviour change for the rare tool that insists on
//! escaping its job (some service launchers and installers), accepted on
//! purpose: allowing breakaway would let exactly the processes this module
//! exists to kill walk out of it.
//!
//! # The one remaining race
//!
//! `std::process::Command` cannot spawn suspended and hand back the primary
//! thread handle, so the job is assigned *after* `spawn()` returns, not before
//! the child's first instruction. A process the root starts in that window —
//! microseconds, against a `cmd.exe`/`sh` that still has to load, parse its
//! command line and call `CreateProcess` — would be outside the job. If the
//! assignment itself fails (the root already ran in a job that forbids nesting,
//! pre-Windows 8) [`kill_tree`] falls back to `taskkill /T /F`, a snapshot-based
//! walk with its own (larger) race. On the kill path `taskkill /T` also runs
//! BEFORE `TerminateJobObject` even when the job is fine, as a backstop for a
//! process that slipped through the spawn-to-assign window (it can only walk
//! from a root that is still alive, hence the order). Neither path ever fails a
//! run: tree-kill is best-effort and the root is always also killed with
//! `Child::kill()`.

use std::ffi::c_void;
use std::mem::size_of;
use std::os::windows::io::AsRawHandle;
use std::process::{Child, Command};

type Handle = *mut c_void;

#[link(name = "kernel32")]
extern "system" {
    fn CreateJobObjectW(attrs: *mut c_void, name: *const u16) -> Handle;
    fn SetInformationJobObject(job: Handle, class: i32, info: *mut c_void, len: u32) -> i32;
    fn AssignProcessToJobObject(job: Handle, process: Handle) -> i32;
    fn TerminateJobObject(job: Handle, exit_code: u32) -> i32;
    fn CloseHandle(h: Handle) -> i32;
}

/// `JobObjectExtendedLimitInformation`.
const JOB_OBJECT_EXTENDED_LIMIT_INFORMATION_CLASS: i32 = 9;
/// Closing the last handle to the job terminates every process in it.
const JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE: u32 = 0x2000;

/// `JOBOBJECT_BASIC_LIMIT_INFORMATION`. `repr(C)` supplies the same padding
/// the Windows headers do, on x86, x86_64 and aarch64 alike.
#[repr(C)]
#[derive(Default)]
struct BasicLimits {
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

/// `IO_COUNTERS`.
#[repr(C)]
#[derive(Default)]
struct IoCounters {
    counts: [u64; 6],
}

/// `JOBOBJECT_EXTENDED_LIMIT_INFORMATION`.
#[repr(C)]
#[derive(Default)]
struct ExtendedLimits {
    basic: BasicLimits,
    io: IoCounters,
    process_memory_limit: usize,
    job_memory_limit: usize,
    peak_process_memory_used: usize,
    peak_job_memory_used: usize,
}

/// A Job Object holding one supervised child and (by inheritance) everything
/// that child starts. The handle is closed on drop.
///
/// While the job is *armed* (the default) closing the handle kills the whole
/// tree: a sirius that dies, panics, or bails out of supervision on an I/O
/// error never leaves an unsupervised agent behind. [`TreeJob::release`]
/// disarms it for the normal-exit path.
pub struct TreeJob(Handle);

impl TreeJob {
    /// Create an armed job and put `child` in it. `None` if either step fails —
    /// the caller then relies on [`kill_tree`]'s `taskkill` fallback.
    pub fn adopt(child: &Child) -> Option<TreeJob> {
        // SAFETY: plain Win32 calls; `job` is owned by the returned `TreeJob`
        // (or closed on the failure paths) and `child`'s handle is valid for
        // the borrow.
        unsafe {
            let job = CreateJobObjectW(std::ptr::null_mut(), std::ptr::null());
            if job.is_null() {
                return None;
            }
            let job = TreeJob(job);
            // Best effort: without it a dead sirius leaves its agent running,
            // but `terminate` still works, so the job is worth keeping.
            job.set_kill_on_close(true);
            if AssignProcessToJobObject(job.0, child.as_raw_handle()) == 0 {
                return None;
            }
            Some(job)
        }
    }

    fn set_kill_on_close(&self, on: bool) -> bool {
        let mut info = ExtendedLimits::default();
        if on {
            info.basic.limit_flags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        }
        // SAFETY: `info` is a correctly laid-out, live struct of the size we pass.
        unsafe {
            SetInformationJobObject(
                self.0,
                JOB_OBJECT_EXTENDED_LIMIT_INFORMATION_CLASS,
                std::ptr::addr_of_mut!(info).cast(),
                size_of::<ExtendedLimits>() as u32,
            ) != 0
        }
    }

    /// Kill every process in the job (exit code 1, as `Child::kill` gives).
    pub fn terminate(&self) -> bool {
        // SAFETY: valid job handle owned by `self`.
        unsafe { TerminateJobObject(self.0, 1) != 0 }
    }

    /// The agent exited on its own: stop the job killing anything when the
    /// handle closes. A process the agent left running is deliberately NOT
    /// killed on a normal exit — that is what unix does (`kill_descendants` only
    /// runs on a timeout), and an integration command may legitimately leave a
    /// helper behind. It simply stops being job-managed.
    pub fn release(&self) {
        self.set_kill_on_close(false);
    }
}

impl Drop for TreeJob {
    fn drop(&mut self) {
        // SAFETY: the handle is ours and closed exactly once.
        unsafe {
            CloseHandle(self.0);
        }
    }
}

/// Kill `child` and everything beneath it, on the timeout path only.
///
/// Two steps, in this order: `taskkill /T /F` first, walking the tree from the
/// still-live root (one extra spawn; it also catches a process that escaped the
/// spawn-to-assign window, which the job cannot see), then `TerminateJobObject`
/// for every member of the job — including anything started while `taskkill`
/// ran. Without a job only the first step happens. The caller still calls
/// `Child::kill()` afterwards to reap the root.
pub fn kill_tree(child: &Child, job: Option<&TreeJob>) {
    taskkill_tree(child.id());
    if let Some(job) = job {
        job.terminate();
    }
}

/// `taskkill /T /F /PID pid`, best-effort. Run by absolute path under
/// `%SystemRoot%\System32`: Rust's own lookup of a bare name searches the
/// executable's directory first, so a `taskkill.exe` planted beside
/// `sirius.exe` would otherwise run.
fn taskkill_tree(pid: u32) {
    let root = std::env::var_os("SystemRoot").unwrap_or_else(|| r"C:\Windows".into());
    let exe = std::path::Path::new(&root)
        .join("System32")
        .join("taskkill.exe");
    let _ = Command::new(exe)
        .args(["/T", "/F", "/PID", &pid.to_string()])
        .output();
}

#[cfg(test)]
mod tests {
    use super::*;

    // The struct handed to SetInformationJobObject must match the Windows
    // headers byte for byte, or the call misreads `limit_flags`.
    #[test]
    fn the_job_structs_match_the_windows_headers() {
        #[cfg(target_pointer_width = "64")]
        {
            assert_eq!(size_of::<BasicLimits>(), 64);
            assert_eq!(size_of::<ExtendedLimits>(), 144);
        }
        #[cfg(target_pointer_width = "32")]
        {
            assert_eq!(size_of::<BasicLimits>(), 48);
            assert_eq!(size_of::<ExtendedLimits>(), 112);
        }
        assert_eq!(size_of::<IoCounters>(), 48);
    }
}
