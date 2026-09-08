//! Abort only after the real cache collector has polled the replacement body.
use std::io;
use std::sync::Mutex;
use std::task::Poll;

use tokio::sync::oneshot;

use super::*;

struct Released(Option<oneshot::Sender<()>>);
impl Drop for Released {
    fn drop(&mut self) {
        if let Some(sender) = self.0.take() {
            let _ = sender.send(());
        }
    }
}

struct PendingBody(Mutex<Option<(oneshot::Sender<()>, oneshot::Sender<()>)>>);
#[async_trait]
impl Handler for PendingBody {
    async fn handle(&self, _: &mut Request, _: &mut Depot, response: &mut Response, _: &mut FlowCtrl) {
        let (ready, released) = self.0.lock().unwrap().take().unwrap();
        let mut ready = Some(ready);
        let owner = Released(Some(released));
        response.status_code(StatusCode::OK);
        response
            .headers_mut()
            .insert("cache-control", "max-age=600".parse().unwrap());
        response.stream(futures_util::stream::poll_fn(move |_| {
            let _keep_alive = &owner;
            if let Some(ready) = ready.take() {
                let _ = ready.send(());
            }
            Poll::<Option<Result<Bytes, io::Error>>>::Pending
        }));
    }
}

#[tokio::test]
async fn aborting_body_collection_releases_body_and_retires_only_observed_copy() {
    tokio::time::timeout(Duration::from_secs(5), async {
        for replace in [false, true] {
            let store = Arc::new(Store::new(4096));
            let handler = Arc::new(CacheHandler {
                store: store.clone(),
                max_entry: 1024,
                default_ttl: None,
            });
            let mut request = Request::default();
            *request.uri_mut() = "http://example.com/".parse().unwrap();
            request
                .headers_mut()
                .insert("cache-control", "no-cache".parse().unwrap());
            let mut depot = Depot::new();
            depot.insert(
                gatel_plan::ExecutionIdentity::DEPOT_KEY,
                gatel_plan::ExecutionIdentity(gatel_model::Hasher::new().finish()),
            );
            let key = handler.key(&request, &depot);
            let entry = |body: &'static [u8]| store::Entry {
                status: 200,
                headers: http::HeaderMap::new(),
                body: Bytes::from_static(body),
                stored: std::time::Instant::now(),
                ttl: Duration::from_secs(600),
                age_on_arrival: Duration::ZERO,
            };
            store.put(key.clone(), String::new(), vec![], entry(b"old"));
            let (started, ready) = oneshot::channel();
            let (released, closed) = oneshot::channel();
            let task = tokio::spawn(async move {
                let mut ctrl = FlowCtrl::new(vec![
                    handler,
                    Arc::new(PendingBody(Mutex::new(Some((started, released))))),
                ]);
                ctrl.call_next(&mut request, &mut depot, &mut Response::new())
                    .await;
            });
            ready.await.unwrap();
            if replace {
                store.put(key.clone(), String::new(), vec![], entry(b"new"));
            }
            task.abort();
            assert!(task.await.unwrap_err().is_cancelled());
            closed.await.unwrap();
            if replace {
                assert_eq!(store.get(&key, "").unwrap().body, "new");
            } else {
                assert!(store.get(&key, "").is_none());
                assert_eq!(store.remembered_resources(), 0);
            }
        }
    })
    .await
    .expect("cache collection cancellation timed out");
}
