//! `access-log`: one event per request, when the request is actually over.
//!
//! A handler returning means the response head was decided, not that the body
//! reached the client. Logging there would report a streaming download as
//! instant and a cancelled one as successful. This directive therefore wraps
//! the response body and reports when that body ends, fails or is dropped.
//!
//! Where the line goes is configuration: standard error for a container, a file
//! for a machine that keeps its own logs. An access log is not a diagnostic
//! log, and giving it its own destination is what lets one be shipped without
//! the other.

use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Instant, SystemTime};

use bytes::Bytes;
use gatel_macros::FromNode;
use gatel_model::{ByteSize, Code, Diagnostic, Diagnostics, FromNode as _, Hasher, Node};
use gatel_observe::{Format, Outcome, Record, Rotate, Sink};
use gatel_plan::{BuildContext, Directive, DirectiveConfig, DirectiveMeta, Phase, Scopes};
use salvo_core::http::{Request, ResBody, Response};
use salvo_core::{Depot, FlowCtrl, Handler, async_trait};

/// What a file grows to before it is rolled, unless the operator says
/// otherwise.
///
/// Not "never": a gateway that fills a disk with its own access log has taken
/// down the thing it was in front of.
const DEFAULT_ROTATE: ByteSize = ByteSize(128 * 1024 * 1024);

#[derive(Debug, FromNode)]
struct Fields {
    #[gatel(prop, default = String::from("text"))]
    format: String,
    /// Off by default: a query string routinely carries tokens and personal
    /// data, and a log is the wrong place for either.
    #[gatel(prop, default = false)]
    query: bool,
    /// `stderr`, or `file:` and a path relative to the configuration.
    #[gatel(prop, default = String::from("stderr"))]
    output: String,
    /// Roll the file at this size. `0` never rolls, for a machine whose log
    /// rotation is somebody else's job.
    #[gatel(prop)]
    rotate: Option<ByteSize>,
    #[gatel(prop, default = 4)]
    keep: u32,
}

/// Where the lines go.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Output {
    Stderr,
    File { path: PathBuf, rotate: Rotate },
}

#[derive(Debug)]
pub struct Config {
    format: Format,
    query: bool,
    output: Output,
}

impl DirectiveConfig for Config {
    fn hash_into(&self, hasher: &mut Hasher) {
        hasher.update_str(self.format.as_str());
        hasher.update_bool(self.query);
        match &self.output {
            Output::Stderr => {
                hasher.update_str("stderr");
            }
            Output::File { path, rotate } => {
                hasher.update_str("file");
                hasher.update_str(&path.display().to_string());
                hasher.update_u64(rotate.size);
                hasher.update_u64(rotate.keep as u64);
            }
        }
    }

    fn build(&self, cx: BuildContext<'_>) -> Result<Arc<dyn Handler>, String> {
        // Opened here rather than at the first request, so a path that cannot
        // be written stops the reload while the previous one still serves.
        let sink = match &self.output {
            Output::Stderr => cx.telemetry.sinks.stderr(),
            Output::File { path, rotate } => cx.telemetry.sinks.file(&cx.root.join(path), *rotate)?,
        };
        Ok(Arc::new(AccessLogHandler {
            format: self.format,
            query: self.query,
            sink,
        }))
    }

    fn describe(&self) -> String {
        match &self.output {
            Output::Stderr => format!("access-log {}", self.format.as_str()),
            Output::File { path, .. } => {
                format!("access-log {} -> {}", self.format.as_str(), path.display())
            }
        }
    }
}

pub struct AccessLog;

impl Directive for AccessLog {
    fn meta(&self) -> &DirectiveMeta {
        // Outermost, so it observes what every other directive did — including
        // a guard that rejected the request before it reached a terminal. Fail
        // open: a gateway that stops serving because it cannot log is worse
        // than one that serves without a log line.
        static META: DirectiveMeta =
            DirectiveMeta::middleware("access-log", Phase::Observe, Scopes::BOTH).fail_open();
        &META
    }

    fn decode(&self, node: &Node, diagnostics: &mut Diagnostics) -> Option<Arc<dyn DirectiveConfig>> {
        let fields = Fields::from_node(node, diagnostics)?;
        let Some(format) = Format::parse(&fields.format) else {
            diagnostics.push(
                Diagnostic::error(
                    Code::InvalidValue,
                    node.origin.clone(),
                    format!("unknown access log format `{}`", fields.format),
                )
                .with_help(format!("known formats: {}", Format::ALL.join(", "))),
            );
            return None;
        };
        let output = decode_output(&fields, node, diagnostics)?;
        Some(Arc::new(Config {
            format,
            query: fields.query,
            output,
        }))
    }
}

fn decode_output(fields: &Fields, node: &Node, diagnostics: &mut Diagnostics) -> Option<Output> {
    let rotates = fields.rotate.is_some();
    match fields.output.as_str() {
        "stderr" => {
            if rotates {
                // Standard error is a stream somebody else owns. Accepting a
                // rotation setting here would be accepting one that does
                // nothing.
                diagnostics.push(
                    Diagnostic::error(
                        Code::InvalidValue,
                        node.origin.clone(),
                        "`rotate` needs a file to rotate",
                    )
                    .with_help("write to `file:...`, or let whatever reads stderr rotate it"),
                );
                return None;
            }
            Some(Output::Stderr)
        }
        other => {
            let Some(path) = other.strip_prefix("file:").filter(|path| !path.is_empty()) else {
                diagnostics.push(
                    Diagnostic::error(
                        Code::InvalidValue,
                        node.origin.clone(),
                        format!("unknown access log output `{other}`"),
                    )
                    .with_help("`stderr`, or `file:` and a path such as `file:logs/access.log`"),
                );
                return None;
            };
            Some(Output::File {
                path: PathBuf::from(path),
                rotate: Rotate {
                    size: fields.rotate.unwrap_or(DEFAULT_ROTATE).0,
                    keep: fields.keep as usize,
                },
            })
        }
    }
}

struct AccessLogHandler {
    format: Format,
    query: bool,
    sink: Arc<Sink>,
}

#[async_trait]
impl Handler for AccessLogHandler {
    async fn handle(
        &self,
        request: &mut Request,
        depot: &mut Depot,
        response: &mut Response,
        ctrl: &mut FlowCtrl,
    ) {
        let started = Instant::now();
        let at = SystemTime::now();
        let method = request.method().to_string();
        let version = version(request.version());
        let path = if self.query {
            request.uri().path_and_query().map(|value| value.to_string())
        } else {
            Some(request.uri().path().to_string())
        }
        .unwrap_or_else(|| "/".to_string());

        ctrl.call_next(request, depot, response).await;

        let pending = Pending {
            format: self.format,
            sink: Arc::clone(&self.sink),
            started,
            record: Record {
                client: depot.get::<std::net::IpAddr>("gatel.client_ip").ok().copied(),
                method,
                path,
                version,
                status: response.status_code.map(|code| code.as_u16()).unwrap_or(200),
                bytes: 0,
                duration: std::time::Duration::ZERO,
                outcome: Outcome::Completed,
                site: depot.get::<String>("gatel.site").cloned().unwrap_or_default(),
                route: depot.get::<String>("gatel.route").cloned().unwrap_or_default(),
                at,
            },
        };

        // Anything already fully in memory is finished now; a streaming body
        // is not, and gets a wrapper that reports when it really ends.
        match std::mem::take(response.body_mut()) {
            ResBody::None => {
                pending.emit(Outcome::Completed, 0);
                *response.body_mut() = ResBody::None;
            }
            ResBody::Once(bytes) => {
                pending.emit(Outcome::Completed, bytes.len() as u64);
                *response.body_mut() = ResBody::Once(bytes);
            }
            ResBody::Chunks(chunks) => {
                pending.emit(
                    Outcome::Completed,
                    chunks.iter().map(|chunk| chunk.len() as u64).sum(),
                );
                *response.body_mut() = ResBody::Chunks(chunks);
            }
            other => {
                *response.body_mut() = ResBody::Boxed(Box::pin(ObservedBody {
                    inner: other,
                    pending: Some(pending),
                    sent: 0,
                }));
            }
        }
    }
}

const fn version(version: http::Version) -> &'static str {
    match version {
        http::Version::HTTP_09 => "HTTP/0.9",
        http::Version::HTTP_10 => "HTTP/1.0",
        http::Version::HTTP_2 => "HTTP/2.0",
        http::Version::HTTP_3 => "HTTP/3.0",
        _ => "HTTP/1.1",
    }
}

/// A record waiting for the body to end.
struct Pending {
    format: Format,
    sink: Arc<Sink>,
    started: Instant,
    record: Record,
}

impl Pending {
    fn emit(mut self, outcome: Outcome, bytes: u64) {
        self.record.outcome = outcome;
        self.record.bytes = bytes;
        self.record.duration = self.started.elapsed();
        self.sink.write(&self.record.render(self.format));
    }
}

pin_project_lite::pin_project! {
    /// Wraps a response body and reports how it ended.
    ///
    /// `PinnedDrop` is what makes a cancelled request distinguishable from a
    /// completed one: if the body is dropped before its end, that is the only
    /// place anyone finds out.
    struct ObservedBody {
        #[pin]
        inner: ResBody,
        pending: Option<Pending>,
        sent: u64,
    }

    impl PinnedDrop for ObservedBody {
        fn drop(this: Pin<&mut Self>) {
            let this = this.project();
            if let Some(pending) = this.pending.take() {
                pending.emit(Outcome::Cancelled, *this.sent);
            }
        }
    }
}

impl http_body::Body for ObservedBody {
    type Data = Bytes;
    type Error = salvo_core::BoxedError;

    fn poll_frame(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<Option<Result<http_body::Frame<Self::Data>, Self::Error>>> {
        let mut this = self.project();
        match this.inner.as_mut().poll_frame(context) {
            Poll::Ready(Some(Ok(frame))) => {
                if let Some(data) = frame.data_ref() {
                    *this.sent += data.len() as u64;
                }
                Poll::Ready(Some(Ok(frame)))
            }
            Poll::Ready(Some(Err(error))) => {
                if let Some(pending) = this.pending.take() {
                    pending.emit(Outcome::Failed, *this.sent);
                }
                Poll::Ready(Some(Err(Box::new(error) as salvo_core::BoxedError)))
            }
            Poll::Ready(None) => {
                if let Some(pending) = this.pending.take() {
                    pending.emit(Outcome::Completed, *this.sent);
                }
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> http_body::SizeHint {
        self.inner.size_hint()
    }
}

#[cfg(test)]
mod tests {
    use gatel_model::{Origin, Value};

    use super::*;

    fn origin() -> Origin {
        Origin::synthetic("test")
    }

    fn node(pairs: &[(&str, Value)]) -> Node {
        let mut node = Node::new("access-log", origin());
        for (name, value) in pairs {
            node = node.with_prop(*name, value.clone());
        }
        node
    }

    fn decode(node: Node) -> (Option<Arc<dyn DirectiveConfig>>, Diagnostics) {
        let mut diagnostics = Diagnostics::new();
        let config = AccessLog.decode(&node, &mut diagnostics);
        (config, diagnostics)
    }

    fn string(value: &str) -> Value {
        Value::string(value, origin())
    }

    #[test]
    fn the_default_is_text_on_standard_error() {
        let (config, diagnostics) = decode(node(&[]));
        assert!(!diagnostics.has_errors());
        assert_eq!(config.unwrap().describe(), "access-log text");
    }

    #[test]
    fn all_three_formats_are_accepted() {
        for name in Format::ALL {
            let (config, diagnostics) = decode(node(&[("format", string(name))]));
            assert!(!diagnostics.has_errors(), "{name}");
            assert!(config.unwrap().describe().contains(name));
        }
    }

    #[test]
    fn an_unknown_format_lists_the_ones_that_exist() {
        let (config, diagnostics) = decode(node(&[("format", string("combined"))]));
        assert!(config.is_none());
        let help = diagnostics.iter().next().unwrap().help.clone().unwrap();
        assert!(help.contains("text") && help.contains("clf"), "{help}");
    }

    #[test]
    fn a_file_output_names_its_path() {
        let (config, diagnostics) = decode(node(&[("output", string("file:logs/access.log"))]));
        assert!(!diagnostics.has_errors());
        assert!(config.unwrap().describe().contains("logs/access.log"));
    }

    #[test]
    fn a_file_rolls_by_default_rather_than_filling_a_disk() {
        let (config, _) = decode(node(&[("output", string("file:a.log"))]));
        let mut hasher = Hasher::new();
        config.unwrap().hash_into(&mut hasher);
        let (explicit, _) = decode(node(&[
            ("output", string("file:a.log")),
            ("rotate", Value::string("128MiB", origin())),
        ]));
        let mut other = Hasher::new();
        explicit.unwrap().hash_into(&mut other);
        assert_eq!(
            hasher.finish(),
            other.finish(),
            "the default has to be a real size, not none"
        );
    }

    #[test]
    fn rotating_standard_error_is_refused_rather_than_ignored() {
        let (config, diagnostics) = decode(node(&[("rotate", string("10MB"))]));
        assert!(config.is_none());
        assert!(
            diagnostics
                .iter()
                .any(|d| d.message.contains("needs a file to rotate"))
        );
    }

    #[test]
    fn an_unknown_output_says_what_the_two_shapes_are() {
        let (config, diagnostics) = decode(node(&[("output", string("syslog"))]));
        assert!(config.is_none());
        let help = diagnostics.iter().next().unwrap().help.clone().unwrap();
        assert!(help.contains("stderr") && help.contains("file:"), "{help}");
    }

    #[test]
    fn an_empty_path_is_not_a_file() {
        let (config, _) = decode(node(&[("output", string("file:"))]));
        assert!(config.is_none());
    }

    #[test]
    fn changing_where_lines_go_changes_the_plan() {
        let hash = |pairs: &[(&str, Value)]| {
            let mut hasher = Hasher::new();
            decode(node(pairs)).0.unwrap().hash_into(&mut hasher);
            hasher.finish()
        };
        assert_ne!(hash(&[]), hash(&[("output", string("file:a.log"))]));
        assert_ne!(
            hash(&[("output", string("file:a.log"))]),
            hash(&[("output", string("file:b.log"))])
        );
        assert_ne!(hash(&[]), hash(&[("format", string("clf"))]));
    }

    #[test]
    fn a_file_is_opened_during_prepare_so_a_bad_path_stops_the_reload() {
        let directory = std::env::temp_dir().join("gatel-access-log-refuse");
        let _ = std::fs::create_dir_all(&directory);
        // The path names a directory, which cannot be appended to.
        let (config, _) = decode(node(&[("output", string("file:."))]));
        let error = config
            .unwrap()
            .build(BuildContext {
                root: &directory,
                state: None,
                telemetry: crate::testing::telemetry(),
            })
            .err()
            .expect("appending to a directory cannot work");
        assert!(error.contains("cannot write"), "{error}");
    }

    #[test]
    fn http_versions_are_named_the_way_a_log_reader_expects() {
        assert_eq!(version(http::Version::HTTP_11), "HTTP/1.1");
        assert_eq!(version(http::Version::HTTP_2), "HTTP/2.0");
        assert_eq!(version(http::Version::HTTP_10), "HTTP/1.0");
    }
}
