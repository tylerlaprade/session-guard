use crate::TerminalKind;
use crate::hooks;
use crate::paths;
use crate::process;
use anyhow::{Context, Result};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

pub fn run(terminal: TerminalKind) -> Result<()> {
    ensure_terminal_available(terminal)?;

    let config_dir = paths::config_dir()?;
    fs::create_dir_all(&config_dir)
        .with_context(|| format!("failed to create {}", config_dir.display()))?;
    fs::write(paths::terminal_file()?, format!("{}\n", terminal.as_str()))
        .context("failed to write terminal choice")?;
    sessions_store_init()?;

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

    write_launch_agent()?;
    load_launch_agent(&paths::launch_agent_plist()?)?;

    println!("session-guard installed");
    println!("Terminal: {terminal}");
    println!(
        "Claude Code hooks: {}",
        hook_summary(claude_installed, claude_hooks.as_ref())
    );
    println!(
        "Codex hooks: {}",
        hook_summary(codex_installed, codex_hooks.as_ref())
    );
    if terminal == TerminalKind::Alacritty {
        println!("Alacritty has no tab control API; restores will open new windows.");
    }
    println!("LaunchAgent: {}", paths::launch_agent_plist()?.display());
    println!("Daemon log: {}", paths::daemon_log()?.display());
    Ok(())
}

fn sessions_store_init() -> Result<()> {
    crate::sessions::ensure_store(&paths::sessions_file()?)
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

fn ensure_terminal_available(terminal: TerminalKind) -> Result<()> {
    let available = match terminal {
        TerminalKind::Ghostty => app_exists("Ghostty"),
        TerminalKind::Iterm2 => app_exists("iTerm") || app_exists("iTerm2"),
        TerminalKind::Terminal => app_exists("Terminal"),
        TerminalKind::Kitty => process::command_exists("kitty"),
        TerminalKind::Wezterm => process::command_exists("wezterm"),
        TerminalKind::Alacritty => process::command_exists("alacritty"),
    };

    if !available {
        anyhow::bail!("{terminal} is not installed or is not available in PATH");
    }

    Ok(())
}

fn app_exists(name: &str) -> bool {
    Command::new("open")
        .args(["-Ra", name])
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

fn write_launch_agent() -> Result<()> {
    let plist = paths::launch_agent_plist()?;
    if let Some(parent) = plist.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }

    let executable = std::env::current_exe().context("failed to locate current executable")?;
    let log_path = paths::daemon_log()?;
    let contents = launch_agent_plist(&executable, &log_path);
    fs::write(&plist, contents).with_context(|| format!("failed to write {}", plist.display()))?;
    fs::set_permissions(&plist, fs::Permissions::from_mode(0o644))
        .with_context(|| format!("failed to set permissions on {}", plist.display()))?;
    Ok(())
}

fn launch_agent_plist(executable: &Path, log_path: &Path) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>com.tylerlaprade.session-guard</string>
  <key>ProgramArguments</key>
  <array>
    <string>{}</string>
    <string>daemon</string>
  </array>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <true/>
  <key>StandardOutPath</key>
  <string>{}</string>
  <key>StandardErrorPath</key>
  <string>{}</string>
</dict>
</plist>
"#,
        xml_escape(&executable.display().to_string()),
        xml_escape(&log_path.display().to_string()),
        xml_escape(&log_path.display().to_string()),
    )
}

fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn load_launch_agent(plist: &Path) -> Result<()> {
    let _ = Command::new("launchctl").arg("unload").arg(plist).status();
    let status = Command::new("launchctl")
        .arg("load")
        .arg(plist)
        .status()
        .context("failed to run launchctl load")?;

    if !status.success() {
        anyhow::bail!("launchctl load failed with status {status}");
    }

    Ok(())
}
