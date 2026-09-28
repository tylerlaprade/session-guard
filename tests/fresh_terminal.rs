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
    child: Option<Child>,
    output: String,
    helpers: Vec<Child>,
}

impl Drop for Terminal {
    fn drop(&mut self) {
        if let Some(child) = self.child.as_mut() {
            let foreground = self
                .master
                .as_ref()
                .map_or(-1, |master| unsafe { libc::tcgetpgrp(master.as_raw_fd()) });
            unsafe {
                if foreground > 0 {
                    libc::kill(-foreground, libc::SIGKILL);
                }
                libc::kill(-(child.id() as i32), libc::SIGKILL);
            }
            let _ = child.kill();
        }
        drop(self.master.take());
        if let Some(child) = self.child.as_mut() {
            let _ = child.wait();
        }
        for helper in &mut self.helpers {
            let _ = helper.kill();
            let _ = helper.wait();
        }
    }
}

impl Terminal {
    fn prepare() -> Self {
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
                "state":"recoverable", "restore_pending":true,
                "shell_pid":2_147_483_647, "shell_pid_started_at":"Wed Jan 1 00:00:00 2020",
                "tab_position":[1,1]
            },{
                "session_id":"second-session", "tool":"codex", "directory":root,
                "registered_at":"2020-01-01T00:00:00Z", "session_name":null,
                "state":"recoverable", "restore_pending":true,
                "shell_pid":2_147_483_647, "shell_pid_started_at":"Wed Jan 1 00:00:00 2020",
                "tab_position":[1,2]
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
        let mut terminal = Self {
            home,
            master: None,
            child: None,
            output: String::new(),
            helpers: Vec::new(),
        };
        std::os::unix::fs::symlink("/bin/sh", terminal.bin("ghostty")).unwrap();
        std::os::unix::fs::symlink("/bin/sleep", terminal.bin("session-guard-stub")).unwrap();
        let daemon = terminal
            .command("session-guard-stub", &["60"])
            .spawn()
            .unwrap();
        fs::write(config.join("daemon.pid"), format!("{}\n", daemon.id())).unwrap();
        terminal.helpers.push(daemon);
        terminal
    }

    fn bin(&self, name: &str) -> std::path::PathBuf {
        self.home.path().join("bin").join(name)
    }

    fn command(&self, name: &str, args: &[&str]) -> Command {
        let root = self.home.path();
        let mut command = Command::new(self.bin(name));
        command
            .args(args)
            .env("HOME", root)
            .env("ZDOTDIR", root)
            .env("SHELL", "/bin/zsh")
            .env("CLAUDE_CONFIG_DIR", root.join(".claude"))
            .env("CODEX_HOME", root.join(".codex"))
            .env("TERM_PROGRAM", "ghostty")
            .env(
                "PATH",
                format!(
                    "{}:/usr/bin:/bin:/usr/sbin:/sbin",
                    root.join("bin").display()
                ),
            );
        command
    }

    fn open_tab(&mut self, queued: &[u8]) {
        self.open_shell(queued, true);
    }

    fn open_shell(&mut self, queued: &[u8], under_a_fresh_terminal: bool) {
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
        let mut command = if under_a_fresh_terminal {
            let mut terminal = Command::new(self.bin("ghostty"));
            terminal.args(["-c", "/bin/zsh -dil; :"]);
            terminal
        } else {
            let mut shell = Command::new("/bin/zsh");
            shell.arg("-dil");
            shell
        };
        command
            .env("HOME", self.home.path())
            .env("ZDOTDIR", self.home.path())
            .env("SHELL", "/bin/zsh")
            .env("CLAUDE_CONFIG_DIR", self.home.path().join(".claude"))
            .env("TERM_PROGRAM", "ghostty")
            .env(
                "PATH",
                format!(
                    "{}:/usr/bin:/bin:/usr/sbin:/sbin",
                    self.home.path().join("bin").display()
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
        self.child = Some(command.spawn().unwrap());
        self.master = Some(master);
    }

    fn offer_file(&self) -> std::path::PathBuf {
        self.home
            .path()
            .join(".config/session-guard/fresh-tab.json")
    }

    fn wait_for(what: &str, done: impl Fn() -> bool, within: Duration) {
        let deadline = Instant::now() + within;
        while !done() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            thread::sleep(Duration::from_millis(50));
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
fn a_filled_offer_runs_the_restore_launch_in_the_fresh_tab() {
    let mut terminal = Terminal::prepare();
    terminal.open_tab(b"");
    let offer_file = terminal.offer_file();
    Terminal::wait_for(
        "the tab's offer",
        || offer_file.exists(),
        Duration::from_secs(5),
    );
    let mut offer: Value = serde_json::from_slice(&fs::read(&offer_file).unwrap()).unwrap();
    offer["answer"] = json!({"Launch": {
        "directory": terminal.home.path(),
        "command": format!("{} launch --session-id first-session", terminal.bin("session-guard").display()),
    }});
    fs::write(&offer_file, offer.to_string()).unwrap();
    terminal.until("RESUMED:--resume first-session");
    assert!(!terminal.output.contains("FRESH_PROMPT>"));
    assert!(!offer_file.exists());
    let records = terminal.records();
    assert_eq!(records[0]["state"], "active");
    assert_eq!(records[1]["restore_pending"], true);
    terminal
        .master
        .as_mut()
        .unwrap()
        .write_all(b"done\n")
        .unwrap();
    terminal.until("FRESH_PROMPT>");
    assert_eq!(terminal.records()[0]["restore_pending"], true);
}

#[test]
fn queued_input_keeps_the_tab_for_the_shell_even_without_a_newline() {
    let mut terminal = Terminal::prepare();
    terminal.open_tab(b"print -r -- USER_INPUT_PRESERVED");
    terminal.until("FRESH_PROMPT>");
    assert!(!terminal.output.contains("RESUMED:"));
    assert!(!terminal.offer_file().exists());
    terminal.output.clear();
    terminal.master.as_mut().unwrap().write_all(b"\n").unwrap();
    terminal.until("\r\nUSER_INPUT_PRESERVED\r\n");
}

#[test]
fn a_declined_offer_gives_the_shell_its_prompt() {
    let mut terminal = Terminal::prepare();
    terminal.open_tab(b"");
    let offer_file = terminal.offer_file();
    Terminal::wait_for(
        "the tab's offer",
        || offer_file.exists(),
        Duration::from_secs(5),
    );
    let mut offer: Value = serde_json::from_slice(&fs::read(&offer_file).unwrap()).unwrap();
    offer["answer"] = json!("Decline");
    fs::write(&offer_file, offer.to_string()).unwrap();
    terminal.until("FRESH_PROMPT>");
    assert!(!terminal.output.contains("RESUMED:"));
    assert!(!offer_file.exists());
    assert_eq!(terminal.records()[0]["state"], "recoverable");
}

#[test]
fn a_shell_without_a_freshly_started_ghostty_makes_no_offer() {
    let mut terminal = Terminal::prepare();
    terminal.open_shell(b"", false);
    terminal.until("FRESH_PROMPT>");
    assert!(!terminal.output.contains("RESUMED:"));
    assert!(!terminal.offer_file().exists());
}
