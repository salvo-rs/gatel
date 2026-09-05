//! Sending part of a file, and sending all of one without reading it first.
//!
//! Two problems with the same fix. Reading a whole file into memory to answer a
//! request means one client asking for a large file costs that much memory, and
//! a handful of them cost all of it — so anything past a small threshold is
//! streamed off disk. And a client that asks for bytes 5,000,000 onward, which
//! is what every video player and every resumed download does, should get those
//! bytes rather than the file from the beginning.

use std::path::Path;

use salvo_core::http::{Request, ResBody, Response, StatusCode};
use tokio::io::{AsyncReadExt, AsyncSeekExt};

/// Files up to this size are read in one go. Bigger ones are streamed.
///
/// Small enough that a page of them is not a memory problem, large enough that
/// the ordinary web asset — a stylesheet, a script, an icon — is still one read
/// and one buffer.
const INLINE_LIMIT: u64 = 64 * 1024;

/// How much of a file the client asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wanted {
    Whole,
    /// Inclusive on both ends, which is how the header is written.
    Part {
        start: u64,
        end: u64,
    },
    /// Asked for, but there is nothing there to send.
    Unsatisfiable,
}

/// Read `Range`, and `If-Range` if it is there.
///
/// Anything malformed, and anything asking for more than one range, is treated
/// as a request for the whole file — which a server is always allowed to send.
/// Answering several ranges at once means a multipart body, and nothing that
/// matters asks for it.
pub fn wanted(request: &Request, size: u64, etag: &str) -> Wanted {
    let Some(asked) = header_of(request, http::header::RANGE) else {
        return Wanted::Whole;
    };
    // `If-Range` is what makes a resumed download safe: if the file changed
    // since the part the client already has, it gets the whole new one rather
    // than a second half that does not match its first.
    //
    // A header that is present but unreadable counts as not matching. Treating
    // it as absent would be reading "I only want a part if it is still the
    // same file" as "give me a part", which is the one direction that
    // corrupts a download.
    if let Some(condition) = request.headers().get(http::header::IF_RANGE)
        && condition.to_str().map(str::trim).ok() != Some(etag)
    {
        return Wanted::Whole;
    }

    let Some(spec) = asked.trim().strip_prefix("bytes=") else {
        return Wanted::Whole;
    };
    if spec.contains(',') {
        return Wanted::Whole;
    }
    let Some((from, to)) = spec.trim().split_once('-') else {
        return Wanted::Whole;
    };

    match (from.trim(), to.trim()) {
        // `bytes=-500`: the last 500 bytes.
        ("", last) => match last.parse::<u64>() {
            // A suffix of an empty file is a request nothing can satisfy.
            // Without this the arithmetic saturates to `0-0`, which is one
            // byte of a file that has none.
            Ok(_) if size == 0 => Wanted::Unsatisfiable,
            Ok(0) => Wanted::Unsatisfiable,
            Ok(last) => Wanted::Part {
                start: size.saturating_sub(last),
                end: size - 1,
            },
            Err(_) => Wanted::Whole,
        },
        (first, "") => match first.parse::<u64>() {
            Ok(start) if start < size => Wanted::Part { start, end: size - 1 },
            Ok(_) => Wanted::Unsatisfiable,
            Err(_) => Wanted::Whole,
        },
        (first, last) => match (first.parse::<u64>(), last.parse::<u64>()) {
            (Ok(start), Ok(end)) if start < size && start <= end => Wanted::Part {
                start,
                // A client may ask past the end; it gets what exists.
                end: end.min(size - 1),
            },
            (Ok(_), Ok(_)) => Wanted::Unsatisfiable,
            _ => Wanted::Whole,
        },
    }
}

fn header_of(request: &Request, name: http::header::HeaderName) -> Option<&str> {
    request.headers().get(name).and_then(|value| value.to_str().ok())
}

/// Write the file, or the part of it that was asked for.
///
/// The body is streamed unless the file is small enough that reading it whole
/// costs less than the machinery to avoid doing so.
pub async fn deliver(
    response: &mut Response,
    path: &Path,
    size: u64,
    wanted: Wanted,
) -> Result<(), std::io::Error> {
    response.headers_mut().insert(
        http::header::ACCEPT_RANGES,
        http::HeaderValue::from_static("bytes"),
    );

    let (start, length) = match wanted {
        Wanted::Whole => {
            response.status_code(StatusCode::OK);
            (0, size)
        }
        Wanted::Unsatisfiable => {
            response.status_code(StatusCode::RANGE_NOT_SATISFIABLE);
            // The client is told how long the file actually is, so its next
            // request can be a sensible one.
            if let Ok(value) = http::HeaderValue::from_str(&format!("bytes */{size}")) {
                response.headers_mut().insert(http::header::CONTENT_RANGE, value);
            }
            return Ok(());
        }
        Wanted::Part { start, end } => {
            response.status_code(StatusCode::PARTIAL_CONTENT);
            if let Ok(value) = http::HeaderValue::from_str(&format!("bytes {start}-{end}/{size}")) {
                response.headers_mut().insert(http::header::CONTENT_RANGE, value);
            }
            (start, end - start + 1)
        }
    };

    if let Ok(value) = http::HeaderValue::from_str(&length.to_string()) {
        response.headers_mut().insert(http::header::CONTENT_LENGTH, value);
    }

    let mut file = tokio::fs::File::open(path).await?;
    if start > 0 {
        file.seek(std::io::SeekFrom::Start(start)).await?;
    }

    if length <= INLINE_LIMIT {
        let mut buffer = Vec::with_capacity(length as usize);
        file.take(length).read_to_end(&mut buffer).await?;
        response.body(ResBody::Once(buffer.into()));
        return Ok(());
    }

    // Streamed: the memory this costs is one buffer, whatever the file weighs.
    let stream = tokio_util::io::ReaderStream::new(file.take(length));
    let frames = futures_util::StreamExt::map(stream, |chunk| match chunk {
        Ok(bytes) => Ok(http_body::Frame::data(bytes)),
        Err(error) => Err(Box::new(error) as salvo_core::BoxedError),
    });
    response.body(ResBody::Boxed(Box::pin(http_body_util::StreamBody::new(frames))));
    Ok(())
}

#[cfg(test)]
mod tests {
    use http::header::HeaderValue;

    use super::*;

    fn request(headers: &[(&str, &str)]) -> Request {
        let mut request = Request::default();
        for (name, value) in headers {
            request.headers_mut().insert(
                http::header::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                HeaderValue::from_str(value).unwrap(),
            );
        }
        request
    }

    const TAG: &str = "\"abc-123\"";

    #[test]
    fn no_range_header_asks_for_the_whole_file() {
        assert_eq!(wanted(&request(&[]), 1000, TAG), Wanted::Whole);
    }

    #[test]
    fn a_closed_range_is_inclusive_at_both_ends() {
        assert_eq!(
            wanted(&request(&[("range", "bytes=0-499")]), 1000, TAG),
            Wanted::Part { start: 0, end: 499 }
        );
    }

    #[test]
    fn an_open_ended_range_runs_to_the_end_of_the_file() {
        // What a resumed download sends.
        assert_eq!(
            wanted(&request(&[("range", "bytes=500-")]), 1000, TAG),
            Wanted::Part { start: 500, end: 999 }
        );
    }

    #[test]
    fn a_suffix_range_counts_back_from_the_end() {
        assert_eq!(
            wanted(&request(&[("range", "bytes=-200")]), 1000, TAG),
            Wanted::Part { start: 800, end: 999 }
        );
    }

    #[test]
    fn a_suffix_longer_than_the_file_is_the_whole_file() {
        assert_eq!(
            wanted(&request(&[("range", "bytes=-5000")]), 1000, TAG),
            Wanted::Part { start: 0, end: 999 }
        );
    }

    #[test]
    fn asking_past_the_end_gets_what_exists() {
        assert_eq!(
            wanted(&request(&[("range", "bytes=900-5000")]), 1000, TAG),
            Wanted::Part { start: 900, end: 999 }
        );
    }

    #[test]
    fn a_start_past_the_end_has_nothing_to_send() {
        assert_eq!(
            wanted(&request(&[("range", "bytes=1000-")]), 1000, TAG),
            Wanted::Unsatisfiable
        );
        assert_eq!(
            wanted(&request(&[("range", "bytes=2000-3000")]), 1000, TAG),
            Wanted::Unsatisfiable
        );
    }

    #[test]
    fn a_backwards_range_is_not_one() {
        assert_eq!(
            wanted(&request(&[("range", "bytes=500-100")]), 1000, TAG),
            Wanted::Unsatisfiable
        );
    }

    #[test]
    fn several_ranges_at_once_get_the_whole_file() {
        // Legal, and much simpler than a multipart body nothing asks for.
        assert_eq!(
            wanted(&request(&[("range", "bytes=0-99,200-299")]), 1000, TAG),
            Wanted::Whole
        );
    }

    #[test]
    fn anything_malformed_gets_the_whole_file() {
        for spec in ["bytes=", "items=0-10", "bytes=abc-def", "nonsense"] {
            assert_eq!(
                wanted(&request(&[("range", spec)]), 1000, TAG),
                Wanted::Whole,
                "`{spec}` should have been ignored"
            );
        }
    }

    #[test]
    fn a_stale_if_range_asks_for_the_whole_file_again() {
        // Otherwise a resumed download would staple the second half of a new
        // file onto the first half of an old one.
        assert_eq!(
            wanted(
                &request(&[("range", "bytes=500-"), ("if-range", "\"different\"")]),
                1000,
                TAG
            ),
            Wanted::Whole
        );
    }

    #[test]
    fn a_matching_if_range_allows_the_range() {
        assert_eq!(
            wanted(&request(&[("range", "bytes=500-"), ("if-range", TAG)]), 1000, TAG),
            Wanted::Part { start: 500, end: 999 }
        );
    }

    async fn write(tag: &str, bytes: &[u8]) -> std::path::PathBuf {
        let directory = std::env::temp_dir().join(format!("gatel-ranges-{tag}"));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("file.bin");
        std::fs::write(&path, bytes).unwrap();
        path
    }

    async fn body_of(response: &mut Response) -> Vec<u8> {
        use http_body_util::BodyExt as _;
        match std::mem::take(response.body_mut()) {
            ResBody::Once(bytes) => bytes.to_vec(),
            ResBody::None => Vec::new(),
            other => other.collect().await.map(|got| got.to_bytes().to_vec()).unwrap(),
        }
    }

    #[tokio::test]
    async fn a_small_file_is_sent_whole() {
        let path = write("small", b"0123456789").await;
        let mut response = Response::new();
        deliver(&mut response, &path, 10, Wanted::Whole).await.unwrap();
        assert_eq!(response.status_code, Some(StatusCode::OK));
        assert_eq!(body_of(&mut response).await, b"0123456789");
        assert_eq!(
            response.headers().get(http::header::ACCEPT_RANGES).unwrap(),
            "bytes"
        );
        assert_eq!(
            response.headers().get(http::header::CONTENT_LENGTH).unwrap(),
            "10"
        );
    }

    #[tokio::test]
    async fn a_part_is_answered_with_206_and_where_it_came_from() {
        let path = write("part", b"0123456789").await;
        let mut response = Response::new();
        deliver(&mut response, &path, 10, Wanted::Part { start: 2, end: 5 })
            .await
            .unwrap();
        assert_eq!(response.status_code, Some(StatusCode::PARTIAL_CONTENT));
        assert_eq!(
            response.headers().get(http::header::CONTENT_RANGE).unwrap(),
            "bytes 2-5/10"
        );
        assert_eq!(response.headers().get(http::header::CONTENT_LENGTH).unwrap(), "4");
        assert_eq!(body_of(&mut response).await, b"2345");
    }

    #[tokio::test]
    async fn nothing_to_send_says_how_long_the_file_is() {
        let path = write("unsat", b"0123456789").await;
        let mut response = Response::new();
        deliver(&mut response, &path, 10, Wanted::Unsatisfiable)
            .await
            .unwrap();
        assert_eq!(response.status_code, Some(StatusCode::RANGE_NOT_SATISFIABLE));
        assert_eq!(
            response.headers().get(http::header::CONTENT_RANGE).unwrap(),
            "bytes */10"
        );
        assert!(body_of(&mut response).await.is_empty());
    }

    #[tokio::test]
    async fn a_large_file_is_streamed_rather_than_read_into_memory() {
        let size = (INLINE_LIMIT + 4096) as usize;
        let path = write("large", &vec![b'x'; size]).await;
        let mut response = Response::new();
        deliver(&mut response, &path, size as u64, Wanted::Whole)
            .await
            .unwrap();
        // A boxed body is the streaming one; `Once` would mean it was all read.
        let body = std::mem::take(response.body_mut());
        assert!(
            matches!(body, ResBody::Boxed(_)),
            "a file this size must not be held in memory"
        );
        response.body(body);
        assert_eq!(body_of(&mut response).await.len(), size);
    }

    #[tokio::test]
    async fn a_large_range_is_streamed_from_where_it_starts() {
        let size = (INLINE_LIMIT + 4096) as usize;
        let mut bytes = vec![b'a'; size];
        bytes[INLINE_LIMIT as usize] = b'Z';
        let path = write("large-part", &bytes).await;

        let mut response = Response::new();
        let start = INLINE_LIMIT;
        deliver(
            &mut response,
            &path,
            size as u64,
            Wanted::Part {
                start,
                end: size as u64 - 1,
            },
        )
        .await
        .unwrap();
        let body = body_of(&mut response).await;
        assert_eq!(body.len(), 4096);
        assert_eq!(body[0], b'Z', "it started where it was asked to");
    }

    #[test]
    fn a_suffix_of_an_empty_file_is_unsatisfiable() {
        // The arithmetic saturates to `0-0` without a size check, which is one
        // byte of a file that has none — and then a read past the end.
        for spec in ["bytes=-1", "bytes=-500", "bytes=-18446744073709551615"] {
            assert_eq!(
                wanted(&request(&[("range", spec)]), 0, TAG),
                Wanted::Unsatisfiable,
                "{spec}"
            );
        }
    }

    #[test]
    fn an_if_range_that_cannot_be_read_counts_as_not_matching() {
        // Present but unreadable is not the same as absent. Reading it as
        // absent turns "only if it is still the same file" into "give me a
        // part", which is the one direction that corrupts a resumed download.
        let mut request = Request::default();
        request
            .headers_mut()
            .insert(http::header::RANGE, HeaderValue::from_static("bytes=0-9"));
        request.headers_mut().insert(
            http::header::IF_RANGE,
            HeaderValue::from_bytes(&[0xff, 0xfe]).unwrap(),
        );
        assert_eq!(wanted(&request, 1000, TAG), Wanted::Whole);
    }
}
