//! Whether Screen Time has the configured terminal locked out. `AppKit` swaps
//! a locked-out app's native fullscreen spaces for overlays, and a tab added
//! under them takes the whole app down when the lockout lifts (2026-10-18:
//! Ghostty aborted on `NSWindowStackController`'s "windowToTakeFrom should be
//! in FS"). The policy behind a lockout needs a private entitlement to read,
//! so the daemon follows the line `AppKit` logs in the terminal on every
//! change.

use crate::TerminalKind;
use crate::paths;
use crate::process::{self, ProcessIdentityStatus};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fs;
use std::io::{BufRead, BufReader, Read};
use std::path::Path;
use std::process::{Child, ChildStdout, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

const STATE_PREFIX: &str = "LockoutUI changed state: ";
const WAIT_INTERVAL: Duration = Duration::from_millis(500);
// A stream can die a moment before its follower sees the end of it, so a
// resumed read starts this far before the old follower stopped. Reading part
// of what it already read again changes nothing.
const RESUME_OVERLAP: chrono::TimeDelta = chrono::TimeDelta::seconds(60);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Lockout {
    Clear,
    Engaged,
    /// The watcher is still reading the terminal's state from the stored log.
    Reading,
    Unknown(String),
}

/// One process, told apart from a later one on a recycled pid.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Instance {
    pub pid: i32,
    pub started: String,
}

impl Instance {
    fn of(pid: i32) -> Result<Self> {
        Ok(Self {
            pid,
            started: process::process_start_identity(pid)?,
        })
    }

    fn is_running(&self) -> bool {
        process::process_identity_status(self.pid, &self.started) == ProcessIdentityStatus::Alive
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
enum Seed {
    Reading,
    /// The terminal already running when the watcher started, read from the
    /// stored log. Later instances are covered by the stream from launch.
    Read {
        terminal: Option<Instance>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Record {
    daemon: Instance,
    watcher: Instance,
    seed: Seed,
    engaged: BTreeSet<Instance>,
    /// When this watcher stopped following, once its seed was read: the next
    /// daemon reads the stored log from here instead of from the terminal's
    /// launch.
    #[serde(default)]
    followed_until: Option<chrono::DateTime<chrono::Utc>>,
}

impl Record {
    fn lockout(&self, current: Option<&Instance>, running: impl Fn(&Instance) -> bool) -> Lockout {
        let Some(current) = current else {
            return Lockout::Clear;
        };
        if !running(&self.daemon) || !running(&self.watcher) {
            return Lockout::Unknown("the session-guard lockout watcher is not running".into());
        }
        if self.seed == Seed::Reading {
            return Lockout::Reading;
        }
        if self.engaged.contains(current) {
            return Lockout::Engaged;
        }
        if self.covers(current) {
            return Lockout::Clear;
        }
        Lockout::Unknown(format!(
            "terminal pid {} started before the lockout watcher and its state was never read",
            current.pid
        ))
    }

    /// Whether this watcher knows `instance`'s state: it read it at start, or
    /// was streaming when `instance` launched.
    fn covers(&self, instance: &Instance) -> bool {
        match &self.seed {
            Seed::Reading => false,
            Seed::Read { terminal } => {
                terminal.as_ref() == Some(instance) || started_no_earlier(instance, &self.watcher)
            }
        }
    }

    /// Where a new watcher picks `instance` up from this stopped one: whether
    /// it was locked out, and since when the stored log is still unread.
    fn resume(&self, instance: &Instance) -> Option<(bool, chrono::DateTime<chrono::Utc>)> {
        let until = self.followed_until?;
        let engaged = self.engaged.contains(instance);
        (engaged || self.covers(instance)).then_some((engaged, until - RESUME_OVERLAP))
    }
}

fn started_no_earlier(process: &Instance, than: &Instance) -> bool {
    match (
        process::parse_identity(&process.started),
        process::parse_identity(&than.started),
    ) {
        (Some(process), Some(than)) => process >= than,
        _ => false,
    }
}

pub fn lockout(terminal: TerminalKind) -> Lockout {
    let current = current_instance(terminal);
    let path = match paths::lockout_state() {
        Ok(path) => path,
        Err(error) => return Lockout::Unknown(format!("{error:#}")),
    };
    match read_record(&path) {
        Ok(Some(record)) => record.lockout(current.as_ref(), Instance::is_running),
        Ok(None) if current.is_none() => Lockout::Clear,
        Ok(None) => {
            Lockout::Unknown("the session-guard lockout watcher has not run on this boot".into())
        }
        Err(error) => Lockout::Unknown(format!("{error:#}")),
    }
}

/// The terminal app itself: its own helper processes start after it does.
fn current_instance(terminal: TerminalKind) -> Option<Instance> {
    process::pids_named_ignoring_case(terminal.process_name())
        .into_iter()
        .filter_map(|pid| Instance::of(pid).ok())
        .min_by_key(|instance| process::parse_identity(&instance.started))
}

fn read_record(path: &Path) -> Result<Option<Record>> {
    match fs::read(path) {
        Ok(contents) => serde_json::from_slice(&contents)
            .map(Some)
            .with_context(|| format!("failed to parse {}", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("failed to read {}", path.display())),
    }
}

fn write_record(path: &Path, record: &Record) -> Result<()> {
    let staged = path.with_extension("json.new");
    fs::write(&staged, serde_json::to_vec(record)?)
        .with_context(|| format!("failed to write {}", staged.display()))?;
    fs::rename(&staged, path).with_context(|| format!("failed to replace {}", path.display()))
}

fn predicate(terminal: TerminalKind) -> String {
    format!(
        r#"process ==[c] "{}" AND subsystem == "com.apple.AppKit" AND category == "ScreenTime" AND eventMessage BEGINSWITH "{STATE_PREFIX}""#,
        terminal.process_name()
    )
}

/// One lockout change: the app's pid, and whether it is now locked out.
fn parse_event(line: &str) -> Result<Option<(i32, bool)>> {
    // `log` opens with a plain-text banner and `log show` closes with a
    // summary object; neither is an event.
    let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
        return Ok(None);
    };
    let Some(state) = value
        .get("eventMessage")
        .and_then(serde_json::Value::as_str)
        .and_then(|message| message.strip_prefix(STATE_PREFIX))
    else {
        return Ok(None);
    };
    let engaged = match state {
        "engaged" => true,
        "disengaged" => false,
        other => anyhow::bail!("unrecognized lockout state '{other}'"),
    };
    let pid = value
        .get("processID")
        .and_then(serde_json::Value::as_i64)
        .and_then(|pid| i32::try_from(pid).ok())
        .context("lockout event has no process id")?;
    Ok(Some((pid, engaged)))
}

/// `log stream` running for the daemon's lifetime, keeping the state file
/// current for restores and for `session-guard lockout`.
pub struct Watcher {
    stream: Child,
    reading: Arc<Mutex<Option<Child>>>,
    follower: JoinHandle<Result<()>>,
}

impl Watcher {
    pub fn start(terminal: TerminalKind) -> Result<Self> {
        let path = paths::lockout_state()?;
        let previous = read_record(&path).ok().flatten();
        if let Some(previous) = &previous {
            stop_orphaned_stream(previous);
        }
        let mut stream = Command::new("log")
            .args([
                "stream",
                "--style",
                "ndjson",
                "--predicate",
                &predicate(terminal),
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .context("failed to start the lockout log stream")?;
        let stdout = stream
            .stdout
            .take()
            .context("lockout log stream has no output")?;
        let record = Record {
            daemon: Instance::of(i32::try_from(std::process::id())?)?,
            watcher: Instance::of(i32::try_from(stream.id())?)?,
            seed: Seed::Reading,
            engaged: BTreeSet::new(),
            followed_until: None,
        };
        write_record(&path, &record)?;
        let reading = Arc::new(Mutex::new(None));
        let follower = {
            let reading = Arc::clone(&reading);
            thread::spawn(move || {
                let mut record = record;
                let result = follow(
                    terminal,
                    &path,
                    &mut record,
                    previous.as_ref(),
                    stdout,
                    &reading,
                );
                if record.seed != Seed::Reading {
                    record.followed_until = Some(chrono::Utc::now());
                }
                write_record(&path, &record)?;
                result
            })
        };
        Ok(Self {
            stream,
            reading,
            follower,
        })
    }

    pub fn is_running(&self) -> bool {
        !self.follower.is_finished()
    }

    /// Stops a watcher on purpose; how its follower ended is the stop's doing.
    pub fn stop(self) -> Result<()> {
        self.shut_down().map(|_| ())
    }

    /// Cleans up after a watcher that ended on its own, and says why it did.
    pub fn reap(self) -> Result<()> {
        self.shut_down()?
    }

    fn shut_down(mut self) -> Result<Result<()>> {
        if let Some(reading) = self
            .reading
            .lock()
            .map_err(|_| anyhow::anyhow!("lockout log reader panicked"))?
            .as_mut()
        {
            let _ = reading.kill();
        }
        let _ = self.stream.kill();
        self.stream
            .wait()
            .context("failed to wait for the lockout log stream")?;
        self.follower
            .join()
            .map_err(|_| anyhow::anyhow!("lockout watcher panicked"))
    }
}

// A daemon killed outright leaves its stream running until the stream next
// writes to the closed pipe, which may be the next lockout.
fn stop_orphaned_stream(previous: &Record) {
    if previous.watcher.is_running() {
        unsafe {
            libc::kill(previous.watcher.pid, libc::SIGTERM);
        }
    }
}

fn follow(
    terminal: TerminalKind,
    path: &Path,
    record: &mut Record,
    previous: Option<&Record>,
    stream: ChildStdout,
    reading: &Mutex<Option<Child>>,
) -> Result<()> {
    let current = current_instance(terminal);
    if let Some(instance) = &current {
        let resumed = previous.and_then(|previous| previous.resume(instance));
        let engaged = read_stored(terminal, instance, resumed.map(|(_, from)| from), reading)?
            .or(resumed.map(|(engaged, _)| engaged))
            .unwrap_or(false);
        if engaged {
            record.engaged.insert(instance.clone());
        }
    }
    record.seed = Seed::Read { terminal: current };
    write_record(path, record)?;

    for line in BufReader::new(stream).lines() {
        let Some((pid, engaged)) = parse_event(&line?)? else {
            continue;
        };
        let instance = Instance::of(pid)?;
        record.engaged.retain(Instance::is_running);
        if engaged {
            record.engaged.insert(instance);
        } else {
            record.engaged.remove(&instance);
        }
        write_record(path, record)?;
    }
    anyhow::bail!("the lockout log stream ended")
}

/// `instance`'s state after its last change in the stored log since `from`,
/// or since its launch; None when it logged no change there.
fn read_stored(
    terminal: TerminalKind,
    instance: &Instance,
    from: Option<chrono::DateTime<chrono::Utc>>,
    reading: &Mutex<Option<Child>>,
) -> Result<Option<bool>> {
    let launched = process::parse_identity(&instance.started)
        .with_context(|| format!("unreadable start time for pid {}", instance.pid))?;
    let started = from.map_or(launched, |from| from.naive_utc().max(launched));
    let mut child = Command::new("log")
        .args([
            "show",
            "--style",
            "ndjson",
            "--start",
            &started.format("%Y-%m-%d %H:%M:%S+0000").to_string(),
            "--predicate",
            &predicate(terminal),
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("failed to read the stored log")?;
    let mut stdout = child.stdout.take().context("log show has no output")?;
    *reading
        .lock()
        .map_err(|_| anyhow::anyhow!("lockout watcher panicked"))? = Some(child);
    let mut output = String::new();
    let read = stdout.read_to_string(&mut output);
    let status = reading
        .lock()
        .map_err(|_| anyhow::anyhow!("lockout watcher panicked"))?
        .take()
        .context("stored log reader went missing")?
        .wait()
        .context("failed to wait for log show")?;
    read.context("failed to read log show output")?;
    if !status.success() {
        anyhow::bail!("log show failed with status {status}");
    }
    let mut engaged = None;
    for line in output.lines() {
        if let Some((pid, state)) = parse_event(line)?
            && pid == instance.pid
        {
            engaged = Some(state);
        }
    }
    Ok(engaged)
}

/// `session-guard lockout`: 0 when the terminal is not locked out (or not
/// running), 1 while it is, 2 when that cannot be known.
pub fn run(wait: bool) -> ! {
    let terminal = match crate::commands::daemon::configured_terminal() {
        Ok(terminal) => terminal,
        Err(error) => {
            eprintln!("cannot tell whether the terminal is locked out: {error:#}");
            std::process::exit(2);
        }
    };
    loop {
        match lockout(terminal) {
            Lockout::Clear => std::process::exit(0),
            Lockout::Engaged | Lockout::Reading if wait => thread::sleep(WAIT_INTERVAL),
            Lockout::Engaged => {
                eprintln!("{terminal} is locked out by Screen Time");
                std::process::exit(1);
            }
            Lockout::Reading => {
                eprintln!(
                    "cannot tell yet whether {terminal} is locked out: the lockout watcher is still reading its state from the log"
                );
                std::process::exit(2);
            }
            Lockout::Unknown(reason) => {
                eprintln!("cannot tell whether {terminal} is locked out: {reason}");
                std::process::exit(2);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn instance(pid: i32, started: &str) -> Instance {
        Instance {
            pid,
            started: started.to_string(),
        }
    }

    fn record(seed: Seed, engaged: &[Instance]) -> Record {
        Record {
            daemon: instance(10, "Sun Oct 18 11:00:00 2026"),
            watcher: instance(11, "Sun Oct 18 11:00:00 2026"),
            seed,
            engaged: engaged.iter().cloned().collect(),
            followed_until: None,
        }
    }

    const EARLIER: &str = "Sun Oct 18 10:00:00 2026";
    const LATER: &str = "Sun Oct 18 11:39:53 2026";

    #[test]
    fn events_carry_the_app_pid_and_its_new_state() {
        let line = r#"{"processID":627,"eventMessage":"LockoutUI changed state: engaged"}"#;
        assert_eq!(parse_event(line).unwrap(), Some((627, true)));
        let line = r#"{"processID":627,"eventMessage":"LockoutUI changed state: disengaged"}"#;
        assert_eq!(parse_event(line).unwrap(), Some((627, false)));
    }

    #[test]
    fn the_banner_and_summary_are_not_events() {
        assert_eq!(
            parse_event("Filtering the log data using ...").unwrap(),
            None
        );
        assert_eq!(parse_event(r#"{"count":2,"finished":1}"#).unwrap(), None);
    }

    #[test]
    fn an_unknown_state_word_fails_instead_of_reading_as_unlocked() {
        let line = r#"{"processID":627,"eventMessage":"LockoutUI changed state: paused"}"#;
        assert!(parse_event(line).is_err());
    }

    #[test]
    fn an_engaged_terminal_is_locked_out_until_it_disengages() {
        let terminal = instance(627, EARLIER);
        let read = Seed::Read {
            terminal: Some(terminal.clone()),
        };
        let locked = record(read.clone(), std::slice::from_ref(&terminal));
        assert_eq!(locked.lockout(Some(&terminal), |_| true), Lockout::Engaged);
        let lifted = record(read, &[]);
        assert_eq!(lifted.lockout(Some(&terminal), |_| true), Lockout::Clear);
    }

    #[test]
    fn a_recycled_pid_does_not_inherit_a_lockout() {
        let locked = instance(627, EARLIER);
        let successor = instance(627, LATER);
        let record = record(Seed::Read { terminal: None }, &[locked]);
        assert_eq!(record.lockout(Some(&successor), |_| true), Lockout::Clear);
    }

    #[test]
    fn a_terminal_older_than_the_watcher_needs_its_stored_state_read() {
        let terminal = instance(627, EARLIER);
        let reading = record(Seed::Reading, &[]);
        assert_eq!(reading.lockout(Some(&terminal), |_| true), Lockout::Reading);
        let unread = record(Seed::Read { terminal: None }, &[]);
        assert!(matches!(
            unread.lockout(Some(&terminal), |_| true),
            Lockout::Unknown(_)
        ));
    }

    #[test]
    fn a_new_watcher_resumes_where_a_stopped_one_left_off() {
        let until = chrono::Utc::now();
        let seeded = instance(627, EARLIER);
        let launched_later = instance(700, LATER);
        let mut stopped = record(
            Seed::Read {
                terminal: Some(seeded.clone()),
            },
            std::slice::from_ref(&seeded),
        );
        stopped.followed_until = Some(until);
        assert_eq!(
            stopped.resume(&seeded),
            Some((true, until - RESUME_OVERLAP))
        );
        assert_eq!(
            stopped.resume(&launched_later),
            Some((false, until - RESUME_OVERLAP))
        );
        assert_eq!(stopped.resume(&instance(800, EARLIER)), None);
    }

    #[test]
    fn a_watcher_that_never_finished_reading_cannot_be_resumed() {
        let terminal = instance(627, EARLIER);
        let mut unread = record(Seed::Reading, &[]);
        unread.followed_until = Some(chrono::Utc::now());
        assert_eq!(unread.resume(&terminal), None);
        let crashed = record(
            Seed::Read {
                terminal: Some(terminal.clone()),
            },
            &[],
        );
        assert_eq!(crashed.resume(&terminal), None);
    }

    #[test]
    fn a_stopped_watcher_knows_nothing_but_a_closed_terminal_is_clear() {
        let terminal = instance(37160, LATER);
        let record = record(Seed::Read { terminal: None }, &[]);
        assert!(matches!(
            record.lockout(Some(&terminal), |_| false),
            Lockout::Unknown(_)
        ));
        assert_eq!(record.lockout(None, |_| false), Lockout::Clear);
    }
}
