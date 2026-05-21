use anyhow::{Context, Result};
use serde_json::{Map as JsonMap, Value as JsonValue, json};
use std::fs;
use std::path::Path;
use toml::Value as TomlValue;
use toml::map::Map as TomlMap;

const CLAUDE_REGISTER: &str = r#"tool_pid="$PPID"; shell_pid="$(ps -o ppid= -p "$tool_pid" | tr -d ' ')"; if [ -n "$shell_pid" ]; then session-guard register --tool claude --pid "$tool_pid" --shell-pid "$shell_pid"; else session-guard register --tool claude --pid "$tool_pid"; fi"#;
const CLAUDE_DEREGISTER: &str = "session-guard deregister";
const CODEX_REGISTER: &str = r#"tool_pid="$PPID"; shell_pid="$(ps -o ppid= -p "$tool_pid" | tr -d ' ')"; if [ -n "$shell_pid" ]; then session-guard register --tool codex --pid "$tool_pid" --shell-pid "$shell_pid"; else session-guard register --tool codex --pid "$tool_pid"; fi"#;
const OLD_CLAUDE_REGISTER: &str = "session-guard register --tool claude --pid \"$PPID\"";
const OLD_CODEX_REGISTER: &str = "session-guard register --tool codex --pid \"$PPID\"";
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

pub fn install_claude_hooks(path: &Path) -> Result<HookChange> {
    let mut root = read_json_config(path)?;
    let mut changed = remove_json_hook_commands(&mut root, old_hook_commands());

    changed |= ensure_json_hook(
        &mut root,
        "SessionStart",
        Some("startup|resume"),
        CLAUDE_REGISTER,
    )?;
    changed |= ensure_json_hook(&mut root, "SessionEnd", None, CLAUDE_DEREGISTER)?;

    if changed {
        write_json_config(path, &root)?;
    }

    Ok(HookChange { changed })
}

pub fn remove_claude_hooks(path: &Path) -> Result<HookChange> {
    if !path.exists() {
        return Ok(HookChange::default());
    }

    let mut root = read_json_config(path)?;
    let changed = remove_json_hook_commands(&mut root, all_hook_commands());

    if changed {
        write_json_config(path, &root)?;
    }

    Ok(HookChange { changed })
}

pub fn install_codex_hooks(path: &Path) -> Result<HookChange> {
    let mut root = read_toml_config(path)?;
    let mut changed = remove_toml_hook_commands(&mut root, old_hook_commands())?;

    changed |= ensure_toml_hook(
        &mut root,
        "SessionStart",
        Some("startup|resume"),
        CODEX_REGISTER,
    )?;
    changed |= ensure_toml_hook(&mut root, "Stop", None, CODEX_REGISTER)?;

    if changed {
        write_toml_config(path, &root)?;
    }

    Ok(HookChange { changed })
}

pub fn remove_codex_hooks(path: &Path) -> Result<HookChange> {
    if !path.exists() {
        return Ok(HookChange::default());
    }

    let mut root = read_toml_config(path)?;
    let changed = remove_toml_hook_commands(&mut root, all_hook_commands())?;

    if changed {
        write_toml_config(path, &root)?;
    }

    Ok(HookChange { changed })
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
        OLD_CLAUDE_REGISTER_ENV,
        OLD_CLAUDE_DEREGISTER_ENV,
        OLD_CODEX_REGISTER,
        OLD_CODEX_REGISTER_ENV,
        OLD_CODEX_DEREGISTER_ENV,
    ]
}

fn all_hook_commands() -> &'static [&'static str] {
    &[
        CLAUDE_REGISTER,
        CLAUDE_DEREGISTER,
        CODEX_REGISTER,
        OLD_CLAUDE_REGISTER,
        OLD_CLAUDE_REGISTER_ENV,
        OLD_CLAUDE_DEREGISTER_ENV,
        OLD_CODEX_REGISTER,
        OLD_CODEX_REGISTER_ENV,
        OLD_CODEX_DEREGISTER_ENV,
    ]
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
            .map(|hooks| {
                hooks.iter().any(|hook| {
                    hook.get("command")
                        .and_then(JsonValue::as_str)
                        .map(|existing| existing == command)
                        .unwrap_or(false)
                })
            })
            .unwrap_or(false)
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
                    .map(|command| !commands.iter().any(|needle| command == *needle))
                    .unwrap_or(true)
            });
            changed |= inner_hooks.len() != before;
        }

        let before = event_hooks.len();
        event_hooks.retain(|group| {
            group
                .get("hooks")
                .and_then(JsonValue::as_array)
                .map(|hooks| !hooks.is_empty())
                .unwrap_or(true)
        });
        changed |= event_hooks.len() != before;
    }

    let empty_events: Vec<String> = hooks
        .iter()
        .filter_map(|(event, value)| {
            value
                .as_array()
                .is_some_and(|event_hooks| event_hooks.is_empty())
                .then(|| event.clone())
        })
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
            .map(|hooks| {
                hooks.iter().any(|hook| {
                    hook.get("command")
                        .and_then(TomlValue::as_str)
                        .map(|existing| existing == command)
                        .unwrap_or(false)
                })
            })
            .unwrap_or(false)
    })
}

fn remove_toml_hook_commands(root: &mut TomlValue, commands: &[&str]) -> Result<bool> {
    let Some(hooks) = root.get_mut("hooks").and_then(TomlValue::as_table_mut) else {
        return Ok(false);
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
                    .map(|command| !commands.iter().any(|needle| command == *needle))
                    .unwrap_or(true)
            });
            changed |= inner_hooks.len() != before;
        }

        let before = event_hooks.len();
        event_hooks.retain(|group| {
            group
                .get("hooks")
                .and_then(TomlValue::as_array)
                .map(|hooks| !hooks.is_empty())
                .unwrap_or(true)
        });
        changed |= event_hooks.len() != before;
    }

    let empty_events: Vec<String> = hooks
        .iter()
        .filter_map(|(event, value)| {
            value
                .as_array()
                .is_some_and(|event_hooks| event_hooks.is_empty())
                .then(|| event.clone())
        })
        .collect();
    for event in empty_events {
        hooks.remove(&event);
        changed = true;
    }

    Ok(changed)
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

    #[test]
    fn claude_hook_install_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");

        assert!(install_claude_hooks(&path).unwrap().changed);
        assert!(!install_claude_hooks(&path).unwrap().changed);

        let root = read_json_config(&path).unwrap();
        let start = root["hooks"]["SessionStart"].as_array().unwrap();
        assert_eq!(start.len(), 1);
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

        assert!(install_claude_hooks(&path).unwrap().changed);

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

        assert!(install_codex_hooks(&path).unwrap().changed);
        assert!(!install_codex_hooks(&path).unwrap().changed);

        let root = read_toml_config(&path).unwrap();
        let start = root["hooks"]["SessionStart"].as_array().unwrap();
        assert_eq!(start.len(), 1);
        let stop = root["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(stop.len(), 1);
        let contents = fs::read_to_string(path).unwrap();
        assert!(contents.contains("[[hooks.SessionStart]]"));
        assert!(contents.contains("[[hooks.Stop]]"));
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

        assert!(install_codex_hooks(&path).unwrap().changed);

        let root = read_toml_config(&path).unwrap();
        let start = root["hooks"]["SessionStart"].as_array().unwrap();
        assert_eq!(start.len(), 1);
        assert!(toml_event_has_command(start, CODEX_REGISTER));
        assert!(!toml_event_has_command(start, OLD_CODEX_REGISTER_ENV));
    }
}
