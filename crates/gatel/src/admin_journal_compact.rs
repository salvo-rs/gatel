//! Offline export: publish a complete new directory, never replace the live pair.
use super::*;

impl Store {
    pub(in super::super::super) fn compact_to(
        &mut self,
        instance: &str,
        registry: &Registry,
        destination: &Path,
    ) -> io::Result<()> {
        let log = self.checkpoint_log()?;
        let bytes = self
            .checkpoint_bytes()?
            .ok_or_else(|| invalid("checkpoint required"))?;
        let (checkpoint, log) = checkpoint::compact(&bytes, instance, registry, &log)?;
        publish(destination, &checkpoint, &log)
    }
}

fn absent(path: &Path) -> io::Result<()> {
    match std::fs::symlink_metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "destination already exists",
        )),
    }
}

struct Staging(Option<PathBuf>);
impl Drop for Staging {
    fn drop(&mut self) {
        // Only our three fixed files; never recurse through unexpected contents.
        let Some(path) = &self.0 else { return };
        for name in ["writer.lock", "intent.log", "checkpoint.bin"] {
            let _ = std::fs::remove_file(path.join(name));
        }
        let _ = std::fs::remove_dir(path);
    }
}

fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

fn publish(destination: &Path, checkpoint: &[u8], log: &[u8]) -> io::Result<()> {
    publish_with(destination, checkpoint, log, |_| Ok(()))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Phase {
    LogSynced,
    CheckpointSynced,
    BeforePublish,
    Published,
}

pub(super) fn publish_with(
    destination: &Path,
    checkpoint: &[u8],
    log: &[u8],
    mut phase: impl FnMut(Phase) -> io::Result<()>,
) -> io::Result<()> {
    let destination = std::path::absolute(destination)?;
    absent(&destination)?;
    let parent = destination
        .parent()
        .ok_or_else(|| invalid("destination needs a parent"))?;
    let metadata = std::fs::symlink_metadata(parent)?;
    if !metadata.is_dir() {
        return Err(invalid("destination parent must be a directory, not a link"));
    }
    private(&metadata)?;
    let staging = parent.join(format!(".gatel-compact-{}", gatel_model::BootId::generate()));
    let builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    let builder = {
        use std::os::unix::fs::DirBuilderExt;
        let mut builder = builder;
        builder.mode(0o700);
        builder
    };
    builder.create(&staging)?;
    let mut cleanup = Staging(Some(staging.clone()));
    write_private(&staging.join("writer.lock"), &[])?;
    write_private(&staging.join("intent.log"), log)?;
    phase(Phase::LogSynced)?;
    write_private(&staging.join("checkpoint.bin"), checkpoint)?;
    phase(Phase::CheckpointSynced)?;
    #[cfg(unix)]
    File::open(&staging)?.sync_all()?;
    // Parent must be trusted and operators must serialize destination changes.
    // This is not a race-proof filesystem sandbox against a hostile local owner.
    absent(&destination)?;
    phase(Phase::BeforePublish)?;
    std::fs::rename(&staging, &destination)?;
    cleanup.0 = None;
    phase(Phase::Published)?;
    #[cfg(unix)]
    File::open(parent)?.sync_all()?;
    Ok(())
}
