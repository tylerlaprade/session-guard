use crate::sessions::{SessionRecord, SessionState, UnknownFields};
use crate::{paths, process, sessions};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::io::{self, BufRead, Read, Seek};

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
    #[serde(flatten)]
    pub unknown_fields: UnknownFields,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackgroundTask {
    pub id: String,
    #[serde(default, rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub description: String,
    #[serde(flatten)]
    pub unknown_fields: UnknownFields,
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
    if event.hook_event_name == "UserPromptSubmit" {
        crate::commands::register::admit_prompted(&contents)?;
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
                unknown_fields: UnknownFields::new(),
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
    was_interrupted(record)
        && record
            .tool
            .spec()
            .was_working
            .is_some_and(|check| check(record))
}

fn was_interrupted(record: &SessionRecord) -> bool {
    record.restore_pending || record.state != SessionState::Recoverable
}

/// The prompt a restore sends, taken from the record along with the state it
/// came from, so no later restore sends it again.
pub fn take_restore_prompt(record: &mut SessionRecord) -> Option<String> {
    let prompt = restore_prompt(record);
    record.mark_active();
    record.activity = None;
    record.background_tasks = None;
    prompt
}

pub fn restore_prompt(record: &SessionRecord) -> Option<String> {
    if let Some(message) = record
        .tool
        .spec()
        .lost_message
        .filter(|_| was_interrupted(record))
        .and_then(|lost| lost(record))
    {
        return Some(message);
    }
    if should_continue(record) {
        return Some("continue".to_string());
    }
    let tasks = record
        .background_tasks
        .as_ref()
        .filter(|tasks| !tasks.is_empty() && was_interrupted(record))?;
    let names: Vec<String> = tasks
        .iter()
        .map(|task| {
            let name = if task.description.is_empty() {
                task.id.as_str()
            } else {
                task.description.as_str()
            };
            if task.kind.is_empty() {
                name.to_string()
            } else {
                format!("{name} ({})", task.kind)
            }
        })
        .collect();
    let resumable = |task: &BackgroundTask| matches!(task.kind.as_str(), "subagent" | "workflow");
    let advice = match (tasks.iter().any(resumable), tasks.iter().all(resumable)) {
        (false, _) => "Check what each one finished, and rerun any that still need to run.",
        (true, true) => {
            "Check what each one finished, and resume any that still need to run where it left \
             off rather than starting it over."
        }
        (true, false) => {
            "Check what each one finished. Resume a subagent or workflow where it left off \
             rather than starting it over, and rerun anything else only if it still needs to run."
        }
    };
    Some(format!(
        "Background tasks stopped when this session was interrupted: {}. {advice}",
        names.join("; ")
    ))
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

pub fn claude_was_working(record: &SessionRecord) -> bool {
    activity_was_working(record) || claude_owes_a_reply(record)
}

// Claude saves a message before its turn starts, so a crash inside the
// daemon's one-second busy sample leaves the message last in the transcript.
// A resume answers it with a synthetic "No response requested." placeholder,
// which the model then takes for its own reply.
fn claude_owes_a_reply(record: &SessionRecord) -> bool {
    record
        .transcript_path
        .as_ref()
        .and_then(|path| judge_backward(path, claude_reply_verdict))
        .unwrap_or(false)
}

/// The first verdict `judge` reaches on a JSON-lines file read from its end,
/// so a long file costs only the lines after the deciding one.
fn judge_backward<T>(
    path: &std::path::Path,
    judge: impl Fn(&serde_json::Value) -> Option<T>,
) -> Option<T> {
    let mut file = std::fs::File::open(path).ok()?;
    let length = file.metadata().ok()?.len();
    let mut window: u64 = 64 * 1024;
    loop {
        let start = length.saturating_sub(window);
        let mut tail = Vec::new();
        file.seek(io::SeekFrom::Start(start)).ok()?;
        file.read_to_end(&mut tail).ok()?;
        let mut lines = tail.split(|byte| *byte == b'\n').rev().collect::<Vec<_>>();
        if start > 0 {
            lines.pop();
        }
        for line in lines {
            // A crash can cut the last line short.
            let Ok(entry) = serde_json::from_slice::<serde_json::Value>(line) else {
                continue;
            };
            if let Some(verdict) = judge(&entry) {
                return Some(verdict);
            }
        }
        if start == 0 {
            return None;
        }
        window *= 2;
    }
}

// Claude adds a message to its prompt history as it is sent and saves it to
// the transcript a moment later, so a crash in between leaves it only in the
// history: a resume has never seen it, and only sending it again recovers it.
pub fn claude_lost_message(record: &SessionRecord) -> Option<String> {
    claude_lost_message_in(record, &paths::tool_home(record.tool).ok()?)
}

fn claude_lost_message_in(record: &SessionRecord, home: &std::path::Path) -> Option<String> {
    let saved_until = judge_backward(record.transcript_path.as_ref()?, |entry| {
        chrono::DateTime::parse_from_rfc3339(entry["timestamp"].as_str()?)
            .ok()
            .map(|saved| saved.timestamp_millis())
    })?;
    let entry = judge_backward(&home.join("history.jsonl"), |entry| {
        if entry["timestamp"].as_i64()? <= saved_until {
            return Some(None);
        }
        (entry["sessionId"] == record.session_id.as_str()).then(|| Some(entry.clone()))
    })??;
    let display = entry["display"].as_str()?;
    if display.starts_with('/') || display.starts_with('!') {
        return None;
    }
    let pieces = claude_message_pieces(display, &entry["pastedContents"], home);
    // The transcript saves a message within moments of its history entry but
    // writes nothing more until the reply starts.
    let last_message = judge_backward(record.transcript_path.as_ref()?, |entry| {
        let skipped = entry["isMeta"] == true || entry["isCompactSummary"] == true;
        (entry["type"] == "user" && !skipped)
            .then(|| claude_message_text(&entry["message"]["content"]))
    })?;
    if last_message.is_some_and(|saved| saved_as(&pieces, &saved)) {
        return None;
    }
    let text: String = pieces
        .iter()
        .map(|piece| match piece {
            MessagePiece::Literal(text)
            | MessagePiece::Pasted(text)
            | MessagePiece::Missing(text) => text.as_str(),
        })
        .collect();
    let lead = if display.contains("[Image #") {
        "A crash cut this message off before it reached you, and its images are lost:"
    } else {
        "A crash cut this message off before it reached you:"
    };
    Some(format!("{lead}\n\n{text}"))
}

fn claude_message_text(content: &serde_json::Value) -> Option<String> {
    match content {
        serde_json::Value::String(text) => Some(text.clone()),
        serde_json::Value::Array(blocks) => {
            let texts: Vec<&str> = blocks
                .iter()
                .filter_map(|block| block["text"].as_str())
                .collect();
            (!texts.is_empty()).then(|| texts.join("\n"))
        }
        _ => None,
    }
}

/// A history entry's text, split at the paste and image markers that the
/// transcript expands in place.
enum MessagePiece {
    Literal(String),
    Pasted(String),
    /// A marker whose content the history does not keep, such as an image.
    Missing(String),
}

fn claude_message_pieces(
    display: &str,
    pastes: &serde_json::Value,
    home: &std::path::Path,
) -> Vec<MessagePiece> {
    let mut pieces = Vec::new();
    let mut remaining = display;
    while let Some((at, end)) = ["[Pasted text #", "[Image #"]
        .iter()
        .filter_map(|marker| remaining.find(marker))
        .min()
        .and_then(|at| Some((at, at + remaining[at..].find(']')? + 1)))
    {
        pieces.push(MessagePiece::Literal(remaining[..at].to_string()));
        let marker = &remaining[at..end];
        let paste = marker
            .strip_prefix("[Pasted text #")
            .and_then(|rest| {
                rest.split(|character: char| !character.is_ascii_digit())
                    .next()
            })
            .map(|id| &pastes[id]);
        let content = paste.and_then(|paste| {
            paste["content"].as_str().map(str::to_string).or_else(|| {
                let hash = paste["contentHash"].as_str()?;
                std::fs::read_to_string(home.join("paste-cache").join(format!("{hash}.txt"))).ok()
            })
        });
        pieces.push(content.map_or_else(
            || MessagePiece::Missing(marker.to_string()),
            MessagePiece::Pasted,
        ));
        remaining = &remaining[end..];
    }
    pieces.push(MessagePiece::Literal(remaining.to_string()));
    pieces
}

/// Whether `saved` is the message the pieces describe: its literal text at
/// both ends, with the pasted text and inner literals in order between.
fn saved_as(pieces: &[MessagePiece], saved: &str) -> bool {
    let [
        MessagePiece::Literal(first),
        middle @ ..,
        MessagePiece::Literal(last),
    ] = pieces
    else {
        return matches!(pieces, [MessagePiece::Literal(only)] if saved == only);
    };
    let Some(mut rest) = saved.strip_prefix(first.as_str()) else {
        return false;
    };
    for piece in middle {
        let (MessagePiece::Literal(text) | MessagePiece::Pasted(text)) = piece else {
            continue;
        };
        let Some(at) = rest.find(text.as_str()) else {
            return false;
        };
        rest = &rest[at + text.len()..];
    }
    rest.ends_with(last.as_str())
}

fn claude_reply_verdict(entry: &serde_json::Value) -> Option<bool> {
    if entry["isMeta"] == true || entry["isCompactSummary"] == true || entry["isSidechain"] == true
    {
        return None;
    }
    match entry["type"].as_str() {
        Some("user") => Some(claude_message_wants_reply(&entry["message"]["content"])),
        Some("assistant")
            if entry["message"]["model"] != "<synthetic>" || entry["isApiErrorMessage"] == true =>
        {
            Some(false)
        }
        _ => None,
    }
}

fn claude_message_wants_reply(content: &serde_json::Value) -> bool {
    match content {
        serde_json::Value::String(text) => !text.starts_with("<local-command-"),
        serde_json::Value::Array(blocks) => !blocks.iter().any(|block| {
            block["text"]
                .as_str()
                .is_some_and(|text| text.starts_with("[Request interrupted by user"))
        }),
        _ => false,
    }
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
        unknown_fields: UnknownFields::new(),
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

// A thread server, not the tab, runs a served thread's turn: closing the tab
// does not stop it, and Codex's daemon resumes the turns it was running when
// it shuts down itself.
pub fn codex_was_working(record: &SessionRecord) -> bool {
    if record.server_pid.is_some() || !activity_was_working(record) {
        return false;
    }
    let Some(path) = &record.transcript_path else {
        return false;
    };
    let Ok(file) = std::fs::File::open(path) else {
        return false;
    };
    let mut own_terminal = false;
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
            own_terminal = crate::transcripts::codex_meta_has_terminal_frontend(payload);
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
    own_terminal
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
            unknown_fields: UnknownFields::new(),
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
    fn a_claude_message_left_without_a_reply_continues_unsampled() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let mut record = record("claude");
        record.activity = None;
        record.transcript_path = Some(path.clone());
        let prompt = json!({"type":"user","message":{"role":"user","content":"Name a fish."}});
        let reply = json!({"type":"assistant","message":{"model":"claude-opus-5-5",
            "content":[{"type":"text","text":"Marlin."}]}});
        let placeholder = json!({"type":"assistant","isApiErrorMessage":false,"message":{
            "model":"<synthetic>","content":[{"type":"text","text":"No response requested."}]}});
        let api_error = json!({"type":"assistant","isApiErrorMessage":true,"message":{
            "model":"<synthetic>","content":[{"type":"text","text":"API Error"}]}});
        let tool_result = json!({"type":"user","message":{"role":"user",
            "content":[{"type":"tool_result","tool_use_id":"tool","content":"done"}]}});
        let interrupted = json!({"type":"user","message":{"role":"user",
            "content":[{"type":"text","text":"[Request interrupted by user]"}]}});
        let command = json!({"type":"user","message":{"role":"user",
            "content":"<command-name>/model</command-name>"}});
        let command_output = json!({"type":"user","message":{"role":"user",
            "content":"<local-command-stdout>Set model</local-command-stdout>"}});
        let meta =
            json!({"type":"user","isMeta":true,"message":{"role":"user","content":"Caveat"}});
        let turn_end = json!({"type":"system","subtype":"turn_duration"});
        let large = json!({"type":"attachment","content":"x".repeat(300 * 1024)});
        let owed = |record: &SessionRecord, entries: &[&serde_json::Value], tail: &str| {
            let mut lines: Vec<String> = entries.iter().map(ToString::to_string).collect();
            lines.push(tail.to_string());
            std::fs::write(&path, lines.join("\n")).unwrap();
            should_continue(record)
        };
        assert!(!owed(&record, &[&prompt, &reply, &turn_end], ""));
        assert!(owed(&record, &[&prompt, &reply, &prompt], ""));
        assert!(owed(&record, &[&prompt, &placeholder, &meta], ""));
        assert!(owed(&record, &[&prompt], r#"{"type":"assistant","mess"#));
        assert!(owed(&record, &[&prompt, &reply, &tool_result], ""));
        assert!(owed(&record, &[&reply, &prompt, &large, &large], ""));
        assert!(!owed(&record, &[&prompt, &reply, &large, &large], ""));
        assert!(!owed(&record, &[&prompt, &api_error], ""));
        assert!(!owed(&record, &[&prompt, &interrupted], ""));
        assert!(!owed(&record, &[&command, &command_output], ""));
        record.restore_pending = false;
        assert!(!owed(&record, &[&prompt], ""));
    }

    #[test]
    fn only_a_message_sent_after_the_transcript_last_saved_is_lost() {
        let dir = tempfile::tempdir().unwrap();
        let transcript = dir.path().join("session.jsonl");
        let mut record = record("claude");
        record.transcript_path = Some(transcript.clone());
        let saved = 1_577_836_805_000_i64;
        let lost = |display: &str, pastes: serde_json::Value, sent: i64, last_saved: &str| {
            let mut lines = vec![json!({"type":"user","timestamp":"2020-01-01T00:00:00.000Z",
                "message":{"role":"user","content":last_saved}})];
            lines.push(json!({"type":"assistant","timestamp":"2020-01-01T00:00:05.000Z"}));
            std::fs::write(&transcript, format!("{}\n{}\n", lines[0], lines[1])).unwrap();
            std::fs::write(
                dir.path().join("history.jsonl"),
                format!(
                    "{}\n",
                    json!({"display":display,"pastedContents":pastes,"sessionId":"session",
                        "timestamp":sent})
                ),
            )
            .unwrap();
            claude_lost_message_in(&record, dir.path())
        };
        let lead = "A crash cut this message off before it reached you:";
        assert_eq!(
            lost("ok", json!({}), saved + 1, "is it ok?").as_deref(),
            Some(format!("{lead}\n\nok").as_str())
        );
        assert_eq!(lost("ok", json!({}), saved + 1, "ok"), None);
        assert_eq!(lost("ok", json!({}), saved, "something else"), None);
        let paste = json!({"1":{"id":1,"type":"text","content":"the log"}});
        let wrapped =
            "Read \n\n<pasted_content id=\"d4a6\">\nthe log\n</pasted_content id=\"d4a6\">\n now";
        assert_eq!(
            lost(
                "Read [Pasted text #1 +2 lines] now",
                paste.clone(),
                saved + 1,
                wrapped
            ),
            None
        );
        assert_eq!(
            lost(
                "Read [Pasted text #1 +2 lines] now",
                paste.clone(),
                saved + 1,
                "Read now"
            )
            .as_deref(),
            Some(format!("{lead}\n\nRead the log now").as_str())
        );
        assert_eq!(
            lost("[Pasted text #1 +2 lines]", paste, saved + 1, "unrelated").as_deref(),
            Some(format!("{lead}\n\nthe log").as_str())
        );
        assert_eq!(
            lost("What is [Image #1]?", json!({}), saved + 1, "unrelated").as_deref(),
            Some(
                "A crash cut this message off before it reached you, and its images are \
                 lost:\n\nWhat is [Image #1]?"
            )
        );
        assert_eq!(lost("/model", json!({}), saved + 1, "unrelated"), None);
        assert_eq!(lost("!git status", json!({}), saved + 1, "unrelated"), None);
    }

    #[test]
    fn an_idle_restore_names_the_background_tasks_it_lost() {
        let mut record = record("claude");
        record.background_tasks = Some(vec![
            BackgroundTask {
                id: "b5hzqrsid".into(),
                kind: "shell".into(),
                description: "QA deploy".into(),
                unknown_fields: UnknownFields::new(),
            },
            BackgroundTask {
                id: "a4505039".into(),
                kind: "subagent".into(),
                description: String::new(),
                unknown_fields: UnknownFields::new(),
            },
        ]);
        assert_eq!(
            restore_prompt(&record).as_deref(),
            Some(
                "Background tasks stopped when this session was interrupted: QA deploy (shell); \
                 a4505039 (subagent). Check what each one finished. Resume a subagent or \
                 workflow where it left off rather than starting it over, and rerun anything \
                 else only if it still needs to run."
            )
        );
        let lost = record.background_tasks.clone();
        record.background_tasks = lost.as_ref().map(|tasks| tasks[..1].to_vec());
        assert_eq!(
            restore_prompt(&record).as_deref(),
            Some(
                "Background tasks stopped when this session was interrupted: QA deploy (shell). \
                 Check what each one finished, and rerun any that still need to run."
            )
        );
        record.background_tasks = lost.as_ref().map(|tasks| tasks[1..].to_vec());
        assert_eq!(
            restore_prompt(&record).as_deref(),
            Some(
                "Background tasks stopped when this session was interrupted: a4505039 \
                 (subagent). Check what each one finished, and resume any that still need to \
                 run where it left off rather than starting it over."
            )
        );
        record.background_tasks = lost;
        record.activity = Some(Activity {
            state: WorkState::Working,
            turn_id: String::new(),
            owner_pid: 42,
            owner_started_at: record.pid_started_at.clone().unwrap(),
            pending_tools: BTreeSet::new(),
            needs_attention: false,
            unknown_fields: UnknownFields::new(),
        });
        assert_eq!(restore_prompt(&record).as_deref(), Some("continue"));
        record.activity = None;
        record.restore_pending = false;
        assert_eq!(restore_prompt(&record), None);
        record.restore_pending = true;
        record.background_tasks = Some(Vec::new());
        assert_eq!(restore_prompt(&record), None);
        record.background_tasks = None;
        assert_eq!(restore_prompt(&record), None);
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
            unknown_fields: UnknownFields::new(),
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

        let in_process = prefix.replace(
            "\"source\":\"cli\"",
            "\"source\":\"vscode\",\"originator\":\"codex-tui\"",
        );
        std::fs::write(&path, &in_process).unwrap();
        assert!(should_continue(&record));
        record.server_pid = Some(7);
        assert!(!should_continue(&record));
    }
}
