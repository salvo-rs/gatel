//! Explicit offline history compaction; no automatic deployment switch.
use super::*;

#[derive(clap::Args)]
#[group(id = "JournalCompactOptions")]
pub(crate) struct Options {
    #[command(flatten)]
    source: super::Options,
    /// New, nonexistent directory under a trusted private parent. Source is retained.
    #[arg(long)]
    destination: PathBuf,
}

pub(crate) fn run(options: Options) -> ExitCode {
    let result = (|| {
        let registry = crate::plugins::registry().map_err(io::Error::other)?;
        let mut journal = Journal::open_existing(&options.source.directory, &options.source.instance)?;
        journal.compact_to(&registry, &options.destination)
    })();
    if result.is_err() {
        eprintln!(
            "gatel: journal compaction failed; inspect source and destination before retrying; source was not modified"
        );
        return ExitCode::from(crate::RUNTIME_FAILURE);
    }
    if writeln!(
        io::stdout().lock(),
        "ok: compacted checkpoint exported; source retained; deployment switch is manual"
    )
    .is_err()
    {
        return ExitCode::from(crate::RUNTIME_FAILURE);
    }
    ExitCode::SUCCESS
}
