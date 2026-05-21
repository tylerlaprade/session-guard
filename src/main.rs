mod adapters;
mod commands;
mod hooks;
mod paths;
mod process;
mod sessions;
mod transcripts;

use anyhow::Result;
use clap::{Parser, Subcommand, ValueEnum};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Tool {
    Claude,
    Codex,
}

impl Tool {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
        }
    }
}

impl std::fmt::Display for Tool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum TerminalKind {
    Ghostty,
    Iterm2,
    Terminal,
    Kitty,
    Wezterm,
    Alacritty,
}

impl TerminalKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ghostty => "ghostty",
            Self::Iterm2 => "iterm2",
            Self::Terminal => "terminal",
            Self::Kitty => "kitty",
            Self::Wezterm => "wezterm",
            Self::Alacritty => "alacritty",
        }
    }
}

impl std::str::FromStr for TerminalKind {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "ghostty" => Ok(Self::Ghostty),
            "iterm2" | "iterm" => Ok(Self::Iterm2),
            "terminal" | "terminal.app" => Ok(Self::Terminal),
            "kitty" => Ok(Self::Kitty),
            "wezterm" => Ok(Self::Wezterm),
            "alacritty" => Ok(Self::Alacritty),
            other => anyhow::bail!("unsupported terminal '{other}'"),
        }
    }
}

impl std::fmt::Display for TerminalKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Parser)]
#[command(name = "session-guard")]
#[command(about = "Restore Claude Code and Codex CLI sessions after a macOS crash or reboot")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    Daemon,
    Install {
        #[arg(long, value_enum)]
        terminal: TerminalKind,
    },
    #[command(hide = true)]
    InstallHooks,
    Uninstall {
        #[arg(long)]
        purge: bool,
    },
    Status,
    #[command(hide = true)]
    Register {
        #[arg(long, value_enum)]
        tool: Tool,
        #[arg(long)]
        session_id: Option<String>,
        #[arg(long)]
        pid: Option<i32>,
        #[arg(long)]
        shell_pid: Option<i32>,
        #[arg(long)]
        directory: Option<PathBuf>,
        #[arg(long)]
        name: Option<String>,
    },
    #[command(hide = true)]
    Deregister {
        #[arg(long)]
        session_id: Option<String>,
    },
    Restore,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Command::Daemon => commands::daemon::run(),
        Command::Install { terminal } => commands::install::run(terminal),
        Command::InstallHooks => commands::install_hooks::run(),
        Command::Uninstall { purge } => commands::uninstall::run(purge),
        Command::Status => commands::status::run(),
        Command::Register {
            tool,
            session_id,
            pid,
            shell_pid,
            directory,
            name,
        } => commands::register::run(tool, session_id, pid, shell_pid, directory, name),
        Command::Deregister { session_id } => commands::deregister::run(session_id),
        Command::Restore => commands::restore::run(),
    }
}
