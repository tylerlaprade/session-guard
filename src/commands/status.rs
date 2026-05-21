use crate::commands::daemon;
use crate::paths;
use crate::process;
use crate::sessions::{self, SessionState};
use anyhow::Result;
use chrono::Utc;

const GREEN: &str = "\x1b[32m";
const RED: &str = "\x1b[31m";
const RESET: &str = "\x1b[0m";

pub fn run() -> Result<()> {
    match daemon::running_daemon_pid()? {
        Some(pid) => println!("Daemon: {GREEN}running{RESET} (pid {pid})"),
        None => println!("Daemon: {RED}stopped{RESET}"),
    }

    sessions::repair_if_corrupt(&paths::sessions_file()?)?;
    let sessions = sessions::read_sessions(&paths::sessions_file()?)?;
    if sessions.is_empty() {
        println!("No tracked sessions.");
        return Ok(());
    }

    println!(
        "{:<12} {:<8} {:<11} {:<36} {:>7} {:<8} {:<8} {:>8}",
        "ID", "Tool", "State", "Directory", "PID", "Tool", "Shell", "Age"
    );

    for session in sessions {
        let alive = session
            .pid
            .map(|pid| process::pid_is_alive(pid) && process::pid_is_tool(pid, session.tool))
            .unwrap_or(false);
        let shell_alive = session
            .shell_pid
            .map(process::pid_is_alive)
            .unwrap_or(false);
        let alive_text = if alive {
            format!("{GREEN}yes{RESET}")
        } else if session.pid.is_none() {
            "-".to_string()
        } else {
            format!("{RED}no{RESET}")
        };
        let shell_text = if shell_alive {
            format!("{GREEN}yes{RESET}")
        } else if session.shell_pid.is_none() {
            "-".to_string()
        } else {
            format!("{RED}no{RESET}")
        };
        println!(
            "{:<12} {:<8} {:<11} {:<36} {:>7} {:<17} {:<17} {:>8}",
            truncate(&session.session_id, 12),
            session.tool,
            state_text(session.state),
            truncate(&session.directory.display().to_string(), 36),
            session
                .pid
                .map(|pid| pid.to_string())
                .unwrap_or_else(|| "-".to_string()),
            alive_text,
            shell_text,
            age(session.registered_at)
        );
    }

    Ok(())
}

fn state_text(state: SessionState) -> &'static str {
    match state {
        SessionState::Active => "active",
        SessionState::Recoverable => "recoverable",
    }
}

fn truncate(value: &str, max: usize) -> String {
    if value.chars().count() <= max {
        return value.to_string();
    }

    let mut truncated: String = value.chars().take(max.saturating_sub(3)).collect();
    truncated.push_str("...");
    truncated
}

fn age(registered_at: chrono::DateTime<Utc>) -> String {
    let seconds = (Utc::now() - registered_at).num_seconds().max(0);
    if seconds < 60 {
        return format!("{seconds}s");
    }

    let minutes = seconds / 60;
    if minutes < 60 {
        return format!("{minutes}m");
    }

    let hours = minutes / 60;
    if hours < 24 {
        return format!("{hours}h");
    }

    format!("{}d", hours / 24)
}
