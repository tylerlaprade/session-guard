# session-guard

`session-guard` tracks active Claude Code, Codex CLI, and Grok sessions and
restores them after a macOS crash or reboot.

Licensed under [GPL-3.0-only](LICENSE).

## Install

```sh
cargo install --path .
session-guard install --terminal ghostty
```

Supported terminal values:

| Value | Restore behavior |
| --- | --- |
| `ghostty` | Opens tabs in Ghostty via its native AppleScript scripting dictionary (`new tab with configuration`). No keystroke automation, so no Accessibility permission is needed. |
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

State is stored in `~/.config/session-guard/active-sessions.json`. The selected
terminal is stored in `~/.config/session-guard/terminal`.

## Recovery Model

Hook identity comes from the JSON payload Claude Code, Codex, and Grok provide
on stdin. The installed hook commands store the tool PID, the parent shell
PID, and each PID's process start time (`ps lstart`) when the hook process can
derive them. Codex installs a `SessionStart` hook, a `Stop` heartbeat, and —
on Codex 0.145+ — a `SessionEnd` hook, which Codex fires only on graceful
shutdown. Grok hooks live in `~/.grok/hooks/` (global) and use a companion
register script because Grok expands `$VAR` in inline hook commands and
rejects unset vars such as `$PPID`.

The Codex desktop app, its scheduled automations, `codex exec`, and subagents
run through the same codex core and fire the same hooks, but their threads
live outside any terminal tab. Registration therefore reads the rollout's
`session_meta` for the session's own id and records only threads a terminal
owns (`source == "cli"`); the transcript fallback applies the same rule. A
rollout also embeds its parent chain's metas — a subagent file ends with the
root terminal's `cli` meta — which is why only own-id metas count. Codex asks
once to trust a new or changed hook at the next interactive launch; until
approved, that hook does not run.

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
session records and transcripts are unaffected. It also removes
verified Cargo build-target directories inside Claude scratchpads whose exact
session UUID is absent from the registry and whose transcript has been
inactive for more than 24 hours. Scratch source, patches, task output, and all
transcripts remain untouched.
