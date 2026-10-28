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

/// Names this boot's processes, so it lives in machine-local storage.
pub fn lockout_state() -> Result<PathBuf> {
    Ok(user_temp_dir()?.join("session-guard-lockout.json"))
}

pub fn lockout_launch() -> Result<PathBuf> {
    Ok(user_temp_dir()?.join("session-guard-lockout-launch.json"))
}

// launchd gives the LaunchAgent no TMPDIR, so the daemon asks for the
// per-user directory that shells' TMPDIR names.
fn user_temp_dir() -> Result<PathBuf> {
    if let Some(directory) = env::var_os("TMPDIR").filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(directory));
    }
    darwin_user_temp_dir()
}

#[cfg(target_os = "macos")]
fn darwin_user_temp_dir() -> Result<PathBuf> {
    use std::os::unix::ffi::OsStringExt;
    let mut buffer = vec![0_u8; usize::try_from(libc::PATH_MAX)?];
    let length = unsafe {
        libc::confstr(
            libc::_CS_DARWIN_USER_TEMP_DIR,
            buffer.as_mut_ptr().cast(),
            buffer.len(),
        )
    };
    if length == 0 || length > buffer.len() {
        anyhow::bail!("macOS reported no per-user temporary directory");
    }
    buffer.truncate(length - 1);
    Ok(PathBuf::from(std::ffi::OsString::from_vec(buffer)))
}

#[cfg(not(target_os = "macos"))]
fn darwin_user_temp_dir() -> Result<PathBuf> {
    Ok(env::temp_dir())
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
