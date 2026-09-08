//! Bounded directory browsing, with URL paths distinct from HTML labels.
use std::path::Path;

use salvo_core::Depot;
use salvo_core::http::{Response, StatusCode};

const MAX_ENTRIES: usize = 10_000;
const MAX_HTML_BYTES: usize = 2 * 1024 * 1024;

pub(crate) async fn write(
    response: &mut Response,
    depot: &mut Depot,
    root: &Path,
    directory: &Path,
    request_path: &str,
) {
    match render(root, directory, request_path, MAX_ENTRIES, MAX_HTML_BYTES).await {
        Ok(body) => {
            response.status_code(StatusCode::OK);
            response.headers_mut().insert(
                http::header::CONTENT_TYPE,
                "text/html; charset=utf-8".parse().unwrap(),
            );
            let _ = response.write_body(body);
        }
        Err(status) => {
            crate::generated(depot);
            response.status_code(status);
            response
                .headers_mut()
                .insert(http::header::CACHE_CONTROL, "no-store".parse().unwrap());
            response.headers_mut().insert(
                http::header::CONTENT_TYPE,
                "text/plain; charset=utf-8".parse().unwrap(),
            );
            let _ = response.write_body(format!("{} directory listing unavailable\n", status.as_u16()));
        }
    }
}

async fn render(
    root: &Path,
    directory: &Path,
    request_path: &str,
    max_entries: usize,
    max_bytes: usize,
) -> Result<String, StatusCode> {
    let mut entries = tokio::fs::read_dir(directory)
        .await
        .map_err(|_| StatusCode::NOT_FOUND)?;
    let mut rows = Vec::new();
    let mut bytes = 0usize;
    let mut scanned = 0usize;
    while let Some(entry) = entries
        .next_entry()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
    {
        scanned += 1;
        if scanned > max_entries {
            return Err(StatusCode::SERVICE_UNAVAILABLE);
        }
        // Non-UTF8 names cannot round-trip through the request path decoder.
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        if !crate::file_path::public_component(&name) {
            continue;
        }
        let Ok(resolved) = tokio::fs::canonicalize(entry.path()).await else {
            continue;
        };
        if !crate::file_path::public_resolved(root, &resolved) {
            continue;
        }
        let Ok(metadata) = tokio::fs::metadata(&resolved).await else {
            continue;
        };
        if !metadata.is_file() && !metadata.is_dir() {
            continue;
        }
        let suffix = if metadata.is_dir() { "/" } else { "" };
        let row = format!(
            "<li><a href=\"./{}{suffix}\">{}{suffix}</a></li>\n",
            segment(&name),
            escape(&name)
        );
        bytes = bytes.saturating_add(row.len());
        if bytes > max_bytes {
            return Err(StatusCode::SERVICE_UNAVAILABLE);
        }
        rows.push((name, row));
    }
    rows.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    let mut body = format!(
        "<!doctype html>\n<meta charset=\"utf-8\">\n<title>{}</title>\n<ul>\n",
        escape(request_path)
    );
    if request_path != "/" {
        body.push_str("<li><a href=\"../\">../</a></li>\n");
    }
    if body.len().saturating_add(bytes).saturating_add(6) > max_bytes {
        return Err(StatusCode::SERVICE_UNAVAILABLE);
    }
    for (_, row) in rows {
        body.push_str(&row);
    }
    body.push_str("</ul>\n");
    Ok(body)
}

fn segment(name: &str) -> String {
    const HEX: &[u8] = b"0123456789ABCDEF";
    let mut output = String::new();
    for byte in name.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
            output.push(char::from(byte));
        } else {
            output.push('%');
            output.push(char::from(HEX[usize::from(byte >> 4)]));
            output.push(char::from(HEX[usize::from(byte & 15)]));
        }
    }
    output
}

pub(crate) fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn href_encoding_is_not_html_escaping() {
        assert_eq!(segment("a #&%?.txt"), "a%20%23%26%25%3F.txt");
        assert_eq!(segment("javascript:alert(1)"), "javascript%3Aalert%281%29");
        assert_eq!(segment("中.txt"), "%E4%B8%AD.txt");
        assert_eq!(escape("<script>\"&"), "&lt;script&gt;&quot;&amp;");
    }
    #[tokio::test]
    async fn listing_budgets_reject_partial_results() {
        let directory = std::env::temp_dir().join(format!(
            "gatel-listing-budget-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        tokio::fs::create_dir(&directory).await.unwrap();
        tokio::fs::write(directory.join("a.txt"), b"a").await.unwrap();
        tokio::fs::write(directory.join(".hidden"), b"hidden")
            .await
            .unwrap();
        let root = tokio::fs::canonicalize(&directory).await.unwrap();
        assert_eq!(
            render(&root, &root, "/", 1, 4096).await,
            Err(StatusCode::SERVICE_UNAVAILABLE)
        );
        assert_eq!(
            render(&root, &root, "/", 2, 10).await,
            Err(StatusCode::SERVICE_UNAVAILABLE)
        );
        let body = render(&root, &root, "/", 2, 4096).await.unwrap();
        assert!(body.contains("./a.txt") && !body.contains(".hidden"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn listing_hides_external_and_broken_symlinks() {
        let directory = std::env::temp_dir().join(format!(
            "gatel-listing-links-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        tokio::fs::create_dir_all(directory.join("root")).await.unwrap();
        tokio::fs::write(directory.join("secret"), b"secret")
            .await
            .unwrap();
        let root = tokio::fs::canonicalize(directory.join("root")).await.unwrap();
        tokio::fs::write(root.join("safe"), b"safe").await.unwrap();
        std::os::unix::fs::symlink(directory.join("secret"), root.join("external")).unwrap();
        std::os::unix::fs::symlink(root.join("absent"), root.join("broken")).unwrap();
        std::os::unix::fs::symlink(root.join("safe"), root.join("internal")).unwrap();
        let body = render(&root, &root, "/", 10, 4096).await.unwrap();
        assert!(body.contains("./internal") && body.contains("./safe"));
        assert!(!body.contains("external") && !body.contains("broken"));
    }
}
