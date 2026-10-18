use crate::Tool;
use crate::harness::{Discovery, ServedThread, ThreadServer};
use crate::sessions::{self, SessionRecord, SessionState};
use crate::{paths, process};
use anyhow::{Context, Result};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use std::io::{self, IsTerminal, Read};
use std::path::{Path, PathBuf};

#[derive(Debug, Deserialize)]
struct HookInput {
    // Claude/Codex use snake_case; Grok uses camelCase on the same fields.
    #[serde(default, alias = "sessionId")]
    session_id: Option<String>,
    cwd: Option<PathBuf>,
    // Grok 1.0.13+ names the workspace `workspaceRoot` on every event.
    #[serde(default, alias = "workspaceRoot")]
    workspace_root: Option<PathBuf>,
    #[serde(default, alias = "transcriptPath")]
    transcript_path: Option<PathBuf>,
    source: Option<String>,
    name: Option<String>,
    #[serde(default)]
    background_tasks: Option<Vec<crate::continuation::BackgroundTask>>,
    #[serde(default, alias = "hookEventName")]
    hook_event_name: Option<String>,
}

const SESSION_ID_ENVS: &[&str] = &[
    "GROK_SESSION_ID",
    "CLAUDE_CODE_SESSION_ID",
    "CODEX_THREAD_ID",
];
const DIRECTORY_ENVS: &[&str] = &["GROK_WORKSPACE_ROOT", "CLAUDE_PROJECT_DIR"];

fn first_env(names: &[&str]) -> Option<String> {
    names
        .iter()
        .find_map(|name| std::env::var(name).ok().filter(|value| !value.is_empty()))
}

pub fn admit_prompted(payload: &str) -> Result<()> {
    let input: HookInput = parse_hook_payload(payload)?;
    let (Some(session_id), Some(transcript)) = (
        input.session_id.as_deref(),
        input.transcript_path.as_deref(),
    ) else {
        return Ok(());
    };
    let Some(emitter) = hook_emitter(i32::try_from(std::process::id())?) else {
        return Ok(());
    };
    let Some(tool) = Tool::all().find(|tool| {
        tool.spec()
            .thread_server
            .as_ref()
            .is_some_and(|server| (server.is_server)(emitter))
            && crate::transcripts::holds_transcript(*tool, transcript)
    }) else {
        return Ok(());
    };
    let emitter_started = process::process_start_identity(emitter).ok();
    if sessions::read_sessions(&paths::sessions_file()?)?
        .iter()
        .any(|known| {
            known.session_id == session_id
                && known.state == SessionState::Active
                && known.server_pid == Some(emitter)
                && known.server_pid_started_at.is_some()
                && known.server_pid_started_at == emitter_started
                && known.pid.is_some_and(|pid| {
                    known.pid_started_at.is_some()
                        && process::process_start_identity(pid).ok() == known.pid_started_at
                })
        })
    {
        return Ok(());
    }
    run_with_input(tool, None, Some(emitter), None, None, None, Some(input))
}

// Codex starts each hook in a separate session beneath its app-server.
pub(crate) fn hook_emitter(process: i32) -> Option<i32> {
    process::parent_pid(process::session_leader(process)?)
}

pub fn run(
    tool: Tool,
    session_id: Option<String>,
    pid: Option<i32>,
    shell_pid: Option<i32>,
    directory: Option<PathBuf>,
    name: Option<String>,
) -> Result<()> {
    let needs_stdin = session_id.is_none() || directory.is_none();
    let hook_input = if needs_stdin {
        read_hook_input()?
    } else {
        None
    };

    run_with_input(
        tool, session_id, pid, shell_pid, directory, name, hook_input,
    )
}

fn run_with_input(
    tool: Tool,
    session_id: Option<String>,
    pid: Option<i32>,
    shell_pid: Option<i32>,
    directory: Option<PathBuf>,
    name: Option<String>,
    hook_input: Option<HookInput>,
) -> Result<()> {
    // An explicit --session-id is an operator decision; only hook-driven
    // registrations get the Codex frontend filter below (a hook cannot say
    // which frontend owns the thread, an operator can).
    let explicit_session = session_id.is_some();
    let session_id = session_id
        .or_else(|| {
            hook_input
                .as_ref()
                .and_then(|input| input.session_id.clone())
        })
        .or_else(|| first_env(SESSION_ID_ENVS))
        .context(
            "missing session id; pass --session-id or run from a hook that provides session_id",
        )?;
    if !explicit_session && crate::scan::is_unused_spare(tool, &session_id) {
        return Ok(());
    }
    let directory = directory
        .or_else(|| hook_input.as_ref().and_then(|input| input.cwd.clone()))
        .or_else(|| {
            hook_input
                .as_ref()
                .and_then(|input| input.workspace_root.clone())
        })
        .or_else(|| first_env(DIRECTORY_ENVS).map(PathBuf::from))
        .context("missing directory; pass --directory or run from a hook that provides cwd")?;

    // The tools fire their session hooks for internal activity too — e.g. codex
    // memory maintenance running under ~/.codex. Those aren't user project
    // sessions and would restore as junk tabs cd'd into a config dir, so never
    // record them.
    if is_internal_directory(&directory, &paths::tool_home(tool)?) {
        return Ok(());
    }

    let transcript_path = hook_input
        .as_ref()
        .and_then(|input| input.transcript_path.clone());

    // A thread server fires the hooks of every tab's threads, so the tab's
    // owner is the client it names (see `served_record`).
    let server = tool
        .spec()
        .thread_server
        .as_ref()
        .zip(pid)
        .filter(|(server, parent)| !explicit_session && (server.is_server)(*parent));

    // Some harnesses fire these hooks from frontends that never hold a
    // terminal tab — the Codex desktop app, its automations, `codex exec`,
    // subagents. The hook payload does not say which frontend owns the
    // thread, so the harness supplies a gate that reads the transcript (see
    // `Discovery::accept`). No readable transcript also disqualifies: resume
    // needs one, so the session could never be restored anyway.
    let accept = match &tool.spec().discovery {
        Discovery::Jsonl { accept, .. } => *accept,
        _ => None,
    };
    if let Some(accept) = accept
        && server.is_none()
        && !explicit_session
        && !transcript_path
            .as_deref()
            .is_some_and(|path| accept(path, &session_id))
    {
        return Ok(());
    }

    let source = hook_input.as_ref().and_then(|input| input.source.clone());
    let hook_event = hook_input
        .as_ref()
        .and_then(|input| input.hook_event_name.clone());
    let background_tasks = hook_input
        .as_ref()
        .and_then(|input| input.background_tasks.clone());
    let name = name.or_else(|| hook_input.and_then(|input| input.name));

    sessions::repair_if_corrupt(&paths::sessions_file()?)?;
    let record = match server {
        Some((server, server_pid)) => {
            let known = sessions::read_sessions(&paths::sessions_file()?)?
                .into_iter()
                .find(|known| known.session_id == session_id);
            let Some(record) = served_record(
                server,
                server_pid,
                hook_event.as_deref(),
                known.as_ref(),
                SessionRecord::new(
                    tool,
                    session_id,
                    None,
                    None,
                    directory,
                    transcript_path,
                    name,
                    source,
                ),
            )?
            else {
                return Ok(());
            };
            record
        }
        None => SessionRecord::new(
            tool,
            session_id,
            pid,
            shell_pid,
            directory,
            transcript_path,
            name,
            source,
        ),
    };
    sessions::register(
        &paths::sessions_file()?,
        SessionRecord {
            background_tasks,
            ..record
        },
    )
}

// Codex background tasks share the TUI endpoint but submit tool output instead of prompts.
fn served_record(
    server: &ThreadServer,
    server_pid: i32,
    hook_event: Option<&str>,
    known: Option<&SessionRecord>,
    mut record: SessionRecord,
) -> Result<Option<SessionRecord>> {
    record.server_pid = Some(server_pid);
    record.server_pid_started_at = process::process_start_identity(server_pid).ok();
    let admitted = known.filter(|known| known.state == SessionState::Active);
    if let Some(known) = admitted.filter(|known| {
        hook_event != Some("SessionStart")
            && record.server_pid_started_at.is_some()
            && known.server_pid == record.server_pid
            && known.server_pid_started_at == record.server_pid_started_at
    }) && let Some(client) = known.pid
    {
        set_owner(&mut record, client, known.shell_pid);
        if record.pid_started_at.is_some() && record.pid_started_at == known.pid_started_at {
            return Ok(Some(record));
        }
    }
    if admitted.is_none() && hook_event != Some("UserPromptSubmit") {
        return Ok(None);
    }
    if record.transcript_path.is_none() {
        return Ok(None);
    }
    let Some(client) = (server.client)(&ServedThread {
        tool: record.tool,
        server: server_pid,
        session_id: &record.session_id,
    })?
    else {
        return Ok(None);
    };
    set_owner(&mut record, client, client_shell(client));
    Ok(Some(record))
}

fn set_owner(record: &mut SessionRecord, pid: i32, shell_pid: Option<i32>) {
    record.pid = Some(pid);
    record.pid_started_at = process::process_start_identity(pid).ok();
    record.shell_pid = shell_pid;
    record.shell_pid_started_at =
        shell_pid.and_then(|pid| process::process_start_identity(pid).ok());
}

fn client_shell(client: i32) -> Option<i32> {
    process::process_environment_variable(client, crate::commands::launch::SHELL_PID_ENV)
        .and_then(|pid| pid.parse().ok())
        .or_else(|| process::parent_pid(client))
}

pub(crate) fn is_internal_directory(directory: &Path, tool_home: &Path) -> bool {
    directory.starts_with(tool_home)
}

pub fn read_session_id_from_hook_stdin() -> Result<Option<String>> {
    Ok(read_hook_input()?
        .and_then(|input| input.session_id)
        .or_else(|| first_env(SESSION_ID_ENVS)))
}

fn read_hook_input() -> Result<Option<HookInput>> {
    if io::stdin().is_terminal() {
        return Ok(None);
    }

    let mut contents = String::new();
    io::stdin()
        .read_to_string(&mut contents)
        .context("failed to read hook stdin")?;
    if contents.trim().is_empty() {
        return Ok(None);
    }

    parse_hook_payload(&contents)
        .with_context(|| format!("failed to parse hook stdin as JSON: {contents}"))
        .map(Some)
}

/// Grok sends every field under both Claude's `snake_case` key and its own
/// `camelCase` key, and serde rejects a key together with its alias as a
/// duplicate, so the `camelCase` twin of a `snake_case` key is dropped first.
pub(crate) fn parse_hook_payload<T: DeserializeOwned>(contents: &str) -> serde_json::Result<T> {
    let mut payload: serde_json::Value = serde_json::from_str(contents)?;
    if let Some(fields) = payload.as_object_mut() {
        let twins: Vec<String> = fields
            .keys()
            .filter(|key| {
                let snake = snake_case(key);
                snake != **key && fields.contains_key(&snake)
            })
            .cloned()
            .collect();
        for twin in twins {
            fields.remove(&twin);
        }
    }
    serde_json::from_value(payload)
}

fn snake_case(key: &str) -> String {
    key.chars().fold(String::new(), |mut snake, character| {
        if character.is_ascii_uppercase() {
            snake.push('_');
            snake.push(character.to_ascii_lowercase());
        } else {
            snake.push(character);
        }
        snake
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn own_pid() -> i32 {
        i32::try_from(std::process::id()).unwrap()
    }

    fn transcript(dir: &tempfile::TempDir, source: &serde_json::Value) -> PathBuf {
        let path = dir
            .path()
            .join(format!("rollout-{}.jsonl", source.to_string().len()));
        std::fs::write(
            &path,
            serde_json::json!({"type": "session_meta", "payload": {"id": "thread", "source": source}})
                .to_string(),
        )
        .unwrap();
        path
    }

    fn served(
        server: &ThreadServer,
        hook_event: &str,
        known: Option<&SessionRecord>,
        transcript: Option<PathBuf>,
    ) -> Option<SessionRecord> {
        let record = SessionRecord::new(
            crate::tool("codex"),
            "thread".to_string(),
            None,
            None,
            PathBuf::from("/tmp/project"),
            transcript,
            None,
            None,
        );
        served_record(server, own_pid(), Some(hook_event), known, record).unwrap()
    }

    const NAMES_THIS_PROCESS: ThreadServer = ThreadServer {
        is_server: |_| true,
        serving: |_| None,
        client: |_| Ok(Some(i32::try_from(std::process::id()).unwrap())),
    };

    const MUST_NOT_ASK: ThreadServer = ThreadServer {
        client: |_| panic!("asked the server about a thread it must not ask about"),
        ..NAMES_THIS_PROCESS
    };

    fn admitted(dir: &tempfile::TempDir) -> SessionRecord {
        served(
            &NAMES_THIS_PROCESS,
            "UserPromptSubmit",
            None,
            Some(transcript(dir, &serde_json::json!("vscode"))),
        )
        .unwrap()
    }

    #[test]
    fn a_first_typed_prompt_admits_a_served_thread_to_the_client_the_server_names() {
        let dir = tempfile::tempdir().unwrap();
        let record = admitted(&dir);
        assert_eq!(record.pid, Some(own_pid()));
        let launcher = std::env::var(crate::commands::launch::SHELL_PID_ENV)
            .ok()
            .and_then(|pid| pid.parse().ok());
        assert_eq!(
            record.shell_pid,
            launcher.or_else(|| process::parent_pid(own_pid()))
        );
        assert_eq!(record.server_pid, Some(own_pid()));
        assert!(record.pid_started_at.is_some());
        assert_eq!(record.server_pid_started_at, record.pid_started_at);
        assert_eq!(record.state, SessionState::Active);
    }

    #[test]
    fn a_lifecycle_without_a_typed_prompt_never_admits_a_served_thread() {
        let dir = tempfile::tempdir().unwrap();
        let vscode = transcript(&dir, &serde_json::json!("vscode"));
        for hook_event in ["SessionStart", "Stop", "SessionEnd"] {
            assert!(
                served(&MUST_NOT_ASK, hook_event, None, Some(vscode.clone())).is_none(),
                "{hook_event}"
            );
        }
    }

    #[test]
    fn a_thread_without_a_transcript_never_joins_a_tab() {
        assert!(served(&NAMES_THIS_PROCESS, "UserPromptSubmit", None, None).is_none());
    }

    #[test]
    fn a_later_hook_keeps_the_running_client_without_asking() {
        let dir = tempfile::tempdir().unwrap();
        let known = admitted(&dir);
        for hook_event in ["Stop", "UserPromptSubmit"] {
            let record = served(&MUST_NOT_ASK, hook_event, Some(&known), None).unwrap();
            assert_eq!(record.pid, known.pid);
            assert_eq!(record.shell_pid, known.shell_pid);
        }
    }

    #[test]
    fn a_new_runtime_or_another_server_asks_again_for_an_admitted_thread() {
        let dir = tempfile::tempdir().unwrap();
        let path = Some(transcript(&dir, &serde_json::json!("cli")));
        let known = admitted(&dir);
        let names_the_parent = ThreadServer {
            client: |_| {
                Ok(process::parent_pid(
                    i32::try_from(std::process::id()).unwrap(),
                ))
            },
            ..NAMES_THIS_PROCESS
        };
        let parent = process::parent_pid(own_pid());
        let pid = |known: &SessionRecord, hook_event| {
            served(&names_the_parent, hook_event, Some(known), path.clone())
                .unwrap()
                .pid
        };
        assert_eq!(pid(&known, "SessionStart"), parent);

        let mut restarted = known.clone();
        restarted.server_pid_started_at = Some("Wed Jan 1 00:00:00 2020".to_string());
        assert_eq!(pid(&restarted, "Stop"), parent);

        let mut exited = known;
        exited.pid_started_at = Some("Wed Jan 1 00:00:00 2020".to_string());
        assert_eq!(pid(&exited, "Stop"), parent);
    }

    #[test]
    fn a_released_thread_rejoins_a_tab_only_through_a_typed_prompt() {
        let dir = tempfile::tempdir().unwrap();
        let path = transcript(&dir, &serde_json::json!("vscode"));
        let mut ending = admitted(&dir);
        ending.mark_ending();
        let mut recoverable = ending.clone();
        recoverable.mark_recoverable();
        for known in [ending, recoverable] {
            for hook_event in ["SessionStart", "Stop"] {
                assert!(
                    served(&MUST_NOT_ASK, hook_event, Some(&known), Some(path.clone())).is_none()
                );
            }
            assert!(
                served(
                    &NAMES_THIS_PROCESS,
                    "UserPromptSubmit",
                    Some(&known),
                    Some(path.clone())
                )
                .is_some()
            );
        }
    }

    #[test]
    fn a_hook_is_emitted_by_the_parent_of_its_session_leader() {
        use std::os::unix::process::CommandExt;
        let mut command = std::process::Command::new("/bin/sh");
        command.args(["-c", "/bin/sleep 30; :"]);
        unsafe {
            command.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
        let mut hook_shell = command.spawn().unwrap();
        let shell_pid = i32::try_from(hook_shell.id()).unwrap();
        let mut hook = None;
        for _ in 0..100 {
            hook = process::child_pids(shell_pid).first().copied();
            if hook.is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let emitters = (hook_emitter(shell_pid), hook.and_then(hook_emitter));
        if let Some(hook) = hook {
            let _ = std::process::Command::new("/bin/kill")
                .arg(hook.to_string())
                .status();
        }
        hook_shell.kill().unwrap();
        hook_shell.wait().unwrap();
        assert_eq!(emitters, (Some(own_pid()), Some(own_pid())));
    }

    #[test]
    fn internal_directory_matches_tool_home_and_descendants() {
        let home = Path::new("/Users/tyler/.codex");
        assert!(is_internal_directory(
            Path::new("/Users/tyler/.codex"),
            home
        ));
        assert!(is_internal_directory(
            Path::new("/Users/tyler/.codex/memories"),
            home
        ));
    }

    #[test]
    fn project_directory_is_not_internal() {
        let home = Path::new("/Users/tyler/.codex");
        assert!(!is_internal_directory(
            Path::new("/Users/tyler/Code/go2rust"),
            home
        ));
        // A sibling that merely shares a prefix string must not match.
        assert!(!is_internal_directory(
            Path::new("/Users/tyler/.codex-backup"),
            home
        ));
    }

    #[test]
    fn grok_hook_json_accepts_workspace_root_when_cwd_is_absent() {
        let input: HookInput = serde_json::from_str(
            r#"{"sessionId":"abc","workspaceRoot":"/tmp/proj","transcriptPath":"/tmp/t.jsonl"}"#,
        )
        .unwrap();
        assert_eq!(input.session_id.as_deref(), Some("abc"));
        assert_eq!(input.cwd, None);
        assert_eq!(
            input.workspace_root.as_deref(),
            Some(Path::new("/tmp/proj"))
        );
        assert_eq!(
            input.transcript_path.as_deref(),
            Some(Path::new("/tmp/t.jsonl"))
        );
    }

    #[test]
    fn grok_hook_json_keeps_cwd_when_both_are_present() {
        let input: HookInput = serde_json::from_str(
            r#"{"sessionId":"abc","cwd":"/tmp/cwd","workspaceRoot":"/tmp/root"}"#,
        )
        .unwrap();
        assert_eq!(input.cwd.as_deref(), Some(Path::new("/tmp/cwd")));
        assert_eq!(
            input.workspace_root.as_deref(),
            Some(Path::new("/tmp/root"))
        );
    }

    #[test]
    fn grok_hook_json_accepts_both_spellings_of_a_field() {
        let input: HookInput = parse_hook_payload(
            r#"{"hookEventName":"session_end","sessionId":"abc","cwd":"/tmp/cwd","workspaceRoot":"/tmp/root","transcriptPath":"/tmp/t.jsonl","hook_event_name":"SessionEnd","session_id":"abc","transcript_path":"/tmp/t.jsonl"}"#,
        )
        .unwrap();
        assert_eq!(input.session_id.as_deref(), Some("abc"));
        assert_eq!(
            input.transcript_path.as_deref(),
            Some(Path::new("/tmp/t.jsonl"))
        );
    }
}
