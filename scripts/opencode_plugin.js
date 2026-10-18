// Installed by `session-guard install`; removed by `session-guard uninstall`.
// Do not edit: session-guard rewrites this file when its contents change.
import { existsSync } from "node:fs";
import process from "node:process";

const REGISTER_THROTTLE_MS = 30_000;
const SESSION_GUARD = "@SESSION_GUARD@";

// Only interactive TUI sessions live in a terminal tab. Headless modes put a
// subcommand in argv[2] (run, serve, web, acp, session, db, ...); the TUI is
// invoked bare, with flags, or with a project path. OpenCode publishes no
// official mode marker, so argv is the only signal.
function isInteractiveTui() {
  const [, , subcommand] = process.argv;
  return !subcommand || subcommand.startsWith("-") || existsSync(subcommand);
}

export function SessionGuard({ $, directory }) {
  if (!(isInteractiveTui() && $)) {
    return {};
  }

  const lastRegistered = new Map();

  const register = async (sessionId, dir) => {
    const now = Date.now();
    if (now - (lastRegistered.get(sessionId) ?? 0) < REGISTER_THROTTLE_MS) {
      return;
    }
    lastRegistered.set(sessionId, now);
    await $`${SESSION_GUARD} register --tool opencode --session-id ${sessionId} --pid ${process.pid} --directory ${dir}`
      .quiet()
      .nothrow();
  };

  // The stdin (hook) form, not --session-id: the hook path keeps the record
  // when the tab's shell is already gone (GUI teardown), matching the other
  // tools' SessionEnd behavior.
  const deregister = async (sessionId) => {
    const payload = JSON.stringify({ sessionId });
    await $`echo ${payload} | ${SESSION_GUARD} deregister`.quiet().nothrow();
  };

  return {
    event: async ({ event }) => {
      const { type, properties } = event;
      if (type === "session.created" || type === "session.updated") {
        const { info } = properties;
        if (info.parentID) {
          return; // subagent child, never a tab
        }
        await register(info.id, info.directory || directory);
      } else if (type === "session.idle") {
        // Turn finished: force a fresh heartbeat like the other tools' Stop.
        lastRegistered.delete(properties.sessionID);
        await register(properties.sessionID, directory);
      } else if (type === "session.deleted") {
        lastRegistered.delete(properties.info.id);
        await deregister(properties.info.id);
      }
    },
    dispose: async () => {
      await Promise.all([...lastRegistered.keys()].map(deregister));
    },
  };
}
