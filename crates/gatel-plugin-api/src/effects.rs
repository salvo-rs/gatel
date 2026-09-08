//! Declarative request-side effects, independent of directive names.
//! These describe trusted plugin code; they are not a sandbox or I/O grant.
use gatel_model::Hasher;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Effect {
    /// Install a streaming request-body bound and reject excessive declared length.
    RequestBodyLimit,
    /// Read request-body bytes, including buffering or transforming them.
    ReadRequestBody,
    /// Initiate a request to another service.
    CallUpstream,
    /// Debit a request quota, even if later middleware rejects the request.
    ConsumeQuota,
    /// May answer from a stored response without invoking downstream handlers.
    /// Every Guard-phase middleware must execute before this lookup.
    CacheLookup,
    /// Validate/transform the request body before consumers see its bytes.
    TransformRequestBody,
}

impl Effect {
    /// Request-side precedence. This does not promise that unknown-length body
    /// overflow can be detected before quota debit or an upstream call.
    pub const fn must_precede(self, other: Self) -> bool {
        (matches!(self, Self::RequestBodyLimit)
            && matches!(
                other,
                Self::ReadRequestBody | Self::CallUpstream | Self::ConsumeQuota | Self::TransformRequestBody
            ))
            || (matches!(self, Self::TransformRequestBody)
                && matches!(other, Self::ReadRequestBody | Self::CallUpstream))
    }
}

pub(crate) fn hash(effects: &[Effect], hasher: &mut Hasher) {
    let mut canonical = effects.to_vec();
    canonical.sort_unstable();
    canonical.dedup();
    hasher.update_u64(canonical.len() as u64);
    for effect in canonical {
        hasher.update_u64(effect as u64);
    }
}
