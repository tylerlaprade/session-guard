use crate::paths;
use crate::sessions;
use anyhow::{Context, Result};
use std::thread;
use std::time::{Duration, Instant};

// A background session registers through its own hooks a moment after
// `claude --bg` returns.
const REGISTRATION_TIMEOUT: Duration = Duration::from_secs(15);

/// Marks a background session for a tab once the terminal is no longer locked
/// out; the daemon opens it. `id` is the session id, or the start of it that
/// `claude --bg` prints.
pub fn run(id: &str) -> Result<()> {
    if id.len() < 8 {
        anyhow::bail!("'{id}' is too short to name one session; give at least 8 characters");
    }
    let path = paths::sessions_file()?;
    let deadline = Instant::now() + REGISTRATION_TIMEOUT;
    loop {
        let marked = sessions::with_sessions_mut(&path, |sessions| {
            let mut matches = sessions
                .iter_mut()
                .filter(|session| session.session_id.starts_with(id));
            let Some(session) = matches.next() else {
                return Ok(false);
            };
            if matches.next().is_some() {
                anyhow::bail!("'{id}' matches more than one session");
            }
            session
                .tool
                .spec()
                .background
                .as_ref()
                .with_context(|| format!("{} has no background mode", session.tool))?;
            session.attach_after_lockout = true;
            Ok(true)
        })?;
        if marked {
            return Ok(());
        }
        if Instant::now() >= deadline {
            anyhow::bail!("no session starting with '{id}' registered");
        }
        thread::sleep(Duration::from_millis(200));
    }
}
