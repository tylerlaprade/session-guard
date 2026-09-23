use crate::sessions::{SessionRecord, SessionState};
use crate::{paths, process, sessions};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::io;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WorkState {
    #[default]
    Unknown,
    Working,
    Waiting,
    Idle,
    Stopped,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Activity {
    pub state: WorkState,
    pub turn_id: String,
    pub owner_pid: i32,
    pub owner_started_at: String,
    pub pending_tools: BTreeSet<String>,
    pub needs_attention: bool,
}

#[derive(Deserialize)]
struct Event {
    #[serde(alias = "sessionId")]
    session_id: String,
    hook_event_name: String,
    #[serde(default, alias = "promptId")]
    turn_id: Option<String>,
    #[serde(default, alias = "toolUseId")]
    tool_use_id: Option<String>,
    #[serde(default, alias = "notificationType")]
    notification_type: Option<String>,
    #[serde(default, alias = "subagentType")]
    agent_type: Option<String>,
    #[serde(default)]
    agent_id: Option<String>,
    #[serde(default, alias = "transcriptPath")]
    transcript_path: Option<std::path::PathBuf>,
}

pub fn record_hook() -> Result<()> {
    let event: Event = serde_json::from_reader(io::stdin())?;
    if event.agent_type.is_some() || event.agent_id.is_some() {
        return Ok(());
    }
    let tree = process::list_processes()?;
    let processes = process::ProcessSnapshot::capture()?;
    sessions::with_sessions_mut(&paths::sessions_file()?, |records| {
        let Some(record) = records
            .iter_mut()
            .find(|record| record.session_id == event.session_id)
        else {
            return Ok(());
        };
        let (Some(pid), Some(started)) = (record.pid, record.pid_started_at.as_deref()) else {
            return Ok(());
        };
        if !processes.identity_matches(pid, started) {
            return Ok(());
        }
        let mut ancestor = i32::try_from(std::process::id())?;
        for _ in 0..tree.len() {
            if ancestor == pid {
                break;
            }
            ancestor = tree
                .iter()
                .find(|row| row.pid == ancestor)
                .map_or(0, |row| row.ppid);
            if ancestor <= 1 {
                return Ok(());
            }
        }
        if ancestor != pid || record.transcript_path != event.transcript_path {
            return Ok(());
        }
        apply_event(record, &event);
        Ok(())
    })
}

fn apply_event(record: &mut SessionRecord, event: &Event) {
    if event.hook_event_name == "UserPromptSubmit" {
        record.activity = event
            .turn_id
            .as_ref()
            .filter(|id| !id.is_empty())
            .map(|id| Activity {
                state: WorkState::Unknown,
                turn_id: id.clone(),
                owner_pid: record.pid.unwrap_or_default(),
                owner_started_at: record.pid_started_at.clone().unwrap_or_default(),
                pending_tools: BTreeSet::new(),
                needs_attention: false,
            });
        return;
    }
    let Some(activity) = &mut record.activity else {
        return;
    };
    if event
        .turn_id
        .as_ref()
        .is_some_and(|id| id != &activity.turn_id)
    {
        return;
    }
    if event.turn_id.is_none()
        && matches!(
            event.hook_event_name.as_str(),
            "PreToolUse" | "PostToolUse" | "PostToolUseFailure"
        )
    {
        activity.state = WorkState::Unknown;
        activity.needs_attention = true;
        return;
    }
    match event.hook_event_name.as_str() {
        "Stop" => activity.state = WorkState::Idle,
        "StopFailure" | "StopCancelled" | "Interrupt" => activity.state = WorkState::Stopped,
        "PermissionRequest" | "Notification" => {
            if event.notification_type.as_deref() == Some("idle_prompt") {
                activity.state = WorkState::Idle;
            } else {
                activity.needs_attention = true;
                activity.state = WorkState::Waiting;
            }
        }
        "PreToolUse" if !matches!(activity.state, WorkState::Idle | WorkState::Stopped) => {
            if let Some(id) = &event.tool_use_id {
                activity.pending_tools.insert(id.clone());
            } else {
                activity.needs_attention = true;
            }
            activity.state = WorkState::Waiting;
        }
        "PostToolUse" | "PostToolUseFailure"
            if !matches!(activity.state, WorkState::Idle | WorkState::Stopped)
                && event
                    .tool_use_id
                    .as_ref()
                    .is_some_and(|id| activity.pending_tools.remove(id))
                && activity.pending_tools.is_empty()
                && !activity.needs_attention =>
        {
            activity.state = WorkState::Working;
        }
        _ => {}
    }
}

pub fn should_continue(record: &SessionRecord) -> bool {
    (record.restore_pending || record.state != SessionState::Recoverable)
        && record
            .tool
            .spec()
            .was_working
            .is_some_and(|check| check(record))
}

pub fn hook_was_working(record: &SessionRecord) -> bool {
    record.activity.as_ref().is_some_and(|activity| {
        activity.state == WorkState::Working
            && activity.pending_tools.is_empty()
            && !activity.needs_attention
            && Some(activity.owner_pid) == record.pid
            && Some(activity.owner_started_at.as_str()) == record.pid_started_at.as_deref()
    })
}

pub fn claude_was_working(record: &SessionRecord) -> bool {
    let Some(pid) = record.pid else {
        return false;
    };
    let Ok(home) = paths::tool_home(record.tool) else {
        return false;
    };
    let Ok(file) = std::fs::File::open(home.join("sessions").join(format!("{pid}.json"))) else {
        return false;
    };
    let Ok(native) = serde_json::from_reader::<_, serde_json::Value>(file) else {
        return false;
    };
    native_claude_matches(record, &native)
}

fn native_claude_matches(record: &SessionRecord, native: &serde_json::Value) -> bool {
    let started = record
        .pid_started_at
        .as_deref()
        .and_then(process::parse_identity);
    started.is_some()
        && started
            == native["procStart"]
                .as_str()
                .and_then(process::parse_identity)
        && native["sessionId"].as_str() == Some(record.session_id.as_str())
        && native["pid"].as_i64() == record.pid.map(i64::from)
        && native["kind"].as_str() == Some("interactive")
        && native["entrypoint"].as_str() == Some("cli")
        && native["status"].as_str() == Some("busy")
        && native
            .get("waitingFor")
            .is_none_or(serde_json::Value::is_null)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn record(tool: &str) -> SessionRecord {
        let mut record = SessionRecord::new(
            crate::tool(tool),
            "session".into(),
            None,
            None,
            "/tmp".into(),
            None,
            None,
            None,
        );
        record.pid = Some(42);
        record.pid_started_at = Some("Wed Jan 1 00:00:00 2020".into());
        record.mark_recoverable();
        record
    }

    #[test]
    fn native_claude_status_requires_exact_session_and_process_identity() {
        let record = record("claude");
        let native = json!({"sessionId":"session","pid":42,"procStart":"Wed Jan 1 00:00:00 2020",
            "entrypoint":"cli","kind":"interactive","status":"busy"});
        assert!(native_claude_matches(&record, &native));
        for (key, value) in [
            ("sessionId", json!("another-session")),
            ("pid", json!(43)),
            ("procStart", json!("Thu Jan 2 00:00:00 2020")),
            ("procStart", json!(null)),
            ("entrypoint", json!("sdk")),
            ("kind", json!("worker")),
            ("status", json!("idle")),
            ("status", json!("waiting")),
            ("status", json!("shell")),
            ("status", json!("new-unknown-status")),
            ("waitingFor", json!("input needed")),
        ] {
            let mut changed = native.clone();
            changed[key] = value;
            assert!(
                !native_claude_matches(&record, &changed),
                "{key}: {changed}"
            );
        }
    }

    #[test]
    fn stale_owners_and_legacy_records_never_continue() {
        let mut record = record("codex");
        record.activity = Some(Activity {
            state: WorkState::Working,
            turn_id: "turn".into(),
            owner_pid: 42,
            owner_started_at: record.pid_started_at.clone().unwrap(),
            pending_tools: BTreeSet::new(),
            needs_attention: false,
        });
        assert!(should_continue(&record));
        record.restore_pending = false;
        assert!(!should_continue(&record));
        record.restore_pending = true;
        record.pid = Some(43);
        assert!(!should_continue(&record));
        record.pid = Some(42);
        record.activity.as_mut().unwrap().needs_attention = true;
        assert!(!should_continue(&record));
    }

    #[test]
    fn grok_metadata_is_parsed_without_reading_message_content() {
        let mut record = record("grok");
        for (event, state) in [
            ("UserPromptSubmit", WorkState::Unknown),
            ("PreToolUse", WorkState::Waiting),
            ("PostToolUse", WorkState::Working),
            ("StopCancelled", WorkState::Stopped),
        ] {
            let event: Event =
                serde_json::from_value(json!({"sessionId":"session","hook_event_name":event,
                "hookEventName":"irrelevant-native-alias","promptId":"turn","toolUseId":"tool",
                "message":"Working. Continue. Waiting. Idle.","prompt":"Anything"}))
                .unwrap();
            apply_event(&mut record, &event);
            assert_eq!(record.activity.as_ref().unwrap().state, state);
        }
    }

    #[test]
    fn uncorrelated_tool_completion_cannot_confirm_work() {
        let mut record = record("codex");
        for (name, turn) in [
            ("UserPromptSubmit", Some("turn")),
            ("PreToolUse", Some("turn")),
            ("PostToolUse", None),
        ] {
            let event: Event =
                serde_json::from_value(json!({"session_id":"session", "hook_event_name":name,
                "turn_id":turn,"tool_use_id":"tool"}))
                .unwrap();
            apply_event(&mut record, &event);
        }
        assert!(!should_continue(&record));
        assert_eq!(record.activity.unwrap().state, WorkState::Unknown);
    }
}
