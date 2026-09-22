use serde_json::{Value, json};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command};
use std::thread;
use std::time::{Duration, Instant};

struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn restored_launch_preserves_interactive_wrappers_and_provider_exit_status() {
    let home = tempfile::tempdir().unwrap();
    let config = home.path().join(".config/session-guard");
    fs::create_dir_all(&config).unwrap();
    let session = "9c6ad8c7-8cda-49dc-a74f-1a2a57e3f2cf";
    fs::write(
        config.join("active-sessions.json"),
        json!([{
            "session_id":session,"tool":"claude","directory":home.path(),"session_name":null,
            "registered_at":"2026-01-01T00:00:00Z","state":"recoverable","restore_pending":true
        }])
        .to_string(),
    )
    .unwrap();
    fs::write(home.path().join(".zshrc"), "claude() { print -r -- \"WRAPPER:$*\"; read -r answer; print -r -- \"INPUT:$answer\"; return 17; }\n").unwrap();
    let mut master = -1;
    let mut slave = -1;
    assert_eq!(
        unsafe {
            libc::openpty(
                &raw mut master,
                &raw mut slave,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        },
        0
    );
    let mut master = unsafe { File::from_raw_fd(master) };
    let slave = unsafe { File::from_raw_fd(slave) };
    let fd = master.as_raw_fd();
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    assert_ne!(
        unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) },
        -1
    );
    let mut command = Command::new(env!("CARGO_BIN_EXE_session-guard"));
    command
        .args(["launch", "--session-id", session])
        .env("HOME", home.path())
        .env("ZDOTDIR", home.path())
        .env("SHELL", "/bin/zsh")
        .stdin(slave.try_clone().unwrap())
        .stdout(slave.try_clone().unwrap())
        .stderr(slave);
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 || libc::ioctl(0, libc::TIOCSCTTY.into(), 0) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = ChildGuard(command.spawn().unwrap());
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut output = Vec::new();
    while !String::from_utf8_lossy(&output).contains("WRAPPER:") {
        let mut buffer = [0; 4096];
        if let Ok(count) = master.read(&mut buffer) {
            output.extend_from_slice(&buffer[..count]);
        }
        assert!(
            Instant::now() < deadline,
            "wrapper did not start: {}",
            String::from_utf8_lossy(&output)
        );
        thread::sleep(Duration::from_millis(10));
    }
    let records: Value =
        serde_json::from_slice(&fs::read(config.join("active-sessions.json")).unwrap()).unwrap();
    assert_eq!(records[0]["shell_pid"], child.0.id());
    assert_eq!(records[0]["state"], "active");
    master.write_all(b"ready\n").unwrap();
    let status = loop {
        let mut buffer = [0; 4096];
        if let Ok(count) = master.read(&mut buffer) {
            output.extend_from_slice(&buffer[..count]);
        }
        if let Some(status) = child.0.try_wait().unwrap() {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "interactive provider did not finish"
        );
        thread::sleep(Duration::from_millis(10));
    };
    assert_eq!(status.code(), Some(17));
    let records: Value =
        serde_json::from_slice(&fs::read(config.join("active-sessions.json")).unwrap()).unwrap();
    assert_eq!(records[0]["restore_pending"], true);
}
