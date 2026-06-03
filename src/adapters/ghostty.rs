use super::{TerminalAdapter, applescript_quote, run_checked};
use crate::process;
use anyhow::{Context, Result};
use std::path::Path;
use std::process::Command;
use std::thread;
use std::time::Duration;

pub struct Ghostty;

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
    new window with configuration cfg
  else
    new tab in front window with configuration cfg
  end if
end tell"#,
            dir = applescript_quote(dir_str),
            input = applescript_quote(&initial_input),
        );

        let mut osa = Command::new("osascript");
        osa.args(["-e", &script]);
        run_checked(osa, "opening Ghostty tab")
    }

    fn is_running(&self) -> bool {
        process::app_is_running("Ghostty")
    }

    fn launch(&self) -> Result<()> {
        let mut command = Command::new("open");
        command.args(["-a", "Ghostty"]);
        run_checked(command, "launching Ghostty")
    }
}
