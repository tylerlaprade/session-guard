use crate::commands::daemon;
use crate::commands::register;
use crate::last_sessions::{self, LastSessionRecord};
use crate::paths;
use crate::process::ProcessSnapshot;
use crate::sessions::{self, SessionRecord};
use anyhow::{Context, Result};
use std::path::Path;

pub fn run(session_id: Option<String>) -> Result<()> {
    // An explicit --session-id is an operator decision and always retires
    // the record; only hook-driven SessionEnds get the teardown gate below.
    let (session_id, from_hook) = match session_id {
        Some(session_id) => (session_id, false),
        None => (
            register::read_session_id_from_hook_stdin()?.context(
                "missing session id; pass --session-id or run from a hook that provides session_id",
            )?,
            true,
        ),
    };

    let sessions_path = paths::sessions_file()?;
    sessions::repair_if_corrupt(&sessions_path)?;
    let Some(record) = sessions::read_sessions(&sessions_path)?
        .into_iter()
        .find(|session| session.session_id == session_id)
    else {
        return Ok(());
    };
    if from_hook && keep_for_crash_restore(&record) {
        return Ok(());
    }
    remember_and_deregister(&sessions_path, &paths::last_sessions_file()?, &record)
}

// A SessionEnd whose terminal tab is already gone is a GUI teardown
// (WindowServer death, logout, jetsam storm), not the user retiring the
// session: tools flush their hooks while the console session collapses
// around them (the 2026-08-25 incident deregistered three live tabs this
// way). Only a still-alive shell proves an in-tab lifecycle end — quit,
// /clear, /resume switching away — which really does retire the session.
// When the shell cannot be checked (ps failing under memory pressure),
// err toward keeping: deletion is irreversible, a kept record is marked
// recoverable by the monitor and expires on its own.
fn keep_for_crash_restore(record: &SessionRecord) -> bool {
    if record.shell_pid.is_none() {
        // Tabless (scan-tracked) sessions carry no teardown signal; a
        // graceful end is the only cleanup they get.
        return false;
    }
    !matches!(
        ProcessSnapshot::capture(),
        Ok(processes) if daemon::session_shell_is_alive(record, &processes)
    )
}

fn remember_and_deregister(
    sessions_path: &Path,
    last_sessions_path: &Path,
    record: &SessionRecord,
) -> Result<()> {
    if let Some(last_session) = LastSessionRecord::from_session(record) {
        last_sessions::remember(last_sessions_path, last_session)?;
    }
    sessions::deregister(sessions_path, &record.session_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Tool;
    use crate::sessions::SessionRecord;
    use std::path::PathBuf;

    #[test]
    fn clean_exit_remembers_session_by_shell_before_removing_active_record() {
        let dir = tempfile::tempdir().unwrap();
        let sessions_path = dir.path().join("active-sessions.json");
        let last_sessions_path = dir.path().join("last-sessions.json");
        let mut session = SessionRecord::new(
            Tool::Claude,
            "session-a".to_string(),
            Some(10),
            Some(20),
            PathBuf::from("/tmp/project"),
            None,
            None,
            Some("startup".to_string()),
        );
        session.shell_pid_started_at = Some("Wed Jan 1 00:00:00 2020".to_string());
        sessions::register(&sessions_path, session.clone()).unwrap();

        remember_and_deregister(&sessions_path, &last_sessions_path, &session).unwrap();

        assert!(sessions::read_sessions(&sessions_path).unwrap().is_empty());
        assert_eq!(
            last_sessions::find(
                &last_sessions_path,
                Tool::Claude,
                20,
                "Wed Jan 1 00:00:00 2020"
            )
            .unwrap()
            .unwrap()
            .session_id,
            "session-a"
        );
    }

    fn record_with_shell(
        shell_pid: Option<i32>,
        shell_pid_started_at: Option<String>,
    ) -> SessionRecord {
        let mut session = SessionRecord::new(
            Tool::Claude,
            "session-a".to_string(),
            Some(10),
            None,
            PathBuf::from("/tmp/project"),
            None,
            None,
            None,
        );
        session.shell_pid = shell_pid;
        session.shell_pid_started_at = shell_pid_started_at;
        session
    }

    #[test]
    fn session_end_with_dead_shell_is_a_teardown_and_keeps_the_record() {
        // Recycled/dead shell identity: the tab is gone, so the SessionEnd
        // came from a GUI teardown and the record must stay restorable.
        let record = record_with_shell(
            Some(std::process::id() as i32),
            Some("Wed Jan 1 00:00:00 2020".to_string()),
        );
        assert!(keep_for_crash_restore(&record));
    }

    #[test]
    fn session_end_with_live_shell_retires_the_record() {
        let pid = std::process::id() as i32;
        let record = record_with_shell(Some(pid), crate::process::process_start_identity(pid).ok());
        assert!(!keep_for_crash_restore(&record));
    }

    #[test]
    fn session_end_without_shell_retires_the_record() {
        assert!(!keep_for_crash_restore(&record_with_shell(None, None)));
    }
}
