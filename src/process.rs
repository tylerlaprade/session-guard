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

pub fn process_start_identity(pid: i32) -> Result<String> {
    query_process_start_identity(pid, "UTC")
}

fn query_process_start_identity(pid: i32, timezone: &str) -> Result<String> {
    let output = Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "lstart="])
        // `lstart` is textual and otherwise inherits the caller's locale and
        // timezone. Hooks and the LaunchAgent do not necessarily share either,
        // so force one stable representation before persisting/comparing it.
        .env("LC_ALL", "C")
        .env("TZ", timezone)
        .output()
        .with_context(|| format!("failed to inspect start time for pid {pid}"))?;

    if !output.status.success() {
        anyhow::bail!("ps could not inspect start time for pid {pid}");
    }

    let identity = normalize_identity(&String::from_utf8_lossy(&output.stdout));
    if identity.is_empty() {
        anyhow::bail!("ps returned no start time for pid {pid}");
    }
    Ok(identity)
}

// `lstart` pads single-digit days with a second space ("Wed Jul  2 ...").
// Identities are compared across ps invocations whose column layouts differ
// (per-pid vs the batched snapshot), so collapse runs of whitespace before
// persisting or comparing.
fn normalize_identity(raw: &str) -> String {
    raw.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// One `ps` pass over every process, replacing per-PID spawns wherever many
/// sessions are checked at once. The registry's exclusive file lock is held
/// across those checks; per-PID `ps` calls there stall hook-driven
/// register/deregister commands, and codex kills its SessionEnd hook after
/// one second.
pub struct ProcessSnapshot {
    /// pid -> (start identity, lowercased command name)
    processes: std::collections::HashMap<i32, (String, String)>,
}

impl ProcessSnapshot {
    pub fn capture() -> Result<Self> {
        let output = Command::new("ps")
            .args(["-axww", "-o", "pid=,lstart=,comm="])
            .env("LC_ALL", "C")
            .env("TZ", "UTC")
            .output()
            .context("failed to snapshot processes")?;

        if !output.status.success() {
            anyhow::bail!("ps could not snapshot processes");
        }

        let mut processes = std::collections::HashMap::new();
        for line in String::from_utf8_lossy(&output.stdout).lines() {
            let mut parts = line.split_whitespace();
            let Some(pid) = parts.next().and_then(|pid| pid.parse::<i32>().ok()) else {
                continue;
            };
            // lstart is always five fields: "Wed Jul  2 09:03:01 2026".
            let start: Vec<&str> = parts.by_ref().take(5).collect();
            if start.len() < 5 {
                continue;
            }
            let comm = parts.collect::<Vec<_>>().join(" ").to_ascii_lowercase();
            processes.insert(pid, (start.join(" "), comm));
        }

        Ok(Self { processes })
    }

    pub fn is_alive(&self, pid: i32) -> bool {
        self.processes.contains_key(&pid)
    }

    pub fn is_tool(&self, pid: i32, tool: Tool) -> bool {
        self.processes
            .get(&pid)
            .is_some_and(|(_, comm)| comm.contains(tool.as_str()))
    }

    pub fn identity_matches(&self, pid: i32, expected_start: &str) -> bool {
        self.processes
            .get(&pid)
            .is_some_and(|(start, _)| start == expected_start)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessIdentityStatus {
    Alive,
    Gone,
    Unknown,
}

pub fn process_identity_status(pid: i32, expected_start: &str) -> ProcessIdentityStatus {
    if !pid_is_alive(pid) {
        return ProcessIdentityStatus::Gone;
    }

    match process_start_identity(pid) {
        Ok(actual_start) if actual_start == expected_start => ProcessIdentityStatus::Alive,
        // The PID exists but belongs to a process started at a different time.
        Ok(_) => ProcessIdentityStatus::Gone,
        Err(_) => ProcessIdentityStatus::Unknown,
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_start_identity_is_normalized_to_utc() {
        let pid = std::process::id() as i32;
        let normalized = process_start_identity(pid).unwrap();
        let utc = query_process_start_identity(pid, "UTC").unwrap();
        let new_york = query_process_start_identity(pid, "America/New_York").unwrap();

        assert_eq!(normalized, utc);
        assert_ne!(utc, new_york, "test requires distinct timezone renderings");
    }

    #[test]
    fn snapshot_agrees_with_per_pid_identity() {
        let pid = std::process::id() as i32;
        let snapshot = ProcessSnapshot::capture().unwrap();
        let identity = process_start_identity(pid).unwrap();

        assert!(snapshot.is_alive(pid));
        assert!(snapshot.identity_matches(pid, &identity));
        assert!(!snapshot.identity_matches(pid, "Wed Jan 1 00:00:00 2020"));
        assert!(!snapshot.is_alive(-1));
    }
}
