use crate::hooks;
use crate::paths;
use crate::process;
use anyhow::Result;

pub fn run() -> Result<()> {
    let claude_installed = process::command_exists("claude");
    let codex_installed = process::command_exists("codex");

    let claude_hooks = if claude_installed {
        Some(hooks::install_claude_hooks(&paths::claude_settings()?)?)
    } else {
        None
    };
    let codex_hooks = if codex_installed {
        Some(hooks::install_codex_hooks(&paths::codex_config()?)?)
    } else {
        None
    };

    println!(
        "Claude Code hooks: {}",
        hook_summary(claude_installed, claude_hooks.as_ref())
    );
    println!(
        "Codex hooks: {}",
        hook_summary(codex_installed, codex_hooks.as_ref())
    );
    Ok(())
}

fn hook_summary(installed: bool, change: Option<&hooks::HookChange>) -> &'static str {
    if !installed {
        return "skipped; tool not found in PATH";
    }

    match change {
        Some(change) if change.changed => "installed",
        Some(_) => "already present",
        None => "skipped",
    }
}
