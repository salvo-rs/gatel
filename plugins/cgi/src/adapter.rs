use std::sync::Arc;

use gatel_cgi_http::{Backend, BackendConfig, BackendFactory};
use gatel_plugin_runtime::{Bundle, DirectiveMeta};

#[derive(Debug)]
struct Cgi;
impl BackendFactory for Cgi {
    fn meta(&self) -> &'static DirectiveMeta {
        static META: DirectiveMeta = DirectiveMeta::terminal("cgi");
        &META
    }
    fn validate(&self, config: BackendConfig<'_>) -> Result<(), String> {
        if !cfg!(windows) {
            return Err("process CGI currently requires Windows Job Objects".into());
        }
        if config.remote_root.is_some() {
            return Err("process CGI does not support remote-root".into());
        }
        if !(1..=1024).contains(&config.process_limit.unwrap_or(64)) {
            return Err("process CGI process-limit must be 1..1024".into());
        }
        if !(1..=1024).contains(&config.concurrency) {
            return Err("process CGI concurrency must be 1..1024".into());
        }
        let target = std::path::Path::new(config.target);
        if !target
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("exe"))
        {
            return Err("process CGI requires an explicit .exe program".into());
        }
        Ok(())
    }
    fn prepare(&self, config: BackendConfig<'_>) -> Result<Arc<dyn Backend>, String> {
        self.validate(config)?;
        self.prepare_with_capacity(config, Arc::new(tokio::sync::Semaphore::new(config.concurrency)))
    }
    fn prepare_with_capacity(
        &self,
        config: BackendConfig<'_>,
        capacity: Arc<tokio::sync::Semaphore>,
    ) -> Result<Arc<dyn Backend>, String> {
        self.validate(config)?;
        #[cfg(windows)]
        {
            windows::prepare(config, capacity)
        }
        #[cfg(not(windows))]
        {
            let _ = capacity;
            Err("process CGI is unavailable on this platform".into())
        }
    }
}
pub fn bundle() -> Bundle {
    Bundle::new("cgi", "1").with(gatel_cgi_http::backend_directive(Cgi))
}

#[cfg(windows)]
mod windows {
    use std::sync::Mutex;

    use gatel_cgi_http::{ExchangeError, Invocation, Running, Sink};
    use gatel_cgi_protocol::head::Head;
    use gatel_plugin_runtime::BackgroundTask;

    use super::*;
    use crate::{exchange, job, supervisor};

    pub(super) fn prepare(
        config: BackendConfig<'_>,
        capacity: Arc<tokio::sync::Semaphore>,
    ) -> Result<Arc<dyn Backend>, String> {
        let executable = config
            .config_root
            .join(config.target)
            .canonicalize()
            .map_err(|_| "CGI executable is unavailable")?;
        if !executable.is_file() {
            return Err("CGI executable must be a regular file".into());
        }
        let directory = config
            .config_root
            .join(config.root)
            .canonicalize()
            .map_err(|_| "CGI root is unavailable")?;
        if !directory.is_dir() {
            return Err("CGI root must be a directory".into());
        }
        let (client, owner) = supervisor::channel_with_capacity(config.concurrency, capacity)?;
        Ok(Arc::new(Prepared {
            program: Arc::new(job::Program {
                executable,
                arguments: Vec::new(),
                directory,
                process_limit: config.process_limit.unwrap_or(64),
            }),
            client,
            owner: Mutex::new(Some(owner)),
        }))
    }
    struct Prepared {
        program: Arc<job::Program>,
        client: supervisor::Client<job::Invocation<Sink>>,
        owner: Mutex<Option<supervisor::Supervisor<job::Invocation<Sink>>>>,
    }
    impl Backend for Prepared {
        fn background(&self) -> Result<Vec<BackgroundTask>, String> {
            self.owner
                .lock()
                .map_err(|_| "CGI supervisor lock poisoned")?
                .take()
                .map(|owner| vec![owner.background()])
                .ok_or_else(|| "CGI supervisor already prepared".into())
        }
        fn run(&self, invocation: Invocation, output: Sink) -> Running {
            let client = self.client.clone();
            let program = self.program.clone();
            Box::pin(async move {
                let limits = exchange::Limits {
                    overall: invocation
                        .deadline
                        .saturating_duration_since(tokio::time::Instant::now()),
                    idle: invocation.idle_timeout,
                    max_request: invocation.max_request,
                    ..exchange::Limits::default()
                };
                client
                    .execute(job::Invocation::new(
                        program,
                        job::Request {
                            environment: invocation.environment,
                            input: invocation.input,
                            output,
                            length: invocation.length,
                            head_request: invocation.head_request,
                            deadline: invocation.deadline,
                            limits,
                        },
                    ))
                    .await
                    .map(|_| ())
                    .map_err(|error| match error {
                        supervisor::Error::Busy
                        | supervisor::Error::Closed
                        | supervisor::Error::Cancelled => ExchangeError::Unavailable,
                        supervisor::Error::Exchange(exchange::Error::Timeout) => ExchangeError::Timeout,
                        _ => ExchangeError::Failed,
                    })
            })
        }
    }
    impl exchange::Output for Sink {
        async fn head(&mut self, head: Head) -> std::io::Result<()> {
            Sink::head(self, head).await
        }
        async fn body(&mut self, body: &[u8]) -> std::io::Result<()> {
            Sink::body(self, body).await
        }
        async fn finish(&mut self) -> std::io::Result<()> {
            Sink::finish(self).await
        }
    }
}
