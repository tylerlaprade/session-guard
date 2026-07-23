use crate::Tool;
use crate::paths;
use crate::sessions::{DEFAULT_RECOVERABLE_DAYS, SessionRecord};
use anyhow::Result;
use chrono::{DateTime, Duration, Utc};
use serde_json::Value;
use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

pub fn discover_recent_sessions() -> Result<Vec<SessionRecord>> {
    let mut sessions = Vec::new();
    discover_claude_sessions(&mut sessions)?;
    discover_codex_sessions(&mut sessions)?;
    discover_grok_sessions(&mut sessions)?;
    Ok(sessions)
}

pub fn claude_projects_dir() -> Result<PathBuf> {
    Ok(std::env::var_os("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or(paths::home_dir()?.join(".claude"))
        .join("projects"))
}

pub fn claude_transcript_path(session_id: &str) -> Result<Option<PathBuf>> {
    let root = claude_projects_dir()?;
    let file_name = format!("{session_id}.jsonl");
    let Ok(entries) = fs::read_dir(root) else {
        return Ok(None);
    };

    for entry in entries.flatten() {
        let project_dir = entry.path();
        if !project_dir.is_dir() {
            continue;
        }

        let path = project_dir.join(&file_name);
        if path.is_file() {
            return Ok(Some(path));
        }
    }

    Ok(None)
}

fn discover_claude_sessions(sessions: &mut Vec<SessionRecord>) -> Result<()> {
    let root = claude_projects_dir()?;
    discover_jsonl(&root, Tool::Claude, sessions)
}

fn discover_codex_sessions(sessions: &mut Vec<SessionRecord>) -> Result<()> {
    let root = std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .unwrap_or(paths::home_dir()?.join(".codex"))
        .join("sessions");
    discover_jsonl(&root, Tool::Codex, sessions)
}

fn discover_grok_sessions(sessions: &mut Vec<SessionRecord>) -> Result<()> {
    let root = paths::tool_home(Tool::Grok)?.join("sessions");
    if !root.is_dir() {
        return Ok(());
    }

    let Ok(cwd_entries) = fs::read_dir(&root) else {
        return Ok(());
    };
    for cwd_entry in cwd_entries.flatten() {
        let cwd_dir = cwd_entry.path();
        if !cwd_dir.is_dir() {
            continue;
        }

        let Ok(session_entries) = fs::read_dir(&cwd_dir) else {
            continue;
        };
        for session_entry in session_entries.flatten() {
            let session_dir = session_entry.path();
            if !session_dir.is_dir() {
                continue;
            }

            let summary_path = session_dir.join("summary.json");
            if !summary_path.is_file() || !is_recent(&summary_path).unwrap_or(false) {
                continue;
            }

            let Some((session_id, directory, timestamp)) =
                read_grok_summary(&summary_path).ok().flatten()
            else {
                continue;
            };
            sessions.push(SessionRecord::from_transcript(
                Tool::Grok,
                session_id,
                directory,
                summary_path,
                timestamp,
            ));
        }
    }

    Ok(())
}

fn read_grok_summary(path: &Path) -> Result<Option<(String, PathBuf, DateTime<Utc>)>> {
    let contents = fs::read_to_string(path)?;
    let value: Value = serde_json::from_str(&contents)?;

    // Grok subagent sessions are children of a primary session, never tabs of
    // their own. (Live subagents never register either: Grok fires separate
    // SubagentStart/SubagentStop events that session-guard does not hook.)
    if value.get("session_kind").and_then(Value::as_str) == Some("subagent") {
        return Ok(None);
    }

    let session_id = value
        .pointer("/info/id")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .or_else(|| {
            path.parent()
                .and_then(|parent| parent.file_name())
                .and_then(|name| name.to_str())
                .map(ToOwned::to_owned)
        });
    let directory = value
        .pointer("/info/cwd")
        .and_then(Value::as_str)
        .map(PathBuf::from);
    let timestamp = value
        .get("updated_at")
        .or_else(|| value.get("created_at"))
        .and_then(Value::as_str)
        .and_then(parse_timestamp)
        .or_else(|| file_modified_at(path).ok());

    Ok(match (session_id, directory, timestamp) {
        (Some(session_id), Some(directory), Some(timestamp)) => {
            Some((session_id, directory, timestamp))
        }
        _ => None,
    })
}

fn discover_jsonl(root: &Path, tool: Tool, sessions: &mut Vec<SessionRecord>) -> Result<()> {
    if !root.is_dir() {
        return Ok(());
    }

    let mut files = Vec::new();
    collect_jsonl_files(root, &mut files);
    for path in files {
        if !is_recent(&path).unwrap_or(false) {
            continue;
        }

        if tool == Tool::Codex
            && !filename_session_id(&path, tool)
                .is_some_and(|session_id| codex_rollout_is_cli(&path, &session_id))
        {
            continue;
        }

        let Some((session_id, directory, timestamp)) = read_metadata(&path, tool).ok().flatten()
        else {
            continue;
        };
        sessions.push(SessionRecord::from_transcript(
            tool, session_id, directory, path, timestamp,
        ));
    }

    Ok(())
}

/// The Codex desktop app, its scheduled automations, `codex exec`, and
/// subagents run through the same codex core as the terminal TUI, sharing
/// `~/.codex` and firing the same hooks — but their threads live outside any
/// terminal tab and restore as junk. The rollout's `session_meta` records the
/// owning frontend. A rollout also embeds its ANCESTOR chain's metas — a
/// subagent file ends with the root terminal's "cli" meta — so only metas for
/// the session's own id count; re-opening the thread appends another own-id
/// meta, and the latest of those decides. Only `source == "cli"` threads
/// belong in a tab.
pub fn codex_rollout_is_cli(path: &Path, session_id: &str) -> bool {
    let Ok(file) = File::open(path) else {
        return false;
    };

    let mut source = None;
    for line in BufReader::new(file).lines() {
        let Ok(line) = line else {
            break;
        };
        if !line.contains("\"session_meta\"") {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if value.get("type").and_then(Value::as_str) == Some("session_meta")
            && value.pointer("/payload/id").and_then(Value::as_str) == Some(session_id)
            && let Some(meta_source) = value.pointer("/payload/source")
        {
            source = Some(meta_source.clone());
        }
    }

    // Subagent threads carry an object source ({"subagent": ...}); as_str
    // rejects those along with "vscode"/"exec"/"mcp".
    source.is_some_and(|source| source.as_str() == Some("cli"))
}

fn collect_jsonl_files(dir: &Path, files: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_jsonl_files(&path, files);
        } else if path
            .extension()
            .is_some_and(|extension| extension == "jsonl")
        {
            files.push(path);
        }
    }
}

fn is_recent(path: &Path) -> Result<bool> {
    let modified = fs::metadata(path)?.modified()?;
    let modified = DateTime::<Utc>::from(modified);
    Ok(modified >= Utc::now() - Duration::days(DEFAULT_RECOVERABLE_DAYS))
}

pub fn read_metadata(path: &Path, tool: Tool) -> Result<Option<(String, PathBuf, DateTime<Utc>)>> {
    let file = File::open(path)?;
    let reader = BufReader::new(file);
    let mut session_id = filename_session_id(path, tool);
    let mut cwd = None;
    let mut timestamp = file_modified_at(path).ok();

    for line in reader.lines().take(200) {
        let line = line?;
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };

        if session_id.is_none() {
            session_id = json_string(&value, &["session_id", "sessionId"])
                .or_else(|| value.pointer("/payload/id").and_then(Value::as_str))
                .map(ToOwned::to_owned);
        }
        if cwd.is_none() {
            cwd = json_string(&value, &["cwd"])
                .or_else(|| value.pointer("/payload/cwd").and_then(Value::as_str))
                .map(PathBuf::from);
        }
        if timestamp.is_none() {
            timestamp = json_string(&value, &["timestamp"])
                .or_else(|| value.pointer("/payload/timestamp").and_then(Value::as_str))
                .and_then(parse_timestamp);
        }

        if session_id.is_some() && cwd.is_some() && timestamp.is_some() {
            break;
        }
    }

    Ok(match (session_id, cwd, timestamp) {
        (Some(session_id), Some(cwd), Some(timestamp)) => Some((session_id, cwd, timestamp)),
        _ => None,
    })
}

fn filename_session_id(path: &Path, tool: Tool) -> Option<String> {
    let stem = path.file_stem()?.to_str()?;
    match tool {
        Tool::Claude => Some(stem.to_string()),
        Tool::Codex => stem
            .len()
            .checked_sub(36)
            .and_then(|start| stem.get(start..))
            .map(ToOwned::to_owned),
        // Grok sessions are directories; summary.json is handled separately.
        Tool::Grok => None,
    }
}

fn json_string<'a>(value: &'a Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter().find_map(|key| value.get(*key)?.as_str())
}

fn parse_timestamp(value: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|timestamp| timestamp.with_timezone(&Utc))
}

fn file_modified_at(path: &Path) -> Result<DateTime<Utc>> {
    Ok(DateTime::<Utc>::from(fs::metadata(path)?.modified()?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::OpenOptions;
    use std::io::Write;

    #[test]
    fn parses_codex_metadata_from_first_line() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir
            .path()
            .join("rollout-2026-04-27T16-36-45-019dd0a8-a320-78b3-a770-fffc78f09c5d.jsonl");
        let mut file = File::create(&path).unwrap();
        writeln!(
            file,
            r#"{{"timestamp":"2026-04-27T20:36:47.712Z","type":"session_meta","payload":{{"id":"019dd0a8-a320-78b3-a770-fffc78f09c5d","cwd":"/tmp/project"}}}}"#
        )
        .unwrap();

        let (id, cwd, _) = read_metadata(&path, Tool::Codex).unwrap().unwrap();
        assert_eq!(id, "019dd0a8-a320-78b3-a770-fffc78f09c5d");
        assert_eq!(cwd, PathBuf::from("/tmp/project"));
    }

    #[test]
    fn codex_rollout_latest_own_session_meta_decides_frontend() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rollout.jsonl");

        // Desktop-only thread: never a tab.
        fs::write(
            &path,
            r#"{"type":"session_meta","payload":{"id":"a","cwd":"/tmp","source":"vscode"}}"#,
        )
        .unwrap();
        assert!(!codex_rollout_is_cli(&path, "a"));

        // Re-opened in a terminal afterwards: the appended own-id cli meta wins.
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        writeln!(file).unwrap();
        writeln!(
            file,
            r#"{{"type":"session_meta","payload":{{"id":"a","cwd":"/tmp","source":"cli"}}}}"#
        )
        .unwrap();
        assert!(codex_rollout_is_cli(&path, "a"));

        // Picked up by the desktop app afterwards: no longer a tab.
        writeln!(
            file,
            r#"{{"type":"session_meta","payload":{{"id":"a","cwd":"/tmp","source":"vscode"}}}}"#
        )
        .unwrap();
        assert!(!codex_rollout_is_cli(&path, "a"));
    }

    #[test]
    fn codex_rollout_ancestor_chain_meta_does_not_leak_cli() {
        // A subagent rollout embeds its ancestor chain after its own meta,
        // ending with the root terminal's "cli" meta. The root's meta must not
        // make the subagent's file count as a terminal session.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("subagent.jsonl");
        fs::write(
            &path,
            concat!(
                r#"{"type":"session_meta","payload":{"id":"child","cwd":"/tmp","source":{"subagent":"review"}}}"#,
                "\n",
                r#"{"type":"session_meta","payload":{"id":"root","cwd":"/tmp","source":"cli"}}"#,
            ),
        )
        .unwrap();

        assert!(!codex_rollout_is_cli(&path, "child"));
    }

    #[test]
    fn codex_rollout_subagent_and_missing_sources_are_not_cli() {
        let dir = tempfile::tempdir().unwrap();

        let subagent = dir.path().join("subagent.jsonl");
        fs::write(
            &subagent,
            r#"{"type":"session_meta","payload":{"id":"a","cwd":"/tmp","source":{"subagent":"review"}}}"#,
        )
        .unwrap();
        assert!(!codex_rollout_is_cli(&subagent, "a"));

        let no_meta = dir.path().join("no-meta.jsonl");
        fs::write(&no_meta, r#"{"type":"turn_context","payload":{}}"#).unwrap();
        assert!(!codex_rollout_is_cli(&no_meta, "a"));

        assert!(!codex_rollout_is_cli(
            &dir.path().join("missing.jsonl"),
            "a"
        ));
    }

    #[test]
    fn grok_subagent_summary_is_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("summary.json");
        fs::write(
            &path,
            r#"{
              "info": {"id": "abc", "cwd": "/tmp/project"},
              "session_kind": "subagent",
              "updated_at": "2026-07-10T00:02:58.951370Z"
            }"#,
        )
        .unwrap();

        assert!(read_grok_summary(&path).unwrap().is_none());
    }

    #[test]
    fn parses_grok_summary_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let session_dir = dir.path().join("019f486e-061f-7343-b53e-f487a0f30e85");
        fs::create_dir_all(&session_dir).unwrap();
        let path = session_dir.join("summary.json");
        fs::write(
            &path,
            r#"{
              "info": {
                "id": "019f486e-061f-7343-b53e-f487a0f30e85",
                "cwd": "/tmp/project"
              },
              "created_at": "2026-07-09T19:49:58.259284Z",
              "updated_at": "2026-07-10T00:02:58.951370Z"
            }"#,
        )
        .unwrap();

        let (id, cwd, timestamp) = read_grok_summary(&path).unwrap().unwrap();
        assert_eq!(id, "019f486e-061f-7343-b53e-f487a0f30e85");
        assert_eq!(cwd, PathBuf::from("/tmp/project"));
        assert_eq!(timestamp.to_rfc3339(), "2026-07-10T00:02:58.951370+00:00");
    }
}
