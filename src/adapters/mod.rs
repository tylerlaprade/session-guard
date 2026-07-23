mod alacritty;
mod ghostty;
mod iterm2;
mod kitty;
mod terminal_app;
mod wezterm;

use crate::TerminalKind;
use anyhow::{Context, Result};
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
    run_checked_timeout(
        &mut command,
        description,
        std::time::Duration::from_secs(30),
    )
}

/// Run a process with a hard timeout so a stuck AppleScript/osascript cannot
/// wedge the restore path (and the sessions-file lock) indefinitely.
pub fn run_checked_timeout(
    command: &mut std::process::Command,
    description: &str,
    timeout: std::time::Duration,
) -> Result<()> {
    use std::thread;
    use std::time::{Duration, Instant};

    let mut child = command
        .spawn()
        .with_context(|| format!("failed to spawn for {description}"))?;
    let start = Instant::now();
    loop {
        match child
            .try_wait()
            .with_context(|| format!("failed to wait for {description}"))?
        {
            Some(status) => {
                if !status.success() {
                    anyhow::bail!("{description} failed with status {status}");
                }
                return Ok(());
            }
            None => {
                if start.elapsed() > timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    anyhow::bail!("{description} timed out after {}s", timeout.as_secs());
                }
                thread::sleep(Duration::from_millis(50));
            }
        }
    }
}
