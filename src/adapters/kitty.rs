use super::{TerminalAdapter, run_checked};
use crate::process;
use anyhow::Result;
use std::path::Path;
use std::process::Command;

pub struct Kitty;

impl TerminalAdapter for Kitty {
    fn open_tab(&self, directory: &Path, command: &str) -> Result<()> {
        let mut process = Command::new("kitty");
        process.args(["@", "launch", "--type=tab", "--cwd"]);
        process.arg(directory);
        process.args(["sh", "-lc", command]);
        run_checked(process, "opening kitty tab")
    }

    fn is_running(&self) -> bool {
        process::cli_process_is_running("kitty")
    }

    fn launch(&self) -> Result<()> {
        Command::new("kitty").spawn()?;
        Ok(())
    }
}
