use crate::Tool;
use crate::paths;
use crate::sessions::{self, SessionRecord};
use anyhow::{Context, Result};
use serde::Deserialize;
use std::io::{self, IsTerminal, Read};
use std::path::PathBuf;

#[derive(Debug, Deserialize)]
struct HookInput {
    session_id: Option<String>,
    cwd: Option<PathBuf>,
    transcript_path: Option<PathBuf>,
    source: Option<String>,
    name: Option<String>,
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

    let session_id = session_id
        .or_else(|| {
            hook_input
                .as_ref()
                .and_then(|input| input.session_id.clone())
        })
        .context(
            "missing session id; pass --session-id or run from a hook that provides session_id",
        )?;
    let directory = directory
        .or_else(|| hook_input.as_ref().and_then(|input| input.cwd.clone()))
        .context("missing directory; pass --directory or run from a hook that provides cwd")?;
    let transcript_path = hook_input
        .as_ref()
        .and_then(|input| input.transcript_path.clone());
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

pub fn read_session_id_from_hook_stdin() -> Result<Option<String>> {
    Ok(read_hook_input()?.and_then(|input| input.session_id))
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

    serde_json::from_str(&contents)
        .with_context(|| format!("failed to parse hook stdin as JSON: {contents}"))
        .map(Some)
}
