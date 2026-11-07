use super::{Selection, TerminalAdapter, run_capture_timeout, run_checked};
use crate::{TerminalKind, process};
use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

/// Names the block, Rex's terminal pane, in the environment of everything
/// started in it. A block keeps its id when Rex brings it back after a crash.
pub const BLOCK_VARIABLE: &str = "REX_BLOCK";

// The first command after a reboot starts the server, which brings back every
// saved session before it answers.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(30);
const FIND_APP_TIMEOUT: Duration = Duration::from_secs(5);
const PROMPT_WAIT: Duration = Duration::from_secs(5);
const PROMPT_POLL: Duration = Duration::from_millis(200);
const ERASE_LINE: char = '\u{15}';

pub struct Rex;

/// The `rex` command: on PATH inside Rex's own terminals, and otherwise only
/// inside the app, which `LaunchServices` finds under whatever name it has.
pub fn executable() -> Result<PathBuf> {
    if process::command_exists("rex") {
        return Ok(PathBuf::from("rex"));
    }
    let bundle_id = TerminalKind::Rex.bundle_id();
    let mut osa = Command::new("osascript");
    osa.args(["-e", include_str!("rex_app_path.applescript"), bundle_id]);
    let app = run_capture_timeout(&mut osa, "finding the Rex app", FIND_APP_TIMEOUT)?;
    bundled_executable(Path::new(&app)).with_context(|| {
        format!("Rex is not installed: no `rex` in PATH and no app with bundle id {bundle_id}")
    })
}

fn bundled_executable(app: &Path) -> Option<PathBuf> {
    std::fs::read_dir(app.join("Contents/Helpers"))
        .ok()?
        .flatten()
        .map(|helper| helper.path().join("Contents/MacOS/rex"))
        .find(|candidate| candidate.is_file())
}

fn new_session_command(rex: &Path, directory: &Path) -> Command {
    let mut new = Command::new(rex);
    new.arg("new");
    new.args(session_label(directory));
    new.arg("-c").arg(directory).arg("--json");
    new
}

// Rex takes a leading dash for a flag, and names a session itself given no label.
fn session_label(directory: &Path) -> Option<String> {
    let name = directory.file_name()?.to_string_lossy();
    let label = name.trim_start_matches('-');
    (!label.is_empty()).then(|| label.to_string())
}

#[derive(Deserialize)]
struct CreatedSession {
    initial_windows: Vec<CreatedWindow>,
}

#[derive(Deserialize)]
struct CreatedWindow {
    block_ids: Vec<String>,
}

fn first_block(reply: &str) -> Result<String> {
    let created: CreatedSession =
        serde_json::from_str(reply).context("Rex's new session reply is not readable")?;
    created
        .initial_windows
        .into_iter()
        .flat_map(|window| window.block_ids)
        .next()
        .context("Rex's new session has no terminal")
}

fn send(rex: &Path, block: &str, text: &str) -> Result<()> {
    let mut command = Command::new(rex);
    command.args(["send", "-b", block, text]);
    run_capture_timeout(&mut command, "typing into a Rex terminal", COMMAND_TIMEOUT).map(drop)
}

#[derive(Deserialize)]
struct BlockProcesses {
    foreground: Option<ForegroundProcess>,
}

#[derive(Deserialize)]
struct ForegroundProcess {
    argv0: Option<String>,
}

/// Rex starts each terminal's shell through `login`, which marks it a login
/// shell with a leading dash, so only that shell itself has one: anything run
/// from its prompt, a nested shell included, does not.
fn at_prompt(reply: &str) -> Result<bool> {
    let processes: BlockProcesses =
        serde_json::from_str(reply).context("Rex's process reply is not readable")?;
    Ok(processes
        .foreground
        .and_then(|foreground| foreground.argv0)
        .is_some_and(|argv0| argv0.starts_with('-')))
}

fn block_at_prompt(rex: &Path, block: &str) -> Option<Result<bool>> {
    let mut call = Command::new(rex);
    call.args(["block", "call", "-b", block, "process"]);
    call.stderr(Stdio::null());
    run_capture_timeout(&mut call, "asking Rex about a terminal", COMMAND_TIMEOUT)
        .ok()
        .map(|reply| at_prompt(&reply))
}

impl TerminalAdapter for Rex {
    // A session's shell outlives the tool, as Ghostty's does: the session stays
    // open once the tool exits, and a tool killed in it can be typed back in.
    fn open_tab(&self, directory: &Path, command: &str, _selection: Selection) -> Result<()> {
        let rex = executable()?;
        let reply = run_capture_timeout(
            &mut new_session_command(&rex, directory),
            "opening a Rex session",
            COMMAND_TIMEOUT,
        )?;
        send(&rex, &first_block(&reply)?, &format!("{command}\n"))
    }

    fn is_running(&self) -> bool {
        executable().is_ok_and(|rex| {
            Command::new(rex)
                .args(["--autostart=false", "server", "status"])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|status| status.success())
        })
    }

    fn launch(&self) -> Result<()> {
        let mut start = Command::new(executable()?);
        start.args(["server", "start"]);
        run_checked(start, "starting the Rex server")
    }

    // A shell Rex just brought back may still be starting, and what it runs from
    // its startup files looks like work at the prompt, so a busy one gets a moment.
    fn run_in_named_tab(&self, tab: &str, command: &str) -> Result<bool> {
        let rex = executable()?;
        let deadline = Instant::now() + PROMPT_WAIT;
        loop {
            let Some(at_prompt) = block_at_prompt(&rex, tab) else {
                return Ok(false);
            };
            if at_prompt? {
                send(&rex, tab, &format!("{ERASE_LINE}{command}\n"))?;
                return Ok(true);
            }
            if Instant::now() >= deadline {
                return Ok(false);
            }
            thread::sleep(PROMPT_POLL);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn arguments(command: &Command) -> Vec<String> {
        command
            .get_args()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn a_session_is_labeled_after_its_folder() {
        assert_eq!(
            arguments(&new_session_command(
                Path::new("rex"),
                Path::new("/Users/me/My Project")
            )),
            ["new", "My Project", "-c", "/Users/me/My Project", "--json"]
        );
    }

    #[test]
    fn rex_names_a_session_whose_folder_gives_no_usable_label() {
        assert_eq!(session_label(Path::new("/tmp/--flag")).unwrap(), "flag");
        assert_eq!(session_label(Path::new("/tmp/--")), None);
        assert_eq!(session_label(Path::new("/")), None);
        assert_eq!(
            arguments(&new_session_command(Path::new("rex"), Path::new("/"))),
            ["new", "-c", "/", "--json"]
        );
    }

    #[test]
    fn the_app_is_searched_for_its_helper_whatever_either_is_called() {
        let app = tempfile::tempdir().unwrap();
        assert_eq!(bundled_executable(app.path()), None);
        let macos = app.path().join("Contents/Helpers/Any.rex/Contents/MacOS");
        std::fs::create_dir_all(&macos).unwrap();
        assert_eq!(bundled_executable(app.path()), None);
        std::fs::write(macos.join("rex"), "").unwrap();
        assert_eq!(bundled_executable(app.path()), Some(macos.join("rex")));
        assert_eq!(bundled_executable(Path::new("")), None);
    }

    #[test]
    fn a_new_sessions_first_terminal_is_read_from_its_reply() {
        let reply = r#"{"session_id":"session:1","initial_windows":[
            {"window_id":"window:1","block_ids":["block:1","block:2"],"revision":1}]}"#;
        assert_eq!(first_block(reply).unwrap(), "block:1");
        assert!(first_block(r#"{"initial_windows":[]}"#).is_err());
        assert!(first_block("not json").is_err());
    }

    #[test]
    fn only_the_terminals_own_login_shell_counts_as_at_its_prompt() {
        let foreground = |process: &str| format!(r#"{{"foreground":{process},"home":"/h"}}"#);
        let process = |argv0: &str| foreground(&format!(r#"{{"pid":1,"argv0":"{argv0}"}}"#));
        assert!(at_prompt(&process("-zsh")).unwrap());
        assert!(!at_prompt(&process("login")).unwrap());
        assert!(!at_prompt(&process("zsh")).unwrap());
        assert!(!at_prompt(&process("claude")).unwrap());
        assert!(!at_prompt(&foreground(r#"{"pid":1}"#)).unwrap());
        assert!(!at_prompt(&foreground("null")).unwrap());
        assert!(at_prompt("not json").is_err());
    }
}
