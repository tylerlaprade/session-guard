use crate::commands::{daemon, deregister};
use crate::paths;
use crate::process::{self, ProcessSnapshot};
use crate::sessions::{self, SessionRecord};
use anyhow::{Context, Result, bail};
use std::os::unix::process::ExitStatusExt;
use std::process::Command;

pub fn run(session_id: &str) -> Result<()> {
    let path = paths::sessions_file()?;
    let owner = i32::try_from(std::process::id())?;
    let owner_started = process::process_start_identity(owner)?;
    let record = sessions::with_sessions_mut(&path, |sessions| {
        let processes = ProcessSnapshot::capture()?;
        let session = sessions
            .iter_mut()
            .find(|session| session.session_id == session_id)
            .context("restore record no longer exists")?;
        if daemon::session_is_alive(session, &processes) {
            bail!("session {session_id} already has a live owner");
        }
        claim(session, owner, &owner_started);
        Ok(session.clone())
    })?;
    run_claimed(&record)
}

pub(crate) fn claim(session: &mut SessionRecord, owner: i32, started: &str) {
    session.pid = Some(owner);
    session.shell_pid = Some(owner);
    session.pid_started_at = Some(started.to_string());
    session.shell_pid_started_at = Some(started.to_string());
    session.mark_active();
    session.source = Some("restore-launch".to_string());
}

pub(crate) fn run_claimed(record: &SessionRecord) -> Result<()> {
    let owner = std::process::id();
    daemon::log_line(&format!(
        "restore owner registered: {} {} pid={owner}",
        record.tool, record.session_id
    ))?;
    let shell = std::env::var_os("SHELL").unwrap_or_else(|| "/bin/sh".into());
    let command = daemon::resume_command(record);
    let result = Command::new(shell)
        .args(["-ic", &command])
        .current_dir(&record.directory)
        .env("SESSION_GUARD_SHELL_PID", owner.to_string())
        .status();
    finish(
        record,
        result.as_ref().is_ok_and(std::process::ExitStatus::success),
    )?;
    let status = result.context("failed to launch restored session")?;
    daemon::log_line(&format!(
        "restored command exited: {} {} status={status}",
        record.tool, record.session_id
    ))?;
    std::process::exit(
        status
            .code()
            .unwrap_or_else(|| 128 + status.signal().unwrap_or(1)),
    );
}

fn finish(record: &SessionRecord, succeeded: bool) -> Result<()> {
    let path = paths::sessions_file()?;
    if !succeeded {
        return sessions::with_sessions_mut(&path, |sessions| {
            if let Some(current) = sessions
                .iter_mut()
                .find(|session| session.session_id == record.session_id)
            {
                current.mark_recoverable();
            }
            Ok(())
        });
    }
    if let Some(current) = sessions::read_sessions(&path)?
        .into_iter()
        .find(|session| session.session_id == record.session_id)
    {
        deregister::end_from_hook(&path, &paths::last_sessions_file()?, &current)?;
    }
    Ok(())
}
