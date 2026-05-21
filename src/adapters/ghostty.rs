use super::{TerminalAdapter, applescript_quote, run_checked, shell_line};
use crate::process;
use anyhow::{Context, Result};
use std::path::Path;
use std::process::Command;
use std::thread;
use std::time::Duration;

pub struct Ghostty;

impl TerminalAdapter for Ghostty {
    fn open_tab(&self, directory: &Path, command: &str) -> Result<()> {
        let was_running = self.is_running();
        if !was_running {
            self.launch()?;
            thread::sleep(Duration::from_millis(700));
        }

        let line = shell_line(directory, command);
        let new_tab = if was_running {
            "keystroke \"t\" using command down\ndelay 0.2"
        } else {
            ""
        };
        let script = format!(
            r#"tell application "Ghostty" to activate
delay 0.2
tell application "System Events"
  tell process "Ghostty"
    {new_tab}
    keystroke {}
    key code 36
  end tell
end tell"#,
            applescript_quote(&line),
        );

        let mut command = Command::new("osascript");
        command.args(["-e", &script]);
        run_checked(command, "opening Ghostty tab").with_context(|| {
            "Ghostty tab restore uses System Events; grant Accessibility permission if macOS blocks it"
        })
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
