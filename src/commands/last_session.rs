use crate::Tool;
use crate::last_sessions;
use crate::paths;
use crate::process;
use crate::sessions;
use anyhow::{Context, Result};

pub fn run(tool: Tool, shell_pid: i32) -> Result<()> {
    let shell_pid_started_at = process::process_start_identity(shell_pid)
        .with_context(|| format!("cannot identify shell process {shell_pid}"))?;

    sessions::repair_if_corrupt(&paths::sessions_file()?)?;
    let active = sessions::read_sessions(&paths::sessions_file()?)?
        .into_iter()
        .filter(|session| {
            session.tool == tool
                && session.shell_pid == Some(shell_pid)
                && session.shell_pid_started_at.as_deref() == Some(&shell_pid_started_at)
        })
        .max_by_key(|session| session.last_seen_at);

    if let Some(session) = active {
        println!("{}", session.session_id);
        return Ok(());
    }

    if let Some(session) = last_sessions::find(
        &paths::last_sessions_file()?,
        tool,
        shell_pid,
        &shell_pid_started_at,
    )? {
        println!("{}", session.session_id);
        return Ok(());
    }

    anyhow::bail!("no {tool} session recorded for shell process {shell_pid}")
}
