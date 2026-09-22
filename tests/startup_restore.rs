use chrono::Utc;
use serde_json::json;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

struct Daemon(Child);

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn run_startup(boot_id: &str, tool: &str, spare: bool) -> String {
    let directory = tempfile::tempdir().unwrap();
    let home = directory.path();
    let config = home.join(".config/session-guard");
    let bin = home.join("bin");
    fs::create_dir_all(&config).unwrap();
    fs::create_dir(&bin).unwrap();
    let now = Utc::now();
    let session = "7a248d2c-865f-4829-af1b-2bee5f0c2b48";
    fs::write(config.join("terminal"), "ghostty\n").unwrap();
    fs::write(
        config.join("active-sessions.json"),
        json!([{
            "session_id": session, "tool": tool, "pid": 2147483647,
            "shell_pid": 2147483646, "directory": home, "session_name": null,
            "registered_at": now, "last_seen_at": now, "state": "recoverable",
            "dead_at": now
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
    fs::write(&script, "#!/bin/sh\ncase \"$2\" in\n  *'is running'*) printf 'true\\n' ;;\n  *'working directory of focused terminal'*) printf '/tmp\\n' ;;\n  *) printf 'test-tab\\n' ;;\nesac\n").unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    let _daemon = Daemon(
        Command::new(env!("CARGO_BIN_EXE_session-guard"))
            .arg("daemon")
            .env("HOME", home)
            .env("CLAUDE_CONFIG_DIR", home.join(".claude"))
            .env("CODEX_HOME", home.join(".codex"))
            .env(
                "PATH",
                format!("{}:/usr/bin:/bin:/usr/sbin:/sbin", bin.display()),
            )
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let log = fs::read_to_string(config.join("daemon.log")).unwrap_or_default();
        if log.contains("Restored ") {
            return log;
        }
        assert!(Instant::now() < deadline, "startup did not finish: {log}");
        thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn reboot_restores_a_codex_death_already_seen_by_the_old_monitor() {
    assert!(run_startup("previous-boot", "codex", false).contains("Restored 1 sessions"));
}

#[test]
fn reboot_keeps_dispatched_background_claude_but_excludes_native_spares() {
    assert!(run_startup("previous-boot", "claude", false).contains("Restored 1 sessions"));
    let log = run_startup("previous-boot", "claude", true);
    assert!(log.contains("Restored 0 sessions"));
    assert!(log.contains("skipped unused spare"));
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
    assert!(run_startup(current_boot.trim(), "codex", false).contains("Restored 0 sessions"));
}
