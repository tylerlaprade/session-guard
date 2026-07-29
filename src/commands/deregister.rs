use crate::commands::register;
use crate::last_sessions::{self, LastSessionRecord};
use crate::paths;
use crate::sessions;
use anyhow::{Context, Result};
use std::path::Path;

pub fn run(session_id: Option<String>) -> Result<()> {
    let session_id = match session_id {
        Some(session_id) => session_id,
        None => register::read_session_id_from_hook_stdin()?.context(
            "missing session id; pass --session-id or run from a hook that provides session_id",
        )?,
    };

    sessions::repair_if_corrupt(&paths::sessions_file()?)?;
    remember_and_deregister(
        &paths::sessions_file()?,
        &paths::last_sessions_file()?,
        &session_id,
    )
}

fn remember_and_deregister(
    sessions_path: &Path,
    last_sessions_path: &Path,
    session_id: &str,
) -> Result<()> {
    if let Some(record) = sessions::read_sessions(sessions_path)?
        .iter()
        .find(|session| session.session_id == session_id)
        .and_then(LastSessionRecord::from_session)
    {
        last_sessions::remember(last_sessions_path, record)?;
    }
    sessions::deregister(sessions_path, session_id)
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
        sessions::register(&sessions_path, session).unwrap();

        remember_and_deregister(&sessions_path, &last_sessions_path, "session-a").unwrap();

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
}
