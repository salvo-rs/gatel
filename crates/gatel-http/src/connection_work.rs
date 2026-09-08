//! Transfer upgraded I/O to the connection owner instead of spawning a detached
//! tunnel. The owner retains its generation, connection permit and drain budget.
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

type Work = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

#[derive(Clone, Default)]
pub(crate) struct ConnectionWork(Arc<Mutex<Handoff>>);

#[derive(Default)]
struct Handoff {
    accepted: bool,
    work: Option<Work>,
}

impl ConnectionWork {
    pub fn set(&self, work: impl Future<Output = ()> + Send + 'static) -> Result<(), ()> {
        let mut held = self.0.lock().unwrap();
        if held.accepted {
            return Err(());
        }
        held.accepted = true;
        held.work = Some(Box::pin(work));
        Ok(())
    }

    pub async fn finish(&self) {
        let work = self.0.lock().unwrap().work.take();
        if let Some(work) = work {
            work.await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn handoff_is_single_and_not_run_until_http_finishes() {
        let (sent, mut received) = tokio::sync::oneshot::channel();
        let work = ConnectionWork::default();
        work.set(async move {
            let _ = sent.send(());
        })
        .unwrap();
        assert!(
            work.set(async {
                panic!("second handoff ran");
            })
            .is_err()
        );
        assert!(matches!(
            received.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ));
        work.finish().await;
        received.await.unwrap();
        assert!(work.set(async {}).is_err());
        work.finish().await;
    }

    #[tokio::test]
    async fn a_failed_http_exchange_drops_unstarted_handoff_resources() {
        let (held, closed) = tokio::sync::oneshot::channel::<()>();
        let work = ConnectionWork::default();
        work.set(async move {
            let _held = held;
            std::future::pending::<()>().await;
        })
        .unwrap();
        drop(work);
        assert!(closed.await.is_err());
    }
}
