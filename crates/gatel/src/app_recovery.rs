//! Recovery shares host admission, blocking preparation and owned activation.
use super::*;

impl App {
    /// Internal bootstrap input, already checked against journal/build policy by
    /// the caller. This does not load files or reconcile journal suffixes. Later
    /// file observation uses admitted intent as its revision baseline. Do not
    /// expose writers before bootstrap completes.
    #[allow(dead_code, reason = "Optional checkpoint bootstrap entry")]
    pub(crate) async fn recover_configuration(
        self: &Arc<Self>,
        expected: gatel_plan::Version,
        serving: Snapshot,
        observed: Snapshot,
    ) -> Result<Outcome, gatel_plan::RestoreIntentError> {
        self.reload_task(move |app| async move {
            app.configuration_work(move |app| app.recover_snapshot_if(&expected, &serving, &observed))
                .await
                .unwrap_or_else(|| {
                    let outcome = Outcome::Stale("configuration submissions are closed".into());
                    outcome.record(&app.telemetry);
                    Ok(outcome)
                })
        })
        .await
    }

    pub(super) fn recover_snapshot_if(
        &self,
        expected: &gatel_plan::Version,
        serving: &Snapshot,
        observed: &Snapshot,
    ) -> Result<Outcome, gatel_plan::RestoreIntentError> {
        let outcome = self
            .store
            .recover_if(expected, serving, observed, &self.registry, |preparation| {
                self.prepare_runtime(preparation)
            })?;
        outcome.record(&self.telemetry);
        Ok(outcome)
    }

    // Keep resource preparation identical for normal reload and cold recovery.
    pub(super) fn prepare_runtime(
        &self,
        preparation: gatel_plan::Preparation<'_, Arc<Runtime>>,
    ) -> Result<Arc<Runtime>, String> {
        let acme = self.acme_factory();
        prepare(
            &preparation,
            &Resources {
                blocking_budget: Some(&self.blocking_budget),
                blocking: self.server.blocking_executor(),
                plugin_retirements: self.server.plugin_retirements(),
                binds: &self.binds,
                state: &self.state,
                certificates: &self.certificates,
                challenges: &self.challenges,
                acme: &acme,
                telemetry: &self.telemetry,
                root: self.root_directory(),
                internal_ca: InternalCa::default_directory(),
            },
        )
    }
}

#[cfg(test)]
#[path = "app_recovery_tests.rs"]
mod tests;
