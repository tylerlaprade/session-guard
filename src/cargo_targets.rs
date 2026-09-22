use crate::paths;
use crate::process::{self, ProcInfo, ProcessIdentityStatus};
use crate::sessions::{self, SessionRecord};
use crate::{Tool, process::process_start_identity};
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::Command;

const MARKER_FILE: &str = ".session-guard-owner.json";
const LOCK_FILE: &str = ".session-guard-owner.lock";
const MARKER_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct ProcessIdentity {
    pid: i32,
    started_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct OwnedTargetMarker {
    version: u32,
    target_session_id: String,
    owner_session_id: Option<String>,
    tool: Tool,
    owner_process: ProcessIdentity,
    created_at: DateTime<Utc>,
    last_used_at: DateTime<Utc>,
}

#[derive(Debug)]
struct ResolvedOwner {
    target_session_id: String,
    owner_session_id: Option<String>,
    tool: Tool,
    process: ProcessIdentity,
}

#[derive(Debug, PartialEq, Eq)]
struct OwnerSelection {
    target_session_id: String,
    owner_session_id: Option<String>,
    tool: Tool,
    pid: i32,
}

#[derive(Debug)]
struct TargetLock(File);

impl Drop for TargetLock {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.0);
    }
}

#[derive(Debug)]
struct OwnedTargetLease {
    _lifecycle: TargetLock,
    _owner: TargetLock,
}

#[derive(Debug, Default)]
pub struct CleanupSummary {
    pub owned_targets: usize,
    pub errors: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheRemoval {
    NotPresent,
    Removed,
}

#[derive(Debug)]
pub struct CacheRemovalPlan {
    base: Option<PathBuf>,
    _lifecycle: TargetLock,
}

pub fn prepare_cache_removal() -> Result<CacheRemovalPlan> {
    let base = paths::cargo_targets_dir()?;
    prepare_cache_removal_at(base)
}

fn prepare_cache_removal_at(base: PathBuf) -> Result<CacheRemovalPlan> {
    let lifecycle = lock_cache_lifecycle(&base, false)?;
    if !base.exists() {
        return Ok(CacheRemovalPlan {
            base: None,
            _lifecycle: lifecycle,
        });
    }

    if !owned_cache_layout_is_valid(&base)? {
        anyhow::bail!("cannot safely remove unknown Cargo target cache layout");
    }
    Ok(CacheRemovalPlan {
        base: Some(base),
        _lifecycle: lifecycle,
    })
}

impl CacheRemovalPlan {
    pub fn remove(self) -> Result<CacheRemoval> {
        let Some(base) = &self.base else {
            return Ok(CacheRemoval::NotPresent);
        };
        fs::remove_dir_all(base).with_context(|| format!("failed to remove {}", base.display()))?;
        Ok(CacheRemoval::Removed)
    }
}

pub fn run(command: &[OsString]) -> Result<()> {
    let owner = resolve_owner()?;
    let (target_dir, _lease) = acquire_owned_target(&paths::cargo_targets_dir()?, &owner)?;

    let status = Command::new(&command[0])
        .args(&command[1..])
        .env("CARGO_TARGET_DIR", &target_dir)
        .status()
        .with_context(|| format!("failed to run {}", command[0].to_string_lossy()))?;

    if status.success() {
        return Ok(());
    }

    std::process::exit(command_exit_code(status))
}

fn command_exit_code(status: std::process::ExitStatus) -> i32 {
    status
        .code()
        .or_else(|| status.signal().map(|signal| 128 + signal))
        .unwrap_or(1)
}

pub fn cleanup_once() -> Result<CleanupSummary> {
    let cargo_targets_dir = paths::cargo_targets_dir()?;
    let _lifecycle = lock_cache_lifecycle(&cargo_targets_dir, true)?;
    let mut summary = CleanupSummary::default();

    match prune_owned_targets(&cargo_targets_dir) {
        Ok(count) => summary.owned_targets = count,
        Err(error) => summary
            .errors
            .push(format!("owned Cargo target cleanup failed: {error:#}")),
    }

    Ok(summary)
}

fn resolve_owner() -> Result<ResolvedOwner> {
    let session_path = paths::sessions_file()?;
    sessions::repair_if_corrupt(&session_path)?;
    let registered = sessions::read_sessions(&session_path)?;
    let processes = process::list_processes()?;
    let ancestors = ancestor_pids(unsafe { libc::getppid() }, &processes);

    let environment_ids = environment_session_ids()?;
    let selection = select_owner(&ancestors, &processes, &registered, &environment_ids)?;
    let started_at = process_start_identity(selection.pid).with_context(|| {
        format!(
            "could not identify the owning {} process start time",
            selection.tool
        )
    })?;

    Ok(ResolvedOwner {
        target_session_id: selection.target_session_id,
        owner_session_id: selection.owner_session_id,
        tool: selection.tool,
        process: ProcessIdentity {
            pid: selection.pid,
            started_at,
        },
    })
}

fn select_owner(
    ancestors: &[i32],
    processes: &[ProcInfo],
    registered: &[SessionRecord],
    environment_ids: &[(Tool, String)],
) -> Result<OwnerSelection> {
    for pid in ancestors {
        let Some(process) = processes.iter().find(|process| process.pid == *pid) else {
            continue;
        };
        let Some(tool) = process_tool(process) else {
            continue;
        };

        let matching_environment_ids: Vec<&str> = environment_ids
            .iter()
            .filter_map(|(candidate_tool, id)| (*candidate_tool == tool).then_some(id.as_str()))
            .collect();
        if matching_environment_ids.len() > 1 {
            anyhow::bail!("multiple session ids are set for {tool}");
        }
        let command_session_id = tool
            .spec()
            .session_id_from_process
            .and_then(|from_process| from_process(process));
        let asserted_session_id = matching_environment_ids
            .first()
            .copied()
            .or(command_session_id.as_deref());

        let mut records: Vec<&SessionRecord> = registered
            .iter()
            .filter(|session| session.tool == tool && session.pid == Some(*pid))
            .collect();
        records.sort_by_key(|session| session.last_seen_at);
        let record = asserted_session_id
            .and_then(|asserted_session_id| {
                records
                    .iter()
                    .rev()
                    .find(|session| session.session_id == asserted_session_id)
                    .copied()
            })
            .or_else(|| records.last().copied());

        let target_session_id = asserted_session_id
            .map(ToOwned::to_owned)
            .or_else(|| record.map(|session| session.session_id.clone()));
        let Some(target_session_id) = target_session_id else {
            continue;
        };
        validate_session_id(&target_session_id)
            .with_context(|| format!("invalid {tool} session id {target_session_id:?}"))?;

        return Ok(OwnerSelection {
            target_session_id,
            owner_session_id: record.map(|session| session.session_id.clone()),
            tool,
            pid: *pid,
        });
    }

    anyhow::bail!(
        "could not identify an owning agent session; run this only inside {}",
        crate::harness::name_list()
    )
}

fn process_tool(process: &ProcInfo) -> Option<Tool> {
    Tool::all().find(|tool| tool.spec().owns_process(process))
}

fn environment_session_ids() -> Result<Vec<(Tool, String)>> {
    let mut ids = Vec::new();
    for tool in Tool::all() {
        let Some(name) = tool.spec().session_id_env else {
            continue;
        };
        if let Some(value) = std::env::var_os(name) {
            let value = value
                .into_string()
                .map_err(|_| anyhow::anyhow!("{name} is not valid UTF-8"))?;
            if !value.is_empty() {
                ids.push((tool, value));
            }
        }
    }
    Ok(ids)
}

fn ancestor_pids(start: i32, processes: &[ProcInfo]) -> Vec<i32> {
    let parents: HashMap<i32, i32> = processes
        .iter()
        .map(|process| (process.pid, process.ppid))
        .collect();
    let mut ancestors = Vec::new();
    let mut seen = HashSet::new();
    let mut current = start;
    while current > 0 && seen.insert(current) && ancestors.len() < 64 {
        ancestors.push(current);
        current = parents.get(&current).copied().unwrap_or(0);
    }
    ancestors
}

#[cfg(test)]
fn register_owned_target(base: &Path, owner: &ResolvedOwner) -> Result<PathBuf> {
    let _lifecycle = lock_cache_lifecycle(base, true)?;
    let owner_dir = prepare_owned_target_dir(base, owner)?;
    let lock = open_target_lock(&owner_dir, true)?;
    lock.lock_exclusive()
        .with_context(|| format!("failed to lock {}", owner_dir.display()))?;
    let result = update_owned_target(&owner_dir, owner);
    let _ = FileExt::unlock(&lock);
    result
}

fn acquire_owned_target(base: &Path, owner: &ResolvedOwner) -> Result<(PathBuf, OwnedTargetLease)> {
    let lifecycle = lock_cache_lifecycle(base, true)?;
    let owner_dir = prepare_owned_target_dir(base, owner)?;
    let lock = open_target_lock(&owner_dir, true)?;
    lock.lock_exclusive()
        .with_context(|| format!("failed to lock {}", owner_dir.display()))?;
    let target_dir = update_owned_target(&owner_dir, owner)?;
    Ok((
        target_dir,
        OwnedTargetLease {
            _lifecycle: lifecycle,
            _owner: TargetLock(lock),
        },
    ))
}

fn prepare_owned_target_dir(base: &Path, owner: &ResolvedOwner) -> Result<PathBuf> {
    ensure_real_directory(base)?;
    ensure_real_directory(&base.join(owner.tool.as_str()))?;
    let owner_dir = owned_target_dir(base, owner.tool, &owner.target_session_id)?;
    ensure_real_directory(&owner_dir)?;
    Ok(owner_dir)
}

fn update_owned_target(owner_dir: &Path, owner: &ResolvedOwner) -> Result<PathBuf> {
    let marker_path = owner_dir.join(MARKER_FILE);

    let existing = if marker_path.exists() {
        ensure_regular_file(&marker_path)?;
        let marker: OwnedTargetMarker = serde_json::from_slice(
            &fs::read(&marker_path)
                .with_context(|| format!("failed to read {}", marker_path.display()))?,
        )
        .with_context(|| format!("failed to parse {}", marker_path.display()))?;
        validate_marker(&marker, owner.tool, &owner.target_session_id)?;
        Some(marker)
    } else {
        None
    };

    let now = Utc::now();
    let marker = OwnedTargetMarker {
        version: MARKER_VERSION,
        target_session_id: owner.target_session_id.clone(),
        owner_session_id: owner.owner_session_id.clone(),
        tool: owner.tool,
        owner_process: owner.process.clone(),
        created_at: existing.map_or(now, |marker| marker.created_at),
        last_used_at: now,
    };
    write_marker(&marker_path, &marker)?;

    let target_dir = owner_dir.join("target");
    if target_dir.exists() {
        ensure_real_directory(&target_dir)?;
    }
    Ok(target_dir)
}

fn open_target_lock(owner_dir: &Path, create: bool) -> Result<File> {
    let path = owner_dir.join(LOCK_FILE);
    if path.exists() {
        ensure_regular_file(&path)?;
    } else if !create {
        anyhow::bail!("owned Cargo target lock is missing");
    }
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(create)
        .truncate(false)
        .open(&path)
        .with_context(|| format!("failed to open {}", path.display()))
}

fn lock_cache_lifecycle(base: &Path, shared: bool) -> Result<TargetLock> {
    let parent = base
        .parent()
        .context("Cargo target cache has no parent directory")?;
    ensure_real_directory(parent)?;
    let path = parent.join(".cargo-targets.lock");
    if path.exists() {
        ensure_regular_file(&path)?;
    }
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .with_context(|| format!("failed to open {}", path.display()))?;
    let result = if shared {
        FileExt::try_lock_shared(&file)
    } else {
        FileExt::try_lock_exclusive(&file)
    };
    if result.is_err() {
        if shared {
            anyhow::bail!("Cargo target cache is being uninstalled");
        }
        anyhow::bail!("cannot uninstall while a session-owned command or cleanup is active");
    }
    Ok(TargetLock(file))
}

fn owned_cache_layout_is_valid(base: &Path) -> Result<bool> {
    if !is_real_directory(base) {
        return Ok(false);
    }
    let entries =
        fs::read_dir(base).with_context(|| format!("failed to inspect {}", base.display()))?;
    for entry in entries {
        let entry = entry.with_context(|| format!("failed to inspect {}", base.display()))?;
        let Some(tool_name) = entry.file_name().to_str().map(ToOwned::to_owned) else {
            return Ok(false);
        };
        if !Tool::all().any(|tool| tool.as_str() == tool_name) || !is_real_directory(&entry.path())
        {
            return Ok(false);
        }

        let owner_entries = fs::read_dir(entry.path())
            .with_context(|| format!("failed to inspect {}", entry.path().display()))?;
        for owner_entry in owner_entries {
            let owner_entry = owner_entry
                .with_context(|| format!("failed to inspect {}", entry.path().display()))?;
            let owner_dir = owner_entry.path();
            let Some(session_id) = owner_entry.file_name().to_str().map(ToOwned::to_owned) else {
                return Ok(false);
            };
            if validate_session_id(&session_id).is_err() || !is_real_directory(&owner_dir) {
                return Ok(false);
            }
            if open_target_lock(&owner_dir, false).is_err() {
                return Ok(false);
            }
        }
    }
    Ok(true)
}

fn owned_target_dir(base: &Path, tool: Tool, session_id: &str) -> Result<PathBuf> {
    validate_session_id(session_id)?;
    Ok(base.join(tool.as_str()).join(session_id))
}

fn ensure_real_directory(path: &Path) -> Result<()> {
    if path.exists() {
        let metadata = fs::symlink_metadata(path)
            .with_context(|| format!("failed to inspect {}", path.display()))?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            anyhow::bail!("{} is not a real directory", path.display());
        }
        return Ok(());
    }

    fs::create_dir_all(path).with_context(|| format!("failed to create {}", path.display()))
}

fn ensure_regular_file(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("failed to inspect {}", path.display()))?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        anyhow::bail!("{} is not a regular file", path.display());
    }
    Ok(())
}

fn write_marker(path: &Path, marker: &OwnedTargetMarker) -> Result<()> {
    let mut contents = serde_json::to_vec_pretty(marker)?;
    contents.push(b'\n');
    let temporary = path.with_extension(format!("tmp-{}", std::process::id()));
    fs::write(&temporary, contents)
        .with_context(|| format!("failed to write {}", temporary.display()))?;
    fs::rename(&temporary, path).with_context(|| format!("failed to replace {}", path.display()))
}

fn validate_marker(marker: &OwnedTargetMarker, tool: Tool, session_id: &str) -> Result<()> {
    if marker.version != MARKER_VERSION
        || marker.tool != tool
        || marker.target_session_id != session_id
        || marker.owner_process.pid <= 0
        || marker.owner_process.started_at.is_empty()
    {
        anyhow::bail!("owned Cargo target marker does not match its directory");
    }
    if let Some(owner_session_id) = &marker.owner_session_id {
        validate_session_id(owner_session_id)?;
    }
    Ok(())
}

fn prune_owned_targets(base: &Path) -> Result<usize> {
    if !is_real_directory(base) {
        return Ok(0);
    }

    let mut pruned = 0;
    for tool in Tool::all() {
        let tool_dir = base.join(tool.as_str());
        if !is_real_directory(&tool_dir) {
            continue;
        }
        let Ok(entries) = fs::read_dir(&tool_dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let owner_dir = entry.path();
            if !is_real_directory(&owner_dir) {
                continue;
            }
            let Some(session_id) = entry.file_name().to_str().map(ToOwned::to_owned) else {
                continue;
            };
            if validate_session_id(&session_id).is_err() {
                continue;
            }

            let Ok(lock) = open_target_lock(&owner_dir, false) else {
                continue;
            };
            if FileExt::try_lock_exclusive(&lock).is_err() {
                continue;
            }
            let _lock = TargetLock(lock);

            let marker_path = owner_dir.join(MARKER_FILE);
            let Some(marker) = read_valid_marker(&marker_path, tool, &session_id) else {
                continue;
            };
            if process::process_identity_status(
                marker.owner_process.pid,
                &marker.owner_process.started_at,
            ) != ProcessIdentityStatus::Gone
            {
                continue;
            }

            let target_dir = owner_dir.join("target");
            if target_dir.exists() && !is_real_directory(&target_dir) {
                continue;
            }
            if target_dir.exists() {
                fs::remove_dir_all(&target_dir)
                    .with_context(|| format!("failed to remove {}", target_dir.display()))?;
            }
            fs::remove_file(&marker_path)
                .with_context(|| format!("failed to remove {}", marker_path.display()))?;
            pruned += 1;
        }
    }
    Ok(pruned)
}

fn read_valid_marker(path: &Path, tool: Tool, session_id: &str) -> Option<OwnedTargetMarker> {
    ensure_regular_file(path).ok()?;
    let marker: OwnedTargetMarker = serde_json::from_slice(&fs::read(path).ok()?).ok()?;
    validate_marker(&marker, tool, session_id).ok()?;
    Some(marker)
}

fn is_real_directory(path: &Path) -> bool {
    fs::symlink_metadata(path)
        .is_ok_and(|metadata| metadata.is_dir() && !metadata.file_type().is_symlink())
}

fn validate_session_id(session_id: &str) -> Result<()> {
    let bytes = session_id.as_bytes();
    if bytes.len() != 36 {
        anyhow::bail!("session id is not a UUID");
    }
    for (index, byte) in bytes.iter().copied().enumerate() {
        let is_hyphen = matches!(index, 8 | 13 | 18 | 23);
        if (is_hyphen && byte != b'-') || (!is_hyphen && !byte.is_ascii_hexdigit()) {
            anyhow::bail!("session id is not a UUID");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration as ChronoDuration;

    const SESSION_ID: &str = "019f4dda-65d2-7440-ba13-f2307c90e7f6";

    fn session_record(session_id: &str) -> SessionRecord {
        SessionRecord::new(
            crate::tool("claude"),
            session_id.to_string(),
            Some(999_999),
            Some(999_998),
            PathBuf::from("/tmp/project"),
            None,
            None,
            Some("test".to_string()),
        )
    }

    fn make_cargo_target(path: &Path) {
        fs::create_dir_all(path.join("debug").join(".fingerprint")).unwrap();
        fs::write(path.join("debug").join(".cargo-lock"), b"").unwrap();
        fs::write(path.join("debug").join("artifact"), b"generated").unwrap();
    }

    #[test]
    fn accepts_uuid_session_ids_only() {
        assert!(validate_session_id(SESSION_ID).is_ok());
        assert!(validate_session_id("../target").is_err());
        assert!(validate_session_id("019f4dda-65d2-7440-ba13-f2307c90e7fg").is_err());
    }

    #[test]
    fn child_signal_uses_shell_compatible_exit_code() {
        let signaled = Command::new("sh")
            .args(["-c", "kill -TERM $$"])
            .status()
            .unwrap();
        assert_eq!(command_exit_code(signaled), 143);

        let ordinary = Command::new("sh").args(["-c", "exit 23"]).status().unwrap();
        assert_eq!(command_exit_code(ordinary), 23);
    }

    #[test]
    fn ancestor_walk_stops_at_cycles() {
        let processes = vec![
            ProcInfo {
                pid: 10,
                ppid: 11,
                command: "child".to_string(),
            },
            ProcInfo {
                pid: 11,
                ppid: 10,
                command: "parent".to_string(),
            },
        ];
        assert_eq!(ancestor_pids(10, &processes), vec![10, 11]);
    }

    #[test]
    fn nearest_verified_tool_ancestor_wins_in_nested_agent_processes() {
        let inner_id = "019f4ddd-1111-7111-8111-111111111111";
        let outer_id = "e0967971-0502-410f-9360-7a544567f57b";
        let processes = vec![
            ProcInfo {
                pid: 50,
                ppid: 100,
                command: "/bin/zsh".to_string(),
            },
            ProcInfo {
                pid: 100,
                ppid: 200,
                command: "/opt/homebrew/bin/codex".to_string(),
            },
            ProcInfo {
                pid: 200,
                ppid: 1,
                command: "claude --resume outer".to_string(),
            },
        ];
        let mut outer = session_record(outer_id);
        outer.pid = Some(200);
        let environment_ids = vec![
            (crate::tool("claude"), outer_id.to_string()),
            (crate::tool("codex"), inner_id.to_string()),
        ];

        let selection = select_owner(
            &ancestor_pids(50, &processes),
            &processes,
            &[outer],
            &environment_ids,
        )
        .unwrap();

        assert_eq!(selection.tool, crate::tool("codex"));
        assert_eq!(selection.pid, 100);
        assert_eq!(selection.target_session_id, inner_id);
        assert_eq!(selection.owner_session_id, None);
    }

    #[test]
    fn legacy_untagged_owned_target_is_pruned_after_owner_exits() {
        let temp = tempfile::tempdir().unwrap();
        let base = temp.path().join("cargo-targets");
        let owner = ResolvedOwner {
            target_session_id: SESSION_ID.to_string(),
            owner_session_id: Some(SESSION_ID.to_string()),
            tool: crate::tool("claude"),
            process: ProcessIdentity {
                pid: 999_999,
                started_at: "old process".to_string(),
            },
        };
        let target = register_owned_target(&base, &owner).unwrap();
        make_cargo_target(&target);
        assert!(!target.join("CACHEDIR.TAG").exists());

        assert_eq!(prune_owned_targets(&base).unwrap(), 1);
        assert!(!target.exists());
    }

    #[test]
    fn real_cargo_target_is_created_by_cargo_and_pruned() {
        let temp = tempfile::tempdir().unwrap();
        let base = temp.path().join("cargo-targets");
        let owner = ResolvedOwner {
            target_session_id: SESSION_ID.to_string(),
            owner_session_id: Some(SESSION_ID.to_string()),
            tool: crate::tool("codex"),
            process: ProcessIdentity {
                pid: 999_999,
                started_at: "old process".to_string(),
            },
        };
        let target = register_owned_target(&base, &owner).unwrap();
        assert!(
            !target.exists(),
            "Cargo must create its own target directory"
        );

        let project = temp.path().join("project");
        fs::create_dir_all(project.join("src")).unwrap();
        fs::write(
            project.join("Cargo.toml"),
            "[package]\nname = \"cleanup-probe\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
        )
        .unwrap();
        fs::write(project.join("src/main.rs"), "fn main() {}\n").unwrap();

        let status = Command::new("cargo")
            .args([
                "check",
                "--offline",
                "--quiet",
                "--manifest-path",
                project.join("Cargo.toml").to_str().unwrap(),
            ])
            .env("CARGO_TARGET_DIR", &target)
            .status()
            .unwrap();
        assert!(status.success());
        assert!(target.join("CACHEDIR.TAG").is_file());

        assert_eq!(prune_owned_targets(&base).unwrap(), 1);
        assert!(!target.exists());
    }

    #[test]
    fn abandoned_owned_target_is_pruned_without_touching_siblings() {
        let temp = tempfile::tempdir().unwrap();
        let base = temp.path().join("cargo-targets");
        let owner = ResolvedOwner {
            target_session_id: SESSION_ID.to_string(),
            owner_session_id: Some(SESSION_ID.to_string()),
            tool: crate::tool("claude"),
            process: ProcessIdentity {
                pid: 999_999,
                started_at: "old process".to_string(),
            },
        };
        let target = register_owned_target(&base, &owner).unwrap();
        make_cargo_target(&target);
        let sibling = base.join("keep-me");
        fs::create_dir_all(&sibling).unwrap();
        fs::write(sibling.join("data"), b"important").unwrap();

        assert_eq!(prune_owned_targets(&base).unwrap(), 1);
        assert!(!target.exists());
        assert!(sibling.join("data").exists());
    }

    #[test]
    fn symlinked_owned_target_root_is_never_traversed() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let real_base = temp.path().join("real-cargo-targets");
        let owner = ResolvedOwner {
            target_session_id: SESSION_ID.to_string(),
            owner_session_id: Some(SESSION_ID.to_string()),
            tool: crate::tool("claude"),
            process: ProcessIdentity {
                pid: 999_999,
                started_at: "old process".to_string(),
            },
        };
        let target = register_owned_target(&real_base, &owner).unwrap();
        make_cargo_target(&target);
        let linked_base = temp.path().join("linked-cargo-targets");
        symlink(&real_base, &linked_base).unwrap();

        assert_eq!(prune_owned_targets(&linked_base).unwrap(), 0);
        assert!(target.exists());
    }

    #[test]
    fn symlinked_target_is_never_traversed() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let base = temp.path().join("cargo-targets");
        let owner = ResolvedOwner {
            target_session_id: SESSION_ID.to_string(),
            owner_session_id: Some(SESSION_ID.to_string()),
            tool: crate::tool("claude"),
            process: ProcessIdentity {
                pid: 999_999,
                started_at: "old process".to_string(),
            },
        };
        let target = register_owned_target(&base, &owner).unwrap();
        let outside = temp.path().join("outside");
        make_cargo_target(&outside);
        symlink(&outside, &target).unwrap();

        assert_eq!(prune_owned_targets(&base).unwrap(), 0);
        assert!(outside.join("debug").join("artifact").is_file());
    }

    #[test]
    fn running_owned_command_lease_blocks_cleanup() {
        let temp = tempfile::tempdir().unwrap();
        let base = temp.path().join("cargo-targets");
        let owner = ResolvedOwner {
            target_session_id: SESSION_ID.to_string(),
            owner_session_id: Some(SESSION_ID.to_string()),
            tool: crate::tool("claude"),
            process: ProcessIdentity {
                pid: 999_999,
                started_at: "old process".to_string(),
            },
        };
        let (target, lease) = acquire_owned_target(&base, &owner).unwrap();
        make_cargo_target(&target);

        assert_eq!(prune_owned_targets(&base).unwrap(), 0);
        assert!(target.exists());

        drop(lease);
        assert_eq!(prune_owned_targets(&base).unwrap(), 1);
        assert!(!target.exists());
    }

    #[test]
    fn uninstall_preflight_aborts_before_removing_active_owned_target() {
        let temp = tempfile::tempdir().unwrap();
        let base = temp.path().join("cargo-targets");
        let owner = ResolvedOwner {
            target_session_id: SESSION_ID.to_string(),
            owner_session_id: Some(SESSION_ID.to_string()),
            tool: crate::tool("claude"),
            process: ProcessIdentity {
                pid: 999_999,
                started_at: "old process".to_string(),
            },
        };
        let (target, lease) = acquire_owned_target(&base, &owner).unwrap();
        make_cargo_target(&target);

        let error = prepare_cache_removal_at(base.clone()).unwrap_err();
        assert!(error.to_string().contains("cannot uninstall"));
        assert!(target.exists());

        drop(lease);
        let plan = prepare_cache_removal_at(base.clone()).unwrap();
        let blocked = acquire_owned_target(&base, &owner).unwrap_err();
        assert!(blocked.to_string().contains("being uninstalled"));
        assert_eq!(plan.remove().unwrap(), CacheRemoval::Removed);
        assert!(!base.exists());
    }

    #[test]
    fn live_exact_owner_process_preserves_owned_target() {
        let temp = tempfile::tempdir().unwrap();
        let base = temp.path().join("cargo-targets");
        let pid = std::process::id() as i32;
        let owner = ResolvedOwner {
            target_session_id: SESSION_ID.to_string(),
            owner_session_id: Some(SESSION_ID.to_string()),
            tool: crate::tool("codex"),
            process: ProcessIdentity {
                pid,
                started_at: process_start_identity(pid).unwrap(),
            },
        };
        let target = register_owned_target(&base, &owner).unwrap();
        make_cargo_target(&target);

        assert_eq!(prune_owned_targets(&base).unwrap(), 0);
        assert!(target.exists());
    }

    #[test]
    fn marker_round_trip_preserves_creation_time() {
        let temp = tempfile::tempdir().unwrap();
        let base = temp.path().join("cargo-targets");
        let owner = ResolvedOwner {
            target_session_id: SESSION_ID.to_string(),
            owner_session_id: Some(SESSION_ID.to_string()),
            tool: crate::tool("codex"),
            process: ProcessIdentity {
                pid: std::process::id() as i32,
                started_at: "now".to_string(),
            },
        };
        register_owned_target(&base, &owner).unwrap();
        let marker_path = base.join("codex").join(SESSION_ID).join(MARKER_FILE);
        let first: OwnedTargetMarker =
            serde_json::from_slice(&fs::read(&marker_path).unwrap()).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(2));
        register_owned_target(&base, &owner).unwrap();
        let second: OwnedTargetMarker =
            serde_json::from_slice(&fs::read(&marker_path).unwrap()).unwrap();

        assert_eq!(first.created_at, second.created_at);
        assert!(second.last_used_at >= first.last_used_at);
        assert!(second.last_used_at - first.last_used_at < ChronoDuration::seconds(1));
    }
}
