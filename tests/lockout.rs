use chrono::{Duration as Age, Utc};
use serde_json::{Value, json};
use std::ffi::{CString, c_char, c_int};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const SESSION: &str = "5f0c2b48-865f-4829-af1b-7a248d2c2bee";
// The name the daemon looks for Screen Time's lockout agent by; this Mac's
// own agent never stands in.
const AGENT: &str = "sg-test-agent";

// Stands in for `log`. The terminal was locked out before the first daemon
// started, so the stored log shows that once; a later daemon knows it only
// from the record the first one left. The stored log takes a moment to read,
// and the stream lifts the lockout when the test creates `lift`.
const LOG: &str = r#"#!/bin/sh
if [ "$1" = show ]; then
    sleep 1
    if [ ! -e "$HOME/reported" ]; then
        : > "$HOME/reported"
        echo "{\"processID\":$TERMINAL_PID,\"eventMessage\":\"LockoutUI changed state: engaged\"}"
    fi
    echo '{"count":0,"finished":1}'
    exit 0
fi
echo $$ > "$HOME/stream.pid"
echo 'Filtering the log data using "stand-in"'
while [ ! -e "$HOME/lift" ]; do sleep 0.1; done
echo "{\"processID\":$TERMINAL_PID,\"eventMessage\":\"LockoutUI changed state: disengaged\"}"
exec sleep 600
"#;

// Stands in for `log` when the test supplies the lockout agent's lines: each
// file dropped in `emit/` is delivered once, in name order, and the debug-level
// stream a launch watches reads `emit-launch/` the same way. A launch's read of
// the requests still open shows `open-requests` when there is one, and the
// launch stream holds back its banner while `hold-banner` exists.
const CUED_LOG: &str = r#"#!/bin/sh
if [ "$1" = show ]; then
    case "$*" in
        *--end*) [ -e "$HOME/open-requests" ] && cat "$HOME/open-requests" ;;
    esac
    echo '{"count":0,"finished":1}'
    exit 0
fi
case "$*" in
    *--debug*) name=launch-stream cues="$HOME/emit-launch" ;;
    *) name=stream cues="$HOME/emit" ;;
esac
echo $$ > "$HOME/$name.pid"
while [ "$name" = launch-stream ] && [ -e "$HOME/hold-banner" ]; do sleep 0.05; done
echo 'Filtering the log data using "stand-in"'
mkdir -p "$cues"
while :; do
    for line in "$cues"/*; do
        [ -e "$line" ] || continue
        cat "$line"
        rm -f "$line"
    done
    sleep 0.05
done
"#;

// Stands in for `alacritty`: with no arguments it launches the terminal, which
// takes a moment to start, and `alacritty --working-directory DIR -e sh -lc
// COMMAND` opens a window. While `late-open` names a delay, the next window
// opens that many seconds late and the open reports failure, as a Ghostty
// script that timed out while Ghostty still added its tab.
const OPEN_WINDOW: &str = r#"#!/usr/bin/env python3
import os, pathlib, subprocess, sys, time
home = pathlib.Path(os.environ["HOME"])
if len(sys.argv) == 1:
    with open(home / "launches", "a") as launches:
        launches.write(f"{os.getpid()}\n")
    (home / "terminal.pid").write_text(str(os.getpid()))
    time.sleep(2)
    os.execv(os.environ["STAND_IN"], [os.environ["STAND_IN"]])
with open(home / "windows", "a") as windows:
    windows.write("opened\n")
command = sys.argv[4:]
late = home / "late-open"
delay = late.read_text().strip() if late.exists() else None
if delay is not None:
    late.unlink()
    command = ["sh", "-c", f'sleep {delay}; : > "$HOME/late-started"; "$@"', "sh", *command]
child = subprocess.Popen(command, cwd=sys.argv[2], stdin=subprocess.DEVNULL,
                         stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                         start_new_session=True)
if delay is not None:
    with open(home / "background.pids", "a") as pids:
        pids.write(f"{child.pid}\n")
    sys.exit(1)
(home / "launcher.pid").write_text(str(child.pid))
"#;

// Stands in for `claude`. `--bg --resume ID` starts a background session and
// registers it the way Claude's hooks do; anything else, such as `attach ID`,
// is recorded and then waits like an interactive session. Claude's background
// service runs each session as its child, and the hook names as its shell the
// launcher of whatever session first started that service, long gone.
const CLAUDE: &str = r#"#!/bin/sh
echo "$*" >> "$HOME/claude.args"
if [ "$1" = --bg ] && [ -e "$HOME/background-fails" ]; then
    exit 1
fi
if [ "$1" = --bg ]; then
    sh -c 'sleep 60 & echo $! > "$HOME/session.pid"; wait' </dev/null >/dev/null 2>&1 &
    echo $! >> "$HOME/background.pids"
    while [ ! -s "$HOME/session.pid" ]; do sleep 0.01; done
    session=$(cat "$HOME/session.pid")
    rm "$HOME/session.pid"
    echo "$session" >> "$HOME/background.pids"
    sh -c : &
    launcher=$!
    wait $launcher
    "$TEST_BINARY" register --tool claude --session-id "$3" --pid "$session" --shell-pid "$launcher" --directory "$PWD" </dev/null >/dev/null
    echo "backgrounded · $(echo "$3" | cut -c1-8)"
    exit 0
fi
exec sleep 30
"#;

unsafe extern "C" {
    fn notify_register_check(name: *const c_char, out_token: *mut c_int) -> u32;
    fn notify_set_state(token: c_int, state: u64) -> u32;
    fn notify_post(name: *const c_char) -> u32;
    fn notify_cancel(token: c_int) -> u32;
}

/// Stands in for loginwindow's lock notifications under names of the test's
/// own. Holding a registration keeps the lock state alive between commands.
struct Screen {
    notices: String,
    token: c_int,
}

impl Screen {
    fn new(notices: String) -> Self {
        let locked = CString::new(format!("{notices}.screenIsLocked")).unwrap();
        let mut token = 0;
        assert_eq!(
            unsafe { notify_register_check(locked.as_ptr(), &raw mut token) },
            0
        );
        Self { notices, token }
    }

    fn lock(&self) {
        assert_eq!(unsafe { notify_set_state(self.token, 1) }, 0);
    }

    /// As loginwindow does: the state first, then the unlock notice.
    fn unlock(&self) {
        assert_eq!(unsafe { notify_set_state(self.token, 0) }, 0);
        let unlocked = CString::new(format!("{}.screenIsUnlocked", self.notices)).unwrap();
        assert_eq!(unsafe { notify_post(unlocked.as_ptr()) }, 0);
    }
}

impl Drop for Screen {
    fn drop(&mut self) {
        unsafe {
            notify_cancel(self.token);
        }
    }
}

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
    terminal: Option<Child>,
    daemon: Option<Child>,
    // A launch is answered for only while Screen Time's lockout agent runs;
    // the daemon looks for this stand-in instead.
    agent: Option<Child>,
    screen: Screen,
    _one_terminal: std::sync::MutexGuard<'static, ()>,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if let Some(daemon) = self.daemon.as_mut() {
            let _ = daemon.kill();
            let _ = daemon.wait();
        }
        if let Some(terminal) = self.terminal.as_mut() {
            let _ = terminal.kill();
            let _ = terminal.wait();
        }
        if let Some(agent) = self.agent.as_mut() {
            let _ = agent.kill();
            let _ = agent.wait();
        }
        // The next fixture finds the terminal by name, so a killed one must be
        // gone before this one lets go of `ONE_TERMINAL`.
        for (file, group) in [
            ("launcher.pid", true),
            ("stream.pid", false),
            ("launch-stream.pid", false),
            ("terminal.pid", false),
        ] {
            if let Some(pid) = self.pid_in(file) {
                unsafe {
                    libc::kill(if group { -pid } else { pid }, libc::SIGKILL);
                }
                let deadline = Instant::now() + Duration::from_secs(5);
                while alive(pid) && Instant::now() < deadline {
                    thread::sleep(Duration::from_millis(20));
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
        Self::build(Some(tool), LOG, true, true)
    }

    /// No terminal is running, so the daemon launches one for its restore.
    fn launching(tool: &str) -> Self {
        Self::build(Some(tool), CUED_LOG, false, true)
    }

    /// The terminal is running and Screen Time has not locked it out.
    fn unlocked(tool: &str) -> Self {
        Self::build(Some(tool), CUED_LOG, true, true)
    }

    /// The same, on a Mac where Screen Time's lockout agent is not running.
    fn launching_without_agent(tool: &str) -> Self {
        Self::build(Some(tool), CUED_LOG, false, false)
    }

    /// No terminal is running and nothing waits to be restored.
    fn idle() -> Self {
        Self::build(None, CUED_LOG, false, true)
    }

    fn build(tool: Option<&str>, log: &str, terminal_running: bool, agent_running: bool) -> Self {
        let one_terminal = ONE_TERMINAL
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let home = tempfile::tempdir().unwrap();
        let root = home.path();
        let screen = Screen::new(format!(
            "test.session-guard.{}",
            root.file_name().unwrap().to_string_lossy()
        ));
        let config = root.join(".config/session-guard");
        let bin = root.join("bin");
        fs::create_dir_all(&config).unwrap();
        fs::create_dir(&bin).unwrap();
        for (name, contents) in [
            ("log", log),
            ("alacritty", OPEN_WINDOW),
            ("claude", CLAUDE),
            ("codex", "#!/bin/sh\nexec sleep 30\n"),
        ] {
            fs::write(bin.join(name), contents).unwrap();
            fs::set_permissions(bin.join(name), fs::Permissions::from_mode(0o755)).unwrap();
        }
        let terminal =
            terminal_running.then(|| Command::new(stand_in_terminal_path()).spawn().unwrap());
        let agent = agent_running.then(|| {
            fs::copy(stand_in_terminal_path(), root.join(AGENT)).unwrap();
            Command::new(root.join(AGENT)).spawn().unwrap()
        });

        let idle_since = Utc::now() - Age::days(1);
        fs::write(config.join("terminal"), "alacritty\n").unwrap();
        let sessions = tool.map(|tool| {
            json!({
                "session_id": SESSION, "tool": tool, "pid": 2_147_483_647,
                "shell_pid": 2_147_483_646, "directory": root, "session_name": null,
                "registered_at": idle_since, "last_seen_at": idle_since, "state": "recoverable",
                "dead_at": idle_since, "restore_pending": true, "transcript_path": null
            })
        });
        fs::write(
            config.join("active-sessions.json"),
            json!(sessions.into_iter().collect::<Vec<_>>()).to_string(),
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
            agent,
            screen,
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
            .env("TERMINAL_PID", self.terminal_pid().to_string())
            .env("STAND_IN", stand_in_terminal_path())
            .env("SESSION_GUARD_LOCKOUT_AGENT", AGENT)
            .env("SESSION_GUARD_SCREEN_NOTICES", &self.screen.notices)
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

    fn start_agent(&mut self) {
        self.agent = Some(Command::new(self.root().join(AGENT)).spawn().unwrap());
    }

    fn stop_agent(&mut self) {
        let mut agent = self.agent.take().unwrap();
        agent.kill().unwrap();
        agent.wait().unwrap();
    }

    fn launches(&self) -> usize {
        fs::read_to_string(self.root().join("launches"))
            .unwrap_or_default()
            .lines()
            .count()
    }

    fn terminal_pid(&self) -> i32 {
        self.terminal.as_ref().map_or_else(
            || self.pid_in("terminal.pid").unwrap_or_default(),
            |terminal| i32::try_from(terminal.id()).unwrap(),
        )
    }

    /// Has the cued `log` stand-in's stream deliver `line`, after anything
    /// named before `name`.
    fn emit(&self, name: &str, line: &Value) {
        self.cue("emit", name, line);
    }

    /// The same, for the stream watching a launch.
    fn emit_launch(&self, name: &str, line: &Value) {
        self.cue("emit-launch", name, line);
    }

    fn cue(&self, folder: &str, name: &str, line: &Value) {
        let cues = self.root().join(folder);
        fs::create_dir_all(&cues).unwrap();
        let staged = cues.join(format!(".{name}"));
        fs::write(&staged, format!("{line}\n")).unwrap();
        fs::rename(staged, cues.join(name)).unwrap();
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
    assert_eq!(fixture.record()["attach_keeps_selection"], false);
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
    assert!(
        fixture
            .daemon_log()
            .contains("Resumed 1 in the background: alacritty is locked out by Screen Time."),
        "{}",
        fixture.daemon_log()
    );
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
    assert_eq!(fixture.record()["attach_keeps_selection"], true);
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

#[test]
fn a_failed_background_start_keeps_the_work_it_would_have_resumed() {
    let mut fixture = Fixture::new("claude");
    let registry = fixture
        .root()
        .join(".config/session-guard/active-sessions.json");
    let mut records: Value = serde_json::from_slice(&fs::read(&registry).unwrap()).unwrap();
    records[0]["background_tasks"] =
        json!([{"id": "t1", "type": "local_bash", "description": "build"}]);
    fs::write(&registry, records.to_string()).unwrap();
    fs::write(fixture.root().join("background-fails"), "").unwrap();

    fixture.start_daemon();
    fixture.wait_for("the failed background start", || {
        fixture.daemon_log().contains("failed to resume")
    });
    fixture.wait_for("the session to be held", || {
        fixture.record()["held_for_lockout"] == true
    });
    let record = fixture.record();
    assert_eq!(record["background_tasks"][0]["id"], "t1", "{record}");
    assert_eq!(record["restore_pending"], true);
    assert_eq!(record["attach_after_lockout"], false);
    fixture.stop_daemon();
}

#[test]
fn behind_a_locked_screen_claude_resumes_in_the_background_and_gets_its_tab_on_unlock() {
    let mut fixture = Fixture::unlocked("claude");
    fixture.screen.lock();
    fixture.start_daemon();
    fixture.wait_for("the background resume", || {
        fixture
            .daemon_log()
            .contains("Resumed 1 in the background: the screen is locked.")
    });
    let locked = fixture.lockout();
    assert_eq!(locked.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&locked.stderr).contains("the screen is locked"));
    let mut waiter = fixture.command(&["lockout", "--wait"]).spawn().unwrap();

    // The attach pass runs every five seconds and must open nothing behind
    // the lock screen.
    thread::sleep(Duration::from_secs(6));
    let claude_args = || fs::read_to_string(fixture.root().join("claude.args")).unwrap_or_default();
    assert!(!claude_args().contains("attach"), "{}", claude_args());
    assert!(
        waiter.try_wait().unwrap().is_none(),
        "--wait returned behind the lock screen"
    );

    fixture.screen.unlock();
    fixture.wait_for("--wait to return", || waiter.try_wait().unwrap().is_some());
    assert_eq!(waiter.wait().unwrap().code(), Some(0));
    fixture.wait_for("the attach tab", || {
        claude_args().contains(&format!("attach {}", &SESSION[..8]))
    });
    fixture.wait_for("the attach flag to clear", || {
        fixture.record()["attach_after_lockout"] == false
    });
    fixture.stop_daemon();
}

/// A tab whose open reported failure but which opened anyway, `late` seconds
/// after; returns the windows opened and the attaches the tabs ran.
fn attach_with_a_late_tab(late: u64) -> (usize, usize) {
    let mut fixture = Fixture::unlocked("claude");
    fixture.screen.lock();
    fixture.start_daemon();
    fixture.wait_for("the background resume", || {
        fixture.record()["attach_after_lockout"] == true
    });
    fs::write(fixture.root().join("late-open"), late.to_string()).unwrap();
    fixture.screen.unlock();
    fixture.wait_for("the late tab", || {
        fixture.root().join("late-started").exists()
    });
    fixture.wait_for("the attach flag to clear", || {
        fixture.record()["attach_after_lockout"] == false
    });
    assert!(
        fixture.daemon_log().contains("which may still open"),
        "{}",
        fixture.daemon_log()
    );
    // Long enough for another attach pass to open a third tab if it would.
    thread::sleep(Duration::from_secs(6));
    let count = |file: &str, line: &str| {
        fs::read_to_string(fixture.root().join(file))
            .unwrap_or_default()
            .lines()
            .filter(|found| found.starts_with(line))
            .count()
    };
    let opened = (count("windows", "opened"), count("claude.args", "attach"));
    assert!(
        fixture.record()["attach_tab_pid"].is_number(),
        "{}",
        fixture.record()
    );
    fixture.stop_daemon();
    opened
}

#[test]
fn a_tab_that_opens_after_its_open_failed_is_not_opened_again() {
    assert_eq!(attach_with_a_late_tab(0), (1, 1));
}

#[test]
fn a_tab_that_opens_after_its_retry_closes_and_leaves_one_attach() {
    assert_eq!(attach_with_a_late_tab(8), (2, 1));
}

#[test]
fn behind_a_locked_screen_no_terminal_launches_until_it_unlocks() {
    let mut fixture = Fixture::launching("codex");
    fixture.screen.lock();
    fixture.start_daemon();
    fixture.wait_for("the startup pass to hold", || {
        fixture
            .daemon_log()
            .contains("Held 1 sessions: the screen is locked.")
    });
    thread::sleep(Duration::from_millis(1500));
    assert_eq!(fixture.launches(), 0);
    assert_eq!(fixture.record()["held_for_lockout"], true);

    fixture.screen.unlock();
    fixture.wait_for("the launch", || {
        fixture.pid_in("launch-stream.pid").is_some_and(alive)
            && fixture.pid_in("terminal.pid").is_some_and(alive)
    });
    fixture.emit_launch("1", &born_line());
    fixture.emit_launch("2", &requested());
    fixture.emit_launch("3", &answered());
    fixture.wait_for("the held restore", || {
        fixture
            .daemon_log()
            .contains("held restore: Restored 1 sessions")
    });
    assert_eq!(fixture.launches(), 1);
    fixture.stop_daemon();
}

fn agent_line(message: &str) -> Value {
    agent_line_on(message, 7)
}

fn agent_line_on(message: &str, thread: u64) -> Value {
    json!({"eventMessage": message, "processID": 762, "threadID": thread})
}

fn born_line() -> Value {
    born("org.alacritty", "Foreground")
}

fn born(app: &str, kind: &str) -> Value {
    agent_line(&format!(
        r#"Calling block with kLSNotifyApplicationBirth {{ "ApplicationType"="{kind}", , "BuildMachineOSBuild"="23A344", "BundleIdentifierLowerCase"="{app}", "CFBundleIdentifier"="{app}"<…> notificationID=<decode: missing data> id=<decode: missing data>"#,
    ))
}

fn checkin_line(pid: i32) -> Value {
    json!({"eventMessage": format!("CHECKIN:0x0-0x32e62e3 {pid} org.alacritty"), "processID": 354})
}

fn requested() -> Value {
    agent_line("Requesting policy for bundle IDs: <private>")
}

fn answered() -> Value {
    answered_on(7)
}

fn answered_on(thread: u64) -> Value {
    agent_line_on(
        "Successfully requested policies for 1 bundleIdentifiers",
        thread,
    )
}

fn began() -> Value {
    began_on(7)
}

fn began_on(thread: u64) -> Value {
    agent_line_on("Begin lockout on app: <private>", thread)
}

fn deciding(output: &Output) -> bool {
    output.status.code() == Some(2)
        && String::from_utf8_lossy(&output.stderr).contains("has not answered")
}

#[test]
fn a_restore_launches_the_terminal_alone_and_opens_tabs_once_screen_time_answers() {
    let mut fixture = Fixture::launching("codex");
    fixture.start_daemon();
    fixture.wait_for("the launch", || {
        fixture.pid_in("launch-stream.pid").is_some_and(alive)
            && fixture.pid_in("terminal.pid").is_some_and(alive)
    });
    // Apps launching alongside are heard of in turn, and only foreground ones
    // are asked about; as logged, both notices come before both requests.
    fixture.emit_launch("1", &born("com.apple.dictionary", "Foreground"));
    fixture.emit_launch("2", &born("com.apple.talagent", "UIElement"));
    fixture.emit_launch("3", &born_line());
    fixture.emit_launch("4", &requested());
    fixture.emit_launch("5", &requested());
    fixture.emit_launch("6", &answered());
    thread::sleep(Duration::from_millis(1500));
    assert!(deciding(&fixture.lockout()));
    assert!(!fixture.root().join("launcher.pid").exists());
    assert!(
        fixture
            .daemon_log()
            .contains("Held 1 sessions: Screen Time has not answered for alacritty's launch yet.")
    );

    fixture.emit_launch("7", &answered());
    fixture.wait_for("the held restore", || {
        fixture
            .daemon_log()
            .contains("held restore: Restored 1 sessions")
    });
    assert_eq!(fixture.lockout().status.code(), Some(0));
    assert!(!fixture.pid_in("launch-stream.pid").is_some_and(alive));
    assert_eq!(fixture.launches(), 1);
    fixture.stop_daemon();
}

#[test]
fn a_terminal_locked_out_at_launch_gets_no_tab_until_the_lockout_lifts() {
    let mut fixture = Fixture::launching("codex");
    fixture.start_daemon();
    fixture.wait_for("the launch", || {
        fixture.pid_in("launch-stream.pid").is_some_and(alive)
            && fixture.pid_in("terminal.pid").is_some_and(alive)
    });
    // Answered on two threads, the lockout comes on the terminal's.
    fixture.emit_launch("1", &born_line());
    fixture.emit_launch("2", &born("com.apple.dictionary", "Foreground"));
    fixture.emit_launch("3", &requested());
    fixture.emit_launch("4", &requested());
    fixture.emit_launch("5", &answered_on(7));
    fixture.emit_launch("6", &answered_on(9));
    fixture.emit_launch("7", &began_on(7));
    fixture.wait_for("the lockout to be known", || {
        fixture.lockout().status.code() == Some(1)
    });
    let engaged = json!({
        "eventMessage": "LockoutUI changed state: engaged",
        "processID": fixture.terminal_pid(),
    });
    fixture.emit("1", &engaged);
    thread::sleep(Duration::from_millis(1500));
    assert_eq!(fixture.lockout().status.code(), Some(1));
    assert!(!fixture.root().join("launcher.pid").exists());
    assert!(!fixture.daemon_log().contains("Restored 1"));

    let disengaged = json!({
        "eventMessage": "LockoutUI changed state: disengaged",
        "processID": fixture.terminal_pid(),
    });
    fixture.emit("2", &disengaged);
    fixture.wait_for("the held restore", || {
        fixture
            .daemon_log()
            .contains("held restore: Restored 1 sessions")
    });
    assert_eq!(fixture.launches(), 1);
    fixture.stop_daemon();
}

#[test]
fn lockout_wait_launches_the_terminal_and_returns_once_screen_time_answers() {
    let mut fixture = Fixture::idle();
    fixture.start_daemon();
    fixture.wait_for("the closed terminal to read as not running", || {
        String::from_utf8_lossy(&fixture.lockout().stderr).contains("is not running")
    });
    fixture.wait_for("the watcher's first read", || {
        fs::read(fixture.state_file())
            .ok()
            .and_then(|state| serde_json::from_slice::<Value>(&state).ok())
            .is_some_and(|state| state["seed"] != "Reading")
    });
    let mut waiting = fixture
        .command(&["lockout", "--wait"])
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    fixture.wait_for("the launch", || {
        fixture.pid_in("launch-stream.pid").is_some_and(alive)
            && fixture.pid_in("terminal.pid").is_some_and(alive)
    });
    thread::sleep(Duration::from_millis(500));
    assert!(waiting.try_wait().unwrap().is_none());
    assert!(deciding(&fixture.lockout()));

    fixture.emit_launch("1", &born_line());
    fixture.emit_launch("2", &requested());
    fixture.emit_launch("3", &answered());
    fixture.wait_for("lockout --wait to return", || {
        waiting.try_wait().unwrap().is_some()
    });
    assert!(waiting.wait().unwrap().success());
    assert_eq!(fixture.launches(), 1);
    fixture.stop_daemon();
}

#[test]
fn a_terminal_launched_by_anyone_else_waits_for_the_answer_after_its_check_in() {
    let mut fixture = Fixture::idle();
    fixture.start_daemon();
    fixture.wait_for("the closed terminal to read as not running", || {
        String::from_utf8_lossy(&fixture.lockout().stderr).contains("is not running")
    });
    fixture.wait_for("the watcher's first read", || {
        fs::read(fixture.state_file())
            .ok()
            .and_then(|state| serde_json::from_slice::<Value>(&state).ok())
            .is_some_and(|state| state["seed"] != "Reading")
    });

    fixture.terminal = Some(Command::new(stand_in_terminal_path()).spawn().unwrap());
    assert!(deciding(&fixture.lockout()));
    fixture.emit("1", &requested());
    fixture.emit("2", &checkin_line(fixture.terminal_pid()));
    fixture.emit("3", &answered());
    fixture.emit("4", &policy_changed());
    fixture.emit("5", &requested());
    fixture.emit(
        "6",
        &agent_line("Successfully requested policies for 12 bundleIdentifiers"),
    );
    fixture.emit("7", &began());
    thread::sleep(Duration::from_millis(1000));
    assert!(deciding(&fixture.lockout()));
    fixture.emit("8", &requested());
    fixture.emit("9", &answered());
    fixture.wait_for("the answer", || fixture.lockout().status.code() == Some(0));

    let mut unlocked = fixture
        .terminal
        .replace(Command::new(stand_in_terminal_path()).spawn().unwrap())
        .unwrap();
    unlocked.kill().unwrap();
    unlocked.wait().unwrap();
    assert!(deciding(&fixture.lockout()));
    fixture.emit("a", &checkin_line(fixture.terminal_pid()));
    fixture.emit("b", &requested());
    fixture.emit("c", &answered());
    fixture.emit("d", &began());
    fixture.wait_for("the lockout", || fixture.lockout().status.code() == Some(1));
    fixture.stop_daemon();
}

#[test]
fn without_screen_times_lockout_agent_a_launched_terminal_takes_tabs_at_once() {
    let mut fixture = Fixture::launching_without_agent("codex");
    fixture.start_daemon();
    fixture.wait_for("the held restore", || {
        fixture
            .daemon_log()
            .contains("held restore: Restored 1 sessions")
    });
    assert!(!fixture.root().join("launch-stream.pid").exists());
    assert_eq!(fixture.lockout().status.code(), Some(0));
    assert_eq!(fixture.launches(), 1);
    fixture.stop_daemon();
}

fn restored_without_a_tab_first(fixture: &Fixture) {
    thread::sleep(Duration::from_millis(1000));
    assert!(deciding(&fixture.lockout()));
    assert!(!fixture.root().join("launcher.pid").exists());
}

fn wait_for_launch(fixture: &Fixture) {
    fixture.wait_for("the launch", || {
        fixture.pid_in("launch-stream.pid").is_some_and(alive)
            && fixture.pid_in("terminal.pid").is_some_and(alive)
    });
}

fn wait_for_held_restore(fixture: &Fixture) {
    fixture.wait_for("the held restore", || {
        fixture
            .daemon_log()
            .contains("held restore: Restored 1 sessions")
    });
}

#[test]
fn a_failed_request_for_the_launch_answers_it() {
    let mut fixture = Fixture::launching("codex");
    fixture.start_daemon();
    wait_for_launch(&fixture);
    fixture.emit_launch("1", &born_line());
    fixture.emit_launch("2", &requested());
    restored_without_a_tab_first(&fixture);
    fixture.emit_launch("3", &agent_line("Error requesting policies: <private>"));
    wait_for_held_restore(&fixture);
    assert_eq!(fixture.lockout().status.code(), Some(0));
    fixture.stop_daemon();
}

#[test]
fn answers_for_requests_open_before_the_launch_come_first() {
    let mut fixture = Fixture::launching("codex");
    // Asked a moment before the launch, and one asked after it that the
    // stream delivers instead.
    let at = |offset: i64| {
        (Utc::now() + Age::seconds(offset))
            .format("%Y-%m-%d %H:%M:%S%.6f+0000")
            .to_string()
    };
    let mut asked = requested();
    asked["timestamp"] = json!(at(-5));
    let mut later = requested();
    later["timestamp"] = json!(at(30));
    fs::write(
        fixture.root().join("open-requests"),
        format!("{asked}\n{later}\n"),
    )
    .unwrap();
    fixture.start_daemon();
    wait_for_launch(&fixture);
    fixture.emit_launch("1", &born_line());
    fixture.emit_launch("2", &requested());
    fixture.emit_launch("3", &answered());
    // Downtime starting meanwhile: one answer for every running app, then
    // its lockouts.
    fixture.emit_launch("4", &requested());
    fixture.emit_launch(
        "5",
        &agent_line("Successfully requested policies for 12 bundleIdentifiers"),
    );
    fixture.emit_launch("6", &began());
    fixture.emit_launch("7", &began());
    restored_without_a_tab_first(&fixture);
    fixture.emit_launch("8", &answered());
    wait_for_held_restore(&fixture);
    assert_eq!(fixture.lockout().status.code(), Some(0));
    fixture.stop_daemon();
}

#[test]
fn an_agent_that_exits_ends_the_wait_for_its_answer() {
    let mut fixture = Fixture::launching("codex");
    fixture.start_daemon();
    wait_for_launch(&fixture);
    fixture.emit_launch("1", &born_line());
    fixture.emit_launch("2", &requested());
    restored_without_a_tab_first(&fixture);
    fixture.stop_agent();
    wait_for_held_restore(&fixture);
    assert!(!fixture.pid_in("launch-stream.pid").is_some_and(alive));
    fixture.stop_daemon();
}

#[test]
fn a_terminal_already_starting_when_its_launch_is_watched_is_followed_from_its_check_in() {
    let mut fixture = Fixture::idle();
    fs::write(fixture.root().join("hold-banner"), "").unwrap();
    fixture.start_daemon();
    fixture.wait_for("the closed terminal to read as not running", || {
        String::from_utf8_lossy(&fixture.lockout().stderr).contains("is not running")
    });
    let mut waiting = fixture
        .command(&["lockout", "--wait"])
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    fixture.wait_for("the launch stream", || {
        fixture.pid_in("launch-stream.pid").is_some_and(alive)
    });
    fixture.terminal = Some(Command::new(stand_in_terminal_path()).spawn().unwrap());
    fs::remove_file(fixture.root().join("hold-banner")).unwrap();
    fixture.wait_for("the launch stream to close", || {
        !fixture.pid_in("launch-stream.pid").is_some_and(alive)
    });
    thread::sleep(Duration::from_millis(500));
    assert!(waiting.try_wait().unwrap().is_none());
    assert!(deciding(&fixture.lockout()));

    fixture.emit("1", &checkin_line(fixture.terminal_pid()));
    fixture.emit("2", &requested());
    fixture.emit("3", &answered());
    fixture.wait_for("lockout --wait to return", || {
        waiting.try_wait().unwrap().is_some()
    });
    assert!(waiting.wait().unwrap().success());
    assert_eq!(fixture.launches(), 0);
    fixture.stop_daemon();
}

fn policy_changed() -> Value {
    agent_line("Received policy changed notification for registration dmf.policy.monitor.app")
}

fn policy_answered() -> Value {
    agent_line_on(
        "Successfully requested policies for 12 bundleIdentifiers",
        9,
    )
}

fn a_policy_request_open_at_launch_is_no_launches(policy_answered_first: bool) {
    let mut fixture = Fixture::launching("codex");
    let asked = |line: Value| {
        let mut line = line;
        line["timestamp"] = json!(
            (Utc::now() - Age::seconds(1))
                .format("%Y-%m-%d %H:%M:%S%.6f+0000")
                .to_string()
        );
        line
    };
    fs::write(
        fixture.root().join("open-requests"),
        format!("{}\n{}\n", asked(policy_changed()), asked(requested())),
    )
    .unwrap();
    fixture.start_daemon();
    wait_for_launch(&fixture);
    fixture.emit_launch("1", &born_line());
    fixture.emit_launch("2", &requested());
    if policy_answered_first {
        fixture.emit_launch("3", &policy_answered());
        fixture.emit_launch("4", &began_on(9));
        fixture.emit_launch("5", &answered());
    } else {
        fixture.emit_launch("3", &answered());
        fixture.emit_launch("4", &policy_answered());
        fixture.emit_launch("5", &began_on(9));
    }
    wait_for_held_restore(&fixture);
    assert_eq!(fixture.lockout().status.code(), Some(0));
    fixture.stop_daemon();
}

#[test]
fn a_policy_request_open_at_launch_answered_first_is_no_launches() {
    a_policy_request_open_at_launch_is_no_launches(true);
}

#[test]
fn a_policy_request_open_at_launch_answered_last_is_no_launches() {
    a_policy_request_open_at_launch_is_no_launches(false);
}

#[test]
fn a_policy_request_open_at_a_check_in_is_no_launches_in_either_order() {
    let mut fixture = Fixture::idle();
    fixture.start_daemon();
    fixture.wait_for("the watcher's first read", || {
        fs::read(fixture.state_file())
            .ok()
            .and_then(|state| serde_json::from_slice::<Value>(&state).ok())
            .is_some_and(|state| state["seed"] != "Reading")
    });
    for (round, policy_answered_first) in [("a", true), ("b", false)] {
        if let Some(mut previous) = fixture
            .terminal
            .replace(Command::new(stand_in_terminal_path()).spawn().unwrap())
        {
            previous.kill().unwrap();
            previous.wait().unwrap();
        }
        let cue = |step: u32| format!("{round}{step}");
        fixture.emit(&cue(1), &policy_changed());
        fixture.emit(&cue(2), &requested());
        fixture.emit(&cue(3), &checkin_line(fixture.terminal_pid()));
        fixture.emit(&cue(4), &requested());
        if policy_answered_first {
            fixture.emit(&cue(5), &policy_answered());
            fixture.emit(&cue(6), &began_on(9));
            fixture.emit(&cue(7), &answered());
        } else {
            fixture.emit(&cue(5), &answered());
            fixture.emit(&cue(6), &policy_answered());
            fixture.emit(&cue(7), &began_on(9));
        }
        fixture.wait_for("the answer", || fixture.lockout().status.code() == Some(0));
    }
    fixture.stop_daemon();
}

#[test]
fn an_agent_that_exits_takes_its_open_requests_with_it() {
    let mut fixture = Fixture::idle();
    fixture.start_daemon();
    fixture.wait_for("the watcher's first read", || {
        fs::read(fixture.state_file())
            .ok()
            .and_then(|state| serde_json::from_slice::<Value>(&state).ok())
            .is_some_and(|state| state["seed"] != "Reading")
    });
    fixture.emit("1", &requested());
    thread::sleep(Duration::from_millis(500));
    fixture.stop_agent();
    fixture.start_agent();
    thread::sleep(Duration::from_millis(1500));

    fixture.terminal = Some(Command::new(stand_in_terminal_path()).spawn().unwrap());
    fixture.emit("2", &checkin_line(fixture.terminal_pid()));
    fixture.emit("3", &requested());
    fixture.emit("4", &answered());
    fixture.wait_for("the answer", || fixture.lockout().status.code() == Some(0));
    fixture.stop_daemon();
}
