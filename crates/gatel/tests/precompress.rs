use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};

use tokio::io::AsyncReadExt;

#[path = "support/cli_process.rs"]
mod process_output;

#[test]
fn timeout_capture_terminates_and_reaps_a_waiting_child() {
    use std::io::Write;
    use std::time::Duration;
    const FIXTURE: &str = "GATEL_PRECOMPRESS_WAIT_FIXTURE";
    if std::env::var(FIXTURE).as_deref() == Ok("waiting-child") {
        println!("fixture entered wait");
        std::io::stdout().flush().unwrap();
        eprintln!("fixture stderr sentinel");
        std::io::stderr().flush().unwrap();
        loop {
            std::thread::park();
        }
    }
    // Re-enter only this test in its fixture role; no shell, external program,
    // socket handoff or extra ignored test is required.
    let timeout = Duration::from_secs(5);
    let result = process_output::output_with_timeout(
        Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "timeout_capture_terminates_and_reaps_a_waiting_child",
                "--nocapture",
            ])
            .env(FIXTURE, "waiting-child"),
        timeout,
    )
    .unwrap_err();
    assert_ne!(result.pid, std::process::id());
    assert!(result.elapsed >= timeout);
    assert!(!result.output.status.success(), "waiting child exited normally");
    assert!(
        String::from_utf8_lossy(&result.output.stdout).contains("fixture entered wait"),
        "fixture did not reach waiting phase before the deadline: {result:?}"
    );
    assert!(String::from_utf8_lossy(&result.output.stderr).contains("fixture stderr sentinel"));
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(result.output.status.signal(), Some(9));
    }
}

#[test]
fn generated_variants_are_served_by_the_real_file_server() {
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::process::{Child, Stdio};
    use std::time::{Duration, Instant};
    struct Process(Child);
    impl Drop for Process {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let directory = Directory::new();
    fs::write(directory.0.join("asset.txt"), "static content ".repeat(100)).unwrap();
    // Build inputs can carry timestamps ahead of the machine running the tool.
    // A freshly generated sidecar must still pass the server's freshness check.
    fs::File::options()
        .write(true)
        .open(directory.0.join("asset.txt"))
        .unwrap()
        .set_modified(std::time::SystemTime::now() + Duration::from_secs(3600))
        .unwrap();
    assert!(directory.run(&[]).status.success());
    let address = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap();
    let config = directory.0.join("gatel.kdl");
    fs::write(&config, format!("global {{ http \"{address}\" }}\nsite \"*\" {{ route \"/*\" {{ file-server root=\".\" precompressed=#true; }} }}")).unwrap();
    let mut process = Process(
        Command::new(env!("CARGO_BIN_EXE_gatel"))
            .arg("run")
            .arg(config)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if TcpStream::connect(address).is_ok() {
            break;
        }
        assert!(
            process.0.try_wait().unwrap().is_none(),
            "server exited before binding"
        );
        assert!(Instant::now() < deadline, "server did not bind");
        std::thread::sleep(Duration::from_millis(10));
    }
    for (coding, suffix) in [("gzip", "gz"), ("br", "br"), ("zstd", "zst")] {
        let mut stream = TcpStream::connect(address).unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        stream.set_write_timeout(Some(Duration::from_secs(5))).unwrap();
        write!(stream, "GET /asset.txt HTTP/1.1\r\nHost: example.com\r\nAccept-Encoding: {coding}\r\nConnection: close\r\n\r\n").unwrap();
        let mut response = Vec::new();
        stream.read_to_end(&mut response).unwrap();
        let split = response.windows(4).position(|part| part == b"\r\n\r\n").unwrap();
        let head = String::from_utf8_lossy(&response[..split]).to_ascii_lowercase();
        assert!(head.starts_with("http/1.1 200"), "{head}");
        assert!(head.contains(&format!("content-encoding: {coding}")), "{head}");
        assert_eq!(
            &response[split + 4..],
            fs::read(directory.0.join(format!("asset.txt.{suffix}"))).unwrap()
        );
    }
}

struct Directory(PathBuf);

#[test]
fn phase_diagnostics_are_paired_opt_in_and_exclude_asset_names_and_contents() {
    let directory = Directory::new();
    fs::write(directory.0.join("sensitive-name.txt"), "sensitive-content").unwrap();
    let output = directory.run(&[]);
    assert!(output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(!stderr.contains("sensitive-name"));
    assert!(!stderr.contains("sensitive-content"));
    let mut opened = None;
    let mut stages = std::collections::BTreeSet::new();
    for line in stderr.lines() {
        let event: serde_json::Value = serde_json::from_str(line).unwrap();
        let fields = &event["fields"];
        let stage = fields["stage"].as_str().unwrap().to_owned();
        match fields["event"].as_str().unwrap() {
            "precompress_phase_started" => {
                assert!(opened.replace(stage.clone()).is_none());
                stages.insert(stage);
            }
            "precompress_phase_finished" => {
                assert_eq!(opened.take(), Some(stage));
                assert!(fields["elapsed_ms"].as_f64().unwrap() >= 0.0);
            }
            other => panic!("unexpected diagnostic: {other}"),
        }
    }
    assert!(opened.is_none());
    assert_eq!(
        stages,
        ["scan", "read", "staging", "encode_write", "publish", "stdout"]
            .map(str::to_owned)
            .into()
    );
    let quiet = process_output::output(
        Command::new(env!("CARGO_BIN_EXE_gatel"))
            .arg("precompress")
            .arg(&directory.0)
            .env("GATEL_LOG", "gatel=info"),
    );
    assert!(quiet.status.success());
    assert!(quiet.stderr.is_empty());
}

impl Directory {
    fn new() -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "gatel precompress {} {}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path.canonicalize().unwrap())
    }
    fn run(&self, args: &[&str]) -> Output {
        process_output::output(
            Command::new(env!("CARGO_BIN_EXE_gatel"))
                .arg("precompress")
                .arg(&self.0)
                .args(args)
                .env("GATEL_LOG", "gatel::precompress=debug")
                .env("GATEL_LOG_FORMAT", "json"),
        )
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

async fn decoded(bytes: &[u8], suffix: &str) -> Vec<u8> {
    use async_compression::tokio::bufread::{BrotliDecoder, GzipDecoder, ZstdDecoder};
    let mut result = Vec::new();
    macro_rules! read {
        ($decoder:ident) => {
            $decoder::new(bytes).read_to_end(&mut result).await.unwrap()
        };
    }
    match suffix {
        "gz" => read!(GzipDecoder),
        "br" => read!(BrotliDecoder),
        "zst" => read!(ZstdDecoder),
        _ => unreachable!(),
    };
    result
}

#[tokio::test]
async fn real_cli_generates_all_formats_and_updates_them_without_touching_sources() {
    let directory = Directory::new();
    fs::create_dir(directory.0.join("nested")).unwrap();
    let file = directory.0.join("nested/asset.TXT");
    for plain in [
        b"first version\0\xff".repeat(500),
        b"replacement version".repeat(500),
    ] {
        fs::write(&file, &plain).unwrap();
        fs::write(directory.0.join("empty.txt"), []).unwrap();
        let output = directory.run(&[]);
        assert!(output.status.success(), "{output:?}");
        assert!(String::from_utf8_lossy(&output.stdout).contains("wrote 6 compressed variant(s)"));
        assert_eq!(fs::read(&file).unwrap(), plain);
        for suffix in ["gz", "br", "zst"] {
            let variant = directory.0.join(format!("nested/asset.TXT.{suffix}"));
            assert_eq!(
                fs::metadata(&variant).unwrap().modified().unwrap(),
                fs::metadata(&file).unwrap().modified().unwrap()
            );
            let compressed = fs::read(variant).unwrap();
            assert_eq!(decoded(&compressed, suffix).await, plain);
            let empty = fs::read(directory.0.join(format!("empty.txt.{suffix}"))).unwrap();
            assert!(decoded(&empty, suffix).await.is_empty());
        }
        assert_eq!(fs::read_dir(directory.0.join("nested")).unwrap().count(), 4);
    }
}

#[test]
fn dry_run_filtering_aliases_and_bad_arguments_have_explicit_behavior() {
    let directory = Directory::new();
    fs::write(directory.0.join("asset.txt"), "plain").unwrap();
    fs::write(directory.0.join(".hidden.txt"), "secret").unwrap();
    fs::write(directory.0.join("ignored.bin"), "binary").unwrap();
    fs::create_dir(directory.0.join(".private")).unwrap();
    fs::write(directory.0.join(".private/asset.txt"), "secret").unwrap();
    let output = directory.run(&["--dry-run", "-e", "gzip,br,gz"]);
    assert!(output.status.success(), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stdout).contains("planned 2 compressed variant(s)"));
    assert!(!directory.0.join("asset.txt.gz").exists());
    let output = directory.run(&["-e", "gzip,br,gz"]);
    assert!(output.status.success(), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stdout).contains("wrote 2 compressed variant(s)"));
    assert!(!directory.0.join(".hidden.txt.gz").exists());
    assert!(!directory.0.join(".private/asset.txt.gz").exists());
    assert!(!directory.0.join("ignored.bin.gz").exists());
    for args in [
        vec!["-e", "unknown"],
        vec!["-t", "../txt"],
        vec!["--max-bytes", "0"],
    ] {
        assert_eq!(directory.run(&args).status.code(), Some(2));
    }
}

#[test]
fn size_and_output_failures_do_not_destroy_existing_artifacts() {
    let directory = Directory::new();
    fs::write(directory.0.join("asset.txt"), "too large").unwrap();
    fs::write(directory.0.join("asset.txt.gz"), "previous artifact").unwrap();
    assert_eq!(directory.run(&["--max-bytes", "1"]).status.code(), Some(3));
    assert_eq!(
        fs::read(directory.0.join("asset.txt.gz")).unwrap(),
        b"previous artifact"
    );
    fs::create_dir(directory.0.join("asset.txt.br")).unwrap();
    assert_eq!(directory.run(&["-e", "br"]).status.code(), Some(3));
    assert!(directory.0.join("asset.txt.br").is_dir());
    assert_eq!(fs::read_dir(&directory.0).unwrap().count(), 3);
}

#[cfg(unix)]
#[test]
fn variants_preserve_read_permissions_but_not_executable_or_special_bits() {
    use std::os::unix::fs::PermissionsExt;
    let directory = Directory::new();
    for mode in [0o400, 0o444, 0o640, 0o755, 0o6755] {
        let source = directory.0.join(format!("asset-{mode:o}.txt"));
        fs::write(&source, b"asset contents").unwrap();
        fs::set_permissions(&source, fs::Permissions::from_mode(mode)).unwrap();
    }
    let result = directory.run(&[]);
    assert!(result.status.success(), "{result:?}");
    for mode in [0o400, 0o444, 0o640, 0o755, 0o6755] {
        let source = directory.0.join(format!("asset-{mode:o}.txt"));
        let original = fs::metadata(&source).unwrap();
        assert_eq!(original.permissions().mode() & 0o7777, mode);
        for suffix in ["gz", "br", "zst"] {
            let variant = fs::metadata(directory.0.join(format!("asset-{mode:o}.txt.{suffix}"))).unwrap();
            assert_eq!(variant.permissions().mode() & 0o7777, mode & 0o666);
            assert_eq!(variant.modified().unwrap(), original.modified().unwrap());
        }
    }
}

#[cfg(windows)]
#[test]
fn junction_roots_are_rejected_and_nested_junctions_are_not_traversed_or_replaced() {
    use std::os::windows::fs::MetadataExt;
    struct Junction(PathBuf);
    impl Junction {
        fn new(path: PathBuf, target: &std::path::Path) -> Self {
            // Directory junctions do not require the symlink creation privilege.
            let result = Command::new("cmd.exe")
                .args(["/D", "/C", "mklink", "/J"])
                .arg(&path)
                .arg(target)
                .output()
                .unwrap();
            assert!(result.status.success(), "cannot create test junction: {result:?}");
            let junction = Self(path);
            assert_ne!(
                fs::symlink_metadata(&junction.0).unwrap().file_attributes() & 0x400,
                0,
                "fixture must exercise the reparse-point branch"
            );
            junction
        }
    }
    impl Drop for Junction {
        fn drop(&mut self) {
            // Remove just the link, never recursively operate on its target.
            let _ = fs::remove_dir(&self.0);
        }
    }

    let directory = Directory::new();
    let outside = Directory::new();
    fs::write(outside.0.join("secret.txt"), b"outside sentinel").unwrap();
    fs::write(directory.0.join("asset.txt"), b"local asset").unwrap();
    let nested = Junction::new(directory.0.join("linked-directory"), &outside.0);
    let root_result = process_output::output(
        Command::new(env!("CARGO_BIN_EXE_gatel"))
            .arg("precompress")
            .arg(&nested.0),
    );
    assert_eq!(root_result.status.code(), Some(3), "{root_result:?}");
    assert!(String::from_utf8_lossy(&root_result.stderr).contains("non-link directory"));

    let result = directory.run(&["-e", "gz"]);
    assert!(result.status.success(), "{result:?}");
    assert!(directory.0.join("asset.txt.gz").is_file());
    assert_eq!(fs::read_dir(&outside.0).unwrap().count(), 1);
    assert_eq!(
        fs::read(outside.0.join("secret.txt")).unwrap(),
        b"outside sentinel"
    );

    let output_link = Junction::new(directory.0.join("asset.txt.br"), &outside.0);
    for arguments in [vec!["-e", "br"], vec!["-e", "br", "--dry-run"]] {
        let result = directory.run(&arguments);
        assert_eq!(result.status.code(), Some(3), "{result:?}");
        assert!(String::from_utf8_lossy(&result.stderr).contains("non-link file"));
        assert_ne!(
            fs::symlink_metadata(&output_link.0).unwrap().file_attributes() & 0x400,
            0
        );
        assert_eq!(fs::read_dir(&outside.0).unwrap().count(), 1);
        assert_eq!(
            fs::read(outside.0.join("secret.txt")).unwrap(),
            b"outside sentinel"
        );
    }
}

#[cfg(unix)]
#[test]
fn links_are_not_followed_or_replaced_and_private_assets_stay_private() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let directory = Directory::new();
    let outside = Directory::new();
    fs::write(outside.0.join("secret.txt"), "do not overwrite").unwrap();
    symlink(&outside.0, directory.0.join("linked-directory")).unwrap();
    symlink(outside.0.join("secret.txt"), directory.0.join("linked-file.txt")).unwrap();
    fs::write(directory.0.join("private.txt"), "private asset").unwrap();
    fs::set_permissions(directory.0.join("private.txt"), fs::Permissions::from_mode(0o600)).unwrap();
    assert!(directory.run(&["-e", "gz"]).status.success());
    assert!(!outside.0.join("secret.txt.gz").exists());
    assert!(!directory.0.join("linked-file.txt.gz").exists());
    assert_eq!(
        fs::metadata(directory.0.join("private.txt.gz"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    fs::remove_file(directory.0.join("private.txt.gz")).unwrap();
    symlink(outside.0.join("secret.txt"), directory.0.join("private.txt.gz")).unwrap();
    assert_eq!(directory.run(&["-e", "gzip"]).status.code(), Some(3));
    assert_eq!(
        fs::read(outside.0.join("secret.txt")).unwrap(),
        b"do not overwrite"
    );
    assert!(
        fs::symlink_metadata(directory.0.join("private.txt.gz"))
            .unwrap()
            .file_type()
            .is_symlink()
    );
}
