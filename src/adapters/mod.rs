mod alacritty;
mod ghostty;
mod iterm2;
mod kitty;
mod terminal_app;
mod wezterm;

use crate::TerminalKind;
use anyhow::Result;
use std::path::Path;

pub trait TerminalAdapter {
    fn open_tab(&self, directory: &Path, command: &str) -> Result<()>;
    fn is_running(&self) -> bool;
    fn launch(&self) -> Result<()>;
}

pub fn adapter_for(kind: TerminalKind) -> Box<dyn TerminalAdapter> {
    match kind {
        TerminalKind::Ghostty => Box::new(ghostty::Ghostty),
        TerminalKind::Iterm2 => Box::new(iterm2::Iterm2),
        TerminalKind::Terminal => Box::new(terminal_app::TerminalApp),
        TerminalKind::Kitty => Box::new(kitty::Kitty),
        TerminalKind::Wezterm => Box::new(wezterm::Wezterm),
        TerminalKind::Alacritty => Box::new(alacritty::Alacritty),
    }
}

pub fn shell_quote(value: &str) -> String {
    if value.is_empty() {
        return "''".to_string();
    }

    format!("'{}'", value.replace('\'', "'\\''"))
}

pub fn applescript_quote(value: &str) -> String {
    format!("{:?}", value)
}

pub fn shell_line(directory: &Path, command: &str) -> String {
    format!(
        "cd {} && exec {}",
        shell_quote(&directory.display().to_string()),
        command
    )
}

pub fn run_checked(mut command: std::process::Command, description: &str) -> Result<()> {
    let status = command.status()?;
    if !status.success() {
        anyhow::bail!("{description} failed with status {status}");
    }
    Ok(())
}
