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
    let claude_home = paths::tool_home(Tool::Claude)?;

    for process in processes {
        let Some(session_id) = trackable_claude_session_id(process) else {
            continue;
        };
        let Some((directory, transcript_path)) =
            resolve_claude_session(&session_id).unwrap_or_default()
        else {
            continue;
        };

        if register::is_internal_directory(&directory, &claude_home) {
            continue;
        }

        records.push(SessionRecord::new(
            Tool::Claude,
            session_id,
            Some(process.pid),
            None,
            directory,
            Some(transcript_path),
            None,
            Some("scan".to_string()),
        ));
    }

    // Codex interactive argv does not reliably carry the live session id, so
    // the process scanner intentionally skips Codex rather than guessing.
    Ok(records)
}

fn resolve_claude_session(session_id: &str) -> Result<Option<(PathBuf, PathBuf)>> {
    let Some(transcript_path) = transcripts::claude_transcript_path(session_id)? else {
        return Ok(None);
    };
    let Some((metadata_id, directory, _)) =
        transcripts::read_metadata(&transcript_path, Tool::Claude)?
    else {
        return Ok(None);
    };

    if metadata_id != session_id {
        return Ok(None);
    }

    Ok(Some((directory, transcript_path)))
}

fn trackable_claude_session_id(process: &ProcInfo) -> Option<String> {
    let command = process.command.as_str();
    if command.contains("--bg-pty-host")
        || command.contains("--bg-spare")
        || command.contains("daemon run")
        || command.contains("codex exec")
    {
        return None;
    }

    let binary = command.split_whitespace().next().unwrap_or_default();
    if binary.starts_with("/Applications/Claude.app/")
        || binary.starts_with("/Applications/Codex.app/")
    {
        return None;
    }

    if !command.contains(".local/share/claude/versions/")
        && std::path::Path::new(binary)
            .file_name()
            .and_then(|name| name.to_str())
            != Some("claude")
    {
        return None;
    }

    extract_session_id(command)
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
            trackable_claude_session_id(&process),
            Some(SESSION_ID.to_string())
        );
    }

    #[test]
    fn rejects_bg_pty_host_wrapper() {
        let process = proc(&format!(
            "/Users/tyler/.local/share/claude/versions/2.1.159 --bg-pty-host /tmp/cc-daemon-501/pty/1.sock 143 43 -- /Users/tyler/.local/share/claude/versions/2.1.159 --session-id {SESSION_ID} --fork-session --resume /Users/tyler/.claude/projects/-Users-tyler-Code-flint/ad7acbf1-a306-4467-8b35-4bed04670d21.jsonl --model opus"
        ));

        assert_eq!(trackable_claude_session_id(&process), None);
    }

    #[test]
    fn rejects_bg_spare() {
        let process = proc(&format!(
            "/Users/tyler/.local/share/claude/versions/2.1.159 --bg-spare --session-id {SESSION_ID}"
        ));

        assert_eq!(trackable_claude_session_id(&process), None);
    }

    #[test]
    fn rejects_daemon_run() {
        let process = proc(&format!(
            "/Users/tyler/.local/bin/claude daemon run --session-id {SESSION_ID}"
        ));

        assert_eq!(trackable_claude_session_id(&process), None);
    }

    #[test]
    fn rejects_desktop_app_helper() {
        let process = proc(&format!(
            "/Applications/Claude.app/Contents/MacOS/Claude Helper --session-id {SESSION_ID}"
        ));

        assert_eq!(trackable_claude_session_id(&process), None);
    }
}
