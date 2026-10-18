use crate::Tool;
use crate::harness::ServedThread;
use crate::{paths, process};
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::os::unix::net::UnixStream;
use std::time::Duration;
use tungstenite::{Message, WebSocket};

const CONTROL_SOCKET: [&str; 2] = ["app-server-control", "app-server-control.sock"];
const TASK_TOOLS_SERVER: &str = "codex_tui";
// Codex exempts this client name from changing its global originator and User-Agent.
const CLIENT_NAME: &str = "codex_app_server_daemon";
const REPLY_TIMEOUT: Duration = Duration::from_secs(5);

pub(crate) fn is_server(pid: i32) -> bool {
    process::process_arguments(pid).is_some_and(|arguments| {
        arguments
            .iter()
            .skip(1)
            .any(|argument| argument == "app-server")
    })
}

pub(crate) fn serving(tool: Tool) -> Option<i32> {
    process::socket_peer_pid(&control_stream(tool)?)
}

fn control_stream(tool: Tool) -> Option<UnixStream> {
    let mut path = paths::tool_home(tool).ok()?;
    path.extend(CONTROL_SOCKET);
    // Codex links long socket paths into /tmp to fit macOS's 104-byte limit.
    UnixStream::connect(std::fs::canonicalize(path).ok()?).ok()
}

pub(crate) fn client(thread: &ServedThread) -> Result<Option<i32>> {
    let Some(mut connection) =
        Connection::open(thread.tool).filter(|connection| connection.server == thread.server)
    else {
        return Ok(None);
    };
    if let Err(error) = connection.request(
        "initialize",
        &json!({"clientInfo": {"name": CLIENT_NAME, "version": env!("CARGO_PKG_VERSION")}}),
    )? {
        bail!("Codex app-server rejected initialize: {error}");
    }
    connection.notify("initialized")?;
    let Ok(read) = connection.request(
        "thread/read",
        &json!({"threadId": thread.session_id, "includeTurns": false}),
    )?
    else {
        return Ok(None);
    };
    if !opened_by_a_client(&read) {
        return Ok(None);
    }
    let Ok(statuses) = connection.request(
        "mcpServerStatus/list",
        &json!({"threadId": thread.session_id, "serverName": TASK_TOOLS_SERVER, "detail": "toolsAndAuthOnly"}),
    )?
    else {
        return Ok(None);
    };
    let Some(port) = task_tools_port(&statuses) else {
        return Ok(None);
    };
    Ok(process::loopback_listener(
        port,
        &process::pids_named(thread.tool.spec().binary),
    ))
}

fn opened_by_a_client(read: &Value) -> bool {
    matches!(
        read.pointer("/thread/source").and_then(Value::as_str),
        Some("cli" | "vscode")
    )
}

fn task_tools_port(statuses: &Value) -> Option<u16> {
    statuses
        .get("data")?
        .as_array()?
        .iter()
        .find(|status| status.get("name").and_then(Value::as_str) == Some(TASK_TOOLS_SERVER))?
        .get("httpOrigin")?
        .as_str()?
        .strip_prefix("http://127.0.0.1:")?
        .parse()
        .ok()
}

struct Connection {
    socket: WebSocket<UnixStream>,
    server: i32,
    next_id: u64,
}

impl Connection {
    fn open(tool: Tool) -> Option<Self> {
        let stream = control_stream(tool)?;
        stream.set_read_timeout(Some(REPLY_TIMEOUT)).ok()?;
        stream.set_write_timeout(Some(REPLY_TIMEOUT)).ok()?;
        let server = process::socket_peer_pid(&stream)?;
        let (socket, _) = tungstenite::client("ws://localhost/", stream).ok()?;
        Some(Self {
            socket,
            server,
            next_id: 1,
        })
    }

    fn notify(&mut self, method: &str) -> Result<()> {
        self.send(&json!({"method": method}))
    }

    fn request(&mut self, method: &str, params: &Value) -> Result<Result<Value, Value>> {
        let id = self.next_id;
        self.next_id += 1;
        self.send(&json!({"id": id, "method": method, "params": params}))?;
        loop {
            let message = self
                .socket
                .read()
                .with_context(|| format!("Codex app-server did not answer {method}"))?;
            let text = match message {
                Message::Text(text) => text,
                Message::Ping(_) | Message::Pong(_) => continue,
                Message::Close(_) => {
                    bail!("Codex app-server closed before answering {method}");
                }
                Message::Binary(_) | Message::Frame(_) => {
                    bail!("Codex app-server sent a non-text reply to {method}");
                }
            };
            let mut reply: Value = serde_json::from_str(text.as_str())?;
            if reply.get("id").and_then(Value::as_u64) != Some(id) {
                continue;
            }
            return Ok(match reply.get_mut("result") {
                Some(result) => Ok(result.take()),
                None => Err(reply["error"].take()),
            });
        }
    }

    fn send(&mut self, message: &Value) -> Result<()> {
        self.socket
            .send(Message::text(message.to_string()))
            .context("failed to write to the Codex app-server")
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        let _ = self.socket.close(None);
        let _ = self.socket.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::{opened_by_a_client, task_tools_port};
    use serde_json::json;

    #[test]
    fn only_threads_a_client_opened_are_eligible() {
        for source in [json!("cli"), json!("vscode")] {
            assert!(opened_by_a_client(&json!({"thread": {"source": source}})));
        }
        for source in [
            json!({"subAgent": {"thread_spawn": {"parent_thread_id": "root", "depth": 1}}}),
            json!({"subAgent": "review"}),
            json!("exec"),
            json!("appServer"),
            json!({"custom": "automation"}),
            json!("unknown"),
        ] {
            assert!(
                !opened_by_a_client(&json!({"thread": {"source": source}})),
                "{source}"
            );
        }
        assert!(!opened_by_a_client(&json!({})));
    }

    #[test]
    fn task_tools_port_reads_the_loopback_origin() {
        let statuses = json!({"data": [
            {"name": "codex_tui", "httpOrigin": "http://127.0.0.1:61224"}
        ]});
        assert_eq!(task_tools_port(&statuses), Some(61224));
    }

    #[test]
    fn task_tools_port_ignores_other_hosts_and_servers() {
        for statuses in [
            json!({"data": [{"name": "codex_tui", "httpOrigin": "https://example.com:443"}]}),
            json!({"data": [{"name": "codex_tui", "httpOrigin": null}]}),
            json!({"data": [{"name": "other", "httpOrigin": "http://127.0.0.1:61224"}]}),
            json!({"data": []}),
        ] {
            assert_eq!(task_tools_port(&statuses), None, "{statuses}");
        }
    }
}
