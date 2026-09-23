use crate::sessions::{SessionRecord, SessionState};
use crate::{paths, process, sessions};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::io::{self, BufRead, Read};

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
    let mut contents = String::new();
    io::stdin().read_to_string(&mut contents)?;
    let event: Event = crate::commands::register::parse_hook_payload(&contents)?;
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

pub fn activity_was_working(record: &SessionRecord) -> bool {
    record.activity.as_ref().is_some_and(|activity| {
        activity.state == WorkState::Working
            && activity.pending_tools.is_empty()
            && !activity.needs_attention
            && Some(activity.owner_pid) == record.pid
            && Some(activity.owner_started_at.as_str()) == record.pid_started_at.as_deref()
    })
}

/// Claude deletes its status file as it exits, before its `SessionEnd` hook
/// runs, so the daemon records whether it is mid-turn while it is alive.
pub fn claude_activity(record: &SessionRecord) -> Option<Activity> {
    let pid = record.pid?;
    let file = std::fs::File::open(
        paths::tool_home(record.tool)
            .ok()?
            .join("sessions")
            .join(format!("{pid}.json")),
    )
    .ok()?;
    let native = serde_json::from_reader::<_, serde_json::Value>(file).ok()?;
    claude_native_activity(record, &native)
}

fn claude_native_activity(record: &SessionRecord, native: &serde_json::Value) -> Option<Activity> {
    Some(Activity {
        state: if native_claude_matches(record, native) {
            WorkState::Working
        } else {
            WorkState::Idle
        },
        turn_id: String::new(),
        owner_pid: record.pid?,
        owner_started_at: record.pid_started_at.clone()?,
        pending_tools: BTreeSet::new(),
        needs_attention: false,
    })
}

pub fn observe(processes: &process::ProcessSnapshot) -> Result<()> {
    let path = paths::sessions_file()?;
    let changes: Vec<(String, Activity)> = sessions::read_sessions(&path)?
        .iter()
        .filter(|record| {
            record.state == SessionState::Active
                && crate::commands::daemon::session_tool_is_alive(record, processes)
        })
        .filter_map(|record| {
            let activity = record.tool.spec().observe_activity?(record)?;
            (record.activity.as_ref() != Some(&activity))
                .then(|| (record.session_id.clone(), activity))
        })
        .collect();
    if changes.is_empty() {
        return Ok(());
    }
    sessions::with_sessions_mut(&path, |records| {
        for (session_id, activity) in changes {
            if let Some(record) = records.iter_mut().find(|record| {
                record.session_id == session_id && record.state == SessionState::Active
            }) {
                record.activity = Some(activity);
            }
        }
        Ok(())
    })
}

pub fn codex_was_working(record: &SessionRecord) -> bool {
    if !activity_was_working(record) {
        return false;
    }
    let Some(path) = &record.transcript_path else {
        return false;
    };
    let Ok(file) = std::fs::File::open(path) else {
        return false;
    };
    let mut own_cli = false;
    let mut active_turn = None;
    for line in io::BufReader::new(file).lines() {
        let Ok(line) = line else {
            return false;
        };
        let Ok(event) = serde_json::from_str::<serde_json::Value>(&line) else {
            return false;
        };
        let payload = &event["payload"];
        if event["type"] == "session_meta" && payload["id"] == record.session_id {
            own_cli = payload["source"] == "cli";
        }
        if event["type"] == "event_msg" {
            match payload["type"].as_str() {
                Some("task_started") => {
                    active_turn = payload["turn_id"].as_str().map(str::to_string);
                }
                Some("task_complete" | "turn_aborted" | "error") => active_turn = None,
                _ => {}
            }
        }
    }
    own_cli
        && active_turn.as_deref()
            == record
                .activity
                .as_ref()
                .map(|activity| activity.turn_id.as_str())
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
        let mut record = record("grok");
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
    fn a_busy_claude_observed_alive_continues_after_it_dies() {
        let mut record = record("claude");
        record.activity = None;
        let busy = json!({"sessionId":"session","pid":42,"procStart":"Wed Jan 1 00:00:00 2020",
            "entrypoint":"cli","kind":"interactive","status":"busy"});
        record.activity = claude_native_activity(&record, &busy);
        assert!(should_continue(&record));
        let mut idle = busy;
        idle["status"] = json!("idle");
        record.activity = claude_native_activity(&record, &idle);
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
            let event: Event = crate::commands::register::parse_hook_payload(
                &json!({"sessionId":"session","session_id":"session","hook_event_name":event,
                "hookEventName":"irrelevant-native-alias","promptId":"turn","toolUseId":"tool",
                "tool_use_id":"tool","message":"Working. Continue. Waiting. Idle.",
                "prompt":"Anything"})
                .to_string(),
            )
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

    #[test]
    fn native_codex_terminal_events_override_stale_working_hooks() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("rollout.jsonl");
        let mut record = record("codex");
        record.transcript_path = Some(path.clone());
        record.activity = Some(Activity {
            state: WorkState::Working,
            turn_id: "turn".into(),
            owner_pid: 42,
            owner_started_at: record.pid_started_at.clone().unwrap(),
            pending_tools: BTreeSet::new(),
            needs_attention: false,
        });
        let prefix = format!(
            "{}\n{}\n",
            json!({"type":"session_meta","payload":{"id":"session","source":"cli"}}),
            json!({"type":"event_msg","payload":{"type":"task_started","turn_id":"turn"}})
        );
        std::fs::write(&path, &prefix).unwrap();
        assert!(should_continue(&record));
        for event in ["task_complete", "turn_aborted", "error"] {
            std::fs::write(
                &path,
                format!(
                    "{prefix}{}\n",
                    json!({"type":"event_msg","payload":{"type":event,"turn_id":"turn"}})
                ),
            )
            .unwrap();
            assert!(!should_continue(&record), "{event}");
        }
        std::fs::write(&path, format!("{prefix}{{")).unwrap();
        assert!(!should_continue(&record));
        std::fs::write(&path, prefix.replace("\"cli\"", "\"exec\"")).unwrap();
        assert!(!should_continue(&record));
    }
}
