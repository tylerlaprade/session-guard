use crate::adapters::{adapter_for, shell_quote};
use crate::cargo_targets;
use crate::paths;
use crate::process::{self, ProcessSnapshot};
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

const CARGO_TARGET_CLEANUP_MONITOR_CYCLES: u16 = 60;

#[derive(Debug, Default)]
pub struct RestoreSummary {
    pub restored_claude: usize,
    pub restored_codex: usize,
    pub restored_grok: usize,
    pub restored_opencode: usize,
    pub pruned_missing_dirs: usize,
    pub pruned_duplicates: usize,
    pub fallback_sessions: usize,
    pub failed: usize,
    pub errors: Vec<String>,
}

impl RestoreSummary {
    pub fn restored_total(&self) -> usize {
        self.restored_claude + self.restored_codex + self.restored_grok + self.restored_opencode
    }

    pub fn message(&self) -> String {
        let mut message = format!(
            "Restored {} sessions ({} Claude Code, {} Codex, {} Grok, {} OpenCode). Pruned {} (directory gone).",
            self.restored_total(),
            self.restored_claude,
            self.restored_codex,
            self.restored_grok,
            self.restored_opencode,
            self.pruned_missing_dirs,
        );
        if self.pruned_duplicates > 0 {
            message.push_str(&format!(
                " Pruned {} duplicate entries.",
                self.pruned_duplicates
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

    // Restore *before* scan. Scan calls mark_active() on still-running tools
    // (including headless Claude workers), which rewrites last_seen_at to
    // "now" and would push crash-window sessions outside any startup window.
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

    let added = reconcile_from_scan()?;
    if added > 0 {
        log_line(&format!("scan added {added} sessions"))?;
    }
    // First observation of this epoch; failures (e.g. disk full) must not
    // stop monitoring, and the registry writes fail loudly on their own.
    let _ = write_daemon_heartbeat();
    run_cargo_target_cleanup()?;

    let mut seconds_until_monitor = 60;
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
                existing.pid_started_at = scanned_session.pid_started_at.clone();
                existing.mark_active();
            } else {
                sessions.push(scanned_session);
                added += 1;
            }
        }
        Ok(added)
    })
}

// Dual PID death is ambiguous (intentional close vs jetsam/WindowServer), so
// monitor never deletes on PID death alone — only deregister or 7-day expiry.
//
// Restore policy:
// - Never open a new tab if the recorded shell is still alive (tab still open).
// - Manual restore: every both-dead recoverable session.
// - Startup restore: only unobserved deaths in the crash cluster — sessions
//   still marked active on disk (no epoch's monitor saw them die) whose
//   last_seen falls within 2 minutes of the newest last_seen already in the
//   file, OR within 2 minutes of the previous epoch's final monitor
//   heartbeat. The second cluster exists because jetsam usually kills the
//   daemon before the tools: hook-driven Stop heartbeats keep advancing the
//   newest last_seen for busy sessions after the monitor dies, while idle
//   sessions stay frozen at the monitor's last tick. Anchoring only on the
//   newest last_seen then misreads those idle crash victims as an older
//   recoverable pile (the 2026-08-25 incident: only the two Grok tabs still
//   chatting near the end were reopened). Together the clusters still avoid
//   reopening intentional closes when the daemon is merely restarted for an
//   upgrade.
// - Restore runs before process scan so survivors cannot rewrite heartbeats.
// - After open_tab succeeds, keep the record (source="restored") with a
//   cooldown so a second restore does not spam duplicate tabs.

const LIVENESS_CLUSTER_WINDOW_SECS: i64 = 120;
const RESTORE_COOLDOWN_SECS: i64 = 30 * 60;
const RESTORED_SOURCE: &str = "restored";

pub fn restore_once(mode: RestoreMode) -> Result<RestoreSummary> {
    let path = paths::sessions_file()?;
    let _ = sessions::repair_if_corrupt(&path)?;
    let fallback_sessions = transcripts::discover_recent_sessions().unwrap_or_default();
    let now = chrono::Utc::now();
    // The previous epoch's final observation, read before this epoch starts
    // writing its own heartbeats.
    let previous_heartbeat = match mode {
        RestoreMode::Startup => read_daemon_heartbeat(),
        RestoreMode::Manual => None,
    };
    // One ps pass for every liveness check below: per-PID ps calls under the
    // exclusive sessions lock stall hook-driven register/deregister (codex
    // kills its SessionEnd hook after one second).
    let processes = ProcessSnapshot::capture()?;

    // Phase 1: decide who to restore under the sessions lock (no AppleScript).
    let (mut summary, alive_kept, to_open) = sessions::with_sessions_mut(&path, |sessions| {
        let mut summary = RestoreSummary::default();
        if sessions.is_empty() && !fallback_sessions.is_empty() {
            summary.fallback_sessions = fallback_sessions.len();
            sessions.extend(fallback_sessions.clone());
        }

        let unique = deduplicate_sessions(std::mem::take(sessions), &mut summary, &processes);

        // Crash cluster uses on-disk last_seen only (before mark_active).
        let activity_cutoff =
            matches!(mode, RestoreMode::Startup)
                .then(|| {
                    unique.iter().map(|s| s.last_seen_at).max().map(|t_max| {
                        t_max - chrono::Duration::seconds(LIVENESS_CLUSTER_WINDOW_SECS)
                    })
                })
                .flatten();

        let mut alive_kept = Vec::new();
        let mut to_open = Vec::new();

        for mut session in unique {
            let was_recoverable = session.state == SessionState::Recoverable;
            if session_is_alive(&session, &processes) {
                session.mark_active();
                alive_kept.push(session);
                continue;
            }

            if !session.directory.is_dir() {
                summary.pruned_missing_dirs += 1;
                continue;
            }

            session.mark_recoverable();

            // Already recoverable on disk ⇒ a previous epoch's monitor saw
            // this session die — an observed close, not one that fell with
            // the epoch. A daemon upgrade/restart must not resurrect it.
            // Manual restore still offers it.
            if matches!(mode, RestoreMode::Startup) && was_recoverable {
                alive_kept.push(session);
                continue;
            }

            // Shell still up ⇒ Ghostty/terminal tab still exists. Opening
            // another tab duplicates work that is already on screen.
            if session_shell_is_alive(&session, &processes) {
                alive_kept.push(session);
                continue;
            }

            if recently_restored(&session, now) {
                alive_kept.push(session);
                continue;
            }

            if outside_startup_clusters(session.last_seen_at, activity_cutoff, previous_heartbeat) {
                // Older recoverable pile (earlier intentional closes, etc.)
                alive_kept.push(session);
                continue;
            }

            to_open.push(session);
        }

        // Hold only non-opening sessions while tabs are opened outside the lock.
        *sessions = alive_kept.clone();
        Ok((summary, alive_kept, to_open))
    })?;

    // Phase 2: open tabs without holding the exclusive sessions lock.
    let mut adapter = None;
    let mut opened = Vec::new();
    for mut session in to_open {
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
            Ok(()) => {
                match session.tool {
                    Tool::Claude => summary.restored_claude += 1,
                    Tool::Codex => summary.restored_codex += 1,
                    Tool::Grok => summary.restored_grok += 1,
                    Tool::Opencode => summary.restored_opencode += 1,
                }
                session.source = Some(RESTORED_SOURCE.to_string());
                session.last_seen_at = now;
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
        opened.push(session);
        thread::sleep(Duration::from_millis(350));
    }

    // Phase 3: write restored/failed records back (merge with anything hooks added).
    sessions::with_sessions_mut(&path, |sessions| {
        let mut by_id: HashMap<String, SessionRecord> = HashMap::new();
        for session in alive_kept
            .into_iter()
            .chain(opened)
            .chain(sessions.drain(..))
        {
            by_id
                .entry(session.session_id.clone())
                .and_modify(|existing| {
                    if should_replace(existing, &session, &processes) {
                        *existing = session.clone();
                    }
                })
                .or_insert(session);
        }
        *sessions = by_id.into_values().collect();
        Ok(())
    })?;

    Ok(summary)
}

// A dead active-on-disk session at startup is a crash victim when it belongs
// to either cluster: near the newest on-disk last_seen (sessions whose hooks
// kept heartbeating up to the catastrophe), or near the previous epoch's
// final monitor heartbeat (idle sessions the dead daemon was refreshing).
fn outside_startup_clusters(
    last_seen: chrono::DateTime<chrono::Utc>,
    activity_cutoff: Option<chrono::DateTime<chrono::Utc>>,
    previous_heartbeat: Option<chrono::DateTime<chrono::Utc>>,
) -> bool {
    let Some(cutoff) = activity_cutoff else {
        return false;
    };
    if last_seen >= cutoff {
        return false;
    }
    let Some(heartbeat) = previous_heartbeat else {
        return true;
    };
    (heartbeat - last_seen).num_seconds().abs() > LIVENESS_CLUSTER_WINDOW_SECS
}

fn read_daemon_heartbeat() -> Option<chrono::DateTime<chrono::Utc>> {
    let path = paths::daemon_heartbeat().ok()?;
    let contents = fs::read_to_string(path).ok()?;
    chrono::DateTime::parse_from_rfc3339(contents.trim())
        .ok()
        .map(|timestamp| timestamp.with_timezone(&chrono::Utc))
}

fn write_daemon_heartbeat() -> Result<()> {
    fs::write(
        paths::daemon_heartbeat()?,
        format!("{}\n", chrono::Utc::now().to_rfc3339()),
    )
    .context("failed to write daemon heartbeat")
}

fn recently_restored(session: &SessionRecord, now: chrono::DateTime<chrono::Utc>) -> bool {
    if session.source.as_deref() != Some(RESTORED_SOURCE) {
        return false;
    }
    now.signed_duration_since(session.last_seen_at)
        .num_seconds()
        < RESTORE_COOLDOWN_SECS
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

fn session_is_alive(session: &SessionRecord, processes: &ProcessSnapshot) -> bool {
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

fn resume_command(session: &SessionRecord) -> String {
    let session_id = shell_quote(&session.session_id);
    match session.tool {
        Tool::Claude => format!("claude --resume {session_id}"),
        Tool::Codex => format!("codex resume {session_id}"),
        Tool::Grok => format!("grok --resume {session_id}"),
        Tool::Opencode => format!("opencode --session {session_id}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
            resume_command(&sample(Tool::Claude, "abc")),
            "claude --resume 'abc'"
        );
        assert_eq!(
            resume_command(&sample(Tool::Codex, "def")),
            "codex resume 'def'"
        );
        assert_eq!(
            resume_command(&sample(Tool::Grok, "ghi")),
            "grok --resume 'ghi'"
        );
        assert_eq!(
            resume_command(&sample(Tool::Opencode, "ses_abc")),
            "opencode --session 'ses_abc'"
        );
    }

    #[test]
    fn tool_liveness_requires_matching_start_identity() {
        let pid = std::process::id() as i32;
        let processes = ProcessSnapshot::capture().unwrap();
        let mut session = sample(Tool::Claude, "abc");
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
        let mut session = sample(Tool::Claude, "abc");
        session.shell_pid = Some(pid);

        session.shell_pid_started_at = Some("Wed Jan 1 00:00:00 2020".to_string());
        assert!(!session_shell_is_alive(&session, &processes));

        session.shell_pid_started_at = process::process_start_identity(pid).ok();
        assert!(session_shell_is_alive(&session, &processes));
    }

    #[test]
    fn startup_clusters_admit_idle_sessions_frozen_at_the_dead_monitors_tick() {
        // The 2026-08-25 incident: the daemon died at 11:24:14 while Grok Stop
        // hooks kept heartbeating until 11:52. Idle Claude/Codex sessions froze
        // at the monitor's last tick and must still count as crash victims.
        let heartbeat = Utc::now();
        let newest = heartbeat + Duration::minutes(28);
        let cutoff = Some(newest - Duration::seconds(LIVENESS_CLUSTER_WINDOW_SECS));

        // Idle session refreshed by the monitor's final tick.
        assert!(!outside_startup_clusters(
            heartbeat,
            cutoff,
            Some(heartbeat)
        ));
        // Busy session that heartbeated up to the catastrophe.
        assert!(!outside_startup_clusters(newest, cutoff, Some(heartbeat)));
        // Genuinely older pile stays excluded.
        let old = heartbeat - Duration::hours(3);
        assert!(outside_startup_clusters(old, cutoff, Some(heartbeat)));
        // A session that heartbeated shortly after the monitor died but well
        // before the catastrophe is ambiguous; keep excluding it.
        let between = heartbeat + Duration::minutes(10);
        assert!(outside_startup_clusters(between, cutoff, Some(heartbeat)));
    }

    #[test]
    fn startup_clusters_without_heartbeat_fall_back_to_newest_activity() {
        let newest = Utc::now();
        let cutoff = Some(newest - Duration::seconds(LIVENESS_CLUSTER_WINDOW_SECS));
        assert!(!outside_startup_clusters(newest, cutoff, None));
        assert!(outside_startup_clusters(
            newest - Duration::minutes(30),
            cutoff,
            None
        ));
    }

    #[test]
    fn manual_restore_has_no_cluster_gate() {
        let ancient = Utc::now() - Duration::days(6);
        assert!(!outside_startup_clusters(ancient, None, None));
    }

    #[test]
    fn recently_restored_respects_source_and_cooldown() {
        let now = Utc::now();
        let mut session = sample(Tool::Claude, "abc");
        assert!(!recently_restored(&session, now));

        session.source = Some(RESTORED_SOURCE.to_string());
        session.last_seen_at = now - Duration::seconds(10);
        assert!(recently_restored(&session, now));

        session.last_seen_at = now - Duration::seconds(RESTORE_COOLDOWN_SECS + 1);
        assert!(!recently_restored(&session, now));
    }

    #[test]
    fn restore_summary_message_lists_counts() {
        let summary = RestoreSummary {
            restored_claude: 2,
            restored_codex: 1,
            restored_grok: 1,
            restored_opencode: 1,
            pruned_missing_dirs: 0,
            pruned_duplicates: 1,
            fallback_sessions: 0,
            failed: 0,
            errors: vec![],
        };
        let message = summary.message();
        assert!(message.contains("Restored 5 sessions"));
        assert!(message.contains("1 OpenCode"));
        assert!(message.contains("Pruned 1 duplicate"));
    }
}
