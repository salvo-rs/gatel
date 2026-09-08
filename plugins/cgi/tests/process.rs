#![cfg(target_os = "linux")]
use std::ffi::OsString;
use std::path::Path;
use std::time::Duration;

use gatel_cgi::process::Process;
use gatel_cgi_protocol::environment::Environment;
use nix::errno::Errno;
use nix::sys::wait::{Id, WaitPidFlag, WaitStatus, waitid};
use nix::unistd::Pid;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn shell(script: &str) -> (Process, gatel_cgi::process::Pipes) {
    Process::spawn(
        Path::new("/bin/sh"),
        &[OsString::from("-c"), OsString::from(script)],
        Path::new("/"),
        &Environment::new(1024),
    )
    .unwrap()
}

#[tokio::test]
async fn explicit_environment_and_three_pipes_work_without_inheritance() {
    let mut environment = Environment::new(1024);
    environment.insert("CGI_TEST", b"isolated").unwrap();
    let (mut process, mut pipes) =
        Process::spawn(Path::new("/usr/bin/env"), &[], Path::new("/"), &environment).unwrap();
    pipes.stdin.shutdown().await.unwrap();
    let mut bytes = Vec::new();
    tokio::time::timeout(Duration::from_secs(3), pipes.stdout.read_to_end(&mut bytes))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(bytes, b"CGI_TEST=isolated\n");
    assert!(process.exited().await.unwrap().success());
    // Observing status twice must not consume/reap the leader.
    assert!(process.exited().await.unwrap().success());
    assert!(process.terminate_and_reap().await.unwrap().success());
    assert!(process.terminate_and_reap().await.unwrap().success());
    assert_eq!(pipes.stderr.read(&mut [0; 1]).await.unwrap(), 0);
}

#[tokio::test]
async fn failed_exit_is_observed_without_consuming_wait_status() {
    let (mut process, mut pipes) = shell("printf diagnostic >&2; exit 7");
    let status = tokio::time::timeout(Duration::from_secs(3), process.exited())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(status.code(), Some(7));
    let pid = Pid::from_raw(process.id() as i32);
    assert!(matches!(
        waitid(
            Id::Pid(pid),
            WaitPidFlag::WEXITED | WaitPidFlag::WNOWAIT | WaitPidFlag::WNOHANG
        )
        .unwrap(),
        WaitStatus::Exited(_, 7)
    ));
    assert_eq!(process.terminate_and_reap().await.unwrap().code(), Some(7));
    assert_eq!(
        waitid(Id::Pid(pid), WaitPidFlag::WEXITED | WaitPidFlag::WNOHANG),
        Err(Errno::ECHILD)
    );
    let mut bytes = Vec::new();
    pipes.stderr.read_to_end(&mut bytes).await.unwrap();
    assert_eq!(bytes, b"diagnostic");
}

#[tokio::test]
async fn cancellation_can_terminate_group_and_reap_leader() {
    let (mut process, mut pipes) = shell("sleep 30 & printf ready; wait");
    let mut ready = [0; 5];
    tokio::time::timeout(Duration::from_secs(3), pipes.stdout.read_exact(&mut ready))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(&ready, b"ready");
    assert!(
        tokio::time::timeout(Duration::from_millis(20), process.exited())
            .await
            .is_err()
    );
    let pid = Pid::from_raw(process.id() as i32);
    let status = tokio::time::timeout(Duration::from_secs(3), process.terminate_and_reap())
        .await
        .unwrap()
        .unwrap();
    assert!(!status.success());
    assert_eq!(
        waitid(Id::Pid(pid), WaitPidFlag::WEXITED | WaitPidFlag::WNOHANG),
        Err(Errno::ECHILD)
    );
    // The sleep descendant inherited stdout; EOF demonstrates its pipe closed,
    // unlike merely killing the shell and leaving sleep running for 30 seconds.
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(3), pipes.stdout.read(&mut [0; 1]))
            .await
            .unwrap()
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn emergency_drop_terminates_group_without_waiting_for_timeout() {
    let (process, mut pipes) = shell("sleep 30 & printf ready; wait");
    tokio::time::timeout(Duration::from_secs(3), pipes.stdout.read_exact(&mut [0; 5]))
        .await
        .unwrap()
        .unwrap();
    drop(process);
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(3), pipes.stdout.read(&mut [0; 1]))
            .await
            .unwrap()
            .unwrap(),
        0
    );
}

#[test]
fn relative_program_or_cwd_is_rejected_before_spawn() {
    for (program, cwd) in [("sh", "/"), ("/bin/sh", ".")] {
        assert!(Process::spawn(Path::new(program), &[], Path::new(cwd), &Environment::new(1)).is_err());
    }
}
