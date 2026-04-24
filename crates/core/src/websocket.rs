//! WebSocket upgrade detection.

use http::HeaderMap;

/// Check whether a set of HTTP request headers indicate a WebSocket upgrade.
pub fn is_websocket_upgrade(headers: &HeaderMap) -> bool {
    let has_upgrade_connection = headers
        .get_all(http::header::CONNECTION)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .any(|token| token.trim().eq_ignore_ascii_case("upgrade"));

    let has_websocket_upgrade = headers
        .get_all(http::header::UPGRADE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .any(|value| value.eq_ignore_ascii_case("websocket"));

    has_upgrade_connection && has_websocket_upgrade
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_websocket_upgrade() {
        let mut headers = HeaderMap::new();
        headers.insert(http::header::CONNECTION, "Upgrade".parse().unwrap());
        headers.insert(http::header::UPGRADE, "websocket".parse().unwrap());
        assert!(is_websocket_upgrade(&headers));
    }

    #[test]
    fn detects_websocket_upgrade_across_repeated_connection_headers() {
        let mut headers = HeaderMap::new();
        headers.append(http::header::CONNECTION, "keep-alive".parse().unwrap());
        headers.append(http::header::CONNECTION, "Upgrade".parse().unwrap());
        headers.insert(http::header::UPGRADE, "websocket".parse().unwrap());
        assert!(is_websocket_upgrade(&headers));
    }
}
