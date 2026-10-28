# session-guard

`session-guard` tracks active Claude Code, Codex CLI, Grok, and OpenCode
sessions, along with Helix, Vim, and Neovim editors opened at a shell prompt,
and restores them after a macOS crash or reboot.

Licensed under [GPL-3.0-only](LICENSE).

## Install

```sh
cargo install --path .
session-guard install --terminal ghostty
```

Supported terminal values:

| Value | Restore behavior |
| --- | --- |
| `ghostty` | Opens tabs in Ghostty via its native AppleScript scripting dictionary (`new tab with configuration`). No keystroke automation, so no Accessibility permission is needed. Restoration waits for the tab's launcher to register a live owner, which a tab whose surface never starts cannot do. Failed launches remain pending. One restore pass keeps its tabs in one window, even when another window comes to the front mid-restore. |
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
session-guard lockout
session-guard lockout --wait
session-guard attach-after-lockout <id>
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

The tools run hooks with the PATH that launched them, which lacks the login
PATH when a terminal starts a tool directly as its command. Every installed
hook therefore names the binary that installed it. A binary under the home
folder is written relative to `${HOME}`, as in
`${HOME}/.cargo/bin/session-guard`, which every harness expands and which keeps
a settings file shared between machines the same on each; any other binary is
written as its quoted absolute path. The OpenCode plugin holds the absolute
path. An install from a different binary replaces the hooks that name the old
one, including the bare `session-guard` commands of earlier versions. The other
programs the hooks run, `ps` and `tr`, live in `/bin` and `/usr/bin`, which
even that minimal PATH includes. An update rewrites only session-guard's hook
entries and keeps every other key in its place.

A session reopens in the folder its transcript began in, which is the folder it
was started in. Hooks report the folder a session has since changed into, but
the start folder decides which project instructions the session loads. A
session without a readable transcript reopens in the last folder its hooks
reported.

OpenCode has no shell-command hooks. OpenCode 2 runs sessions on a shared
background server, which cannot tell which tab shows a session, so
session-guard installs a TUI plugin at
`~/.config/opencode/plugins/session-guard/tui.js`; it runs in the tab's own
`opencode` process. The plugin registers the root sessions that TUI shows, its
open session tabs and the current route, against the TUI's process ID,
refreshes a record on `session.idle` like the other tools' Stop heartbeat, and
deregisters a session the TUI stops showing and, in its cleanup when the TUI
exits, every session it holds (via hook stdin, so the teardown gate applies).
Headless modes such as `opencode run` and `serve` start no TUI, so they never
register. An install removes the OpenCode 1 plugin that earlier releases put
at `~/.config/opencode/plugin/session-guard.js`, which OpenCode 2 rejects.
Restores run `opencode --session <id>`; session ids are stable across resume.
OpenCode keeps sessions in sqlite, so the transcript fallback does not cover
it.

The Codex desktop app, its scheduled automations, `codex exec`, and subagents
run through the same codex core and fire the same hooks, but their threads
live outside any terminal tab. Registration therefore reads the rollout's
`session_meta` for the session's own id and records only threads a terminal
opened: `source == "cli"` from a TUI running its threads in-process, or
`source == "vscode"` with the `codex-tui` originator from a TUI on the shared
app-server. The transcript fallback applies the same rule. A rollout also
embeds its parent chain's metas — a subagent file ends with the root
terminal's `cli` meta — which is why only own-id metas count. Codex asks once
to trust a new or changed hook at the next interactive launch; until
approved, that hook does not run.

Newer Codex TUIs run their threads in a shared app-server daemon, which fires
the hooks for every tab, so a hook's parent is the daemon rather than the tab's
`codex`. When the hook's parent is a Codex app-server, registration asks the
daemon serving `$CODEX_HOME/app-server-control/app-server-control.sock`,
confirmed by the socket's peer process ID, for the thread's `codex_tui` MCP
endpoint. Each TUI gives the threads it opens its own task-tools endpoint on a
loopback port, so the process listening on that port is the tab's `codex`,
and its shell owns the tab. The thread is then judged by that TUI, exactly like
an in-process one; the record also keeps the daemon's identity. Session-guard
connects as the client name Codex exempts from setting the daemon's
originator and User-Agent.

A served thread joins a tab only through a prompt a user typed into it.
Codex fires `UserPromptSubmit` only for typed input, before recording that
input, and session-guard's existing `UserPromptSubmit` hook (`session-guard
activity`) registers the thread for the daemon that started the
hook: the parent of the hook's session leader, since Codex starts every hook
in a new session. The harness is the one whose thread server that is and whose
configured transcript root holds the thread. Registration completes before
the turn's first model request, so it does not depend on reaching `Stop`.
The TUI's background tasks carry its
endpoint too, but the TUI starts their turns with a tool output, which fires
no `UserPromptSubmit`, so they stay unrecorded unless someone types into one.
The daemon must also report a client source for the thread (`cli` or
`vscode`), which no subagent, `codex exec`, or internal thread has, whatever
configuration it inherits. Desktop threads have no `codex_tui` endpoint and
stay unrecorded. Once admitted, a later hook from the same daemon keeps a TUI
that is still running; `SessionStart`, which begins a new runtime for the
thread, and a different daemon ask again for the client without needing
another prompt.

A served TUI thread that nobody has typed into yet is not recorded. Codex
fires no hook for a thread before its first turn, and the daemon exposes
nothing that tells a TUI's main thread from a background task the TUI just
started: both come from the TUI's connection with the same source, originator,
thread source, endpoint, status, and zero turns. A TUI running its threads
in-process fires no hook before the first turn either.

A SessionEnd hook says the session id is finished, not why: tools fire it for
an in-tab end (quit, `/clear`, `/resume` switching away), for a tab closed
with the tool inside (Cmd-W), and while the terminal itself is quitting or
dying — and the tab's shell can be alive or dead at that instant in every one
of those cases. So the hook only marks the record `ending`. About ten seconds
later the daemon settles it by the terminal instance that owned the tab: still
up means the user ended it and the record retires to `last-sessions.json`;
gone (or relaunched after the shell started) means a teardown and the record
becomes recoverable, dated at its SessionEnd, even when the dying tab's shell
or tool is still exiting. Without a configured terminal, a running tool or
shell means the user ended it. Tabless (scan-tracked) sessions
retire on SessionEnd directly, since a graceful end is the only cleanup they
get. A SessionEnd for an already released record leaves it released. Codex's
shared server can unload an old thread after its terminal has closed; that
later end must not turn the old record into a new crash victim. The shared
server also ends every thread it runs when it shuts down or is replaced, while
the TUIs holding them reconnect to the next one, and a replaced server can
still be ending threads after its successor has taken them over. A served
thread's SessionEnd counts only when it comes from the server its record names,
found as the parent of the hook's session leader, and that server still
serves the control socket; any other leaves the record alone. A restored
thread's record names no server until its first typed prompt, so the
SessionEnd the server fires as the restored TUI reattaches, closing the
runtime the dead TUI left loaded, leaves it alone. Codex fires the resumed
thread's `SessionStart` only with that first prompt, which admits the restored
TUI.

A TUI on the shared server fires no SessionEnd when it quits. It only
unsubscribes from its thread, and the server runs the thread's SessionEnd when
it unloads the idle thread, `thread_unload_delay_secs` later (30 minutes by
default). The TUI's exit status tells the two endings apart: `/quit` exits 0,
while a kill, a crash, or a closed tab ends it by a signal. So the daemon
watches each served TUI's process through the kernel's process events
(kqueue, with `NOTE_EXITSTATUS`), and an exit with status 0 ends the record
as a SessionEnd would. A TUI ended by a signal is handled like any death
without a SessionEnd, except that the daemon never types a restore into its
tab.

Two limits remain. The watch is macOS-only: Linux reports a process's exit
status only to its parent. And the daemon starts watching a TUI within about
a second of seeing it register, so a TUI that quits sooner than that, or while
the daemon is restarting, is missed. Its record then behaves as before the
watch: it stays recoverable while the tab's shell lives, closing the tab
releases it, the server's late SessionEnd retires it, and a terminal crash or
reboot in between reopens the thread. The missing SessionEnd is reported
upstream.

The daemon records the operating system's boot identifier in its heartbeat.
A reboot restores confirmed pending interruptions, including sessions whose
death the old daemon already observed. Restarting only the daemon does not
reopen those sessions.

Liveness comes from the recorded process IDs and their start identities, not
conversation activity. A session left idle for three days is still alive.
When a tracked session is interrupted, its record becomes `pending`; there
is no two-minute death cluster, activity-age cutoff, or expiry for a confirmed
pending recovery.

A tab can also die without a SessionEnd reaching session-guard, when a tool is
killed along with its tab or its hook fails. The daemon judges those deaths by the same
aftermath as a SessionEnd once the ten-second grace has passed. If the terminal
instance that owned the tab is still running, the user ended it: the record stops waiting for a relaunch and stays recoverable
through `restore --all` until it expires.

A tool can also die alone, killed for memory or crashed, while its tab stays
open. Closing a tab takes its shell down with the tool, so a shell that outlives
a tool that fired no SessionEnd tells the two apart, for every tool whose clean
exit always ends its record: not a Codex TUI on the shared server, and not an
editor, which has no hooks. Once the grace has passed,
the daemon types the session's `session-guard launch` into that shell's own tab,
found by its tty, when the shell sits idle at its prompt; a shell the user has
already put to other work is released like a closed tab. Typing into a tab
needs Ghostty's scripting. Other terminals release the record as before.

A restored tab's launcher handles the same death itself. When the tool is
killed by a signal that only memory pressure, a crash, or `kill -9` sends
(KILL, SEGV, BUS, ABRT, ILL, FPE, TRAP, SYS) after it registered, the launcher
claims the session again and resumes it in place, after waiting out any
critical memory pressure. A closed tab's SIGHUP, and SIGINT and SIGTERM, are
stops, and a tool that dies before it registers is not relaunched, so a crash
at startup cannot loop. Either way, the resume gets the same continuation and
lost-message handling as any restore. Any other failed exit marks the session
recoverable, unless the tool fired its SessionEnd first: Claude exits 1 on a
closed tab's SIGHUP after its SessionEnd, so that end is settled like any
other, and a tab closed under a running terminal retires its session.

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

Before its first prompt, the shell offers its tab to the daemon when it is
the sole terminal of the Ghostty that owns it, the daemon is running, and
there is no queued input, including an unfinished line. Ghostty starts every
terminal, tab or split, as its own child process, so the shell asks the kernel
for the children of the Ghostty above it: sole means that Ghostty has exactly
one, on the shell's own line of parents. The check takes no scripting call and
no process listing, so a new tab in a running Ghostty costs little more than
starting the binary. The shell decides nothing about what to restore.
An offer from a new Ghostty instance counts as that instance's relaunch, and
the daemon's ordinary restore pass runs at once. The pass opens its first
session in the offered tab instead of a new one, running the same launch
command, and opens the rest as new tabs. When nothing needs restoring, the
daemon declines within about a second and the shell shows its prompt.

The shell withdraws its offer when input arrives or after fifteen seconds
without an answer, and it rechecks for input before running what the daemon
handed it. Existing prompts, re-sourced configuration, subshells, and multiple
tabs or splits never make an offer. There is no polling of the prompt and no
injected command or keystroke.

When session-guard itself launches Ghostty, it disables the default empty window
for that launch only. The first restored session creates the first window.

### Screen Time lockouts

When Screen Time locks the terminal out, for Downtime or an app limit, AppKit
covers its native fullscreen windows. A tab opened under that cover crashes the
terminal when the lockout lifts. The daemon therefore opens no tab while the
terminal is locked out: a restore pass that finds it locked out holds its
sessions, still pending, and opens them once the lockout lifts. The hold is
kept on each session's record, so a daemon restarted during the lockout still
opens them.

Reading the policy behind a lockout takes a private entitlement, so the daemon
follows the line AppKit logs in the terminal at each change (`LockoutUI changed
state: engaged` or `disengaged`). It runs one `log stream` for that line while
it runs and keeps the state in `$TMPDIR/session-guard-lockout.json`, keyed by
the terminal's PID and start time. A daemon that starts while the terminal is
already running reads that terminal's state back from the stored log. After a
clean stop it reads only from where the previous daemon stopped following,
which takes seconds; after a crash, or for a terminal it never saw, it reads
from the terminal's launch, which can take a minute or two. Restores wait until
it knows. A pass that cannot know, because the watcher is not running, holds as
well and says why.

AppKit logs nothing for a terminal that launches unlocked, and logs `engaged`
about a second and a half after launch for one that launches locked out. To
know a launch was not locked out, the daemon also follows Screen Time's lockout
agent. The agent asks for the policy of each app launched in the foreground,
in the order it hears of the launches, and logs any lockout right after the
answer that decided it, on the same thread. It redacts the app's name in those
lines, and nothing in them ties an answer to its launch, so the daemon counts
on that order:

- When session-guard launches the terminal itself, for a restore, a background
  session's tab, or `session-guard lockout --wait`, it first opens a short
  debug-level `log stream`. That stream carries the agent's launch notices,
  which name each app and say whether it runs in the foreground, so the
  daemon knows how many answers come before the terminal's. It also reads the
  last minute of the stored log for requests still unanswered when the stream
  opened, whose answers come first. The stream closes once the agent has
  answered. A terminal that was already starting when the stream opened is
  followed from its check-in instead, as below.
- For a terminal launched any other way, the daemon's own stream sees
  LaunchServices check the terminal in, by PID. The agent's next answer, after
  the ones still due for requests already asked, is the terminal's. Another
  app launching in the same instant can take its place in that order; the
  daemon's own launches are exact.

An answer for more than one app is the one the agent asks for every running
app when its policy changes, at the start and end of Downtime, and decides no
launch. A request that fails decides the launch as unlocked, since nothing
locks an app out after a failed request; a lockout that comes later still
shows in AppKit's own line. If the agent exits while a launch waits for its
answer, the wait ends there.

Until the agent has answered for a launch, the terminal is undecided:
`session-guard lockout` answers 2, `--wait` waits, and restores hold. An answer
followed by no lockout within 250 ms counts as unlocked. Outside Downtime the
answer takes a few milliseconds; the slowest measured on a swapping Mac took
1.3 seconds. A launch session-guard makes also reads the stored log first,
which took 1.5 seconds and takes longer on a swapping Mac. A terminal that is not running counts as undecided too, because a
tab opened as it launches would come before the answer: `session-guard
lockout` answers 2 and says so, and `--wait` launches it and waits.

A Mac that has never set up Screen Time does not run the lockout agent at all,
and nothing there can be locked out, so a launch with no agent running is
decided at once.

### Kid mode

Kid mode is a layer of the Kanata keyboard remapper under which a child uses
the Mac. A tab opened then, or the terminal launching, would take the screen
from them, so the daemon treats kid mode like a lockout: `session-guard
lockout` answers 1 and says so, `--wait` waits, and restores and background
sessions' tabs wait until kid mode ends. Kanata greets each client of its TCP
port with the layer it is on; the daemon reads the port from Kanata's launch
daemon, `/Library/LaunchDaemons/com.tylerlaprade.kanata.plist`. Without that
file, or with Kanata not listening, there is no kid mode.

When the daemon does open a tab, it keeps the tab that was selected in that
window selected, so the new tab never takes the screen from whoever is using
the terminal.

A Claude session needs no tab to keep working, so a pass that finds the
terminal locked out resumes it at once with `claude --bg --resume`, sending the
same prompt a tab would, and the daemon opens a tab running `claude attach`
once the lockout lifts. Codex, Grok and OpenCode have no background mode, so
their sessions wait for the lockout to lift. A script that starts a session
with `claude --bg` itself can hand it to the daemon the same way:

```sh
session-guard attach-after-lockout 1499f9ac   # the short id `claude --bg` prints
```

The session registers through its own hooks a moment after `claude --bg`
returns, and later still on a busy Mac. A hand-off that comes first is kept
until the session registers, and the registration takes it.

Scripts that open terminal tabs can ask the same question:

```sh
session-guard lockout         # 0: not locked out, or not running; 1: locked out; 2: unknown
session-guard lockout --wait  # waits while locked out or still being read: 0 once clear, 2 if it becomes unknown
```

### Continuing interrupted work

Claude, Codex, and Grok restores append the ordinary prompt `continue` only
when an interrupted session has confirmed working status. Idle, stopped,
waiting, unknown, and legacy recovery records reopen without a prompt.
OpenCode currently reopens without automatic continuation.

Claude's native `sessions/<pid>.json` supplies its status. Claude deletes that
file as it exits, before its SessionEnd hook runs, so the daemon records the
status of each live Claude session every five seconds and the restore reads
that record. The session ID, PID, and process start identity must match the
interrupted owner, the frontend must be an interactive CLI, and the status must
be `busy` without a waiting reason. A missing, unreadable, or unfamiliar native
record reads as idle, so it means no continuation.

A crash between two samples can still leave a message unanswered. Claude saves
a message to its transcript before its turn starts, and on resume it answers a
message that has no reply with a synthetic "No response requested." placeholder,
which the model then reads as its own answer. The restore therefore also
continues a Claude session whose transcript ends in a user record: a message,
task notification, or tool result, with meta and compaction records and that
placeholder skipped. An Esc interrupt or a local command's output ends a turn,
so it means no continuation. The restore reads the transcript backward from its
end, so a long transcript costs only its last records.

A crash can also come after Claude adds a message to its prompt history,
`~/.claude/history.jsonl`, and before it saves the message to the transcript.
The resumed session has then never seen the message. When the session's latest
history entry is newer than the transcript's last record, and the transcript's
last message is not that entry's text (the transcript saves a message within
moments of its entry, then nothing until the reply starts), the restore sends
that message again as its prompt, rebuilt with its pasted text from the entry or
`paste-cache`, and says that a crash cut it off. Images are not kept in the
history, so the resend says they are lost. Slash commands and `!` shell
commands are not sent again. Once the resumed session saves the resent message,
the transcript is newer than the entry, so a later restore does not send it
again.

Codex and Grok use observation-only lifecycle hooks, keyed by turn ID and exact
process owner. A prompt submission starts as unknown because another hook can
reject it. An observed tool start/completion pair confirms work; outstanding
tools remain waiting. Approval and notification events suppress continuation
for the rest of that turn. Stop, failure, and explicit interrupt events settle
it, and late events from another turn cannot revive it. No prompt, response,
tool argument, or notification message text is classified.
Codex also requires a matching native `task_started` event without a later
completion, abort, or error event. These are lifecycle attributes, not message text.
A thread on Codex's shared app-server never gets `continue`: closing its tab
does not stop the turn, and the daemon resumes the turns it was running when
it shuts down itself.

Claude's `Stop` hook lists the tasks still running in the background as a
turn ends: background shells, agents, monitors, and the like. Registration
keeps the latest list for that exact process. When an interrupted session was
not mid-turn but had background tasks running, its restore gets a prompt that
names each task by its description and kind and asks the session to check what
each one finished. It asks for a subagent or workflow to be resumed where it left
off rather than started over, and for anything else to be rerun only if it still
needs to run, mentioning only the kinds the session lost. A session that lost no
background tasks still reopens without a prompt, and one that was mid-turn
still gets `continue`. A turn that ends without `Stop`, such as an interrupted
or failed one, leaves the previous list in place. Only Claude's hooks report
background tasks.

The launcher consumes the old activity and background-task list before
starting the restored process. A failed launch can be retried, but does not
reuse the same continuation decision. New activity requires new native status
or lifecycle evidence.

A restored session's launcher runs the session-guard build that launched it
until the session ends, then rewrites the session store. Every stored record
therefore keeps the fields that build does not know, so an older launcher
cannot erase what a newer build recorded, such as another session's background
tasks.

The daemon updates the hooks already installed each time it starts, so
reinstalling the binary and restarting the daemon brings them up to date;
`session-guard install-hooks` does the same by hand. Already-running tools may
need a new session to load added hooks. Codex asks to trust new hooks; without
that trust, automatic continuation has no hook evidence and remains disabled,
and a thread on Codex's shared app-server is never recorded, since only its
`UserPromptSubmit` hook admits it. Conversation restoration for every other
session still works.

The daemon restores pending sessions when the terminal returns. A relaunch is
a terminal instance the daemon has never seen, so an existing surviving
terminal process or a missed process snapshot does not count. Tabs of the
previous instance can take several seconds to die after it does, so the
returned terminal keeps restoring them as they die, trying each once. Tabs it
opened itself are never treated as victims. The short startup delay is only
for terminal readiness.

### Tab order

Once a minute the daemon reads Ghostty's tab order through its scripting
dictionary and records each live session's window and tab by its shell's tty.
A restore reopens tabs in that order, and a reused first tab takes the session
that was first. Tabs from several windows reopen in one window, in window
order. Sessions never seen in a tab follow in their existing order.

### Editors

Helix (`hx`), Vim, `vi`, and Neovim run with no hooks, so the daemon's
once-a-minute process scan records them instead. It keeps an editor only when
its parent is a shell and neither it nor any ancestor is a tracked session, so
`git commit` editors, an agent's editor, and an editor that a restore itself
relaunched are skipped. The record holds the editor's exact argument vector
and working directory from the kernel, and a restore replays that command.
This reopens the files the editor was launched with, not buffers opened
afterward, and never unsaved changes. Quitting the editor, or closing its tab
while the terminal stays up, drops the record.

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
threads that never held a terminal tab. A harness whose threads can run in a
separate server sets `thread_server`, which names the client holding a thread.

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
