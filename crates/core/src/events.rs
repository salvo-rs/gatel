//! Lightweight pub/sub event bus for inter-module communication.
//!
//! Modules can emit events when interesting things happen (config reload,
//! certificate issued, upstream health change, etc.) and other modules can
//! subscribe to those events to react.
//!
//! # Example
//!
//! ```ignore
//! let bus = EventBus::new();
//! let mut rx = bus.subscribe();
//! bus.emit(Event::ConfigReloaded);
//! let event = rx.recv().await.unwrap();
//! ```

use std::fmt;

use tokio::sync::broadcast;

/// Maximum number of events buffered in the channel before old events are
/// dropped for slow subscribers.
const EVENT_CHANNEL_CAPACITY: usize = 256;

/// Events that can be emitted by gatel modules.
#[derive(Debug, Clone)]
pub enum Event {
    /// Configuration was reloaded successfully.
    ConfigReloaded,
    /// Configuration reload failed.
    ConfigReloadFailed { error: String },
    /// A TLS certificate was issued or renewed.
    CertIssued { domain: String },
    /// A TLS certificate renewal failed.
    CertRenewalFailed { domain: String, error: String },
    /// An upstream backend changed health status.
    UpstreamHealthChanged { address: String, healthy: bool },
    /// The server is shutting down.
    ShutdownInitiated,
    /// Custom event from a plugin module.
    Custom { name: String, data: String },
}

impl fmt::Display for Event {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Event::ConfigReloaded => write!(f, "config_reloaded"),
            Event::ConfigReloadFailed { error } => write!(f, "config_reload_failed: {error}"),
            Event::CertIssued { domain } => write!(f, "cert_issued: {domain}"),
            Event::CertRenewalFailed { domain, error } => {
                write!(f, "cert_renewal_failed: {domain}: {error}")
            }
            Event::UpstreamHealthChanged { address, healthy } => {
                write!(f, "upstream_health: {address} healthy={healthy}")
            }
            Event::ShutdownInitiated => write!(f, "shutdown_initiated"),
            Event::Custom { name, data } => write!(f, "custom:{name}: {data}"),
        }
    }
}

/// Broadcast-based event bus.
///
/// Cloning an `EventBus` produces a handle to the same underlying channel.
#[derive(Clone)]
pub struct EventBus {
    tx: broadcast::Sender<Event>,
}

impl EventBus {
    /// Create a new event bus.
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        Self { tx }
    }

    /// Emit an event to all subscribers.
    ///
    /// Returns the number of subscribers that received the event.
    /// If there are no subscribers the event is silently dropped.
    pub fn emit(&self, event: Event) -> usize {
        self.tx.send(event).unwrap_or(0)
    }

    /// Subscribe to events. Returns a receiver that yields events.
    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.tx.subscribe()
    }
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn emit_and_receive() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();

        bus.emit(Event::ConfigReloaded);

        let event = rx.recv().await.unwrap();
        assert!(matches!(event, Event::ConfigReloaded));
    }

    #[tokio::test]
    async fn multiple_subscribers() {
        let bus = EventBus::new();
        let mut rx1 = bus.subscribe();
        let mut rx2 = bus.subscribe();

        let count = bus.emit(Event::ShutdownInitiated);
        assert_eq!(count, 2);

        assert!(matches!(
            rx1.recv().await.unwrap(),
            Event::ShutdownInitiated
        ));
        assert!(matches!(
            rx2.recv().await.unwrap(),
            Event::ShutdownInitiated
        ));
    }

    #[tokio::test]
    async fn no_subscribers() {
        let bus = EventBus::new();
        let count = bus.emit(Event::ConfigReloaded);
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn custom_event() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();

        bus.emit(Event::Custom {
            name: "my_plugin".into(),
            data: "something happened".into(),
        });

        let event = rx.recv().await.unwrap();
        if let Event::Custom { name, data } = event {
            assert_eq!(name, "my_plugin");
            assert_eq!(data, "something happened");
        } else {
            panic!("expected Custom event");
        }
    }
}
