use super::{TerminalAdapter, run_checked, shell_line};
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
        let mut command = Command::new("osascript");
        command.args(["-e", include_str!("iterm2_open_tab.applescript"), &line]);
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
