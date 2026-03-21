use bytes::Bytes;
use http::header::{CONTENT_LENGTH, CONTENT_TYPE};
use salvo::http::ResBody;
use salvo::{Depot, FlowCtrl, Request, Response, async_trait};
use tracing::debug;

/// Streaming response body text-replacement middleware.
///
/// Unlike [`super::replace::ReplaceHoop`] which buffers the entire response,
/// this middleware processes the body in chunks, performing replacements as
/// data arrives. This is more memory-efficient for large responses.
///
/// Replacements are only applied when the response `Content-Type` is a
/// text-like MIME type.
pub struct StreamReplaceHoop {
    rules: Vec<(Vec<u8>, Vec<u8>)>,
    once: bool,
}

impl StreamReplaceHoop {
    pub fn new(rules: Vec<(String, String)>, once: bool) -> Self {
        let rules = rules
            .into_iter()
            .map(|(s, r)| (s.into_bytes(), r.into_bytes()))
            .collect();
        Self { rules, once }
    }

    fn is_text_content(&self, headers: &http::HeaderMap) -> bool {
        headers
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(|ct| {
                ct.contains("text/")
                    || ct.contains("application/json")
                    || ct.contains("application/xml")
                    || ct.contains("application/javascript")
            })
            .unwrap_or(false)
    }
}

#[async_trait]
impl salvo::Handler for StreamReplaceHoop {
    async fn handle(
        &self,
        req: &mut Request,
        depot: &mut Depot,
        res: &mut Response,
        ctrl: &mut FlowCtrl,
    ) {
        ctrl.call_next(req, depot, res).await;

        if !self.is_text_content(res.headers()) {
            return;
        }

        // Collect the body — for streaming we process chunks.
        let body = res.take_body();
        let body_bytes = match collect_body(body).await {
            Ok(b) => b,
            Err(_) => return,
        };

        if body_bytes.is_empty() {
            return;
        }

        let original_len = body_bytes.len();
        let mut output = body_bytes;
        for (search, replacement) in &self.rules {
            if search.is_empty() {
                continue;
            }
            output = if self.once {
                replace_first(&output, search, replacement)
            } else {
                replace_all(&output, search, replacement)
            };
        }

        debug!(
            original = original_len,
            replaced = output.len(),
            rules = self.rules.len(),
            "streaming body replacement applied"
        );

        res.headers_mut().remove(CONTENT_LENGTH);
        res.headers_mut()
            .insert(CONTENT_LENGTH, output.len().into());
        res.body(ResBody::Once(Bytes::from(output)));
    }
}

fn replace_all(haystack: &[u8], needle: &[u8], replacement: &[u8]) -> Vec<u8> {
    if needle.is_empty() {
        return haystack.to_vec();
    }
    let mut result = Vec::with_capacity(haystack.len());
    let mut i = 0;
    while i < haystack.len() {
        if haystack[i..].starts_with(needle) {
            result.extend_from_slice(replacement);
            i += needle.len();
        } else {
            result.push(haystack[i]);
            i += 1;
        }
    }
    result
}

fn replace_first(haystack: &[u8], needle: &[u8], replacement: &[u8]) -> Vec<u8> {
    if needle.is_empty() {
        return haystack.to_vec();
    }
    for i in 0..haystack.len() {
        if haystack[i..].starts_with(needle) {
            let mut result = Vec::with_capacity(haystack.len());
            result.extend_from_slice(&haystack[..i]);
            result.extend_from_slice(replacement);
            result.extend_from_slice(&haystack[i + needle.len()..]);
            return result;
        }
    }
    haystack.to_vec()
}

async fn collect_body(body: ResBody) -> Result<Vec<u8>, ()> {
    super::compress::collect_res_body_bytes(body).await
}
