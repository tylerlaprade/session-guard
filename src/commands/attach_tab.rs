use crate::paths;
use crate::process;
use crate::sessions;
use anyhow::{Context, Result};
use std::os::unix::process::ExitStatusExt;
use std::process::Command;

/// The exit status of a tab that found the session already shown in another;
/// the line the daemon types into the tab closes it on this status.
pub const ALREADY_SHOWN: i32 = 75;

/// What a background session's tab runs: claims the tab for the session under
/// the registry lock, then shows the session. Ghostty can still add a tab
/// after its script timed out, beside the one a retry opened, so the second
/// tab to claim a session leaves it to the first.
pub fn run(session_id: &str) -> Result<()> {
    let path = paths::sessions_file()?;
    let owner = i32::try_from(std::process::id())?;
    let started = process::process_start_identity(owner)?;
    let attach = sessions::with_sessions_mut(&path, |sessions| {
        let session = sessions
            .iter_mut()
            .find(|session| session.session_id == session_id)
            .with_context(|| format!("session-guard has no record of session {session_id}"))?;
        if session.has_attach_tab() {
            return Ok(None);
        }
        let background = session
            .tool
            .spec()
            .background
            .as_ref()
            .with_context(|| format!("{} has no background mode", session.tool))?;
        session.attach_tab_pid = Some(owner);
        session.attach_tab_started_at = Some(started.clone());
        session.attach_after_lockout = false;
        session.attach_tab_pending = false;
        session.attach_keeps_selection = false;
        Ok(Some((background.attach)(session_id)))
    })?;
    let Some(attach) = attach else {
        eprintln!("session-guard: session {session_id} is already shown in another tab.");
        std::process::exit(ALREADY_SHOWN);
    };
    let shell = std::env::var_os("SHELL").unwrap_or_else(|| "/bin/sh".into());
    let status = Command::new(shell)
        .args(["-ic", &attach])
        .status()
        .context("failed to show the background session")?;
    let code = status
        .code()
        .unwrap_or_else(|| 128 + status.signal().unwrap_or(1));
    // The tab closes on ALREADY_SHOWN; the attach command's own failures keep it open.
    std::process::exit(if code == ALREADY_SHOWN { 1 } else { code });
}
