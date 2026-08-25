use crate::Tool;
use anyhow::{Context, Result};
use std::env;
use std::path::PathBuf;

pub fn home_dir() -> Result<PathBuf> {
    env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .context("HOME is not set")
}

pub fn config_dir() -> Result<PathBuf> {
    Ok(home_dir()?.join(".config").join("session-guard"))
}

pub fn sessions_file() -> Result<PathBuf> {
    Ok(config_dir()?.join("active-sessions.json"))
}

pub fn last_sessions_file() -> Result<PathBuf> {
    Ok(config_dir()?.join("last-sessions.json"))
}

pub fn terminal_file() -> Result<PathBuf> {
    Ok(config_dir()?.join("terminal"))
}

pub fn daemon_log() -> Result<PathBuf> {
    Ok(config_dir()?.join("daemon.log"))
}

pub fn daemon_pid() -> Result<PathBuf> {
    Ok(config_dir()?.join("daemon.pid"))
}

pub fn daemon_heartbeat() -> Result<PathBuf> {
    Ok(config_dir()?.join("daemon-heartbeat"))
}

pub fn cargo_targets_dir() -> Result<PathBuf> {
    Ok(home_dir()?
        .join("Library")
        .join("Caches")
        .join("session-guard")
        .join("cargo-targets"))
}

/// The tool's own config/state root, honoring the same env overrides the tools
/// themselves use. Sessions whose working directory lives inside this root are
/// internal tool activity (e.g. codex memory maintenance under `~/.codex`),
/// never user project sessions.
pub fn tool_home(tool: Tool) -> Result<PathBuf> {
    tool.spec().home.resolve()
}

pub fn launch_agent_plist() -> Result<PathBuf> {
    Ok(home_dir()?
        .join("Library")
        .join("LaunchAgents")
        .join("com.tylerlaprade.session-guard.plist"))
}
