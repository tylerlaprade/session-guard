use super::{TerminalAdapter, run_checked};
use crate::process;
use anyhow::Result;
use std::path::Path;
use std::process::Command;

pub struct Wezterm;

impl TerminalAdapter for Wezterm {
    fn open_tab(&self, directory: &Path, command: &str) -> Result<()> {
        let mut process = Command::new("wezterm");
        process.args(["cli", "spawn", "--cwd"]);
        process.arg(directory);
        process.args(["--", "sh", "-lc", command]);
        run_checked(process, "opening WezTerm tab")
    }

    fn is_running(&self) -> bool {
        process::cli_process_is_running("wezterm-gui") || process::cli_process_is_running("wezterm")
    }

    fn launch(&self) -> Result<()> {
        Command::new("wezterm").spawn()?;
        Ok(())
    }
}
