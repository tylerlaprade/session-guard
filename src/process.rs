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

pub fn boot_identifier() -> Option<String> {
    #[cfg(target_os = "macos")]
    {
        let mut buffer = [0_u8; 128];
        let mut length = buffer.len();
        if unsafe {
            libc::sysctlbyname(
                c"kern.bootsessionuuid".as_ptr(),
                buffer.as_mut_ptr().cast(),
                &raw mut length,
                std::ptr::null_mut(),
                0,
            )
        } != 0
        {
            return None;
        }
        String::from_utf8(buffer.get(..length)?.to_vec())
            .ok()
            .map(|value| value.trim_end_matches('\0').to_string())
    }
    #[cfg(not(target_os = "macos"))]
    {
        std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
            .ok()
            .map(|value| value.trim().to_string())
    }
}

/// The exact argument vector a process was started with, from the kernel:
/// `ps` joins arguments with spaces, which cannot be replayed faithfully.
#[cfg(target_os = "macos")]
pub fn process_arguments(pid: i32) -> Option<Vec<String>> {
    let mut name = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid];
    let mut length = 0;
    if unsafe {
        libc::sysctl(
            name.as_mut_ptr(),
            3,
            std::ptr::null_mut(),
            &raw mut length,
            std::ptr::null_mut(),
            0,
        )
    } != 0
    {
        return None;
    }
    let mut buffer = vec![0_u8; length];
    if unsafe {
        libc::sysctl(
            name.as_mut_ptr(),
            3,
            buffer.as_mut_ptr().cast(),
            &raw mut length,
            std::ptr::null_mut(),
            0,
        )
    } != 0
    {
        return None;
    }
    buffer.truncate(length);
    parse_process_arguments(&buffer)
}

#[cfg(not(target_os = "macos"))]
pub fn process_arguments(pid: i32) -> Option<Vec<String>> {
    let raw = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    raw.strip_suffix(&[0])
        .unwrap_or(&raw)
        .split(|byte| *byte == 0)
        .map(|argument| String::from_utf8(argument.to_vec()).ok())
        .collect()
}

/// `KERN_PROCARGS2` lays out argc, the executable path, NUL padding, and then
/// argc NUL-terminated arguments followed by the environment.
fn parse_process_arguments(buffer: &[u8]) -> Option<Vec<String>> {
    let argc = usize::try_from(i32::from_ne_bytes(buffer.get(..4)?.try_into().ok()?)).ok()?;
    let rest = buffer.get(4..)?;
    let executable_end = rest.iter().position(|byte| *byte == 0)?;
    let arguments_start = executable_end
        + rest
            .get(executable_end..)?
            .iter()
            .position(|byte| *byte != 0)?;
    let arguments: Vec<String> = rest
        .get(arguments_start..)?
        .split(|byte| *byte == 0)
        .take(argc)
        .map(|argument| String::from_utf8(argument.to_vec()).ok())
        .collect::<Option<_>>()?;
    (arguments.len() == argc).then_some(arguments)
}

#[cfg(target_os = "macos")]
pub fn process_directory(pid: i32) -> Option<std::path::PathBuf> {
    let mut info = std::mem::MaybeUninit::<libc::proc_vnodepathinfo>::zeroed();
    let size = i32::try_from(std::mem::size_of::<libc::proc_vnodepathinfo>()).ok()?;
    let written = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDVNODEPATHINFO,
            0,
            info.as_mut_ptr().cast(),
            size,
        )
    };
    if written != size {
        return None;
    }
    let info = unsafe { info.assume_init() };
    let path = unsafe { std::ffi::CStr::from_ptr(info.pvi_cdir.vip_path.as_ptr().cast()) };
    Some(std::path::PathBuf::from(path.to_str().ok()?)).filter(|path| path.is_absolute())
}

#[cfg(not(target_os = "macos"))]
pub fn process_directory(pid: i32) -> Option<std::path::PathBuf> {
    std::fs::read_link(format!("/proc/{pid}/cwd")).ok()
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
/// register/deregister commands, and codex kills its `SessionEnd` hook after
/// one second.
pub struct ProcessSnapshot {
    /// pid -> (start identity, lowercased command name)
    processes: std::collections::HashMap<i32, (String, String)>,
    /// pid -> controlling terminal device, for processes that have one.
    ttys: std::collections::HashMap<i32, String>,
}

impl ProcessSnapshot {
    pub fn capture() -> Result<Self> {
        let output = Command::new("ps")
            .args(["-axww", "-o", "pid=,lstart=,stat=,tty=,comm="])
            .env("LC_ALL", "C")
            .env("TZ", "UTC")
            .output()
            .context("failed to snapshot processes")?;

        if !output.status.success() {
            anyhow::bail!("ps could not snapshot processes");
        }

        let mut processes = std::collections::HashMap::new();
        let mut ttys = std::collections::HashMap::new();
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
            let Some(state) = parts.next() else {
                continue;
            };
            if state.starts_with('Z') {
                continue;
            }
            let Some(tty) = parts.next() else {
                continue;
            };
            if tty != "??" {
                ttys.insert(pid, format!("/dev/{tty}"));
            }
            let comm = parts.collect::<Vec<_>>().join(" ").to_ascii_lowercase();
            processes.insert(pid, (start.join(" "), comm));
        }

        Ok(Self { processes, ttys })
    }

    pub fn tty(&self, pid: i32) -> Option<&str> {
        self.ttys.get(&pid).map(String::as_str)
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

    /// Whether a process running `executable` (final path component,
    /// lowercase) that started no later than `started_by` is still up. Tells a
    /// tab closed under a living terminal from a terminal that went down with
    /// its tabs: a terminal relaunched after the shell started cannot have
    /// owned it. Without a comparable start, any instance counts.
    pub fn instance_running_since(&self, executable: &str, started_by: Option<&str>) -> bool {
        let deadline = started_by.and_then(parse_identity);
        self.processes.values().any(|(start, comm)| {
            comm.rsplit('/').next() == Some(executable)
                && match (deadline, parse_identity(start)) {
                    (Some(deadline), Some(start)) => start <= deadline,
                    _ => true,
                }
        })
    }
}

impl ProcessSnapshot {
    /// Start time of the oldest running process of `executable`: the
    /// terminal instance itself, with its per-surface helpers being younger.
    pub fn instance_started_at(&self, executable: &str) -> Option<chrono::NaiveDateTime> {
        self.processes
            .values()
            .filter(|(_, comm)| comm.rsplit('/').next() == Some(executable))
            .filter_map(|(start, _)| parse_identity(start))
            .min()
    }
}

pub(crate) fn parse_identity(identity: &str) -> Option<chrono::NaiveDateTime> {
    chrono::NaiveDateTime::parse_from_str(identity, "%a %b %d %H:%M:%S %Y").ok()
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
        let Ok(pid) = line[..pid_end].parse::<i32>() else {
            continue;
        };

        let rest = line[pid_end..].trim_start();
        let Some(ppid_end) = rest.find(char::is_whitespace) else {
            continue;
        };
        let Ok(ppid) = rest[..ppid_end].parse::<i32>() else {
            continue;
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
    std::env::var_os("PATH").is_some_and(|paths| {
        std::env::split_paths(&paths).any(|path| is_executable(path.join(name)))
    })
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
            .is_ok_and(|metadata| metadata.permissions().mode() & 0o111 != 0)
    }

    #[cfg(not(unix))]
    {
        true
    }
}

pub fn app_is_running(app_name: &str) -> bool {
    Command::new("osascript")
        .args(["-e", include_str!("app_is_running.applescript"), app_name])
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
        .is_ok_and(|status| status.success())
}

#[cfg(test)]
mod tests {
    #[test]
    fn arguments_keep_spaces_and_empty_strings_apart() {
        let mut buffer = 3_i32.to_ne_bytes().to_vec();
        buffer.extend_from_slice(b"/usr/local/bin/hx\0\0\0hx\0my notes.txt\0\0PATH=/bin\0");
        assert_eq!(
            parse_process_arguments(&buffer),
            Some(vec!["hx".into(), "my notes.txt".into(), String::new()])
        );
        assert_eq!(parse_process_arguments(&buffer[..10]), None);
    }

    #[test]
    fn a_live_process_reports_its_own_arguments_and_directory() {
        let pid = std::process::id() as i32;
        assert_eq!(
            process_arguments(pid),
            Some(std::env::args().collect::<Vec<_>>())
        );
        assert_eq!(
            process_directory(pid),
            Some(std::env::current_dir().unwrap())
        );
    }

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

    #[test]
    fn identities_parse_with_unpadded_days() {
        let early = parse_identity("Thu Jul 2 09:03:01 2026").unwrap();
        let late = parse_identity("Sat Jul 11 09:03:01 2026").unwrap();
        assert!(early < late);
        assert!(parse_identity("not a start time").is_none());
    }

    #[test]
    fn instance_running_since_counts_only_processes_older_than_the_shell() {
        let snapshot = ProcessSnapshot::capture().unwrap();
        let executable = std::env::current_exe().unwrap();
        let executable = executable
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_ascii_lowercase();
        let own_start = process_start_identity(std::process::id() as i32).unwrap();

        assert!(snapshot.instance_running_since(&executable, Some(&own_start)));
        assert!(snapshot.instance_running_since(&executable, None));
        assert!(snapshot.instance_running_since(&executable, Some("Fri Jan 1 00:00:00 2100")));
        assert!(!snapshot.instance_running_since(&executable, Some("Wed Jan 1 00:00:00 2020")));
        assert!(!snapshot.instance_running_since("no-such-terminal-xyz", None));
    }
}
