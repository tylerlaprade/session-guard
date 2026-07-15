use crate::adapters::{adapter_for, shell_quote};
use crate::cargo_targets;
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

const CARGO_TARGET_CLEANUP_MONITOR_CYCLES: u16 = 60;

#[derive(Debug, Default)]
pub struct RestoreSummary {
    pub restored_claude: usize,
    pub restored_codex: usize,
    pub restored_grok: usize,
    pub pruned_missing_dirs: usize,
    pub pruned_duplicates: usize,
    pub fallback_sessions: usize,
    pub failed: usize,
    pub errors: Vec<String>,
}

impl RestoreSummary {
    pub fn restored_total(&self) -> usize {
        self.restored_claude + self.restored_codex + self.restored_grok
    }

    pub fn message(&self) -> String {
        let mut message = format!(
            "Restored {} sessions ({} Claude Code, {} Codex, {} Grok). Pruned {} (directory gone).",
            self.restored_total(),
            self.restored_claude,
            self.restored_codex,
            self.restored_grok,
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
            if summary.claude_scratch_targets > 0 {
                log_line(&format!(
                    "cleanup removed {} inactive Claude scratch Cargo targets",
                    summary.claude_scratch_targets
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
// - Startup restore: only the crash cluster — last_seen within 2 minutes of
//   the newest last_seen already in the file. That reopens sessions that died
//   with the previous daemon epoch, without reopening hours-old intentional
//   closes when the daemon is merely restarted for an upgrade.
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

    // Phase 1: decide who to restore under the sessions lock (no AppleScript).
    let (mut summary, alive_kept, to_open) =
        sessions::with_sessions_mut(&path, |sessions| {
            let mut summary = RestoreSummary::default();
            if sessions.is_empty() && !fallback_sessions.is_empty() {
                summary.fallback_sessions = fallback_sessions.len();
                sessions.extend(fallback_sessions.clone());
            }

            let unique = deduplicate_sessions(std::mem::take(sessions), &mut summary);

            // Crash cluster uses on-disk last_seen only (before mark_active).
            let activity_cutoff = matches!(mode, RestoreMode::Startup)
                .then(|| {
                    unique.iter().map(|s| s.last_seen_at).max().map(|t_max| {
                        t_max - chrono::Duration::seconds(LIVENESS_CLUSTER_WINDOW_SECS)
                    })
                })
                .flatten();

            let mut alive_kept = Vec::new();
            let mut to_open = Vec::new();

            for mut session in unique {
                if session_is_alive(&session) {
                    session.mark_active();
                    alive_kept.push(session);
                    continue;
                }

                if !session.directory.is_dir() {
                    summary.pruned_missing_dirs += 1;
                    continue;
                }

                session.mark_recoverable();

                // Shell still up ⇒ Ghostty/terminal tab still exists. Opening
                // another tab duplicates work that is already on screen.
                if session_shell_is_alive(&session) {
                    alive_kept.push(session);
                    continue;
                }

                if recently_restored(&session, now) {
                    alive_kept.push(session);
                    continue;
                }

                if let Some(cutoff) = activity_cutoff
                    && session.last_seen_at < cutoff
                {
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
            .chain(opened.into_iter())
            .chain(sessions.drain(..))
        {
            by_id
                .entry(session.session_id.clone())
                .and_modify(|existing| {
                    if should_replace(existing, &session) {
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
    sessions::with_sessions_mut(&paths::sessions_file()?, |sessions| {
        let mut summary = MonitorSummary::default();
        for session in sessions.iter_mut() {
            if session_is_alive(session) {
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

fn session_is_alive(session: &SessionRecord) -> bool {
    let tool_alive = session
        .pid
        .map(|pid| process::pid_is_alive(pid) && process::pid_is_tool(pid, session.tool))
        .unwrap_or(false);
    if !tool_alive {
        return false;
    }

    // Hook-tracked sessions record a shell PID for the Ghostty/terminal tab.
    // A forked/headless tool can outlive that tab (Claude remote workers under
    // launchd). Requiring the shell prevents "still alive" from blocking
    // restore of a tab the user no longer has.
    if session.shell_pid.is_some() {
        return session_shell_is_alive(session);
    }

    true
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
        Tool::Grok => format!("grok --resume {session_id}"),
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
            pruned_missing_dirs: 0,
            pruned_duplicates: 1,
            fallback_sessions: 0,
            failed: 0,
            errors: vec![],
        };
        let message = summary.message();
        assert!(message.contains("Restored 4 sessions"));
        assert!(message.contains("Pruned 1 duplicate"));
    }
}
