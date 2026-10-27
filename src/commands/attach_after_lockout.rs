use crate::paths;
use crate::sessions;
use anyhow::Result;

/// Marks a background session for a tab once the terminal is no longer locked
/// out; the daemon opens it. `id` is the session id, or the start of it that
/// `claude --bg` prints. A session that has not registered yet is marked when
/// it does.
pub fn run(id: &str) -> Result<()> {
    if id.len() < 8 {
        anyhow::bail!("'{id}' is too short to name one session; give at least 8 characters");
    }
    sessions::attach_after_lockout(&paths::sessions_file()?, id).map(drop)
}
