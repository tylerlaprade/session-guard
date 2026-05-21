use crate::hooks;
use crate::paths;
use anyhow::{Context, Result};
use std::fs;
use std::process::Command;

pub fn run(purge: bool) -> Result<()> {
    let claude = hooks::remove_claude_hooks(&paths::claude_settings()?)?;
    let codex = hooks::remove_codex_hooks(&paths::codex_config()?)?;

    let plist = paths::launch_agent_plist()?;
    if plist.exists() {
        let _ = Command::new("launchctl").arg("unload").arg(&plist).status();
        fs::remove_file(&plist).with_context(|| format!("failed to remove {}", plist.display()))?;
    }

    if purge {
        let config_dir = paths::config_dir()?;
        if config_dir.exists() {
            fs::remove_dir_all(&config_dir)
                .with_context(|| format!("failed to remove {}", config_dir.display()))?;
        }
    }

    println!("session-guard uninstalled");
    println!(
        "Claude Code hooks: {}",
        if claude.changed {
            "removed"
        } else {
            "not present"
        }
    );
    println!(
        "Codex hooks: {}",
        if codex.changed {
            "removed"
        } else {
            "not present"
        }
    );
    println!("LaunchAgent: removed");
    if purge {
        println!("Config directory: removed");
    }

    Ok(())
}
