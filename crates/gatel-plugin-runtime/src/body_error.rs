//! Typed request-body failures shared by host guards and optional terminals.
use std::error::Error;
use std::fmt;

/// Effective host/guard bound, also applicable after a plugin transforms bytes.
/// Cooperative metadata for trusted plugins, not a replacement for body wrappers.
#[derive(Debug, Clone, Copy)]
pub struct RequestBodyLimit(u64);
impl RequestBodyLimit {
    const KEY: &'static str = "gatel.request_body_limit";
    pub fn install(depot: &mut salvo_core::Depot, limit: u64) {
        let limit = Self::get(depot).map_or(limit, |existing| existing.min(limit));
        depot.insert(Self::KEY, Self(limit));
    }
    pub fn get(depot: &salvo_core::Depot) -> Option<u64> {
        depot.get::<Self>(Self::KEY).ok().map(|limit| limit.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BodyTooLarge {
    pub limit: u64,
}

impl fmt::Display for BodyTooLarge {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "request body is larger than the {} byte limit", self.limit)
    }
}

impl Error for BodyTooLarge {}

impl BodyTooLarge {
    /// Inspect typed causes, never arbitrary backend/error text. The bounded
    /// walk also protects against a plugin supplying a cyclic source chain.
    pub fn find(mut error: &(dyn Error + 'static)) -> Option<Self> {
        for _ in 0..32 {
            if let Some(value) = error.downcast_ref::<Self>() {
                return Some(*value);
            }
            // These adapters do not necessarily expose their stored error as
            // source() (or may expose only its source, skipping the marker).
            error = match error.downcast_ref::<salvo_core::Error>() {
                Some(salvo_core::Error::Other(inner)) => inner.as_ref(),
                Some(salvo_core::Error::Io(inner)) => inner,
                _ => match error.downcast_ref::<std::io::Error>() {
                    Some(inner) => inner.get_ref()?,
                    None => error.source()?,
                },
            };
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transformations_inherit_the_tightest_installed_body_bound() {
        let mut depot = salvo_core::Depot::new();
        assert_eq!(RequestBodyLimit::get(&depot), None);
        for (installed, expected) in [(1024, 1024), (64, 64), (2048, 64)] {
            RequestBodyLimit::install(&mut depot, installed);
            assert_eq!(RequestBodyLimit::get(&depot), Some(expected));
        }
    }

    #[test]
    fn typed_cause_not_message_controls_classification() {
        let error = salvo_core::Error::other(BodyTooLarge { limit: 4 });
        assert_eq!(BodyTooLarge::find(&error), Some(BodyTooLarge { limit: 4 }));
        let nested = salvo_core::Error::other(salvo_core::Error::Io(std::io::Error::other(error)));
        assert_eq!(BodyTooLarge::find(&nested), Some(BodyTooLarge { limit: 4 }));
        assert_eq!(
            BodyTooLarge::find(&std::io::Error::other(
                "request body is larger than the 4 byte limit"
            )),
            None
        );
    }

    #[test]
    fn cyclic_plugin_error_is_bounded() {
        #[derive(Debug)]
        struct Cycle;
        impl fmt::Display for Cycle {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("cycle")
            }
        }
        impl Error for Cycle {
            fn source(&self) -> Option<&(dyn Error + 'static)> {
                Some(self)
            }
        }
        assert_eq!(BodyTooLarge::find(&Cycle), None);
    }
}
