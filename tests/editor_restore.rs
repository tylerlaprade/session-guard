use serde_json::json;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::Command;

#[test]
fn a_restored_editor_reopens_with_its_exact_arguments() {
    let home = tempfile::tempdir().unwrap();
    let root = home.path();
    fs::create_dir_all(root.join(".config/session-guard")).unwrap();
    fs::create_dir(root.join("bin")).unwrap();
    let project = root.join("my project");
    fs::create_dir(&project).unwrap();
    fs::write(
        root.join(".config/session-guard/active-sessions.json"),
        json!([{
            "tool":"editor","session_id":"hx-1-2","directory":project,"session_name":null,
            "registered_at":"2020-01-01T00:00:00Z","state":"recoverable","restore_pending":true,
            "pid":2_147_483_647,"pid_started_at":"Wed Jan 1 00:00:00 2020",
            "command":["hx","my notes.txt","it's.md"]
        }])
        .to_string(),
    )
    .unwrap();
    let editor = root.join("bin/hx");
    fs::write(&editor, "#!/bin/sh\npwd\nprintf '[%s]\\n' \"$@\"\n").unwrap();
    fs::set_permissions(editor, fs::Permissions::from_mode(0o755)).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_session-guard"))
        .args(["launch", "--session-id", "hx-1-2"])
        .env("HOME", root)
        .env("SHELL", "/bin/sh")
        .env(
            "PATH",
            format!("{}:/usr/bin:/bin", root.join("bin").display()),
        )
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        format!(
            "{}\n[my notes.txt]\n[it's.md]\n",
            project.canonicalize().unwrap().display()
        )
    );
}
