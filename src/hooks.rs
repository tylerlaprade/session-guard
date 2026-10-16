use crate::Tool;
use crate::adapters::shell_quote;
use crate::harness::{
    ACTIVITY_COMMAND, HookAction, HookEvent, Integration, SESSION_GUARD, ToolPath,
};
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

/// The installed binary, as the hooks name it in place of `SESSION_GUARD`.
struct Binary {
    command: String,
    literal: String,
}

impl Binary {
    fn running() -> Result<Self> {
        let path = std::env::current_exe().context("failed to locate the session-guard binary")?;
        Ok(Self::at(&path, &crate::paths::home_dir()?))
    }

    /// A binary under the home folder is named through `${HOME}`, which every
    /// harness expands, so a settings file shared between machines stays the
    /// same on each of them.
    fn at(path: &Path, home: &Path) -> Self {
        let is_plain = |path: &Path| {
            path.to_str().is_some_and(|text| {
                text.chars().all(|character| {
                    character.is_ascii_alphanumeric() || "._/+-".contains(character)
                })
            })
        };
        let command = match path.strip_prefix(home) {
            Ok(relative) if is_plain(home) && is_plain(relative) => {
                format!("${{HOME}}/{}", relative.display())
            }
            _ => shell_quote(&path.display().to_string()),
        };
        Self {
            command,
            literal: JsonValue::String(path.display().to_string()).to_string(),
        }
    }

    fn command(&self, template: &str) -> String {
        template.replace(SESSION_GUARD, &self.command)
    }

    fn plugin(&self, source: &str) -> String {
        source.replace(&format!("\"{SESSION_GUARD}\""), &self.literal)
    }
}

/// Installs the register/deregister hooks for one harness, in whatever shape
/// that harness accepts. Adding a harness that reuses one of these shapes needs
/// no code here — only a registry entry.
pub fn install(tool: Tool) -> Result<HookChange> {
    let Some(path) = config_path(&tool.spec().integration) else {
        return Ok(HookChange::default());
    };
    install_with(tool, &path.resolve()?, &Binary::running()?)
}

fn config_path(integration: &Integration) -> Option<&ToolPath> {
    match integration {
        Integration::JsonSettings { path, .. }
        | Integration::TomlConfig { path, .. }
        | Integration::ScriptDir { path, .. }
        | Integration::PluginFile { path, .. } => Some(path),
        Integration::ProcessScan => None,
    }
}

fn install_with(tool: Tool, path: &Path, binary: &Binary) -> Result<HookChange> {
    let with_activity = |events: &[HookEvent]| -> Vec<HookEvent> {
        events
            .iter()
            .chain(tool.spec().activity_hooks)
            .cloned()
            .collect()
    };
    match &tool.spec().integration {
        Integration::JsonSettings {
            events,
            register,
            deregister,
            ..
        } => install_json_hooks(path, events, register, deregister, binary),
        Integration::TomlConfig {
            events,
            register,
            deregister,
            ..
        } => install_toml_hooks(path, &with_activity(events), register, deregister, binary),
        Integration::ScriptDir {
            events,
            script_name,
            script,
            manifest_name,
            deregister,
            ..
        } => install_script_dir(
            path,
            &with_activity(events),
            script_name,
            &binary.command(script),
            manifest_name,
            deregister,
            binary,
        ),
        Integration::PluginFile { name, source, .. } => {
            install_plugin_file(path, name, &binary.plugin(source))
        }
        Integration::ProcessScan => Ok(HookChange { changed: false }),
    }
}

/// Whether session-guard's hooks are already in place for this harness, so a
/// newer build can bring them up to date without adding hooks to a tool the
/// user never installed them for.
pub fn installed(tool: Tool) -> Result<bool> {
    Ok(match &tool.spec().integration {
        Integration::JsonSettings { path, .. } => json_file_has_own_hook(&path.resolve()?)?,
        Integration::TomlConfig { path, .. } => toml_file_has_own_hook(&path.resolve()?)?,
        Integration::ScriptDir {
            path,
            manifest_name,
            ..
        } => json_file_has_own_hook(&path.resolve()?.join(manifest_name))?,
        Integration::PluginFile { path, name, .. } => path.resolve()?.join(name).is_file(),
        Integration::ProcessScan => false,
    })
}

fn json_file_has_own_hook(path: &Path) -> Result<bool> {
    Ok(json_hook_commands(&read_json_config(path)?).any(is_own_hook))
}

fn toml_file_has_own_hook(path: &Path) -> Result<bool> {
    Ok(toml_hook_commands(&read_toml_config(path)?).any(is_own_hook))
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
        Integration::ProcessScan => Ok(HookChange { changed: false }),
    }
}

#[cfg(test)]
const TEST_HOME: &str = "/Users/tester";

#[cfg(test)]
fn test_binary() -> Binary {
    Binary::at(
        Path::new("/Users/tester/.cargo/bin/session-guard"),
        Path::new(TEST_HOME),
    )
}

/// Installs into an explicit directory or file instead of the harness's real
/// path, so tests can exercise a harness end to end in a temp dir.
#[cfg(test)]
fn install_at(tool: Tool, path: &Path) -> Result<HookChange> {
    install_with(tool, path, &test_binary())
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
        Integration::ProcessScan => Ok(HookChange { changed: false }),
    }
}

/// The command template a hook runs for one lifecycle event.
fn hook_template<'a>(event: &HookEvent, register: &'a str, deregister: &'a str) -> &'a str {
    match event.action {
        HookAction::Register => register,
        HookAction::Deregister => deregister,
        HookAction::Activity => ACTIVITY_COMMAND,
    }
}

fn rendered_hooks<'a>(
    events: &'a [HookEvent],
    register: &str,
    deregister: &str,
    binary: &Binary,
) -> Vec<(&'a HookEvent, String)> {
    events
        .iter()
        .map(|event| {
            (
                event,
                binary.command(hook_template(event, register, deregister)),
            )
        })
        .collect()
}

fn install_json_hooks(
    path: &Path,
    events: &[HookEvent],
    register: &str,
    deregister: &str,
    binary: &Binary,
) -> Result<HookChange> {
    let hooks = rendered_hooks(events, register, deregister, binary);
    let mut root = read_json_config(path)?;
    let mut changed = remove_json_hook_commands(&mut root, |command| {
        is_own_hook(command) && !hooks.iter().any(|(_, current)| current == command)
    });

    for (event, command) in &hooks {
        changed |= ensure_json_hook(&mut root, event.event, event.matcher, command)?;
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
    let changed = remove_json_hook_commands(&mut root, is_own_hook);

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
    binary: &Binary,
) -> Result<HookChange> {
    let hooks = rendered_hooks(events, register, deregister, binary);
    let mut root = read_toml_config(path)?;
    let mut changed = remove_toml_hook_commands(&mut root, |command| {
        is_own_hook(command) && !hooks.iter().any(|(_, current)| current == command)
    });

    for (event, command) in &hooks {
        changed |= ensure_toml_hook(&mut root, event.event, event.matcher, command)?;
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
    let changed = remove_toml_hook_commands(&mut root, is_own_hook);

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
    binary: &Binary,
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

    let manifest = hooks_manifest_json(&rendered_hooks(events, script_name, deregister, binary));
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
fn hooks_manifest_json(hooks_by_event: &[(&HookEvent, String)]) -> String {
    let mut hooks = serde_json::Map::new();
    for (event, command) in hooks_by_event {
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

/// Every command template session-guard installs. Templates come from the
/// registry, so a new harness is covered the moment it is added.
fn hook_templates() -> impl Iterator<Item = &'static str> {
    Tool::all()
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
            Integration::PluginFile { .. } | Integration::ProcessScan => Vec::new(),
        })
        .chain([ACTIVITY_COMMAND])
}

/// Whether session-guard installed this command, current or retired, naming
/// any binary, so a reinstall replaces it and an uninstall leaves nothing.
fn is_own_hook(command: &str) -> bool {
    old_hook_commands().contains(&command)
        || hook_templates().any(|template| is_rendering_of(command, template))
}

fn is_rendering_of(command: &str, template: &str) -> bool {
    let Some((prefix, rest)) = template.split_once(SESSION_GUARD) else {
        return command == template;
    };
    let Some(from_binary) = command.strip_prefix(prefix) else {
        return false;
    };
    let after_binary = rest
        .split_once(SESSION_GUARD)
        .map_or(rest, |(next, _)| next);
    let binary_length = if after_binary.is_empty() {
        Some(from_binary.len())
    } else {
        from_binary.find(after_binary)
    };
    binary_length.is_some_and(|length| {
        length > 0 && command == template.replace(SESSION_GUARD, &from_binary[..length])
    })
}

fn json_hook_commands(root: &JsonValue) -> impl Iterator<Item = &str> {
    root.get("hooks")
        .and_then(JsonValue::as_object)
        .into_iter()
        .flat_map(|hooks| hooks.values())
        .filter_map(JsonValue::as_array)
        .flatten()
        .filter_map(|group| group.get("hooks").and_then(JsonValue::as_array))
        .flatten()
        .filter_map(|hook| hook.get("command").and_then(JsonValue::as_str))
}

fn toml_hook_commands(root: &TomlValue) -> impl Iterator<Item = &str> {
    root.get("hooks")
        .and_then(TomlValue::as_table)
        .into_iter()
        .flat_map(|hooks| hooks.values())
        .filter_map(TomlValue::as_array)
        .flatten()
        .filter_map(|group| group.get("hooks").and_then(TomlValue::as_array))
        .flatten()
        .filter_map(|hook| hook.get("command").and_then(TomlValue::as_str))
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

fn remove_json_hook_commands(root: &mut JsonValue, is_removed: impl Fn(&str) -> bool) -> bool {
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
                    .is_none_or(|command| !is_removed(command))
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

fn remove_toml_hook_commands(root: &mut TomlValue, is_removed: impl Fn(&str) -> bool) -> bool {
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
                    .is_none_or(|command| !is_removed(command))
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
        CLAUDE_DEREGISTER, CLAUDE_REGISTER, CODEX_DEREGISTER, CODEX_REGISTER, GROK_DEREGISTER,
        GROK_HOOKS_FILE_NAME, GROK_REGISTER_SCRIPT_NAME, OPENCODE_PLUGIN_NAME,
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
        assert!(json_event_has_command(
            stop,
            &test_binary().command(CLAUDE_REGISTER)
        ));
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
        assert!(json_event_has_command(
            start,
            &test_binary().command(CLAUDE_REGISTER)
        ));
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
        assert!(json_event_has_command(
            start,
            &test_binary().command(CLAUDE_REGISTER)
        ));
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
        assert!(json_event_has_command(
            start,
            &test_binary().command(CLAUDE_REGISTER)
        ));
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
        assert_eq!(stop.len(), 2);
        assert!(toml_event_has_command(
            stop,
            &test_binary().command(ACTIVITY_COMMAND)
        ));
        assert!(toml_event_has_command(
            root["hooks"]["Interrupt"].as_array().unwrap(),
            &test_binary().command(ACTIVITY_COMMAND)
        ));
        let end = root["hooks"]["SessionEnd"].as_array().unwrap();
        assert!(toml_event_has_command(
            end,
            &test_binary().command(CODEX_DEREGISTER)
        ));
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
        assert!(toml_event_has_command(
            start,
            &test_binary().command(CODEX_REGISTER)
        ));
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
command = '{OLD_CODEX_REGISTER_ENV}'
"#
            ),
        )
        .unwrap();

        assert!(install_at(crate::tool("codex"), &path).unwrap().changed);

        let root = read_toml_config(&path).unwrap();
        let start = root["hooks"]["SessionStart"].as_array().unwrap();
        assert_eq!(start.len(), 1);
        assert!(toml_event_has_command(
            start,
            &test_binary().command(CODEX_REGISTER)
        ));
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
        assert!(json_event_has_command(
            end,
            &test_binary().command(GROK_DEREGISTER)
        ));
        let script = fs::read_to_string(&script_path).unwrap();
        assert!(script.contains("SESSION_GUARD_SHELL_PID"));
        assert!(script.contains("GROK_SESSION_ID"));
        assert!(script.contains("GROK_WORKSPACE_ROOT"));
        assert!(script.contains("/bin/ps"));

        assert!(script.contains("set -- ${HOME}/.cargo/bin/session-guard register"));

        // Grok expands $VAR in the JSON command field and fails the hook when
        // the variable is unset (e.g. $PPID); only HOME is always set.
        let json_text = fs::read_to_string(json_path).unwrap();
        assert!(json_text.contains("${HOME}/.cargo/bin/session-guard deregister"));
        assert!(!json_text.replace("${HOME}", "").contains('$'));
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
        assert!(
            contents.contains(r#"const SESSION_GUARD = "/Users/tester/.cargo/bin/session-guard";"#)
        );
        assert!(contents.contains("${SESSION_GUARD} register --tool opencode"));
        assert!(contents.contains("${SESSION_GUARD} deregister"));
        assert!(!contents.contains(SESSION_GUARD));
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

    #[test]
    fn installs_name_the_binary_and_replace_bare_name_hooks() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let bare = |template: &str| template.replace(SESSION_GUARD, "session-guard");
        let unrelated = "/opt/homebrew/bin/claude-title hook";
        fs::write(
            &path,
            json!({"hooks": {
                "Stop": [
                    {"hooks": [{"type": "command", "command": bare(CLAUDE_REGISTER)}]},
                    {"hooks": [{"type": "command", "command": unrelated}]}
                ],
                "SessionEnd": [
                    {"hooks": [{"type": "command", "command": bare(CLAUDE_DEREGISTER)}]}
                ]
            }})
            .to_string(),
        )
        .unwrap();
        assert_eq!(bare(CLAUDE_DEREGISTER), "session-guard deregister");
        assert!(json_file_has_own_hook(&path).unwrap());

        assert!(install_at(crate::tool("claude"), &path).unwrap().changed);
        let root = read_json_config(&path).unwrap();
        let commands: Vec<&str> = json_hook_commands(&root).collect();
        assert!(commands.contains(&unrelated));
        assert!(commands.contains(&"${HOME}/.cargo/bin/session-guard deregister"));
        assert!(
            !commands
                .iter()
                .any(|command| command.contains("'session-guard'"))
        );
        assert_eq!(
            commands
                .iter()
                .filter(|command| command.contains("${HOME}/.cargo/bin/session-guard register"))
                .count(),
            2
        );
        assert!(json_file_has_own_hook(&path).unwrap());

        assert!(remove_json_hooks(&path).unwrap().changed);
        let root = read_json_config(&path).unwrap();
        assert_eq!(json_hook_commands(&root).collect::<Vec<_>>(), [unrelated]);
        assert!(!json_file_has_own_hook(&path).unwrap());
    }

    #[test]
    fn a_moved_binary_replaces_the_previous_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let moved = Binary::at(
            Path::new("/opt/session guard/bin/session-guard"),
            Path::new(TEST_HOME),
        );
        assert_eq!(
            moved.command(CODEX_DEREGISTER),
            "'/opt/session guard/bin/session-guard' deregister"
        );
        install_with(crate::tool("codex"), &path, &moved).unwrap();
        assert!(toml_file_has_own_hook(&path).unwrap());
        assert!(install_at(crate::tool("codex"), &path).unwrap().changed);
        let root = read_toml_config(&path).unwrap();
        assert!(!toml_hook_commands(&root).any(|command| command.contains("/opt/")));
        assert!(!install_at(crate::tool("codex"), &path).unwrap().changed);
    }

    #[test]
    fn only_exact_renderings_of_a_template_are_recognized() {
        let template = CLAUDE_REGISTER;
        assert!(is_rendering_of(&test_binary().command(template), template));
        assert!(is_rendering_of(
            &template.replace(SESSION_GUARD, "/x/sg"),
            template
        ));
        let mixed = template
            .replacen(SESSION_GUARD, "/x/sg", 1)
            .replace(SESSION_GUARD, "/y/sg");
        assert!(!is_rendering_of(&mixed, template));
        assert!(!is_rendering_of(
            &template.replace(SESSION_GUARD, ""),
            template
        ));
        assert!(!is_rendering_of(
            "session-guard deregister --all",
            CLAUDE_DEREGISTER
        ));
        assert!(!is_rendering_of(
            "/opt/homebrew/bin/claude-title hook",
            CLAUDE_DEREGISTER
        ));
    }

    #[test]
    fn hook_updates_keep_the_settings_key_order() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        fs::write(
            &path,
            r#"{"statusLine": {"type": "command"}, "env": {"Z": "1", "A": "2"},
                "hooks": {"Stop": [{"hooks": [{"type": "command", "command": "y"}]}], "PreToolUse": [{"hooks": [{"type": "command", "command": "x"}]}]}}"#,
        )
        .unwrap();
        install_at(crate::tool("claude"), &path).unwrap();
        let root = read_json_config(&path).unwrap();
        let keys = |value: &JsonValue| {
            value
                .as_object()
                .unwrap()
                .keys()
                .cloned()
                .collect::<Vec<_>>()
        };
        assert_eq!(keys(&root), ["statusLine", "env", "hooks"]);
        assert_eq!(keys(&root["env"]), ["Z", "A"]);
        assert_eq!(keys(&root["hooks"])[..2], ["Stop", "PreToolUse"]);

        let toml_path = dir.path().join("config.toml");
        fs::write(&toml_path, "model = \"m\"\napproval_policy = \"never\"\n").unwrap();
        install_at(crate::tool("codex"), &toml_path).unwrap();
        let contents = fs::read_to_string(&toml_path).unwrap();
        assert!(contents.find("model =").unwrap() < contents.find("approval_policy =").unwrap());
    }
}
