use super::{TerminalAdapter, applescript_quote, run_checked, shell_line};
use crate::process;
use anyhow::Result;
use std::path::Path;
use std::process::Command;

pub struct TerminalApp;

impl TerminalAdapter for TerminalApp {
    fn open_tab(&self, directory: &Path, command: &str) -> Result<()> {
        let line = shell_line(directory, command);
        let script = format!(
            r#"tell application "Terminal"
  activate
  do script {}
end tell"#,
            applescript_quote(&line),
        );

        let mut command = Command::new("osascript");
        command.args(["-e", &script]);
        run_checked(command, "opening Terminal.app tab")
    }

    fn is_running(&self) -> bool {
        process::app_is_running("Terminal")
    }

    fn launch(&self) -> Result<()> {
        let mut command = Command::new("open");
        command.args(["-a", "Terminal"]);
        run_checked(command, "launching Terminal.app")
    }
}
