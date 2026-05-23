use crate::Tool;
use anyhow::{Context, Result};
use chrono::{DateTime, Duration, Utc};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

pub const DEFAULT_RECOVERABLE_DAYS: i64 = 7;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SessionState {
    Active,
    Recoverable,
}

fn default_state() -> SessionState {
    SessionState::Active
}

fn now() -> DateTime<Utc> {
    Utc::now()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionRecord {
    pub session_id: String,
    pub tool: Tool,
    #[serde(default)]
    pub pid: Option<i32>,
    #[serde(default)]
    pub shell_pid: Option<i32>,
    pub directory: PathBuf,
    #[serde(default)]
    pub transcript_path: Option<PathBuf>,
    pub session_name: Option<String>,
    pub registered_at: DateTime<Utc>,
    #[serde(default = "now")]
    pub last_seen_at: DateTime<Utc>,
    #[serde(default = "default_state")]
    pub state: SessionState,
    #[serde(default)]
    pub dead_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub recoverable_until: Option<DateTime<Utc>>,
    #[serde(default)]
    pub source: Option<String>,
}

impl SessionRecord {
    pub fn new(
        tool: Tool,
        session_id: String,
        pid: Option<i32>,
        shell_pid: Option<i32>,
        directory: PathBuf,
        transcript_path: Option<PathBuf>,
        session_name: Option<String>,
        source: Option<String>,
    ) -> Self {
        let now = Utc::now();
        Self {
            session_id,
            tool,
            pid,
            shell_pid,
            directory,
            transcript_path,
            session_name,
            registered_at: now,
            last_seen_at: now,
            state: SessionState::Active,
            dead_at: None,
            recoverable_until: None,
            source,
        }
    }

    pub fn from_transcript(
        tool: Tool,
        session_id: String,
        directory: PathBuf,
        transcript_path: PathBuf,
        timestamp: DateTime<Utc>,
    ) -> Self {
        Self {
            session_id,
            tool,
            pid: None,
            shell_pid: None,
            directory,
            transcript_path: Some(transcript_path),
            session_name: None,
            registered_at: timestamp,
            last_seen_at: timestamp,
            state: SessionState::Recoverable,
            dead_at: Some(timestamp),
            recoverable_until: Some(timestamp + Duration::days(DEFAULT_RECOVERABLE_DAYS)),
            source: Some("transcript-fallback".to_string()),
        }
    }

    pub fn mark_recoverable(&mut self) {
        let now = Utc::now();
        self.state = SessionState::Recoverable;
        self.dead_at.get_or_insert(now);
        self.recoverable_until
            .get_or_insert(now + Duration::days(DEFAULT_RECOVERABLE_DAYS));
    }

    pub fn mark_active(&mut self) {
        self.state = SessionState::Active;
        self.dead_at = None;
        self.recoverable_until = None;
        self.last_seen_at = Utc::now();
    }

    pub fn recoverable_expired(&self) -> bool {
        self.recoverable_until
            .map(|until| until < Utc::now())
            .unwrap_or(false)
    }
}

pub fn ensure_store(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }

    if !path.exists() {
        fs::write(path, b"[]\n").with_context(|| format!("failed to create {}", path.display()))?;
    }

    Ok(())
}

pub fn read_sessions(path: &Path) -> Result<Vec<SessionRecord>> {
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

pub fn with_sessions_mut<T>(
    path: &Path,
    update: impl FnOnce(&mut Vec<SessionRecord>) -> Result<T>,
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
    let mut sessions = read_from_file(&mut file)?;
    let original = serde_json::to_vec(&sessions)?;
    let result = update(&mut sessions);

    if result.is_ok() && serde_json::to_vec(&sessions)? != original {
        write_to_file(&mut file, &sessions)?;
    }

    let _ = file.unlock();
    result
}

fn read_from_file(file: &mut File) -> Result<Vec<SessionRecord>> {
    file.seek(SeekFrom::Start(0))?;
    let mut contents = String::new();
    file.read_to_string(&mut contents)?;

    if contents.trim().is_empty() {
        return Ok(Vec::new());
    }

    serde_json::from_str(&contents).context("failed to parse active sessions")
}

fn write_to_file(file: &mut File, sessions: &[SessionRecord]) -> Result<()> {
    file.set_len(0)?;
    file.seek(SeekFrom::Start(0))?;
    serde_json::to_writer_pretty(&mut *file, sessions)?;
    file.write_all(b"\n")?;
    file.sync_data()?;
    Ok(())
}

pub fn register(path: &Path, record: SessionRecord) -> Result<()> {
    with_sessions_mut(path, |sessions| {
        sessions.retain(|session| session.session_id != record.session_id);
        sessions.push(record);
        Ok(())
    })
}

pub fn repair_if_corrupt(path: &Path) -> Result<Option<PathBuf>> {
    ensure_store(path)?;

    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .with_context(|| format!("failed to open {}", path.display()))?;
    file.lock_exclusive()
        .with_context(|| format!("failed to lock {}", path.display()))?;

    let mut contents = String::new();
    file.read_to_string(&mut contents)
        .with_context(|| format!("failed to read {}", path.display()))?;

    if contents.trim().is_empty() || serde_json::from_str::<Vec<SessionRecord>>(&contents).is_ok() {
        let _ = file.unlock();
        return Ok(None);
    }

    let backup = path.with_extension(format!("json.corrupt-{}", Utc::now().timestamp()));
    fs::rename(path, &backup).with_context(|| {
        format!(
            "failed to preserve corrupt session file as {}",
            backup.display()
        )
    })?;
    fs::write(path, b"[]\n").with_context(|| format!("failed to recreate {}", path.display()))?;
    let _ = file.unlock();
    Ok(Some(backup))
}

pub fn deregister(path: &Path, session_id: &str) -> Result<()> {
    with_sessions_mut(path, |sessions| {
        sessions.retain(|session| session.session_id != session_id);
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_replaces_existing_session_id() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("active-sessions.json");

        register(
            &path,
            SessionRecord::new(
                Tool::Claude,
                "abc".to_string(),
                Some(1),
                Some(10),
                PathBuf::from("/tmp/one"),
                None,
                None,
                Some("startup".to_string()),
            ),
        )
        .unwrap();
        register(
            &path,
            SessionRecord::new(
                Tool::Claude,
                "abc".to_string(),
                Some(2),
                Some(20),
                PathBuf::from("/tmp/two"),
                Some(PathBuf::from("/tmp/two.jsonl")),
                Some("name".to_string()),
                Some("resume".to_string()),
            ),
        )
        .unwrap();

        let sessions = read_sessions(&path).unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].pid, Some(2));
        assert_eq!(sessions[0].shell_pid, Some(20));
        assert_eq!(sessions[0].directory, PathBuf::from("/tmp/two"));
        assert_eq!(sessions[0].state, SessionState::Active);
    }

    #[test]
    fn old_schema_deserializes_as_active() {
        let json = r#"[
          {
            "session_id": "abc",
            "tool": "claude",
            "pid": 42,
            "directory": "/tmp/project",
            "session_name": null,
            "registered_at": "2026-05-17T10:30:00Z"
          }
        ]"#;

        let sessions: Vec<SessionRecord> = serde_json::from_str(json).unwrap();
        assert_eq!(sessions[0].pid, Some(42));
        assert_eq!(sessions[0].state, SessionState::Active);
        assert!(sessions[0].transcript_path.is_none());
    }
}
