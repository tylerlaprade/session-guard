use crate::Tool;
use anyhow::{Context, Result};
use std::process::Command;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcInfo {
    pub pid: i32,
    pub ppid: i32,
    pub command: String,
}

pub fn pid_is_alive(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }

    let result = unsafe { libc::kill(pid, 0) };
    if result == 0 {
        return true;
    }

    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

pub fn pid_is_tool(pid: i32, tool: Tool) -> bool {
    process_comm(pid)
        .map(|comm| {
            let comm = comm.to_ascii_lowercase();
            comm.contains(tool.as_str())
        })
        .unwrap_or(false)
}

pub fn process_comm(pid: i32) -> Result<String> {
    let output = Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "comm="])
        .output()
        .with_context(|| format!("failed to inspect pid {pid}"))?;

    if !output.status.success() {
        anyhow::bail!("ps could not inspect pid {pid}");
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

pub fn process_command(pid: i32) -> Result<String> {
    let output = Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "command="])
        .output()
        .with_context(|| format!("failed to inspect pid {pid}"))?;

    if !output.status.success() {
        anyhow::bail!("ps could not inspect pid {pid}");
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

pub fn list_processes() -> Result<Vec<ProcInfo>> {
    let output = Command::new("ps")
        .args(["-axww", "-o", "pid=,ppid=,command="])
        .output()
        .context("failed to list processes")?;

    if !output.status.success() {
        anyhow::bail!("ps could not list processes");
    }

    let text = String::from_utf8_lossy(&output.stdout);
    let mut processes = Vec::new();
    for line in text.lines() {
        let line = line.trim_start();
        let Some(pid_end) = line.find(char::is_whitespace) else {
            continue;
        };
        let pid = match line[..pid_end].parse::<i32>() {
            Ok(pid) => pid,
            Err(_) => continue,
        };

        let rest = line[pid_end..].trim_start();
        let Some(ppid_end) = rest.find(char::is_whitespace) else {
            continue;
        };
        let ppid = match rest[..ppid_end].parse::<i32>() {
            Ok(ppid) => ppid,
            Err(_) => continue,
        };

        processes.push(ProcInfo {
            pid,
            ppid,
            command: rest[ppid_end..].trim_start().to_string(),
        });
    }

    Ok(processes)
}

pub fn command_exists(name: &str) -> bool {
    std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).any(|path| is_executable(path.join(name))))
        .unwrap_or(false)
}

fn is_executable(path: impl AsRef<std::path::Path>) -> bool {
    let path = path.as_ref();
    if !path.is_file() {
        return false;
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        path.metadata()
            .map(|metadata| metadata.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }

    #[cfg(not(unix))]
    {
        true
    }
}

pub fn app_is_running(app_name: &str) -> bool {
    let script = format!("application {:?} is running", app_name);
    Command::new("osascript")
        .args(["-e", &script])
        .output()
        .ok()
        .and_then(|output| {
            output
                .status
                .success()
                .then(|| String::from_utf8_lossy(&output.stdout).trim() == "true")
        })
        .unwrap_or(false)
}

pub fn cli_process_is_running(process_name: &str) -> bool {
    Command::new("pgrep")
        .args(["-x", process_name])
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}
