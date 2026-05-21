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

pub fn launch_agent_plist() -> Result<PathBuf> {
    Ok(home_dir()?
        .join("Library")
        .join("LaunchAgents")
        .join("com.tylerlaprade.session-guard.plist"))
}
