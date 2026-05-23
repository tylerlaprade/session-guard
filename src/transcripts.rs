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
    Ok(sessions)
}

fn discover_claude_sessions(sessions: &mut Vec<SessionRecord>) -> Result<()> {
    let root = std::env::var_os("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or(paths::home_dir()?.join(".claude"))
        .join("projects");
    discover_jsonl(&root, Tool::Claude, sessions)
}

fn discover_codex_sessions(sessions: &mut Vec<SessionRecord>) -> Result<()> {
    let root = std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .unwrap_or(paths::home_dir()?.join(".codex"))
        .join("sessions");
    discover_jsonl(&root, Tool::Codex, sessions)
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

        let Some((session_id, directory, timestamp)) =
            read_metadata(&path, tool).ok().flatten()
        else {
            continue;
        };
        sessions.push(SessionRecord::from_transcript(
            tool, session_id, directory, path, timestamp,
        ));
    }

    Ok(())
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

fn read_metadata(path: &Path, tool: Tool) -> Result<Option<(String, PathBuf, DateTime<Utc>)>> {
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
}
