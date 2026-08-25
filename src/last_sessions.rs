use crate::Tool;
use crate::sessions::SessionRecord;
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LastSessionRecord {
    pub session_id: String,
    pub tool: Tool,
    pub shell_pid: i32,
    pub shell_pid_started_at: String,
    pub directory: PathBuf,
    pub ended_at: DateTime<Utc>,
}

impl LastSessionRecord {
    pub fn from_session(session: &SessionRecord) -> Option<Self> {
        Some(Self {
            session_id: session.session_id.clone(),
            tool: session.tool,
            shell_pid: session.shell_pid?,
            shell_pid_started_at: session.shell_pid_started_at.clone()?,
            directory: session.directory.clone(),
            ended_at: Utc::now(),
        })
    }
}

pub fn remember(path: &Path, record: LastSessionRecord) -> Result<()> {
    with_records_mut(path, |records| {
        records.retain(|existing| {
            existing.tool != record.tool
                || existing.shell_pid != record.shell_pid
                || existing.shell_pid_started_at != record.shell_pid_started_at
        });
        records.push(record);
        Ok(())
    })
}

pub fn find(
    path: &Path,
    tool: Tool,
    shell_pid: i32,
    shell_pid_started_at: &str,
) -> Result<Option<LastSessionRecord>> {
    Ok(read_records(path)?.into_iter().find(|record| {
        record.tool == tool
            && record.shell_pid == shell_pid
            && record.shell_pid_started_at == shell_pid_started_at
    }))
}

fn ensure_store(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    if !path.exists() {
        fs::write(path, b"[]\n").with_context(|| format!("failed to create {}", path.display()))?;
    }
    Ok(())
}

fn read_records(path: &Path) -> Result<Vec<LastSessionRecord>> {
    ensure_store(path)?;
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .with_context(|| format!("failed to open {}", path.display()))?;
    file.lock_shared()
        .with_context(|| format!("failed to lock {}", path.display()))?;
    let result = read_from_file(&mut file);
    let _ = file.unlock();
    result
}

fn with_records_mut<T>(
    path: &Path,
    update: impl FnOnce(&mut Vec<LastSessionRecord>) -> Result<T>,
) -> Result<T> {
    ensure_store(path)?;
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .with_context(|| format!("failed to open {}", path.display()))?;
    file.lock_exclusive()
        .with_context(|| format!("failed to lock {}", path.display()))?;
    let mut records = read_from_file(&mut file)?;
    let result = update(&mut records);
    if result.is_ok() {
        write_to_file(&mut file, &records)?;
    }
    let _ = file.unlock();
    result
}

fn read_from_file(file: &mut File) -> Result<Vec<LastSessionRecord>> {
    file.seek(SeekFrom::Start(0))?;
    let mut contents = String::new();
    file.read_to_string(&mut contents)?;
    if contents.trim().is_empty() {
        return Ok(Vec::new());
    }
    serde_json::from_str(&contents).context("failed to parse last sessions")
}

fn write_to_file(file: &mut File, records: &[LastSessionRecord]) -> Result<()> {
    file.set_len(0)?;
    file.seek(SeekFrom::Start(0))?;
    serde_json::to_writer_pretty(&mut *file, records)?;
    file.write_all(b"\n")?;
    file.sync_data()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sessions::SessionRecord;

    fn session(id: &str, tool: Tool, shell_pid: i32) -> SessionRecord {
        let mut session = SessionRecord::new(
            tool,
            id.to_string(),
            Some(10),
            Some(shell_pid),
            PathBuf::from("/tmp/project"),
            None,
            None,
            Some("startup".to_string()),
        );
        session.shell_pid_started_at = Some("Wed Jan 1 00:00:00 2020".to_string());
        session
    }

    #[test]
    fn remember_replaces_only_the_same_shell_and_tool() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("last-sessions.json");

        for record in [
            LastSessionRecord::from_session(&session("claude-old", crate::tool("claude"), 20))
                .unwrap(),
            LastSessionRecord::from_session(&session("codex", crate::tool("codex"), 20)).unwrap(),
            LastSessionRecord::from_session(&session("other-shell", crate::tool("claude"), 30))
                .unwrap(),
            LastSessionRecord::from_session(&session("claude-new", crate::tool("claude"), 20))
                .unwrap(),
        ] {
            remember(&path, record).unwrap();
        }

        let records = read_records(&path).unwrap();
        assert_eq!(records.len(), 3);
        assert_eq!(
            find(&path, crate::tool("claude"), 20, "Wed Jan 1 00:00:00 2020")
                .unwrap()
                .unwrap()
                .session_id,
            "claude-new"
        );
    }
}
