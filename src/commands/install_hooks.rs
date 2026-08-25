use crate::Tool;
use crate::harness::Integration;
use crate::hooks;
use crate::process;
use anyhow::Result;

pub fn run() -> Result<()> {
    for line in install_all()? {
        println!("{line}");
    }
    Ok(())
}

/// Installs hooks for every harness found in PATH and returns the report lines,
/// one per harness plus any note the harness asked to show.
pub fn install_all() -> Result<Vec<String>> {
    let mut lines = Vec::new();

    for tool in Tool::all() {
        let spec = tool.spec();
        let present = process::command_exists(spec.binary);
        let change = if present {
            Some(hooks::install(tool)?)
        } else {
            None
        };

        lines.push(format!(
            "{} {}: {}",
            spec.display_name,
            surface(tool),
            hook_summary(present, change.as_ref())
        ));

        if let Some(note) = spec.install_note
            && change.is_some_and(|change| change.changed)
        {
            lines.push(note.to_string());
        }
    }

    Ok(lines)
}

/// What the harness calls the thing being installed.
pub fn surface(tool: Tool) -> &'static str {
    match tool.spec().integration {
        Integration::PluginFile { .. } => "plugin",
        _ => "hooks",
    }
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
