//! Checkpoint replacement under the journal's retained writer lock.
use std::path::{Path, PathBuf};

use gatel_plan::{ConfigurationCapture, Registry};

use super::super::{MAX_LOG, checkpoint, recover};
use super::{File, OpenOptions, Read, Seek, SeekFrom, Store, Write, invalid, io, private};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Written,
    Synced,
    BeforeRename,
    Renamed,
    DirectorySynced,
}

fn existing(path: &Path) -> io::Result<Option<File>> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if !metadata.is_file() => {
            return Err(invalid("checkpoint must be a regular file, not a link"));
        }
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    }
    let file = File::open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(invalid("checkpoint handle is not a regular file"));
    }
    private(&metadata)?;
    if metadata.len() > checkpoint::MAX_BYTES as u64 {
        return Err(invalid("checkpoint exceeds size limit"));
    }
    Ok(Some(file))
}

struct Staged(Option<PathBuf>);
impl Drop for Staged {
    fn drop(&mut self) {
        if let Some(path) = &self.0 {
            let _ = std::fs::remove_file(path);
        }
    }
}

impl Store {
    fn checkpoint_log(&mut self) -> io::Result<Vec<u8>> {
        if self.poisoned {
            return Err(invalid("journal writer is poisoned; reopen and inspect outcome"));
        }
        self.poisoned = true;
        if self.file.metadata()?.len() > MAX_LOG as u64 {
            return Err(invalid("journal exceeds size limit"));
        }
        self.file.seek(SeekFrom::Start(0))?;
        let mut bytes = Vec::new();
        Read::by_ref(&mut self.file)
            .take(MAX_LOG as u64 + 1)
            .read_to_end(&mut bytes)?;
        let observed =
            recover(&bytes, 0).map_err(|_| invalid("invalid journal during checkpoint inspection"))?;
        if observed.sequence != self.sequence
            || observed.valid_bytes != self.valid_bytes
            || observed.text != self.text.as_deref()
            || observed.incomplete_tail != self.incomplete_tail
        {
            return Err(invalid("journal changed outside its writer"));
        }
        self.poisoned = false;
        Ok(bytes)
    }

    pub(in super::super) fn load_checkpoint(
        &mut self,
        instance: &str,
        registry: &Registry,
    ) -> io::Result<Option<checkpoint::Checkpoint>> {
        let log = self.checkpoint_log()?;
        self.load_checkpoint_from(instance, registry, &log)
    }

    fn load_checkpoint_from(
        &self,
        instance: &str,
        registry: &Registry,
        log: &[u8],
    ) -> io::Result<Option<checkpoint::Checkpoint>> {
        self.checkpoint_bytes()?
            .map(|bytes| checkpoint::decode(&bytes, instance, registry, log))
            .transpose()
    }

    fn checkpoint_bytes(&self) -> io::Result<Option<Vec<u8>>> {
        let Some(file) = existing(&self.directory.join("checkpoint.bin"))? else {
            return Ok(None);
        };
        let mut bytes = Vec::new();
        file.take(checkpoint::MAX_BYTES as u64 + 1)
            .read_to_end(&mut bytes)?;
        Ok(Some(bytes))
    }

    pub(in super::super) fn save_checkpoint<R>(
        &mut self,
        instance: &str,
        registry: &Registry,
        capture: &ConfigurationCapture<R>,
    ) -> io::Result<()> {
        self.save_checkpoint_with(instance, registry, capture, |_| Ok(()))
    }

    fn save_checkpoint_with<R>(
        &mut self,
        instance: &str,
        registry: &Registry,
        capture: &ConfigurationCapture<R>,
        mut phase: impl FnMut(Phase) -> io::Result<()>,
    ) -> io::Result<()> {
        self.require_writable()?;
        let log = self.checkpoint_log()?;
        let bytes = checkpoint::encode(instance, registry, &log, capture)?;
        // Refuse silently replacing corrupt, foreign, incompatible or unsafe
        // checkpoint material. Migration/recovery must be a separate operation.
        if let Some(previous) = self.checkpoint_bytes()? {
            let _ = checkpoint::decode(&previous, instance, registry, &log)?;
            // Compare the complete validated envelope, not just generation or
            // plan identity: observed provenance and log prefix also matter.
            // Retain all reads/checks above; only staging/sync/rename are elided.
            if previous == bytes {
                return Ok(());
            }
        }
        let target = self.directory.join("checkpoint.bin");
        let path = self
            .directory
            .join(format!("checkpoint-{}.tmp", gatel_model::BootId::generate()));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        self.poisoned = true;
        let mut file = options.open(&path)?;
        let mut staged = Staged(Some(path));
        // Explicitly close before rename (including on errors) for Windows.
        let write = (|| {
            file.write_all(&bytes)?;
            phase(Phase::Written)?;
            file.sync_all()?;
            phase(Phase::Synced)
        })();
        drop(file);
        write?;
        phase(Phase::BeforeRename)?;
        std::fs::rename(staged.0.as_ref().unwrap(), &target)?;
        staged.0 = None;
        phase(Phase::Renamed)?;
        #[cfg(unix)]
        File::open(&self.directory)?.sync_all()?;
        phase(Phase::DirectorySynced)?;
        self.poisoned = false;
        Ok(())
    }
}

#[cfg(test)]
#[path = "admin_checkpoint_store_tests.rs"]
mod tests;

#[path = "admin_journal_compact.rs"]
mod compact;
