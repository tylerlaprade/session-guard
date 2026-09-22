use crate::adapters::{adapter_for, shell_quote};
use crate::cargo_targets;
use crate::last_sessions::{self, LastSessionRecord};
use crate::paths;
use crate::process::{self, ProcessSnapshot};
use crate::scan;
use crate::sessions::{self, SessionRecord, SessionState};
use crate::transcripts;
use crate::{TerminalKind, Tool};
use anyhow::{Context, Result};
use fs2::FileExt;
use signal_hook::consts::signal::{SIGINT, SIGTERM, SIGUSR1, SIGUSR2};
use signal_hook::flag;
use std::collections::BTreeMap;
use std::collections::{HashMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

const CARGO_TARGET_CLEANUP_MONITOR_CYCLES: u16 = 60;

#[derive(Debug, Default)]
pub struct RestoreSummary {
    /// Restores per harness, keyed by tool. Harness-agnostic so a new harness
    /// counts itself without a new field.
    pub restored: BTreeMap<Tool, usize>,
    pub pruned_missing_dirs: usize,
    pub pruned_duplicates: usize,
    pub fallback_sessions: usize,
    pub retired: usize,
    pub failed: usize,
    pub errors: Vec<String>,
}

impl RestoreSummary {
    pub fn restored_total(&self) -> usize {
        self.restored.values().sum()
    }

    pub fn record_restore(&mut self, tool: Tool) {
        *self.restored.entry(tool).or_default() += 1;
    }

    pub fn message(&self) -> String {
        use std::fmt::Write as _;
        let per_tool: Vec<String> = Tool::all()
            .map(|tool| {
                format!(
                    "{} {}",
                    self.restored.get(&tool).copied().unwrap_or_default(),
                    tool.spec().display_name
                )
            })
            .collect();
        let mut message = format!(
            "Restored {} sessions ({}). Pruned {} (directory gone).",
            self.restored_total(),
            per_tool.join(", "),
            self.pruned_missing_dirs,
        );
        if self.pruned_duplicates > 0 {
            let _ = write!(
                message,
                " Pruned {} duplicate entries.",
                self.pruned_duplicates
            );
        }
        if self.fallback_sessions > 0 {
            let _ = write!(
                message,
                " Added {} transcript fallback sessions.",
                self.fallback_sessions
            );
        }
        if self.retired > 0 {
            let _ = write!(message, " Retired {} ended sessions.", self.retired);
        }
        if self.failed > 0 {
            let _ = write!(message, " Failed to restore {} sessions.", self.failed);
        }
        message
    }
}

#[derive(Debug, Default)]
pub struct MonitorSummary {
    pub marked_recoverable: usize,
    pub pruned_expired: usize,
}

#[derive(Debug, Default)]
pub struct SettleSummary {
    pub retired: usize,
    pub marked_recoverable: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestoreMode {
    Startup,
    Manual,
    All,
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

    let shutdown = Arc::new(AtomicBool::new(false));
    let restore_requested = Arc::new(AtomicBool::new(false));
    let restore_all_requested = Arc::new(AtomicBool::new(false));
    flag::register(SIGTERM, Arc::clone(&shutdown))?;
    flag::register(SIGINT, Arc::clone(&shutdown))?;
    flag::register(SIGUSR1, Arc::clone(&restore_requested))?;
    flag::register(SIGUSR2, Arc::clone(&restore_all_requested))?;

    let summary = restore_once(RestoreMode::Startup)?;
    log_line(&summary.message())?;
    for error in &summary.errors {
        log_line(error)?;
    }

    let added = reconcile_from_scan()?;
    if added > 0 {
        log_line(&format!("scan added {added} sessions"))?;
    }
    // First observation of this epoch; failures (e.g. disk full) must not
    // stop monitoring, and the registry writes fail loudly on their own.
    let _ = write_daemon_heartbeat();
    run_cargo_target_cleanup()?;

    let mut seconds_until_monitor = 60;
    let mut seconds_until_settle = SESSION_END_SETTLE_INTERVAL_SECS;
    let mut terminal_watch = TerminalWatch::default();
    let mut monitor_cycles_until_cleanup = CARGO_TARGET_CLEANUP_MONITOR_CYCLES;
    while !shutdown.load(Ordering::Relaxed) {
        thread::sleep(Duration::from_secs(1));

        if restore_requested.swap(false, Ordering::Relaxed) {
            let summary = restore_once(RestoreMode::Manual)?;
            log_line(&format!("manual restore: {}", summary.message()))?;
            for error in &summary.errors {
                log_line(error)?;
            }
        }
        if restore_all_requested.swap(false, Ordering::Relaxed) {
            let summary = restore_once(RestoreMode::All)?;
            log_line(&format!("manual restore --all: {}", summary.message()))?;
            for error in &summary.errors {
                log_line(error)?;
            }
        }

        seconds_until_settle -= 1;
        if seconds_until_settle == 0 {
            seconds_until_settle = SESSION_END_SETTLE_INTERVAL_SECS;
            if let Ok(processes) = ProcessSnapshot::capture() {
                let summary = settle_endings(&processes)?;
                if summary.retired > 0 || summary.marked_recoverable > 0 {
                    log_line(&format!(
                        "settled {} ended sessions: {} retired, {} recoverable",
                        summary.retired + summary.marked_recoverable,
                        summary.retired,
                        summary.marked_recoverable
                    ))?;
                }
                if terminal_returned_with_victims(&mut terminal_watch, &processes)? {
                    let summary = restore_once(RestoreMode::Manual)?;
                    log_line(&format!("terminal returned: {}", summary.message()))?;
                    for error in &summary.errors {
                        log_line(error)?;
                    }
                }
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
            if summary.pruned_expired > 0 {
                log_line(&format!(
                    "monitor pruned {} expired recoverable sessions",
                    summary.pruned_expired
                ))?;
            }

            monitor_cycles_until_cleanup -= 1;
            if monitor_cycles_until_cleanup == 0 {
                run_cargo_target_cleanup()?;
                monitor_cycles_until_cleanup = CARGO_TARGET_CLEANUP_MONITOR_CYCLES;
            }
            seconds_until_monitor = 60;
        }
    }

    log_line("daemon stopped")?;
    remove_pid_file_if_current()?;
    Ok(())
}

fn run_cargo_target_cleanup() -> Result<()> {
    match cargo_targets::cleanup_once() {
        Ok(summary) => {
            if summary.owned_targets > 0 {
                log_line(&format!(
                    "cleanup removed {} abandoned session-owned Cargo targets",
                    summary.owned_targets
                ))?;
            }
            for error in summary.errors {
                log_line(&error)?;
            }
        }
        Err(error) => log_line(&format!("Cargo target cleanup failed: {error:#}"))?,
    }
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
                existing
                    .pid_started_at
                    .clone_from(&scanned_session.pid_started_at);
                existing.mark_active();
            } else {
                sessions.push(scanned_session);
                added += 1;
            }
        }
        Ok(added)
    })
}

const RESTORE_RECEIPT_TIMEOUT: Duration = Duration::from_secs(15);
// A terminal quitting gracefully closes tabs one by one and can still be
// running when a tab's SessionEnd fires; a crashed one is gone before the hooks
// run and may already be back (2026-09-03: Ghostty died at :51, the hooks
// fired at :52, its relaunch started at :55). The aftermath is judged only
// after this grace, and only a terminal older than the shell counts as its
// owner.
const SESSION_END_GRACE_SECS: i64 = 10;
const SESSION_END_SETTLE_INTERVAL_SECS: u32 = 5;
// AppleScript into a terminal that is still launching fails or opens ghost
// tabs, so a returned terminal is given this long before its tabs come back.
const TERMINAL_RELAUNCH_SETTLE_SECS: i64 = 5;

pub fn restore_once(mode: RestoreMode) -> Result<RestoreSummary> {
    let restore_lock = crate::commands::restore::lock()?;
    restore_lock.lock_exclusive()?;
    let path = paths::sessions_file()?;
    let _ = sessions::repair_if_corrupt(&path)?;
    let fallback_sessions = if sessions::read_sessions(&path)?.is_empty() {
        transcripts::discover_recent_sessions().unwrap_or_default()
    } else {
        Vec::new()
    };
    // The previous epoch's final observation, read before this epoch starts
    // writing its own heartbeats.
    let heartbeat = match mode {
        RestoreMode::Startup => read_daemon_heartbeat(),
        RestoreMode::Manual | RestoreMode::All => None,
    };
    let rebooted = heartbeat
        .as_ref()
        .is_some_and(|value| value.boot_changed(process::boot_identifier().as_deref()));
    // One ps pass for every liveness check below: per-PID ps calls under the
    // exclusive sessions lock stall hook-driven register/deregister (codex
    // kills its SessionEnd hook after one second).
    let processes = ProcessSnapshot::capture()?;
    let terminal = configured_terminal().ok();
    let last_sessions_path = paths::last_sessions_file()?;

    // Phase 1: decide who to restore under the sessions lock (no AppleScript).
    let (mut summary, to_open) = sessions::with_sessions_mut(&path, |sessions| {
        let mut summary = RestoreSummary::default();
        if sessions.is_empty() && !fallback_sessions.is_empty() {
            summary.fallback_sessions = fallback_sessions.len();
            sessions.extend(fallback_sessions.clone());
        }

        let unique = deduplicate_sessions(std::mem::take(sessions), &mut summary, &processes);

        let mut alive_kept = Vec::new();
        let mut to_open = Vec::new();

        for mut session in unique {
            let was_recoverable = session.state == SessionState::Recoverable;

            if session.state == SessionState::Ending
                && ending_verdict(
                    &session,
                    &processes,
                    terminal.map(TerminalKind::process_name),
                ) == EndingVerdict::Retire
            {
                remember(&last_sessions_path, &session)?;
                summary.retired += 1;
                continue;
            }

            if session_is_alive(&session, &processes) {
                session.mark_active();
                alive_kept.push(session);
                continue;
            }

            if scan::is_unused_spare(session.tool, &session.session_id) {
                log_line(&format!(
                    "skipped unused spare: {} {}",
                    session.tool, session.session_id
                ))?;
                alive_kept.push(session);
                continue;
            }

            if !session.directory.is_dir() {
                summary.pruned_missing_dirs += 1;
                continue;
            }

            session.mark_recoverable();

            if mode == RestoreMode::Startup && was_recoverable && !rebooted {
                alive_kept.push(session);
                continue;
            }

            // Shell still up ⇒ Ghostty/terminal tab still exists. Opening
            // another tab duplicates work that is already on screen.
            if session_shell_is_alive(&session, &processes) {
                alive_kept.push(session);
                continue;
            }

            if mode != RestoreMode::All && !session.restore_pending {
                alive_kept.push(session);
                continue;
            }

            to_open.push(session);
        }

        *sessions = alive_kept
            .into_iter()
            .chain(to_open.iter().cloned())
            .collect();
        Ok((summary, to_open))
    })?;

    // Phase 2: open tabs without holding the exclusive sessions lock.
    let mut adapter = None;
    for session in to_open {
        if adapter.is_none() {
            let kind = configured_terminal()?;
            adapter = Some(adapter_for(kind));
        }

        let command = format!(
            "{} launch --session-id {}",
            shell_quote(&std::env::current_exe()?.to_string_lossy()),
            shell_quote(&session.session_id)
        );
        match adapter
            .as_ref()
            .unwrap()
            .open_tab(&session.directory, &command)
            .and_then(|()| wait_for_restore_owner(&path, &session.session_id))
        {
            Ok(()) => {
                log_line(&format!(
                    "confirmed restored owner: {} {}",
                    session.tool, session.session_id
                ))?;
                summary.record_restore(session.tool);
            }
            Err(error) => {
                summary.failed += 1;
                summary.errors.push(format!(
                    "failed to restore {} in {}: {error:#}",
                    session.session_id,
                    session.directory.display()
                ));
            }
        }
        thread::sleep(Duration::from_millis(350));
    }

    Ok(summary)
}

fn wait_for_restore_owner(path: &std::path::Path, session_id: &str) -> Result<()> {
    let deadline = std::time::Instant::now() + RESTORE_RECEIPT_TIMEOUT;
    loop {
        let processes = ProcessSnapshot::capture()?;
        if sessions::read_sessions(path)?.iter().any(|session| {
            session.session_id == session_id
                && session.state == SessionState::Active
                && session_is_alive(session, &processes)
        }) {
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            anyhow::bail!("restored tab did not register a live owner for {session_id}");
        }
        thread::sleep(Duration::from_millis(100));
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EndingVerdict {
    Retire,
    Recoverable,
}

// A SessionEnd is the tool's word that the session id is done, not whether the
// user ended it or the terminal fell out from under it. The aftermath tells:
// the tool or its shell still running means an in-tab end (/exit, /clear,
// /resume switching away), and a dead tab under a terminal that is still up
// means the tab was closed on purpose (Cmd-W). A terminal that started after
// the shell cannot have owned the tab, so a quick relaunch does not count.
// Without a known terminal, err toward keeping: retirement is irreversible
// and a recoverable record expires on its own.
pub(crate) fn ending_verdict(
    session: &SessionRecord,
    processes: &ProcessSnapshot,
    terminal_executable: Option<&str>,
) -> EndingVerdict {
    if session_tool_is_alive(session, processes) || session_shell_is_alive(session, processes) {
        return EndingVerdict::Retire;
    }
    let terminal_outlived_the_tab = terminal_executable.is_some_and(|executable| {
        processes.instance_running_since(executable, session.shell_pid_started_at.as_deref())
    });
    if terminal_outlived_the_tab {
        EndingVerdict::Retire
    } else {
        EndingVerdict::Recoverable
    }
}

fn ending_settled(session: &SessionRecord, now: chrono::DateTime<chrono::Utc>) -> bool {
    session.state == SessionState::Ending
        && session
            .ending_at
            .is_some_and(|ending_at| (now - ending_at).num_seconds() >= SESSION_END_GRACE_SECS)
}

fn remember(last_sessions_path: &std::path::Path, session: &SessionRecord) -> Result<()> {
    if let Some(record) = LastSessionRecord::from_session(session) {
        last_sessions::remember(last_sessions_path, record)?;
    }
    Ok(())
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct TerminalWatch {
    observed_at: Option<chrono::NaiveDateTime>,
    current: Option<chrono::NaiveDateTime>,
    pending: Option<chrono::NaiveDateTime>,
}

impl TerminalWatch {
    fn relaunched(
        &mut self,
        current: Option<chrono::NaiveDateTime>,
        now: chrono::NaiveDateTime,
    ) -> Option<chrono::NaiveDateTime> {
        let Some(observed_at) = self.observed_at.replace(now) else {
            self.current = current;
            return None;
        };
        if self.current != current {
            self.current = current;
            self.pending = current.filter(|started| {
                started.and_utc().timestamp() >= observed_at.and_utc().timestamp()
            });
        }
        let started = self.pending?;
        if (now - started).num_seconds() < TERMINAL_RELAUNCH_SETTLE_SECS {
            return None;
        }
        self.pending.take()
    }
}

// Restore on relaunch only when tabs died with the previous instance; a stale
// pile alone must not reopen just because the terminal was reopened.
fn terminal_returned_with_victims(
    watch: &mut TerminalWatch,
    processes: &ProcessSnapshot,
) -> Result<bool> {
    let Ok(terminal) = configured_terminal() else {
        return Ok(false);
    };
    let now = chrono::Utc::now();
    let Some(started_at) = watch.relaunched(
        processes.instance_started_at(terminal.process_name()),
        now.naive_utc(),
    ) else {
        return Ok(false);
    };
    let has_victims = sessions::read_sessions(&paths::sessions_file()?)?
        .iter()
        .any(|session| needs_terminal_restore(session, processes));
    if has_victims {
        log_line(&format!(
            "terminal relaunch detected: {terminal}, started at {started_at} UTC"
        ))?;
    }
    Ok(has_victims)
}

pub(crate) fn needs_terminal_restore(session: &SessionRecord, processes: &ProcessSnapshot) -> bool {
    (session.restore_pending || session.state != SessionState::Recoverable)
        && !session_is_alive(session, processes)
        && !session_shell_is_alive(session, processes)
}

pub fn settle_endings(processes: &ProcessSnapshot) -> Result<SettleSummary> {
    let path = paths::sessions_file()?;
    let now = chrono::Utc::now();
    let _ = sessions::repair_if_corrupt(&path)?;
    if !sessions::read_sessions(&path)?
        .iter()
        .any(|session| ending_settled(session, now))
    {
        return Ok(SettleSummary::default());
    }
    let terminal = configured_terminal().ok();
    let last_sessions_path = paths::last_sessions_file()?;

    sessions::with_sessions_mut(&path, |sessions| {
        let mut summary = SettleSummary::default();
        let mut kept = Vec::with_capacity(sessions.len());
        for mut session in sessions.drain(..) {
            if !ending_settled(&session, now) {
                kept.push(session);
                continue;
            }
            match ending_verdict(
                &session,
                processes,
                terminal.map(TerminalKind::process_name),
            ) {
                EndingVerdict::Retire => {
                    remember(&last_sessions_path, &session)?;
                    summary.retired += 1;
                }
                EndingVerdict::Recoverable => {
                    session.mark_recoverable();
                    summary.marked_recoverable += 1;
                    kept.push(session);
                }
            }
        }
        *sessions = kept;
        Ok(summary)
    })
}

#[derive(serde::Serialize, serde::Deserialize)]
struct DaemonHeartbeat {
    timestamp: chrono::DateTime<chrono::Utc>,
    boot_id: Option<String>,
}

impl DaemonHeartbeat {
    fn boot_changed(&self, current: Option<&str>) -> bool {
        matches!((self.boot_id.as_deref(), current), (Some(previous), Some(current)) if previous != current)
    }
}

fn read_daemon_heartbeat() -> Option<DaemonHeartbeat> {
    let path = paths::daemon_heartbeat().ok()?;
    let contents = fs::read_to_string(path).ok()?;
    serde_json::from_str(&contents).ok().or_else(|| {
        chrono::DateTime::parse_from_rfc3339(contents.trim())
            .ok()
            .map(|timestamp| DaemonHeartbeat {
                timestamp: timestamp.with_timezone(&chrono::Utc),
                boot_id: None,
            })
    })
}

fn write_daemon_heartbeat() -> Result<()> {
    fs::write(
        paths::daemon_heartbeat()?,
        serde_json::to_vec(&DaemonHeartbeat {
            timestamp: chrono::Utc::now(),
            boot_id: process::boot_identifier(),
        })?,
    )
    .context("failed to write daemon heartbeat")
}

pub fn monitor_once() -> Result<MonitorSummary> {
    let _ = sessions::repair_if_corrupt(&paths::sessions_file()?)?;
    // Snapshot before taking the lock; a transiently failing ps skips the
    // tick rather than killing the daemon or marking everything dead.
    let Ok(processes) = ProcessSnapshot::capture() else {
        return Ok(MonitorSummary::default());
    };
    let summary = sessions::with_sessions_mut(&paths::sessions_file()?, |sessions| {
        let mut summary = MonitorSummary::default();
        for session in sessions.iter_mut() {
            // Ending records belong to settle_endings; a still-running tool
            // must not revive an id its own SessionEnd already closed.
            if session.state == SessionState::Ending {
                continue;
            }
            if session_is_alive(session, &processes) {
                session.mark_active();
                continue;
            }

            // Tool (and possibly shell) are dead. Jetsam and WindowServer
            // crashes kill both PIDs while this daemon often keeps running —
            // never treat that as an intentional close.
            if session.state == SessionState::Active {
                session.mark_recoverable();
                summary.marked_recoverable += 1;
            }
        }

        let before = sessions.len();
        sessions.retain(|session| !session.recoverable_expired());
        summary.pruned_expired = before - sessions.len();
        Ok(summary)
    })?;
    // Only after a real observation pass: a stale heartbeat must mean "the
    // monitor stopped watching here", never "ps kept failing".
    let _ = write_daemon_heartbeat();
    Ok(summary)
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

pub(crate) fn session_is_alive(session: &SessionRecord, processes: &ProcessSnapshot) -> bool {
    if !session_tool_is_alive(session, processes) {
        return false;
    }

    // Hook-tracked sessions record a shell PID for the Ghostty/terminal tab.
    // A forked/headless tool can outlive that tab (Claude remote workers under
    // launchd). Requiring the shell prevents "still alive" from blocking
    // restore of a tab the user no longer has.
    if session.shell_pid.is_some() {
        return session_shell_is_alive(session, processes);
    }

    true
}

// When the record carries the process start time, liveness means "that exact
// process": a recycled PID, or another same-named process on it, reads as
// dead. Records that predate identity capture fall back to name matching.
pub(crate) fn session_tool_is_alive(session: &SessionRecord, processes: &ProcessSnapshot) -> bool {
    match (session.pid, session.pid_started_at.as_deref()) {
        (Some(pid), Some(started_at)) => processes.identity_matches(pid, started_at),
        (Some(pid), None) => processes.is_alive(pid) && processes.is_tool(pid, session.tool),
        (None, _) => false,
    }
}

pub(crate) fn session_shell_is_alive(session: &SessionRecord, processes: &ProcessSnapshot) -> bool {
    match (session.shell_pid, session.shell_pid_started_at.as_deref()) {
        (Some(pid), Some(started_at)) => processes.identity_matches(pid, started_at),
        (Some(pid), None) => processes.is_alive(pid),
        (None, _) => false,
    }
}

fn deduplicate_sessions(
    sessions: Vec<SessionRecord>,
    summary: &mut RestoreSummary,
    processes: &ProcessSnapshot,
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
                if should_replace(existing, &session, processes) {
                    *existing = session.clone();
                }
            })
            .or_insert(session);
    }

    by_id.into_values().collect()
}

fn should_replace(
    existing: &SessionRecord,
    candidate: &SessionRecord,
    processes: &ProcessSnapshot,
) -> bool {
    let existing_alive = session_is_alive(existing, processes);
    let candidate_alive = session_is_alive(candidate, processes);
    if candidate_alive != existing_alive {
        return candidate_alive;
    }

    if candidate.state != existing.state {
        return candidate.state == SessionState::Active;
    }

    candidate.last_seen_at > existing.last_seen_at
}

pub(crate) fn resume_command(session: &SessionRecord) -> String {
    session
        .tool
        .spec()
        .resume
        .replace("{session_id}", &shell_quote(&session.session_id))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_return_detects_deaths_before_the_next_monitor_tick() {
        let processes = ProcessSnapshot::capture().unwrap();
        let mut session = sample(crate::tool("codex"), "idle");
        session.pid = None;
        session.shell_pid = None;
        session.last_seen_at = Utc::now() - Duration::days(3);
        assert!(needs_terminal_restore(&session, &processes));
        session.mark_recoverable();
        assert!(needs_terminal_restore(&session, &processes));
        session.restore_pending = false;
        assert!(!needs_terminal_restore(&session, &processes));
    }
    use crate::sessions::SessionRecord;
    use chrono::{Duration, Utc};
    use std::path::PathBuf;

    fn sample(tool: Tool, id: &str) -> SessionRecord {
        SessionRecord::new(
            tool,
            id.to_string(),
            None,
            None,
            PathBuf::from("/tmp/proj"),
            None,
            None,
            None,
        )
    }

    #[test]
    fn resume_commands_match_each_tool() {
        assert_eq!(
            resume_command(&sample(crate::tool("claude"), "abc")),
            "claude --resume 'abc'"
        );
        assert_eq!(
            resume_command(&sample(crate::tool("codex"), "def")),
            "codex resume 'def'"
        );
        assert_eq!(
            resume_command(&sample(crate::tool("grok"), "ghi")),
            "grok --resume 'ghi'"
        );
        assert_eq!(
            resume_command(&sample(crate::tool("opencode"), "ses_abc")),
            "opencode --session 'ses_abc'"
        );
    }

    #[test]
    fn tool_liveness_requires_matching_start_identity() {
        let pid = std::process::id() as i32;
        let processes = ProcessSnapshot::capture().unwrap();
        let mut session = sample(crate::tool("claude"), "abc");
        session.pid = Some(pid);

        // Same PID number, different process start time: a recycled PID.
        session.pid_started_at = Some("Wed Jan 1 00:00:00 2020".to_string());
        assert!(!session_tool_is_alive(&session, &processes));
        assert!(!session_is_alive(&session, &processes));

        session.pid_started_at = process::process_start_identity(pid).ok();
        assert!(session.pid_started_at.is_some());
        assert!(session_tool_is_alive(&session, &processes));
    }

    #[test]
    fn shell_liveness_requires_matching_start_identity() {
        let pid = std::process::id() as i32;
        let processes = ProcessSnapshot::capture().unwrap();
        let mut session = sample(crate::tool("claude"), "abc");
        session.shell_pid = Some(pid);

        session.shell_pid_started_at = Some("Wed Jan 1 00:00:00 2020".to_string());
        assert!(!session_shell_is_alive(&session, &processes));

        session.shell_pid_started_at = process::process_start_identity(pid).ok();
        assert!(session_shell_is_alive(&session, &processes));
    }

    #[test]
    fn a_reboot_is_distinct_from_a_daemon_restart() {
        let heartbeat = DaemonHeartbeat {
            timestamp: Utc::now(),
            boot_id: Some("previous-boot".to_string()),
        };
        assert!(heartbeat.boot_changed(Some("new-boot")));
        assert!(!heartbeat.boot_changed(Some("previous-boot")));
        assert!(!heartbeat.boot_changed(None));
        let legacy = DaemonHeartbeat {
            timestamp: Utc::now(),
            boot_id: None,
        };
        assert!(!legacy.boot_changed(Some("new-boot")));
    }

    fn own_executable() -> String {
        std::env::current_exe()
            .unwrap()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_ascii_lowercase()
    }

    fn ended_session(shell_pid: i32, shell_started_at: &str) -> SessionRecord {
        let mut session = sample(crate::tool("claude"), "abc");
        session.pid = Some(i32::MAX);
        session.pid_started_at = Some("Wed Jan 1 00:00:00 2020".to_string());
        session.shell_pid = Some(shell_pid);
        session.shell_pid_started_at = Some(shell_started_at.to_string());
        session.mark_ending();
        session
    }

    #[test]
    fn ending_verdict_retires_an_in_tab_end() {
        let pid = std::process::id() as i32;
        let processes = ProcessSnapshot::capture().unwrap();
        let session = ended_session(pid, &process::process_start_identity(pid).unwrap());
        assert_eq!(
            ending_verdict(&session, &processes, None),
            EndingVerdict::Retire
        );
    }

    #[test]
    fn ending_verdict_retires_a_tab_closed_under_a_living_terminal() {
        // The test binary stands in for the terminal: it predates a shell
        // "started" in 2100, so it owned that tab and is still up.
        let processes = ProcessSnapshot::capture().unwrap();
        let session = ended_session(i32::MAX, "Fri Jan 1 00:00:00 2100");
        assert_eq!(
            ending_verdict(&session, &processes, Some(&own_executable())),
            EndingVerdict::Retire
        );
    }

    #[test]
    fn ending_verdict_keeps_a_tab_whose_terminal_went_down() {
        let processes = ProcessSnapshot::capture().unwrap();
        let session = ended_session(i32::MAX, "Fri Jan 1 00:00:00 2100");
        assert_eq!(
            ending_verdict(&session, &processes, Some("no-such-terminal-xyz")),
            EndingVerdict::Recoverable
        );
        assert_eq!(
            ending_verdict(&session, &processes, None),
            EndingVerdict::Recoverable
        );
    }

    #[test]
    fn ending_verdict_ignores_a_terminal_relaunched_after_the_shell() {
        // Ghostty quit at 11:20:51 and was back by 11:21:14; the new instance
        // started after every dead shell and must not claim their tabs.
        let processes = ProcessSnapshot::capture().unwrap();
        let session = ended_session(i32::MAX, "Wed Jan 1 00:00:00 2020");
        assert_eq!(
            ending_verdict(&session, &processes, Some(&own_executable())),
            EndingVerdict::Recoverable
        );
    }

    #[test]
    fn ending_settles_only_after_the_grace_period() {
        let mut session = sample(crate::tool("claude"), "abc");
        assert!(!ending_settled(&session, Utc::now()));
        session.mark_ending();
        let ending_at = session.ending_at.unwrap();
        assert!(!ending_settled(
            &session,
            ending_at + Duration::seconds(SESSION_END_GRACE_SECS - 1)
        ));
        assert!(ending_settled(
            &session,
            ending_at + Duration::seconds(SESSION_END_GRACE_SECS)
        ));
    }

    #[test]
    fn terminal_watch_does_not_treat_an_existing_survivor_as_a_relaunch() {
        let now = Utc::now().naive_utc();
        let old = now - Duration::hours(2);
        let survivor = now - Duration::hours(1);
        let mut watch = TerminalWatch::default();
        assert_eq!(watch.relaunched(Some(old), now), None);
        assert_eq!(
            watch.relaunched(Some(survivor), now + Duration::seconds(5)),
            None
        );
    }

    #[test]
    fn terminal_watch_does_not_treat_a_missing_snapshot_as_a_relaunch() {
        let now = Utc::now().naive_utc();
        let old = now - Duration::hours(2);
        let mut watch = TerminalWatch::default();
        assert_eq!(watch.relaunched(Some(old), now), None);
        assert_eq!(watch.relaunched(None, now + Duration::seconds(5)), None);
        assert_eq!(
            watch.relaunched(Some(old), now + Duration::seconds(10)),
            None
        );
    }

    #[test]
    fn terminal_watch_reports_a_relaunch_once_the_new_instance_can_be_scripted() {
        let now = Utc::now().naive_utc();
        let old = now - Duration::hours(2);
        let fresh = now + Duration::seconds(1);
        let mut watch = TerminalWatch::default();

        assert_eq!(watch.relaunched(Some(old), now), None);
        assert_eq!(watch.relaunched(Some(old), now), None);
        assert_eq!(watch.relaunched(Some(fresh), fresh), None);
        let settled_at = fresh + Duration::seconds(TERMINAL_RELAUNCH_SETTLE_SECS);
        assert_eq!(watch.relaunched(Some(fresh), settled_at), Some(fresh));
        assert_eq!(watch.relaunched(Some(fresh), settled_at), None);
    }

    #[test]
    fn terminal_watch_treats_none_then_some_as_a_relaunch() {
        let now = Utc::now().naive_utc();
        let back = now + Duration::seconds(1);
        let mut watch = TerminalWatch::default();

        assert_eq!(watch.relaunched(Some(back - Duration::hours(1)), now), None);
        assert_eq!(watch.relaunched(None, now), None);
        assert_eq!(watch.relaunched(None, now), None);
        assert_eq!(
            watch.relaunched(Some(back), back + Duration::seconds(5)),
            Some(back)
        );
    }

    #[test]
    fn terminal_watch_ignores_a_daemon_that_starts_with_no_terminal() {
        let now = Utc::now().naive_utc();
        let mut watch = TerminalWatch::default();
        assert_eq!(watch.relaunched(None, now), None);
        assert_eq!(
            watch.relaunched(Some(now), now + Duration::seconds(5)),
            Some(now)
        );
    }

    #[test]
    fn restore_summary_message_lists_counts() {
        let mut restored = BTreeMap::new();
        restored.insert(crate::tool("claude"), 2);
        restored.insert(crate::tool("codex"), 1);
        restored.insert(crate::tool("grok"), 1);
        restored.insert(crate::tool("opencode"), 1);
        let summary = RestoreSummary {
            restored,
            pruned_missing_dirs: 0,
            pruned_duplicates: 1,
            fallback_sessions: 0,
            retired: 2,
            failed: 0,
            errors: vec![],
        };
        let message = summary.message();
        assert!(message.contains("Restored 5 sessions"));
        assert!(message.contains("1 OpenCode"));
        assert!(message.contains("Pruned 1 duplicate"));
        assert!(message.contains("Retired 2 ended sessions"));
    }
}
