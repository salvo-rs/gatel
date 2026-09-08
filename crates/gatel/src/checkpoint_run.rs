//! Explicit checkpoint-only run mode; never falls back to memory-only writes.
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use gatel_plan::Outcome;

use crate::app::CheckpointSession;
use crate::{App, CONFIG_REJECTED, RUNTIME_FAILURE, admin, watch};

#[derive(clap::Args, Default)]
#[group(id = "CheckpointRunOptions")]
pub(crate) struct Options {
    /// Save every configuration update in this existing private directory; no WAL or idempotent receipts.
    #[arg(long, requires = "checkpoint_instance", conflicts_with_all = ["bootstrap_journal", "bootstrap_instance"])]
    pub checkpoint_journal: Option<PathBuf>,
    /// Stable checkpoint identity; not a credential. Dry-run also initializes/saves checkpoints.
    #[arg(long, requires = "checkpoint_journal")]
    checkpoint_instance: Option<String>,
}

pub(crate) fn run(
    app: Result<App, String>,
    options: Options,
    admin: admin::cli::Options,
    dry_run: bool,
) -> ExitCode {
    let result = (|| {
        let app = app?;
        let directory = options
            .checkpoint_journal
            .ok_or("checkpoint directory is missing")?;
        let instance = options
            .checkpoint_instance
            .ok_or("checkpoint instance is missing")?;
        let session = CheckpointSession::open(app, &directory, &instance).map_err(str::to_owned)?;
        let admin = admin.prepare()?;
        Ok::<_, String>((session, admin))
    })();
    let (mut session, admin) = match result {
        Ok(prepared) => prepared,
        Err(error) => {
            eprintln!("gatel: {error}");
            return ExitCode::from(RUNTIME_FAILURE);
        }
    };
    let runtime = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("gatel: cannot start the async runtime: {error}");
            return ExitCode::from(RUNTIME_FAILURE);
        }
    };
    runtime.block_on(async move {
        tracing::debug!(target: "gatel::process", event = "process_started",
            boot_id = %session.app().configuration_status().boot_id, config = %session.app().root.display());
        tracing::warn!(target: "gatel::config", event = "checkpoint_mode",
            "checkpoint-only run mode; crash before save may retain the previous checkpoint; no idempotent receipts");
        let startup = session.startup().await;
        match startup {
            Ok(Outcome::Started { generation }) => {
                for address in session.app().server.active_addresses().await {
                    tracing::info!(target: "gatel::listener", event = "serving", %address);
                }
                tracing::info!(target: "gatel::config", event = "config_committed", generation = generation.number());
            }
            Ok(Outcome::Rejected(diagnostics)) => {
                eprintln!("{diagnostics}");
                session.app().shutdown().await;
                return ExitCode::from(CONFIG_REJECTED);
            }
            other => {
                eprintln!("gatel: checkpoint startup failed: {other:?}");
                session.app().shutdown().await;
                return ExitCode::from(RUNTIME_FAILURE);
            }
        }
        let session = Arc::new(session);
        let mut admin = match admin.map(|prepared| prepared.start_checkpoint(&session)).transpose() {
            Ok(admin) => admin,
            Err(error) => { eprintln!("gatel: {error}"); session.app().shutdown().await; return ExitCode::from(RUNTIME_FAILURE); }
        };
        let mut failed = false;
        if !dry_run {
            tokio::select! {
                result = watch::reload_checkpoint_until(session.clone(), watch::shutdown_signal()) => {
                    if let Err(error) = result {
                        eprintln!("gatel: checkpoint watcher stopped: {error}");
                        failed = true;
                    }
                }
                result = async {
                    match admin.as_mut() {
                        Some(server) => server.terminated().await,
                        None => std::future::pending().await,
                    }
                } => { eprintln!("gatel: admin listener stopped unexpectedly: {result:?}"); failed = true; }
            }
        }
        tracing::info!(target: "gatel::process", event = "shutdown_started");
        if let Some(admin) = admin
            && let Err(error) = admin.shutdown().await
        {
            eprintln!("gatel: {error}");
            failed = true;
        }
        session.app().shutdown().await;
        tracing::info!(target: "gatel::process", event = "shutdown_complete");
        if failed { ExitCode::from(RUNTIME_FAILURE) } else {
            if dry_run { println!("ok: checkpoint confirmed, bound and ready; exiting because of --dry-run"); }
            ExitCode::SUCCESS
        }
    })
}
