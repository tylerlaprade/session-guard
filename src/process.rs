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

/// Kernel notice of watched processes' exits, each with its wait status.
// Apple's kqueue(2) calls NOTE_EXITSTATUS valid only for children, but the
// kernel reports it for any process the user may signal; a test pins that.
#[cfg(target_os = "macos")]
pub struct ExitWatch {
    queue: std::os::fd::OwnedFd,
}

#[cfg(target_os = "macos")]
impl ExitWatch {
    pub fn new() -> std::io::Result<Self> {
        let queue = unsafe { libc::kqueue() };
        if queue < 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(Self {
            queue: unsafe { std::os::fd::FromRawFd::from_raw_fd(queue) },
        })
    }

    /// Starts watching `pid`; false when it has already exited.
    pub fn watch(&self, pid: i32) -> bool {
        let Ok(ident) = usize::try_from(pid) else {
            return false;
        };
        let change = libc::kevent {
            ident,
            filter: libc::EVFILT_PROC,
            flags: libc::EV_ADD | libc::EV_ONESHOT,
            fflags: libc::NOTE_EXIT | libc::NOTE_EXITSTATUS,
            data: 0,
            udata: std::ptr::null_mut(),
        };
        (unsafe {
            libc::kevent(
                std::os::fd::AsRawFd::as_raw_fd(&self.queue),
                &raw const change,
                1,
                std::ptr::null_mut(),
                0,
                std::ptr::null(),
            )
        }) == 0
    }

    /// Watched processes that exited, as (pid, wait status), waiting up to
    /// `timeout` for the first.
    pub fn exits(&self, timeout: std::time::Duration) -> Vec<(i32, i32)> {
        let mut events: [libc::kevent; 16] = unsafe { std::mem::zeroed() };
        let wait = libc::timespec {
            tv_sec: libc::time_t::try_from(timeout.as_secs()).unwrap_or(libc::time_t::MAX),
            tv_nsec: libc::c_long::from(timeout.subsec_nanos()),
        };
        let count = unsafe {
            libc::kevent(
                std::os::fd::AsRawFd::as_raw_fd(&self.queue),
                std::ptr::null(),
                0,
                events.as_mut_ptr(),
                libc::c_int::try_from(events.len()).unwrap_or(libc::c_int::MAX),
                &raw const wait,
            )
        };
        events
            .iter()
            .take(usize::try_from(count).unwrap_or(0))
            .filter(|event| event.fflags & libc::NOTE_EXIT != 0)
            .filter_map(|event| {
                Some((
                    i32::try_from(event.ident).ok()?,
                    i32::try_from(event.data).ok()?,
                ))
            })
            .collect()
    }
}

#[cfg(not(target_os = "macos"))]
pub struct ExitWatch;

#[cfg(not(target_os = "macos"))]
impl ExitWatch {
    pub fn new() -> std::io::Result<Self> {
        Err(std::io::Error::from(std::io::ErrorKind::Unsupported))
    }

    pub fn watch(&self, _pid: i32) -> bool {
        false
    }

    pub fn exits(&self, timeout: std::time::Duration) -> Vec<(i32, i32)> {
        std::thread::sleep(timeout);
        Vec::new()
    }
}

/// Whether the kernel reports critical memory pressure, the level at which it
/// kills processes to reclaim memory.
#[cfg(target_os = "macos")]
pub fn memory_pressure_is_critical() -> bool {
    const CRITICAL: libc::c_int = 4;
    let mut level: libc::c_int = 0;
    let mut length = std::mem::size_of::<libc::c_int>();
    (unsafe {
        libc::sysctlbyname(
            c"kern.memorystatus_vm_pressure_level".as_ptr(),
            (&raw mut level).cast(),
            &raw mut length,
            std::ptr::null_mut(),
            0,
        )
    }) == 0
        && level >= CRITICAL
}

#[cfg(not(target_os = "macos"))]
pub fn memory_pressure_is_critical() -> bool {
    false
}

/// The exact argument vector a process was started with, from the kernel:
/// `ps` joins arguments with spaces, which cannot be replayed faithfully.
#[cfg(target_os = "macos")]
pub fn process_arguments(pid: i32) -> Option<Vec<String>> {
    parse_process_arguments(&process_arguments_buffer(pid)?)
}

#[cfg(target_os = "macos")]
pub fn process_environment_variable(pid: i32, name: &str) -> Option<String> {
    parse_process_environment_variable(&process_arguments_buffer(pid)?, name)
}

#[cfg(not(target_os = "macos"))]
pub fn process_environment_variable(pid: i32, name: &str) -> Option<String> {
    std::fs::read(format!("/proc/{pid}/environ"))
        .ok()?
        .split(|byte| *byte == 0)
        .find_map(|entry| environment_value(entry, name))
}

#[cfg(target_os = "macos")]
fn process_arguments_buffer(pid: i32) -> Option<Vec<u8>> {
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
    Some(buffer)
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
    let (argc, strings) = process_strings(buffer)?;
    let arguments: Vec<String> = strings
        .take(argc)
        .map(|argument| String::from_utf8(argument.to_vec()).ok())
        .collect::<Option<_>>()?;
    (arguments.len() == argc).then_some(arguments)
}

fn parse_process_environment_variable(buffer: &[u8], name: &str) -> Option<String> {
    let (argc, strings) = process_strings(buffer)?;
    strings
        .skip(argc)
        .take_while(|entry| !entry.is_empty())
        .find_map(|entry| environment_value(entry, name))
}

fn process_strings(buffer: &[u8]) -> Option<(usize, impl Iterator<Item = &[u8]>)> {
    let argc = usize::try_from(i32::from_ne_bytes(buffer.get(..4)?.try_into().ok()?)).ok()?;
    let rest = buffer.get(4..)?;
    let executable_end = rest.iter().position(|byte| *byte == 0)?;
    let arguments_start = executable_end
        + rest
            .get(executable_end..)?
            .iter()
            .position(|byte| *byte != 0)?;
    Some((argc, rest.get(arguments_start..)?.split(|byte| *byte == 0)))
}

fn environment_value(entry: &[u8], name: &str) -> Option<String> {
    let value = entry.strip_prefix(name.as_bytes())?.strip_prefix(b"=")?;
    String::from_utf8(value.to_vec()).ok()
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
    if pid <= 0 {
        anyhow::bail!("invalid process id {pid}");
    }
    let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::zeroed();
    let size = i32::try_from(std::mem::size_of::<libc::proc_bsdinfo>())?;
    let written = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            info.as_mut_ptr().cast(),
            size,
        )
    };
    if written != size {
        anyhow::bail!(
            "kernel could not inspect start time for pid {pid}: {}",
            std::io::Error::last_os_error()
        );
    }
    let info = unsafe { info.assume_init() };
    let timestamp = i64::try_from(info.pbi_start_tvsec)?;
    let started = chrono::DateTime::from_timestamp(timestamp, 0)
        .with_context(|| format!("invalid kernel start time for pid {pid}"))?;
    Ok(started.format("%a %b %-d %H:%M:%S %Y").to_string())
}

#[cfg(test)]
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
#[cfg(test)]
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
    /// Processes in their terminal's foreground process group.
    foreground: std::collections::HashSet<i32>,
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
        let mut foreground = std::collections::HashSet::new();
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
            if state.contains('+') {
                foreground.insert(pid);
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

        Ok(Self {
            processes,
            ttys,
            foreground,
        })
    }

    /// Whether the process is in its terminal's foreground: for a shell, that
    /// it sits at its prompt rather than running a command.
    pub fn in_foreground(&self, pid: i32) -> bool {
        self.foreground.contains(&pid)
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
    /// Start times of the running processes of `executable`. The oldest is the
    /// terminal instance itself, with its per-surface helpers being younger.
    pub fn instance_starts(
        &self,
        executable: &str,
    ) -> std::collections::BTreeSet<chrono::NaiveDateTime> {
        self.processes
            .values()
            .filter(|(_, comm)| comm.rsplit('/').next() == Some(executable))
            .filter_map(|(start, _)| parse_identity(start))
            .collect()
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

pub fn parent_pid(pid: i32) -> Option<i32> {
    let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::zeroed();
    let size = i32::try_from(std::mem::size_of::<libc::proc_bsdinfo>()).ok()?;
    let written = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            info.as_mut_ptr().cast(),
            size,
        )
    };
    if written != size {
        return None;
    }
    i32::try_from(unsafe { info.assume_init() }.pbi_ppid).ok()
}

/// The leader of the session a process belongs to.
pub fn session_leader(pid: i32) -> Option<i32> {
    let leader = unsafe { libc::getsid(pid) };
    (leader > 0).then_some(leader)
}

pub fn child_pids(pid: i32) -> Vec<i32> {
    pid_list(|buffer, size| unsafe { libc::proc_listchildpids(pid, buffer, size) })
}

pub fn pids_named(name: &str) -> Vec<i32> {
    pids_whose_name(|found| found == name.as_bytes())
}

/// For names known lowercased, as `ps -o comm=` reports them here; the kernel
/// keeps the executable's own case (`iTerm2`).
pub fn pids_named_ignoring_case(name: &str) -> Vec<i32> {
    pids_whose_name(|found| found.eq_ignore_ascii_case(name.as_bytes()))
}

fn pids_whose_name(matches: impl Fn(&[u8]) -> bool) -> Vec<i32> {
    pid_list(|buffer, size| unsafe { libc::proc_listallpids(buffer, size) })
        .into_iter()
        .filter(|&pid| {
            let mut buffer = [0_u8; 64];
            let length = unsafe { libc::proc_name(pid, buffer.as_mut_ptr().cast(), 64) };
            usize::try_from(length).is_ok_and(|length| matches(&buffer[..length.min(buffer.len())]))
        })
        .collect()
}

pub fn socket_peer_pid(socket: &impl std::os::fd::AsRawFd) -> Option<i32> {
    let mut pid: libc::pid_t = 0;
    let mut length = libc::socklen_t::try_from(std::mem::size_of::<libc::pid_t>()).ok()?;
    let result = unsafe {
        libc::getsockopt(
            socket.as_raw_fd(),
            libc::SOL_LOCAL,
            libc::LOCAL_PEERPID,
            (&raw mut pid).cast(),
            &raw mut length,
        )
    };
    (result == 0).then_some(pid)
}

pub fn loopback_listener(port: u16, candidates: &[i32]) -> Option<i32> {
    candidates.iter().copied().find(|&pid| {
        socket_descriptors(pid)
            .into_iter()
            .any(|descriptor| listens_on_loopback(pid, descriptor, port))
    })
}

fn socket_descriptors(pid: i32) -> Vec<i32> {
    let size =
        unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDLISTFDS, 0, std::ptr::null_mut(), 0) };
    let Ok(capacity) = usize::try_from(size) else {
        return Vec::new();
    };
    let mut descriptors = vec![
        libc::proc_fdinfo {
            proc_fd: 0,
            proc_fdtype: 0,
        };
        capacity / std::mem::size_of::<libc::proc_fdinfo>()
    ];
    let written = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDLISTFDS,
            0,
            descriptors.as_mut_ptr().cast(),
            size,
        )
    };
    let Ok(written) = usize::try_from(written) else {
        return Vec::new();
    };
    descriptors.truncate(written / std::mem::size_of::<libc::proc_fdinfo>());
    descriptors
        .into_iter()
        .filter(|descriptor| u32::try_from(libc::PROX_FDTYPE_SOCKET) == Ok(descriptor.proc_fdtype))
        .map(|descriptor| descriptor.proc_fd)
        .collect()
}

const PROC_PIDFDSOCKETINFO: libc::c_int = 3;
const SOCKINFO_TCP: libc::c_int = 2;
const TSI_S_LISTEN: libc::c_int = 1;

// These layouts match sys/proc_info.h; libc does not expose the socket bindings.
#[repr(C)]
struct SocketFdInfo {
    _pfi: [u64; 3],
    psi: SocketInfo,
}

#[repr(C)]
struct SocketInfo {
    _soi_stat: libc::vinfo_stat,
    _soi_so: u64,
    _soi_pcb: u64,
    _soi_type: libc::c_int,
    _soi_protocol: libc::c_int,
    soi_family: libc::c_int,
    _soi_options: libc::c_short,
    _soi_linger: libc::c_short,
    _soi_state: libc::c_short,
    _soi_qlen: libc::c_short,
    _soi_incqlen: libc::c_short,
    _soi_qlimit: libc::c_short,
    _soi_timeo: libc::c_short,
    _soi_error: libc::c_ushort,
    _soi_oobmark: u32,
    _soi_rcv: [u32; 6],
    _soi_snd: [u32; 6],
    soi_kind: libc::c_int,
    _rfu_1: u32,
    soi_proto: SocketProtocolInfo,
}

#[repr(C)]
union SocketProtocolInfo {
    pri_tcp: TcpSockInfo,
    _pri_un: [u64; 66],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct TcpSockInfo {
    tcpsi_ini: InSockInfo,
    tcpsi_state: libc::c_int,
    _tcpsi_timer: [libc::c_int; 4],
    _tcpsi_mss: libc::c_int,
    _tcpsi_flags: u32,
    _rfu_1: u32,
    _tcpsi_tp: u64,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct InSockInfo {
    _insi_fport: libc::c_int,
    insi_lport: libc::c_int,
    _insi_gencnt: u64,
    _insi_flags: u32,
    _insi_flow: u32,
    _insi_vflag: u8,
    _insi_ip_ttl: u8,
    _rfu_1: u32,
    _insi_faddr: [u32; 4],
    insi_laddr: [u32; 4],
    _insi_v4: u8,
    _insi_v6: [u32; 3],
}

fn listens_on_loopback(pid: i32, descriptor: i32, port: u16) -> bool {
    let mut info = std::mem::MaybeUninit::<SocketFdInfo>::zeroed();
    let Ok(size) = libc::c_int::try_from(std::mem::size_of::<SocketFdInfo>()) else {
        return false;
    };
    let written = unsafe {
        libc::proc_pidfdinfo(
            pid,
            descriptor,
            PROC_PIDFDSOCKETINFO,
            info.as_mut_ptr().cast(),
            size,
        )
    };
    if written != size {
        return false;
    }
    let socket = unsafe { info.assume_init() }.psi;
    if socket.soi_family != libc::AF_INET || socket.soi_kind != SOCKINFO_TCP {
        return false;
    }
    let tcp = unsafe { socket.soi_proto.pri_tcp };
    tcp.tcpsi_state == TSI_S_LISTEN
        && u16::try_from(tcp.tcpsi_ini.insi_lport).map(u16::from_be) == Ok(port)
        && std::net::Ipv4Addr::from(u32::from_be(tcp.tcpsi_ini.insi_laddr[3]))
            == std::net::Ipv4Addr::LOCALHOST
}

fn pid_list(list: impl Fn(*mut libc::c_void, libc::c_int) -> libc::c_int) -> Vec<i32> {
    let mut capacity = 256;
    loop {
        let mut pids = vec![0_i32; capacity];
        let Ok(size) = libc::c_int::try_from(pids.len() * std::mem::size_of::<i32>()) else {
            return Vec::new();
        };
        let Ok(count) = usize::try_from(list(pids.as_mut_ptr().cast(), size)) else {
            return Vec::new();
        };
        if count < capacity {
            pids.truncate(count);
            return pids;
        }
        capacity *= 2;
    }
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
    fn environment_follows_the_arguments_up_to_the_first_empty_string() {
        let mut buffer = 2_i32.to_ne_bytes().to_vec();
        buffer.extend_from_slice(
            b"/bin/codex\0\0codex\0SESSION_GUARD_SHELL_PID=7\0PATH=/bin\0SESSION_GUARD=no\0\0ptr_munge=\0",
        );
        assert_eq!(
            parse_process_environment_variable(&buffer, "PATH"),
            Some("/bin".into())
        );
        assert_eq!(
            parse_process_environment_variable(&buffer, "SESSION_GUARD"),
            Some("no".into())
        );
        assert_eq!(
            parse_process_environment_variable(&buffer, "SESSION_GUARD_SHELL_PID=7"),
            None
        );
        assert_eq!(parse_process_environment_variable(&buffer, "codex"), None);
        assert_eq!(
            parse_process_environment_variable(&buffer, "ptr_munge"),
            None
        );
    }

    #[test]
    fn a_live_process_reports_the_environment_it_started_with() {
        let pid = std::process::id() as i32;
        assert_eq!(
            process_environment_variable(pid, "HOME"),
            std::env::var("HOME").ok()
        );
        assert_eq!(
            process_environment_variable(pid, "SESSION_GUARD_NO_SUCH_VARIABLE"),
            None
        );
    }

    #[test]
    fn the_kernel_names_the_loopback_listener_and_the_socket_peer() {
        let own = std::process::id() as i32;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        assert_eq!(loopback_listener(port, &[own]), Some(own));
        let client = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
        let client_port = client.local_addr().unwrap().port();
        assert_eq!(loopback_listener(client_port, &[own]), None);
        drop(listener);
        assert_eq!(loopback_listener(port, &[own]), None);

        let (left, _right) = std::os::unix::net::UnixStream::pair().unwrap();
        assert_eq!(socket_peer_pid(&left), Some(own));
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
    fn the_kernel_names_this_process_and_its_parent_and_children() {
        let own = std::process::id() as i32;
        let parent = parent_pid(own).unwrap();
        assert_eq!(parent, unsafe { libc::getppid() });
        assert!(child_pids(parent).contains(&own));
        let executable = std::env::current_exe().unwrap();
        let name = executable.file_name().unwrap().to_str().unwrap();
        let kernel_name: String = name.chars().take(2 * libc::MAXCOMLEN).collect();
        assert!(pids_named(&kernel_name).contains(&own));
        assert_eq!(pids_named("no-such-process-xyz"), Vec::<i32>::new());
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

    #[cfg(target_os = "macos")]
    #[test]
    fn an_exit_watch_reads_the_status_of_a_process_it_did_not_start() {
        // The subshell exits at once, so launchd adopts the sleeper: the
        // process this test watches is not its child.
        let orphan = |script: &str| {
            let output = Command::new("/bin/sh")
                .args(["-c", &format!("({script}) >/dev/null 2>&1 & echo $!")])
                .output()
                .unwrap();
            String::from_utf8(output.stdout)
                .unwrap()
                .trim()
                .parse::<i32>()
                .unwrap()
        };
        let watch = ExitWatch::new().unwrap();
        let exits = orphan("sleep 1; exit 3");
        let killed = orphan("sleep 30");
        assert_ne!(parent_pid(exits), Some(std::process::id() as i32));
        assert!(watch.watch(exits));
        assert!(watch.watch(killed));
        assert_eq!(unsafe { libc::kill(killed, libc::SIGKILL) }, 0);
        let mut statuses = std::collections::HashMap::new();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while statuses.len() < 2 && std::time::Instant::now() < deadline {
            statuses.extend(watch.exits(std::time::Duration::from_secs(1)));
        }
        assert_eq!(statuses.get(&exits), Some(&(3 << 8)));
        assert_eq!(statuses.get(&killed), Some(&libc::SIGKILL));
        assert!(!watch.watch(exits));
    }
}
