//! Protocol-only building blocks shared by CGI transports. No sockets, runtime,
//! subprocesses or compiler dependency. See docs/CGI.md for the supported profile.
#![forbid(unsafe_code)]
pub mod environment;
pub mod fastcgi;
pub mod head;
pub mod scgi;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Limit,
    InvalidEnvironment,
    DuplicateEnvironment,
    InvalidHeader,
    DuplicateHeader,
    InvalidStatus,
    UnsupportedLocalRedirect,
    InvalidRecord,
    UnexpectedRecord,
    StreamClosed,
    IncompleteResponse,
    BackendRejected,
    BodyLength,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "CGI protocol error: {self:?}")
    }
}
impl std::error::Error for Error {}
