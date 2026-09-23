use crate::Tool;
use crate::harness::Discovery;
use crate::paths;
use crate::sessions::{self, SessionRecord};
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
        && !explicit_session
        && !transcript_path
            .as_deref()
            .is_some_and(|path| accept(path, &session_id))
    {
        return Ok(());
    }

    let source = hook_input.as_ref().and_then(|input| input.source.clone());
    let name = name.or_else(|| hook_input.and_then(|input| input.name));

    sessions::repair_if_corrupt(&paths::sessions_file()?)?;
    let record = SessionRecord::new(
        tool,
        session_id,
        pid,
        shell_pid,
        directory,
        transcript_path,
        name,
        source,
    );
    sessions::register(&paths::sessions_file()?, record)
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
