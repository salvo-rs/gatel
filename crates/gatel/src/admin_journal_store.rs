//! Synchronous append-only store. The future host adapter must run it in bounded
//! blocking work and coordinate persistence with configuration transactions.
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;

use super::{Fingerprint, MAX_LOG, Recovered, checksum, encode, recover, recover_each};

#[path = "admin_checkpoint_store.rs"]
mod checkpoint;

pub(super) struct Store {
    file: File,
    // Stable lock inode is never renamed/unlinked by this implementation.
    _lock: WriterLock,
    directory: std::path::PathBuf,
    sequence: u64,
    text: Option<String>,
    valid_bytes: usize,
    incomplete_tail: bool,
    tail_identity: Option<Fingerprint>,
    poisoned: bool,
    writable: bool,
}

/// Ownership ends here, not when the last inherited/duplicated descriptor
/// happens to close. In particular, CLOEXEC does not close a forked descriptor
/// until exec, and an unrelated child must not prolong the parent's writer lock.
struct WriterLock {
    file: File,
    owner: u32,
}

impl WriterLock {
    fn acquire(file: File) -> io::Result<Self> {
        file.try_lock()
            .map_err(|error| io::Error::other(format!("cannot acquire journal writer lock: {error}")))?;
        Ok(Self {
            file,
            owner: std::process::id(),
        })
    }
}

impl Drop for WriterLock {
    fn drop(&mut self) {
        // A raw-fork child is not the owner and must not unlock the parent's
        // shared open file description if it unwinds without exec.
        if self.owner == std::process::id() {
            let _ = self.file.unlock();
        }
        // Closing is still the fallback on unlock failure or process kill.
    }
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

fn private(metadata: &std::fs::Metadata) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "journal directory/files must be private",
            ));
        }
    }
    #[cfg(not(unix))]
    let _ = metadata; // ACL policy remains the operator's responsibility.
    Ok(())
}

fn open_file(path: &Path, create: bool, writable: bool) -> io::Result<File> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if !metadata.is_file() => {
            return Err(invalid("journal paths must be regular files, not links"));
        }
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let mut options = OpenOptions::new();
    options.read(true).write(writable).create(create).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(invalid("journal handle must be a regular file"));
    }
    private(&metadata)?;
    Ok(file)
}

impl Store {
    /// Existing private directory only. Parent paths must be trusted; these
    /// checks are not a race-proof sandbox against a malicious local owner.
    pub(super) fn open(directory: &Path) -> io::Result<Self> {
        Self::open_replaying(directory, |_, _| Ok(()))
    }

    /// Replay before returning a writable store, under the same writer lock.
    /// The caller must discard its staged replay state when this fails.
    pub(super) fn open_replaying(
        directory: &Path,
        visit: impl FnMut(u64, &str) -> Result<(), super::Error>,
    ) -> io::Result<Self> {
        Self::open_mode(directory, visit, true)
    }

    /// Inspection never creates files or opens the log for writing. The stable
    /// existing lock is still opened read/write for Windows exclusive locking.
    pub(super) fn open_existing_replaying(
        directory: &Path,
        visit: impl FnMut(u64, &str) -> Result<(), super::Error>,
    ) -> io::Result<Self> {
        Self::open_mode(directory, visit, false)
    }

    fn open_mode(
        directory: &Path,
        visit: impl FnMut(u64, &str) -> Result<(), super::Error>,
        create: bool,
    ) -> io::Result<Self> {
        let directory = std::path::absolute(directory)?;
        let metadata = std::fs::symlink_metadata(&directory)?;
        if !metadata.is_dir() {
            return Err(invalid("journal directory must not be a link"));
        }
        private(&metadata)?;
        let lock = WriterLock::acquire(open_file(&directory.join("writer.lock"), create, true)?)?;
        let mut file = open_file(&directory.join("intent.log"), create, create)?;
        if file.metadata()?.len() > MAX_LOG as u64 {
            return Err(invalid("journal exceeds size limit"));
        }
        let mut bytes = Vec::new();
        Read::by_ref(&mut file)
            .take(MAX_LOG as u64 + 1)
            .read_to_end(&mut bytes)?;
        let recovered = recover_each(&bytes, 0, visit).map_err(|error| invalid(error.to_string()))?;
        // Make newly created empty files durable before callers can append.
        if create {
            lock.file.sync_all()?;
            file.sync_all()?;
            #[cfg(unix)]
            File::open(&directory)?.sync_all()?;
        }
        Ok(Self {
            file,
            _lock: lock,
            directory: directory.to_owned(),
            sequence: recovered.sequence,
            text: recovered.text.map(str::to_owned),
            valid_bytes: recovered.valid_bytes,
            incomplete_tail: recovered.incomplete_tail,
            tail_identity: recovered.incomplete_tail.then(|| tail_identity(&bytes)),
            poisoned: false,
            writable: create,
        })
    }

    pub(super) fn recovered(&self) -> Recovered<'_> {
        Recovered {
            sequence: self.sequence,
            text: self.text.as_deref(),
            valid_bytes: self.valid_bytes,
            incomplete_tail: self.incomplete_tail,
        }
    }

    pub(super) fn remaining_capacity(&self) -> usize {
        MAX_LOG - self.valid_bytes
    }

    fn require_writable(&self) -> io::Result<()> {
        if !self.writable {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "journal is inspection-only",
            ));
        }
        Ok(())
    }

    /// Explicitly discard only the incomplete suffix observed when opening.
    /// All writers must honor the retained lock. This is not automatic repair
    /// and cannot distinguish unfinished writes from later loss of committed bytes.
    pub(super) fn discard_incomplete_tail(&mut self) -> io::Result<usize> {
        self.discard_tail_with(|file, length| {
            file.set_len(length)?;
            file.sync_all()
        })
    }

    fn discard_tail_with(
        &mut self,
        truncate: impl FnOnce(&mut File, u64) -> io::Result<()>,
    ) -> io::Result<usize> {
        self.require_writable()?;
        if self.poisoned {
            return Err(invalid("journal writer is poisoned; reopen and inspect outcome"));
        }
        if !self.incomplete_tail {
            return Ok(0);
        }
        // Any failed inspection or mutation requires reopening, including an
        // external rewrite with unchanged length. Never retry from stale state.
        self.poisoned = true;
        if self.file.metadata()?.len() > MAX_LOG as u64 {
            return Err(invalid("journal exceeds size limit"));
        }
        self.file.seek(SeekFrom::Start(0))?;
        let mut bytes = Vec::new();
        Read::by_ref(&mut self.file)
            .take(MAX_LOG as u64 + 1)
            .read_to_end(&mut bytes)?;
        let recovered = recover(&bytes, 0).map_err(|error| invalid(error.to_string()))?;
        if Some(tail_identity(&bytes)) != self.tail_identity
            || !recovered.incomplete_tail
            || recovered.valid_bytes != self.valid_bytes
            || recovered.sequence != self.sequence
            || recovered.text != self.text.as_deref()
        {
            return Err(invalid("journal changed since incomplete tail was observed"));
        }
        let removed = bytes.len() - self.valid_bytes;
        truncate(&mut self.file, self.valid_bytes as u64)?;
        self.incomplete_tail = false;
        self.tail_identity = None;
        self.poisoned = false;
        Ok(removed)
    }

    /// Success only after write_all + sync_all. Failure after mutation begins
    /// has unknown durability and poisons this handle; never continue appending.
    pub(super) fn append(&mut self, text: &str) -> io::Result<u64> {
        self.append_with(text, |file, bytes| {
            file.write_all(bytes)?;
            file.sync_all()
        })
    }

    fn append_with(
        &mut self,
        text: &str,
        persist: impl FnOnce(&mut File, &[u8]) -> io::Result<()>,
    ) -> io::Result<u64> {
        self.require_writable()?;
        if self.poisoned {
            return Err(invalid("journal writer is poisoned; reopen and inspect outcome"));
        }
        if self.incomplete_tail {
            return Err(invalid(
                "journal has an incomplete tail; explicit recovery required",
            ));
        }
        let sequence = self
            .sequence
            .checked_add(1)
            .ok_or_else(|| invalid("journal sequence exhausted"))?;
        let record = encode(sequence, text).map_err(|error| invalid(error.to_string()))?;
        if record.len() > MAX_LOG - self.valid_bytes {
            return Err(invalid("journal capacity exhausted; checkpoint required"));
        }
        self.poisoned = true;
        if self.file.metadata()?.len() != self.valid_bytes as u64 {
            return Err(invalid("journal length changed outside its writer"));
        }
        self.file.seek(SeekFrom::End(0))?;
        persist(&mut self.file, &record)?;
        self.sequence = sequence;
        self.text = Some(text.to_owned());
        self.valid_bytes += record.len();
        self.poisoned = false;
        Ok(sequence)
    }
}

fn tail_identity(bytes: &[u8]) -> Fingerprint {
    checksum("gatel.admin-intent.tail-observation.v1", bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Directory(std::path::PathBuf);
    impl Directory {
        fn new() -> Self {
            let path =
                std::env::temp_dir().join(format!("gatel-journal-{}", gatel_model::BootId::generate()));
            let builder = std::fs::DirBuilder::new();
            #[cfg(unix)]
            let builder = {
                use std::os::unix::fs::DirBuilderExt;
                let mut builder = builder;
                builder.mode(0o700);
                builder
            };
            builder.create(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Directory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn exclusive_writer_reopens_synced_records_and_empty_intent() {
        let directory = Directory::new();
        let mut store = Store::open(&directory.0).unwrap();
        assert!(Store::open(&directory.0).is_err());
        assert_eq!(store.append("first; ").unwrap(), 1);
        assert_eq!(store.append("").unwrap(), 2);
        drop(store);
        let store = Store::open(&directory.0).unwrap();
        assert_eq!(store.recovered().text, Some(""));
        assert_eq!(store.recovered().sequence, 2);
    }

    #[cfg(unix)]
    #[test]
    fn owner_drop_releases_lock_even_while_a_descriptor_copy_survives() {
        let directory = Directory::new();
        let store = Store::open(&directory.0).unwrap();
        // dup and fork share the open file description backing flock. Keeping
        // a clone deterministically models a child before close-on-exec runs.
        let inherited = store._lock.file.try_clone().unwrap();
        assert!(Store::open(&directory.0).is_err());
        drop(store);
        let reopened = Store::open(&directory.0).expect("owner drop must release its writer lock");
        assert!(Store::open(&directory.0).is_err());
        drop(inherited);
        assert!(Store::open(&directory.0).is_err());
        drop(reopened);
        assert!(Store::open(&directory.0).is_ok());
    }

    #[test]
    fn killed_process_releases_lock_and_preserves_synced_prefix() {
        use std::process::{Child, Command, Stdio};
        use std::time::{Duration, Instant};
        const DIRECTORY: &str = "GATEL_JOURNAL_CRASH_DIRECTORY";
        const MODE: &str = "GATEL_JOURNAL_CRASH_MODE";
        if let Some(directory) = std::env::var_os(DIRECTORY) {
            let directory = Path::new(&directory);
            eprintln!("crash fixture: opening journal store");
            let mut store = Store::open(directory).unwrap();
            eprintln!("crash fixture: appending synced prefix");
            store.append("committed;").unwrap();
            if std::env::var(MODE).unwrap() == "partial" {
                assert!(
                    store
                        .append_with("unfinished;", |file, bytes| {
                            eprintln!("crash fixture: writing partial record");
                            file.write_all(&bytes[..bytes.len() - 1])?;
                            // Stabilize the partial bytes so this fixture tests recovery
                            // classification, not whether the OS happened to flush them.
                            eprintln!("crash fixture: syncing partial record");
                            file.sync_all()?;
                            Err(io::Error::other("injected partial append"))
                        })
                        .is_err()
                );
            }
            eprintln!("crash fixture: signaling ready");
            std::fs::write(directory.join("ready"), b"holding-lock").unwrap();
            loop {
                std::thread::park();
            }
        }
        struct Process(Child);
        impl Drop for Process {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        for mode in ["synced", "partial"] {
            let directory = Directory::new();
            let log = std::fs::File::create(directory.0.join("child.log")).unwrap();
            let mut child = Process(Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "admin::journal::store::tests::killed_process_releases_lock_and_preserves_synced_prefix", "--nocapture"])
                .env(DIRECTORY, &directory.0).env(MODE, mode)
                .stdin(Stdio::null()).stdout(Stdio::from(log.try_clone().unwrap())).stderr(Stdio::from(log))
                .spawn().unwrap());
            let deadline = Instant::now() + Duration::from_secs(10);
            while !directory.0.join("ready").exists() {
                let status = child.0.try_wait().unwrap();
                assert!(
                    status.is_none() && Instant::now() < deadline,
                    "journal child mode={mode} pid={} status={status:?}: {}",
                    child.0.id(),
                    std::fs::read_to_string(directory.0.join("child.log")).unwrap_or_default()
                );
                std::thread::sleep(Duration::from_millis(10));
            }
            let error = match Store::open(&directory.0) {
                Ok(_) => panic!("second process acquired active writer lock"),
                Err(error) => error,
            };
            assert!(error.to_string().contains("cannot acquire journal writer lock"));
            child.0.kill().unwrap();
            let status = child.0.wait().unwrap();
            assert!(!status.success());
            #[cfg(unix)]
            {
                use std::os::unix::process::ExitStatusExt;
                assert_eq!(status.signal(), Some(9));
            }
            let before = std::fs::read(directory.0.join("intent.log")).unwrap();
            let mut store = Store::open(&directory.0).unwrap();
            assert_eq!(store.recovered().sequence, 1);
            assert_eq!(store.recovered().text, Some("committed;"));
            assert_eq!(store.recovered().incomplete_tail, mode == "partial");
            if mode == "partial" {
                assert!(store.append("must-not-follow-tail;").is_err());
                assert_eq!(std::fs::read(directory.0.join("intent.log")).unwrap(), before);
            } else {
                assert_eq!(store.append("after-restart;").unwrap(), 2);
            }
        }
    }

    #[test]
    fn partial_write_poisons_handle_and_reopen_preserves_tail_without_truncation() {
        let directory = Directory::new();
        let mut store = Store::open(&directory.0).unwrap();
        store.append("first;").unwrap();
        assert!(
            store
                .append_with("second;", |file, bytes| {
                    file.write_all(&bytes[..bytes.len() - 1])?;
                    Err(io::Error::other("injected partial write"))
                })
                .is_err()
        );
        assert!(
            store
                .append("third;")
                .unwrap_err()
                .to_string()
                .contains("poisoned")
        );
        drop(store);
        let before = std::fs::read(directory.0.join("intent.log")).unwrap();
        let mut store = Store::open(&directory.0).unwrap();
        assert_eq!(store.recovered().text, Some("first;"));
        assert!(store.recovered().incomplete_tail);
        assert!(store.append("third;").is_err());
        assert_eq!(std::fs::read(directory.0.join("intent.log")).unwrap(), before);
    }

    #[test]
    fn explicit_tail_discard_preserves_prefix_and_resumes_sequence() {
        // Every cut includes partial magic, header and UTF-8 body, plus an
        // empty log prefix. Opening alone must never change the fixture.
        let second = encode(2, "配置;").unwrap();
        for cut in 1..second.len() {
            let directory = Directory::new();
            let mut store = Store::open(&directory.0).unwrap();
            store.append("first;").unwrap();
            store.file.write_all(&second[..cut]).unwrap();
            drop(store);
            let mut store = Store::open(&directory.0).unwrap();
            assert_eq!(store.discard_incomplete_tail().unwrap(), cut);
            assert_eq!(store.recovered().text, Some("first;"));
            assert!(!store.recovered().incomplete_tail);
            assert_eq!(store.discard_incomplete_tail().unwrap(), 0);
            assert_eq!(store.append("replacement;").unwrap(), 2);
            drop(store);
            assert_eq!(
                Store::open(&directory.0).unwrap().recovered().text,
                Some("replacement;")
            );
        }
        let directory = Directory::new();
        let mut store = Store::open(&directory.0).unwrap();
        store.file.write_all(b"GAT").unwrap();
        drop(store);
        let mut store = Store::open(&directory.0).unwrap();
        assert_eq!(store.discard_incomplete_tail().unwrap(), 3);
        assert_eq!(store.recovered().text, None);
        assert_eq!(store.append("").unwrap(), 1);
    }

    #[test]
    fn tail_discard_rejects_same_length_rewrites_without_mutation() {
        for rewrite_prefix in [false, true] {
            let directory = Directory::new();
            let mut store = Store::open(&directory.0).unwrap();
            store.append("first;").unwrap();
            let partial = encode(2, "unfinished;").unwrap();
            store.file.write_all(&partial[..partial.len() - 1]).unwrap();
            drop(store);
            let mut store = Store::open(&directory.0).unwrap();
            let path = directory.0.join("intent.log");
            let mut changed = std::fs::read(&path).unwrap();
            if rewrite_prefix {
                let replacement = encode(1, "other;").unwrap();
                changed[..replacement.len()].copy_from_slice(&replacement);
            } else {
                *changed.last_mut().unwrap() ^= 1;
            }
            // Both changes still parse as a valid prefix + unfinished tail.
            assert!(recover(&changed, 0).unwrap().incomplete_tail);
            std::fs::write(&path, &changed).unwrap();
            assert!(store.discard_incomplete_tail().is_err());
            assert_eq!(std::fs::read(&path).unwrap(), changed);
            assert!(
                store
                    .discard_incomplete_tail()
                    .unwrap_err()
                    .to_string()
                    .contains("poisoned")
            );
            assert!(store.append("next;").is_err());
        }
    }

    #[test]
    fn failed_tail_sync_poisons_even_when_truncation_is_visible() {
        let directory = Directory::new();
        let mut store = Store::open(&directory.0).unwrap();
        store.append("first;").unwrap();
        store.file.write_all(b"GAT").unwrap();
        drop(store);
        let mut store = Store::open(&directory.0).unwrap();
        assert!(
            store
                .discard_tail_with(|file, length| {
                    file.set_len(length)?;
                    Err(io::Error::other("injected truncate sync failure"))
                })
                .is_err()
        );
        assert!(store.recovered().incomplete_tail);
        assert!(store.append("next;").is_err());
        assert!(store.discard_incomplete_tail().is_err());
        drop(store);
        let mut store = Store::open(&directory.0).unwrap();
        assert!(!store.recovered().incomplete_tail);
        assert_eq!(store.append("next;").unwrap(), 2);
    }

    #[test]
    fn failed_sync_may_leave_complete_record_but_cannot_return_success() {
        let directory = Directory::new();
        let mut store = Store::open(&directory.0).unwrap();
        assert!(
            store
                .append_with("unknown;", |file, bytes| {
                    file.write_all(bytes)?;
                    Err(io::Error::other("injected sync failure"))
                })
                .is_err()
        );
        assert_eq!(store.recovered().sequence, 0);
        assert!(store.append("next;").is_err());
        drop(store);
        assert_eq!(
            Store::open(&directory.0).unwrap().recovered().text,
            Some("unknown;")
        );
    }

    #[test]
    fn corrupt_or_nonregular_files_are_refused_without_overwriting() {
        let directory = Directory::new();
        let store = Store::open(&directory.0).unwrap();
        drop(store);
        let path = directory.0.join("intent.log");
        std::fs::write(&path, b"corrupt").unwrap();
        assert!(Store::open(&directory.0).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"corrupt");
        let other = Directory::new();
        std::fs::create_dir(other.0.join("intent.log")).unwrap();
        assert!(Store::open(&other.0).is_err());
    }

    #[test]
    fn invalid_input_does_not_poison_and_oversized_file_is_not_loaded() {
        let directory = Directory::new();
        let mut store = Store::open(&directory.0).unwrap();
        assert!(store.append(&"x".repeat(super::super::MAX_PAYLOAD + 1)).is_err());
        assert_eq!(store.append("valid;").unwrap(), 1);
        drop(store);
        let path = directory.0.join("intent.log");
        OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(MAX_LOG as u64 + 1)
            .unwrap();
        assert!(Store::open(&directory.0).is_err());
        assert_eq!(std::fs::metadata(path).unwrap().len(), MAX_LOG as u64 + 1);
    }

    #[cfg(unix)]
    #[test]
    fn unix_private_permissions_and_symlink_rejection_are_enforced() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let directory = Directory::new();
        std::fs::set_permissions(&directory.0, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(Store::open(&directory.0).is_err());
        std::fs::set_permissions(&directory.0, std::fs::Permissions::from_mode(0o700)).unwrap();
        drop(Store::open(&directory.0).unwrap());
        let log = directory.0.join("intent.log");
        std::fs::set_permissions(&log, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(Store::open(&directory.0).is_err());
        let other = Directory::new();
        symlink(&log, other.0.join("intent.log")).unwrap();
        assert!(Store::open(&other.0).is_err());
        assert_eq!(std::fs::metadata(log).unwrap().len(), 0);
    }
}
