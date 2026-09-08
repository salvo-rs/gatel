//! Request-scoped offers shared by the encoder and local file producer.
//! File metadata allows negotiation before preconditions or body delivery.
use salvo_core::Depot;
use salvo_core::http::{Request, Response, StatusCode};

use crate::encode::Encoding;

const OFFERS: &str = "gatel.file.encoding-offers";
const SELECTED: &str = "gatel.file.encoding-selected";

#[derive(Clone, Default)]
pub(crate) struct Offers(Vec<(Encoding, u64)>);

pub(crate) fn push(depot: &mut Depot, encodings: &[Encoding], minimum: u64) -> Offers {
    let previous = depot.get::<Offers>(OFFERS).cloned().unwrap_or_default();
    let mut offers = Offers(encodings.iter().map(|encoding| (*encoding, minimum)).collect());
    offers.0.extend(previous.0.iter().copied());
    depot.insert(OFFERS, offers);
    previous
}

pub(crate) fn restore(depot: &mut Depot, previous: Offers) {
    depot.insert(OFFERS, previous);
}

pub(crate) fn available(depot: &Depot, request: &Request, size: u64, mime: &str) -> Vec<Encoding> {
    let mut headers = http::HeaderMap::new();
    headers.insert(
        http::header::CONTENT_TYPE,
        http::HeaderValue::from_str(mime).expect("static file MIME"),
    );
    crate::headers::file_preview(depot, &mut headers);
    let mime = headers
        .get(http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok());
    if crate::encode_metadata::no_transform(request.headers())
        || crate::encode_metadata::no_transform(&headers)
        || mime.is_some_and(|mime| !crate::encode::is_compressible(mime))
    {
        return Vec::new();
    }
    let mut available = Vec::new();
    if let Ok(offers) = depot.get::<Offers>(OFFERS) {
        for &(encoding, minimum) in &offers.0 {
            if size >= minimum && !available.contains(&encoding) {
                available.push(encoding);
            }
        }
    }
    available
}

pub(crate) fn selected(depot: &mut Depot) {
    depot.insert(SELECTED, true);
}

pub(crate) fn is_selected(depot: &Depot) -> bool {
    depot.get::<bool>(SELECTED).copied().unwrap_or(false)
}

pub(crate) fn reject(depot: &mut Depot, response: &mut Response) {
    crate::generated(depot);
    response.status_code(StatusCode::NOT_ACCEPTABLE);
    response.headers_mut().insert(
        http::header::CACHE_CONTROL,
        http::HeaderValue::from_static("no-store"),
    );
    // No unacceptable identity payload and no validator for an unselected file.
    // An explicit empty body suppresses Salvo's automatic error-page filler.
    *response.body_mut() = salvo_core::http::ResBody::Once(bytes::Bytes::new());
    response
        .headers_mut()
        .insert(http::header::CONTENT_LENGTH, http::HeaderValue::from_static("0"));
}
