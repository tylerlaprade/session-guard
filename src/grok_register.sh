#!/bin/sh
# shellcheck disable=SC2217 # stdin is closed on purpose; see the /bin/ps note below
tool_pid="$PPID"
# Headless runs (-p/--single/--prompt-file/--prompt-json) fire these hooks
# too, but they print and exit — they never hold a terminal tab, so never
# register them. Grok exposes no headless marker in the hook payload or env;
# the process argv is the only signal. (A prompt whose text contains these
# flags with surrounding spaces is misread as headless and merely goes
# untracked.)
# /bin/ps, stdin closed: a PATH `ps` wrapper must not run, and a piped hook
# payload must not stall ps.
cmd="$(/bin/ps -o command= -p "$tool_pid" </dev/null 2>/dev/null)"
case " $cmd " in
  *" -p "*|*" --single "*|*" --single="*|*" --prompt-file "*|*" --prompt-file="*|*" --prompt-json "*|*" --prompt-json="*)
    exit 0
    ;;
esac
# Grok 1.0.13+ injects GROK_SESSION_ID and GROK_WORKSPACE_ROOT on every hook.
# Pass them as flags so register does not depend on stdin JSON field names
# (cwd vs workspaceRoot). Older Grok still works: missing flags fall back
# to stdin.
shell_pid="${SESSION_GUARD_SHELL_PID:-}"
[ -n "$shell_pid" ] || shell_pid="$(/bin/ps -o ppid= -p "$tool_pid" </dev/null 2>/dev/null | tr -d ' ')"
set -- session-guard register --tool grok --pid "$tool_pid"
[ -n "$shell_pid" ] && set -- "$@" --shell-pid "$shell_pid"
[ -n "$GROK_SESSION_ID" ] && set -- "$@" --session-id "$GROK_SESSION_ID"
directory="${GROK_WORKSPACE_ROOT:-$CLAUDE_PROJECT_DIR}"
[ -n "$directory" ] && set -- "$@" --directory "$directory"
exec "$@"
