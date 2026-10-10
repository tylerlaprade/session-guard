# session-guard

After `cargo install --path .`, restart the daemon with
`launchctl unload ~/Library/LaunchAgents/com.tylerlaprade.session-guard.plist`
and then `launchctl load` of the same plist, the pair `install.rs` runs. Never
use `launchctl kickstart -k`: launchd keeps a launch constraint pinned to the
previous build's ad hoc signature, the new binary fails it with
`OS_REASON_CODESIGNING | Launch Constraint Violation`, and the service sits in
"spawn scheduled" for good (2026-09-03). Unloading and loading registers the
agent against the new binary. Confirm with `session-guard status`
("Daemon: running") and a fresh `daemon started` line in
`~/.config/session-guard/daemon.log`.
