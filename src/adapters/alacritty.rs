use super::{TerminalAdapter, run_checked};
use crate::process;
use anyhow::Result;
use std::path::Path;
use std::process::Command;

pub struct Alacritty;

impl TerminalAdapter for Alacritty {
    fn open_tab(&self, directory: &Path, command: &str) -> Result<()> {
        let mut process = Command::new("alacritty");
        process.arg("--working-directory");
        process.arg(directory);
        process.args(["-e", "sh", "-lc", command]);
        run_checked(process, "opening Alacritty window")
    }

    fn is_running(&self) -> bool {
        process::cli_process_is_running("alacritty")
    }

    fn launch(&self) -> Result<()> {
        Command::new("alacritty").spawn()?;
        Ok(())
    }
}
