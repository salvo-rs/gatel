//! Candidate-local log policies. No plugin callbacks or I/O during publication.
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};

use crate::sink::{Rotate, Sink, Sinks};

#[derive(Debug)]
pub struct StagedSinks {
    sinks: Sinks,
    requested: Mutex<BTreeMap<String, (Arc<Sink>, Rotate)>>,
}

impl StagedSinks {
    pub(crate) fn new(sinks: Sinks) -> Self {
        Self {
            sinks,
            requested: Mutex::default(),
        }
    }

    pub fn file(&self, path: &Path, rotate: Rotate) -> Result<Arc<Sink>, String> {
        let sink = self.sinks.file_handle(path, Rotate::NEVER)?;
        let mut requested = self.requested.lock().unwrap_or_else(|p| p.into_inner());
        if let Some((_, previous)) = requested.get(sink.name()) {
            if *previous != rotate {
                return Err(format!(
                    "conflicting rotation policies for log sink `{}`",
                    sink.name()
                ));
            }
        } else {
            requested.insert(sink.name().to_owned(), (sink.clone(), rotate));
        }
        Ok(sink)
    }

    pub fn finish(self) -> SinkUpdate {
        SinkUpdate {
            requested: self
                .requested
                .into_inner()
                .unwrap_or_else(|p| p.into_inner())
                .into_values()
                .collect(),
        }
    }
}

#[derive(Debug)]
pub struct SinkUpdate {
    requested: Vec<(Arc<Sink>, Rotate)>,
}

pub struct SinkPublication<'a> {
    policies: Vec<(MutexGuard<'a, Rotate>, Rotate)>,
}

impl SinkUpdate {
    pub fn begin(&self) -> SinkPublication<'_> {
        SinkPublication {
            policies: self
                .requested
                .iter()
                .map(|(sink, rotate)| (sink.policy.lock().unwrap_or_else(|p| p.into_inner()), *rotate))
                .collect(),
        }
    }
}

impl SinkPublication<'_> {
    pub fn publish(self) {
        for (mut policy, next) in self.policies {
            *policy = next;
        }
    }
}
