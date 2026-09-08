//! Bounded adaptation for protocols that require an exact request length.
use tokio::io::{AsyncRead, AsyncReadExt};

pub(crate) enum Error {
    TooLarge,
    Read,
    Timeout,
}

pub(crate) async fn buffer(
    reader: &mut (impl AsyncRead + Unpin),
    cap: u64,
    idle: std::time::Duration,
) -> Result<Vec<u8>, Error> {
    let mut body = Vec::new();
    let mut chunk = [0; 8192];
    loop {
        // Read at most one byte beyond the cap, including for a zero-byte cap.
        let available = (cap - body.len() as u64)
            .saturating_add(1)
            .min(chunk.len() as u64) as usize;
        let count = tokio::time::timeout(idle, reader.read(&mut chunk[..available]))
            .await
            .map_err(|_| Error::Timeout)?
            .map_err(|error| {
                if error
                    .get_ref()
                    .is_some_and(|cause| gatel_plugin_runtime::BodyTooLarge::find(cause).is_some())
                {
                    Error::TooLarge
                } else {
                    Error::Read
                }
            })?;
        if count == 0 {
            return Ok(body);
        }
        if count as u64 > cap - body.len() as u64 {
            return Err(Error::TooLarge);
        }
        body.extend_from_slice(&chunk[..count]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const IDLE: std::time::Duration = std::time::Duration::from_secs(1);

    #[tokio::test]
    async fn exact_limit_empty_and_overflow() {
        assert_eq!(
            buffer(&mut b"hello".as_slice(), 5, IDLE).await.ok().unwrap(),
            b"hello"
        );
        assert!(matches!(
            buffer(&mut b"hello!".as_slice(), 5, IDLE).await,
            Err(Error::TooLarge)
        ));
        assert!(
            buffer(&mut b"".as_slice(), 0, IDLE)
                .await
                .ok()
                .unwrap()
                .is_empty()
        );
        assert!(matches!(
            buffer(&mut b"x".as_slice(), 0, IDLE).await,
            Err(Error::TooLarge)
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn stalled_input_hits_idle_deadline() {
        let (_writer, mut reader) = tokio::io::duplex(16);
        let start = tokio::time::Instant::now();
        assert!(matches!(buffer(&mut reader, 16, IDLE).await, Err(Error::Timeout)));
        assert_eq!(start.elapsed(), IDLE);
    }

    #[tokio::test(start_paused = true)]
    async fn progress_resets_idle_but_does_not_disable_outer_deadline() {
        use tokio::io::AsyncWriteExt;
        for overall in [
            std::time::Duration::from_millis(1200),
            std::time::Duration::from_secs(5),
        ] {
            let (mut writer, mut reader) = tokio::io::duplex(16);
            let producer = tokio::spawn(async move {
                for _ in 0..3 {
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                    writer.write_all(b"x").await.unwrap();
                }
                writer.shutdown().await.unwrap();
            });
            let result = tokio::time::timeout(overall, buffer(&mut reader, 16, IDLE)).await;
            if overall < std::time::Duration::from_secs(2) {
                assert!(result.is_err(), "progress cannot renew the overall deadline");
                producer.abort();
                let _ = producer.await;
            } else {
                assert_eq!(result.unwrap().ok().unwrap(), b"xxx");
                producer.await.unwrap();
            }
        }
    }
}
