use crate::commands::register;
use crate::last_sessions::{self, LastSessionRecord};
use crate::paths;
use crate::sessions::{self, SessionRecord};
use anyhow::{Context, Result};
use std::path::Path;

pub fn run(session_id: Option<String>) -> Result<()> {
    // An explicit --session-id is an operator decision and always retires
    // the record; only hook-driven SessionEnds are deferred below.
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
    if from_hook {
        return end_from_hook(&sessions_path, &paths::last_sessions_file()?, &record);
    }
    remember_and_deregister(&sessions_path, &paths::last_sessions_file()?, &record)
}

// A SessionEnd says the session id is finished, not why. Tools fire it for an
// in-tab quit, for a tab closed with the tool inside, and while the terminal
// itself is going down — and at that instant the tab's shell can be alive or
// dead in every one of those cases (2026-08-25 lost three live tabs because
// the shells were already gone; 2026-09-03 lost four because the shells
// outlived a quitting Ghostty by a second). So the hook only marks the record
// ending; the daemon settles it once the aftermath is visible
// (daemon::settle_endings). Tabless (scan-tracked) sessions carry no tab to
// observe, so a graceful end is the only cleanup they get.
fn end_from_hook(
    sessions_path: &Path,
    last_sessions_path: &Path,
    record: &SessionRecord,
) -> Result<()> {
    if record.shell_pid.is_none() {
        return remember_and_deregister(sessions_path, last_sessions_path, record);
    }
    sessions::with_sessions_mut(sessions_path, |sessions| {
        if let Some(session) = sessions
            .iter_mut()
            .find(|session| session.session_id == record.session_id)
        {
            session.mark_ending();
        }
        Ok(())
    })
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
    use crate::sessions::{SessionRecord, SessionState};
    use std::path::PathBuf;

    #[test]
    fn clean_exit_remembers_session_by_shell_before_removing_active_record() {
        let dir = tempfile::tempdir().unwrap();
        let sessions_path = dir.path().join("active-sessions.json");
        let last_sessions_path = dir.path().join("last-sessions.json");
        let mut session = SessionRecord::new(
            crate::tool("claude"),
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
                crate::tool("claude"),
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
            crate::tool("claude"),
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
    fn session_end_from_a_tab_only_marks_the_record_ending() {
        let dir = tempfile::tempdir().unwrap();
        let sessions_path = dir.path().join("active-sessions.json");
        let last_sessions_path = dir.path().join("last-sessions.json");
        let pid = std::process::id() as i32;
        let record = record_with_shell(Some(pid), crate::process::process_start_identity(pid).ok());
        sessions::register(&sessions_path, record.clone()).unwrap();

        end_from_hook(&sessions_path, &last_sessions_path, &record).unwrap();

        let sessions = sessions::read_sessions(&sessions_path).unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].state, SessionState::Ending);
        assert!(sessions[0].ending_at.is_some());
        assert!(!last_sessions_path.exists());
    }

    #[test]
    fn session_end_without_shell_retires_the_record() {
        let dir = tempfile::tempdir().unwrap();
        let sessions_path = dir.path().join("active-sessions.json");
        let last_sessions_path = dir.path().join("last-sessions.json");
        let record = record_with_shell(None, None);
        sessions::register(&sessions_path, record.clone()).unwrap();

        end_from_hook(&sessions_path, &last_sessions_path, &record).unwrap();

        assert!(sessions::read_sessions(&sessions_path).unwrap().is_empty());
    }
}
