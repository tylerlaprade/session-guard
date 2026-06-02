use crate::adapters::{adapter_for, shell_quote};
use crate::paths;
use crate::process;
use crate::scan;
use crate::sessions::{self, SessionRecord, SessionState};
use crate::transcripts;
use crate::{TerminalKind, Tool};
use anyhow::{Context, Result};
use signal_hook::consts::signal::{SIGINT, SIGTERM, SIGUSR1};
use signal_hook::flag;
use std::collections::{HashMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

#[derive(Debug, Default)]
pub struct RestoreSummary {
    pub restored_claude: usize,
    pub restored_codex: usize,
    pub pruned_missing_dirs: usize,
    pub pruned_duplicates: usize,
    pub preserved_recoverable: usize,
    pub fallback_sessions: usize,
    pub failed: usize,
    pub errors: Vec<String>,
}

impl RestoreSummary {
    pub fn restored_total(&self) -> usize {
        self.restored_claude + self.restored_codex
    }

    pub fn message(&self) -> String {
        let mut message = format!(
            "Restored {} sessions ({} Claude Code, {} Codex). Pruned {} (directory gone).",
            self.restored_total(),
            self.restored_claude,
            self.restored_codex,
            self.pruned_missing_dirs,
        );
        if self.pruned_duplicates > 0 {
            message.push_str(&format!(
                " Pruned {} duplicate entries.",
                self.pruned_duplicates
            ));
        }
        if self.preserved_recoverable > 0 {
            message.push_str(&format!(
                " Preserved {} recoverable sessions.",
                self.preserved_recoverable
            ));
        }
        if self.fallback_sessions > 0 {
            message.push_str(&format!(
                " Added {} transcript fallback sessions.",
                self.fallback_sessions
            ));
        }
        if self.failed > 0 {
            message.push_str(&format!(" Failed to restore {} sessions.", self.failed));
        }
        message
    }
}

#[derive(Debug, Default)]
pub struct MonitorSummary {
    pub marked_recoverable: usize,
    pub removed_closed: usize,
    pub pruned_expired: usize,
}

#[derive(Debug, Clone, Copy)]
pub enum RestoreMode {
    Startup,
    Manual,
}

pub fn run() -> Result<()> {
    fs::create_dir_all(paths::config_dir()?)?;
    sessions::ensure_store(&paths::sessions_file()?)?;

    if let Some(pid) = running_daemon_pid()? {
        println!("session-guard daemon already running with pid {pid}");
        return Ok(());
    }

    write_pid_file()?;
    log_line("daemon started")?;

    let added = reconcile_from_scan()?;
    if added > 0 {
        log_line(&format!("scan added {added} sessions"))?;
    }

    let shutdown = Arc::new(AtomicBool::new(false));
    let restore_requested = Arc::new(AtomicBool::new(false));
    flag::register(SIGTERM, Arc::clone(&shutdown))?;
    flag::register(SIGINT, Arc::clone(&shutdown))?;
    flag::register(SIGUSR1, Arc::clone(&restore_requested))?;

    let summary = restore_once(RestoreMode::Startup)?;
    log_line(&summary.message())?;
    for error in &summary.errors {
        log_line(error)?;
    }

    let mut seconds_until_monitor = 60;
    while !shutdown.load(Ordering::Relaxed) {
        thread::sleep(Duration::from_secs(1));

        if restore_requested.swap(false, Ordering::Relaxed) {
            let summary = restore_once(RestoreMode::Manual)?;
            log_line(&format!("manual restore: {}", summary.message()))?;
            for error in &summary.errors {
                log_line(error)?;
            }
        }

        seconds_until_monitor -= 1;
        if seconds_until_monitor == 0 {
            let added = reconcile_from_scan()?;
            if added > 0 {
                log_line(&format!("scan added {added} sessions"))?;
            }

            let summary = monitor_once()?;
            if summary.marked_recoverable > 0 {
                log_line(&format!(
                    "monitor marked {} sessions recoverable",
                    summary.marked_recoverable
                ))?;
            }
            if summary.removed_closed > 0 {
                log_line(&format!(
                    "monitor removed {} sessions whose tool and shell exited",
                    summary.removed_closed
                ))?;
            }
            if summary.pruned_expired > 0 {
                log_line(&format!(
                    "monitor pruned {} expired recoverable sessions",
                    summary.pruned_expired
                ))?;
            }
            seconds_until_monitor = 60;
        }
    }

    log_line("daemon stopped")?;
    remove_pid_file_if_current()?;
    Ok(())
}

pub fn reconcile_from_scan() -> Result<usize> {
    let path = paths::sessions_file()?;
    let _ = sessions::repair_if_corrupt(&path)?;
    let scanned = process::list_processes()
        .and_then(|processes| scan::discover_sessions(&processes))
        .unwrap_or_default();

    sessions::with_sessions_mut(&path, |sessions| {
        let mut added = 0;
        for scanned_session in scanned {
            if let Some(existing) = sessions
                .iter_mut()
                .find(|session| session.session_id == scanned_session.session_id)
            {
                existing.pid = scanned_session.pid;
                existing.mark_active();
            } else {
                sessions.push(scanned_session);
                added += 1;
            }
        }
        Ok(added)
    })
}

// The daemon's monitor cycle bumps `last_seen_at` for every alive session
// roughly every 60 seconds. So when the daemon restarts, the maximum
// `last_seen_at` in the file marks the last moment the previous daemon ran —
// i.e., right before it (and usually the sessions running alongside it) went
// away. Sessions whose `last_seen_at` falls within this window of that maximum
// were alive when the daemon stopped and are restored; anything much older was
// already sitting in the recoverable pile and is left alone.
//
// This deliberately keys off the daemon's own last heartbeat rather than the
// kernel boot time. A logout or GUI-session crash kills every terminal — and
// the daemon with them — without rebooting the kernel, so a boot-time check
// would never fire for those sessions even though they should come back.
const LIVENESS_CLUSTER_WINDOW_SECS: i64 = 120;

pub fn restore_once(mode: RestoreMode) -> Result<RestoreSummary> {
    let path = paths::sessions_file()?;
    let _ = sessions::repair_if_corrupt(&path)?;
    let fallback_sessions = transcripts::discover_recent_sessions().unwrap_or_default();
    let mut adapter = None;

    sessions::with_sessions_mut(&path, |sessions| {
        let mut summary = RestoreSummary::default();
        if sessions.is_empty() && !fallback_sessions.is_empty() {
            summary.fallback_sessions = fallback_sessions.len();
            sessions.extend(fallback_sessions.clone());
        }

        let unique = deduplicate_sessions(std::mem::take(sessions), &mut summary);

        let activity_cutoff =
            matches!(mode, RestoreMode::Startup)
                .then(|| {
                    unique.iter().map(|s| s.last_seen_at).max().map(|t_max| {
                        t_max - chrono::Duration::seconds(LIVENESS_CLUSTER_WINDOW_SECS)
                    })
                })
                .flatten();

        let mut kept = Vec::new();

        for mut session in unique {
            if session_is_alive(&session) {
                session.mark_active();
                kept.push(session);
                continue;
            }

            if !session.directory.is_dir() {
                summary.pruned_missing_dirs += 1;
                continue;
            }

            session.mark_recoverable();

            if let Some(cutoff) = activity_cutoff
                && session.last_seen_at < cutoff
            {
                summary.preserved_recoverable += 1;
                kept.push(session);
                continue;
            }

            if matches!(mode, RestoreMode::Startup) && !transcript_present(&session) {
                summary.preserved_recoverable += 1;
                kept.push(session);
                continue;
            }

            if adapter.is_none() {
                let kind = configured_terminal()?;
                adapter = Some(adapter_for(kind));
            }

            let command = resume_command(&session);
            match adapter
                .as_ref()
                .unwrap()
                .open_tab(&session.directory, &command)
            {
                Ok(()) => match session.tool {
                    Tool::Claude => summary.restored_claude += 1,
                    Tool::Codex => summary.restored_codex += 1,
                },
                Err(error) => {
                    summary.failed += 1;
                    summary.errors.push(format!(
                        "failed to restore {} in {}: {error:#}",
                        session.session_id,
                        session.directory.display()
                    ));
                    kept.push(session);
                }
            }

            thread::sleep(Duration::from_millis(350));
        }

        *sessions = kept;
        Ok(summary)
    })
}

pub fn monitor_once() -> Result<MonitorSummary> {
    let _ = sessions::repair_if_corrupt(&paths::sessions_file()?)?;
    sessions::with_sessions_mut(&paths::sessions_file()?, |sessions| {
        let mut summary = MonitorSummary::default();
        for session in sessions.iter_mut() {
            if session_is_alive(session) {
                session.mark_active();
                continue;
            }

            if session.state == SessionState::Active
                && session.shell_pid.is_some()
                && !session_shell_is_alive(session)
            {
                continue;
            }

            if session.state == SessionState::Active {
                session.mark_recoverable();
                summary.marked_recoverable += 1;
            }
        }

        let before = sessions.len();
        sessions.retain(|session| {
            let closed = session.state == SessionState::Active
                && session.shell_pid.is_some()
                && !session_shell_is_alive(session)
                && !session_is_alive(session);
            !closed
        });
        summary.removed_closed = before - sessions.len();

        let before = sessions.len();
        sessions.retain(|session| !session.recoverable_expired());
        summary.pruned_expired = before - sessions.len();
        Ok(summary)
    })
}

pub fn running_daemon_pid() -> Result<Option<i32>> {
    let pid_path = paths::daemon_pid()?;
    if !pid_path.exists() {
        return Ok(None);
    }

    let contents = fs::read_to_string(&pid_path)
        .with_context(|| format!("failed to read {}", pid_path.display()))?;
    let Ok(pid) = contents.trim().parse::<i32>() else {
        return Ok(None);
    };

    if !process::pid_is_alive(pid) {
        return Ok(None);
    }

    let command = process::process_command(pid).unwrap_or_default();
    if command.contains("session-guard") {
        Ok(Some(pid))
    } else {
        Ok(None)
    }
}

pub fn log_line(message: &str) -> Result<()> {
    let log_path = paths::daemon_log()?;
    if let Some(parent) = log_path.parent() {
        fs::create_dir_all(parent)?;
    }

    let timestamp = chrono::Utc::now().to_rfc3339();
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("failed to open {}", log_path.display()))?;
    writeln!(file, "[{timestamp}] {message}")?;
    Ok(())
}

fn write_pid_file() -> Result<()> {
    fs::write(paths::daemon_pid()?, format!("{}\n", std::process::id()))
        .context("failed to write daemon pid file")
}

fn remove_pid_file_if_current() -> Result<()> {
    let pid_path = paths::daemon_pid()?;
    if !pid_path.exists() {
        return Ok(());
    }

    let contents = fs::read_to_string(&pid_path)?;
    if contents.trim() == std::process::id().to_string() {
        fs::remove_file(pid_path)?;
    }

    Ok(())
}

fn configured_terminal() -> Result<TerminalKind> {
    let path = paths::terminal_file()?;
    let contents =
        fs::read_to_string(&path).with_context(|| format!("failed to read {}", path.display()))?;
    contents.parse()
}

fn transcript_present(session: &SessionRecord) -> bool {
    match &session.transcript_path {
        Some(path) => path.exists(),
        None => true,
    }
}

fn session_is_alive(session: &SessionRecord) -> bool {
    session
        .pid
        .map(|pid| process::pid_is_alive(pid) && process::pid_is_tool(pid, session.tool))
        .unwrap_or(false)
}

fn session_shell_is_alive(session: &SessionRecord) -> bool {
    session
        .shell_pid
        .map(process::pid_is_alive)
        .unwrap_or(false)
}

fn deduplicate_sessions(
    sessions: Vec<SessionRecord>,
    summary: &mut RestoreSummary,
) -> Vec<SessionRecord> {
    let mut ids = HashSet::new();
    let mut by_id: HashMap<String, SessionRecord> = HashMap::new();

    for session in sessions {
        let duplicate = !ids.insert(session.session_id.clone());
        if duplicate {
            summary.pruned_duplicates += 1;
        }

        by_id
            .entry(session.session_id.clone())
            .and_modify(|existing| {
                if should_replace(existing, &session) {
                    *existing = session.clone();
                }
            })
            .or_insert(session);
    }

    by_id.into_values().collect()
}

fn should_replace(existing: &SessionRecord, candidate: &SessionRecord) -> bool {
    let existing_alive = session_is_alive(existing);
    let candidate_alive = session_is_alive(candidate);
    if candidate_alive != existing_alive {
        return candidate_alive;
    }

    if candidate.state != existing.state {
        return candidate.state == SessionState::Active;
    }

    candidate.last_seen_at > existing.last_seen_at
}

fn resume_command(session: &SessionRecord) -> String {
    let session_id = shell_quote(&session.session_id);
    match session.tool {
        Tool::Claude => format!("claude --resume {session_id}"),
        Tool::Codex => format!("codex resume {session_id}"),
    }
}
