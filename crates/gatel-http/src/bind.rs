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

    fn free_address() -> SocketAddr {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap()
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
        let good = free_address();

        // Hold an address so binding it fails.
        let taken = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let occupied = taken.local_addr().unwrap();

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
