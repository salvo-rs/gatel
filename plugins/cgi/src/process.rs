//! Linux process-group primitive. Not a sandbox or a complete platform Job.
//!
//! Keep the leader waitable until group signalling is finished, so a cached
//! numeric PGID is never signalled after reaping and possible PID reuse.
use std::ffi::{OsStr, OsString};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::process::{ExitStatus, Stdio};
use std::time::Duration;

use gatel_cgi_protocol::environment::Environment;
use nix::errno::Errno;
use nix::sys::signal::{Signal, killpg};
use nix::sys::wait::{Id, WaitPidFlag, WaitStatus, waitid};
use nix::unistd::Pid;
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};

pub struct Pipes {
    pub stdin: ChildStdin,
    pub stdout: ChildStdout,
    pub stderr: ChildStderr,
}

/// The caller must not use waitpid(-1), ignore SIGCHLD, or otherwise reap this
/// child outside this owner. Descendants escaping the group are not contained.
pub struct Process {
    child: Child,
    pid: Pid,
    signal_pending: bool,
}
impl Process {
    /// Explicit executable/cwd/arguments and a cleared environment; no shell
    /// expansion, PATH search, inherited secrets, or request-selected program.
    /// Call only inside admitted work, never during configuration preparation.
    pub fn spawn(
        program: &Path,
        arguments: &[OsString],
        cwd: &Path,
        environment: &Environment,
    ) -> io::Result<(Self, Pipes)> {
        if !program.is_absolute() || !cwd.is_absolute() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "CGI executable and cwd must be absolute",
            ));
        }
        let mut command = Command::new(program);
        command
            .args(arguments)
            .current_dir(cwd)
            .env_clear()
            .envs(
                environment
                    .iter()
                    .map(|(name, value)| (OsStr::new(name), OsStr::from_bytes(value))),
            )
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .kill_on_drop(true);
        let child = command.spawn()?;
        let pid = Pid::from_raw(child.id().expect("unreaped child has a PID") as i32);
        let mut process = Self {
            child,
            pid,
            signal_pending: true,
        };
        let pipes = Pipes {
            stdin: process
                .child
                .stdin
                .take()
                .ok_or_else(|| io::Error::other("missing CGI stdin"))?,
            stdout: process
                .child
                .stdout
                .take()
                .ok_or_else(|| io::Error::other("missing CGI stdout"))?,
            stderr: process
                .child
                .stderr
                .take()
                .ok_or_else(|| io::Error::other("missing CGI stderr"))?,
        };
        Ok((process, pipes))
    }

    pub fn id(&self) -> u32 {
        self.pid.as_raw() as u32
    }

    /// Observe without reaping. Cancellation is safe; no background blocking
    /// waiter is created. The unreaped leader reserves its process-group ID.
    pub async fn exited(&self) -> io::Result<ExitStatus> {
        loop {
            match self.observe()? {
                WaitStatus::Exited(_, code) => return Ok(ExitStatus::from_raw(code << 8)),
                WaitStatus::Signaled(_, signal, core) => {
                    return Ok(ExitStatus::from_raw(signal as i32 | if core { 128 } else { 0 }));
                }
                _ => tokio::time::sleep(Duration::from_millis(10)).await,
            }
        }
    }

    fn observe(&self) -> io::Result<WaitStatus> {
        waitid(
            Id::Pid(self.pid),
            WaitPidFlag::WEXITED | WaitPidFlag::WNOHANG | WaitPidFlag::WNOWAIT,
        )
        .map_err(io::Error::from)
    }

    fn terminate(&mut self) -> io::Result<()> {
        if !self.signal_pending {
            return Ok(());
        }
        // ECHILD means ownership was violated; do not signal a possibly reused
        // ID. Never turn that condition into a successful cleanup.
        self.observe()?;
        match killpg(self.pid, Signal::SIGKILL) {
            Ok(()) | Err(Errno::ESRCH) => self.signal_pending = false,
            Err(error) => return Err(error.into()),
        }
        Ok(())
    }

    /// Signal the entire group, then reap our direct child. This does NOT prove
    /// that non-child descendants have been reaped or escaped descendants have
    /// exited; a production platform Job needs a stronger containment boundary.
    pub async fn terminate_and_reap(&mut self) -> io::Result<ExitStatus> {
        self.terminate()?;
        self.child.wait().await
    }
}
impl Drop for Process {
    fn drop(&mut self) {
        // Synchronous emergency group termination, followed by Tokio's child
        // kill-on-drop/reaper fallback. Explicit cleanup is the verified path.
        let _ = self.terminate();
    }
}
