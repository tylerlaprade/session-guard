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
| `ghostty` | Opens tabs in Ghostty via its native AppleScript scripting dictionary (`new tab with configuration`). No keystroke automation, so no Accessibility permission is needed. Restoration waits for the surface to start and for its launcher to register a live owner. Failed launches remain pending. |
| `iterm2` | Opens tabs in the current iTerm2 window. |
| `terminal` | Uses Terminal.app `do script`. |
| `kitty` | Uses `kitty @ launch --type=tab`. |
| `wezterm` | Uses `wezterm cli spawn`. |
| `alacritty` | Alacritty has no tab control API, so restores open new windows. |

## Commands

```sh
session-guard status
session-guard restore
session-guard restore --all
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

A SessionEnd hook says the session id is finished, not why: tools fire it for
an in-tab end (quit, `/clear`, `/resume` switching away), for a tab closed
with the tool inside (Cmd-W), and while the terminal itself is quitting or
dying — and the tab's shell can be alive or dead at that instant in every one
of those cases. So the hook only marks the record `ending`. About ten seconds
later the daemon settles it: the tool or shell still running, or the terminal
instance that owned the tab still up, means the user ended it and the record
retires to `last-sessions.json`; a dead tab under a terminal that is gone (or
was relaunched after the shell started) means a teardown and the record
becomes recoverable, dated at its SessionEnd. Tabless (scan-tracked) sessions
retire on SessionEnd directly, since a graceful end is the only cleanup they
get.

The daemon records the operating system's boot identifier in its heartbeat.
A reboot restores confirmed pending interruptions, including sessions whose
death the old daemon already observed. Restarting only the daemon does not
reopen those sessions.

Liveness comes from the recorded process IDs and their start identities, not
conversation activity. A session left idle for three days is still alive.
When a tracked session is interrupted, its record becomes `pending`; there
is no two-minute death cluster, activity-age cutoff, or expiry for a confirmed
pending recovery.

Restored tabs run through `session-guard launch`. That foreground controller
registers its process identity before starting the provider in the user's
interactive shell, preserving shell wrappers. It remains alive for the command's
lifetime. Provider hooks can update the tool PID afterward, but restoration and
liveness do not depend on another hook or user prompt. A restore is confirmed
only after a fresh live owner is registered; an open tab alone is insufficient.

Registry updates replace complete, synced snapshots under a stable lock.
Pending records remain on disk throughout launch. A failed launch retains its
recovery record and reports the failure.

### Reusing Ghostty's first tab

For zsh with Ghostty's native shell integration, put this near the end of
`.zshrc`, before any plugin that must be sourced last:

```zsh
if [[ $ZSH_EVAL_CONTEXT == file && -o login && -o interactive &&
      -z $ZSH_EXECUTION_STRING && $TERM_PROGRAM == ghostty ]] &&
    (( ${+_ghostty_state} && _ghostty_state == 0 )); then
    session-guard shell-start
fi
```

The shell makes one attempt before its first prompt. It can claim one confirmed
interruption from an earlier Ghostty instance only when it is Ghostty's sole
terminal and has no queued input, including an unfinished line. The claim uses
the same registry and launcher as ordinary restoration. A shared restore lock
prevents the daemon and shell from opening the same session; a busy lock makes
the shell skip reuse without waiting. The daemon restores the remaining sessions.

Existing prompts, re-sourced configuration, subshells, multiple tabs or splits,
unknown legacy records, and failed native lookups are never reclaimed. There is
no polling of the prompt and no injected command or keystroke. Native lookup
targets the running Ghostty PID without permission to reconnect or relaunch it.

When session-guard itself launches Ghostty, it disables the default empty window
for that launch only. The first restored session creates the first window.
Both paths use the same working-only continuation policy below.

### Continuing interrupted work

Claude, Codex, and Grok restores append the ordinary prompt `continue` only
when an interrupted session has confirmed working status. Idle, stopped,
waiting, unknown, and legacy recovery records reopen without a prompt.
OpenCode currently reopens without automatic continuation.

Claude's native `sessions/<pid>.json` supplies its status. The session ID, PID,
and process start identity must match the interrupted owner, the frontend must
be an interactive CLI, and the status must be `busy` without a waiting reason.
A missing, removed, unreadable, or unfamiliar native record means no continuation.

Codex and Grok use observation-only lifecycle hooks, keyed by turn ID and exact
process owner. A prompt submission starts as unknown because another hook can
reject it. An observed tool start/completion pair confirms work; outstanding
tools remain waiting. Approval and notification events suppress continuation
for the rest of that turn. Stop, failure, and explicit interrupt events settle
it, and late events from another turn cannot revive it. No prompt, response,
tool argument, or notification message text is classified.
Codex also requires a matching native `task_started` event without a later
completion, abort, or error event. These are lifecycle attributes, not message text.

The launcher consumes the old activity before starting the restored process.
A failed launch can be retried, but does not reuse the same continuation decision.
New activity requires new native status or lifecycle evidence. Background-task
loss is not automatically continued in this version.

Run `session-guard install-hooks` after upgrading. Already-running tools may
need a new session to load added hooks. Codex asks to trust new hooks; without
that trust, conversation restoration still works but automatic continuation
has no hook evidence and remains disabled.

The daemon restores pending sessions when the terminal returns. An existing
surviving terminal process or a missed process snapshot does not count as a
relaunch. The short startup delay is only for terminal readiness.

Claude workers explicitly marked `dispatch.source = "spare"` in Claude's
native roster are excluded unless promoted to a native job. Useful detached
workers remain eligible. No conversation-content check is used.

Older recovery records whose original tab ownership is no longer known remain
available through `session-guard restore --all`. They are not guessed to be
open tabs, and their existing seven-day retention policy is unchanged. Normal
`session-guard restore` restores confirmed pending interruptions.

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
    activity_hooks: &[],
    was_working: None,
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
