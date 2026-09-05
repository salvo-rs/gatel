//! What `retry` actually does.
//!
//! Every field in the `retry` block used to be decoded, validated, hashed into
//! the plan and printed by `explain` — and only `attempts` reached the request
//! path. A knob that does nothing is worse than one that was never offered,
//! because an operator who sets it stops looking for the real problem.
//!
//! So each of these is about one field doing what it says, and the order they
//! are written in is the order somebody would ask about them: what is tried
//! again, how long each try gets, what may be sent twice, and how many of them
//! may be happening at once.

mod support;

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use support::{Gateway, free_port, request};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// A backend that reads the request and then closes without answering.
///
/// A post-send failure: the request is on the wire and the gateway cannot know
/// whether it was acted on.
async fn hangs_up(seen: Arc<AtomicU64>) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            let seen = Arc::clone(&seen);
            tokio::spawn(async move {
                let mut buffer = [0u8; 8192];
                if stream.read(&mut buffer).await.is_ok() {
                    seen.fetch_add(1, Ordering::Relaxed);
                }
                drop(stream);
            });
        }
    });
    address
}

/// A backend that reads the request and then says nothing at all, forever.
async fn never_answers(seen: Arc<AtomicU64>) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let mut held = Vec::new();
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            let mut buffer = [0u8; 8192];
            let _ = stream.read(&mut buffer).await;
            seen.fetch_add(1, Ordering::Relaxed);
            // Kept alive deliberately: closing would be an answer of a kind.
            held.push(stream);
        }
    });
    address
}

/// A backend that answers with how many body bytes it was given.
async fn counts_the_body(seen: Arc<AtomicU64>) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            let seen = Arc::clone(&seen);
            tokio::spawn(async move {
                let mut buffer = Vec::new();
                let mut chunk = [0u8; 4096];
                loop {
                    let Ok(read) = stream.read(&mut chunk).await else {
                        return;
                    };
                    if read == 0 {
                        return;
                    }
                    buffer.extend_from_slice(&chunk[..read]);
                    let text = String::from_utf8_lossy(&buffer).into_owned();
                    let Some((head, body)) = text.split_once("\r\n\r\n") else {
                        continue;
                    };
                    let declared: usize = head
                        .lines()
                        .find(|line| line.to_ascii_lowercase().starts_with("content-length:"))
                        .and_then(|line| line.split(':').nth(1))
                        .and_then(|value| value.trim().parse().ok())
                        .unwrap_or(0);
                    if body.len() < declared {
                        continue;
                    }
                    seen.fetch_add(1, Ordering::Relaxed);
                    let answer = format!("received {} bytes", body.len());
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{answer}",
                        answer.len()
                    );
                    let _ = stream.write_all(response.as_bytes()).await;
                    buffer.clear();
                }
            });
        }
    });
    address
}

/// A gateway whose upstream tries `bad` first and `good` second.
///
/// `balance "first"` rather than round robin, so the failing endpoint is always
/// the one picked to begin with and the test is about retrying rather than
/// about which endpoint happened to come up.
async fn gateway(tag: &str, bad: SocketAddr, good: SocketAddr, retry: &str) -> Gateway {
    let gateway = Gateway::new(tag);
    let port = free_port();
    let outcome = gateway
        .apply(&format!(
            r#"
global {{ http "127.0.0.1:{port}" }}
upstream "api" {{
    endpoint "{bad}" id="bad" priority=0
    endpoint "{good}" id="good" priority=1
    balance "first"
    {retry}
    timeouts connect="1s" queue="1s" response-head="10s" overall="20s"
}}
site "*" {{ route "/*" {{ proxy upstream="api" }} }}
"#
        ))
        .await;
    assert!(
        matches!(outcome, gatel_plan::Outcome::Started { .. }),
        "{outcome:?}"
    );
    gateway
}

fn post(body: &str) -> String {
    format!(
        "POST /upload HTTP/1.1\r\nHost: example.com\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n{body}",
        body.len()
    )
}

#[tokio::test]
async fn a_request_with_a_body_still_gets_another_endpoint_when_the_first_cannot_be_reached() {
    // Nothing was sent, so the backend cannot have acted on it — which is true
    // whatever the method is. This used to be refused for any request carrying
    // a body, which meant a POST got no benefit from a second backend at all.
    let answered = Arc::new(AtomicU64::new(0));
    let good = counts_the_body(Arc::clone(&answered)).await;
    let dead: SocketAddr = format!("127.0.0.1:{}", free_port()).parse().unwrap();
    let gateway = gateway("retry-post", dead, good, r#"retry attempts=2"#).await;

    let response = request(gateway.address(), &post("hello world")).await;

    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    assert!(response.contains("received 11 bytes"), "{response}");
    assert_eq!(answered.load(Ordering::Relaxed), 1);
    gateway.stop().await;
}

#[tokio::test]
async fn safe_does_not_send_a_request_the_backend_may_already_have_seen() {
    // The first backend read the request and then died. Under `safe` that is
    // the end of it: the gateway cannot know whether the write went through,
    // and guessing wrong duplicates whatever the request did.
    let saw = Arc::new(AtomicU64::new(0));
    let answered = Arc::new(AtomicU64::new(0));
    let bad = hangs_up(Arc::clone(&saw)).await;
    let good = counts_the_body(Arc::clone(&answered)).await;
    let gateway = gateway("retry-safe", bad, good, r#"retry mode="safe" attempts=3"#).await;

    let response = request(gateway.address(), &post("hello")).await;

    assert!(response.starts_with("HTTP/1.1 502"), "{response}");
    assert_eq!(saw.load(Ordering::Relaxed), 1);
    assert_eq!(
        answered.load(Ordering::Relaxed),
        0,
        "the second backend must never have been told about it"
    );
    gateway.stop().await;
}

#[tokio::test]
async fn replay_sends_the_buffered_body_again_and_it_arrives_whole() {
    let saw = Arc::new(AtomicU64::new(0));
    let answered = Arc::new(AtomicU64::new(0));
    let bad = hangs_up(Arc::clone(&saw)).await;
    let good = counts_the_body(Arc::clone(&answered)).await;
    let gateway = gateway(
        "retry-replay",
        bad,
        good,
        r#"retry mode="replay" attempts=3 buffer="64KiB""#,
    )
    .await;

    let response = request(gateway.address(), &post("hello world")).await;

    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    // Not "received 0 bytes": a replayed request that arrived without its body
    // would look like a success and corrupt whatever it was sent to.
    assert!(response.contains("received 11 bytes"), "{response}");
    assert_eq!(saw.load(Ordering::Relaxed), 1);
    assert_eq!(answered.load(Ordering::Relaxed), 1);
    gateway.stop().await;
}

#[tokio::test]
async fn a_body_larger_than_the_buffer_is_not_replayed() {
    // The operator said how much memory a replay may cost. A body over that is
    // streamed straight through, which means it cannot be sent twice — and
    // saying so with a 502 beats quietly sending half of it somewhere else.
    let saw = Arc::new(AtomicU64::new(0));
    let answered = Arc::new(AtomicU64::new(0));
    let bad = hangs_up(Arc::clone(&saw)).await;
    let good = counts_the_body(Arc::clone(&answered)).await;
    let gateway = gateway(
        "retry-toobig",
        bad,
        good,
        r#"retry mode="replay" attempts=3 buffer="8B""#,
    )
    .await;

    let response = request(gateway.address(), &post("this is longer than eight bytes")).await;

    assert!(response.starts_with("HTTP/1.1 502"), "{response}");
    assert_eq!(answered.load(Ordering::Relaxed), 0);
    gateway.stop().await;
}

#[tokio::test]
async fn per_try_gives_up_on_one_attempt_without_giving_up_on_the_request() {
    // `response-head` is ten seconds and `overall` is twenty. Without `per-try`
    // the first attempt would spend the whole head budget and this would take
    // ten seconds; with it, the attempt is abandoned after half a second and
    // the second backend answers.
    let saw = Arc::new(AtomicU64::new(0));
    let answered = Arc::new(AtomicU64::new(0));
    let bad = never_answers(Arc::clone(&saw)).await;
    let good = counts_the_body(Arc::clone(&answered)).await;
    let gateway = gateway(
        "retry-pertry",
        bad,
        good,
        r#"retry mode="replay" attempts=2 per-try="500ms" buffer="64KiB""#,
    )
    .await;

    let started = std::time::Instant::now();
    let response = request(gateway.address(), &post("hello")).await;
    let elapsed = started.elapsed();

    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    assert_eq!(answered.load(Ordering::Relaxed), 1);
    assert!(
        elapsed < Duration::from_secs(5),
        "the per-try budget was not applied: {elapsed:?}"
    );
    gateway.stop().await;
}

#[tokio::test]
async fn a_budget_of_nothing_means_no_retries_at_all() {
    // The one setting of `budget-percent` whose effect is visible from a single
    // request. The proportional arithmetic itself is unit-tested where it
    // lives, because reproducing a retry storm from out here would be a race.
    let answered = Arc::new(AtomicU64::new(0));
    let good = counts_the_body(Arc::clone(&answered)).await;
    let dead: SocketAddr = format!("127.0.0.1:{}", free_port()).parse().unwrap();
    let gateway = gateway(
        "retry-nobudget",
        dead,
        good,
        r#"retry attempts=3 budget-percent=0"#,
    )
    .await;

    let response = request(gateway.address(), &post("hello")).await;

    assert!(response.starts_with("HTTP/1.1 502"), "{response}");
    assert_eq!(
        answered.load(Ordering::Relaxed),
        0,
        "with no budget the second endpoint should never be tried"
    );
    gateway.stop().await;
}

#[tokio::test]
async fn a_backend_that_stops_mid_body_does_not_hold_the_request_open() {
    // `response-head` bounds how long an answer may take to start. `body-idle`
    // bounds how long it may take to continue, and until it was applied a
    // backend could send one byte and then hold a connection, a pool slot and
    // an in-flight count until `overall` — which is minutes, because it has to
    // cover a download that is legitimately long.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let stalls = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let mut held = Vec::new();
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            let mut buffer = [0u8; 8192];
            let _ = stream.read(&mut buffer).await;
            // A complete head promising ten bytes, then one of them, then
            // nothing at all.
            let _ = stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\nX")
                .await;
            held.push(stream);
        }
    });

    let gateway = Gateway::new("body-idle");
    let port = free_port();
    let outcome = gateway
        .apply(&format!(
            r#"
global {{ http "127.0.0.1:{port}" }}
upstream "api" {{
    endpoint "{stalls}"
    timeouts connect="1s" response-head="5s" body-idle="500ms" overall="60s"
}}
site "*" {{ route "/*" {{ proxy upstream="api" }} }}
"#
        ))
        .await;
    assert!(
        matches!(outcome, gatel_plan::Outcome::Started { .. }),
        "{outcome:?}"
    );

    let started = std::time::Instant::now();
    let response = support::get(gateway.address(), "/slow").await;
    let elapsed = started.elapsed();

    // The head was already sent, so there is no status left to change: the
    // body is simply cut short.
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    let body = response
        .split_once(
            "

",
        )
        .map(|(_, body)| body)
        .unwrap_or("");
    assert!(
        body.len() < 10,
        "the body should have been cut short, not completed: {body:?}"
    );
    assert!(
        elapsed < Duration::from_secs(2),
        "the body-idle gap was not applied: {elapsed:?}"
    );
    gateway.stop().await;
}

#[tokio::test]
async fn without_a_retry_block_nothing_is_tried_twice() {
    // The default has to stay the conservative one: a gateway that retried
    // without being asked would duplicate requests for every operator who never
    // read this far.
    let answered = Arc::new(AtomicU64::new(0));
    let good = counts_the_body(Arc::clone(&answered)).await;
    let dead: SocketAddr = format!("127.0.0.1:{}", free_port()).parse().unwrap();
    let gateway = gateway("retry-none", dead, good, "").await;

    let response = request(gateway.address(), &post("hello")).await;

    assert!(response.starts_with("HTTP/1.1 502"), "{response}");
    assert_eq!(answered.load(Ordering::Relaxed), 0);
    gateway.stop().await;
}
