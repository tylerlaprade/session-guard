use crate::commands::register;
use crate::harness::ThreadServer;
use crate::last_sessions::{self, LastSessionRecord};
use crate::sessions::{self, SessionRecord};
use crate::{paths, process};
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
        if let Some(server) = &record.tool.spec().thread_server
            && !ended_by_its_server(
                &record,
                server,
                register::hook_emitter(i32::try_from(std::process::id())?),
            )
        {
            return Ok(());
        }
        return end_from_hook(&sessions_path, &paths::last_sessions_file()?, &record);
    }
    remember_and_deregister(&sessions_path, &paths::last_sessions_file()?, &record)
}

// A SessionEnd says the session id is finished, not why. Tools fire it for an
// in-tab quit, for a tab closed with the tool inside, and while the terminal
// itself is going down — and at that instant the tab's shell can be alive or
// dead in every one of those cases (2026-08-25 lost three live tabs because
// the shells were already gone; 2026-09-03 lost four because the shells
// outlived a crashed Ghostty by a second). So the hook only marks the record
// ending; the daemon settles it once the aftermath is visible
// (daemon::settle_endings). Tabless (scan-tracked) sessions carry no tab to
// observe, so a graceful end is the only cleanup they get.
pub(crate) fn end_from_hook(
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

// Codex can deliver the old daemon's ends after its successor resumes the live TUIs.
fn ended_by_its_server(
    record: &SessionRecord,
    server: &ThreadServer,
    emitter: Option<i32>,
) -> bool {
    let Some(server_pid) = record.server_pid else {
        return !emitter.is_some_and(server.is_server);
    };
    emitter == Some(server_pid)
        && process::process_start_identity(server_pid).ok() == record.server_pid_started_at
        && (server.serving)(record.tool) == Some(server_pid)
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

        assert_eq!(
            sessions::read_sessions(&sessions_path).unwrap(),
            Vec::<SessionRecord>::new()
        );
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
    fn a_late_session_end_leaves_a_released_record_out_of_the_next_relaunch() {
        let dir = tempfile::tempdir().unwrap();
        let sessions_path = dir.path().join("active-sessions.json");
        let last_sessions_path = dir.path().join("last-sessions.json");
        let processes = crate::process::ProcessSnapshot::capture().unwrap();
        let dead = "Wed Jan 1 00:00:00 2020".to_string();
        let mut record = record_with_shell(Some(i32::MAX), Some(dead.clone()));
        record.pid_started_at = Some(dead);
        record.mark_recoverable();
        record.restore_pending = false;
        sessions::register(&sessions_path, record.clone()).unwrap();

        end_from_hook(&sessions_path, &last_sessions_path, &record).unwrap();

        let session = &sessions::read_sessions(&sessions_path).unwrap()[0];
        assert_eq!(session.state, SessionState::Recoverable);
        assert!(!session.restore_pending);
        assert!(!crate::commands::daemon::needs_terminal_restore(
            session, &processes
        ));
    }

    fn own_pid() -> i32 {
        i32::try_from(std::process::id()).unwrap()
    }

    const SERVING_THIS_PROCESS: ThreadServer = ThreadServer {
        is_server: crate::codex_server::is_server,
        serving: |_| Some(i32::try_from(std::process::id()).unwrap()),
        client: |_| Ok(None),
    };

    fn served_record_naming_this_process() -> SessionRecord {
        let mut record = record_with_shell(Some(own_pid()), None);
        record.server_pid = Some(own_pid());
        record.server_pid_started_at = crate::process::process_start_identity(own_pid()).ok();
        record
    }

    #[test]
    fn a_served_thread_ends_only_from_the_serving_server_its_record_names() {
        let record = served_record_naming_this_process();
        assert!(ended_by_its_server(
            &record,
            &SERVING_THIS_PROCESS,
            Some(own_pid())
        ));

        let serving_another = ThreadServer {
            serving: |_| Some(1),
            ..SERVING_THIS_PROCESS
        };
        assert!(!ended_by_its_server(
            &record,
            &serving_another,
            Some(own_pid())
        ));
        let serving_none = ThreadServer {
            serving: |_| None,
            ..SERVING_THIS_PROCESS
        };
        assert!(!ended_by_its_server(
            &record,
            &serving_none,
            Some(own_pid())
        ));

        let mut stale = record;
        stale.server_pid_started_at = Some("Wed Jan 1 00:00:00 2020".to_string());
        assert!(!ended_by_its_server(
            &stale,
            &SERVING_THIS_PROCESS,
            Some(own_pid())
        ));

        let unserved = record_with_shell(Some(own_pid()), None);
        assert!(ended_by_its_server(&unserved, &serving_none, None));
        assert!(ended_by_its_server(
            &unserved,
            &SERVING_THIS_PROCESS,
            Some(own_pid())
        ));
    }

    // Resuming a thread shuts down the runtime its dead client left loaded,
    // which fires SessionEnd before the restored record names any server.
    #[test]
    fn a_server_ending_a_thread_its_record_does_not_name_it_for_is_ignored() {
        let restored = record_with_shell(Some(own_pid()), None);
        let any_server = ThreadServer {
            is_server: |_| true,
            ..SERVING_THIS_PROCESS
        };
        assert!(!ended_by_its_server(
            &restored,
            &any_server,
            Some(own_pid())
        ));
    }

    #[test]
    fn a_replaced_server_ending_a_thread_its_successor_took_over_is_ignored() {
        let refreshed_by_the_successor = served_record_naming_this_process();
        let replaced = crate::process::parent_pid(own_pid()).unwrap();
        assert!(!ended_by_its_server(
            &refreshed_by_the_successor,
            &SERVING_THIS_PROCESS,
            Some(replaced)
        ));
        assert!(!ended_by_its_server(
            &refreshed_by_the_successor,
            &SERVING_THIS_PROCESS,
            None
        ));
    }

    #[test]
    fn session_end_without_shell_retires_the_record() {
        let dir = tempfile::tempdir().unwrap();
        let sessions_path = dir.path().join("active-sessions.json");
        let last_sessions_path = dir.path().join("last-sessions.json");
        let record = record_with_shell(None, None);
        sessions::register(&sessions_path, record.clone()).unwrap();

        end_from_hook(&sessions_path, &last_sessions_path, &record).unwrap();

        assert_eq!(
            sessions::read_sessions(&sessions_path).unwrap(),
            Vec::<SessionRecord>::new()
        );
    }
}
