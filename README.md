# session-guard

`session-guard` tracks active Claude Code, Codex CLI, Grok, and OpenCode
sessions and restores them after a macOS crash or reboot.

Licensed under [GPL-3.0-only](LICENSE).

## Install

```sh
cargo install --path .
session-guard install --terminal ghostty
```

Supported terminal values:

| Value | Restore behavior |
| --- | --- |
| `ghostty` | Opens tabs in Ghostty via its native AppleScript scripting dictionary (`new tab with configuration`). No keystroke automation, so no Accessibility permission is needed. A wedged Ghostty can return a tab id yet never start the surface process (a permanent "ghost" tab, seen under memory pressure); restore polls the new surface's working directory — reported once the shell starts, via Ghostty's default shell integration — and counts a tab that never starts as a failed restore, so the session keeps no restore cooldown and stays retryable. |
| `iterm2` | Opens tabs in the current iTerm2 window. |
| `terminal` | Uses Terminal.app `do script`. |
| `kitty` | Uses `kitty @ launch --type=tab`. |
| `wezterm` | Uses `wezterm cli spawn`. |
| `alacritty` | Alacritty has no tab control API, so restores open new windows. |

## Commands

```sh
session-guard status
session-guard restore
session-guard daemon
session-guard cargo-target -- cargo test -p my-crate
session-guard uninstall
session-guard uninstall --purge
```

Active state is stored in `~/.config/session-guard/active-sessions.json`.
Cleanly ended sessions are stored by tool and parent shell in
`~/.config/session-guard/last-sessions.json` so tab-local tools can resume the
right session. The selected terminal is stored in
`~/.config/session-guard/terminal`.

## Recovery Model

Hook identity comes from the JSON payload Claude Code, Codex, and Grok provide
on stdin, and from runner-injected environment variables when those are set.
The installed hook commands store the tool PID, the parent shell PID, and each
PID's process start time (`ps lstart`) when the hook process can derive them.
A launcher that inserts an intermediate process can pass
`SESSION_GUARD_SHELL_PID` to preserve the owning terminal shell. Codex installs
a `SessionStart` hook, a `Stop` heartbeat, and —
on Codex 0.145+ — a `SessionEnd` hook, which Codex fires only on graceful
shutdown. Grok hooks live in `~/.grok/hooks/` (global) and use a companion
register script because Grok expands `$VAR` in inline hook commands and
rejects unset vars such as `$PPID`. Grok 1.0.13+ injects `GROK_SESSION_ID`
and `GROK_WORKSPACE_ROOT` on every hook; the script passes those as flags so
register does not depend on stdin JSON field names (`cwd` vs `workspaceRoot`).

OpenCode has no shell-command hooks; session-guard installs a JS plugin at
`~/.config/opencode/plugin/session-guard.js` that OpenCode loads into its
process. The plugin registers on `session.created`/`session.updated`, uses
`session.idle` as the per-turn heartbeat, and deregisters through `dispose`
on graceful shutdown (via hook stdin, so the teardown gate applies). It
registers only interactive TUI sessions — headless modes (`opencode run`,
`serve`, `web`, `acp`) carry a subcommand in argv, which is the only mode
marker OpenCode exposes — and skips subagent child sessions (`parentID`).
Restores run `opencode --session <id>`; session ids are stable across
resume. OpenCode keeps sessions in sqlite, so the transcript fallback does
not cover it.

The Codex desktop app, its scheduled automations, `codex exec`, and subagents
run through the same codex core and fire the same hooks, but their threads
live outside any terminal tab. Registration therefore reads the rollout's
`session_meta` for the session's own id and records only threads a terminal
owns (`source == "cli"`); the transcript fallback applies the same rule. A
rollout also embeds its parent chain's metas — a subagent file ends with the
root terminal's `cli` meta — which is why only own-id metas count. Codex asks
once to trust a new or changed hook at the next interactive launch; until
approved, that hook does not run.

A SessionEnd hook only retires a session when the recorded terminal shell is
still alive — proof of an in-tab end (quit, `/clear`, `/resume` switching
away). When the shell is already gone, the SessionEnd came from a GUI
teardown (WindowServer death, logout) whose dying tools still flush their
hooks; deregistering there would erase tabs that crash restore must reopen.
Tabless (scan-tracked) sessions still deregister on SessionEnd, since a
graceful end is the only cleanup they get.

The daemon writes a heartbeat timestamp (`daemon-heartbeat`) after every
monitor pass. At startup restore, a dead session still marked active counts
as a crash victim when its `last_seen` sits near either the newest recorded
activity or that final heartbeat. The second anchor matters because jetsam
usually kills the daemon before the tools: busy sessions keep advancing
`last_seen` through their Stop hooks after the monitor dies, while idle
sessions stay frozen at the monitor's last tick and would otherwise be
misread as old closes.

The daemon does not treat a dead tool PID as proof that a session should be
forgotten. Jetsam (memory pressure) and WindowServer crashes often kill the
tool and shell while the daemon keeps running; that must not erase the
registry entry. "Alive" means the exact recorded process: when a record
carries a process start time, a recycled PID — even one now running the same
tool — counts as dead. During normal monitoring:

| Tool PID | Shell PID | Meaning | Action |
| --- | --- | --- | --- |
| alive | alive | Session is running in its tab | Keep active |
| alive | dead | Tool outlived the tab (e.g. headless worker) | Mark recoverable |
| dead | alive | Tool died while the tab still exists | Mark recoverable |
| dead | dead | Both gone (close, jetsam, or crash) | Mark recoverable |
| unknown | unknown | PID data unavailable | Keep recoverable |

Sessions leave the registry only via explicit deregister (`SessionEnd` hooks)
or after the 7-day recoverable expiry. If the shell PID is still alive,
restore never opens a new tab (the existing terminal tab is enough).

**Startup restore** reopens only unobserved deaths in the crash cluster:
sessions still marked active on disk (no monitor witnessed them die) whose
`last_seen_at` falls within 2 minutes of the newest heartbeat already in the
file (and whose tool *and* shell are dead). That brings back work that died
with the previous daemon epoch without reopening observed closes when the
daemon is merely restarted for an upgrade. **Manual** `session-guard restore`
reopens every both-dead recoverable session.

Restore runs before process scan so surviving headless workers cannot rewrite
heartbeats. After a successful tab open the record stays recoverable until
hooks re-register it live; a 30-minute cooldown prevents duplicate tabs.

If `active-sessions.json` is missing or had to be moved aside as corrupt,
restore can rebuild recent recoverable entries from transcript/session files:

```text
~/.claude/projects/**/*.jsonl
~/.codex/sessions/**/*.jsonl
~/.grok/sessions/**/summary.json
```

Interactive Codex sessions are restored with `codex resume <session-id>`.
Grok sessions are restored with `grok --resume <session-id>`. Grok subagent
sessions (`session_kind: "subagent"`) and headless runs (`-p`/`--single`,
`--prompt-file`) are never recorded. Claude sessions register only when
`CLAUDE_CODE_ENTRYPOINT` is `cli`, which excludes the desktop app, IDE
extension panes, and SDK/headless runs.

## Adding a harness

Everything session-guard knows about one agent CLI lives in a single `Harness`
entry in `src/harness.rs`. Nothing else in the codebase branches on which
harness it is holding, so supporting another one means adding an entry to
`HARNESSES` and nothing more:

```rust
Harness {
    id: "amp",                       // the --tool value, and the on-disk name
    display_name: "Amp",
    binary: "amp",                   // must be in PATH before hooks install
    home: ToolPath { env: Some("AMP_HOME"), default: &[".amp"], suffix: &[] },
    resume: "amp --resume {session_id}",
    discovery: Discovery::Jsonl {
        root: &["sessions"],         // relative to home
        session_id_from_stem: |stem| Some(stem.to_string()),
        accept: None,
    },
    integration: Integration::JsonSettings {
        path: ToolPath { env: Some("AMP_HOME"), default: &[".amp"], suffix: &["settings.json"] },
        events: STANDARD_HOOKS,
        register: AMP_REGISTER,
        deregister: AMP_DEREGISTER,
    },
    session_id_env: Some("AMP_SESSION_ID"),
    identifies_process: None,        // defaults to matching `binary`
    session_id_from_process: None,
    install_note: None,
}
```

Four integration shapes are already implemented: hook arrays in a JSON settings
file, hook tables in a TOML config, a hooks directory holding a script plus a
manifest, and a plugin module dropped into a plugin directory. A harness that
reuses one of those needs no Rust beyond its register and deregister command
strings. Only a genuinely new config format calls for a new `Integration`
variant.

Where a harness needs bespoke parsing — a session id buried in a filename, a
transcript that must be filtered before it counts — the entry carries a
function pointer rather than forcing a match arm somewhere upstream. Codex uses
both: its id is the trailing UUID of the rollout stem, and `accept` rejects
threads that never held a terminal tab.

If the harness keeps sessions somewhere the crash scan cannot read, use
`Discovery::Opaque`; hooks still track it live, only the transcript fallback
stops applying. The tests in `src/harness.rs` check every entry for a unique
id, a resume template that interpolates `{session_id}`, declared hook events,
and paths that resolve.

## Temporary Cargo targets

Use the repository's normal shared Cargo target for routine builds. When a
build genuinely needs an isolated target, run it through session-guard:

```sh
session-guard cargo-target -- cargo test -p my-crate
```

The wrapper derives the Claude, Codex, or Grok session/thread ID and verifies
the owning tool process by both PID and process start time. It then sets
`CARGO_TARGET_DIR` to a per-session directory under
`~/Library/Caches/session-guard/cargo-targets/`. It fails without running the
command if it cannot prove that ownership.

The daemon reuses the target for that session while its exact owning process
is alive. Once that PID and process-start identity are gone, it removes only
the target bearing session-guard's matching ownership marker; recoverable
session records and transcripts are unaffected. Automatic cleanup never
removes Cargo targets outside session-guard's own cache directory.
