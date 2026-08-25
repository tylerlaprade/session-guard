use crate::Tool;
use crate::cargo_targets::{self, CacheRemoval};
use crate::commands;
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

    let removals: Vec<(Tool, hooks::HookChange)> = Tool::all()
        .map(|tool| Ok((tool, hooks::remove(tool)?)))
        .collect::<Result<_>>()?;

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
    for (tool, change) in removals {
        println!(
            "{} {}: {}",
            tool.spec().display_name,
            commands::install_hooks::surface(tool),
            if change.changed {
                "removed"
            } else {
                "not present"
            }
        );
    }
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
