use super::{TerminalAdapter, applescript_quote, run_checked};
use crate::process;
use anyhow::{Context, Result};
use std::cell::RefCell;
use std::path::Path;
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

// A wedged Ghostty can take the whole window this long; the surface poll
// below also waits this long for a slow shell under memory pressure.
const SURFACE_START_TIMEOUT: Duration = Duration::from_secs(15);
const SURFACE_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// One restore pass's tabs stay together. The first goes into the front
/// window, or a new one when none is open, and the rest follow it by the
/// window's id: a window that comes to the front mid-restore, such as one a
/// restored session opens for itself, must not collect the remaining tabs.
#[derive(Default)]
pub struct Ghostty {
    restore_window: RefCell<Option<String>>,
}

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

impl Ghostty {
    /// Keep the window a restore script reported for the pass's next tab,
    /// and return the tab it opened.
    fn remember_window(&self, reply: &str) -> Result<String> {
        let (window_id, tab_id) = reply
            .split_once('\t')
            .filter(|(window_id, tab_id)| !window_id.is_empty() && !tab_id.is_empty())
            .context("Ghostty returned no window and tab id")?;
        *self.restore_window.borrow_mut() = Some(window_id.to_string());
        Ok(tab_id.to_string())
    }
}

/// The script opening one restored tab: in `window` while it is open,
/// otherwise in the front window, or a fresh one when there is none. At
/// login-time restore Ghostty is often running with no window yet, and
/// `new tab in front window` fails with -1728 ("Can't get front window") in
/// that state. It answers with the window's id and the tab's, tab-separated.
fn restore_tab_script(directory: &str, input: &str, window: &str) -> String {
    format!(
        r#"tell application "Ghostty"
  set cfg to new surface configuration
  set initial working directory of cfg to {directory}
  set initial input of cfg to {input}
  set restoreWindow to missing value
  repeat with candidate in windows
    if id of candidate is {window} then set restoreWindow to contents of candidate
  end repeat
  if restoreWindow is missing value then
    if (count of windows) is 0 then
      set restoreWindow to new window with configuration cfg
      return (id of restoreWindow) & (character id 9) & (id of selected tab of restoreWindow)
    end if
    set restoreWindow to front window
  end if
  set newTab to new tab in restoreWindow with configuration cfg
  return (id of restoreWindow) & (character id 9) & (id of newTab)
end tell"#,
        directory = applescript_quote(directory),
        input = applescript_quote(input),
        window = applescript_quote(window),
    )
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
        let script = restore_tab_script(
            dir_str,
            &format!("{command}\n"),
            &self.restore_window.borrow().clone().unwrap_or_default(),
        );
        let mut osa = Command::new("osascript");
        osa.args(["-e", &script]);
        // Ghostty scripting can hang when the app is busy/recovering; do not
        // block the daemon (or its sessions lock) forever.
        let reply =
            super::run_capture_timeout(&mut osa, "opening Ghostty tab", SURFACE_START_TIMEOUT)?;
        let tab_id = self.remember_window(&reply)?;
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A pass's first tab goes wherever Ghostty puts it; the rest go into
    /// the window that first tab landed in, by that window's id.
    #[test]
    fn a_pass_keeps_its_later_tabs_in_its_first_tab_window() {
        let ghostty = Ghostty::default();
        let first = restore_tab_script("/tmp", "launch\n", "");
        assert!(first.contains(r#"if id of candidate is "" then"#));
        assert_eq!(
            ghostty.remember_window("tab-group-1\ttab-7").unwrap(),
            "tab-7"
        );
        let remembered = ghostty.restore_window.borrow().clone().unwrap();
        let second = restore_tab_script("/tmp", "launch\n", &remembered);
        assert!(second.contains(r#"if id of candidate is "tab-group-1" then"#));
        assert!(ghostty.remember_window("tab-7").is_err());
        assert!(ghostty.remember_window("\ttab-7").is_err());
    }
}
