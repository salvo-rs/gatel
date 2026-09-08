//! A real serving process owns the bootstrap lock until forced termination.
use std::io::{Read, Write};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use super::*;

struct Process(Child);
impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn request(address: std::net::SocketAddr) -> std::io::Result<String> {
    let mut stream = std::net::TcpStream::connect_timeout(&address, Duration::from_millis(200))?;
    stream.set_read_timeout(Some(Duration::from_millis(500)))?;
    stream.set_write_timeout(Some(Duration::from_millis(500)))?;
    stream.write_all(b"GET / HTTP/1.1\r\nHost: fixture\r\nConnection: close\r\n\r\n")?;
    let mut response = String::new();
    stream.take(32 * 1024).read_to_string(&mut response)?;
    Ok(response)
}

#[test]
fn serving_bootstrap_retains_lock_and_kill_preserves_checkpoint() {
    const CHILD_DIRECTORY: &str = "GATEL_BOOTSTRAP_SERVING_FIXTURE";
    if let Some(directory) = std::env::var_os(CHILD_DIRECTORY) {
        let directory = PathBuf::from(directory);
        let code = crate::run(
            directory.join("missing.kdl"),
            false,
            vec![],
            crate::admin::cli::Options::default(),
            options(&directory, INSTANCE),
        );
        panic!("bootstrap server unexpectedly returned: {code:?}");
    }
    let directory = Directory::new();
    let registry = crate::plugins::registry().unwrap();
    let socket = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = socket.local_addr().unwrap();
    let text = format!(
        "global {{ http \"{address}\"; }}\nsite \"*\" {{ route \"/\" {{ respond \"live-checkpoint-bootstrap\"; }} }}"
    );
    let mut hash = Hasher::new();
    hash.update_str(&text);
    let source = SourceSnapshot::new(
        SourceId::file("main"),
        SourceRev(23),
        hash.finish(),
        gatel_kdl::lower(&text, "fixture").unwrap(),
    );
    let generation = GenerationStore::<()>::new();
    assert!(matches!(
        generation.attempt(&Snapshot::new().with(source), &registry, |_| Ok(())),
        Outcome::Started { .. }
    ));
    let mut journal = Journal::open(&directory.0, INSTANCE).unwrap();
    journal
        .save_checkpoint(&registry, &generation.capture_configuration())
        .unwrap();
    drop(journal);
    let log = std::fs::read(directory.0.join("intent.log")).unwrap();
    let checkpoint = std::fs::read(directory.0.join("checkpoint.bin")).unwrap();
    let output = File::create(directory.0.join("child.log")).unwrap();
    drop(socket);
    let mut process = Process(Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "admin::journal::store::checkpoint::tests::bootstrap::process::serving_bootstrap_retains_lock_and_kill_preserves_checkpoint", "--nocapture"])
        .env(CHILD_DIRECTORY, &directory.0)
        .stdin(Stdio::null()).stdout(Stdio::from(output.try_clone().unwrap())).stderr(Stdio::from(output))
        .spawn().unwrap());
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let status = process.0.try_wait().unwrap();
        assert!(
            status.is_none() && Instant::now() < deadline,
            "bootstrap child failed or timed out: {status:?}: {}",
            std::fs::read_to_string(directory.0.join("child.log")).unwrap_or_default()
        );
        if request(address).is_ok_and(|response| {
            response.starts_with("HTTP/1.1 200") && response.contains("live-checkpoint-bootstrap")
        }) {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    for _ in 0..3 {
        assert!(
            Journal::open_existing(&directory.0, INSTANCE).is_err(),
            "serving bootstrap released its lock early"
        );
        assert!(request(address).unwrap().contains("live-checkpoint-bootstrap"));
    }
    assert_eq!(std::fs::read(directory.0.join("intent.log")).unwrap(), log);
    assert_eq!(
        std::fs::read(directory.0.join("checkpoint.bin")).unwrap(),
        checkpoint
    );
    process.0.kill().unwrap();
    let status = process.0.wait().unwrap();
    assert!(!status.success());
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(status.signal(), Some(9));
    }
    let mut reopened = Journal::open_existing(&directory.0, INSTANCE).unwrap();
    assert_eq!(
        reopened
            .load_checkpoint(&registry)
            .unwrap()
            .unwrap()
            .metadata
            .plan,
        generation.current().unwrap().plan_hash()
    );
    assert_eq!(std::fs::read(directory.0.join("intent.log")).unwrap(), log);
    assert_eq!(
        std::fs::read(directory.0.join("checkpoint.bin")).unwrap(),
        checkpoint
    );
    assert!(!directory.0.join("missing.kdl").exists());
}
