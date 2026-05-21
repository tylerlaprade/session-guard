use super::{TerminalAdapter, applescript_quote, run_checked, shell_line};
use crate::process;
use anyhow::Result;
use std::path::Path;
use std::process::Command;
use std::thread;
use std::time::Duration;

pub struct Iterm2;

impl TerminalAdapter for Iterm2 {
    fn open_tab(&self, directory: &Path, command: &str) -> Result<()> {
        if !self.is_running() {
            self.launch()?;
            thread::sleep(Duration::from_millis(700));
        }

        let line = shell_line(directory, command);
        let script = format!(
            r#"tell application "iTerm2"
  activate
  if (count of windows) = 0 then
    create window with default profile
    tell current session to write text {}
  else
    tell current window
      create tab with default profile
      tell current session to write text {}
    end tell
  end tell
end tell"#,
            applescript_quote(&line),
            applescript_quote(&line),
        );

        let mut command = Command::new("osascript");
        command.args(["-e", &script]);
        run_checked(command, "opening iTerm2 tab")
    }

    fn is_running(&self) -> bool {
        process::app_is_running("iTerm2")
    }

    fn launch(&self) -> Result<()> {
        let mut command = Command::new("open");
        command.args(["-a", "iTerm"]);
        run_checked(command, "launching iTerm2")
    }
}
