use salvo::http::StatusCode;
use salvo::{Depot, FlowCtrl, Request, Response, async_trait};
use tracing::debug;

/// HTTP redirect handler.
///
/// Returns a redirect response with a configurable status code (301, 302, 307, 308)
/// and a target URL. The target may contain `{path}` and `{query}` placeholders
/// that are expanded from the incoming request.
pub struct RedirectHandler {
    target_template: String,
    status: StatusCode,
}

impl RedirectHandler {
    /// Create a new redirect handler.
    ///
    /// `status_code` should be one of 301, 302, 307, 308. Invalid codes
    /// default to 302 (Found).
    pub fn new(target_template: String, status_code: u16) -> Self {
        let status = StatusCode::from_u16(status_code).unwrap_or(StatusCode::FOUND);
        Self {
            target_template,
            status,
        }
    }
}

#[async_trait]
impl salvo::Handler for RedirectHandler {
    async fn handle(
        &self,
        req: &mut Request,
        _depot: &mut Depot,
        res: &mut Response,
        _ctrl: &mut FlowCtrl,
    ) {
        let path = req.uri().path().to_string();
        let query = req.uri().query().unwrap_or("").to_string();

        let location = self
            .target_template
            .replace("{path}", &path)
            .replace("{query}", &query);

        debug!(
            status = self.status.as_u16(),
            location = location.as_str(),
            "redirecting"
        );

        res.status_code(self.status);
        let _ = res.add_header(http::header::LOCATION, location, true);
    }
}
