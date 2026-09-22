use super::{TerminalAdapter, applescript_quote, run_checked};
use crate::process;
use anyhow::{Context, Result};
use std::path::Path;
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

// A wedged Ghostty can take the whole window this long; the surface poll
// below also waits this long for a slow shell under memory pressure.
const SURFACE_START_TIMEOUT: Duration = Duration::from_secs(15);
const SURFACE_POLL_INTERVAL: Duration = Duration::from_millis(500);

pub struct Ghostty;

pub(crate) fn is_only_terminal(tty: &str) -> Result<bool> {
    let mut command = Command::new("osascript");
    command.args([
        "-l",
        "JavaScript",
        "-e",
        include_str!("ghostty_first_terminal.js"),
        tty,
    ]);
    super::run_capture_timeout(
        &mut command,
        "checking first Ghostty terminal",
        Duration::from_secs(2),
    )
    .map(|result| result == "true")
}

impl TerminalAdapter for Ghostty {
    fn open_tab(&self, directory: &Path, command: &str) -> Result<()> {
        if !self.is_running() {
            self.launch()?;
            thread::sleep(Duration::from_millis(700));
        }

        let dir_str = directory
            .to_str()
            .context("Ghostty restore requires a UTF-8 directory path")?;
        let initial_input = format!("{command}\n");
        // Add a tab to the existing window, but start a fresh window when there
        // is none. At login-time restore Ghostty is often running with no window
        // yet, and `new tab in front window` fails with -1728 ("Can't get front
        // window") in that state.
        let script = format!(
            r#"tell application "Ghostty"
  set cfg to new surface configuration
  set initial working directory of cfg to {dir}
  set initial input of cfg to {input}
  if (count of windows) is 0 then
    set newWindow to new window with configuration cfg
    return id of selected tab of newWindow
  else
    return id of (new tab in front window with configuration cfg)
  end if
end tell"#,
            dir = applescript_quote(dir_str),
            input = applescript_quote(&initial_input),
        );

        let mut osa = Command::new("osascript");
        osa.args(["-e", &script]);
        // Ghostty scripting can hang when the app is busy/recovering; do not
        // block the daemon (or its sessions lock) forever.
        let tab_id =
            super::run_capture_timeout(&mut osa, "opening Ghostty tab", SURFACE_START_TIMEOUT)?;
        anyhow::ensure!(!tab_id.is_empty(), "Ghostty returned no tab id");
        wait_for_surface_start(&tab_id)
    }

    fn is_running(&self) -> bool {
        process::app_is_running("Ghostty")
    }

    fn launch(&self) -> Result<()> {
        let mut command = Command::new("open");
        command.args(["-a", "Ghostty", "--args", "--initial-window=false"]);
        run_checked(command, "launching Ghostty")
    }
}

// Ghostty can accept the scripting request and return a tab id, yet fail to
// start the surface process — observed under memory pressure as
// "error initializing surface err=error.OutOfMemory", leaving a permanent
// ghost tab. Counting that as restored burns the restore cooldown on a tab
// that does not exist. The surface's working directory stays empty until its
// shell starts and reports pwd (Ghostty's default shell integration), so
// poll it and fail the restore when it never populates; the session then
// stays recoverable and a later restore retries.
fn wait_for_surface_start(tab_id: &str) -> Result<()> {
    let script = format!(
        r#"tell application "Ghostty"
  repeat with w in windows
    repeat with t in tabs of w
      if id of t is {id} then return working directory of focused terminal of t
    end repeat
  end repeat
  return ""
end tell"#,
        id = applescript_quote(tab_id),
    );

    let deadline = Instant::now() + SURFACE_START_TIMEOUT;
    loop {
        let mut osa = Command::new("osascript");
        osa.args(["-e", &script]);
        if let Ok(working_directory) =
            super::run_capture_timeout(&mut osa, "checking Ghostty surface", SURFACE_START_TIMEOUT)
            && !working_directory.is_empty()
        {
            return Ok(());
        }

        if Instant::now() >= deadline {
            anyhow::bail!("Ghostty tab {tab_id} never started its terminal (ghost surface)");
        }
        thread::sleep(SURFACE_POLL_INTERVAL);
    }
}
