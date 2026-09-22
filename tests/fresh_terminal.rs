use fs2::FileExt;
use serde_json::{Value, json};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::{fs::PermissionsExt, process::CommandExt};
use std::process::{Child, Command};
use std::thread;
use std::time::{Duration, Instant};

const STARTUP: &str = r"if [[ $ZSH_EVAL_CONTEXT == file && -o login && -o interactive &&
      -z $ZSH_EXECUTION_STRING && $TERM_PROGRAM == ghostty ]] &&
    (( ${+_ghostty_state} && _ghostty_state == 0 )); then
    session-guard shell-start
fi";

struct Terminal {
    home: tempfile::TempDir,
    master: Option<File>,
    child: Child,
    output: String,
    restore_lock: Option<File>,
}

impl Drop for Terminal {
    fn drop(&mut self) {
        unsafe {
            libc::kill(-(self.child.id() as i32), libc::SIGKILL);
        }
        let _ = self.child.kill();
        drop(self.master.take());
        let _ = self.child.wait();
    }
}

impl Terminal {
    fn start(queued: &[u8], locked: bool, pending: bool) -> Self {
        let home = tempfile::tempdir().unwrap();
        let root = home.path();
        let config = root.join(".config/session-guard");
        fs::create_dir_all(&config).unwrap();
        fs::create_dir_all(root.join("Library/LaunchAgents")).unwrap();
        fs::write(
            root.join("Library/LaunchAgents/com.tylerlaprade.session-guard.plist"),
            "installed",
        )
        .unwrap();
        fs::create_dir(root.join("bin")).unwrap();
        fs::write(config.join("terminal"), "ghostty\n").unwrap();
        fs::write(
            config.join("active-sessions.json"),
            json!([{
                "session_id":"first-session", "tool":"claude", "directory":root,
                "registered_at":"2020-01-01T00:00:00Z", "session_name":null,
                "state":"recoverable", "restore_pending":pending,
                "shell_pid":2_147_483_647, "shell_pid_started_at":"Wed Jan 1 00:00:00 2020"
            },{
                "session_id":"second-session", "tool":"codex", "directory":root,
                "registered_at":"2020-01-01T00:00:00Z", "session_name":null,
                "state":"recoverable", "restore_pending":pending,
                "shell_pid":2_147_483_647, "shell_pid_started_at":"Wed Jan 1 00:00:00 2020"
            }])
            .to_string(),
        )
        .unwrap();
        fs::write(root.join("bin/osascript"), "#!/bin/sh\nprintf true\n").unwrap();
        fs::set_permissions(
            root.join("bin/osascript"),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        std::os::unix::fs::symlink(
            env!("CARGO_BIN_EXE_session-guard"),
            root.join("bin/session-guard"),
        )
        .unwrap();
        fs::write(root.join(".zshrc"), format!("typeset -gi _ghostty_state=0\nclaude() {{ print -r -- \"RESUMED:$*\"; read -r answer; return 17; }}\n{STARTUP}\nPROMPT='FRESH_PROMPT> '\n")).unwrap();
        let restore_lock = locked.then(|| {
            let lock = File::create(config.join("restore.lock")).unwrap();
            lock.lock_exclusive().unwrap();
            lock
        });
        let (mut master, mut slave) = (-1, -1);
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
        master.write_all(queued).unwrap();
        let mut command = Command::new("/bin/zsh");
        command
            .arg("-dil")
            .env("HOME", root)
            .env("ZDOTDIR", root)
            .env("SHELL", "/bin/zsh")
            .env("CLAUDE_CONFIG_DIR", root.join(".claude"))
            .env("TERM_PROGRAM", "ghostty")
            .env(
                "PATH",
                format!(
                    "{}:/usr/bin:/bin:/usr/sbin:/sbin",
                    root.join("bin").display()
                ),
            )
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
        Self {
            child: command.spawn().unwrap(),
            home,
            master: Some(master),
            output: String::new(),
            restore_lock,
        }
    }

    fn until(&mut self, text: &str) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !self.output.contains(text) {
            let mut buffer = [0; 4096];
            if let Ok(count) = self.master.as_mut().unwrap().read(&mut buffer) {
                self.output
                    .push_str(&String::from_utf8_lossy(&buffer[..count]));
            }
            assert!(
                Instant::now() < deadline,
                "expected {text:?}: {}",
                self.output
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn records(&self) -> Value {
        serde_json::from_slice(
            &fs::read(
                self.home
                    .path()
                    .join(".config/session-guard/active-sessions.json"),
            )
            .unwrap(),
        )
        .unwrap()
    }
}

#[test]
fn first_prompt_claims_one_session_and_preserves_the_interactive_wrapper() {
    let mut terminal = Terminal::start(b"", false, true);
    terminal.until("RESUMED:--resume first-session");
    assert!(!terminal.output.contains("FRESH_PROMPT>"));
    let records = terminal.records();
    assert_eq!(records[0]["state"], "active");
    assert_eq!(records[0]["restore_pending"], false);
    assert_eq!(records[1]["restore_pending"], true);
    assert!(records[0]["pid"].as_u64().unwrap() > 1);
    let lock = File::open(
        terminal
            .home
            .path()
            .join(".config/session-guard/restore.lock"),
    )
    .unwrap();
    lock.try_lock_exclusive().unwrap();
    drop(lock);
    terminal
        .master
        .as_mut()
        .unwrap()
        .write_all(b"done\n")
        .unwrap();
    terminal.until("FRESH_PROMPT>");
    assert_eq!(terminal.records()[0]["restore_pending"], true);
    terminal.output.clear();
    terminal
        .master
        .as_mut()
        .unwrap()
        .write_all(b"source ~/.zshrc\n")
        .unwrap();
    terminal.until("FRESH_PROMPT>");
    assert!(!terminal.output.contains("RESUMED:"));
}

#[test]
fn queued_input_is_left_for_the_shell_even_without_a_newline() {
    let mut terminal = Terminal::start(b"print -r -- USER_INPUT_PRESERVED", false, true);
    terminal.until("FRESH_PROMPT>");
    assert!(!terminal.output.contains("RESUMED:"));
    assert_eq!(terminal.records()[0]["restore_pending"], true);
    terminal.output.clear();
    terminal.master.as_mut().unwrap().write_all(b"\n").unwrap();
    terminal.until("\r\nUSER_INPUT_PRESERVED\r\n");
}

#[test]
fn daemon_restore_and_legacy_unknown_records_leave_the_shell_alone() {
    for (locked, pending) in [(true, true), (false, false)] {
        let mut terminal = Terminal::start(b"", locked, pending);
        terminal.until("FRESH_PROMPT>");
        assert!(!terminal.output.contains("RESUMED:"));
        assert_eq!(terminal.records()[0]["state"], "recoverable");
        drop(terminal.restore_lock.take());
    }
}
