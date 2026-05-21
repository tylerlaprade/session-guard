# session-guard

`session-guard` tracks active Claude Code and Codex CLI sessions and restores
them after a macOS crash or reboot.

Licensed under [GPL-3.0-only](LICENSE).

## Install

```sh
cargo install --path .
session-guard install --terminal ghostty
```

Supported terminal values:

| Value | Restore behavior |
| --- | --- |
| `ghostty` | Opens tabs in Ghostty with AppleScript keystrokes. Requires macOS Accessibility permission for System Events. |
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
session-guard uninstall
session-guard uninstall --purge
```

State is stored in `~/.config/session-guard/active-sessions.json`. The selected
terminal is stored in `~/.config/session-guard/terminal`.

## Recovery Model

Hook identity comes from the JSON payload Claude Code and Codex provide on
stdin. The installed hook commands only use PID data as best-effort telemetry:
they store the tool PID and the parent shell PID when the hook process can
derive them. Codex installs a `SessionStart` hook plus a `Stop` heartbeat;
it does not install a made-up `SessionEnd` hook.

The daemon does not treat a dead tool PID as proof that a session should be
forgotten. During normal monitoring:

| Tool PID | Shell PID | Meaning | Action |
| --- | --- | --- | --- |
| alive | alive | Session is running | Keep active |
| dead | alive | Tool died while the tab still exists | Mark recoverable |
| dead | dead | Tool and shell exited | Remove during normal monitor |
| unknown | unknown | PID data unavailable | Keep recoverable |

At daemon startup, dead sessions from before the current boot are restored.
This keeps login recovery separate from normal process-death cleanup. Manual
`session-guard restore` restores recoverable sessions immediately.

If `active-sessions.json` is missing or had to be moved aside as corrupt,
restore can rebuild recent recoverable entries from transcript/session files:

```text
~/.claude/projects/**/*.jsonl
~/.codex/sessions/**/*.jsonl
```

Codex non-interactive `codex exec` sessions are not currently tracked by hooks.
Interactive Codex sessions are tracked and restored with
`codex resume <session-id>`.
