//! Windows containment with assignment before execution and explicit empty-job
//! verification. No async blocking waiters or raw Win32 calls in this crate.
use std::ffi::OsString;
use std::io;
use std::path::Path;
use std::process::{ExitStatus, Stdio};
use std::sync::Arc;
use std::time::Duration;

use gatel_cgi_protocol::environment::Environment;
use gatel_process_windows::Job;
use process_wrap::tokio::{ChildWrapper, CommandWrap, CommandWrapper, CreationFlags, JobObject, KillOnDrop};
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};

pub struct Pipes {
    pub stdin: ChildStdin,
    pub stdout: ChildStdout,
    pub stderr: ChildStderr,
}

/// A queryable outer Job, assigned while process-wrap's child is suspended.
/// process-wrap assigns its inner termination Job and resumes only afterwards.
/// Retaining our outer Job permits nonblocking whole-scope completion queries;
/// process-wrap's `wait()` can detach a blocking completion-port waiter, so we
/// deliberately never call it.
#[derive(Debug)]
struct Track(Arc<Job>);
impl CommandWrapper for Track {
    fn post_spawn(&mut self, _: &mut Command, child: &mut Child, _: &CommandWrap) -> io::Result<()> {
        let handle = child
            .process_handle()
            .ok_or_else(|| io::Error::other("missing child process handle"))?;
        self.0.assign(handle)
    }
}

pub struct Process {
    child: Box<dyn ChildWrapper>,
    tracking: Arc<Job>,
    pid: u32,
}
impl Process {
    /// Only explicit .exe programs; never cmd/bat automatic shell dispatch.
    /// The supplied environment must be UTF-8 (unlike Unix byte environments).
    pub fn spawn(
        program: &Path,
        arguments: &[OsString],
        cwd: &Path,
        environment: &Environment,
    ) -> io::Result<(Self, Pipes)> {
        Self::spawn_with_limit(program, arguments, cwd, environment, 64)
    }
    /// The process limit includes the root and all Job-contained descendants.
    pub fn spawn_with_limit(
        program: &Path,
        arguments: &[OsString],
        cwd: &Path,
        environment: &Environment,
        process_limit: u32,
    ) -> io::Result<(Self, Pipes)> {
        if !program.is_absolute()
            || !cwd.is_absolute()
            || !program
                .extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("exe"))
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "CGI requires an absolute .exe and cwd",
            ));
        }
        let environment: Vec<_> = environment
            .iter()
            .map(|(name, value)| {
                std::str::from_utf8(value)
                    .map(|value| (name, value))
                    .map_err(|_| {
                        io::Error::new(
                            io::ErrorKind::InvalidInput,
                            "CGI environment must be UTF-8 on Windows",
                        )
                    })
            })
            .collect::<io::Result<_>>()?;
        let tracking = Arc::new(Job::new(process_limit)?);
        let mut command = Command::new(program);
        command
            .args(arguments)
            .current_dir(cwd)
            .env_clear()
            .envs(environment)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // CREATE_NO_WINDOW = 0x08000000, retained when JobObject adds suspension.
        let mut flags = CreationFlags(Default::default());
        flags.0.0 = 0x08000000;
        let mut command = CommandWrap::from(command);
        command
            .wrap(KillOnDrop)
            .wrap(flags)
            .wrap(Track(tracking.clone()))
            .wrap(JobObject);
        let child = command.spawn()?;
        let pid = child.id().ok_or_else(|| io::Error::other("missing child PID"))?;
        let mut process = Self { child, tracking, pid };
        let pipes = Pipes {
            stdin: process
                .child
                .stdin()
                .take()
                .ok_or_else(|| io::Error::other("missing CGI stdin"))?,
            stdout: process
                .child
                .stdout()
                .take()
                .ok_or_else(|| io::Error::other("missing CGI stdout"))?,
            stderr: process
                .child
                .stderr()
                .take()
                .ok_or_else(|| io::Error::other("missing CGI stderr"))?,
        };
        Ok((process, pipes))
    }
    pub fn id(&self) -> u32 {
        self.pid
    }

    pub fn active_processes(&self) -> io::Result<usize> {
        self.tracking.active_processes().map(|count| count as usize)
    }

    /// Root exit is distinct from whole-job completion. Nonblocking queries
    /// avoid process-wrap's cancellation-unsafe blocking wait path entirely.
    pub async fn exited(&mut self) -> io::Result<ExitStatus> {
        loop {
            if let Some(status) = self.child.try_wait()? {
                return Ok(status);
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    pub async fn terminate_and_reap(&mut self) -> io::Result<ExitStatus> {
        self.child.start_kill()?;
        let status = self.exited().await?;
        while self.active_processes()? != 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        Ok(status)
    }
}
impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
        // Both Jobs have kill-on-close; no handles are exposed or inherited.
    }
}
