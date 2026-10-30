use serde_json::{Value, json};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
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

#[test]
fn an_idle_restore_after_a_stop_with_background_tasks_asks_to_restart_them() {
    let home = tempfile::tempdir().unwrap();
    let root = home.path();
    let store = root.join(".config/session-guard/active-sessions.json");
    fs::create_dir(root.join("bin")).unwrap();
    let executable = root.join("bin/claude");
    fs::write(&executable, "#!/bin/sh\nprintf '%s\\n' \"$@\"\nexit 17\n").unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();
    let register = |pid: u32, mut payload: Value| {
        use std::io::Write;
        payload["cwd"] = json!(root);
        let mut child = Command::new(env!("CARGO_BIN_EXE_session-guard"))
            .args(["register", "--tool", "claude", "--pid"])
            .arg(pid.to_string())
            .env("HOME", root)
            .env("CLAUDE_CONFIG_DIR", root.join(".claude"))
            .stdin(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(payload.to_string().as_bytes())
            .unwrap();
        assert!(child.wait().unwrap().success());
        let records: Value = serde_json::from_slice(&fs::read(&store).unwrap()).unwrap();
        records[0]["background_tasks"].clone()
    };
    let stop = json!({"session_id":ID,"hook_event_name":"Stop","background_tasks":[
        {"id":"b5hzqrsid","type":"shell","status":"running","description":"QA deploy",
            "command":"sleep 40"},
        {"id":"a4505039","type":"subagent","status":"running","description":"Move memories",
            "agent_type":"general-purpose"}]});
    let owner = std::process::id();
    assert_eq!(register(owner, stop.clone())[0]["id"], "b5hzqrsid");
    let carried = register(
        owner,
        json!({"session_id":ID,"hook_event_name":"SessionStart"}),
    );
    assert_eq!(carried[1]["description"], "Move memories");
    assert_eq!(register(1, json!({"session_id":ID})), Value::Null);
    assert_eq!(register(owner, stop)[1]["id"], "a4505039");

    let mut records: Value = serde_json::from_slice(&fs::read(&store).unwrap()).unwrap();
    records[0]["pid"] = json!(2_147_483_647);
    records[0]["state"] = json!("recoverable");
    records[0]["restore_pending"] = json!(true);
    fs::write(&store, records.to_string()).unwrap();
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
        assert_eq!(output.status.code(), Some(17));
        let arguments: Vec<String> = String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(str::to_string)
            .collect();
        let expected = if attempt == 0 {
            vec![
                "--resume".to_string(),
                ID.to_string(),
                "Background tasks stopped when this session was interrupted: QA deploy (shell); \
                 Move memories (subagent). Check what each one finished. Resume a subagent or \
                 workflow where it left off rather than starting it over, and rerun anything \
                 else only if it still needs to run."
                    .to_string(),
            ]
        } else {
            vec!["--resume".to_string(), ID.to_string()]
        };
        assert_eq!(arguments, expected, "attempt {attempt}");
    }
}

#[test]
fn registration_without_a_shell_pid_takes_the_owning_shell_from_the_tool_process() {
    let home = tempfile::tempdir().unwrap();
    let register = |tool_pid: u32| {
        let status = Command::new(env!("CARGO_BIN_EXE_session-guard"))
            .args([
                "register",
                "--tool",
                "opencode",
                "--session-id",
                ID,
                "--pid",
            ])
            .arg(tool_pid.to_string())
            .arg("--directory")
            .arg(home.path())
            .env("HOME", home.path())
            .status()
            .unwrap();
        assert!(status.success());
        let records: Value = serde_json::from_slice(
            &fs::read(
                home.path()
                    .join(".config/session-guard/active-sessions.json"),
            )
            .unwrap(),
        )
        .unwrap();
        records[0]["shell_pid"].clone()
    };
    // macOS hides a platform binary's environment from other processes, so the
    // stand-in tool is session-guard itself, waiting on a child.
    let tool = || {
        let mut command = Command::new(env!("CARGO_BIN_EXE_session-guard"));
        command
            .args(["cargo-target", "--", "/bin/sleep", "30"])
            .env("HOME", home.path())
            .process_group(0);
        command
    };
    let stop = |mut tool: std::process::Child| {
        let group = i32::try_from(tool.id()).unwrap();
        unsafe {
            libc::kill(-group, libc::SIGKILL);
        }
        tool.wait().unwrap();
        assert_ne!(unsafe { libc::kill(-group, 0) }, 0, "tool group survived");
    };
    // A launcher named in the environment owns the tool only while it is the
    // tool's ancestor; Claude's background service hands sessions the name of
    // one long gone.
    let inherited = tool()
        .env("SESSION_GUARD_SHELL_PID", "424242")
        .spawn()
        .unwrap();
    assert_eq!(register(inherited.id()), json!(std::process::id()));
    stop(inherited);

    let launched = tool()
        .env("SESSION_GUARD_SHELL_PID", std::process::id().to_string())
        .spawn()
        .unwrap();
    assert_eq!(register(launched.id()), json!(std::process::id()));
    stop(launched);

    let typed = tool()
        .env_remove("SESSION_GUARD_SHELL_PID")
        .spawn()
        .unwrap();
    assert_eq!(register(typed.id()), json!(std::process::id()));
    stop(typed);
}

#[test]
fn a_message_lost_before_claude_saved_it_is_sent_again_once() {
    let home = tempfile::tempdir().unwrap();
    let root = home.path();
    let store = root.join(".config/session-guard/active-sessions.json");
    let claude_home = root.join(".claude");
    fs::create_dir_all(root.join(".config/session-guard")).unwrap();
    fs::create_dir_all(claude_home.join("paste-cache")).unwrap();
    fs::create_dir(root.join("bin")).unwrap();
    let transcript = root.join("transcript.jsonl");
    fs::write(
        &transcript,
        format!(
            "{}\n{}\n",
            json!({"type":"user","timestamp":"2020-01-01T00:00:00.000Z",
                "message":{"role":"user","content":"Name a bird."}}),
            json!({"type":"assistant","timestamp":"2020-01-01T00:00:05.000Z",
                "message":{"model":"claude-opus-5-5","content":[{"type":"text","text":"Heron."}]}})
        ),
    )
    .unwrap();
    fs::write(
        claude_home.join("paste-cache/0123abcd.txt"),
        "the pasted log",
    )
    .unwrap();
    let saved = 1_577_836_805_000_i64;
    fs::write(
        claude_home.join("history.jsonl"),
        format!(
            "{}\n{}\n{}\n",
            json!({"display":"Name a bird.","pastedContents":{},"sessionId":ID,
                "timestamp":saved - 5_000}),
            json!({"display":"Read this: [Pasted text #1 +3 lines] and fix it.",
                "pastedContents":{"1":{"id":1,"type":"text","contentHash":"0123abcd"}},
                "sessionId":ID,"timestamp":saved + 1}),
            json!({"display":"Another session's message.","pastedContents":{},
                "sessionId":"other-session","timestamp":saved + 2})
        ),
    )
    .unwrap();
    fs::write(
        &store,
        json!([{
            "tool":"claude","session_id":ID,"directory":root,"session_name":null,
            "registered_at":"2020-01-01T00:00:00Z","state":"recoverable","restore_pending":true,
            "pid":2_147_483_647,"pid_started_at":STARTED,"transcript_path":transcript
        }])
        .to_string(),
    )
    .unwrap();
    let executable = root.join("bin/claude");
    fs::write(
        &executable,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\"\nprintf '{{\"type\":\"user\",\"timestamp\":\"%s\"}}\\n' \
             \"$(date -u +%Y-%m-%dT%H:%M:%S.000Z)\" >> '{}'\nexit 17\n",
            transcript.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();
    for attempt in 0..2 {
        let output = Command::new(env!("CARGO_BIN_EXE_session-guard"))
            .args(["launch", "--session-id", ID])
            .env("HOME", root)
            .env("CLAUDE_CONFIG_DIR", &claude_home)
            .env("SHELL", "/bin/sh")
            .env(
                "PATH",
                format!("{}:/usr/bin:/bin", root.join("bin").display()),
            )
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(17));
        let expected = if attempt == 0 {
            format!(
                "--resume\n{ID}\nA crash cut this message off before it reached you:\n\n\
                 Read this: the pasted log and fix it.\n"
            )
        } else {
            format!("--resume\n{ID}\n")
        };
        assert_eq!(
            String::from_utf8_lossy(&output.stdout),
            expected,
            "attempt {attempt}"
        );
    }
}

#[test]
fn a_launcher_resumes_its_tool_in_place_only_after_a_kill_once_it_came_up() {
    for (signal, registers, relaunched) in [
        ("KILL", true, true),
        ("KILL", false, false),
        ("TERM", true, false),
        ("HUP", true, false),
    ] {
        let home = tempfile::tempdir().unwrap();
        let root = home.path();
        let store = root.join(".config/session-guard/active-sessions.json");
        fs::create_dir_all(root.join(".config/session-guard")).unwrap();
        fs::create_dir(root.join("bin")).unwrap();
        fs::write(
            &store,
            json!([{
                "tool":"claude","session_id":ID,"directory":root,"session_name":null,
                "registered_at":"2020-01-01T00:00:00Z","state":"recoverable","restore_pending":true,
                "pid":2_147_483_647,"pid_started_at":STARTED
            }])
            .to_string(),
        )
        .unwrap();
        let register = if registers {
            format!(
                "printf '{{\"session_id\":\"{ID}\",\"hook_event_name\":\"SessionStart\",\
                 \"cwd\":\"{}\"}}' | '{}' register --tool claude --pid $$\n",
                root.display(),
                env!("CARGO_BIN_EXE_session-guard")
            )
        } else {
            String::new()
        };
        let executable = root.join("bin/claude");
        fs::write(
            &executable,
            format!(
                "#!/bin/sh\nprintf 'run\\n'\nif [ -e '{0}/crashed' ]; then exit 17; fi\n\
                 touch '{0}/crashed'\n{register}kill -{signal} $$\n",
                root.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();
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
        let case = format!("{signal} registers={registers}");
        assert_eq!(
            String::from_utf8_lossy(&output.stdout).lines().count(),
            if relaunched { 2 } else { 1 },
            "{case}"
        );
        assert_eq!(
            String::from_utf8_lossy(&output.stderr).contains("Resuming it here."),
            relaunched,
            "{case}"
        );
        if relaunched {
            assert_eq!(output.status.code(), Some(17), "{case}");
        }
        let records: Value = serde_json::from_slice(&fs::read(&store).unwrap()).unwrap();
        assert_eq!(records[0]["state"], "recoverable", "{case}");
    }
}

#[test]
fn a_failed_exit_after_the_tools_session_end_is_left_for_the_daemon_to_settle() {
    for (ends_itself, state) in [(true, "ending"), (false, "recoverable")] {
        let home = tempfile::tempdir().unwrap();
        let root = home.path();
        let store = root.join(".config/session-guard/active-sessions.json");
        fs::create_dir_all(root.join(".config/session-guard")).unwrap();
        fs::create_dir(root.join("bin")).unwrap();
        fs::write(
            &store,
            json!([{
                "tool":"claude","session_id":ID,"directory":root,"session_name":null,
                "registered_at":"2020-01-01T00:00:00Z","state":"recoverable","restore_pending":true,
                "pid":2_147_483_647,"pid_started_at":STARTED
            }])
            .to_string(),
        )
        .unwrap();
        let session_end = if ends_itself {
            format!(
                "printf '{{\"session_id\":\"{ID}\",\"hook_event_name\":\"SessionEnd\"}}' | '{}' deregister\n",
                env!("CARGO_BIN_EXE_session-guard")
            )
        } else {
            String::new()
        };
        let executable = root.join("bin/claude");
        fs::write(&executable, format!("#!/bin/sh\n{session_end}exit 1\n")).unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();
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
        assert_eq!(output.status.code(), Some(1), "ends itself: {ends_itself}");
        let records: Value = serde_json::from_slice(&fs::read(&store).unwrap()).unwrap();
        assert_eq!(records[0]["state"], state, "ends itself: {ends_itself}");
    }
}
