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
on stdin. The installed hook commands only use PID data as best-effort
telemetry: they store the tool PID and the parent shell PID when the hook
process can derive them. Codex installs a `SessionStart` hook plus a `Stop`
heartbeat; it does not install a made-up `SessionEnd` hook. Grok hooks live in
`~/.grok/hooks/` (global) and use a companion register script because Grok
expands `$VAR` in inline hook commands and rejects unset vars such as `$PPID`.

The daemon does not treat a dead tool PID as proof that a session should be
forgotten. During normal monitoring:

| Tool PID | Shell PID | Meaning | Action |
| --- | --- | --- | --- |
| alive | alive | Session is running | Keep active |
| dead | alive | Tool died while the tab still exists | Mark recoverable |
| dead | dead | Tool and shell exited | Remove during normal monitor |
| unknown | unknown | PID data unavailable | Keep recoverable |

At daemon startup, sessions that were alive in the final heartbeat before the
previous daemon stopped — i.e., that died alongside it — are restored. This
keys off the daemon's own last heartbeat rather than the kernel boot time, so a
logout or GUI-session crash (which kills every terminal and the daemon without
rebooting the kernel) triggers recovery the same as a reboot does. Sessions
already sitting in the recoverable pile from earlier are left alone. Manual
`session-guard restore` restores recoverable sessions immediately.

If `active-sessions.json` is missing or had to be moved aside as corrupt,
restore can rebuild recent recoverable entries from transcript/session files:

```text
~/.claude/projects/**/*.jsonl
~/.codex/sessions/**/*.jsonl
~/.grok/sessions/**/summary.json
```

Codex non-interactive `codex exec` sessions are not currently tracked by hooks.
Interactive Codex sessions are tracked and restored with
`codex resume <session-id>`. Grok sessions are restored with
`grok --resume <session-id>`.

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
