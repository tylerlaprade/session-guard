use crate::adapters::shell_quote;
use crate::commands::{daemon, deregister};
use crate::paths;
use crate::process::{self, ProcessSnapshot};
use crate::sessions::{self, SessionRecord};
use anyhow::{Context, Result, bail};
use std::os::unix::process::ExitStatusExt;
use std::process::Command;

pub(crate) const SHELL_PID_ENV: &str = "SESSION_GUARD_SHELL_PID";

struct LaunchRequest {
    record: SessionRecord,
    prompt: Option<String>,
}

pub fn run(session_id: &str) -> Result<()> {
    let path = paths::sessions_file()?;
    let owner = i32::try_from(std::process::id())?;
    let owner_started = process::process_start_identity(owner)?;
    loop {
        let request = sessions::with_sessions_mut(&path, |sessions| {
            let processes = ProcessSnapshot::capture()?;
            let session = sessions
                .iter_mut()
                .find(|session| session.session_id == session_id)
                .context("restore record no longer exists")?;
            if daemon::session_is_alive(session, &processes) {
                bail!("session {session_id} already has a live owner");
            }
            Ok(claim(session, owner, &owner_started))
        })?;
        let status = run_claimed(&request)?;
        if let Some(signal) = crash_signal(status)
            && came_up(&path, session_id, owner)?
        {
            daemon::log_line(&format!(
                "relaunching in its tab after signal {signal}: {} {session_id}",
                request.record.tool
            ))?;
            eprintln!(
                "session-guard: {} was killed by signal {signal}. Resuming it here.",
                request.record.tool.spec().display_name
            );
            wait_out_critical_memory_pressure();
            continue;
        }
        finish(&request.record, status.success())?;
        std::process::exit(
            status
                .code()
                .unwrap_or_else(|| 128 + status.signal().unwrap_or(1)),
        );
    }
}

/// The signal that killed the tool, when it was killed rather than stopped:
/// out of memory, by a crash, or by `kill -9`. A closed tab's SIGHUP and the
/// deliberate SIGINT and SIGTERM are stops. The tool runs under `$SHELL -ic`,
/// which reports a command killed by signal N as exit status 128 + N.
fn crash_signal(status: std::process::ExitStatus) -> Option<i32> {
    let signal = status.signal().or_else(|| {
        status
            .code()
            .filter(|code| *code > 128)
            .map(|code| code - 128)
    })?;
    [
        libc::SIGKILL,
        libc::SIGSEGV,
        libc::SIGBUS,
        libc::SIGABRT,
        libc::SIGILL,
        libc::SIGFPE,
        libc::SIGTRAP,
        libc::SIGSYS,
    ]
    .contains(&signal)
    .then_some(signal)
}

// The tool's registration replaces this launcher as the record's owner, so a
// tool that crashes before it registers is not relaunched into a loop.
fn came_up(path: &std::path::Path, session_id: &str, owner: i32) -> Result<bool> {
    Ok(sessions::read_sessions(path)?
        .iter()
        .any(|session| session.session_id == session_id && session.pid != Some(owner)))
}

fn wait_out_critical_memory_pressure() {
    if !process::memory_pressure_is_critical() {
        return;
    }
    eprintln!("session-guard: waiting for memory pressure to ease first.");
    while process::memory_pressure_is_critical() {
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
}

fn claim(session: &mut SessionRecord, owner: i32, started: &str) -> LaunchRequest {
    let prompt = crate::continuation::restore_prompt(session);
    session.pid = Some(owner);
    session.shell_pid = Some(owner);
    session.pid_started_at = Some(started.to_string());
    session.shell_pid_started_at = Some(started.to_string());
    session.server_pid = None;
    session.server_pid_started_at = None;
    session.mark_active();
    session.source = Some("restore-launch".to_string());
    session.activity = None;
    session.background_tasks = None;
    LaunchRequest {
        record: session.clone(),
        prompt,
    }
}

fn run_claimed(request: &LaunchRequest) -> Result<std::process::ExitStatus> {
    let record = &request.record;
    let owner = std::process::id();
    daemon::log_line(&format!(
        "restore owner registered: {} {} pid={owner}",
        record.tool, record.session_id
    ))?;
    let shell = std::env::var_os("SHELL").unwrap_or_else(|| "/bin/sh".into());
    let mut command = daemon::resume_command(record);
    if let Some(prompt) = &request.prompt {
        command.push(' ');
        command.push_str(&shell_quote(prompt));
        daemon::log_line(&format!(
            "continuing interrupted work: {} {}",
            record.tool, record.session_id
        ))?;
    }
    let result = Command::new(shell)
        .args(["-ic", &command])
        .current_dir(&record.directory)
        .env(SHELL_PID_ENV, owner.to_string())
        .status();
    if result.is_err() {
        finish(record, false)?;
    }
    let status = result.context("failed to launch restored session")?;
    daemon::log_line(&format!(
        "restored command exited: {} {} status={status}",
        record.tool, record.session_id
    ))?;
    Ok(status)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_kill_or_crash_counts_as_a_crash() {
        let exited = |code: i32| std::process::ExitStatus::from_raw(code << 8);
        let signaled = std::process::ExitStatus::from_raw;
        assert_eq!(
            crash_signal(exited(128 + libc::SIGKILL)),
            Some(libc::SIGKILL)
        );
        assert_eq!(
            crash_signal(exited(128 + libc::SIGSEGV)),
            Some(libc::SIGSEGV)
        );
        assert_eq!(crash_signal(signaled(libc::SIGABRT)), Some(libc::SIGABRT));
        for stopped in [libc::SIGHUP, libc::SIGINT, libc::SIGTERM] {
            assert_eq!(crash_signal(exited(128 + stopped)), None, "{stopped}");
            assert_eq!(crash_signal(signaled(stopped)), None, "{stopped}");
        }
        for code in [0, 1, 2, 127, 128] {
            assert_eq!(crash_signal(exited(code)), None, "{code}");
        }
    }
}
