# Cargo target isolation trial

Status: paused for new agent sessions  
Trial window: July 29 through August 12, 2026

## What happened

Queenspawn Games sends its Rust workspace builds to one shared
`.cargo-target`. Cargo locks that build directory when needed. If two commands
need the same output, one may wait and then reuse the other's work.

`session-guard cargo-target` was added after parallel agent work produced lock
waits and missing-artifact errors. It gives one agent session a separate build
directory and deletes that directory after the session ends.

Later review found another process touching the shared build directory: a
weekly `cargo-sweep` job deletes artifacts it judges unused for 14 days.
`cargo-sweep` removes those files without taking Cargo's build lock. The
original failures therefore did not show that ordinary concurrent Cargo builds
need separate targets.

Session-owned targets also have real costs. They repeat compilation, consume
more disk, and need agents to invoke a special command.

## Trial decision

Dotfiles commit `f183f91` removed the global instruction that sent isolated
builds through `session-guard cargo-target`.

The command, daemon cleanup, tests, and installed binary remain intact.
Existing sessions may retain the old instruction until they end. New sessions
should use each repository's configured Cargo target.

The weekly sweep remains enabled. A rare build failure or rebuild during that
sweep is an accepted cost for this trial.

## What counts as evidence

Lock waiting by itself is normal and does not justify isolation. Neither does a
cold rebuild after the weekly sweep.

Evidence for restoring session-owned targets must show a repeated practical
failure outside the sweep window, such as:

- a shared-target build cannot finish after the competing build ends;
- Cargo reports missing build artifacts during ordinary concurrent builds; or
- an isolated rerun is needed to obtain a trustworthy result.

Record the command, working directory, time, error, other Cargo processes, and
whether `cargo-sweep` was running. Shared-target disk growth is a separate
cleanup issue.

## Restore

The implementation does not need to be rebuilt or reinstalled. Restore this
line to `dotfiles/.claude/CLAUDE.md`:

```text
- For Rust work, reuse the repository's configured Cargo target directory by default. If isolation is genuinely necessary, run Cargo through `session-guard cargo-target -- cargo ...`; it reuses one isolated `CARGO_TARGET_DIR` owned by the current agent session and retires it after that session ends. Never create an ad hoc target directory.
```

If the trial ends without material failures, remove the Cargo feature from
`session-guard`. If material failures recur, preserve their evidence before
restoring the instruction.
