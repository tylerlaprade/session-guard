use chrono::{Duration as Age, Utc};
use serde_json::{Value, json};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const SESSION: &str = "5f0c2b48-865f-4829-af1b-7a248d2c2bee";

// Stands in for `log`. The stored log holds no change and takes a moment to
// read. The first stream reports the terminal locked out, so a later daemon
// knows it only from the record the first one left; any stream lifts the
// lockout when the test creates `lift`.
const LOG: &str = r#"#!/bin/sh
[ "$1" = show ] && { sleep 1; echo '{"count":0,"finished":1}'; exit 0; }
echo $$ > "$HOME/stream.pid"
echo 'Filtering the log data using "stand-in"'
if [ ! -e "$HOME/reported" ]; then
    : > "$HOME/reported"
    echo "{\"processID\":$TERMINAL_PID,\"eventMessage\":\"LockoutUI changed state: engaged\"}"
fi
while [ ! -e "$HOME/lift" ]; do sleep 0.1; done
echo "{\"processID\":$TERMINAL_PID,\"eventMessage\":\"LockoutUI changed state: disengaged\"}"
exec sleep 600
"#;

// Stands in for `alacritty --working-directory DIR -e sh -lc COMMAND`.
const OPEN_WINDOW: &str = r#"#!/usr/bin/env python3
import os, pathlib, subprocess, sys
child = subprocess.Popen(sys.argv[4:], cwd=sys.argv[2], stdin=subprocess.DEVNULL,
                         stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                         start_new_session=True)
pathlib.Path(os.environ["HOME"], "launcher.pid").write_text(str(child.pid))
"#;

// Stands in for `claude`. `--bg --resume ID` starts a background session and
// registers it the way Claude's hooks do; anything else, such as `attach ID`,
// is recorded and then waits like an interactive session.
const CLAUDE: &str = r#"#!/bin/sh
echo "$*" >> "$HOME/claude.args"
if [ "$1" = --bg ]; then
    sleep 60 </dev/null >/dev/null 2>&1 &
    echo $! >> "$HOME/background.pids"
    "$TEST_BINARY" register --tool claude --session-id "$3" --pid $! --shell-pid $! --directory "$PWD" </dev/null >/dev/null
    echo "backgrounded · $(echo "$3" | cut -c1-8) · stand-in"
    exit 0
fi
exec sleep 30
"#;

fn stand_in_terminal_path() -> PathBuf {
    std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("examples/alacritty")
}

// The terminal is the oldest process with its name, so two stand-ins running at
// once would read each other's lockout.
static ONE_TERMINAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

struct Fixture {
    home: tempfile::TempDir,
    terminal: Child,
    daemon: Option<Child>,
    _one_terminal: std::sync::MutexGuard<'static, ()>,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if let Some(daemon) = self.daemon.as_mut() {
            let _ = daemon.kill();
            let _ = daemon.wait();
        }
        let _ = self.terminal.kill();
        let _ = self.terminal.wait();
        for (file, group) in [("launcher.pid", true), ("stream.pid", false)] {
            if let Some(pid) = self.pid_in(file) {
                unsafe {
                    libc::kill(if group { -pid } else { pid }, libc::SIGKILL);
                }
            }
        }
        for pid in fs::read_to_string(self.root().join("background.pids"))
            .unwrap_or_default()
            .lines()
            .filter_map(|pid| pid.parse::<i32>().ok())
        {
            unsafe {
                libc::kill(pid, libc::SIGKILL);
            }
        }
    }
}

impl Fixture {
    fn new(tool: &str) -> Self {
        let one_terminal = ONE_TERMINAL
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let home = tempfile::tempdir().unwrap();
        let root = home.path();
        let config = root.join(".config/session-guard");
        let bin = root.join("bin");
        fs::create_dir_all(&config).unwrap();
        fs::create_dir(&bin).unwrap();
        for (name, contents) in [
            ("log", LOG),
            ("alacritty", OPEN_WINDOW),
            ("claude", CLAUDE),
            ("codex", "#!/bin/sh\nexec sleep 30\n"),
        ] {
            fs::write(bin.join(name), contents).unwrap();
            fs::set_permissions(bin.join(name), fs::Permissions::from_mode(0o755)).unwrap();
        }
        let terminal = Command::new(stand_in_terminal_path()).spawn().unwrap();

        let idle_since = Utc::now() - Age::days(1);
        fs::write(config.join("terminal"), "alacritty\n").unwrap();
        fs::write(
            config.join("active-sessions.json"),
            json!([{
                "session_id": SESSION, "tool": tool, "pid": 2_147_483_647,
                "shell_pid": 2_147_483_646, "directory": root, "session_name": null,
                "registered_at": idle_since, "last_seen_at": idle_since, "state": "recoverable",
                "dead_at": idle_since, "restore_pending": true, "transcript_path": null
            }])
            .to_string(),
        )
        .unwrap();
        fs::write(
            config.join("daemon-heartbeat"),
            json!({"timestamp": Utc::now(), "boot_id": "previous-boot"}).to_string(),
        )
        .unwrap();
        Self {
            home,
            terminal,
            daemon: None,
            _one_terminal: one_terminal,
        }
    }

    fn root(&self) -> &Path {
        self.home.path()
    }

    fn command(&self, args: &[&str]) -> Command {
        let root = self.root();
        let mut command = Command::new(env!("CARGO_BIN_EXE_session-guard"));
        command
            .args(args)
            .env("HOME", root)
            .env("TMPDIR", root)
            .env("CLAUDE_CONFIG_DIR", root.join(".claude"))
            .env("CODEX_HOME", root.join(".codex"))
            .env("SHELL", "/bin/sh")
            .env("TEST_BINARY", env!("CARGO_BIN_EXE_session-guard"))
            .env("TERMINAL_PID", self.terminal.id().to_string())
            .env(
                "PATH",
                format!(
                    "{}:/usr/bin:/bin:/usr/sbin:/sbin",
                    root.join("bin").display()
                ),
            )
            .stdin(Stdio::null());
        command
    }

    fn lockout(&self) -> Output {
        self.command(&["lockout"]).output().unwrap()
    }

    fn daemon_log(&self) -> String {
        fs::read_to_string(self.root().join(".config/session-guard/daemon.log")).unwrap_or_default()
    }

    fn record(&self) -> Value {
        let path = self
            .root()
            .join(".config/session-guard/active-sessions.json");
        serde_json::from_slice::<Value>(&fs::read(path).unwrap()).unwrap()[0].clone()
    }

    fn pid_in(&self, file: &str) -> Option<i32> {
        fs::read_to_string(self.root().join(file))
            .ok()?
            .trim()
            .parse()
            .ok()
    }

    fn wait_for(&self, what: &str, mut condition: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(15);
        while !condition() {
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {what}: {}",
                self.daemon_log()
            );
            thread::sleep(Duration::from_millis(50));
        }
    }

    fn start_daemon(&mut self) {
        self.daemon = Some(
            self.command(&["daemon"])
                .stdout(Stdio::null())
                .spawn()
                .unwrap(),
        );
    }

    /// Stops the daemon the way launchd does and checks it took its stream
    /// along.
    fn stop_daemon(&mut self) {
        let stream = self.pid_in("stream.pid").unwrap();
        let daemon = self.daemon.take().unwrap();
        unsafe {
            libc::kill(i32::try_from(daemon.id()).unwrap(), libc::SIGTERM);
        }
        let status = daemon.wait_with_output().unwrap().status;
        assert!(status.success(), "daemon exited with {status}");
        assert!(!alive(stream), "the daemon left its log stream running");
        assert!(
            !self
                .daemon_log()
                .contains("lockout watcher did not stop cleanly")
        );
        let state: Value = serde_json::from_slice(&fs::read(self.state_file()).unwrap()).unwrap();
        assert!(state["followed_until"].is_string(), "{state}");
        let unknown = self.lockout();
        assert_eq!(unknown.status.code(), Some(2));
        assert!(String::from_utf8_lossy(&unknown.stderr).contains("watcher is not running"));
    }

    fn state_file(&self) -> PathBuf {
        self.root().join("session-guard-lockout.json")
    }
}

fn alive(pid: i32) -> bool {
    unsafe { libc::kill(pid, 0) == 0 }
}

#[test]
fn a_locked_out_terminal_gets_no_tab_until_the_lockout_lifts() {
    let mut fixture = Fixture::new("codex");
    fixture.start_daemon();
    fixture.wait_for("the watcher to start", || fixture.state_file().exists());
    let mut waiter = fixture.command(&["lockout", "--wait"]).spawn().unwrap();

    fixture.wait_for("the startup pass to hold", || {
        fixture.daemon_log().contains("Held 1 sessions")
    });
    fixture.wait_for("the lockout to be known", || {
        fixture.lockout().status.code() == Some(1)
    });
    let locked = fixture.lockout();
    assert!(
        String::from_utf8_lossy(&locked.stderr).contains("alacritty is locked out by Screen Time")
    );
    assert!(!fixture.root().join("launcher.pid").exists());
    assert_eq!(fixture.record()["restore_pending"], true);
    assert!(!fixture.daemon_log().contains("Restored"));

    assert!(
        waiter.try_wait().unwrap().is_none(),
        "--wait returned while the state was being read or locked out"
    );

    // A daemon restarted during the lockout, on the same boot, still holds
    // the session rather than dropping it as an old recoverable record, and
    // knows the lockout from where the previous daemon left off.
    fixture.stop_daemon();
    assert_eq!(waiter.wait().unwrap().code(), Some(2));
    assert_eq!(fixture.record()["held_for_lockout"], true);
    fixture.start_daemon();
    fixture.wait_for("the restarted daemon to hold", || {
        fixture.daemon_log().matches("Held 1 sessions").count() == 2
    });
    fixture.wait_for("the lockout to be known again", || {
        fixture.lockout().status.code() == Some(1)
    });
    assert!(!fixture.root().join("launcher.pid").exists());

    waiter = fixture.command(&["lockout", "--wait"]).spawn().unwrap();
    thread::sleep(Duration::from_millis(700));
    assert!(
        waiter.try_wait().unwrap().is_none(),
        "--wait returned while locked out"
    );

    fs::write(fixture.root().join("lift"), "").unwrap();
    fixture.wait_for("--wait to return", || waiter.try_wait().unwrap().is_some());
    assert_eq!(waiter.wait().unwrap().code(), Some(0));
    fixture.wait_for("the held restore", || {
        fixture
            .daemon_log()
            .contains("held restore: Restored 1 sessions")
    });
    fixture.wait_for("the restored owner", || {
        fixture.record()["state"] == "active"
    });
    assert_eq!(fixture.record()["held_for_lockout"], false);

    fixture.stop_daemon();
}

#[test]
fn a_claude_session_resumes_in_the_background_and_gets_its_tab_after_the_lockout() {
    let mut fixture = Fixture::new("claude");
    fixture.start_daemon();
    fixture.wait_for("the background resume", || {
        fixture.daemon_log().contains("Resumed 1 in the background")
    });
    let claude_args = || fs::read_to_string(fixture.root().join("claude.args")).unwrap_or_default();
    assert!(claude_args().contains(&format!("--bg --resume {SESSION}")));
    assert_eq!(fixture.record()["attach_after_lockout"], true);
    assert_eq!(fixture.record()["state"], "active");

    // The attach pass runs every five seconds and must open nothing while the
    // terminal is still locked out.
    fixture.wait_for("the lockout to be known", || {
        fixture.lockout().status.code() == Some(1)
    });
    thread::sleep(Duration::from_secs(6));
    assert!(
        !claude_args().contains("attach"),
        "{}\n{}",
        claude_args(),
        fixture.daemon_log()
    );

    fs::write(fixture.root().join("lift"), "").unwrap();
    fixture.wait_for("the attach tab", || {
        claude_args().contains(&format!("attach {}", &SESSION[..8]))
    });
    fixture.wait_for("the attach flag to clear", || {
        fixture.record()["attach_after_lockout"] == false
    });
    // The startup pass ran while the watcher was still reading the state, so
    // it held the session; the lockout, once known, sent it to the background.
    assert!(fixture.daemon_log().contains(
        "held restore: Restored 0 sessions (0 Claude Code, 0 Codex, 0 Grok, 0 OpenCode). Pruned 0 (directory gone). Resumed 1 in the background: alacritty is locked out by Screen Time."
    ), "{}", fixture.daemon_log());
    fixture.stop_daemon();
}

#[test]
fn attach_after_lockout_marks_the_one_session_its_prefix_names() {
    let fixture = Fixture::new("claude");
    let too_short = fixture
        .command(&["attach-after-lockout", &SESSION[..6]])
        .output()
        .unwrap();
    assert!(!too_short.status.success());
    let marked = fixture
        .command(&["attach-after-lockout", &SESSION[..8]])
        .output()
        .unwrap();
    assert!(
        marked.status.success(),
        "{}",
        String::from_utf8_lossy(&marked.stderr)
    );
    assert_eq!(fixture.record()["attach_after_lockout"], true);
}

#[test]
fn attach_after_lockout_refuses_a_tool_without_a_background_mode() {
    let fixture = Fixture::new("codex");
    let refused = fixture
        .command(&["attach-after-lockout", &SESSION[..8]])
        .output()
        .unwrap();
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("has no background mode"));
    assert_eq!(fixture.record()["attach_after_lockout"], Value::Null);
}
