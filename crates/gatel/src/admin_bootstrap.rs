//! Explicit read-only bootstrap, not runtime write durability.
use std::path::PathBuf;
use std::sync::Arc;

use super::transaction::Journal;
use super::transaction::recovery::Disposition;

#[derive(clap::Args, Default)]
#[group(id = "BootstrapOptions")]
pub(crate) struct Options {
    /// Bootstrap from an existing checkpoint; subsequent reloads remain in memory.
    #[arg(long, requires = "bootstrap_instance")]
    pub bootstrap_journal: Option<PathBuf>,
    /// Stable identity used when the journal was initialized, not a credential.
    #[arg(long, requires = "bootstrap_journal")]
    bootstrap_instance: Option<String>,
}

pub(crate) struct Loaded {
    // Retain exclusive ownership until the process has shut down; no append,
    // checkpoint replacement or implicit tail repair is permitted by this handle.
    _journal: Journal,
    serving: gatel_model::Snapshot,
    observed: gatel_model::Snapshot,
}

impl Options {
    pub fn load(self, registry: &gatel_plan::Registry) -> Result<Option<Loaded>, &'static str> {
        let (directory, instance) = match (self.bootstrap_journal, self.bootstrap_instance) {
            (None, None) => return Ok(None),
            (Some(directory), Some(instance)) => (directory, instance),
            _ => return Err("bootstrap journal and instance must be provided together"),
        };
        let mut journal = Journal::open_existing(&directory, &instance).map_err(
            |_| "cannot open bootstrap journal: check identity, files, permissions and writer lock",
        )?;
        let inspection = journal
            .inspect_recovery(registry)
            .map_err(|_| "invalid or incompatible bootstrap checkpoint/journal")?;
        if inspection.disposition != Disposition::CheckpointCurrent {
            return Err(
                "bootstrap requires a checkpoint covering the entire journal; inspect and reconcile it first",
            );
        }
        let checkpoint = inspection.checkpoint.ok_or("bootstrap checkpoint is missing")?;
        Ok(Some(Loaded {
            _journal: journal,
            serving: checkpoint.sources,
            observed: checkpoint.observed,
        }))
    }
}

impl Loaded {
    pub async fn restore(&mut self, app: &Arc<crate::app::App>) -> Result<gatel_plan::Outcome, &'static str> {
        app.recover_configuration(
            app.configuration_status().version(),
            std::mem::take(&mut self.serving),
            std::mem::take(&mut self.observed),
        )
        .await
        .map_err(|_| "bootstrap source admission failed")
    }
}
