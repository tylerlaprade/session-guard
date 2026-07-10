use crate::cargo_targets::{self, CacheRemoval};
use crate::hooks;
use crate::paths;
use anyhow::{Context, Result};
use std::fs;
use std::process::Command;

pub fn run(purge: bool) -> Result<()> {
    // Preflight before touching hooks, the LaunchAgent, or recovery config.
    // The returned plan holds the exclusive cache lifecycle lease through
    // removal, so another tab cannot start using a target mid-uninstall.
    let cargo_cache_plan = cargo_targets::prepare_cache_removal()?;

    let claude = hooks::remove_claude_hooks(&paths::claude_settings()?)?;
    let codex = hooks::remove_codex_hooks(&paths::codex_config()?)?;
    let grok = hooks::remove_grok_hooks(&paths::grok_hooks_dir()?)?;

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

    let cargo_cache = cargo_cache_plan.remove()?;

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
    println!(
        "Grok hooks: {}",
        if grok.changed {
            "removed"
        } else {
            "not present"
        }
    );
    println!("LaunchAgent: removed");
    println!(
        "Session-owned Cargo targets: {}",
        match cargo_cache {
            CacheRemoval::Removed => "removed",
            CacheRemoval::NotPresent => "not present",
        }
    );
    if purge {
        println!("Config directory: removed");
    }

    Ok(())
}
