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
use tokio::io::AsyncSeekExt;

/// Files up to this size are read in one go. Bigger ones are streamed.
///
/// Small enough that a page of them is not a memory problem, large enough that
/// the ordinary web asset — a stylesheet, a script, an icon — is still one read
/// and one buffer.
const INLINE_LIMIT: u64 = 64 * 1024;

/// How much is read from disk at a time once a file is streamed.
///
/// `ReaderStream`'s own default is 4 KiB, which turns a 10 MiB file into two
/// and a half thousand frames — a read, an allocation and a write apiece. The
/// cost of that is not subtle: it was the difference between finishing a large
/// download and not finishing one at all while other clients waited.
///
/// 64 KiB rather than something larger because this is per in-flight response,
/// and a gateway serving a thousand of them should not be holding sixty
/// megabytes of read buffer. It is deliberately the same number as
/// [`INLINE_LIMIT`]: below that a file is read whole, above it the file is read
/// in pieces of exactly that size.
const CHUNK: usize = INLINE_LIMIT as usize;

/// One opened representation owns both the metadata and the eventual reader.
/// A path replacement after opening cannot select another file for its body.
pub struct OpenedFile {
    file: tokio::fs::File,
    metadata: std::fs::Metadata,
}

impl OpenedFile {
    pub async fn open(path: &Path) -> Result<Self, std::io::Error> {
        let file = tokio::fs::File::open(path).await?;
        let metadata = file.metadata().await?;
        if !metadata.is_file() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "not a regular file",
            ));
        }
        Ok(Self { file, metadata })
    }

    pub fn metadata(&self) -> &std::fs::Metadata {
        &self.metadata
    }
}

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
    if request.headers().contains_key(http::header::IF_RANGE) {
        let Some(condition) = header_of(request, http::header::IF_RANGE) else {
            return Wanted::Whole;
        };
        if !crate::preconditions::strong_equal(condition.trim().as_bytes(), etag.as_bytes()) {
            return Wanted::Whole;
        }
    }

    let Some((unit, spec)) = asked.trim().split_once('=') else {
        return Wanted::Whole;
    };
    if !unit.eq_ignore_ascii_case("bytes") || spec.contains(',') {
        return Wanted::Whole;
    }
    let Some((from, to)) = spec.split_once('-') else {
        return Wanted::Whole;
    };

    match (from, to) {
        // `bytes=-500`: the last 500 bytes.
        ("", last) => match decimal(last) {
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
        (first, "") => match decimal(first) {
            Ok(start) if start < size => Wanted::Part { start, end: size - 1 },
            Ok(_) => Wanted::Unsatisfiable,
            Err(_) => Wanted::Whole,
        },
        (first, last) => match (decimal(first), decimal(last)) {
            (Ok(start), Ok(end)) if start > end => Wanted::Whole,
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
    let mut values = request.headers().get_all(name).iter();
    let value = values.next()?.to_str().ok()?;
    values.next().is_none().then_some(value)
}

// Saturating decimal arithmetic keeps valid arbitrarily large range positions
// meaningful (large end/suffix clamps, large start is unsatisfiable). Rust's
// numeric parser additionally accepts '+' and rejects overflow, neither of
// which is the range grammar/semantics needed here.
fn decimal(value: &str) -> Result<u64, ()> {
    if value.is_empty() {
        return Err(());
    }
    value.bytes().try_fold(0u64, |number, digit| {
        if !digit.is_ascii_digit() {
            return Err(());
        }
        Ok(number.saturating_mul(10).saturating_add(u64::from(digit - b'0')))
    })
}

/// Write the file, or the part of it that was asked for.
///
/// The body is streamed unless the file is small enough that reading it whole
/// costs less than the machinery to avoid doing so.
pub async fn deliver(
    response: &mut Response,
    opened: OpenedFile,
    wanted: Wanted,
) -> Result<(), std::io::Error> {
    deliver_with_backpressure(response, opened, wanted, None).await
}

/// The host enables direct streamed frames only after configuring bounded
/// downstream TCP output. Other adapters keep the original reader.
pub(crate) async fn deliver_with_backpressure(
    response: &mut Response,
    opened: OpenedFile,
    wanted: Wanted,
    bounded_output: Option<&gatel_plan::BoundedTcpOutput>,
) -> Result<(), std::io::Error> {
    let size = opened.metadata.len();
    if let Wanted::Part { start, end } = wanted
        && (start > end || end >= size)
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "range outside selected file",
        ));
    }
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

    let mut file = opened.file;
    if length <= INLINE_LIMIT {
        // The idle Tokio file transfers its original handle without reopening
        // the path. Read directly into the outgoing buffer in one worker.
        let mut file = file.into_std().await;
        if length < crate::file_body::POOLED_MIN as u64 {
            let buffer = tokio::task::spawn_blocking(move || {
                if start != 0 {
                    std::io::Seek::seek(&mut file, std::io::SeekFrom::Start(start))?;
                }
                crate::file_body::read_exact_prefix(file, length as usize)
            })
            .await
            .map_err(std::io::Error::other)??;
            response.body(ResBody::Once(buffer.into()));
        } else {
            let buffer = tokio::task::spawn_blocking(move || {
                if start != 0 {
                    std::io::Seek::seek(&mut file, std::io::SeekFrom::Start(start))?;
                }
                crate::file_body::read_bytes(file, length as usize)
            })
            .await
            .map_err(std::io::Error::other)??;
            response.body(ResBody::Once(buffer));
        }
        return Ok(());
    }
    if length >= 1024 * 1024
        && let Some(output) = bounded_output
    {
        output.enable();
        response.body(crate::file_body::stream(file.into_std().await, start, length));
        return Ok(());
    }
    if start != 0 {
        file.seek(std::io::SeekFrom::Start(start)).await?;
    }
    let stream =
        tokio_util::io::ReaderStream::with_capacity(crate::file_body::ExactReader::new(file, length), CHUNK);
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

    async fn deliver(
        response: &mut Response,
        path: &Path,
        size: u64,
        wanted: Wanted,
    ) -> Result<(), std::io::Error> {
        let opened = OpenedFile::open(path).await?;
        assert_eq!(opened.metadata().len(), size);
        super::deliver(response, opened, wanted).await
    }

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

    const LARGE_FILE: u64 = 1024 * 1024;

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
            Wanted::Whole
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
    fn duplicate_range_fields_and_weak_if_range_do_not_select_a_fragment() {
        let mut request = request(&[("range", "bytes=0-9"), ("if-range", TAG)]);
        request
            .headers_mut()
            .append(http::header::IF_RANGE, HeaderValue::from_static(TAG));
        assert_eq!(wanted(&request, 100, TAG), Wanted::Whole);
        request.headers_mut().remove(http::header::IF_RANGE);
        request
            .headers_mut()
            .append(http::header::RANGE, HeaderValue::from_static("bytes=20-29"));
        assert_eq!(wanted(&request, 100, TAG), Wanted::Whole);
        request
            .headers_mut()
            .insert(http::header::RANGE, HeaderValue::from_static("bytes=0-9"));
        request
            .headers_mut()
            .insert(http::header::IF_RANGE, HeaderValue::from_static("W/\"abc-123\""));
        assert_eq!(wanted(&request, 100, "W/\"abc-123\""), Wanted::Whole);
        assert_eq!(wanted(&request, 100, TAG), Wanted::Whole);
    }

    #[test]
    fn decimal_range_grammar_and_large_positions_are_handled_without_overflow() {
        for spec in [
            "bytes=+1-3",
            "bytes=1-+3",
            "bytes=-+3",
            "bytes=1 -3",
            "bytes=1- 3",
            "bytes= 1-3",
        ] {
            assert_eq!(
                wanted(&request(&[("range", spec)]), 10, TAG),
                Wanted::Whole,
                "{spec}"
            );
        }
        for spec in [
            "bytes=0-184467440737095516160",
            "bytes=-184467440737095516160",
            "BYTES=0-9",
        ] {
            assert_eq!(
                wanted(&request(&[("range", spec)]), 10, TAG),
                Wanted::Part { start: 0, end: 9 },
                "{spec}"
            );
        }
        assert_eq!(
            wanted(&request(&[("range", "bytes=184467440737095516160-")]), 10, TAG),
            Wanted::Unsatisfiable
        );
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
        let directory = std::env::temp_dir().join(format!(
            "gatel-ranges-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
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
    async fn range_delivery_preserves_bytes_at_inline_boundary_and_nonzero_offsets() {
        let original: Vec<u8> = (0..(1024 * 1024 + 70 * 1024)).map(|n| (n % 251) as u8).collect();
        let path = write("boundary-offset", &original).await;
        for start in [0, 1024 * 1024 + 13] {
            for length in [
                32 * 1024 - 1,
                32 * 1024,
                32 * 1024 + 1,
                INLINE_LIMIT - 1,
                INLINE_LIMIT,
                INLINE_LIMIT + 1,
                INLINE_LIMIT + 1024,
            ] {
                let wanted = Wanted::Part {
                    start,
                    end: start + length - 1,
                };
                let mut response = Response::new();
                deliver(&mut response, &path, original.len() as u64, wanted)
                    .await
                    .unwrap();
                assert_eq!(response.status_code, Some(StatusCode::PARTIAL_CONTENT));
                assert_eq!(
                    response.headers()[http::header::CONTENT_LENGTH],
                    length.to_string()
                );
                assert_eq!(
                    matches!(response.body_mut(), ResBody::Once(_)),
                    length <= INLINE_LIMIT
                );
                assert_eq!(
                    body_of(&mut response).await,
                    original[start as usize..(start + length) as usize]
                );
            }
        }
    }

    #[tokio::test]
    async fn large_range_lengths_and_offsets_preserve_frames_and_bytes() {
        use http_body_util::BodyExt as _;
        let original: Vec<_> = (0..LARGE_FILE as usize + 32).map(|n| (n % 251) as u8).collect();
        let path = write("direct-read-boundary", &original).await;
        for enabled in [false, true] {
            let output = enabled.then(gatel_plan::BoundedTcpOutput::default);
            for start in [0, 13] {
                for length in [LARGE_FILE - 1, LARGE_FILE, LARGE_FILE + 1] {
                    if let Some(output) = &output {
                        output.reset();
                    }
                    let mut response = Response::new();
                    super::deliver_with_backpressure(
                        &mut response,
                        OpenedFile::open(&path).await.unwrap(),
                        Wanted::Part {
                            start,
                            end: start + length - 1,
                        },
                        output.as_ref(),
                    )
                    .await
                    .unwrap();
                    if let Some(output) = &output {
                        assert_eq!(output.enabled(), length >= LARGE_FILE);
                    }
                    let mut body = std::mem::take(response.body_mut());
                    let mut received = Vec::new();
                    while let Some(frame) = body.frame().await {
                        let bytes = frame.unwrap().into_data().unwrap();
                        assert!(bytes.len() <= CHUNK);
                        received.extend_from_slice(&bytes);
                    }
                    assert_eq!(received, original[start as usize..(start + length) as usize]);
                }
            }
        }
    }

    #[tokio::test]
    async fn large_ranges_preserve_offsets_and_bound_outgoing_frames() {
        use http_body_util::BodyExt as _;
        let original: Vec<_> = (0..LARGE_FILE as usize * 2)
            .map(|offset| (offset % 251) as u8)
            .collect();
        let path = write("buffered-range", &original).await;
        let start = 17;
        let length = LARGE_FILE as usize + 123;
        let mut response = Response::new();
        deliver(
            &mut response,
            &path,
            original.len() as u64,
            Wanted::Part {
                start: start as u64,
                end: (start + length - 1) as u64,
            },
        )
        .await
        .unwrap();
        let mut body = std::mem::take(response.body_mut());
        let mut received = Vec::new();
        while let Some(frame) = body.frame().await {
            let bytes = frame.unwrap().into_data().unwrap();
            assert!(bytes.len() <= CHUNK);
            received.extend_from_slice(&bytes);
        }
        assert_eq!(received, original[start..start + length]);
    }

    #[tokio::test]
    async fn truncation_after_a_streamed_frame_still_reports_early_eof() {
        use http_body_util::BodyExt as _;
        let size = LARGE_FILE as usize + 1;
        let path = write("truncate-buffered", &vec![b'x'; size]).await;
        let mut response = Response::new();
        deliver(&mut response, &path, size as u64, Wanted::Whole)
            .await
            .unwrap();
        let mut body = std::mem::take(response.body_mut());
        let first = body.frame().await.unwrap().unwrap().into_data().unwrap();
        let mut received = first.len();
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(0)
            .unwrap();
        while let Ok(frame) = body.frame().await.expect("a truncated body must report an error") {
            received += frame.into_data().unwrap().len();
        }
        // Already-read bytes may be forwarded, but read-ahead stays bounded.
        assert!(received <= CHUNK);
    }

    #[tokio::test]
    async fn inline_read_does_not_include_bytes_appended_after_metadata() {
        let path = write("inline-growth", b"original").await;
        let opened = OpenedFile::open(&path).await.unwrap();
        use std::io::Write;
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"new tail")
            .unwrap();
        let mut response = Response::new();
        super::deliver(&mut response, opened, Wanted::Whole)
            .await
            .unwrap();
        assert_eq!(response.headers()[http::header::CONTENT_LENGTH], "8");
        assert_eq!(body_of(&mut response).await, b"original");
    }

    #[tokio::test]
    async fn truncated_offset_ranges_never_publish_an_incomplete_inline_body() {
        for length in [32 * 1024 - 1, 32 * 1024, INLINE_LIMIT] {
            let start = 1024 * 1024 + 13;
            let path = write("truncate-offset", &vec![b'x'; (start + length) as usize]).await;
            let opened = OpenedFile::open(&path).await.unwrap();
            std::fs::OpenOptions::new()
                .write(true)
                .open(&path)
                .unwrap()
                .set_len(start + length - 1)
                .unwrap();
            let mut response = Response::new();
            let error = super::deliver(
                &mut response,
                opened,
                Wanted::Part {
                    start,
                    end: start + length - 1,
                },
            )
            .await
            .unwrap_err();
            assert_eq!(error.kind(), std::io::ErrorKind::UnexpectedEof);
            assert!(matches!(response.body_mut(), ResBody::None));
        }
    }

    #[tokio::test]
    async fn empty_file_delivers_an_empty_inline_body() {
        let path = write("empty-inline", b"").await;
        let mut response = Response::new();
        deliver(&mut response, &path, 0, Wanted::Whole).await.unwrap();
        assert_eq!(response.headers()[http::header::CONTENT_LENGTH], "0");
        assert!(matches!(response.body_mut(), ResBody::Once(_)));
        assert!(body_of(&mut response).await.is_empty());
    }

    #[tokio::test]
    async fn pathname_replacement_cannot_change_an_opened_representation() {
        for size in [
            32,
            INLINE_LIMIT as usize,
            INLINE_LIMIT as usize + 4096,
            LARGE_FILE as usize + 4096,
        ] {
            let original = vec![b'a'; size];
            let path = write("replace-opened", &original).await;
            let opened = OpenedFile::open(&path).await.unwrap();
            assert_eq!(opened.metadata().len(), size as u64);
            // Replacing the name is the usual deployment pattern. The open
            // reader must keep the old file, including on the streaming path.
            std::fs::rename(&path, path.with_extension("old")).unwrap();
            std::fs::write(&path, b"replacement").unwrap();
            let mut response = Response::new();
            super::deliver(&mut response, opened, Wanted::Whole)
                .await
                .unwrap();
            assert_eq!(body_of(&mut response).await, original);
            assert_eq!(response.headers()[http::header::CONTENT_LENGTH], size.to_string());
        }
    }

    #[tokio::test]
    async fn truncation_is_an_error_for_inline_and_streaming_bodies() {
        use http_body_util::BodyExt;
        for size in [
            32,
            INLINE_LIMIT as usize,
            INLINE_LIMIT as usize + 4096,
            LARGE_FILE as usize + 4096,
        ] {
            let path = write("truncate-opened", &vec![b'a'; size]).await;
            let opened = OpenedFile::open(&path).await.unwrap();
            std::fs::OpenOptions::new()
                .write(true)
                .open(&path)
                .unwrap()
                .set_len(3)
                .unwrap();
            let mut response = Response::new();
            let result = super::deliver(&mut response, opened, Wanted::Whole).await;
            if size <= INLINE_LIMIT as usize {
                assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::UnexpectedEof);
            } else {
                result.unwrap();
                assert!(std::mem::take(response.body_mut()).collect().await.is_err());
            }
        }
    }

    #[tokio::test]
    async fn opened_range_uses_original_handle_and_rejects_invalid_bounds() {
        let path = write("range-opened", b"0123456789").await;
        let opened = OpenedFile::open(&path).await.unwrap();
        std::fs::rename(&path, path.with_extension("old")).unwrap();
        std::fs::write(&path, b"new").unwrap();
        let mut response = Response::new();
        super::deliver(&mut response, opened, Wanted::Part { start: 2, end: 5 })
            .await
            .unwrap();
        assert_eq!(body_of(&mut response).await, b"2345");
        let opened = OpenedFile::open(&path).await.unwrap();
        assert_eq!(
            super::deliver(
                &mut response,
                opened,
                Wanted::Part {
                    start: 2,
                    end: u64::MAX
                }
            )
            .await
            .unwrap_err()
            .kind(),
            std::io::ErrorKind::InvalidInput
        );
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
