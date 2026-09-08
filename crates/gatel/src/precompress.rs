//! Offline asset preparation. No configuration, listener or implicit shell.
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicU64, Ordering};

use async_compression::tokio::bufread::{BrotliEncoder, GzipEncoder, ZstdEncoder};
use tokio::io::AsyncReadExt;

#[path = "precompress_timing.rs"]
mod timing;
use timing::Phase;

#[derive(Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum Encoding {
    #[value(alias = "gz")]
    Gzip,
    #[value(alias = "br")]
    Brotli,
    #[value(alias = "zst")]
    Zstd,
}
impl Encoding {
    fn suffix(self) -> &'static str {
        match self {
            Self::Gzip => ".gz",
            Self::Brotli => ".br",
            Self::Zstd => ".zst",
        }
    }
}
#[derive(clap::Args)]
pub(crate) struct Options {
    /// Directory to process. Use an offline, trusted tree without concurrent writers.
    root: PathBuf,
    #[arg(
        short,
        long,
        value_enum,
        value_delimiter = ',',
        default_value = "gzip,brotli,zstd"
    )]
    encodings: Vec<Encoding>,
    #[arg(short, long, value_delimiter = ',', default_value = "html,css,js,json,xml,svg,txt,wasm", value_parser = extension)]
    types: Vec<String>,
    /// Maximum source bytes per file (also enforced while reading).
    #[arg(long, default_value = "67108864", value_parser = clap::value_parser!(u64).range(1..=1073741824))]
    max_bytes: u64,
    /// List planned outputs without reading bodies or writing files.
    #[arg(long)]
    dry_run: bool,
}
fn extension(value: &str) -> Result<String, String> {
    let value = value.trim();
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_alphanumeric()) {
        return Err("extensions must be nonempty ASCII letters/digits without a leading dot".into());
    }
    Ok(value.to_ascii_lowercase())
}

pub(crate) fn run(mut options: Options) -> ExitCode {
    let mut unique = Vec::new();
    options.encodings.retain(|encoding| {
        if unique.contains(encoding) {
            false
        } else {
            unique.push(*encoding);
            true
        }
    });
    let result = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| error.to_string())
        .and_then(|runtime| runtime.block_on(process(&options)));
    match result {
        Ok(count) => {
            if writeln!(
                io::stdout().lock(),
                "{} {count} compressed variant(s)",
                if options.dry_run { "planned" } else { "wrote" }
            )
            .is_err()
            {
                return ExitCode::from(super::RUNTIME_FAILURE);
            }
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("gatel: precompress: {error}");
            ExitCode::from(super::RUNTIME_FAILURE)
        }
    }
}
fn link(metadata: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if metadata.file_attributes() & 0x400 != 0 {
            return true;
        }
    }
    metadata.file_type().is_symlink()
}
fn regular(path: &Path) -> io::Result<fs::Metadata> {
    if path.canonicalize()? != path {
        return Err(io::Error::other("source path changed or traverses a link"));
    }
    let metadata = fs::symlink_metadata(path)?;
    if link(&metadata) || !metadata.is_file() {
        return Err(io::Error::other("expected a regular, non-link file"));
    }
    Ok(metadata)
}
fn destination(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
        Ok(metadata) if metadata.is_file() && !link(&metadata) => Ok(()),
        Ok(_) => Err(io::Error::other("output is not a regular, non-link file")),
    }
}
fn sidecar(source: &Path, encoding: Encoding) -> PathBuf {
    let mut name = source.as_os_str().to_os_string();
    name.push(encoding.suffix());
    name.into()
}
fn files(root: &Path, types: &[String]) -> io::Result<Vec<PathBuf>> {
    let mut directories = vec![(root.to_path_buf(), 0)];
    let mut result = Vec::new();
    let mut entries_seen = 0;
    while let Some((directory, depth)) = directories.pop() {
        if depth > 64 {
            return Err(io::Error::other("directory depth exceeds 64"));
        }
        let metadata = fs::symlink_metadata(&directory)?;
        if link(&metadata) || !metadata.is_dir() {
            return Err(io::Error::other("directory changed during traversal"));
        }
        for entry in fs::read_dir(directory)? {
            entries_seen += 1;
            if entries_seen > 100_000 {
                return Err(io::Error::other("tree exceeds 100000 entries"));
            }
            let entry = entry?;
            if entry.file_name().to_string_lossy().starts_with('.') {
                continue;
            }
            let path = entry.path();
            let metadata = fs::symlink_metadata(&path)?;
            if link(&metadata) {
                continue;
            }
            if metadata.is_dir() {
                directories.push((path, depth + 1));
            } else if metadata.is_file() {
                let Some(ext) = path.extension().and_then(|ext| ext.to_str()) else {
                    continue;
                };
                if ["gz", "br", "zst"]
                    .iter()
                    .any(|suffix| ext.eq_ignore_ascii_case(suffix))
                {
                    continue;
                }
                if types.iter().any(|wanted| ext.eq_ignore_ascii_case(wanted)) {
                    result.push(path);
                }
            }
        }
    }
    result.sort();
    Ok(result)
}
async fn process(options: &Options) -> Result<usize, String> {
    let scan = Phase::start("scan");
    let metadata = fs::symlink_metadata(&options.root).map_err(|error| error.to_string())?;
    if link(&metadata) || !metadata.is_dir() {
        return Err("root must be a non-link directory".into());
    }
    let root = options.root.canonicalize().map_err(|error| error.to_string())?;
    let files = files(&root, &options.types).map_err(|error| error.to_string())?;
    drop(scan);
    let mut count = 0;
    for path in files {
        let result: io::Result<()> = async {
            let read = Phase::start("read");
            let metadata = regular(&path)?;
            if metadata.len() > options.max_bytes {
                return Err(io::Error::other("source exceeds --max-bytes"));
            }
            let mut bytes = Vec::new();
            if !options.dry_run {
                fs::File::open(&path)?
                    .take(options.max_bytes + 1)
                    .read_to_end(&mut bytes)?;
                if bytes.len() as u64 > options.max_bytes {
                    return Err(io::Error::other("source grew beyond --max-bytes"));
                }
            }
            drop(read);
            for &encoding in &options.encodings {
                let output = sidecar(&path, encoding);
                destination(&output)?;
                if !options.dry_run {
                    publish(&path, &metadata, &output, &bytes, encoding).await?;
                }
                count += 1;
                let _stdout = Phase::start("stdout");
                writeln!(io::stdout().lock(), "{} -> {}", path.display(), output.display())?;
            }
            Ok(())
        }
        .await;
        result.map_err(|error| format!("{}: {error}", path.display()))?;
    }
    Ok(count)
}
struct Staged(PathBuf);
impl Drop for Staged {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

async fn publish(
    source: &Path,
    metadata: &fs::Metadata,
    output: &Path,
    bytes: &[u8],
    encoding: Encoding,
) -> io::Result<()> {
    let staging = Phase::start("staging");
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let parent = output
        .parent()
        .ok_or_else(|| io::Error::other("output has no parent"))?;
    let mut attempts = 0;
    let (staged, mut file) = loop {
        attempts += 1;
        if attempts > 32 {
            return Err(io::Error::other("cannot allocate a unique staging file"));
        }
        let path = parent.join(format!(
            ".gatel-precompress-{}-{}.tmp",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let mut open = fs::OpenOptions::new();
        open.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            open.mode(0o600);
        }
        match open.open(&path) {
            Ok(file) => break (Staged(path), file),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    };
    drop(staging);
    let encode = Phase::start("encode_write");
    let mut encoder: std::pin::Pin<Box<dyn tokio::io::AsyncRead + '_>> = match encoding {
        Encoding::Gzip => Box::pin(GzipEncoder::new(bytes)),
        Encoding::Brotli => Box::pin(BrotliEncoder::new(bytes)),
        Encoding::Zstd => Box::pin(ZstdEncoder::new(bytes)),
    };
    let mut buffer = [0; 8192];
    loop {
        let size = encoder.read(&mut buffer).await?;
        if size == 0 {
            break;
        }
        file.write_all(&buffer[..size])?;
    }
    drop(encoder);
    drop(encode);
    let _publish = Phase::start("publish");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(metadata.permissions().mode() & 0o666))?;
    }
    // Preserve the input timestamp so freshly built variants remain eligible
    // even when deployment inputs are dated ahead of this machine's clock.
    // Set it before sync/rename: failure must leave the old output untouched.
    file.set_modified(metadata.modified()?)?;
    file.sync_all()?;
    drop(file);
    let current = regular(source)?;
    if current.len() != metadata.len() || current.modified()? != metadata.modified()? {
        return Err(io::Error::other("source changed while compressing"));
    }
    destination(output)?;
    fs::rename(&staged.0, output)?;
    #[cfg(unix)]
    fs::File::open(parent)?.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn source_change_before_commit_preserves_previous_output_and_cleans_staging() {
        let directory = std::env::temp_dir().join(format!("gatel-precompress-commit-{}", std::process::id()));
        fs::create_dir(&directory).unwrap();
        let directory = directory.canonicalize().unwrap();
        let source = directory.join("asset.txt");
        let output = directory.join("asset.txt.gz");
        fs::write(&source, "first").unwrap();
        let metadata = fs::metadata(&source).unwrap();
        fs::write(&output, "previous artifact").unwrap();
        fs::write(&source, "changed length").unwrap();
        assert!(
            publish(&source, &metadata, &output, b"first", Encoding::Gzip)
                .await
                .is_err()
        );
        assert_eq!(fs::read(&output).unwrap(), b"previous artifact");
        assert_eq!(fs::read_dir(&directory).unwrap().count(), 2);
        fs::remove_file(output).unwrap();
        fs::remove_file(source).unwrap();
        fs::remove_dir(directory).unwrap();
    }
}
