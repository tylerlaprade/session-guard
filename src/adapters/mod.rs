mod alacritty;
pub(crate) mod ghostty;
mod iterm2;
mod kitty;
mod terminal_app;
mod wezterm;

use crate::TerminalKind;
use anyhow::{Context, Result};
use std::path::Path;

/// Whether a tab the daemon opens takes the selection in its window. A
/// restored tab does, so it can be watched opening; a spawned session's tab
/// opened after a lockout keeps the tab in front, as it would have opened at
/// once. Only Ghostty can keep it; in the other terminals the new tab or
/// window always takes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Selection {
    Take,
    Keep,
}

pub trait TerminalAdapter {
    fn open_tab(&self, directory: &Path, command: &str, selection: Selection) -> Result<()>;
    fn is_running(&self) -> bool;
    fn launch(&self) -> Result<()>;
    /// Whether every tab asked for so far has been added or never will be:
    /// a script that gave up waiting can still have its tab added later.
    fn caught_up(&self) -> bool {
        true
    }
    /// Every terminal's tty with its (window, tab) position, for a terminal
    /// whose scripting exposes its tab order.
    fn tab_positions(&self) -> Result<Vec<((u32, u32), String)>> {
        Ok(Vec::new())
    }
    /// Runs `command` at the prompt of the tab on `tty`, replacing anything
    /// typed there. False when no tab has that tty, or the terminal's
    /// scripting cannot type into a tab.
    fn run_in_tab(&self, _tty: &str, _command: &str) -> Result<bool> {
        Ok(false)
    }
}

pub fn adapter_for(kind: TerminalKind) -> Box<dyn TerminalAdapter> {
    match kind {
        TerminalKind::Ghostty => Box::new(ghostty::Ghostty::default()),
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

/// Launches a terminal by running its binary, and returns once the terminal's
/// process is running, so a restore pass right after it does not launch a
/// second one. Fails if the binary exits first.
pub fn launch_until_running(program: &str, is_running: impl Fn() -> bool) -> Result<()> {
    let mut child = std::process::Command::new(program)
        .spawn()
        .with_context(|| format!("launching {program}"))?;
    while !is_running() {
        if let Some(status) = child.try_wait()? {
            anyhow::bail!("{program} exited ({status}) before it was running");
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    Ok(())
}

/// Run a process with a hard timeout so a stuck AppleScript/osascript cannot
/// wedge the restore path (and the sessions-file lock) indefinitely.
pub fn run_checked_timeout(
    command: &mut std::process::Command,
    description: &str,
    timeout: std::time::Duration,
) -> Result<()> {
    run_capture_timeout(command, description, timeout).map(|_| ())
}

/// Like `run_checked_timeout`, but returns trimmed stdout. Only for commands
/// with small output (an `AppleScript` result): stdout is drained after exit,
/// so output larger than the pipe buffer would deadlock the wait loop.
pub fn run_capture_timeout(
    command: &mut std::process::Command,
    description: &str,
    timeout: std::time::Duration,
) -> Result<String> {
    use std::io::Read;
    use std::thread;
    use std::time::{Duration, Instant};

    command.stdout(std::process::Stdio::piped());
    let mut child = command
        .spawn()
        .with_context(|| format!("failed to spawn for {description}"))?;
    let start = Instant::now();
    loop {
        if let Some(status) = child
            .try_wait()
            .with_context(|| format!("failed to wait for {description}"))?
        {
            let mut output = String::new();
            if let Some(mut stdout) = child.stdout.take() {
                let _ = stdout.read_to_string(&mut output);
            }
            if !status.success() {
                anyhow::bail!("{description} failed with status {status}");
            }
            return Ok(output.trim().to_string());
        }
        if start.elapsed() > timeout {
            let _ = child.kill();
            let _ = child.wait();
            anyhow::bail!("{description} timed out after {}s", timeout.as_secs());
        }
        thread::sleep(Duration::from_millis(50));
    }
}
