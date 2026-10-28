//! Socket ownership.
//!
//! Listening sockets belong to the process, not to a generation. A reload that
//! keeps the same address keeps the same socket, so no connection is ever
//! refused in the gap between two configurations, and a reload that adds an
//! address either binds every new one or none of them. New sockets belong to
//! their candidate until publication; the table's reservation index is weak.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, MutexGuard, Weak};

use tokio::net::TcpListener;

/// Every socket the process is listening on.
#[derive(Clone, Default)]
pub struct BindTable {
    inner: Arc<Mutex<Bindings>>,
}

#[derive(Default)]
struct Bindings {
    sockets: BTreeMap<SocketAddr, Arc<TcpListener>>,
    // This index never owns a socket. A failed candidate cannot keep a
    // listener alive through it; overlapping candidates can share reservations.
    reserved: BTreeMap<SocketAddr, Weak<TcpListener>>,
}

/// Candidate-owned sockets, installed only after the host's stale checks.
pub(crate) struct BindUpdate {
    target: BindTable,
    pending: Mutex<Option<BTreeMap<SocketAddr, Arc<TcpListener>>>>,
    retired: Mutex<Option<BTreeMap<SocketAddr, Arc<TcpListener>>>>,
}

impl std::fmt::Debug for BindUpdate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BindUpdate").finish_non_exhaustive()
    }
}

pub(crate) struct BindPublication<'a> {
    bindings: MutexGuard<'a, Bindings>,
    pending: MutexGuard<'a, Option<BTreeMap<SocketAddr, Arc<TcpListener>>>>,
    retired: MutexGuard<'a, Option<BTreeMap<SocketAddr, Arc<TcpListener>>>>,
}

impl BindUpdate {
    pub(crate) fn begin(&self) -> BindPublication<'_> {
        BindPublication {
            bindings: self.target.inner.lock().unwrap_or_else(|p| p.into_inner()),
            pending: self.pending.lock().unwrap_or_else(|p| p.into_inner()),
            retired: self.retired.lock().unwrap_or_else(|p| p.into_inner()),
        }
    }

    pub(crate) fn finish(&self) {
        let retired = self.retired.lock().unwrap_or_else(|p| p.into_inner()).take();
        // Closing removed sockets happens outside the generation/publication
        // locks. Accept loops retain their own handles until activation.
        drop(retired);
    }
}

impl BindPublication<'_> {
    pub(crate) fn publish(mut self) {
        if let Some(next) = self.pending.take() {
            *self.retired = Some(std::mem::replace(&mut self.bindings.sockets, next));
        }
    }
}

impl std::fmt::Debug for BindTable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let bindings = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        f.debug_struct("BindTable")
            .field("bound", &bindings.sockets.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl BindTable {
    pub fn new() -> Self {
        Self::default()
    }

    /// Transfer a bound TCP listener into process-owned socket storage without
    /// closing and rebinding it. Its actual local address must appear in the
    /// configuration; port zero is resolved by the OS before registration.
    ///
    /// The caller must enter a Tokio runtime with I/O enabled, as for `ensure`.
    /// The listener becomes nonblocking. On error the supplied handle is dropped
    /// and existing entries are unchanged. Duplicate addresses never replace an
    /// active listener. This is an embedding hook, not an OS activation protocol.
    pub fn adopt(&self, listener: std::net::TcpListener) -> Result<SocketAddr, String> {
        let address = listener
            .local_addr()
            .map_err(|error| format!("cannot inspect listener: {error}"))?;
        let mut bindings = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        if bindings.sockets.contains_key(&address)
            || bindings.reserved.get(&address).and_then(Weak::upgrade).is_some()
        {
            return Err(format!("listener {address} is already registered"));
        }
        listener
            .set_nonblocking(true)
            .map_err(|error| format!("cannot configure listener {address}: {error}"))?;
        let listener = TcpListener::from_std(listener)
            .map_err(|error| format!("cannot register listener {address}: {error}"))?;
        let listener = Arc::new(listener);
        bindings.reserved.insert(address, Arc::downgrade(&listener));
        bindings.sockets.insert(address, listener);
        Ok(address)
    }

    /// Bind every address that is not bound yet, and hand back the whole set.
    ///
    /// All or nothing: if the third of four addresses is already in use, the
    /// two that were just opened are closed again and the table is left exactly
    /// as it was. This is what makes a failed reload leave the running
    /// configuration untouched rather than half-applied.
    pub fn ensure(&self, wanted: &[SocketAddr]) -> Result<BTreeMap<SocketAddr, Arc<TcpListener>>, String> {
        let mut bindings = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let selected = gather(&mut bindings, wanted)?;
        bindings.sockets.extend(selected.clone());
        Ok(selected)
    }

    /// Prepare a complete candidate without publishing its new addresses.
    /// Dropping it after failure, cancellation or a stale check closes every
    /// new socket no other candidate needs. Existing/adopted sockets stay put.
    pub(crate) fn stage(&self, wanted: &[SocketAddr]) -> Result<BindUpdate, String> {
        let pending = gather(&mut self.inner.lock().unwrap_or_else(|p| p.into_inner()), wanted)?;
        Ok(BindUpdate {
            target: self.clone(),
            pending: Mutex::new(Some(pending)),
            retired: Mutex::new(None),
        })
    }

    /// Close every socket no longer wanted, returning what was released.
    pub fn retain(&self, wanted: &[SocketAddr]) -> Vec<SocketAddr> {
        let mut bindings = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let released: Vec<SocketAddr> = bindings
            .sockets
            .keys()
            .filter(|address| !wanted.contains(address))
            .copied()
            .collect();
        for address in &released {
            bindings.sockets.remove(address);
        }
        released
    }

    pub fn bound(&self) -> Vec<SocketAddr> {
        self.inner
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .sockets
            .keys()
            .copied()
            .collect()
    }

    pub fn get(&self, address: &SocketAddr) -> Option<Arc<TcpListener>> {
        self.inner
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .sockets
            .get(address)
            .cloned()
    }
}

fn gather(
    bindings: &mut Bindings,
    wanted: &[SocketAddr],
) -> Result<BTreeMap<SocketAddr, Arc<TcpListener>>, String> {
    bindings
        .reserved
        .retain(|_, listener| listener.strong_count() != 0);
    let mut selected = BTreeMap::new();
    for address in wanted {
        if selected.contains_key(address) {
            continue;
        }
        let listener = match bindings
            .sockets
            .get(address)
            .cloned()
            .or_else(|| bindings.reserved.get(address).and_then(Weak::upgrade))
        {
            Some(listener) => listener,
            None => Arc::new(bind(*address).map_err(|error| format!("cannot bind {address}: {error}"))?),
        };
        bindings.reserved.insert(*address, Arc::downgrade(&listener));
        selected.insert(*address, listener);
    }
    Ok(selected)
}

/// Bind synchronously so the whole set can be opened inside `prepare`, which
/// is not async, and so a failure is reported before anything is published.
fn bind(address: SocketAddr) -> std::io::Result<TcpListener> {
    let listener = std::net::TcpListener::bind(address)?;
    listener.set_nonblocking(true)?;
    TcpListener::from_std(listener)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn unpublished_candidate_releases_only_its_new_sockets() {
        let table = BindTable::new();
        let adopted = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = table.adopt(adopted).unwrap();
        let original = table.get(&address).unwrap();
        let added = free_address();
        let candidate = table.stage(&[address, added]).unwrap();
        assert_eq!(table.bound(), vec![address]);
        assert!(table.get(&added).is_none());
        assert!(std::net::TcpListener::bind(added).is_err());
        // Even acquiring and dropping a publication guard changes nothing.
        drop(candidate.begin());
        drop(candidate);
        assert!(std::net::TcpListener::bind(added).is_ok());
        assert!(Arc::ptr_eq(&original, &table.get(&address).unwrap()));
    }

    #[tokio::test]
    async fn overlapping_candidates_share_reservations_without_owning_each_other() {
        let table = BindTable::new();
        let shared = free_address();
        let discarded = free_address();
        let loser = table.stage(&[shared, discarded]).unwrap();
        let winner = table.stage(&[shared]).unwrap();
        {
            let first = loser.pending.lock().unwrap();
            let second = winner.pending.lock().unwrap();
            assert!(Arc::ptr_eq(
                &first.as_ref().unwrap()[&shared],
                &second.as_ref().unwrap()[&shared]
            ));
        }
        assert!(table.bound().is_empty());
        winner.begin().publish();
        winner.finish();
        drop(loser);
        assert_eq!(table.bound(), vec![shared]);
        assert!(std::net::TcpListener::bind(discarded).is_ok());
        assert!(std::net::TcpListener::bind(shared).is_err());
        // The published Runtime/update must not keep removed ports alive.
        table.retain(&[]);
        assert!(std::net::TcpListener::bind(shared).is_ok());
    }

    #[tokio::test]
    async fn cancelling_a_candidate_drops_its_reservations() {
        let table = BindTable::new();
        let address = free_address();
        let (ready, prepared) = tokio::sync::oneshot::channel();
        let task = tokio::spawn({
            let table = table.clone();
            async move {
                let _candidate = table.stage(&[address]).unwrap();
                ready.send(()).unwrap();
                std::future::pending::<()>().await;
            }
        });
        prepared.await.unwrap();
        assert!(table.bound().is_empty());
        assert!(std::net::TcpListener::bind(address).is_err());
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert!(std::net::TcpListener::bind(address).is_ok());
    }

    #[tokio::test]
    async fn concurrent_candidates_do_not_race_to_bind_the_same_address() {
        let table = BindTable::new();
        let address = free_address();
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let spawn = || {
            let table = table.clone();
            let barrier = Arc::clone(&barrier);
            let runtime = tokio::runtime::Handle::current();
            std::thread::spawn(move || {
                let _entered = runtime.enter();
                barrier.wait();
                table.stage(&[address]).unwrap()
            })
        };
        let first = spawn();
        let second = spawn();
        let first = first.join().unwrap();
        let second = second.join().unwrap();
        assert!(table.bound().is_empty());
        drop(first);
        assert!(std::net::TcpListener::bind(address).is_err());
        drop(second);
        assert!(std::net::TcpListener::bind(address).is_ok());
    }

    #[tokio::test]
    async fn adopting_keeps_queued_connections_and_rejects_duplicate_ownership() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let table = BindTable::new();
        let socket = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = socket.local_addr().unwrap();
        let duplicate = socket.try_clone().unwrap();
        let mut client = tokio::net::TcpStream::connect(address).await.unwrap();
        client.write_all(b"queued before adoption").await.unwrap();
        assert_eq!(table.adopt(socket).unwrap(), address);
        let adopted = table.get(&address).unwrap();
        assert!(table.adopt(duplicate).unwrap_err().contains("already registered"));
        assert!(Arc::ptr_eq(
            &adopted,
            &table.ensure(&[address]).unwrap()[&address]
        ));
        let (mut accepted, _) = tokio::time::timeout(std::time::Duration::from_secs(2), adopted.accept())
            .await
            .unwrap()
            .unwrap();
        let mut bytes = [0; 22];
        tokio::time::timeout(std::time::Duration::from_secs(2), accepted.read_exact(&mut bytes))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(&bytes, b"queued before adoption");
        drop(accepted);
        drop(client);
        let weak = Arc::downgrade(&adopted);
        drop(adopted);
        assert_eq!(table.retain(&[]), vec![address]);
        assert!(weak.upgrade().is_none());
    }

    fn free_address() -> SocketAddr {
        static USED: std::sync::Mutex<std::collections::BTreeSet<SocketAddr>> =
            std::sync::Mutex::new(std::collections::BTreeSet::new());
        let mut used = USED.lock().unwrap();
        let mut repeated = Vec::new();
        loop {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let address = listener.local_addr().unwrap();
            if used.insert(address) {
                return address;
            }
            // Do not immediately close a repeated allocation: the OS may keep
            // returning that same port instead of selecting a distinct one.
            repeated.push(listener);
        }
    }

    #[tokio::test]
    async fn an_address_that_stays_keeps_its_socket() {
        let table = BindTable::new();
        let address = free_address();

        let first = table.ensure(&[address]).unwrap();
        let second = table.ensure(&[address]).unwrap();
        assert!(
            Arc::ptr_eq(&first[&address], &second[&address]),
            "rebinding would refuse connections during the gap"
        );
    }

    #[tokio::test]
    async fn a_partly_failing_bind_leaves_nothing_behind() {
        let table = BindTable::new();
        // Hold an address so binding it fails.
        let taken = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let occupied = taken.local_addr().unwrap();
        let good = free_address();
        assert_ne!(good, occupied);

        let error = table.ensure(&[good, occupied]).unwrap_err();
        assert!(error.contains("cannot bind"));
        assert!(
            table.bound().is_empty(),
            "the good socket must not survive a failed set"
        );

        drop(taken);
        assert!(
            table.ensure(&[good, occupied]).is_ok(),
            "retrying after the conflict works"
        );
    }

    #[tokio::test]
    async fn removing_an_address_closes_it() {
        let table = BindTable::new();
        let first = free_address();
        let second = free_address();
        table.ensure(&[first, second]).unwrap();

        let released = table.retain(&[first]);
        assert_eq!(released, vec![second]);
        assert_eq!(table.bound(), vec![first]);
    }

    #[tokio::test]
    async fn the_returned_set_is_exactly_what_was_asked_for() {
        let table = BindTable::new();
        let first = free_address();
        let second = free_address();
        table.ensure(&[first, second]).unwrap();

        // A later generation wants only one of them; the other stays bound
        // until `retain` is called after the commit succeeds.
        let subset = table.ensure(&[first]).unwrap();
        assert_eq!(subset.keys().copied().collect::<Vec<_>>(), vec![first]);
        assert_eq!(table.bound().len(), 2);
    }
}
