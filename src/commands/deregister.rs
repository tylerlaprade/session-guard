use crate::commands::register;
use crate::paths;
use crate::sessions;
use anyhow::{Context, Result};

pub fn run(session_id: Option<String>) -> Result<()> {
    let session_id = match session_id {
        Some(session_id) => session_id,
        None => register::read_session_id_from_hook_stdin()?.context(
            "missing session id; pass --session-id or run from a hook that provides session_id",
        )?,
    };

    sessions::repair_if_corrupt(&paths::sessions_file()?)?;
    sessions::deregister(&paths::sessions_file()?, &session_id)
}
