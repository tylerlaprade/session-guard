use chrono::{Duration as Age, Utc};
use serde_json::{Value, json};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

struct Daemon {
    child: Child,
    launcher_pid: std::path::PathBuf,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Ok(contents) = fs::read_to_string(&self.launcher_pid)
            && let Ok(pid) = contents.trim().parse::<i32>()
            && pid > 1
        {
            unsafe {
                libc::kill(-pid, libc::SIGKILL);
            }
        }
    }
}

fn run_startup(boot_id: &str, tool: &str, spare: bool, pending: bool) -> (String, Value) {
    let directory = tempfile::tempdir().unwrap();
    let home = directory.path();
    let config = home.join(".config/session-guard");
    let bin = home.join("bin");
    fs::create_dir_all(&config).unwrap();
    fs::create_dir(&bin).unwrap();
    let now = Utc::now();
    let idle_since = now - Age::days(3);
    let session = "7a248d2c-865f-4829-af1b-2bee5f0c2b48";
    fs::write(config.join("terminal"), "ghostty\n").unwrap();
    fs::write(
        config.join("active-sessions.json"),
        json!([{
            "session_id": session, "tool": tool, "pid": 2_147_483_647,
            "shell_pid": 2_147_483_646, "directory": home, "session_name": null,
            "registered_at": idle_since, "last_seen_at": idle_since, "state": "recoverable",
            "dead_at": idle_since, "restore_pending": pending
        }])
        .to_string(),
    )
    .unwrap();
    fs::write(
        config.join("daemon-heartbeat"),
        json!({"timestamp": now, "boot_id": boot_id}).to_string(),
    )
    .unwrap();
    if spare {
        fs::create_dir_all(home.join(".claude/daemon")).unwrap();
        fs::write(
            home.join(".claude/daemon/roster.json"),
            json!({"workers": {"spare": {
                "sessionId": session, "dispatch": {"source": "spare"}
            }}})
            .to_string(),
        )
        .unwrap();
    }
    let script = bin.join("osascript");
    fs::write(&script, r#"#!/usr/bin/env python3
import os, pathlib, subprocess, sys
script = sys.argv[-1]
if "is running" in script:
    print("true")
elif "working directory of focused terminal" in script:
    print("/tmp")
else:
    child = subprocess.Popen([os.environ["TEST_BINARY"], "launch", "--session-id", os.environ["TEST_SESSION"]],
                             stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                             start_new_session=True)
    pathlib.Path(os.environ["HOME"], "launcher.pid").write_text(str(child.pid))
    print("test-window\ttest-tab")
"#).unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    let provider = bin.join(tool);
    fs::write(&provider, "#!/bin/sh\nexec sleep 30\n").unwrap();
    fs::set_permissions(&provider, fs::Permissions::from_mode(0o755)).unwrap();
    let spawn_daemon = || {
        Command::new(env!("CARGO_BIN_EXE_session-guard"))
            .arg("daemon")
            .env("HOME", home)
            .env("CLAUDE_CONFIG_DIR", home.join(".claude"))
            .env("CODEX_HOME", home.join(".codex"))
            .env("SHELL", "/bin/sh")
            .env("TEST_BINARY", env!("CARGO_BIN_EXE_session-guard"))
            .env("TEST_SESSION", session)
            .env(
                "PATH",
                format!("{}:/usr/bin:/bin:/usr/sbin:/sbin", bin.display()),
            )
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap()
    };
    let mut daemon = Daemon {
        child: spawn_daemon(),
        launcher_pid: home.join("launcher.pid"),
    };
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut first_owner = None;
    loop {
        let log = fs::read_to_string(config.join("daemon.log")).unwrap_or_default();
        if log.matches("Restored ").count() > usize::from(first_owner.is_some()) {
            let records: Value =
                serde_json::from_slice(&fs::read(config.join("active-sessions.json")).unwrap())
                    .unwrap();
            if !spare && pending && boot_id == "previous-boot" {
                let record = &records[0];
                assert_eq!(record["state"], "active");
                assert_eq!(record["restore_pending"], false);
                assert_ne!(record["pid"], 2_147_483_647);
                assert_ne!(record["shell_pid"], 2_147_483_646);
                assert!(!record["pid_started_at"].as_str().unwrap().is_empty());
                assert_eq!(record["dead_at"], Value::Null);
                let duplicate = Command::new(env!("CARGO_BIN_EXE_session-guard"))
                    .args(["launch", "--session-id", session])
                    .env("HOME", home)
                    .output()
                    .unwrap();
                assert!(!duplicate.status.success());
                assert!(
                    String::from_utf8_lossy(&duplicate.stderr).contains("already has a live owner")
                );
                let owner = record["shell_pid"].as_i64().unwrap() as i32;
                if first_owner.is_none() {
                    daemon.child.kill().unwrap();
                    daemon.child.wait().unwrap();
                    unsafe {
                        libc::kill(-owner, libc::SIGKILL);
                    }
                    fs::write(
                        config.join("daemon-heartbeat"),
                        json!({"timestamp": now, "boot_id": "another-boot"}).to_string(),
                    )
                    .unwrap();
                    first_owner = Some(owner);
                    daemon.child = spawn_daemon();
                    continue;
                }
                assert_ne!(Some(owner), first_owner);
            }
            return (log, records);
        }
        assert!(Instant::now() < deadline, "startup did not finish: {log}");
        thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn three_day_idle_sessions_restore_without_provider_hooks() {
    for tool in ["codex", "claude", "grok", "opencode"] {
        assert!(
            run_startup("previous-boot", tool, false, true)
                .0
                .contains("Restored 1 sessions")
        );
    }
}

#[test]
fn reboot_excludes_native_spares_and_keeps_legacy_records() {
    let (log, records) = run_startup("previous-boot", "claude", true, true);
    assert!(log.contains("Restored 0 sessions"));
    assert!(log.contains("skipped unused spare"));
    assert_eq!(records.as_array().unwrap().len(), 1);
    let (log, records) = run_startup("previous-boot", "codex", false, false);
    assert!(log.contains("Restored 0 sessions"));
    assert_eq!(records.as_array().unwrap().len(), 1);
}

#[test]
fn restarting_only_the_daemon_does_not_reopen_observed_deaths() {
    #[cfg(target_os = "macos")]
    let current_boot = {
        let output = Command::new("sysctl")
            .args(["-n", "kern.bootsessionuuid"])
            .output()
            .unwrap();
        assert!(output.status.success());
        String::from_utf8(output.stdout).unwrap()
    };
    #[cfg(not(target_os = "macos"))]
    let current_boot = fs::read_to_string("/proc/sys/kernel/random/boot_id").unwrap();
    assert!(
        run_startup(current_boot.trim(), "codex", false, true)
            .0
            .contains("Restored 0 sessions")
    );
}
