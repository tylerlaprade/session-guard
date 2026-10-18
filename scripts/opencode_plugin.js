// Installed by `session-guard install`; removed by `session-guard uninstall`.
// Do not edit: session-guard rewrites this file when its contents change.
import { spawn } from "node:child_process";
import process from "node:process";
import { createEffect, createRoot } from "solid-js";

const SESSION_GUARD = "@SESSION_GUARD@";

function sessionGuard(args, input) {
  return new Promise((resolve) => {
    const child = spawn(SESSION_GUARD, args, {
      stdio: [input === undefined ? "ignore" : "pipe", "ignore", "ignore"],
    });
    child.on("error", resolve);
    child.on("close", resolve);
    child.stdin?.end(input);
  });
}

// The TUI runs in the tab while sessions live on a shared background server,
// so this tab holds exactly the root sessions its tabs and route show.
function shownSessions(data, ui) {
  const shown = new Set(
    ui.tabs.enabled() ? ui.tabs.list().map((tab) => tab.sessionID) : [],
  );
  const route = ui.router.current();
  if (route.type === "session") {
    shown.add(data.session.root(route.sessionID));
  }
  return shown;
}

export default {
  id: "session-guard",
  setup(context) {
    const { data, ui } = context;
    const registered = new Set();

    const register = (sessionId) =>
      sessionGuard([
        "register",
        "--tool",
        "opencode",
        "--session-id",
        sessionId,
        "--pid",
        String(process.pid),
        "--directory",
        data.session.get(sessionId)?.location.directory ?? process.cwd(),
      ]);

    // The stdin (hook) form, not --session-id: the daemon then judges the
    // end by what the terminal does next, as with the other tools' SessionEnd.
    const deregister = (sessionId) =>
      sessionGuard(["deregister"], JSON.stringify({ sessionId }));

    const stopWatching = createRoot((dispose) => {
      createEffect(() => {
        const shown = shownSessions(data, ui);
        for (const sessionId of shown) {
          if (!registered.has(sessionId)) {
            registered.add(sessionId);
            register(sessionId);
          }
        }
        for (const sessionId of registered) {
          if (!shown.has(sessionId)) {
            registered.delete(sessionId);
            deregister(sessionId);
          }
        }
      });
      return dispose;
    });

    // A finished turn refreshes the record, like the other tools' Stop hook.
    const stopRefreshing = data.on("session.idle", (event) => {
      if (registered.has(event.data.sessionID)) {
        register(event.data.sessionID);
      }
    });

    return async () => {
      stopRefreshing();
      stopWatching();
      await Promise.all([...registered].map(deregister));
    };
  },
};
