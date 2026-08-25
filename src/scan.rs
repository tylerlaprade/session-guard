//! Discovers detached tool sessions that never pass through terminal hooks.
//!
//! Claude Code Remote Control forks a session worker under launchd with a fresh
//! `--session-id`; the hook path never sees that worker, so the daemon has to
//! find it from the process table and recover the real cwd from its transcript.

use crate::Tool;
use crate::commands::register;
use crate::paths;
use crate::process::ProcInfo;
use crate::sessions::SessionRecord;
use crate::transcripts;
use anyhow::Result;
use std::path::PathBuf;

pub fn discover_sessions(processes: &[ProcInfo]) -> Result<Vec<SessionRecord>> {
    let mut records = Vec::new();

    for tool in Tool::all() {
        // Only harnesses that can name a live session from its argv are
        // recoverable this way; Codex interactive argv, for one, does not
        // carry the id, so it is skipped rather than guessed at.
        let Some(session_id_from_process) = tool.spec().session_id_from_process else {
            continue;
        };
        let tool_home = paths::tool_home(tool)?;

        for process in processes {
            let Some(session_id) = session_id_from_process(process) else {
                continue;
            };
            let Some((directory, transcript_path)) =
                resolve_session(tool, &session_id).unwrap_or_default()
            else {
                continue;
            };

            if register::is_internal_directory(&directory, &tool_home) {
                continue;
            }

            records.push(SessionRecord::new(
                tool,
                session_id,
                Some(process.pid),
                None,
                directory,
                Some(transcript_path),
                None,
                Some("scan".to_string()),
            ));
        }
    }

    Ok(records)
}

fn resolve_session(tool: Tool, session_id: &str) -> Result<Option<(PathBuf, PathBuf)>> {
    let Some(transcript_path) = transcripts::transcript_path(tool, session_id)? else {
        return Ok(None);
    };
    let Some((metadata_id, directory, _)) = transcripts::read_metadata(&transcript_path, tool)?
    else {
        return Ok(None);
    };

    if metadata_id != session_id {
        return Ok(None);
    }

    Ok(Some((directory, transcript_path)))
}

pub(crate) fn claude_session_id_from_process(process: &ProcInfo) -> Option<String> {
    if !is_claude_session_process(process) {
        return None;
    }
    extract_session_id(&process.command)
}

pub(crate) fn is_claude_session_process(process: &ProcInfo) -> bool {
    if !is_claude_process(process) {
        return false;
    }

    let command = process.command.as_str();
    let mut parts = command.split_whitespace();
    let _binary = parts.next();
    let first = parts.next();
    let second = parts.next();
    if matches!(first, Some("--bg-pty-host" | "--bg-spare"))
        || (first == Some("daemon") && second == Some("run"))
    {
        return false;
    }

    true
}

/// Conservative Claude CLI/process recognition for destructive cleanup.
/// Unlike discovery, this intentionally includes background/daemon shapes: an
/// unregistered live Claude process must make cleanup keep data, not guess.
pub(crate) fn is_claude_process(process: &ProcInfo) -> bool {
    let command = process.command.as_str();
    let binary = command.split_whitespace().next().unwrap_or_default();
    if binary.starts_with("/Applications/Claude.app/")
        || binary.starts_with("/Applications/Codex.app/")
    {
        return false;
    }

    if !binary.contains(".local/share/claude/versions/")
        && std::path::Path::new(binary)
            .file_name()
            .and_then(|name| name.to_str())
            != Some("claude")
    {
        return false;
    }
    true
}

fn extract_session_id(command: &str) -> Option<String> {
    let mut parts = command.split_whitespace();
    while let Some(part) = parts.next() {
        if part != "--session-id" {
            continue;
        }

        let candidate = parts.next()?;
        if is_uuid(candidate) {
            return Some(candidate.to_string());
        }
    }

    None
}

fn is_uuid(value: &str) -> bool {
    if value.len() != 36 {
        return false;
    }

    value.chars().enumerate().all(|(index, char)| match index {
        8 | 13 | 18 | 23 => char == '-',
        _ => char.is_ascii_hexdigit(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SESSION_ID: &str = "7a248d2c-865f-4829-af1b-2bee5f0c2b48";

    fn proc(command: &str) -> ProcInfo {
        ProcInfo {
            pid: 40893,
            ppid: 1,
            command: command.to_string(),
        }
    }

    #[test]
    fn selects_detached_remote_control_child() {
        let process = proc(&format!(
            "/Users/tyler/.local/share/claude/versions/2.1.159 --session-id {SESSION_ID} --fork-session --resume /Users/tyler/.claude/projects/-Users-tyler-Code-flint/ad7acbf1-a306-4467-8b35-4bed04670d21.jsonl --allow-dangerously-skip-permissions --model opus --permission-mode auto"
        ));

        assert_eq!(
            claude_session_id_from_process(&process),
            Some(SESSION_ID.to_string())
        );
    }

    #[test]
    fn rejects_bg_pty_host_wrapper() {
        let process = proc(&format!(
            "/Users/tyler/.local/share/claude/versions/2.1.159 --bg-pty-host /tmp/cc-daemon-501/pty/1.sock 143 43 -- /Users/tyler/.local/share/claude/versions/2.1.159 --session-id {SESSION_ID} --fork-session --resume /Users/tyler/.claude/projects/-Users-tyler-Code-flint/ad7acbf1-a306-4467-8b35-4bed04670d21.jsonl --model opus"
        ));

        assert_eq!(claude_session_id_from_process(&process), None);
    }

    #[test]
    fn rejects_bg_spare() {
        let process = proc(&format!(
            "/Users/tyler/.local/share/claude/versions/2.1.159 --bg-spare --session-id {SESSION_ID}"
        ));

        assert_eq!(claude_session_id_from_process(&process), None);
    }

    #[test]
    fn rejects_daemon_run() {
        let process = proc(&format!(
            "/Users/tyler/.local/bin/claude daemon run --session-id {SESSION_ID}"
        ));

        assert_eq!(claude_session_id_from_process(&process), None);
    }

    #[test]
    fn rejects_desktop_app_helper() {
        let process = proc(&format!(
            "/Applications/Claude.app/Contents/MacOS/Claude Helper --session-id {SESSION_ID}"
        ));

        assert_eq!(claude_session_id_from_process(&process), None);
    }

    #[test]
    fn prompt_text_that_names_internal_commands_is_still_a_live_session() {
        for phrase in ["--bg-pty-host", "--bg-spare", "daemon run", "codex exec"] {
            let process = proc(&format!(
                "claude --allow-dangerously-skip-permissions please inspect {phrase} behavior"
            ));
            assert!(is_claude_session_process(&process), "phrase: {phrase}");
            assert!(is_claude_process(&process), "phrase: {phrase}");
        }
    }
}
