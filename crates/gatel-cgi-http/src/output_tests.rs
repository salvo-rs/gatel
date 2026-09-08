use super::*;

fn sink() -> (Sink, mpsc::Receiver<Message>, Completion) {
    let (sender, receiver) = mpsc::channel(1);
    let completion = Arc::new(std::sync::atomic::AtomicU8::new(EXPECT_HEAD));
    (
        Sink {
            sender,
            completion: completion.clone(),
        },
        receiver,
        completion,
    )
}
fn head() -> Head {
    Head {
        status: StatusCode::OK,
        headers: http::HeaderMap::new(),
        content_length: Some(0),
    }
}

#[tokio::test]
async fn panicking_exchange_is_failure_even_after_finish() {
    struct Dropped(Arc<std::sync::atomic::AtomicBool>);
    impl Drop for Dropped {
        fn drop(&mut self) {
            self.0.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }
    for completed in [false, true] {
        let (mut sink, _receiver, completion) = sink();
        let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let guard = Dropped(dropped.clone());
        let transfer: Running = Box::pin(async move {
            let _guard = guard;
            if completed {
                sink.head(head()).await.unwrap();
                sink.finish().await.unwrap();
            }
            panic!("plugin panic text must not become a response");
        });
        assert!(matches!(
            checked_transfer(transfer, completion).await,
            Err(ExchangeError::Failed)
        ));
        assert!(dropped.load(std::sync::atomic::Ordering::SeqCst));
    }
}

#[tokio::test]
async fn large_callback_is_split_under_single_message_backpressure() {
    let (mut sink, mut receiver, completion) = sink();
    sink.head(head()).await.unwrap();
    receiver.recv().await.unwrap();
    let body: Vec<u8> = (0..OUTPUT_CHUNK * 5 + 17).map(|i| (i % 251) as u8).collect();
    let transfer = async {
        sink.body(&body).await.unwrap();
        sink.finish().await.unwrap();
        drop(sink);
    };
    let reading = async {
        let mut received = Vec::new();
        let mut chunks = 0;
        while let Some(message) = receiver.recv().await {
            let Message::Body(bytes) = message else {
                panic!("unexpected head")
            };
            assert!(bytes.len() <= OUTPUT_CHUNK);
            received.extend(bytes);
            chunks += 1;
        }
        assert_eq!(chunks, 6);
        received
    };
    let (_, received) = tokio::join!(transfer, reading);
    assert_eq!(received, body);
    checked_transfer(Box::pin(async { Ok(()) }), completion)
        .await
        .unwrap();
}

#[tokio::test]
async fn cancelled_large_callback_keeps_only_one_queued_chunk_and_fails() {
    use std::future::Future as _;
    let (mut sink, mut receiver, completion) = sink();
    sink.head(head()).await.unwrap();
    receiver.recv().await.unwrap();
    let bytes = vec![b'x'; OUTPUT_CHUNK * 8];
    let mut body = Box::pin(sink.body(&bytes));
    let mut context = Context::from_waker(std::task::Waker::noop());
    assert!(body.as_mut().poll(&mut context).is_pending());
    drop(body);
    let Message::Body(chunk) = receiver.try_recv().unwrap() else {
        panic!("expected body")
    };
    assert_eq!(chunk.len(), OUTPUT_CHUNK);
    assert!(receiver.try_recv().is_err());
    assert!(sink.finish().await.is_err());
    assert!(
        checked_transfer(Box::pin(async { Ok(()) }), completion)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn exchange_success_requires_explicit_finish() {
    for finish in [false, true] {
        let (mut sink, _receiver, completion) = sink();
        let transfer: Running = Box::pin(async move {
            sink.head(head()).await.unwrap();
            if finish {
                sink.finish().await.unwrap();
            }
            Ok(())
        });
        assert_eq!(checked_transfer(transfer, completion).await.is_ok(), finish);
    }
}

#[tokio::test]
async fn invalid_output_order_poisoning_cannot_be_ignored() {
    for misuse in 0..4 {
        let (mut sink, mut receiver, completion) = sink();
        if misuse == 0 {
            assert!(sink.body(b"premature").await.is_err());
        } else {
            sink.head(head()).await.unwrap();
            receiver.recv().await.unwrap();
            if misuse == 1 {
                assert!(sink.head(head()).await.is_err());
            } else {
                sink.finish().await.unwrap();
                if misuse == 2 {
                    assert!(sink.finish().await.is_err());
                } else {
                    assert!(sink.body(b"late").await.is_err());
                }
            }
        }
        assert!(
            checked_transfer(Box::pin(async { Ok(()) }), completion)
                .await
                .is_err()
        );
    }
}

#[tokio::test]
async fn cancelled_output_callback_cannot_resume_as_success() {
    use std::future::Future as _;
    let (mut sink, _receiver, completion) = sink();
    sink.head(head()).await.unwrap(); // Full channel: body callback will block.
    let mut body = Box::pin(sink.body(b"pending"));
    let mut context = Context::from_waker(std::task::Waker::noop());
    assert!(body.as_mut().poll(&mut context).is_pending());
    drop(body);
    assert!(sink.finish().await.is_err());
    assert!(
        checked_transfer(Box::pin(async { Ok(()) }), completion)
            .await
            .is_err()
    );
}
