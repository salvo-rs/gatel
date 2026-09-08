use gatel_cgi_http::{ExchangeError, ExchangeRequest, Protocol, Running, Sink};
use gatel_cgi_protocol::head::Head;
use gatel_plugin_runtime::{Bundle, DirectiveMeta};

use crate::exchange;

#[derive(Debug)]
struct Scgi;
impl Protocol for Scgi {
    fn meta(&self) -> &'static DirectiveMeta {
        static META: DirectiveMeta = DirectiveMeta::terminal("scgi");
        &META
    }
    fn run(&self, request: ExchangeRequest, mut output: Sink) -> Running {
        Box::pin(async move {
            let limits = exchange::Limits {
                overall: request.timeout,
                idle: request.idle_timeout,
                max_request: request.max_request,
                ..exchange::Limits::default()
            };
            exchange::run(
                request.stream,
                request.input,
                &request.environment,
                request.length,
                request.head_request,
                &mut output,
                limits,
            )
            .await
            .map(|_| ())
            .map_err(|error| {
                if matches!(error, exchange::Error::Timeout) {
                    ExchangeError::Timeout
                } else {
                    ExchangeError::Failed
                }
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
pub fn bundle() -> Bundle {
    Bundle::new("scgi", "1").with(gatel_cgi_http::directive(Scgi))
}
