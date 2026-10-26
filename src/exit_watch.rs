use crate::commands::{daemon, deregister};
use crate::paths;
use crate::process::{self, ExitWatch, ProcessIdentityStatus};
use crate::sessions::{self, SessionRecord, SessionState};
use anyhow::{Context, Result};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

/// Watches each live TUI on a shared thread server for its exit. Such a TUI
/// fires no `SessionEnd` when it quits, so its exit status is the only word on
/// how it ended: zero is a quit, which ends its record as a `SessionEnd` would,
/// and a signal is a kill, a crash or a closed tab, which the daemon already
/// judges by what the terminal does next.
pub struct Watcher {
    stop: Arc<AtomicBool>,
    follower: JoinHandle<Result<()>>,
}

impl Watcher {
    pub fn start() -> Result<Self> {
        let queue = ExitWatch::new().context("failed to open a kernel queue")?;
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = Arc::clone(&stop);
        let follower = thread::Builder::new()
            .name("exit-watch".to_string())
            .spawn(move || follow(&queue, &stopping))?;
        Ok(Self { stop, follower })
    }

    pub fn is_running(&self) -> bool {
        !self.follower.is_finished()
    }

    pub fn reap(self) -> Result<()> {
        self.stop.store(true, Ordering::Relaxed);
        self.follower
            .join()
            .map_err(|_| anyhow::anyhow!("exit watcher panicked"))?
    }
}

fn follow(queue: &ExitWatch, stop: &AtomicBool) -> Result<()> {
    let path = paths::sessions_file()?;
    let mut watched: HashMap<i32, String> = HashMap::new();
    let mut read_at = None;
    while !stop.load(Ordering::Relaxed) {
        let modified = std::fs::metadata(&path)
            .and_then(|file| file.modified())
            .ok();
        if modified != read_at {
            read_at = modified;
            for record in sessions::read_sessions(&path)? {
                watch_served_tui(queue, &mut watched, &record);
            }
        }
        for (pid, status) in queue.exits(Duration::from_secs(1)) {
            if let Some(started) = watched.remove(&pid)
                && status == 0
            {
                end_quit_records(&path, pid, &started)?;
            }
        }
    }
    Ok(())
}

fn watch_served_tui(queue: &ExitWatch, watched: &mut HashMap<i32, String>, record: &SessionRecord) {
    let (Some(pid), Some(started)) = (record.pid, record.pid_started_at.as_deref()) else {
        return;
    };
    if record.state == SessionState::Active
        && record.server_pid.is_some()
        && !watched.contains_key(&pid)
        && matches!(
            process::process_identity_status(pid, started),
            ProcessIdentityStatus::Alive
        )
        && queue.watch(pid)
    {
        watched.insert(pid, started.to_string());
    }
}

fn end_quit_records(path: &Path, pid: i32, started: &str) -> Result<()> {
    let quit: Vec<SessionRecord> = sessions::read_sessions(path)?
        .into_iter()
        .filter(|record| {
            record.server_pid.is_some()
                && record.pid == Some(pid)
                && record.pid_started_at.as_deref() == Some(started)
        })
        .collect();
    for record in quit {
        deregister::end_from_hook(path, &paths::last_sessions_file()?, &record)?;
        daemon::log_line(&format!(
            "served TUI quit: {} {}",
            record.tool, record.session_id
        ))?;
    }
    Ok(())
}
