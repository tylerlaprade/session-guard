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

pub fn terminal_file() -> Result<PathBuf> {
    Ok(config_dir()?.join("terminal"))
}

pub fn daemon_log() -> Result<PathBuf> {
    Ok(config_dir()?.join("daemon.log"))
}

pub fn daemon_pid() -> Result<PathBuf> {
    Ok(config_dir()?.join("daemon.pid"))
}

pub fn claude_settings() -> Result<PathBuf> {
    Ok(home_dir()?.join(".claude").join("settings.json"))
}

pub fn codex_config() -> Result<PathBuf> {
    Ok(home_dir()?.join(".codex").join("config.toml"))
}

/// The tool's own config/state root, honoring the same env overrides the tools
/// themselves use. Sessions whose working directory lives inside this root are
/// internal tool activity (e.g. codex memory maintenance under `~/.codex`),
/// never user project sessions.
pub fn tool_home(tool: Tool) -> Result<PathBuf> {
    match tool {
        Tool::Claude => Ok(env::var_os("CLAUDE_CONFIG_DIR")
            .map(PathBuf::from)
            .unwrap_or(home_dir()?.join(".claude"))),
        Tool::Codex => Ok(env::var_os("CODEX_HOME")
            .map(PathBuf::from)
            .unwrap_or(home_dir()?.join(".codex"))),
    }
}

pub fn launch_agent_plist() -> Result<PathBuf> {
    Ok(home_dir()?
        .join("Library")
        .join("LaunchAgents")
        .join("com.tylerlaprade.session-guard.plist"))
}
