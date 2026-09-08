//! Socket ownership.
//!
//! Listening sockets belong to the process, not to a generation. A reload that
//! keeps the same address keeps the same socket, so no connection is ever
//! refused in the gap between two configurations, and a reload that adds an
//! address either binds every new one or none of them.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use tokio::net::TcpListener;

/// Every socket the process is listening on.
#[derive(Default)]
pub struct BindTable {
    sockets: Mutex<BTreeMap<SocketAddr, Arc<TcpListener>>>,
}

impl std::fmt::Debug for BindTable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let sockets = self.sockets.lock().unwrap_or_else(|p| p.into_inner());
        f.debug_struct("BindTable")
            .field("bound", &sockets.keys().collect::<Vec<_>>())
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
        let mut sockets = self.sockets.lock().unwrap_or_else(|p| p.into_inner());
        if sockets.contains_key(&address) {
            return Err(format!("listener {address} is already registered"));
        }
        listener
            .set_nonblocking(true)
            .map_err(|error| format!("cannot configure listener {address}: {error}"))?;
        let listener = TcpListener::from_std(listener)
            .map_err(|error| format!("cannot register listener {address}: {error}"))?;
        sockets.insert(address, Arc::new(listener));
        Ok(address)
    }

    /// Bind every address that is not bound yet, and hand back the whole set.
    ///
    /// All or nothing: if the third of four addresses is already in use, the
    /// two that were just opened are closed again and the table is left exactly
    /// as it was. This is what makes a failed reload leave the running
    /// configuration untouched rather than half-applied.
    pub fn ensure(&self, wanted: &[SocketAddr]) -> Result<BTreeMap<SocketAddr, Arc<TcpListener>>, String> {
        let mut sockets = self.sockets.lock().unwrap_or_else(|p| p.into_inner());
        let mut opened: BTreeMap<SocketAddr, Arc<TcpListener>> = BTreeMap::new();

        for address in wanted {
            if sockets.contains_key(address) || opened.contains_key(address) {
                continue;
            }
            match bind(*address) {
                Ok(listener) => {
                    opened.insert(*address, Arc::new(listener));
                }
                Err(error) => {
                    // `opened` is dropped here, closing everything this call
                    // created; the table never saw them.
                    return Err(format!("cannot bind {address}: {error}"));
                }
            }
        }

        sockets.extend(opened);
        Ok(wanted
            .iter()
            .filter_map(|address| sockets.get(address).map(|s| (*address, Arc::clone(s))))
            .collect())
    }

    /// Close every socket no longer wanted, returning what was released.
    pub fn retain(&self, wanted: &[SocketAddr]) -> Vec<SocketAddr> {
        let mut sockets = self.sockets.lock().unwrap_or_else(|p| p.into_inner());
        let released: Vec<SocketAddr> = sockets
            .keys()
            .filter(|address| !wanted.contains(address))
            .copied()
            .collect();
        for address in &released {
            sockets.remove(address);
        }
        released
    }

    pub fn bound(&self) -> Vec<SocketAddr> {
        self.sockets
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .keys()
            .copied()
            .collect()
    }

    pub fn get(&self, address: &SocketAddr) -> Option<Arc<TcpListener>> {
        self.sockets
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(address)
            .cloned()
    }
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
