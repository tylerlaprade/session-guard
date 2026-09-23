//! The harness registry.
//!
//! Everything session-guard knows about one agent CLI lives in a single
//! `Harness` entry in `HARNESSES`. Adding support for another harness means
//! adding an entry here and nothing else: no other module matches on which
//! tool it is holding. Where a harness needs genuinely bespoke parsing — a
//! session id buried in a filename, a rollout that must be filtered — the
//! entry carries a function pointer instead of forcing a new match arm
//! upstream.

use crate::process::ProcInfo;
use anyhow::Result;
use chrono::{DateTime, Utc};
use std::path::{Path, PathBuf};

/// What a discovered session tells us: its id, its working directory, and when
/// it was last active.
pub type SessionMetadata = (String, PathBuf, DateTime<Utc>);

// Every Claude frontend — terminal CLI, the desktop app, IDE extension
// sidebars, SDK/headless runs — shares ~/.claude and fires these same hooks.
// CLAUDE_CODE_ENTRYPOINT (inherited by hook subprocesses) names the frontend;
// only "cli" sessions live in a terminal tab, so only those register. A
// `claude` launched inside a VS Code integrated terminal is still "cli".
// (Headless remote workers never register via hooks anyway; the process scan
// tracks them.)
pub(crate) const CLAUDE_REGISTER: &str = r#"[ "$CLAUDE_CODE_ENTRYPOINT" = cli ] || exit 0; tool_pid="$PPID"; shell_pid="${SESSION_GUARD_SHELL_PID:-}"; [ -n "$shell_pid" ] || shell_pid="$(ps -o ppid= -p "$tool_pid" | tr -d ' ')"; if [ -n "$shell_pid" ]; then session-guard register --tool claude --pid "$tool_pid" --shell-pid "$shell_pid"; else session-guard register --tool claude --pid "$tool_pid"; fi"#;
pub(crate) const CLAUDE_DEREGISTER: &str = "session-guard deregister";
pub(crate) const CODEX_REGISTER: &str = r#"tool_pid="$PPID"; shell_pid="${SESSION_GUARD_SHELL_PID:-}"; [ -n "$shell_pid" ] || shell_pid="$(ps -o ppid= -p "$tool_pid" | tr -d ' ')"; if [ -n "$shell_pid" ]; then session-guard register --tool codex --pid "$tool_pid" --shell-pid "$shell_pid"; else session-guard register --tool codex --pid "$tool_pid"; fi"#;
pub(crate) const CODEX_DEREGISTER: &str = "session-guard deregister";
// Grok expands $VAR / ${VAR} in hook `command` strings and fails the hook when
// the var is unset. $PPID is a shell special, not an env var, so inline Claude-
// style commands break. Keep the register logic in a companion script instead.
// Grok 1.0.13+ also injects GROK_SESSION_ID / GROK_WORKSPACE_ROOT; the script
// passes those as flags because stdin JSON field names have moved.
pub(crate) const GROK_REGISTER_SCRIPT_NAME: &str = "session-guard-register.sh";
pub(crate) const GROK_HOOKS_FILE_NAME: &str = "session-guard.json";
pub(crate) const GROK_REGISTER_SCRIPT: &str = include_str!("grok_register.sh");
pub(crate) const GROK_DEREGISTER: &str = "session-guard deregister";
// OpenCode has no shell-command hooks; its extension point is a JS plugin
// module loaded into the opencode process (Bun), discovered from
// `<config>/plugin/*.js`. The plugin registers over the session event bus
// and shells out to session-guard with Bun's `$`.
pub(crate) const OPENCODE_PLUGIN_NAME: &str = "session-guard.js";
pub(crate) const OPENCODE_PLUGIN: &str = include_str!("opencode_plugin.js");

/// A path the harness owns, resolved as `$ENV/suffix` when `env` is set and
/// non-empty, else `$HOME/default/suffix`.
pub struct ToolPath {
    pub env: Option<&'static str>,
    pub default: &'static [&'static str],
    pub suffix: &'static [&'static str],
}

impl ToolPath {
    pub fn resolve(&self) -> Result<PathBuf> {
        let mut path = match self.env.and_then(std::env::var_os) {
            Some(value) if !value.is_empty() => PathBuf::from(value),
            _ => {
                let mut base = crate::paths::home_dir()?;
                base.extend(self.default);
                base
            }
        };
        path.extend(self.suffix);
        Ok(path)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum HookAction {
    Register,
    Deregister,
    Activity,
}

/// One lifecycle hook the harness should fire.
#[derive(Clone)]
pub struct HookEvent {
    pub event: &'static str,
    pub matcher: Option<&'static str>,
    pub action: HookAction,
}

use HookAction::{Deregister, Register};

/// `SessionStart` registers, `Stop` re-registers so a session that lost its
/// entry mid-life stays restorable, and `SessionEnd` clears it on a clean exit.
pub const STANDARD_HOOKS: &[HookEvent] = &[
    HookEvent {
        event: "SessionStart",
        matcher: Some("startup|resume"),
        action: Register,
    },
    HookEvent {
        event: "Stop",
        matcher: None,
        action: Register,
    },
    HookEvent {
        event: "SessionEnd",
        matcher: None,
        action: Deregister,
    },
];

/// Grok rejects matchers on lifecycle events, so `SessionStart` carries none.
pub const GROK_HOOKS: &[HookEvent] = &[
    HookEvent {
        event: "SessionStart",
        matcher: None,
        action: Register,
    },
    HookEvent {
        event: "Stop",
        matcher: None,
        action: Register,
    },
    HookEvent {
        event: "SessionEnd",
        matcher: None,
        action: Deregister,
    },
];

pub const CODEX_ACTIVITY_HOOKS: &[HookEvent] = &[
    HookEvent {
        event: "UserPromptSubmit",
        matcher: None,
        action: HookAction::Activity,
    },
    HookEvent {
        event: "PreToolUse",
        matcher: None,
        action: HookAction::Activity,
    },
    HookEvent {
        event: "PostToolUse",
        matcher: None,
        action: HookAction::Activity,
    },
    HookEvent {
        event: "PermissionRequest",
        matcher: None,
        action: HookAction::Activity,
    },
    HookEvent {
        event: "Stop",
        matcher: None,
        action: HookAction::Activity,
    },
    HookEvent {
        event: "Interrupt",
        matcher: None,
        action: HookAction::Activity,
    },
];

pub const GROK_ACTIVITY_HOOKS: &[HookEvent] = &[
    HookEvent {
        event: "UserPromptSubmit",
        matcher: None,
        action: HookAction::Activity,
    },
    HookEvent {
        event: "PreToolUse",
        matcher: None,
        action: HookAction::Activity,
    },
    HookEvent {
        event: "PostToolUse",
        matcher: None,
        action: HookAction::Activity,
    },
    HookEvent {
        event: "PostToolUseFailure",
        matcher: None,
        action: HookAction::Activity,
    },
    HookEvent {
        event: "Stop",
        matcher: None,
        action: HookAction::Activity,
    },
    HookEvent {
        event: "StopFailure",
        matcher: None,
        action: HookAction::Activity,
    },
    HookEvent {
        event: "StopCancelled",
        matcher: None,
        action: HookAction::Activity,
    },
    HookEvent {
        event: "Notification",
        matcher: None,
        action: HookAction::Activity,
    },
];

/// How session-guard installs its register/deregister hooks into the harness.
pub enum Integration {
    /// Hook arrays inside a JSON settings file.
    JsonSettings {
        path: ToolPath,
        events: &'static [HookEvent],
        register: &'static str,
        deregister: &'static str,
    },
    /// Hook tables inside a TOML config file.
    TomlConfig {
        path: ToolPath,
        events: &'static [HookEvent],
        register: &'static str,
        deregister: &'static str,
    },
    /// A hooks directory holding a companion script and a JSON manifest.
    ScriptDir {
        path: ToolPath,
        events: &'static [HookEvent],
        script_name: &'static str,
        script: &'static str,
        manifest_name: &'static str,
        deregister: &'static str,
    },
    /// A plugin module dropped into a plugin directory.
    PluginFile {
        path: ToolPath,
        name: &'static str,
        source: &'static str,
    },
}

/// How the crash-recovery scan finds sessions the hooks never recorded.
pub enum Discovery {
    /// Recursive `*.jsonl` transcripts under the harness home.
    Jsonl {
        root: &'static [&'static str],
        /// Session id from a transcript's file stem, when the name carries it.
        session_id_from_stem: fn(&str) -> Option<String>,
        /// Optional gate: reject transcripts that never held a terminal tab.
        accept: Option<fn(&Path, &str) -> bool>,
    },
    /// `<root>/<cwd>/<session>/<file>` directories holding a JSON summary.
    SummaryDirs {
        root: &'static [&'static str],
        file: &'static str,
        read: fn(&Path) -> Result<Option<SessionMetadata>>,
    },
    /// Sessions leave no per-session file to find (`OpenCode` keeps them in sqlite).
    Opaque,
}

pub struct Harness {
    pub activity_hooks: &'static [HookEvent],
    pub was_working: Option<fn(&crate::sessions::SessionRecord) -> bool>,
    /// Reads a live session's work state for the daemon to record, for a
    /// harness whose state is gone once its process exits.
    pub observe_activity:
        Option<fn(&crate::sessions::SessionRecord) -> Option<crate::continuation::Activity>>,
    /// Wire name: the `--tool` value, and how sessions are stored on disk.
    pub id: &'static str,
    /// Name for human-facing output.
    pub display_name: &'static str,
    /// Executable that must exist in PATH before hooks are installed.
    pub binary: &'static str,
    /// The harness config/state root. Sessions running inside it are internal
    /// tool activity, never user project sessions.
    pub home: ToolPath,
    /// Resume template; `{session_id}` is substituted.
    pub resume: &'static str,
    pub discovery: Discovery,
    pub integration: Integration,
    /// Recovers a live session id from a process that never fired a hook.
    pub session_id_from_process: Option<fn(&ProcInfo) -> Option<String>>,
    pub is_unused_spare: Option<fn(&Path, &str) -> bool>,
    /// Environment variable a live session exports carrying its own id.
    pub session_id_env: Option<&'static str>,
    /// Recognizes the harness's own process. Defaults to matching `binary`
    /// against the executable name when the harness needs nothing cleverer.
    pub identifies_process: Option<fn(&ProcInfo) -> bool>,
    /// Printed after a successful hook install, when the harness needs a word.
    pub install_note: Option<&'static str>,
}

pub static HARNESSES: &[Harness] = &[
    Harness {
        id: "claude",
        activity_hooks: &[],
        was_working: Some(crate::continuation::activity_was_working),
        observe_activity: Some(crate::continuation::claude_activity),
        display_name: "Claude Code",
        binary: "claude",
        home: ToolPath {
            env: Some("CLAUDE_CONFIG_DIR"),
            default: &[".claude"],
            suffix: &[],
        },
        resume: "claude --resume {session_id}",
        discovery: Discovery::Jsonl {
            root: &["projects"],
            session_id_from_stem: |stem| Some(stem.to_string()),
            accept: None,
        },
        integration: Integration::JsonSettings {
            path: ToolPath {
                env: Some("CLAUDE_CONFIG_DIR"),
                default: &[".claude"],
                suffix: &["settings.json"],
            },
            events: STANDARD_HOOKS,
            register: CLAUDE_REGISTER,
            deregister: CLAUDE_DEREGISTER,
        },
        session_id_env: Some("CLAUDE_CODE_SESSION_ID"),
        identifies_process: Some(crate::scan::is_claude_session_process),
        session_id_from_process: Some(crate::scan::claude_session_id_from_process),
        is_unused_spare: Some(crate::scan::claude_is_unused_spare),
        install_note: None,
    },
    Harness {
        id: "codex",
        activity_hooks: CODEX_ACTIVITY_HOOKS,
        was_working: Some(crate::continuation::codex_was_working),
        observe_activity: None,
        display_name: "Codex",
        binary: "codex",
        home: ToolPath {
            env: Some("CODEX_HOME"),
            default: &[".codex"],
            suffix: &[],
        },
        resume: "codex resume {session_id}",
        discovery: Discovery::Jsonl {
            root: &["sessions"],
            // `rollout-<timestamp>-<uuid>.jsonl`: the id is the trailing uuid.
            session_id_from_stem: |stem| {
                stem.len()
                    .checked_sub(36)
                    .and_then(|start| stem.get(start..))
                    .map(ToOwned::to_owned)
            },
            accept: Some(crate::transcripts::codex_rollout_is_cli),
        },
        integration: Integration::TomlConfig {
            path: ToolPath {
                env: Some("CODEX_HOME"),
                default: &[".codex"],
                suffix: &["config.toml"],
            },
            events: STANDARD_HOOKS,
            register: CODEX_REGISTER,
            deregister: CODEX_DEREGISTER,
        },
        session_id_env: Some("CODEX_THREAD_ID"),
        identifies_process: None,
        session_id_from_process: None,
        is_unused_spare: None,
        install_note: Some(
            "Codex asks once to trust new or changed hooks on the next interactive launch; they do not run until approved.",
        ),
    },
    Harness {
        id: "grok",
        activity_hooks: GROK_ACTIVITY_HOOKS,
        was_working: Some(crate::continuation::activity_was_working),
        observe_activity: None,
        display_name: "Grok",
        binary: "grok",
        home: ToolPath {
            env: Some("GROK_HOME"),
            default: &[".grok"],
            suffix: &[],
        },
        resume: "grok --resume {session_id}",
        discovery: Discovery::SummaryDirs {
            root: &["sessions"],
            file: "summary.json",
            read: crate::transcripts::read_grok_summary,
        },
        integration: Integration::ScriptDir {
            path: ToolPath {
                env: Some("GROK_HOME"),
                default: &[".grok"],
                suffix: &["hooks"],
            },
            events: GROK_HOOKS,
            script_name: GROK_REGISTER_SCRIPT_NAME,
            script: GROK_REGISTER_SCRIPT,
            manifest_name: GROK_HOOKS_FILE_NAME,
            deregister: GROK_DEREGISTER,
        },
        session_id_env: Some("GROK_SESSION_ID"),
        identifies_process: None,
        session_id_from_process: None,
        is_unused_spare: None,
        install_note: None,
    },
    Harness {
        id: "opencode",
        activity_hooks: &[],
        was_working: None,
        observe_activity: None,
        display_name: "OpenCode",
        binary: "opencode",
        // OpenCode splits config (~/.config/opencode) from state; the state
        // root is where internal activity would run.
        home: ToolPath {
            env: None,
            default: &[".local", "share", "opencode"],
            suffix: &[],
        },
        resume: "opencode --session {session_id}",
        discovery: Discovery::Opaque,
        integration: Integration::PluginFile {
            path: ToolPath {
                env: Some("OPENCODE_CONFIG_DIR"),
                default: &[".config", "opencode"],
                suffix: &["plugin"],
            },
            name: OPENCODE_PLUGIN_NAME,
            source: OPENCODE_PLUGIN,
        },
        session_id_env: None,
        identifies_process: None,
        session_id_from_process: None,
        is_unused_spare: None,
        install_note: None,
    },
];

impl Harness {
    /// Whether this process is an instance of the harness.
    pub fn owns_process(&self, process: &ProcInfo) -> bool {
        if let Some(identifies) = self.identifies_process {
            return identifies(process);
        }
        process
            .command
            .split_whitespace()
            .next()
            .and_then(|executable| Path::new(executable).file_name())
            .and_then(|name| name.to_str())
            == Some(self.binary)
    }
}

pub fn find(id: &str) -> Option<&'static Harness> {
    HARNESSES.iter().find(|harness| harness.id == id)
}

/// Harness names for help and error text, e.g. "Claude Code, Codex, or Grok".
pub fn name_list() -> String {
    let names: Vec<&str> = HARNESSES
        .iter()
        .map(|harness| harness.display_name)
        .collect();
    match names.split_last() {
        Some((last, [])) => (*last).to_string(),
        Some((last, rest)) => format!("{}, or {last}", rest.join(", ")),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::{HARNESSES, Integration, name_list};

    #[test]
    fn ids_and_binaries_are_unique() {
        let mut ids: Vec<&str> = HARNESSES.iter().map(|harness| harness.id).collect();
        let count = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), count, "harness ids must be unique");
    }

    #[test]
    fn every_resume_template_names_the_session() {
        for harness in HARNESSES {
            assert!(
                harness.resume.contains("{session_id}"),
                "{} resume template must interpolate {{session_id}}",
                harness.id
            );
        }
    }

    #[test]
    fn every_hook_integration_can_name_its_commands() {
        for harness in HARNESSES {
            let events = match &harness.integration {
                Integration::JsonSettings { events, .. }
                | Integration::TomlConfig { events, .. }
                | Integration::ScriptDir { events, .. } => events,
                Integration::PluginFile { .. } => continue,
            };
            assert!(
                !events.is_empty(),
                "{} installs hooks but declares no events",
                harness.id
            );
        }
    }

    #[test]
    fn every_path_resolves() {
        for harness in HARNESSES {
            assert!(harness.home.resolve().is_ok(), "{} home", harness.id);
            let path = match &harness.integration {
                Integration::JsonSettings { path, .. }
                | Integration::TomlConfig { path, .. }
                | Integration::ScriptDir { path, .. }
                | Integration::PluginFile { path, .. } => path,
            };
            assert!(path.resolve().is_ok(), "{} integration path", harness.id);
        }
    }

    /// The hook commands are shell held in Rust string literals, so a careless
    /// edit to the Rust around them can leak into the command and ship a
    /// broken hook. Rust syntax inside one is always that mistake.
    #[test]
    fn payloads_contain_no_rust_syntax() {
        let payloads = [
            ("CLAUDE_REGISTER", super::CLAUDE_REGISTER),
            ("CLAUDE_DEREGISTER", super::CLAUDE_DEREGISTER),
            ("CODEX_REGISTER", super::CODEX_REGISTER),
            ("CODEX_DEREGISTER", super::CODEX_DEREGISTER),
            ("GROK_DEREGISTER", super::GROK_DEREGISTER),
        ];
        for (name, payload) in payloads {
            for marker in ["pub(crate)", "&'static str", "-> Result<"] {
                assert!(
                    !payload.contains(marker),
                    "{name} contains Rust syntax {marker:?}"
                );
            }
        }
    }

    #[test]
    fn name_list_reads_as_a_sentence() {
        assert_eq!(name_list(), "Claude Code, Codex, Grok, or OpenCode");
    }
}
