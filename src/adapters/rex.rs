use super::{NamedTab, Selection, TerminalAdapter, run_capture_timeout};
use crate::{TerminalKind, process};
use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, PoisonError};
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

static FOUND_IN_APP: Mutex<Option<PathBuf>> = Mutex::new(None);

/// The `rex` command: on PATH inside Rex's own terminals, and otherwise only
/// inside the app, which `LaunchServices` finds under whatever name it has.
pub fn executable() -> Result<PathBuf> {
    if process::command_exists("rex") {
        return Ok(PathBuf::from("rex"));
    }
    remembered(&FOUND_IN_APP, find_in_app)
}

/// Asking `LaunchServices` takes an `AppleScript` call, which a swapping Mac can
/// take seconds to answer, so a process asks once and asks again only after
/// the file it was told about is gone. A failed answer is never kept.
fn remembered(
    found: &Mutex<Option<PathBuf>>,
    find: impl FnOnce() -> Result<PathBuf>,
) -> Result<PathBuf> {
    let mut found = found.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(rex) = found.as_ref().filter(|rex| rex.is_file()) {
        return Ok(rex.clone());
    }
    let rex = find()?;
    *found = Some(rex.clone());
    Ok(rex)
}

fn find_in_app() -> Result<PathBuf> {
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

fn rex_command(rex: &Path) -> Command {
    let mut command = Command::new(rex);
    // Inside a Rex terminal, a block in any other session counts as not found.
    command.env_remove("REX_SESSION");
    command
}

fn new_session_command(rex: &Path, directory: &Path) -> Command {
    let mut new = rex_command(rex);
    new.arg("new");
    new.args(folder_label(directory));
    new.arg("-c").arg(directory).arg("--json");
    new
}

// The tab beside it is busy, so someone may be using it: the new tab leaves
// the selection where it is.
fn new_tab_command(rex: &Path, session: &str, directory: &Path) -> Command {
    let mut new = rex_command(rex);
    new.args(["window", "new", "-s", session]);
    new.args(folder_label(directory));
    new.arg("-c")
        .arg(directory)
        .args(["--focus=false", "--json"]);
    new
}

// Rex takes a leading dash for a flag, and names a session or tab itself given
// no label.
fn folder_label(directory: &Path) -> Option<String> {
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

fn new_session_block(reply: &str) -> Result<String> {
    let created: CreatedSession =
        serde_json::from_str(reply).context("Rex's new session reply is not readable")?;
    created
        .initial_windows
        .into_iter()
        .flat_map(|window| window.block_ids)
        .next()
        .context("Rex's new session has no terminal")
}

fn new_tab_block(reply: &str) -> Result<String> {
    let created: CreatedWindow =
        serde_json::from_str(reply).context("Rex's new tab reply is not readable")?;
    created
        .block_ids
        .into_iter()
        .next()
        .context("Rex's new tab has no terminal")
}

/// Opens a tab running `command` beside the block `beside` names, in the Rex
/// session holding that block, or else in a Rex session of its own.
fn open_in(rex: &Path, directory: &Path, command: &str, beside: Option<&str>) -> Result<()> {
    let block = match beside
        .map(|block| session_holding(rex, block))
        .transpose()?
        .flatten()
    {
        Some(session) => new_tab_block(&run_capture_timeout(
            &mut new_tab_command(rex, &session, directory),
            "opening a Rex tab",
            COMMAND_TIMEOUT,
        )?)?,
        None => new_session_block(&run_capture_timeout(
            &mut new_session_command(rex, directory),
            "opening a Rex session",
            COMMAND_TIMEOUT,
        )?)?,
    };
    send(rex, &block, &format!("{command}\n"))
}

fn send(rex: &Path, block: &str, text: &str) -> Result<()> {
    let mut command = rex_command(rex);
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

#[derive(Deserialize)]
struct SessionList {
    sessions: Vec<ListedSession>,
}

#[derive(Deserialize)]
struct ListedSession {
    session_id: String,
}

#[derive(Deserialize)]
struct BlockList {
    blocks: Vec<ListedBlock>,
}

#[derive(Deserialize)]
struct ListedBlock {
    block_id: String,
}

/// The Rex session holding `block`, read from the server's listings so that
/// only an answer, never a failure to get one, says a block is gone. A server
/// this starts brings back its saved sessions before it answers.
fn session_holding(rex: &Path, block: &str) -> Result<Option<String>> {
    let mut list = rex_command(rex);
    list.args(["ls", "--json"]);
    let sessions: SessionList = serde_json::from_str(&run_capture_timeout(
        &mut list,
        "listing Rex sessions",
        COMMAND_TIMEOUT,
    )?)
    .context("Rex's session list is not readable")?;
    for session in sessions.sessions {
        let mut blocks = rex_command(rex);
        blocks.args(["block", "ls", "-s", &session.session_id, "--json"]);
        let listed: BlockList = serde_json::from_str(&run_capture_timeout(
            &mut blocks,
            "listing a Rex session's terminals",
            COMMAND_TIMEOUT,
        )?)
        .context("Rex's terminal list is not readable")?;
        if listed.blocks.iter().any(|listed| listed.block_id == block) {
            return Ok(Some(session.session_id));
        }
    }
    Ok(None)
}

fn block_at_prompt(rex: &Path, block: &str) -> Option<Result<bool>> {
    let mut call = rex_command(rex);
    call.args(["block", "call", "-b", block, "process"]);
    call.stderr(Stdio::null());
    run_capture_timeout(&mut call, "asking Rex about a terminal", COMMAND_TIMEOUT)
        .ok()
        .map(|reply| at_prompt(&reply))
}

impl TerminalAdapter for Rex {
    // A session's shell outlives the tool, as Ghostty's does: the session stays
    // open once the tool exits, and a tool killed in it can be typed back in.
    fn open_tab(
        &self,
        directory: &Path,
        command: &str,
        _selection: Selection,
        beside: Option<&str>,
    ) -> Result<()> {
        open_in(&executable()?, directory, command, beside)
    }

    fn is_running(&self) -> bool {
        executable().is_ok_and(|rex| {
            rex_command(&rex)
                .args(["--autostart=false", "server", "status"])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|status| status.success())
        })
    }

    // Any `rex` command starts a missing server the way the app's does, which
    // brings back its saved sessions and still exits once they are all closed.
    fn launch(&self) -> Result<()> {
        let mut list = rex_command(&executable()?);
        list.args(["ls", "--json"]);
        run_capture_timeout(&mut list, "starting the Rex server", COMMAND_TIMEOUT).map(drop)
    }

    // A shell Rex just brought back may still be starting, and what it runs from
    // its startup files looks like work at the prompt, so a busy one gets a moment.
    fn run_in_named_tab(&self, tab: &str, command: &str) -> Result<NamedTab> {
        let rex = executable()?;
        if session_holding(&rex, tab)?.is_none() {
            return Ok(NamedTab::Closed);
        }
        let deadline = Instant::now() + PROMPT_WAIT;
        loop {
            let Some(at_prompt) = block_at_prompt(&rex, tab) else {
                return Ok(NamedTab::Unavailable);
            };
            if at_prompt? {
                send(&rex, tab, &format!("{ERASE_LINE}{command}\n"))?;
                return Ok(NamedTab::Ran);
            }
            if Instant::now() >= deadline {
                return Ok(NamedTab::Unavailable);
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
    fn a_rex_terminal_can_reach_blocks_outside_its_own_session() {
        let command = rex_command(Path::new("rex"));
        assert!(
            command
                .get_envs()
                .any(|(name, value)| name == "REX_SESSION" && value.is_none())
        );
    }

    #[test]
    fn listings_are_read_from_rexs_replies() {
        let sessions: SessionList =
            serde_json::from_str(r#"{"sessions":[{"session_id":"session:1","label":"work"}]}"#)
                .unwrap();
        assert_eq!(sessions.sessions[0].session_id, "session:1");
        let blocks: BlockList = serde_json::from_str(
            r#"{"blocks":[{"block_id":"block:1","label":"shell","window_id":"window:1"}]}"#,
        )
        .unwrap();
        assert_eq!(blocks.blocks[0].block_id, "block:1");
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
    fn a_tab_beside_another_is_labeled_after_its_folder_and_leaves_the_selection() {
        assert_eq!(
            arguments(&new_tab_command(
                Path::new("rex"),
                "session:1",
                Path::new("/Users/me/My Project")
            )),
            [
                "window",
                "new",
                "-s",
                "session:1",
                "My Project",
                "-c",
                "/Users/me/My Project",
                "--focus=false",
                "--json"
            ]
        );
    }

    #[test]
    fn rex_names_a_session_or_tab_whose_folder_gives_no_usable_label() {
        assert_eq!(folder_label(Path::new("/tmp/--flag")).unwrap(), "flag");
        assert_eq!(folder_label(Path::new("/tmp/--")), None);
        assert_eq!(folder_label(Path::new("/")), None);
        assert_eq!(
            arguments(&new_session_command(Path::new("rex"), Path::new("/"))),
            ["new", "-c", "/", "--json"]
        );
        assert_eq!(
            arguments(&new_tab_command(
                Path::new("rex"),
                "session:1",
                Path::new("/")
            )),
            [
                "window",
                "new",
                "-s",
                "session:1",
                "-c",
                "/",
                "--focus=false",
                "--json"
            ]
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
    fn the_app_is_asked_for_rex_once_until_the_file_it_named_is_gone() {
        let folder = tempfile::tempdir().unwrap();
        let rex = folder.path().join("rex");
        std::fs::write(&rex, "").unwrap();
        let asked = std::cell::Cell::new(0);
        let find = || {
            asked.set(asked.get() + 1);
            Ok(rex.clone())
        };
        let found = Mutex::new(None);
        assert_eq!(remembered(&found, find).unwrap(), rex);
        assert_eq!(remembered(&found, find).unwrap(), rex);
        assert_eq!(asked.get(), 1);

        std::fs::remove_file(&rex).unwrap();
        assert_eq!(remembered(&found, find).unwrap(), rex);
        assert_eq!(asked.get(), 2);
    }

    #[test]
    fn a_failed_search_for_rex_is_asked_again_next_time() {
        let folder = tempfile::tempdir().unwrap();
        let rex = folder.path().join("rex");
        std::fs::write(&rex, "").unwrap();
        let found = Mutex::new(None);
        assert!(remembered(&found, || anyhow::bail!("finding the Rex app timed out")).is_err());
        assert_eq!(remembered(&found, || Ok(rex.clone())).unwrap(), rex);
    }

    #[test]
    fn a_new_sessions_first_terminal_is_read_from_its_reply() {
        let reply = r#"{"session_id":"session:1","initial_windows":[
            {"window_id":"window:1","block_ids":["block:1","block:2"],"revision":1}]}"#;
        assert_eq!(new_session_block(reply).unwrap(), "block:1");
        assert!(new_session_block(r#"{"initial_windows":[]}"#).is_err());
        assert!(new_session_block("not json").is_err());
    }

    #[test]
    fn a_new_tabs_first_terminal_is_read_from_its_reply() {
        let reply = r#"{"window_id":"window:1","block_ids":["block:1","block:2"],"revision":3}"#;
        assert_eq!(new_tab_block(reply).unwrap(), "block:1");
        assert!(new_tab_block(r#"{"window_id":"window:1","block_ids":[],"revision":3}"#).is_err());
        assert!(new_tab_block(r#"{"window_id":null,"block_ids":null,"revision":3}"#).is_err());
        assert!(new_tab_block("not json").is_err());
    }

    // Session N holds block N.
    const REX: &str = r#"#!/bin/sh
printf '%s\n' "$*" >> "$0.log"
case $1 in
  ls) echo '{"sessions":[{"session_id":"session:1"},{"session_id":"session:2"}]}' ;;
  block) echo "{\"blocks\":[{\"block_id\":\"block:${4#session:}\"}]}" ;;
  window) echo '{"window_id":"window:3","block_ids":["block:3"],"revision":4}' ;;
  new) echo '{"session_id":"session:3","initial_windows":[{"window_id":"window:4","block_ids":["block:4"]}]}' ;;
esac
"#;

    fn rex_calls(open: impl FnOnce(&Path)) -> Vec<String> {
        use std::os::unix::fs::PermissionsExt;
        let folder = tempfile::tempdir().unwrap();
        let rex = folder.path().join("rex");
        std::fs::write(&rex, REX).unwrap();
        std::fs::set_permissions(&rex, std::fs::Permissions::from_mode(0o755)).unwrap();
        open(&rex);
        std::fs::read_to_string(folder.path().join("rex.log"))
            .unwrap()
            .lines()
            .filter(|line| !line.is_empty())
            .map(str::to_string)
            .collect()
    }

    #[test]
    fn a_tab_reopens_beside_its_old_one_in_the_rex_session_holding_it() {
        let calls = rex_calls(|rex| {
            open_in(rex, Path::new("/me/app"), "resume", Some("block:2")).unwrap();
        });
        assert_eq!(
            calls,
            [
                "ls --json",
                "block ls -s session:1 --json",
                "block ls -s session:2 --json",
                "window new -s session:2 app -c /me/app --focus=false --json",
                "send -b block:3 resume",
            ]
        );
    }

    #[test]
    fn a_tab_gets_a_rex_session_of_its_own_when_none_holds_its_old_one() {
        let calls = rex_calls(|rex| {
            open_in(rex, Path::new("/me/app"), "resume", Some("block:9")).unwrap();
        });
        assert_eq!(
            calls,
            [
                "ls --json",
                "block ls -s session:1 --json",
                "block ls -s session:2 --json",
                "new app -c /me/app --json",
                "send -b block:4 resume",
            ]
        );
        let calls = rex_calls(|rex| open_in(rex, Path::new("/me/app"), "resume", None).unwrap());
        assert_eq!(
            calls,
            ["new app -c /me/app --json", "send -b block:4 resume"]
        );
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
