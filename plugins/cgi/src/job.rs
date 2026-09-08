//! Windows CGI invocation joining process ownership with the exchange and
//! generation supervisor. Linux requires a stronger completion boundary first.
use std::ffi::OsString;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use gatel_cgi_protocol::environment::Environment;
use tokio::io::AsyncRead;
use tokio::time::Instant;

use crate::exchange::{self, Error, Limits, Output};
use crate::process::Process;
use crate::supervisor::Job;

/// Trusted immutable configuration, never derived from request fields.
pub struct Program {
    pub executable: PathBuf,
    pub arguments: Vec<OsString>,
    pub directory: PathBuf,
    /// Includes the root process; enforced before the executable resumes.
    pub process_limit: u32,
}

pub struct Request<O> {
    pub environment: Environment,
    pub input: Box<dyn AsyncRead + Unpin + Send>,
    pub output: O,
    pub length: u64,
    pub head_request: bool,
    /// Includes queue admission and startup, not just pipe transfer time.
    pub deadline: Instant,
    pub limits: Limits,
}

pub struct Invocation<O> {
    program: Arc<Program>,
    request: Option<Request<O>>,
    process: Option<Process>,
}
impl<O: Output + 'static> Invocation<O> {
    /// Inert: no process or task starts until the supervisor polls Job::run.
    pub fn new(program: Arc<Program>, request: Request<O>) -> Self {
        Self {
            program,
            request: Some(request),
            process: None,
        }
    }
}
impl<O: Output + 'static> Job for Invocation<O> {
    async fn run(&mut self) -> Result<u64, Error> {
        let mut request = self
            .request
            .take()
            .ok_or_else(|| io::Error::other("CGI job already started"))?;
        if request.length > request.limits.max_request {
            return Err(Error::RequestLength);
        }
        if Instant::now() >= request.deadline || request.limits.overall.is_zero() {
            return Err(Error::Timeout);
        }
        let (process, pipes) = Process::spawn_with_limit(
            &self.program.executable,
            &self.program.arguments,
            &self.program.directory,
            &request.environment,
            self.program.process_limit,
        )?;
        self.process = Some(process);
        let process = self.process.as_mut().expect("process just installed");
        request.limits.overall = request
            .limits
            .overall
            .min(request.deadline.saturating_duration_since(Instant::now()));
        let operation = exchange::run(
            pipes.stdin,
            pipes.stdout,
            pipes.stderr,
            request.input,
            async {
                // Root status alone is insufficient: no response finish before
                // terminating leftovers and observing an empty Windows Job.
                let status = process.exited().await?;
                process.terminate_and_reap().await?;
                Ok(status)
            },
            request.length,
            request.head_request,
            &mut request.output,
            request.limits,
        );
        tokio::time::timeout_at(request.deadline, operation)
            .await
            .map_err(|_| Error::Timeout)?
    }

    async fn cleanup(&mut self) -> io::Result<()> {
        if let Some(process) = &mut self.process {
            // Client cancellation never interrupts this phase. If the OS does
            // not confirm completion, the supervisor seals the generation.
            tokio::time::timeout(Duration::from_secs(2), process.terminate_and_reap())
                .await
                .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "CGI cleanup deadline exceeded"))??;
        }
        Ok(())
    }
}
