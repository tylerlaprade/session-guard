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
            if is_unused_spare(tool, &session_id) {
                continue;
            }
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

const EDITORS: &[&str] = &["hx", "helix", "vi", "vim", "nvim"];
const SHELLS: &[&str] = &["zsh", "bash", "fish", "sh", "dash", "ksh", "tcsh", "nu"];

fn executable_name(process: &ProcInfo) -> &str {
    let executable = process
        .command
        .split_whitespace()
        .next()
        .unwrap_or_default();
    executable
        .rsplit('/')
        .next()
        .unwrap_or_default()
        .trim_start_matches('-')
}

pub(crate) fn is_editor_process(process: &ProcInfo) -> bool {
    EDITORS.contains(&executable_name(process))
}

/// Editors typed at a tab's shell prompt, each recorded with the exact
/// command that reopens it. An editor under anything but a shell (`git
/// commit`, an agent's editor key) was not the user's own launch, and one
/// beneath a tracked session is already restored as part of it, including an
/// editor that a restore relaunched.
pub fn discover_editors(
    processes: &[ProcInfo],
    tracked: &std::collections::HashSet<i32>,
) -> Vec<SessionRecord> {
    let by_pid: std::collections::HashMap<i32, &ProcInfo> = processes
        .iter()
        .map(|process| (process.pid, process))
        .collect();
    let within_tracked = |process: &ProcInfo| {
        let mut pid = process.pid;
        for _ in 0..by_pid.len() {
            if tracked.contains(&pid) {
                return true;
            }
            match by_pid.get(&pid) {
                Some(current) if current.ppid > 1 => pid = current.ppid,
                _ => return false,
            }
        }
        false
    };
    processes
        .iter()
        .filter(|process| is_editor_process(process))
        .filter(|process| {
            by_pid
                .get(&process.ppid)
                .is_some_and(|parent| SHELLS.contains(&executable_name(parent)))
        })
        .filter(|process| !within_tracked(process))
        .filter_map(|process| {
            let command = crate::process::process_arguments(process.pid)?;
            let directory = crate::process::process_directory(process.pid)?;
            let started = crate::process::process_start_identity(process.pid).ok()?;
            let started = crate::process::parse_identity(&started)?
                .and_utc()
                .timestamp();
            let mut record = SessionRecord::new(
                Tool::editor(),
                format!("{}-{}-{started}", executable_name(process), process.pid),
                Some(process.pid),
                Some(process.ppid),
                directory,
                None,
                None,
                Some("scan".to_string()),
            );
            record.command = Some(command);
            Some(record)
        })
        .collect()
}

pub fn is_unused_spare(tool: Tool, session_id: &str) -> bool {
    tool.spec()
        .is_unused_spare
        .is_some_and(|check| paths::tool_home(tool).is_ok_and(|home| check(&home, session_id)))
}

pub(crate) fn claude_is_unused_spare(home: &std::path::Path, session_id: &str) -> bool {
    if is_uuid(session_id)
        && let Ok(file) =
            std::fs::File::open(home.join("jobs").join(&session_id[..8]).join("state.json"))
        && let Ok(job) = serde_json::from_reader::<_, serde_json::Value>(file)
        && job.get("sessionId").and_then(serde_json::Value::as_str) == Some(session_id)
        && job
            .get("state")
            .and_then(serde_json::Value::as_str)
            .is_some()
    {
        return false;
    }
    let Ok(file) = std::fs::File::open(home.join("daemon/roster.json")) else {
        return false;
    };
    let Ok(roster) = serde_json::from_reader::<_, serde_json::Value>(file) else {
        return false;
    };
    roster
        .get("workers")
        .and_then(serde_json::Value::as_object)
        .is_some_and(|workers| {
            workers.values().any(|worker| {
                worker.get("sessionId").and_then(serde_json::Value::as_str) == Some(session_id)
                    && worker
                        .pointer("/dispatch/source")
                        .and_then(serde_json::Value::as_str)
                        == Some("spare")
            })
        })
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

    #[test]
    fn only_an_explicit_unused_spare_is_excluded() {
        let home = tempfile::tempdir().unwrap();
        let daemon = home.path().join("daemon");
        std::fs::create_dir(&daemon).unwrap();
        for source in ["spare", "slash", "fleet", "shell", "respawn"] {
            let roster = serde_json::json!({"workers": {"worker": {
                "sessionId": SESSION_ID,
                "dispatch": {"source": source},
                "ptySock": "/tmp/spare/worker.sock"
            }}});
            std::fs::write(daemon.join("roster.json"), roster.to_string()).unwrap();
            assert_eq!(
                claude_is_unused_spare(home.path(), SESSION_ID),
                source == "spare"
            );
            assert!(!claude_is_unused_spare(home.path(), "different-session"));
        }
    }

    #[test]
    fn missing_or_unknown_native_status_does_not_discard_a_session() {
        let home = tempfile::tempdir().unwrap();
        assert!(!claude_is_unused_spare(home.path(), SESSION_ID));
        std::fs::create_dir(home.path().join("daemon")).unwrap();
        std::fs::write(home.path().join("daemon/roster.json"), "{").unwrap();
        assert!(!claude_is_unused_spare(home.path(), SESSION_ID));
    }

    #[test]
    fn a_spare_promoted_into_a_native_job_is_preserved() {
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(home.path().join("daemon")).unwrap();
        std::fs::write(
            home.path().join("daemon/roster.json"),
            serde_json::json!({"workers": {"worker": {
                "sessionId": SESSION_ID, "dispatch": {"source": "spare"}
            }}})
            .to_string(),
        )
        .unwrap();
        assert!(claude_is_unused_spare(home.path(), SESSION_ID));
        let job = home.path().join("jobs").join(&SESSION_ID[..8]);
        std::fs::create_dir_all(&job).unwrap();
        std::fs::write(
            job.join("state.json"),
            serde_json::json!({"sessionId": SESSION_ID, "state": "running"}).to_string(),
        )
        .unwrap();
        assert!(!claude_is_unused_spare(home.path(), SESSION_ID));
    }

    fn editor_under(parent: &str) -> Vec<ProcInfo> {
        vec![
            ProcInfo {
                pid: 900_001,
                ppid: 900_000,
                command: parent.to_string(),
            },
            ProcInfo {
                pid: 900_000,
                ppid: 1,
                command: "/Applications/Ghostty.app/Contents/MacOS/ghostty".to_string(),
            },
            ProcInfo {
                pid: std::process::id() as i32,
                ppid: 900_001,
                command: "hx notes.txt".to_string(),
            },
        ]
    }

    #[test]
    fn an_editor_typed_at_a_shell_is_recorded_with_its_exact_command() {
        let records = discover_editors(&editor_under("-zsh"), &std::collections::HashSet::new());
        assert_eq!(records.len(), 1);
        let record = &records[0];
        assert_eq!(record.tool, Tool::editor());
        assert_eq!(record.shell_pid, Some(900_001));
        assert_eq!(record.directory, std::env::current_dir().unwrap());
        assert_eq!(
            record.command.as_deref(),
            Some(std::env::args().collect::<Vec<_>>().as_slice())
        );
        assert!(
            record
                .session_id
                .starts_with(&format!("hx-{}-", std::process::id()))
        );
    }

    #[test]
    fn an_editor_not_launched_by_the_user_at_a_shell_is_ignored() {
        let untracked = std::collections::HashSet::new();
        assert!(discover_editors(&editor_under("git commit"), &untracked).is_empty());
        assert!(discover_editors(&editor_under("/opt/homebrew/bin/nvim"), &untracked).is_empty());
        for tracked in [900_000, 900_001, std::process::id() as i32] {
            assert!(
                discover_editors(
                    &editor_under("-zsh"),
                    &std::collections::HashSet::from([tracked])
                )
                .is_empty()
            );
        }
    }

    #[test]
    fn editors_are_named_by_their_executable() {
        for command in [
            "hx",
            "/opt/homebrew/bin/nvim --clean a.txt",
            "vim",
            "vi -R x",
            "helix",
        ] {
            assert!(is_editor_process(&proc(command)), "{command}");
        }
        for command in ["-zsh", "hxd file", "/usr/bin/vimtutor", "claude"] {
            assert!(!is_editor_process(&proc(command)), "{command}");
        }
    }

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
