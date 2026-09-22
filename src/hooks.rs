use crate::Tool;
use crate::harness::{HookAction, HookEvent, Integration};
use anyhow::{Context, Result};
use serde_json::{Map as JsonMap, Value as JsonValue, json};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use toml::Value as TomlValue;
use toml::map::Map as TomlMap;

const OLD_CLAUDE_REGISTER: &str = "session-guard register --tool claude --pid \"$PPID\"";
// The pre-entrypoint-gate register command; without it in the removal lists,
// installs would leave both hooks and the ungated one would re-admit desktop
// and SDK sessions.
const OLD_CLAUDE_REGISTER_UNGATED: &str = r#"tool_pid="$PPID"; shell_pid="$(ps -o ppid= -p "$tool_pid" | tr -d ' ')"; if [ -n "$shell_pid" ]; then session-guard register --tool claude --pid "$tool_pid" --shell-pid "$shell_pid"; else session-guard register --tool claude --pid "$tool_pid"; fi"#;
const OLD_CLAUDE_REGISTER_PARENT: &str = r#"[ "$CLAUDE_CODE_ENTRYPOINT" = cli ] || exit 0; tool_pid="$PPID"; shell_pid="$(ps -o ppid= -p "$tool_pid" | tr -d ' ')"; if [ -n "$shell_pid" ]; then session-guard register --tool claude --pid "$tool_pid" --shell-pid "$shell_pid"; else session-guard register --tool claude --pid "$tool_pid"; fi"#;
const OLD_CODEX_REGISTER: &str = "session-guard register --tool codex --pid \"$PPID\"";
const OLD_CODEX_REGISTER_PARENT: &str = r#"tool_pid="$PPID"; shell_pid="$(ps -o ppid= -p "$tool_pid" | tr -d ' ')"; if [ -n "$shell_pid" ]; then session-guard register --tool codex --pid "$tool_pid" --shell-pid "$shell_pid"; else session-guard register --tool codex --pid "$tool_pid"; fi"#;
const OLD_CLAUDE_REGISTER_ENV: &str = "session-guard register --tool claude --session-id \"$CLAUDE_SESSION_ID\" --pid \"$PPID\" --directory \"$PWD\"";
const OLD_CLAUDE_DEREGISTER_ENV: &str =
    "session-guard deregister --session-id \"$CLAUDE_SESSION_ID\"";
const OLD_CODEX_REGISTER_ENV: &str = "session-guard register --tool codex --session-id \"$CODEX_SESSION_ID\" --pid \"$PPID\" --directory \"$PWD\"";
const OLD_CODEX_DEREGISTER_ENV: &str =
    "session-guard deregister --session-id \"$CODEX_SESSION_ID\"";

#[derive(Debug, Default)]
pub struct HookChange {
    pub changed: bool,
}

/// Installs the register/deregister hooks for one harness, in whatever shape
/// that harness accepts. Adding a harness that reuses one of these shapes needs
/// no code here — only a registry entry.
pub fn install(tool: Tool) -> Result<HookChange> {
    match &tool.spec().integration {
        Integration::JsonSettings {
            path,
            events,
            register,
            deregister,
        } => install_json_hooks(&path.resolve()?, events, register, deregister),
        Integration::TomlConfig {
            path,
            events,
            register,
            deregister,
        } => install_toml_hooks(&path.resolve()?, events, register, deregister),
        Integration::ScriptDir {
            path,
            events,
            script_name,
            script,
            manifest_name,
            deregister,
        } => install_script_dir(
            &path.resolve()?,
            events,
            script_name,
            script,
            manifest_name,
            deregister,
        ),
        Integration::PluginFile { path, name, source } => {
            install_plugin_file(&path.resolve()?, name, source)
        }
    }
}

pub fn remove(tool: Tool) -> Result<HookChange> {
    match &tool.spec().integration {
        Integration::JsonSettings { path, .. } => remove_json_hooks(&path.resolve()?),
        Integration::TomlConfig { path, .. } => remove_toml_hooks(&path.resolve()?),
        Integration::ScriptDir {
            path,
            script_name,
            manifest_name,
            ..
        } => remove_files(&path.resolve()?, &[manifest_name, script_name]),
        Integration::PluginFile { path, name, .. } => remove_files(&path.resolve()?, &[name]),
    }
}

/// Installs into an explicit directory or file instead of the harness's real
/// path, so tests can exercise a harness end to end in a temp dir.
#[cfg(test)]
fn install_at(tool: Tool, path: &Path) -> Result<HookChange> {
    match &tool.spec().integration {
        Integration::JsonSettings {
            events,
            register,
            deregister,
            ..
        } => install_json_hooks(path, events, register, deregister),
        Integration::TomlConfig {
            events,
            register,
            deregister,
            ..
        } => install_toml_hooks(path, events, register, deregister),
        Integration::ScriptDir {
            events,
            script_name,
            script,
            manifest_name,
            deregister,
            ..
        } => install_script_dir(path, events, script_name, script, manifest_name, deregister),
        Integration::PluginFile { name, source, .. } => install_plugin_file(path, name, source),
    }
}

#[cfg(test)]
fn remove_at(tool: Tool, path: &Path) -> Result<HookChange> {
    match &tool.spec().integration {
        Integration::JsonSettings { .. } => remove_json_hooks(path),
        Integration::TomlConfig { .. } => remove_toml_hooks(path),
        Integration::ScriptDir {
            script_name,
            manifest_name,
            ..
        } => remove_files(path, &[manifest_name, script_name]),
        Integration::PluginFile { name, .. } => remove_files(path, &[name]),
    }
}

/// The command a hook runs for one lifecycle event.
fn hook_command<'a>(event: &HookEvent, register: &'a str, deregister: &'a str) -> &'a str {
    match event.action {
        HookAction::Register => register,
        HookAction::Deregister => deregister,
    }
}

fn install_json_hooks(
    path: &Path,
    events: &[HookEvent],
    register: &str,
    deregister: &str,
) -> Result<HookChange> {
    let mut root = read_json_config(path)?;
    let mut changed = remove_json_hook_commands(&mut root, old_hook_commands());

    for event in events {
        changed |= ensure_json_hook(
            &mut root,
            event.event,
            event.matcher,
            hook_command(event, register, deregister),
        )?;
    }

    if changed {
        write_json_config(path, &root)?;
    }

    Ok(HookChange { changed })
}

fn remove_json_hooks(path: &Path) -> Result<HookChange> {
    if !path.exists() {
        return Ok(HookChange::default());
    }

    let mut root = read_json_config(path)?;
    let changed = remove_json_hook_commands(&mut root, &all_hook_commands());

    if changed {
        write_json_config(path, &root)?;
    }

    Ok(HookChange { changed })
}

fn install_toml_hooks(
    path: &Path,
    events: &[HookEvent],
    register: &str,
    deregister: &str,
) -> Result<HookChange> {
    let mut root = read_toml_config(path)?;
    let mut changed = remove_toml_hook_commands(&mut root, old_hook_commands());

    for event in events {
        changed |= ensure_toml_hook(
            &mut root,
            event.event,
            event.matcher,
            hook_command(event, register, deregister),
        )?;
    }

    if changed {
        write_toml_config(path, &root)?;
    }

    Ok(HookChange { changed })
}

fn remove_toml_hooks(path: &Path) -> Result<HookChange> {
    if !path.exists() {
        return Ok(HookChange::default());
    }

    let mut root = read_toml_config(path)?;
    let changed = remove_toml_hook_commands(&mut root, &all_hook_commands());

    if changed {
        write_toml_config(path, &root)?;
    }

    Ok(HookChange { changed })
}

fn install_script_dir(
    dir: &Path,
    events: &[HookEvent],
    script_name: &str,
    script: &str,
    manifest_name: &str,
    deregister: &str,
) -> Result<HookChange> {
    fs::create_dir_all(dir).with_context(|| format!("failed to create {}", dir.display()))?;

    let script_path = dir.join(script_name);
    let mut changed = write_if_changed(&script_path, script)?;
    if changed || !is_executable(&script_path) {
        let mut perms = fs::metadata(&script_path)
            .with_context(|| format!("failed to stat {}", script_path.display()))?
            .permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&script_path, perms)
            .with_context(|| format!("failed to chmod {}", script_path.display()))?;
        changed = true;
    }

    let manifest = hooks_manifest_json(events, script_name, deregister);
    changed |= write_if_changed(&dir.join(manifest_name), &manifest)?;

    Ok(HookChange { changed })
}

fn install_plugin_file(dir: &Path, name: &str, source: &str) -> Result<HookChange> {
    fs::create_dir_all(dir).with_context(|| format!("failed to create {}", dir.display()))?;
    let changed = write_if_changed(&dir.join(name), source)?;
    Ok(HookChange { changed })
}

fn remove_files(dir: &Path, names: &[&str]) -> Result<HookChange> {
    let mut changed = false;
    for name in names {
        let path = dir.join(name);
        if path.exists() {
            fs::remove_file(&path)
                .with_context(|| format!("failed to remove {}", path.display()))?;
            changed = true;
        }
    }
    Ok(HookChange { changed })
}

/// A standalone hooks manifest: `{"hooks": {"<Event>": [{"hooks": [...]}]}}`.
fn hooks_manifest_json(events: &[HookEvent], register: &str, deregister: &str) -> String {
    let mut hooks = serde_json::Map::new();
    for event in events {
        let command = hook_command(event, register, deregister);
        hooks
            .entry(event.event)
            .or_insert_with(|| json!([]))
            .as_array_mut()
            .expect("hook event array")
            .push(json!({
                "hooks": [{ "type": "command", "command": command }]
            }));
    }

    let mut contents = serde_json::to_string_pretty(&json!({ "hooks": hooks }))
        .expect("static hooks manifest json");
    contents.push('\n');
    contents
}

fn write_if_changed(path: &Path, contents: &str) -> Result<bool> {
    if path.exists() {
        let existing = fs::read_to_string(path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        if existing == contents {
            return Ok(false);
        }
    }
    fs::write(path, contents).with_context(|| format!("failed to write {}", path.display()))?;
    Ok(true)
}

fn is_executable(path: &Path) -> bool {
    fs::metadata(path).is_ok_and(|metadata| metadata.permissions().mode() & 0o111 != 0)
}

fn read_json_config(path: &Path) -> Result<JsonValue> {
    if !path.exists() {
        return Ok(JsonValue::Object(JsonMap::new()));
    }

    let contents =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    if contents.trim().is_empty() {
        return Ok(JsonValue::Object(JsonMap::new()));
    }

    serde_json::from_str(&contents).with_context(|| format!("failed to parse {}", path.display()))
}

fn write_json_config(path: &Path, root: &JsonValue) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }

    let mut contents = serde_json::to_string_pretty(root)?;
    contents.push('\n');
    fs::write(path, contents).with_context(|| format!("failed to write {}", path.display()))
}

fn old_hook_commands() -> &'static [&'static str] {
    &[
        OLD_CLAUDE_REGISTER,
        OLD_CLAUDE_REGISTER_UNGATED,
        OLD_CLAUDE_REGISTER_PARENT,
        OLD_CLAUDE_REGISTER_ENV,
        OLD_CLAUDE_DEREGISTER_ENV,
        OLD_CODEX_REGISTER,
        OLD_CODEX_REGISTER_PARENT,
        OLD_CODEX_REGISTER_ENV,
        OLD_CODEX_DEREGISTER_ENV,
    ]
}

/// Every command session-guard has ever installed, current and retired, so an
/// uninstall leaves nothing behind. Current commands come from the registry, so
/// a new harness is covered the moment it is added.
fn all_hook_commands() -> Vec<&'static str> {
    let mut commands: Vec<&'static str> = Tool::all()
        .flat_map(|tool| match &tool.spec().integration {
            Integration::JsonSettings {
                register,
                deregister,
                ..
            }
            | Integration::TomlConfig {
                register,
                deregister,
                ..
            } => vec![*register, *deregister],
            Integration::ScriptDir { deregister, .. } => vec![*deregister],
            Integration::PluginFile { .. } => Vec::new(),
        })
        .collect();
    commands.extend_from_slice(old_hook_commands());
    commands.sort_unstable();
    commands.dedup();
    commands
}

fn ensure_json_hook(
    root: &mut JsonValue,
    event: &str,
    matcher: Option<&str>,
    command: &str,
) -> Result<bool> {
    let root = json_object_mut(root, "Claude settings root")?;
    let hooks = root
        .entry("hooks".to_string())
        .or_insert_with(|| JsonValue::Object(JsonMap::new()));
    let hooks = json_object_mut(hooks, "Claude hooks")?;
    let event_hooks = hooks
        .entry(event.to_string())
        .or_insert_with(|| JsonValue::Array(Vec::new()));
    let event_hooks = json_array_mut(event_hooks, event)?;

    if json_event_has_command(event_hooks, command) {
        return Ok(false);
    }

    let mut group = JsonMap::new();
    if let Some(matcher) = matcher {
        group.insert(
            "matcher".to_string(),
            JsonValue::String(matcher.to_string()),
        );
    }
    group.insert(
        "hooks".to_string(),
        JsonValue::Array(vec![json!({
            "type": "command",
            "command": command
        })]),
    );
    event_hooks.push(JsonValue::Object(group));
    Ok(true)
}

fn json_event_has_command(event_hooks: &[JsonValue], command: &str) -> bool {
    event_hooks.iter().any(|group| {
        group
            .get("hooks")
            .and_then(JsonValue::as_array)
            .is_some_and(|hooks| {
                hooks.iter().any(|hook| {
                    hook.get("command")
                        .and_then(JsonValue::as_str)
                        .is_some_and(|existing| existing == command)
                })
            })
    })
}

fn remove_json_hook_commands(root: &mut JsonValue, commands: &[&str]) -> bool {
    let Some(hooks) = root.get_mut("hooks").and_then(JsonValue::as_object_mut) else {
        return false;
    };

    let mut changed = false;
    let events: Vec<String> = hooks.keys().cloned().collect();
    for event in events {
        let Some(event_hooks) = hooks.get_mut(&event).and_then(JsonValue::as_array_mut) else {
            continue;
        };

        for group in event_hooks.iter_mut() {
            let Some(inner_hooks) = group.get_mut("hooks").and_then(JsonValue::as_array_mut) else {
                continue;
            };
            let before = inner_hooks.len();
            inner_hooks.retain(|hook| {
                hook.get("command")
                    .and_then(JsonValue::as_str)
                    .is_none_or(|command| !commands.contains(&command))
            });
            changed |= inner_hooks.len() != before;
        }

        let before = event_hooks.len();
        event_hooks.retain(|group| {
            group
                .get("hooks")
                .and_then(JsonValue::as_array)
                .is_none_or(|hooks| !hooks.is_empty())
        });
        changed |= event_hooks.len() != before;
    }

    let empty_events: Vec<String> = hooks
        .iter()
        .filter(|(_, value)| value.as_array().is_some_and(std::vec::Vec::is_empty))
        .map(|(event, _)| event.clone())
        .collect();
    for event in empty_events {
        hooks.remove(&event);
        changed = true;
    }

    changed
}

fn json_object_mut<'a>(
    value: &'a mut JsonValue,
    name: &str,
) -> Result<&'a mut JsonMap<String, JsonValue>> {
    value
        .as_object_mut()
        .with_context(|| format!("{name} must be a JSON object"))
}

fn json_array_mut<'a>(value: &'a mut JsonValue, name: &str) -> Result<&'a mut Vec<JsonValue>> {
    value
        .as_array_mut()
        .with_context(|| format!("{name} hooks must be a JSON array"))
}

fn read_toml_config(path: &Path) -> Result<TomlValue> {
    if !path.exists() {
        return Ok(TomlValue::Table(TomlMap::new()));
    }

    let contents =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    if contents.trim().is_empty() {
        return Ok(TomlValue::Table(TomlMap::new()));
    }

    contents
        .parse::<TomlValue>()
        .with_context(|| format!("failed to parse {}", path.display()))
}

fn write_toml_config(path: &Path, root: &TomlValue) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }

    let mut contents = toml::to_string_pretty(root)?;
    contents.push('\n');
    fs::write(path, contents).with_context(|| format!("failed to write {}", path.display()))
}

fn ensure_toml_hook(
    root: &mut TomlValue,
    event: &str,
    matcher: Option<&str>,
    command: &str,
) -> Result<bool> {
    let root = toml_table_mut(root, "Codex config root")?;
    let hooks = root
        .entry("hooks".to_string())
        .or_insert_with(|| TomlValue::Table(TomlMap::new()));
    let hooks = toml_table_mut(hooks, "Codex hooks")?;
    let event_hooks = hooks
        .entry(event.to_string())
        .or_insert_with(|| TomlValue::Array(Vec::new()));
    let event_hooks = toml_array_mut(event_hooks, event)?;

    if toml_event_has_command(event_hooks, command) {
        return Ok(false);
    }

    let mut group = TomlMap::new();
    if let Some(matcher) = matcher {
        group.insert(
            "matcher".to_string(),
            TomlValue::String(matcher.to_string()),
        );
    }
    group.insert(
        "hooks".to_string(),
        TomlValue::Array(vec![TomlValue::Table(toml_command_hook(command))]),
    );
    event_hooks.push(TomlValue::Table(group));
    Ok(true)
}

fn toml_command_hook(command: &str) -> TomlMap<String, TomlValue> {
    let mut hook = TomlMap::new();
    hook.insert("type".to_string(), TomlValue::String("command".to_string()));
    hook.insert(
        "command".to_string(),
        TomlValue::String(command.to_string()),
    );
    hook
}

fn toml_event_has_command(event_hooks: &[TomlValue], command: &str) -> bool {
    event_hooks.iter().any(|group| {
        group
            .get("hooks")
            .and_then(TomlValue::as_array)
            .is_some_and(|hooks| {
                hooks.iter().any(|hook| {
                    hook.get("command")
                        .and_then(TomlValue::as_str)
                        .is_some_and(|existing| existing == command)
                })
            })
    })
}

fn remove_toml_hook_commands(root: &mut TomlValue, commands: &[&str]) -> bool {
    let Some(hooks) = root.get_mut("hooks").and_then(TomlValue::as_table_mut) else {
        return false;
    };

    let mut changed = false;
    let events: Vec<String> = hooks.keys().cloned().collect();
    for event in events {
        let Some(event_hooks) = hooks.get_mut(&event).and_then(TomlValue::as_array_mut) else {
            continue;
        };

        for group in event_hooks.iter_mut() {
            let Some(inner_hooks) = group.get_mut("hooks").and_then(TomlValue::as_array_mut) else {
                continue;
            };
            let before = inner_hooks.len();
            inner_hooks.retain(|hook| {
                hook.get("command")
                    .and_then(TomlValue::as_str)
                    .is_none_or(|command| !commands.contains(&command))
            });
            changed |= inner_hooks.len() != before;
        }

        let before = event_hooks.len();
        event_hooks.retain(|group| {
            group
                .get("hooks")
                .and_then(TomlValue::as_array)
                .is_none_or(|hooks| !hooks.is_empty())
        });
        changed |= event_hooks.len() != before;
    }

    let empty_events: Vec<String> = hooks
        .iter()
        .filter(|(_, value)| value.as_array().is_some_and(std::vec::Vec::is_empty))
        .map(|(event, _)| event.clone())
        .collect();
    for event in empty_events {
        hooks.remove(&event);
        changed = true;
    }

    changed
}

fn toml_table_mut<'a>(
    value: &'a mut TomlValue,
    name: &str,
) -> Result<&'a mut TomlMap<String, TomlValue>> {
    value
        .as_table_mut()
        .with_context(|| format!("{name} must be a TOML table"))
}

fn toml_array_mut<'a>(value: &'a mut TomlValue, name: &str) -> Result<&'a mut Vec<TomlValue>> {
    value
        .as_array_mut()
        .with_context(|| format!("{name} hooks must be a TOML array"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::{
        CLAUDE_REGISTER, CODEX_DEREGISTER, CODEX_REGISTER, GROK_DEREGISTER, GROK_HOOKS_FILE_NAME,
        GROK_REGISTER_SCRIPT_NAME, OPENCODE_PLUGIN_NAME,
    };

    #[test]
    fn claude_hook_install_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");

        assert!(install_at(crate::tool("claude"), &path).unwrap().changed);
        assert!(!install_at(crate::tool("claude"), &path).unwrap().changed);

        let root = read_json_config(&path).unwrap();
        let start = root["hooks"]["SessionStart"].as_array().unwrap();
        assert_eq!(start.len(), 1);
        let stop = root["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(stop.len(), 1);
        assert!(json_event_has_command(stop, CLAUDE_REGISTER));
    }

    #[test]
    fn claude_install_replaces_ungated_register_hook() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        fs::write(
            &path,
            json!({
                "hooks": {
                    "SessionStart": [{
                        "matcher": "startup|resume",
                        "hooks": [{
                            "type": "command",
                            "command": OLD_CLAUDE_REGISTER_UNGATED
                        }]
                    }]
                }
            })
            .to_string(),
        )
        .unwrap();

        assert!(install_at(crate::tool("claude"), &path).unwrap().changed);

        let root = read_json_config(&path).unwrap();
        let start = root["hooks"]["SessionStart"].as_array().unwrap();
        assert_eq!(start.len(), 1);
        assert!(json_event_has_command(start, CLAUDE_REGISTER));
        assert!(!json_event_has_command(start, OLD_CLAUDE_REGISTER_UNGATED));
    }

    #[test]
    fn claude_install_replaces_parent_only_register_hook() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        fs::write(
            &path,
            json!({
                "hooks": {
                    "SessionStart": [{
                        "matcher": "startup|resume",
                        "hooks": [{
                            "type": "command",
                            "command": OLD_CLAUDE_REGISTER_PARENT
                        }]
                    }]
                }
            })
            .to_string(),
        )
        .unwrap();

        assert!(install_at(crate::tool("claude"), &path).unwrap().changed);

        let root = read_json_config(&path).unwrap();
        let start = root["hooks"]["SessionStart"].as_array().unwrap();
        assert_eq!(start.len(), 1);
        assert!(json_event_has_command(start, CLAUDE_REGISTER));
        assert!(!json_event_has_command(start, OLD_CLAUDE_REGISTER_PARENT));
    }

    #[test]
    fn claude_install_replaces_old_env_hook() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        fs::write(
            &path,
            json!({
                "hooks": {
                    "SessionStart": [{
                        "hooks": [{
                            "type": "command",
                            "command": OLD_CLAUDE_REGISTER_ENV
                        }]
                    }]
                }
            })
            .to_string(),
        )
        .unwrap();

        assert!(install_at(crate::tool("claude"), &path).unwrap().changed);

        let root = read_json_config(&path).unwrap();
        let start = root["hooks"]["SessionStart"].as_array().unwrap();
        assert_eq!(start.len(), 1);
        assert!(json_event_has_command(start, CLAUDE_REGISTER));
        assert!(!json_event_has_command(start, OLD_CLAUDE_REGISTER_ENV));
    }

    #[test]
    fn codex_hook_install_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");

        assert!(install_at(crate::tool("codex"), &path).unwrap().changed);
        assert!(!install_at(crate::tool("codex"), &path).unwrap().changed);

        let root = read_toml_config(&path).unwrap();
        let start = root["hooks"]["SessionStart"].as_array().unwrap();
        assert_eq!(start.len(), 1);
        let stop = root["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(stop.len(), 1);
        let end = root["hooks"]["SessionEnd"].as_array().unwrap();
        assert!(toml_event_has_command(end, CODEX_DEREGISTER));
        let contents = fs::read_to_string(path).unwrap();
        assert!(contents.contains("[[hooks.SessionStart]]"));
        assert!(contents.contains("[[hooks.Stop]]"));
        assert!(contents.contains("[[hooks.SessionEnd]]"));
    }

    #[test]
    fn codex_install_replaces_parent_only_register_hook() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(
            &path,
            format!(
                r#"[[hooks.SessionStart]]

[[hooks.SessionStart.hooks]]
type = "command"
command = "{}"
"#,
                OLD_CODEX_REGISTER_PARENT
                    .replace('\\', "\\\\")
                    .replace('"', "\\\"")
            ),
        )
        .unwrap();

        assert!(install_at(crate::tool("codex"), &path).unwrap().changed);

        let root = read_toml_config(&path).unwrap();
        let start = root["hooks"]["SessionStart"].as_array().unwrap();
        assert_eq!(start.len(), 1);
        assert!(toml_event_has_command(start, CODEX_REGISTER));
        assert!(!toml_event_has_command(start, OLD_CODEX_REGISTER_PARENT));
    }

    #[test]
    fn codex_install_replaces_old_env_hook() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(
            &path,
            format!(
                r#"[[hooks.SessionStart]]

[[hooks.SessionStart.hooks]]
type = "command"
command = '{}'
"#,
                OLD_CODEX_REGISTER_ENV
            ),
        )
        .unwrap();

        assert!(install_at(crate::tool("codex"), &path).unwrap().changed);

        let root = read_toml_config(&path).unwrap();
        let start = root["hooks"]["SessionStart"].as_array().unwrap();
        assert_eq!(start.len(), 1);
        assert!(toml_event_has_command(start, CODEX_REGISTER));
        assert!(!toml_event_has_command(start, OLD_CODEX_REGISTER_ENV));
    }

    #[test]
    fn grok_hook_install_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let hooks_dir = dir.path().join("hooks");

        assert!(install_at(crate::tool("grok"), &hooks_dir).unwrap().changed);
        assert!(!install_at(crate::tool("grok"), &hooks_dir).unwrap().changed);

        let json_path = hooks_dir.join(GROK_HOOKS_FILE_NAME);
        let script_path = hooks_dir.join(GROK_REGISTER_SCRIPT_NAME);
        assert!(json_path.is_file());
        assert!(script_path.is_file());
        assert!(is_executable(&script_path));

        let root = read_json_config(&json_path).unwrap();
        let start = root["hooks"]["SessionStart"].as_array().unwrap();
        assert_eq!(start.len(), 1);
        assert!(json_event_has_command(start, GROK_REGISTER_SCRIPT_NAME));
        let stop = root["hooks"]["Stop"].as_array().unwrap();
        assert!(json_event_has_command(stop, GROK_REGISTER_SCRIPT_NAME));
        let end = root["hooks"]["SessionEnd"].as_array().unwrap();
        assert!(json_event_has_command(end, GROK_DEREGISTER));
        let script = fs::read_to_string(&script_path).unwrap();
        assert!(script.contains("SESSION_GUARD_SHELL_PID"));
        assert!(script.contains("GROK_SESSION_ID"));
        assert!(script.contains("GROK_WORKSPACE_ROOT"));
        assert!(script.contains("/bin/ps"));

        // Register script must not embed bare $VAR in the JSON command field —
        // Grok expands those and fails when unset (e.g. $PPID).
        let json_text = fs::read_to_string(json_path).unwrap();
        assert!(!json_text.contains("$PPID"));
        assert!(!json_text.contains("${"));
    }

    #[test]
    fn opencode_plugin_install_is_idempotent_and_removable() {
        let dir = tempfile::tempdir().unwrap();
        let plugin_dir = dir.path().join("plugin");

        assert!(
            install_at(crate::tool("opencode"), &plugin_dir)
                .unwrap()
                .changed
        );
        assert!(
            !install_at(crate::tool("opencode"), &plugin_dir)
                .unwrap()
                .changed
        );

        let path = plugin_dir.join(OPENCODE_PLUGIN_NAME);
        let contents = fs::read_to_string(&path).unwrap();
        assert!(contents.contains("session-guard register --tool opencode"));
        assert!(contents.contains("session-guard deregister"));
        assert!(contents.contains("export const SessionGuard"));

        assert!(
            remove_at(crate::tool("opencode"), &plugin_dir)
                .unwrap()
                .changed
        );
        assert!(!path.exists());
        assert!(
            !remove_at(crate::tool("opencode"), &plugin_dir)
                .unwrap()
                .changed
        );
    }

    #[test]
    fn grok_hook_remove_deletes_owned_files() {
        let dir = tempfile::tempdir().unwrap();
        let hooks_dir = dir.path().join("hooks");
        assert!(install_at(crate::tool("grok"), &hooks_dir).unwrap().changed);
        assert!(remove_at(crate::tool("grok"), &hooks_dir).unwrap().changed);
        assert!(!hooks_dir.join(GROK_HOOKS_FILE_NAME).exists());
        assert!(!hooks_dir.join(GROK_REGISTER_SCRIPT_NAME).exists());
        assert!(!remove_at(crate::tool("grok"), &hooks_dir).unwrap().changed);
    }
}
