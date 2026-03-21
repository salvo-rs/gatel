use std::collections::HashMap;

use http::header::CONTENT_TYPE;
use salvo::{Depot, FlowCtrl, Request, Response, async_trait};
use tracing::debug;

/// Custom error page middleware.
///
/// When the downstream handler produces a response with a status code that
/// matches one of the configured error pages, the response body is replaced
/// with the custom content. This is applied after the inner chain completes.
pub struct ErrorPagesHoop {
    /// Maps HTTP status code to the replacement body.
    pages: HashMap<u16, ErrorPage>,
}

struct ErrorPage {
    body: String,
    content_type: String,
}

impl ErrorPagesHoop {
    /// Create from a map of status code → body string.
    ///
    /// Content type is auto-detected: if the body starts with `<` it is
    /// assumed to be HTML, otherwise plain text.
    pub fn new(pages: HashMap<u16, String>) -> Self {
        let pages = pages
            .into_iter()
            .map(|(code, body)| {
                let content_type = if body.trim_start().starts_with('<') {
                    "text/html; charset=utf-8".to_string()
                } else {
                    "text/plain; charset=utf-8".to_string()
                };
                (code, ErrorPage { body, content_type })
            })
            .collect();
        Self { pages }
    }
}

#[async_trait]
impl salvo::Handler for ErrorPagesHoop {
    async fn handle(
        &self,
        req: &mut Request,
        depot: &mut Depot,
        res: &mut Response,
        ctrl: &mut FlowCtrl,
    ) {
        ctrl.call_next(req, depot, res).await;

        let status = match res.status_code {
            Some(s) => s.as_u16(),
            None => return,
        };

        if let Some(page) = self.pages.get(&status) {
            debug!(status, "serving custom error page");
            res.headers_mut().insert(
                CONTENT_TYPE,
                page.content_type.parse().unwrap_or_else(|_| {
                    http::HeaderValue::from_static("text/plain; charset=utf-8")
                }),
            );
            res.headers_mut().insert(
                http::header::CONTENT_LENGTH,
                http::HeaderValue::from(page.body.len()),
            );
            res.body(page.body.clone());
        }
    }
}
