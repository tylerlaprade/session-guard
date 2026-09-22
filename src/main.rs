mod adapters;
mod cargo_targets;
mod commands;
mod harness;
mod hooks;
mod last_sessions;
mod paths;
mod process;
mod scan;
mod sessions;
mod transcripts;

use anyhow::Result;
use clap::{Parser, Subcommand, ValueEnum};
use serde::{Deserialize, Serialize};
use std::ffi::OsString;
use std::path::PathBuf;

/// A supported harness, identified by its registry entry. Every tool-specific
/// fact lives behind `spec()`; nothing outside `harness.rs` branches on which
/// harness this is.
#[derive(Clone, Copy)]
pub struct Tool(&'static harness::Harness);

impl Tool {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        self.0.id
    }

    #[must_use]
    pub fn spec(self) -> &'static harness::Harness {
        self.0
    }

    pub fn all() -> impl Iterator<Item = Tool> {
        harness::HARNESSES.iter().map(Tool)
    }

    pub fn from_id(id: &str) -> Option<Self> {
        harness::find(id).map(Tool)
    }
}

/// Ids are unique across the registry, so they decide identity.
impl PartialEq for Tool {
    fn eq(&self, other: &Self) -> bool {
        self.0.id == other.0.id
    }
}

impl Eq for Tool {}

impl std::hash::Hash for Tool {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.0.id.hash(state);
    }
}

impl PartialOrd for Tool {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Tool {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.0.id.cmp(other.0.id)
    }
}

impl std::fmt::Debug for Tool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0.id)
    }
}

impl std::fmt::Display for Tool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0.id)
    }
}

impl Serialize for Tool {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.0.id)
    }
}

impl<'de> Deserialize<'de> for Tool {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let id = String::deserialize(deserializer)?;
        Tool::from_id(&id).ok_or_else(|| serde::de::Error::custom(format!("unknown tool '{id}'")))
    }
}

impl ValueEnum for Tool {
    fn value_variants<'a>() -> &'a [Self] {
        static VARIANTS: std::sync::OnceLock<Vec<Tool>> = std::sync::OnceLock::new();
        VARIANTS.get_or_init(|| Tool::all().collect())
    }

    fn to_possible_value(&self) -> Option<clap::builder::PossibleValue> {
        Some(clap::builder::PossibleValue::new(self.0.id))
    }
}

/// Test-only lookup so cases can name a harness without a registry index.
#[cfg(test)]
#[must_use]
pub fn tool(id: &str) -> Tool {
    Tool::from_id(id).expect("known harness")
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
    #[must_use]
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

    /// Executable name as `ps -o comm=` reports it, lowercased.
    #[must_use]
    pub fn process_name(self) -> &'static str {
        match self {
            Self::Ghostty => "ghostty",
            Self::Iterm2 => "iterm2",
            Self::Terminal => "terminal",
            Self::Kitty => "kitty",
            Self::Wezterm => "wezterm-gui",
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
#[command(about = "Restore agent CLI sessions after a macOS crash or reboot")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    Daemon,
    /// Run a command with a temporary Cargo target owned by this agent session.
    CargoTarget {
        #[arg(last = true, required = true)]
        command: Vec<OsString>,
    },
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
    ShellStart,
    #[command(hide = true)]
    Launch {
        #[arg(long)]
        session_id: String,
    },
    #[command(hide = true)]
    LastSession {
        #[arg(long, value_enum)]
        tool: Tool,
        #[arg(long)]
        shell_pid: i32,
    },
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
    Restore {
        #[arg(
            long,
            help = "Include legacy recovery records without a confirmed pending interruption"
        )]
        all: bool,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Command::Daemon => commands::daemon::run(),
        Command::CargoTarget { command } => cargo_targets::run(&command),
        Command::Install { terminal } => commands::install::run(terminal),
        Command::InstallHooks => commands::install_hooks::run(),
        Command::Uninstall { purge } => commands::uninstall::run(purge),
        Command::Status => commands::status::run(),
        Command::ShellStart => commands::shell_start::run(),
        Command::Launch { session_id } => commands::launch::run(&session_id),
        Command::LastSession { tool, shell_pid } => commands::last_session::run(tool, shell_pid),
        Command::Register {
            tool,
            session_id,
            pid,
            shell_pid,
            directory,
            name,
        } => commands::register::run(tool, session_id, pid, shell_pid, directory, name),
        Command::Deregister { session_id } => commands::deregister::run(session_id),
        Command::Restore { all } => commands::restore::run(all),
    }
}
