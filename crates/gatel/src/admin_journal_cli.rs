//! Offline diagnostic: bounded replay and compile, never runtime preparation.
use std::io;
use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;

use super::transaction::recovery::{Disposition, Inspection, Reason};
use super::transaction::{Decision, Journal};

#[path = "admin_compact_cli.rs"]
pub(crate) mod compact;

#[derive(clap::Args)]
pub(crate) struct Options {
    /// Existing private directory containing writer.lock and intent.log.
    #[arg(long)]
    directory: PathBuf,
    /// Stable deployment identity used when the journal was initialized (not a secret).
    #[arg(long)]
    instance: String,
}

pub(crate) fn run(options: Options) -> ExitCode {
    let result = crate::plugins::registry()
        .map_err(io::Error::other)
        .and_then(|registry| inspect(&options, &registry));
    match result {
        Ok((report, code)) => {
            if io::stdout().lock().write_all(report.as_bytes()).is_err() {
                eprintln!("gatel: cannot write journal inspection report");
                return ExitCode::from(crate::RUNTIME_FAILURE);
            }
            ExitCode::from(code)
        }
        Err(_) => {
            // Do not print raw registry/decoder/I/O diagnostics: neither source
            // content, caller-controlled identity nor paths belong in this report.
            eprintln!(
                "gatel: journal inspection failed; check existing files, identity, permissions, writer lock, integrity and build compatibility"
            );
            ExitCode::from(crate::RUNTIME_FAILURE)
        }
    }
}

fn inspect(options: &Options, registry: &gatel_plan::Registry) -> io::Result<(String, u8)> {
    let mut journal = Journal::open_existing(&options.directory, &options.instance)?;
    let inspection = journal.inspect_recovery(registry)?;
    Ok(report(&inspection))
}

pub(super) fn report(inspection: &Inspection) -> (String, u8) {
    let (state, code) = match inspection.disposition {
        Disposition::Uninitialized => ("uninitialized", 1),
        Disposition::CheckpointCurrent => ("checkpoint-current", 0),
        Disposition::CheckpointWithRejectedIntent => ("checkpoint-with-rejected-intent", 0),
        Disposition::Reconcile(reason) => (
            match reason {
                Reason::IncompleteTail => "reconcile-incomplete-tail",
                Reason::UnknownOutcome => "reconcile-unknown-outcome",
                Reason::MissingCheckpoint => "reconcile-missing-checkpoint",
                Reason::AppliedAfterCheckpoint => "reconcile-applied-after-checkpoint",
            },
            1,
        ),
    };
    let decision = match inspection.decision {
        None => "none",
        Some(Decision::Applied) => "applied",
        Some(Decision::Rejected) => "rejected",
    };
    let optional = |value: Option<u64>| value.map_or_else(|| "none".into(), |n| n.to_string());
    let checkpoint = inspection.checkpoint.as_ref();
    let count = |value: Option<usize>| value.map_or_else(|| "none".into(), |n| n.to_string());
    let serving_sources = count(checkpoint.map(|c| c.sources.sources().count()));
    let observed_sources = count(checkpoint.map(|c| c.observed.sources().count()));
    let empty_observed_sources =
        count(checkpoint.map(|c| c.observed.sources().filter(|s| s.nodes.is_empty()).count()));
    // A validated checkpoint guarantees equal source revisions have identical
    // full content. A difference still does not imply failed configuration:
    // comment-only NoOp and newly registered empty contributions also differ.
    let observed_differs = checkpoint.map_or_else(
        || "none".into(),
        |c| (c.sources.revs() != c.observed.revs()).to_string(),
    );
    (
        format!(
            "state={state}\njournal_sequence={}\ncheckpoint_sequence={}\nlatest_intent_sequence={}\napplied_intent_sequence={}\nlatest_decision={decision}\nincomplete_tail={}\ncheckpoint_serving_sources={serving_sources}\ncheckpoint_observed_sources={observed_sources}\ncheckpoint_empty_observed_sources={empty_observed_sources}\ncheckpoint_observed_differs={observed_differs}\n",
            inspection.journal_sequence,
            optional(
                inspection
                    .checkpoint
                    .as_ref()
                    .map(|c| c.metadata.journal_sequence)
            ),
            optional(inspection.latest.as_ref().map(|i| i.sequence)),
            optional(inspection.applied.as_ref().map(|i| i.sequence)),
            inspection.incomplete_tail,
        ),
        code,
    )
}

#[cfg(test)]
#[path = "admin_journal_cli_tests.rs"]
mod tests;
