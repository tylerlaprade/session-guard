use anyhow::{Context, Result};
use serde_json::{Map as JsonMap, Value as JsonValue, json};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use toml::Value as TomlValue;
use toml::map::Map as TomlMap;

// Every Claude frontend — terminal CLI, the desktop app, IDE extension
// sidebars, SDK/headless runs — shares ~/.claude and fires these same hooks.
// CLAUDE_CODE_ENTRYPOINT (inherited by hook subprocesses) names the frontend;
// only "cli" sessions live in a terminal tab, so only those register. A
// `claude` launched inside a VS Code integrated terminal is still "cli".
// (Headless remote workers never register via hooks anyway; the process scan
// tracks them.)
const CLAUDE_REGISTER: &str = r#"[ "$CLAUDE_CODE_ENTRYPOINT" = cli ] || exit 0; tool_pid="$PPID"; shell_pid="$(ps -o ppid= -p "$tool_pid" | tr -d ' ')"; if [ -n "$shell_pid" ]; then session-guard register --tool claude --pid "$tool_pid" --shell-pid "$shell_pid"; else session-guard register --tool claude --pid "$tool_pid"; fi"#;
const CLAUDE_DEREGISTER: &str = "session-guard deregister";
const CODEX_REGISTER: &str = r#"tool_pid="$PPID"; shell_pid="$(ps -o ppid= -p "$tool_pid" | tr -d ' ')"; if [ -n "$shell_pid" ]; then session-guard register --tool codex --pid "$tool_pid" --shell-pid "$shell_pid"; else session-guard register --tool codex --pid "$tool_pid"; fi"#;
const CODEX_DEREGISTER: &str = "session-guard deregister";
// Grok expands $VAR / ${VAR} in hook `command` strings and fails the hook when
// the var is unset. $PPID is a shell special, not an env var, so inline Claude-
// style commands break. Keep the register logic in a companion script instead.
const GROK_REGISTER_SCRIPT_NAME: &str = "session-guard-register.sh";
const GROK_HOOKS_FILE_NAME: &str = "session-guard.json";
const GROK_REGISTER_SCRIPT: &str = r#"#!/bin/sh
tool_pid="$PPID"
# Headless runs (-p/--single, --prompt-file) fire these hooks too, but they
# print and exit — they never hold a terminal tab, so never register them.
# Grok exposes no headless marker in the hook payload or env; the process
# argv is the only signal. (A prompt whose text contains these flags with
# surrounding spaces is misread as headless and merely goes untracked.)
case " $(ps -o command= -p "$tool_pid") " in
  *" -p "*|*" --single "*|*" --single="*|*" --prompt-file "*|*" --prompt-file="*) exit 0 ;;
esac
shell_pid="$(ps -o ppid= -p "$tool_pid" | tr -d ' ')"
if [ -n "$shell_pid" ]; then
  exec session-guard register --tool grok --pid "$tool_pid" --shell-pid "$shell_pid"
else
  exec session-guard register --tool grok --pid "$tool_pid"
fi
"#;
const GROK_DEREGISTER: &str = "session-guard deregister";
const OLD_CLAUDE_REGISTER: &str = "session-guard register --tool claude --pid \"$PPID\"";
// The pre-entrypoint-gate register command; without it in the removal lists,
// installs would leave both hooks and the ungated one would re-admit desktop
// and SDK sessions.
const OLD_CLAUDE_REGISTER_UNGATED: &str = r#"tool_pid="$PPID"; shell_pid="$(ps -o ppid= -p "$tool_pid" | tr -d ' ')"; if [ -n "$shell_pid" ]; then session-guard register --tool claude --pid "$tool_pid" --shell-pid "$shell_pid"; else session-guard register --tool claude --pid "$tool_pid"; fi"#;
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
    // Claude fires SessionStart only at creation and never re-announces a live
    // session, so a session that loses its registration mid-life stays lost: a
    // stray SessionEnd (e.g. /clear, or switching away with /resume) deregisters
    // it, and a SessionStart whose source the matcher skips (clear|compact) never
    // re-adds it. Re-registering on Stop (every turn) self-heals this the same
    // way the Codex Stop hook does, so a still-open session remains restorable.
    changed |= ensure_json_hook(&mut root, "Stop", None, CLAUDE_REGISTER)?;
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
    // Codex 0.145 added a real SessionEnd hook. It fires only on graceful
    // shutdown, so a crash still leaves the entry recoverable; a clean quit
    // deregisters instead of lingering for the 7-day expiry.
    changed |= ensure_toml_hook(&mut root, "SessionEnd", None, CODEX_DEREGISTER)?;

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

pub fn install_grok_hooks(hooks_dir: &Path) -> Result<HookChange> {
    fs::create_dir_all(hooks_dir)
        .with_context(|| format!("failed to create {}", hooks_dir.display()))?;

    let script_path = hooks_dir.join(GROK_REGISTER_SCRIPT_NAME);
    let json_path = hooks_dir.join(GROK_HOOKS_FILE_NAME);
    let mut changed = false;

    changed |= write_if_changed(&script_path, GROK_REGISTER_SCRIPT)?;
    if changed || !is_executable(&script_path) {
        let mut perms = fs::metadata(&script_path)
            .with_context(|| format!("failed to stat {}", script_path.display()))?
            .permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&script_path, perms)
            .with_context(|| format!("failed to chmod {}", script_path.display()))?;
        changed = true;
    }

    let desired = grok_hooks_json();
    changed |= write_if_changed(&json_path, &desired)?;

    Ok(HookChange { changed })
}

pub fn remove_grok_hooks(hooks_dir: &Path) -> Result<HookChange> {
    let mut changed = false;
    for name in [GROK_HOOKS_FILE_NAME, GROK_REGISTER_SCRIPT_NAME] {
        let path = hooks_dir.join(name);
        if path.exists() {
            fs::remove_file(&path)
                .with_context(|| format!("failed to remove {}", path.display()))?;
            changed = true;
        }
    }
    Ok(HookChange { changed })
}

fn grok_hooks_json() -> String {
    // SessionStart has no matcher: Grok rejects matchers on lifecycle events.
    // Stop re-registers each turn so a session that lost its registration stays
    // restorable, matching Claude/Codex.
    let root = json!({
        "hooks": {
            "SessionStart": [{
                "hooks": [{
                    "type": "command",
                    "command": GROK_REGISTER_SCRIPT_NAME
                }]
            }],
            "Stop": [{
                "hooks": [{
                    "type": "command",
                    "command": GROK_REGISTER_SCRIPT_NAME
                }]
            }],
            "SessionEnd": [{
                "hooks": [{
                    "type": "command",
                    "command": GROK_DEREGISTER
                }]
            }]
        }
    });
    let mut contents = serde_json::to_string_pretty(&root).expect("static grok hooks json");
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
    fs::metadata(path)
        .map(|metadata| metadata.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
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
        OLD_CLAUDE_REGISTER_UNGATED,
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
                    .map(|command| !commands.contains(&command))
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
        .filter(|(_, value)| {
            value
                .as_array()
                .is_some_and(|event_hooks| event_hooks.is_empty())
        })
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
                    .map(|command| !commands.contains(&command))
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
        .filter(|(_, value)| {
            value
                .as_array()
                .is_some_and(|event_hooks| event_hooks.is_empty())
        })
        .map(|(event, _)| event.clone())
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

        assert!(install_claude_hooks(&path).unwrap().changed);

        let root = read_json_config(&path).unwrap();
        let start = root["hooks"]["SessionStart"].as_array().unwrap();
        assert_eq!(start.len(), 1);
        assert!(json_event_has_command(start, CLAUDE_REGISTER));
        assert!(!json_event_has_command(start, OLD_CLAUDE_REGISTER_UNGATED));
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
        let end = root["hooks"]["SessionEnd"].as_array().unwrap();
        assert!(toml_event_has_command(end, CODEX_DEREGISTER));
        let contents = fs::read_to_string(path).unwrap();
        assert!(contents.contains("[[hooks.SessionStart]]"));
        assert!(contents.contains("[[hooks.Stop]]"));
        assert!(contents.contains("[[hooks.SessionEnd]]"));
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

    #[test]
    fn grok_hook_install_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let hooks_dir = dir.path().join("hooks");

        assert!(install_grok_hooks(&hooks_dir).unwrap().changed);
        assert!(!install_grok_hooks(&hooks_dir).unwrap().changed);

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

        // Register script must not embed bare $VAR in the JSON command field —
        // Grok expands those and fails when unset (e.g. $PPID).
        let json_text = fs::read_to_string(json_path).unwrap();
        assert!(!json_text.contains("$PPID"));
        assert!(!json_text.contains("${"));
    }

    #[test]
    fn grok_hook_remove_deletes_owned_files() {
        let dir = tempfile::tempdir().unwrap();
        let hooks_dir = dir.path().join("hooks");
        assert!(install_grok_hooks(&hooks_dir).unwrap().changed);
        assert!(remove_grok_hooks(&hooks_dir).unwrap().changed);
        assert!(!hooks_dir.join(GROK_HOOKS_FILE_NAME).exists());
        assert!(!hooks_dir.join(GROK_REGISTER_SCRIPT_NAME).exists());
        assert!(!remove_grok_hooks(&hooks_dir).unwrap().changed);
    }
}
