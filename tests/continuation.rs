use serde_json::{Value, json};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::Command;

const ID: &str = "7a248d2c-865f-4829-af1b-2bee5f0c2b48";
const STARTED: &str = "Wed Jan 1 00:00:00 2020";

#[test]
fn only_confirmed_working_restores_receive_one_continuation_prompt() {
    for tool in ["claude", "codex", "grok", "opencode"] {
        for state in ["working", "idle", "waiting", "stopped", "unknown"] {
            let home = tempfile::tempdir().unwrap();
            let root = home.path();
            fs::create_dir_all(root.join(".config/session-guard")).unwrap();
            fs::create_dir(root.join("bin")).unwrap();
            let store = root.join(".config/session-guard/active-sessions.json");
            let rollout = root.join("rollout.jsonl");
            fs::write(
                &rollout,
                format!(
                    "{}\n{}\n",
                    json!({"type":"session_meta","payload":{"id":ID,"source":"cli"}}),
                    json!({"type":"event_msg","payload":{"type":"task_started","turn_id":"turn-1"}})
                ),
            )
            .unwrap();
            fs::write(&store, json!([{
                "tool":tool,"session_id":ID,"directory":root,"session_name":null,
                "registered_at":"2020-01-01T00:00:00Z","state":"recoverable","restore_pending":true,
                "pid":2_147_483_647,"pid_started_at":STARTED,
                "transcript_path":rollout,
                "activity":{"state":state,"turn_id":"turn-1","owner_pid":2_147_483_647,
                    "owner_started_at":STARTED,"pending_tools":[],"needs_attention":false}
            }]).to_string()).unwrap();
            let executable = root.join("bin").join(tool);
            fs::write(&executable, "#!/bin/sh\nprintf '%s\\n' \"$@\"\nexit 17\n").unwrap();
            fs::set_permissions(executable, fs::Permissions::from_mode(0o755)).unwrap();
            for attempt in 0..2 {
                let output = Command::new(env!("CARGO_BIN_EXE_session-guard"))
                    .args(["launch", "--session-id", ID])
                    .env("HOME", root)
                    .env("CLAUDE_CONFIG_DIR", root.join(".claude"))
                    .env("SHELL", "/bin/sh")
                    .env(
                        "PATH",
                        format!("{}:/usr/bin:/bin", root.join("bin").display()),
                    )
                    .output()
                    .unwrap();
                assert_eq!(
                    output.status.code(),
                    Some(17),
                    "{}",
                    String::from_utf8_lossy(&output.stderr)
                );
                assert_eq!(
                    String::from_utf8_lossy(&output.stdout)
                        .lines()
                        .filter(|line| *line == "continue")
                        .count(),
                    usize::from(attempt == 0 && state == "working" && tool != "opencode"),
                    "{tool} {state} {attempt}"
                );
                let records: Value = serde_json::from_slice(&fs::read(&store).unwrap()).unwrap();
                assert_eq!(records[0]["restore_pending"], true);
                assert_eq!(records[0]["activity"], Value::Null);
            }
        }
    }
}

#[test]
fn activity_hooks_and_registration_share_the_same_exact_process_owner() {
    let home = tempfile::tempdir().unwrap();
    let register = || {
        let status = Command::new(env!("CARGO_BIN_EXE_session-guard"))
            .args([
                "register",
                "--tool",
                "codex",
                "--session-id",
                ID,
                "--pid",
                &std::process::id().to_string(),
                "--directory",
            ])
            .arg(home.path())
            .env("HOME", home.path())
            .status()
            .unwrap();
        assert!(status.success());
    };
    register();
    let hook = |event, turn, tool_id| {
        use std::io::Write;
        let mut child = Command::new(env!("CARGO_BIN_EXE_session-guard"))
            .arg("activity")
            .env("HOME", home.path())
            .stdin(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(json!({"session_id":ID,"hook_event_name":event,"turn_id":turn,"tool_use_id":tool_id}).to_string().as_bytes()).unwrap();
        assert!(child.wait().unwrap().success());
        let records: Value = serde_json::from_slice(
            &fs::read(
                home.path()
                    .join(".config/session-guard/active-sessions.json"),
            )
            .unwrap(),
        )
        .unwrap();
        records[0]["activity"]["state"]
            .as_str()
            .unwrap()
            .to_string()
    };
    assert_eq!(hook("UserPromptSubmit", "a", ""), "unknown");
    assert_eq!(hook("PreToolUse", "a", "tool-1"), "waiting");
    assert_eq!(hook("PostToolUse", "a", "tool-1"), "working");
    register();
    assert_eq!(hook("Interrupt", "a", ""), "stopped");
    assert_eq!(hook("PostToolUse", "a", "tool-1"), "stopped");
    assert_eq!(hook("UserPromptSubmit", "b", ""), "unknown");
    assert_eq!(hook("Stop", "a", ""), "unknown");
    assert_eq!(hook("PreToolUse", "b", "tool-2"), "waiting");
    assert_eq!(hook("PreToolUse", "b", "tool-3"), "waiting");
    assert_eq!(hook("PostToolUse", "b", "tool-2"), "waiting");
    assert_eq!(hook("PostToolUse", "b", "tool-3"), "working");
    assert_eq!(hook("PermissionRequest", "b", "tool-4"), "waiting");
    assert_eq!(hook("Stop", "b", ""), "idle");
}
