#![cfg(windows)]
use std::ffi::OsString;
use std::io::Write;
use std::os::windows::process::CommandExt;
use std::path::Path;
use std::time::Duration;

use gatel_cgi::process::{Pipes, Process};
use gatel_cgi_protocol::environment::Environment;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};

#[test]
fn child_fixture() {
    let Ok(mode) = std::env::var("CGI_FIXTURE") else {
        return;
    };
    if mode == "sleep" {
        std::thread::sleep(Duration::from_secs(30));
        std::process::exit(0);
    }
    if mode == "exit" {
        println!(
            "ENV:{}:{}",
            std::env::var("CGI_TEST").unwrap(),
            std::env::var_os("PATH").is_none()
        );
        eprintln!("diagnostic");
        std::process::exit(7);
    }
    let mut command = std::process::Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", "child_fixture", "--nocapture"])
        .env_clear()
        .env("CGI_FIXTURE", "sleep")
        .creation_flags(0x08000000 | if mode == "breakaway" { 0x01000000 } else { 0 });
    let child = command.spawn();
    if mode == "breakaway" {
        assert!(child.is_err(), "breakaway must be refused");
        println!("BLOCKED");
        std::process::exit(0);
    }
    let mut child = child.unwrap();
    if mode == "limit" {
        assert!(
            command.spawn().is_err(),
            "third process must exceed the two-process Job limit"
        );
        println!("LIMITED");
        std::process::exit(0);
    }
    println!("READY");
    std::io::stdout().flush().unwrap();
    if mode == "orphan" {
        std::process::exit(0);
    }
    let _ = child.wait();
    std::process::exit(0);
}

fn start(mode: &str) -> (Process, Pipes) {
    let mut environment = Environment::new(1024);
    environment.insert("CGI_FIXTURE", mode.as_bytes()).unwrap();
    environment.insert("CGI_TEST", b"isolated").unwrap();
    Process::spawn_with_limit(
        &std::env::current_exe().unwrap(),
        &[
            OsString::from("--exact"),
            OsString::from("child_fixture"),
            OsString::from("--nocapture"),
        ],
        &std::env::current_dir().unwrap(),
        &environment,
        if mode == "limit" { 2 } else { 64 },
    )
    .unwrap()
}
async fn ready(stdout: &mut BufReader<tokio::process::ChildStdout>) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let mut line = String::new();
            assert_ne!(stdout.read_line(&mut line).await.unwrap(), 0);
            if line.trim() == "READY" {
                break;
            }
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn exit_status_environment_and_whole_job_completion() {
    let (mut process, mut pipes) = start("exit");
    let status = tokio::time::timeout(Duration::from_secs(5), process.exited())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(status.code(), Some(7));
    let mut output = String::new();
    pipes.stdout.read_to_string(&mut output).await.unwrap();
    assert!(output.contains("ENV:isolated:true"), "{output}");
    let mut error = String::new();
    pipes.stderr.read_to_string(&mut error).await.unwrap();
    assert!(error.contains("diagnostic"));
    assert_eq!(process.terminate_and_reap().await.unwrap().code(), Some(7));
    assert_eq!(process.active_processes().unwrap(), 0);
    assert_eq!(process.terminate_and_reap().await.unwrap().code(), Some(7));
}

#[tokio::test]
async fn cancellation_and_root_exit_still_clean_descendants() {
    for mode in ["tree", "orphan"] {
        let (mut process, pipes) = start(mode);
        let mut stdout = BufReader::new(pipes.stdout);
        ready(&mut stdout).await;
        if mode == "orphan" {
            assert!(process.exited().await.unwrap().success());
            assert!(process.active_processes().unwrap() >= 1);
        } else {
            assert!(
                tokio::time::timeout(Duration::from_millis(20), process.exited())
                    .await
                    .is_err()
            );
            assert!(process.active_processes().unwrap() >= 2);
        }
        tokio::time::timeout(Duration::from_secs(5), process.terminate_and_reap())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(process.active_processes().unwrap(), 0);
        let mut rest = Vec::new();
        tokio::time::timeout(Duration::from_secs(2), stdout.read_to_end(&mut rest))
            .await
            .unwrap()
            .unwrap();
    }
}

#[tokio::test]
async fn drop_terminates_descendants_and_closes_inherited_pipe() {
    let (process, pipes) = start("tree");
    let mut stdout = BufReader::new(pipes.stdout);
    ready(&mut stdout).await;
    drop(process);
    let mut rest = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), stdout.read_to_end(&mut rest))
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn breakaway_flag_cannot_escape_the_job() {
    let (mut process, mut pipes) = start("breakaway");
    assert!(
        tokio::time::timeout(Duration::from_secs(5), process.exited())
            .await
            .unwrap()
            .unwrap()
            .success()
    );
    let mut bytes = String::new();
    pipes.stdout.read_to_string(&mut bytes).await.unwrap();
    assert!(bytes.contains("BLOCKED"), "{bytes}");
    process.terminate_and_reap().await.unwrap();
    assert_eq!(process.active_processes().unwrap(), 0);
}

#[tokio::test]
async fn process_limit_includes_root_and_descendants() {
    let (mut process, mut pipes) = start("limit");
    assert!(
        tokio::time::timeout(Duration::from_secs(5), process.exited())
            .await
            .unwrap()
            .unwrap()
            .success()
    );
    assert!(process.active_processes().unwrap() > 0);
    tokio::time::timeout(Duration::from_secs(3), process.terminate_and_reap())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(process.active_processes().unwrap(), 0);
    let mut bytes = String::new();
    pipes.stdout.read_to_string(&mut bytes).await.unwrap();
    assert!(bytes.contains("LIMITED"), "{bytes}");
}

#[test]
fn implicit_shell_and_non_utf8_environment_are_rejected() {
    let cwd = std::env::current_dir().unwrap();
    for program in [Path::new("cmd.exe"), Path::new("C:\\script.cmd")] {
        assert!(Process::spawn(program, &[], &cwd, &Environment::new(1)).is_err());
    }
    let mut environment = Environment::new(1024);
    environment.insert("BAD", &[0xff]).unwrap();
    assert!(Process::spawn(&std::env::current_exe().unwrap(), &[], &cwd, &environment).is_err());
}
