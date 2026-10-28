//! Whether Screen Time has the configured terminal locked out. `AppKit` swaps
//! a locked-out app's native fullscreen spaces for overlays, and a tab added
//! under them takes the whole app down when the lockout lifts (2026-10-18:
//! Ghostty aborted on `NSWindowStackController`'s "windowToTakeFrom should be
//! in FS"). The policy behind a lockout needs a private entitlement to read,
//! so the daemon follows the line `AppKit` logs in the terminal on every
//! change. A terminal that launches unlocked logs nothing there, so for a
//! launch the daemon follows Screen Time's lockout agent instead: it asks for
//! each launched app's policy in turn, answers in order, and logs a lockout
//! right after the answer that decided it.

use crate::TerminalKind;
use crate::adapters::adapter_for;
use crate::paths;
use crate::process::{self, ProcessIdentityStatus};
use anyhow::{Context, Result};
use chrono::Timelike;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, VecDeque};
use std::fs;
use std::io::{BufRead, BufReader, Read};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::Path;
use std::process::{Child, ChildStdout, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const STATE_PREFIX: &str = "LockoutUI changed state: ";
const CHECKIN_PREFIX: &str = "CHECKIN:0x";
const BIRTH_PREFIX: &str = "Calling block with kLSNotifyApplicationBirth ";
// The launch notice lists the app's properties in key order and is cut off
// after about a kilobyte; this key comes third, before any long path.
const BIRTH_APP_KEY: &str = r#""BundleIdentifierLowerCase"=""#;
const BIRTH_TYPE_KEY: &str = r#""ApplicationType"=""#;
// The agent asks about a launch only for an app of this type.
const FOREGROUND: &str = "Foreground";
const REQUESTED_PREFIX: &str = "Requesting policy for bundle IDs: ";
const ANSWERED_PREFIX: &str = "Successfully requested policies for ";
const FAILED_PREFIX: &str = "Error requesting policies: ";
const POLICY_CHANGED_PREFIX: &str = "Received policy changed notification";
const BEGAN_PREFIX: &str = "Begin lockout on app: ";
const AGENT: &str = "lockoutagent";
// The tests stand in for the agent, and for its absence, by another name.
const AGENT_PROCESS_VARIABLE: &str = "SESSION_GUARD_LOCKOUT_AGENT";
// The lockout agent logs a lockout right after the answer that decided it, on
// the same thread: 0.2 ms after it for one app, 15 ms for nine. Nothing marks
// an answer that locked nothing, so this is how long one gets to show.
const BEGIN_GRACE: Duration = Duration::from_millis(250);
const WAIT_INTERVAL: Duration = Duration::from_millis(500);
// A stream can die a moment before its follower sees the end of it, so a
// resumed read starts this far before the old follower stopped. Reading part
// of what it already read again changes nothing.
const RESUME_OVERLAP: chrono::TimeDelta = chrono::TimeDelta::seconds(60);
// A launch's stream counts the agent's requests still open when it starts
// from this far back in the stored log. The slowest answer measured took 1.3
// seconds; a request open for longer than this would be missed.
const OPEN_REQUESTS_LOOKBACK: chrono::TimeDelta = chrono::TimeDelta::seconds(60);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Lockout {
    Clear,
    Engaged,
    /// The terminal is not running, and a tab opened as it launches would
    /// come before Screen Time's answer for the launch.
    NotRunning,
    /// The watcher is still reading the terminal's state from the stored log.
    Reading,
    /// The terminal just launched and Screen Time has not answered for it yet.
    Deciding,
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
    /// Instances `AppKit` reports locked out.
    engaged: BTreeSet<Instance>,
    /// When this watcher stopped following, once its seed was read: the next
    /// daemon reads the stored log from here instead of from the terminal's
    /// launch.
    #[serde(default)]
    followed_until: Option<chrono::DateTime<chrono::Utc>>,
    /// Instances `AppKit` has reported on; its word on them is final.
    #[serde(default)]
    reported: BTreeSet<Instance>,
    /// Launches the lockout agent locked out right after answering for them.
    #[serde(default)]
    began: BTreeSet<Instance>,
    /// Launches the lockout agent answered for without a lockout, and a
    /// terminal read from the stored log with no launch left undecided.
    #[serde(default)]
    decided: BTreeSet<Instance>,
}

impl Record {
    fn lockout(
        &self,
        current: Option<&Instance>,
        launch: Option<&Launch>,
        running: impl Fn(&Instance) -> bool,
    ) -> Lockout {
        let watching = running(&self.daemon) && running(&self.watcher);
        // AppKit's word on the instance is final, even over a launch still
        // waiting for the agent's answer.
        if let Some(current) = current
            && watching
            && matches!(self.seed, Seed::Read { .. })
        {
            if self.engaged.contains(current) {
                return Lockout::Engaged;
            }
            if self.reported.contains(current) {
                return Lockout::Clear;
            }
        }
        if launch.is_some_and(|launch| launch.pending(current, &running)) {
            return Lockout::Deciding;
        }
        let Some(current) = current else {
            return launch
                .and_then(|launch| launch.failure(&running))
                .map_or(Lockout::NotRunning, Lockout::Unknown);
        };
        if !watching {
            return Lockout::Unknown("the session-guard lockout watcher is not running".into());
        }
        let Seed::Read { terminal } = &self.seed else {
            return Lockout::Reading;
        };
        if let Some(locked) = launch.and_then(|launch| launch.answer_for(current)) {
            return if locked {
                Lockout::Engaged
            } else {
                Lockout::Clear
            };
        }
        if self.began.contains(current) {
            return Lockout::Engaged;
        }
        if self.decided.contains(current) {
            return Lockout::Clear;
        }
        if terminal.as_ref() == Some(current) || started_no_earlier(current, &self.watcher) {
            return Lockout::Deciding;
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

    /// Since when a new watcher still has to read the stored log for
    /// `instance`, picking it up where this stopped one left off.
    fn resume(&self, instance: &Instance) -> Option<chrono::DateTime<chrono::Utc>> {
        let until = self.followed_until?;
        (self.engaged.contains(instance) || self.covers(instance)).then_some(until - RESUME_OVERLAP)
    }

    /// Takes over what a stopped watcher knew about `instance`.
    fn carry(&mut self, previous: &Self, instance: &Instance) {
        for (known, before) in [
            (&mut self.engaged, &previous.engaged),
            (&mut self.reported, &previous.reported),
            (&mut self.began, &previous.began),
            (&mut self.decided, &previous.decided),
        ] {
            if before.contains(instance) {
                known.insert(instance.clone());
            }
        }
    }

    fn forget_exited(&mut self) {
        for known in [
            &mut self.engaged,
            &mut self.reported,
            &mut self.began,
            &mut self.decided,
        ] {
            known.retain(Instance::is_running);
        }
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

/// A launch of the terminal by a process that watched the lockout agent's
/// launch notices, which name the app, to read its answer for that launch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Launch {
    by: Instance,
    /// Taken just before the launch; an instance started earlier is not it.
    at: chrono::DateTime<chrono::Utc>,
    outcome: Outcome,
    /// The debug-level stream watching the launch, while it runs.
    #[serde(default)]
    stream: Option<Instance>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
enum Outcome {
    Pending,
    Answered { terminal: Instance, locked: bool },
    Failed(String),
}

impl Launch {
    fn pending(&self, current: Option<&Instance>, running: impl Fn(&Instance) -> bool) -> bool {
        self.outcome == Outcome::Pending
            && running(&self.by)
            && current.is_none_or(|current| self.started(current))
    }

    fn started(&self, instance: &Instance) -> bool {
        process::parse_identity(&instance.started).is_some_and(|started| {
            started >= self.at.naive_utc().with_nanosecond(0).unwrap_or_default()
        })
    }

    fn answer_for(&self, instance: &Instance) -> Option<bool> {
        match &self.outcome {
            Outcome::Answered { terminal, locked } if terminal == instance => Some(*locked),
            Outcome::Answered { .. } | Outcome::Pending | Outcome::Failed(_) => None,
        }
    }

    /// Why the launch failed, while the process that tried is still up. The
    /// terminal is not running then, and launching it again on every pass
    /// would relaunch it every second; restores hold, saying why, until the
    /// terminal runs or the daemon restarts.
    fn failure(&self, running: impl Fn(&Instance) -> bool) -> Option<String> {
        match &self.outcome {
            Outcome::Failed(reason) => {
                running(&self.by).then(|| format!("launching it failed: {reason}"))
            }
            Outcome::Pending | Outcome::Answered { .. } => None,
        }
    }
}

pub fn lockout(terminal: TerminalKind) -> Lockout {
    let current = current_instance(terminal);
    let (state, launch) = match (paths::lockout_state(), paths::lockout_launch()) {
        (Ok(state), Ok(launch)) => (state, launch),
        (Err(error), _) | (_, Err(error)) => return Lockout::Unknown(format!("{error:#}")),
    };
    let launch = match read_json::<Launch>(&launch) {
        Ok(launch) => launch,
        Err(error) => return Lockout::Unknown(format!("{error:#}")),
    };
    match read_json::<Record>(&state) {
        Ok(Some(record)) => record.lockout(current.as_ref(), launch.as_ref(), Instance::is_running),
        Ok(None)
            if launch
                .as_ref()
                .is_some_and(|launch| launch.pending(current.as_ref(), Instance::is_running)) =>
        {
            Lockout::Deciding
        }
        Ok(None) if current.is_none() => Lockout::NotRunning,
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

/// Without its lockout agent Screen Time locks nothing out, and nothing would
/// ever answer for a launch.
fn agent_running() -> bool {
    !process::pids_named_ignoring_case(&agent_process()).is_empty()
}

fn agent_process() -> String {
    std::env::var(AGENT_PROCESS_VARIABLE).unwrap_or_else(|_| AGENT.to_string())
}

fn read_json<T: DeserializeOwned>(path: &Path) -> Result<Option<T>> {
    match fs::read(path) {
        Ok(contents) => serde_json::from_slice(&contents)
            .map(Some)
            .with_context(|| format!("failed to parse {}", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("failed to read {}", path.display())),
    }
}

fn write_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let staged = path.with_extension("json.new");
    fs::write(&staged, serde_json::to_vec(value)?)
        .with_context(|| format!("failed to write {}", staged.display()))?;
    fs::rename(&staged, path).with_context(|| format!("failed to replace {}", path.display()))
}

fn agent_clause(launch_notice: &str) -> String {
    format!(
        r#"process == "{AGENT}" AND ({launch_notice}eventMessage BEGINSWITH "{POLICY_CHANGED_PREFIX}" OR eventMessage BEGINSWITH "{REQUESTED_PREFIX}" OR eventMessage BEGINSWITH "{ANSWERED_PREFIX}" OR eventMessage BEGINSWITH "{FAILED_PREFIX}" OR eventMessage BEGINSWITH "{BEGAN_PREFIX}")"#
    )
}

fn predicate(terminal: TerminalKind) -> String {
    format!(
        r#"(process ==[c] "{}" AND subsystem == "com.apple.AppKit" AND category == "ScreenTime" AND eventMessage BEGINSWITH "{STATE_PREFIX}") OR (process == "launchservicesd" AND eventMessage BEGINSWITH "{CHECKIN_PREFIX}") OR ({})"#,
        terminal.process_name(),
        agent_clause("")
    )
}

fn launch_predicate() -> String {
    agent_clause(&format!(r#"eventMessage BEGINSWITH "{BIRTH_PREFIX}" OR "#))
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Event {
    /// `AppKit` in the terminal: whether Screen Time now has it locked out.
    Lockout { pid: i32, engaged: bool },
    /// `LaunchServices` checking in a launched app.
    CheckedIn { pid: i32, app: String },
    /// The lockout agent hearing of an app's launch, named in lower case;
    /// only a stream at debug level carries it.
    Born { app: String, foreground: bool },
    /// The policy changed, as at the start and end of Downtime, so the
    /// agent's next request is for every running app, not for a launch.
    PolicyChanged,
    /// The lockout agent asking for a launched app's policy, or for every
    /// running app's after a policy change.
    Requested,
    /// The answer to one of the agent's requests, logged on `thread`: for
    /// how many apps, or None when the request failed.
    Answered { thread: u64, apps: Option<usize> },
    /// The agent locking an app out, right after the answer that decided it
    /// and on the same thread.
    Began { thread: u64 },
}

fn parse_event(line: &str) -> Result<Option<Event>> {
    // `log` opens with a plain-text banner and `log show` closes with a
    // summary object; neither is an event.
    let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
        return Ok(None);
    };
    let Some(message) = value
        .get("eventMessage")
        .and_then(serde_json::Value::as_str)
    else {
        return Ok(None);
    };
    let thread = || {
        value
            .get("threadID")
            .and_then(serde_json::Value::as_u64)
            .with_context(|| format!("no thread for '{message}'"))
    };
    if message.starts_with(REQUESTED_PREFIX) {
        return Ok(Some(Event::Requested));
    }
    if message.starts_with(POLICY_CHANGED_PREFIX) {
        return Ok(Some(Event::PolicyChanged));
    }
    if let Some(apps) = message.strip_prefix(ANSWERED_PREFIX) {
        let apps = apps
            .split_whitespace()
            .next()
            .and_then(|apps| apps.parse().ok())
            .with_context(|| format!("unrecognized answer '{message}'"))?;
        return Ok(Some(Event::Answered {
            thread: thread()?,
            apps: Some(apps),
        }));
    }
    if message.starts_with(FAILED_PREFIX) {
        return Ok(Some(Event::Answered {
            thread: thread()?,
            apps: None,
        }));
    }
    if message.starts_with(BEGAN_PREFIX) {
        return Ok(Some(Event::Began { thread: thread()? }));
    }
    if message.starts_with(CHECKIN_PREFIX) {
        let mut fields = message.split_whitespace().skip(1);
        let (Some(pid), Some(app)) = (fields.next(), fields.next()) else {
            anyhow::bail!("unrecognized check-in '{message}'");
        };
        return Ok(Some(Event::CheckedIn {
            pid: pid
                .parse()
                .with_context(|| format!("unrecognized check-in '{message}'"))?,
            app: app.to_string(),
        }));
    }
    if let Some(notice) = message.strip_prefix(BIRTH_PREFIX) {
        let field = |key: &str| {
            notice
                .split_once(key)
                .and_then(|(_, rest)| rest.split_once('"'))
                .map(|(value, _)| value.to_string())
                .with_context(|| format!("launch notice has no {key}: '{message}'"))
        };
        return Ok(Some(Event::Born {
            app: field(BIRTH_APP_KEY)?,
            foreground: field(BIRTH_TYPE_KEY)? == FOREGROUND,
        }));
    }
    let Some(state) = message.strip_prefix(STATE_PREFIX) else {
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
    Ok(Some(Event::Lockout { pid, engaged }))
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
        let previous = read_json::<Record>(&path).ok().flatten();
        if let Some(previous) = &previous {
            stop_orphaned_stream(previous);
        }
        stop_orphaned_launch_stream(&paths::lockout_launch()?);
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
            reported: BTreeSet::new(),
            began: BTreeSet::new(),
            decided: BTreeSet::new(),
        };
        write_json(&path, &record)?;
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
                write_json(&path, &record)?;
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
    let mut stream = BufReader::new(stream);
    let mut answers = Answers::default();
    let current = current_instance(terminal);
    let resumed = current.as_ref().and_then(|instance| {
        let previous = previous?;
        let from = previous.resume(instance)?;
        record.carry(previous, instance);
        Some(from)
    });
    if let Some(instance) = &current {
        for line in read_stored(terminal, instance, resumed, reading)?.lines() {
            answers.follow(line, terminal, record)?;
        }
    }
    // A change the stream delivered while the stored log was being read is
    // newer than what that read found.
    while readable(&stream, 0)? {
        answers.follow(&read_line(&mut stream)?, terminal, record)?;
    }
    if let Some(instance) = &current
        && !answers.deciding(instance)
    {
        record.decided.insert(instance.clone());
    }
    record.seed = Seed::Read { terminal: current };
    write_json(path, record)?;

    // Watched for as long as the agent runs: one that exits takes its open
    // requests with it, and its count with them.
    let mut agent = None;
    loop {
        if agent.is_none() {
            agent = AgentExit::watch()?;
            if agent.is_none() {
                for decision in answers.agent_gone() {
                    decision.record(record);
                }
            }
        }
        match next(&stream, agent.as_ref(), answers.deadline())? {
            Next::Line => answers.follow(&read_line(&mut stream)?, terminal, record)?,
            Next::AgentExited => {
                agent = None;
                for decision in answers.agent_gone() {
                    decision.record(record);
                }
            }
            Next::Deadline => {
                for decision in answers.expire() {
                    decision.record(record);
                }
            }
        }
        write_json(path, record)?;
    }
}

/// Readable once Screen Time's lockout agent exits. launchd starts it on
/// demand, so it can end while a launch waits for its answer.
struct AgentExit {
    queue: OwnedFd,
}

impl AgentExit {
    /// None when the agent is not running.
    fn watch() -> Result<Option<Self>> {
        let pids = process::pids_named_ignoring_case(&agent_process());
        if pids.is_empty() {
            return Ok(None);
        }
        let queue = unsafe { libc::kqueue() };
        if queue < 0 {
            return Err(std::io::Error::last_os_error())
                .context("failed to watch the lockout agent");
        }
        let queue = unsafe { OwnedFd::from_raw_fd(queue) };
        let mut watched = 0;
        for pid in pids {
            let change = libc::kevent {
                ident: usize::try_from(pid)?,
                filter: libc::EVFILT_PROC,
                flags: libc::EV_ADD | libc::EV_ONESHOT,
                fflags: libc::NOTE_EXIT,
                data: 0,
                udata: std::ptr::null_mut(),
            };
            let registered = unsafe {
                libc::kevent(
                    queue.as_raw_fd(),
                    &raw const change,
                    1,
                    std::ptr::null_mut(),
                    0,
                    std::ptr::null(),
                )
            };
            // A pid that exited since the listing cannot be watched.
            if registered == 0 {
                watched += 1;
            }
        }
        Ok((watched > 0).then_some(Self { queue }))
    }
}

enum Next {
    Line,
    AgentExited,
    Deadline,
}

/// Waits for a line from `stream`, the agent's exit, or `deadline`.
fn next(
    stream: &BufReader<ChildStdout>,
    agent: Option<&AgentExit>,
    deadline: Option<Instant>,
) -> Result<Next> {
    if !stream.buffer().is_empty() {
        return Ok(Next::Line);
    }
    let mut ready = vec![libc::pollfd {
        fd: stream.get_ref().as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    }];
    if let Some(agent) = agent {
        ready.push(libc::pollfd {
            fd: agent.queue.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        });
    }
    let timeout = deadline.map_or(Ok(-1), |deadline| {
        i32::try_from(
            deadline
                .saturating_duration_since(Instant::now())
                .as_millis(),
        )
    })?;
    let count = libc::nfds_t::try_from(ready.len())?;
    match unsafe { libc::poll(ready.as_mut_ptr(), count, timeout) } {
        -1 => Err(std::io::Error::last_os_error()).context("failed to poll the lockout log stream"),
        0 => Ok(Next::Deadline),
        _ if ready[0].revents != 0 => Ok(Next::Line),
        _ => Ok(Next::AgentExited),
    }
}

/// Whether the stream has a line ready, waiting up to `timeout_ms` for one.
fn readable(stream: &BufReader<ChildStdout>, timeout_ms: i32) -> Result<bool> {
    if !stream.buffer().is_empty() {
        return Ok(true);
    }
    let mut ready = libc::pollfd {
        fd: stream.get_ref().as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    match unsafe { libc::poll(&raw mut ready, 1, timeout_ms) } {
        -1 => Err(std::io::Error::last_os_error()).context("failed to poll the lockout log stream"),
        0 => Ok(false),
        _ => Ok(true),
    }
}

fn read_line(stream: &mut BufReader<ChildStdout>) -> Result<String> {
    let mut line = String::new();
    if stream.read_line(&mut line)? == 0 {
        anyhow::bail!("the lockout log stream ended");
    }
    Ok(line)
}

/// What the lockout agent decided for a launch of the terminal.
#[derive(Debug, PartialEq, Eq)]
struct Decision {
    terminal: Instance,
    locked: bool,
}

impl Decision {
    fn record(self, record: &mut Record) {
        if self.locked {
            record.began.insert(self.terminal);
        } else {
            record.decided.insert(self.terminal);
        }
    }
}

/// Pairs the lockout agent's answers with the terminal's launch. The agent
/// asks about each foreground launch as it hears of it, and nothing in its
/// lines ties an answer to its launch: they run on several threads and the
/// app's name is redacted. So this takes the answers to come in the order of
/// the launches, as they did for launches heard of together. An answer for
/// more than one app is the one the agent asks for every running app when
/// its policy changes, at the start and end of Downtime; one that covers a
/// single running app would pass for a launch's.
///
/// Answers for requests asked before the terminal's launch come first: the
/// daemon's stream counts the ones open when the terminal checks in, and a
/// launch's own stream reads them from the stored log. A launch the agent
/// hears of in the moment before either, and asks about just after, can
/// still take the terminal's place; that window is a few milliseconds.
///
/// Nothing times a launch out. An agent that runs but never asks about a
/// foreground launch would leave the terminal deciding for good, but that
/// has never been seen: every foreground launch measured got its request,
/// and a Mac without Screen Time runs no agent, which `agent_running`
/// settles at once. A timer would trade that for a guess at the agent's
/// slowest answer; add one only with a launch that went unasked.
#[derive(Default)]
struct Answers {
    /// Launches the agent has not answered for yet, oldest first, each with
    /// the terminal's instance if it is the terminal's.
    waiting: VecDeque<Option<Instance>>,
    /// Launch requests asked and not yet answered, as far as this stream has
    /// seen.
    open: OpenRequests,
    /// Answers still due for requests asked before the terminal's launch.
    earlier: usize,
    /// The terminal's launches answered just now, each with the answer's
    /// thread, waiting out `BEGIN_GRACE` for a lockout on that thread.
    answered: Vec<(Instance, u64, Instant)>,
}

/// The agent's launch requests still unanswered. The request after a policy
/// change is the one for every running app, answered for more than one app,
/// and is kept apart.
#[derive(Default)]
struct OpenRequests {
    launches: usize,
    policy_next: bool,
    policies: usize,
}

impl OpenRequests {
    fn take(&mut self, event: &Event) {
        match event {
            Event::PolicyChanged => self.policy_next = true,
            Event::Requested => {
                if std::mem::take(&mut self.policy_next) {
                    self.policies += 1;
                } else {
                    self.launches += 1;
                }
            }
            Event::Answered { apps, .. } => {
                if apps.is_some_and(|apps| apps > 1) && self.policies > 0 {
                    self.policies -= 1;
                } else {
                    // An answer for many apps with no policy request open
                    // answers one whose notice came before this count began,
                    // and which was counted as a launch's.
                    self.launches = self.launches.saturating_sub(1);
                }
            }
            Event::Began { .. }
            | Event::Lockout { .. }
            | Event::CheckedIn { .. }
            | Event::Born { .. } => {}
        }
    }
}

impl Answers {
    fn deadline(&self) -> Option<Instant> {
        self.answered
            .iter()
            .map(|(_, _, at)| *at + BEGIN_GRACE)
            .min()
    }

    fn launched(&mut self, terminal: Option<Instance>) {
        self.waiting.push_back(terminal);
    }

    /// The terminal checked in; what the agent was asked before is answered
    /// first.
    fn checked_in(&mut self, terminal: Instance) {
        self.earlier = self.open.launches;
        self.waiting.push_back(Some(terminal));
    }

    fn deciding(&self, terminal: &Instance) -> bool {
        self.waiting
            .iter()
            .flatten()
            .any(|waiting| waiting == terminal)
            || self
                .answered
                .iter()
                .any(|(answered, _, _)| answered == terminal)
    }

    fn is_deciding(&self) -> bool {
        self.waiting.iter().flatten().next().is_some() || !self.answered.is_empty()
    }

    fn take_answered_on(&mut self, thread: u64, locked: bool) -> Vec<Decision> {
        let (on_thread, others) = std::mem::take(&mut self.answered)
            .into_iter()
            .partition(|(_, on, _)| *on == thread);
        self.answered = others;
        on_thread
            .into_iter()
            .map(|(terminal, _, _)| Decision { terminal, locked })
            .collect()
    }

    /// Takes one of the agent's lines; any other event is for the caller.
    fn apply(&mut self, event: &Event) -> Vec<Decision> {
        match event {
            Event::PolicyChanged | Event::Requested => {
                self.open.take(event);
                Vec::new()
            }
            Event::Answered { thread, apps } => {
                self.open.take(event);
                // A thread logs a lockout right after the answer that decided
                // it, so its next answer means the one before locked nothing.
                let mut decisions = self.take_answered_on(*thread, false);
                if apps.is_some_and(|apps| apps > 1) {
                    return decisions;
                }
                if self.earlier > 0 {
                    self.earlier -= 1;
                    return decisions;
                }
                if let Some(Some(asked)) = self.waiting.pop_front() {
                    match apps {
                        // Nothing locks an app out after a failed request;
                        // a lockout that comes later shows in AppKit's line.
                        None => decisions.push(Decision {
                            terminal: asked,
                            locked: false,
                        }),
                        Some(_) => self.answered.push((asked, *thread, Instant::now())),
                    }
                }
                decisions
            }
            Event::Began { thread } => self.take_answered_on(*thread, true),
            Event::Lockout { .. } | Event::CheckedIn { .. } | Event::Born { .. } => Vec::new(),
        }
    }

    /// The answers for the terminal's launches that came with no lockout
    /// after them in time.
    fn expire(&mut self) -> Vec<Decision> {
        let now = Instant::now();
        let (due, waiting) = std::mem::take(&mut self.answered)
            .into_iter()
            .partition(|(_, _, at)| *at + BEGIN_GRACE <= now);
        self.answered = waiting;
        due.into_iter()
            .map(|(terminal, _, _)| Decision {
                terminal,
                locked: false,
            })
            .collect()
    }

    /// The agent exited, so it answers nothing more and locks nothing out.
    fn agent_gone(&mut self) -> Vec<Decision> {
        let waiting = std::mem::take(&mut self.waiting).into_iter().flatten();
        let answered = std::mem::take(&mut self.answered)
            .into_iter()
            .map(|(terminal, _, _)| terminal);
        self.open = OpenRequests::default();
        self.earlier = 0;
        waiting
            .chain(answered)
            .map(|terminal| Decision {
                terminal,
                locked: false,
            })
            .collect()
    }

    /// Takes a line of the watcher's stream into `record`.
    fn follow(&mut self, line: &str, terminal: TerminalKind, record: &mut Record) -> Result<()> {
        let Some(event) = parse_event(line.trim_end())? else {
            return Ok(());
        };
        match &event {
            Event::Lockout { pid, engaged } => {
                // A process gone since it logged can take no tab.
                let Ok(instance) = Instance::of(*pid) else {
                    return Ok(());
                };
                record.forget_exited();
                if *engaged {
                    record.engaged.insert(instance.clone());
                } else {
                    record.engaged.remove(&instance);
                }
                record.reported.insert(instance);
            }
            Event::CheckedIn { pid, app } if app == terminal.bundle_id() => {
                if let Ok(instance) = Instance::of(*pid) {
                    if agent_running() {
                        self.checked_in(instance);
                    } else {
                        record.decided.insert(instance);
                    }
                }
            }
            Event::CheckedIn { .. } | Event::Born { .. } => {}
            Event::PolicyChanged
            | Event::Requested
            | Event::Answered { .. }
            | Event::Began { .. } => {
                for decision in self.apply(&event) {
                    decision.record(record);
                }
            }
        }
        Ok(())
    }
}

/// The stored log since `from`, or since `instance`'s launch.
fn read_stored(
    terminal: TerminalKind,
    instance: &Instance,
    from: Option<chrono::DateTime<chrono::Utc>>,
    reading: &Mutex<Option<Child>>,
) -> Result<String> {
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
    Ok(output)
}

/// Launches the terminal on a thread that reads the lockout agent's answer for
/// the launch; `lockout` reads Deciding until the answer is in. A failure goes
/// to `report` as well as to `lockout`.
pub fn launch(
    terminal: TerminalKind,
    report: impl FnOnce(String) + Send + 'static,
) -> Result<JoinHandle<()>> {
    let path = paths::lockout_launch()?;
    stop_orphaned_launch_stream(&path);
    let by = Instance::of(i32::try_from(std::process::id())?)?;
    let at = chrono::Utc::now();
    let pending = Launch {
        by,
        at,
        outcome: Outcome::Pending,
        stream: None,
    };
    write_json(&path, &pending)?;
    Ok(thread::spawn(move || {
        let watching = |stream| {
            write_json(
                &path,
                &Launch {
                    stream: Some(stream),
                    ..pending.clone()
                },
            )
        };
        let outcome = match launch_watched(terminal, watching) {
            Ok(Decision { terminal, locked }) => Outcome::Answered { terminal, locked },
            Err(error) => {
                report(format!("{error:#}"));
                Outcome::Failed(format!("{error:#}"))
            }
        };
        let _ = write_json(
            &path,
            &Launch {
                outcome,
                ..pending.clone()
            },
        );
    }))
}

// A launch watched by a process killed outright leaves its debug-level
// stream running, and logging at debug level costs the whole Mac.
fn stop_orphaned_launch_stream(path: &Path) {
    if let Ok(Some(Launch {
        by,
        outcome: Outcome::Pending,
        stream: Some(stream),
        ..
    })) = read_json::<Launch>(path)
        && !by.is_running()
        && stream.is_running()
    {
        unsafe {
            libc::kill(stream.pid, libc::SIGTERM);
        }
    }
}

fn launch_watched(
    terminal: TerminalKind,
    watching: impl FnOnce(Instance) -> Result<()>,
) -> Result<Decision> {
    let adapter = adapter_for(terminal);
    let unlocked = |terminal| Decision {
        terminal,
        locked: false,
    };
    if !agent_running() {
        adapter.launch()?;
        return Ok(unlocked(launched(terminal)?));
    }
    let mut stream = Command::new("log")
        .args([
            "stream",
            "--style",
            "ndjson",
            "--debug",
            "--predicate",
            &launch_predicate(),
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("failed to start the launch log stream")?;
    let decision = Instance::of(i32::try_from(stream.id())?)
        .and_then(watching)
        .and_then(|()| {
            stream
                .stdout
                .take()
                .context("launch log stream has no output")
        })
        .and_then(|stdout| {
            let mut stream = BufReader::new(stdout);
            // `log` prints its banner once the stream is live, so a launch
            // after it cannot slip by.
            read_line(&mut stream)?;
            let live = chrono::Utc::now();
            // A terminal that started before the stream was live was heard of
            // before it, and launching it only brings it forward; the
            // daemon's stream follows it from its check-in instead.
            if current_instance(terminal).is_some() {
                anyhow::bail!("{terminal} was already starting when its launch was watched");
            }
            let mut answers = Answers {
                earlier: open_requests(live)?,
                ..Answers::default()
            };
            let Some(agent) = AgentExit::watch()? else {
                adapter.launch()?;
                return Ok(unlocked(launched(terminal)?));
            };
            adapter.launch()?;
            let launched = launched(terminal)?;
            let app = terminal.bundle_id().to_ascii_lowercase();
            loop {
                let decisions = match next(&stream, Some(&agent), answers.deadline())? {
                    Next::AgentExited => return Ok(unlocked(launched)),
                    Next::Deadline => answers.expire(),
                    Next::Line => match parse_event(read_line(&mut stream)?.trim_end())? {
                        // The agent asks about foreground launches only.
                        Some(Event::Born {
                            app: born,
                            foreground: false,
                        }) if born == app => return Ok(unlocked(launched)),
                        Some(Event::Born {
                            app: born,
                            foreground: true,
                        }) => {
                            answers.launched((born == app).then(|| launched.clone()));
                            Vec::new()
                        }
                        Some(event) => answers.apply(&event),
                        None => Vec::new(),
                    },
                };
                if let Some(decision) = decisions.into_iter().next() {
                    return Ok(decision);
                }
            }
        });
    let _ = stream.kill();
    let _ = stream.wait();
    decision
}

/// How many of the agent's requests were still unanswered at `live`, read
/// from the stored log.
fn open_requests(live: chrono::DateTime<chrono::Utc>) -> Result<usize> {
    // `log show` takes whole seconds only, so it reads up to the second after
    // `live` and leaves out what came later, which the stream delivers.
    let second =
        |at: chrono::DateTime<chrono::Utc>| at.format("%Y-%m-%d %H:%M:%S+0000").to_string();
    let output = Command::new("log")
        .args([
            "show",
            "--style",
            "ndjson",
            "--start",
            &second(live - OPEN_REQUESTS_LOOKBACK),
            "--end",
            &second(live + chrono::TimeDelta::seconds(1)),
            "--predicate",
            &agent_clause(""),
        ])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .context("failed to read the agent's open requests")?;
    if !output.status.success() {
        anyhow::bail!("log show failed with status {}", output.status);
    }
    let mut open = OpenRequests::default();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let Some(event) = parse_event(line)? else {
            continue;
        };
        if logged_at(&serde_json::from_str(line)?)? <= live {
            open.take(&event);
        }
    }
    Ok(open.launches)
}

fn logged_at(line: &serde_json::Value) -> Result<chrono::DateTime<chrono::Utc>> {
    let logged = line
        .get("timestamp")
        .and_then(serde_json::Value::as_str)
        .context("log line has no time")?;
    Ok(
        chrono::DateTime::parse_from_str(logged, "%Y-%m-%d %H:%M:%S%.f%z")
            .with_context(|| format!("unreadable log time '{logged}'"))?
            .to_utc(),
    )
}

fn launched(terminal: TerminalKind) -> Result<Instance> {
    current_instance(terminal)
        .with_context(|| format!("{terminal} is not running after its launch"))
}

/// `session-guard lockout`: 0 when a tab can be opened now, 1 while the
/// terminal is locked out, 2 when that cannot be known yet or the terminal is
/// not running. `--wait` launches a terminal that is not running and waits
/// until a tab can be opened.
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
            Lockout::NotRunning if wait => {
                if let Err(error) = launch(terminal, drop).and_then(|launching| {
                    launching
                        .join()
                        .map_err(|_| anyhow::anyhow!("the launch watcher panicked"))
                }) {
                    eprintln!("cannot launch {terminal}: {error:#}");
                    std::process::exit(2);
                }
            }
            Lockout::Engaged | Lockout::Reading | Lockout::Deciding if wait => {
                thread::sleep(WAIT_INTERVAL);
            }
            Lockout::Engaged => {
                eprintln!("{terminal} is locked out by Screen Time");
                std::process::exit(1);
            }
            Lockout::NotRunning => {
                eprintln!(
                    "{terminal} is not running, and a tab opened as it launches would come before Screen Time's answer; `session-guard lockout --wait` launches it and waits"
                );
                std::process::exit(2);
            }
            Lockout::Deciding => {
                eprintln!(
                    "cannot tell yet whether {terminal} is locked out: Screen Time has not answered for its launch"
                );
                std::process::exit(2);
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
    use serde_json::json;

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
            reported: engaged.iter().cloned().collect(),
            began: BTreeSet::new(),
            decided: BTreeSet::new(),
        }
    }

    fn launch(outcome: Outcome) -> Launch {
        Launch {
            by: instance(12, "Sun Oct 18 11:30:00 2026"),
            at: process::parse_identity("Sun Oct 18 11:39:50 2026")
                .unwrap()
                .and_utc(),
            outcome,
            stream: None,
        }
    }

    const EARLIER: &str = "Sun Oct 18 10:00:00 2026";
    const LATER: &str = "Sun Oct 18 11:39:53 2026";

    #[test]
    fn events_carry_the_app_pid_and_its_new_state() {
        let line = r#"{"processID":627,"eventMessage":"LockoutUI changed state: engaged"}"#;
        assert_eq!(
            parse_event(line).unwrap(),
            Some(Event::Lockout {
                pid: 627,
                engaged: true
            })
        );
        let line = r#"{"processID":627,"eventMessage":"LockoutUI changed state: disengaged"}"#;
        assert_eq!(
            parse_event(line).unwrap(),
            Some(Event::Lockout {
                pid: 627,
                engaged: false
            })
        );
    }

    #[test]
    fn launch_lines_name_the_app_without_any_profile() {
        let checked_in = r#"{"eventMessage":"CHECKIN:0x0-0x32e62e3 18305 com.mitchellh.ghostty","processID":354}"#;
        assert_eq!(
            parse_event(checked_in).unwrap(),
            Some(Event::CheckedIn {
                pid: 18305,
                app: "com.mitchellh.ghostty".into()
            })
        );
        // Captured from lockoutagent on macOS 15, cut off where `log` cuts it.
        let born = include_str!("lockout_launch_notice.json");
        assert_eq!(
            parse_event(born).unwrap(),
            Some(Event::Born {
                app: "com.apple.calculator".into(),
                foreground: true
            })
        );
    }

    #[test]
    fn the_lockout_agent_lines_are_read_with_their_names_redacted() {
        for (message, event) in [
            (
                "Requesting policy for bundle IDs: <private>",
                Event::Requested,
            ),
            (
                "Successfully requested policies for 12 bundleIdentifiers",
                Event::Answered {
                    thread: 7,
                    apps: Some(12),
                },
            ),
            (
                "Error requesting policies: <private>",
                Event::Answered {
                    thread: 7,
                    apps: None,
                },
            ),
            (
                "Begin lockout on app: <private>",
                Event::Began { thread: 7 },
            ),
        ] {
            let line =
                json!({"eventMessage": message, "processID": 762, "threadID": 7}).to_string();
            assert_eq!(parse_event(&line).unwrap(), Some(event));
        }
    }

    #[test]
    fn unreadable_launch_lines_fail_instead_of_being_skipped() {
        let checked_in = r#"{"eventMessage":"CHECKIN:0x0-0x32e62e3 com.mitchellh.ghostty"}"#;
        assert!(parse_event(checked_in).is_err());
        let born = r#"{"eventMessage":"Calling block with kLSNotifyApplicationBirth { \"ApplicationType\"=\"Foreground\"<…>"}"#;
        assert!(parse_event(born).is_err());
        let typeless = r#"{"eventMessage":"Calling block with kLSNotifyApplicationBirth { \"BundleIdentifierLowerCase\"=\"com.apple.talagent\"<…>"}"#;
        assert!(parse_event(typeless).is_err());
    }

    fn decide(answers: &mut Answers, events: &[Event]) -> Vec<Decision> {
        events
            .iter()
            .flat_map(|event| answers.apply(event))
            .collect()
    }

    const ONE: Option<usize> = Some(1);

    fn unlocked(terminal: &Instance) -> Vec<Decision> {
        vec![Decision {
            terminal: terminal.clone(),
            locked: false,
        }]
    }

    fn locked(terminal: &Instance) -> Vec<Decision> {
        vec![Decision {
            terminal: terminal.clone(),
            locked: true,
        }]
    }

    #[test]
    fn the_answer_after_the_launch_decides_it() {
        let launched = instance(700, LATER);
        let mut answers = Answers::default();
        answers.launched(Some(launched.clone()));
        let asked = [
            Event::Requested,
            Event::Answered {
                thread: 7,
                apps: ONE,
            },
        ];
        assert_eq!(decide(&mut answers, &asked), []);
        assert!(answers.deciding(&launched));
        answers.answered[0].2 -= BEGIN_GRACE;
        assert_eq!(answers.expire(), unlocked(&launched));

        let mut answers = Answers::default();
        answers.launched(Some(launched.clone()));
        let asked = [
            Event::Requested,
            Event::Answered {
                thread: 7,
                apps: ONE,
            },
            Event::Began { thread: 7 },
        ];
        assert_eq!(decide(&mut answers, &asked), locked(&launched));
    }

    #[test]
    fn a_lockout_goes_to_the_answer_logged_on_its_thread() {
        // Two launches answered on two threads: the lockout logged after the
        // other answer, on the terminal's thread, is still the terminal's.
        let launched = instance(700, LATER);
        let mut answers = Answers::default();
        answers.launched(Some(launched.clone()));
        answers.launched(None);
        let events = [
            Event::Requested,
            Event::Requested,
            Event::Answered {
                thread: 7,
                apps: ONE,
            },
            Event::Answered {
                thread: 9,
                apps: ONE,
            },
            Event::Began { thread: 7 },
        ];
        assert_eq!(decide(&mut answers, &events), locked(&launched));
    }

    #[test]
    fn the_next_answer_on_the_same_thread_means_no_lockout() {
        let launched = instance(700, LATER);
        let mut answers = Answers::default();
        answers.launched(Some(launched.clone()));
        answers.launched(None);
        let events = [
            Event::Requested,
            Event::Requested,
            Event::Answered {
                thread: 7,
                apps: ONE,
            },
            Event::Answered {
                thread: 7,
                apps: ONE,
            },
            Event::Began { thread: 7 },
        ];
        assert_eq!(decide(&mut answers, &events), unlocked(&launched));
    }

    #[test]
    fn a_failed_request_answers_the_launch_without_a_lockout() {
        let launched = instance(700, LATER);
        let mut answers = Answers::default();
        answers.launched(Some(launched.clone()));
        let events = [
            Event::Requested,
            Event::Answered {
                thread: 7,
                apps: None,
            },
        ];
        assert_eq!(decide(&mut answers, &events), unlocked(&launched));
        assert_eq!(answers.open.launches, 0);
    }

    #[test]
    fn answers_due_for_earlier_requests_do_not_decide_the_launch() {
        let launched = instance(700, LATER);
        let mut answers = Answers::default();
        assert_eq!(decide(&mut answers, &[Event::Requested]), []);
        answers.checked_in(launched.clone());
        let events = [
            Event::Answered {
                thread: 7,
                apps: ONE,
            },
            Event::Began { thread: 7 },
        ];
        assert_eq!(decide(&mut answers, &events), []);
        assert!(answers.deciding(&launched));
        let events = [
            Event::Requested,
            Event::Answered {
                thread: 7,
                apps: ONE,
            },
            Event::Answered {
                thread: 7,
                apps: ONE,
            },
        ];
        assert_eq!(decide(&mut answers, &events), unlocked(&launched));
    }

    #[test]
    fn a_policy_request_open_at_the_check_in_is_no_launches_in_either_order() {
        let launched = instance(700, LATER);
        let policy_open = [Event::PolicyChanged, Event::Requested];
        for policy_first in [true, false] {
            let mut answers = Answers::default();
            assert_eq!(decide(&mut answers, &policy_open), []);
            answers.checked_in(launched.clone());
            assert_eq!(answers.earlier, 0);
            let launch_answer = Event::Answered {
                thread: 7,
                apps: ONE,
            };
            let policy_answer = Event::Answered {
                thread: 9,
                apps: Some(12),
            };
            let answers_in_order = if policy_first {
                [policy_answer, launch_answer]
            } else {
                [launch_answer, policy_answer]
            };
            let mut events = vec![Event::Requested];
            events.extend(answers_in_order);
            events.push(Event::Answered {
                thread: 7,
                apps: ONE,
            });
            assert_eq!(decide(&mut answers, &events), unlocked(&launched));
        }
    }

    #[test]
    fn an_answer_for_many_apps_with_no_policy_request_open_was_counted_as_a_launch() {
        // The count began between a policy notice and its request.
        let mut open = OpenRequests::default();
        open.take(&Event::Requested);
        assert_eq!(open.launches, 1);
        open.take(&Event::Answered {
            thread: 9,
            apps: Some(12),
        });
        assert_eq!(open.launches, 0);
    }

    #[test]
    fn launches_heard_of_together_are_answered_in_the_order_heard() {
        // As logged for Calculator and Dictionary launched at once: both
        // notices, then both requests, then both answers.
        let launched = instance(700, LATER);
        let mut answers = Answers::default();
        answers.launched(None);
        answers.launched(Some(launched.clone()));
        let events = [
            Event::Requested,
            Event::Requested,
            Event::Answered {
                thread: 7,
                apps: ONE,
            },
        ];
        assert_eq!(decide(&mut answers, &events), []);
        assert!(answers.deciding(&launched));
        let events = [
            Event::Answered {
                thread: 9,
                apps: ONE,
            },
            Event::Began { thread: 9 },
        ];
        assert_eq!(decide(&mut answers, &events), locked(&launched));
    }

    #[test]
    fn the_answer_for_every_running_app_is_not_a_launches() {
        // Downtime starting: one answer for 12 apps, then its lockouts.
        let launched = instance(700, LATER);
        let mut answers = Answers::default();
        answers.launched(Some(launched.clone()));
        let mut events = vec![
            Event::Requested,
            Event::Answered {
                thread: 7,
                apps: Some(12),
            },
        ];
        events.extend(std::iter::repeat_n(Event::Began { thread: 7 }, 9));
        assert_eq!(decide(&mut answers, &events), []);
        assert!(answers.deciding(&launched));
    }

    #[test]
    fn an_agent_that_exits_answers_nothing_more() {
        let launched = instance(700, LATER);
        let mut answers = Answers::default();
        answers.launched(Some(launched.clone()));
        assert!(answers.is_deciding());
        assert_eq!(answers.agent_gone(), unlocked(&launched));
        assert!(!answers.is_deciding());
    }

    #[test]
    fn a_background_apps_launch_notice_says_so() {
        let line = json!({
            "eventMessage": r#"Calling block with kLSNotifyApplicationBirth { "ApplicationType"="UIElement", , "BundleIdentifierLowerCase"="com.apple.talagent", "CFBundleExecutable"="talagent"<…>"#,
            "processID": 762,
        })
        .to_string();
        assert_eq!(
            parse_event(&line).unwrap(),
            Some(Event::Born {
                app: "com.apple.talagent".into(),
                foreground: false
            })
        );
    }

    #[test]
    fn a_launch_is_deciding_until_screen_time_answers() {
        let launched = instance(700, LATER);
        let mut record = record(Seed::Read { terminal: None }, &[]);
        assert_eq!(
            record.lockout(Some(&launched), None, |_| true),
            Lockout::Deciding
        );
        record.decided.insert(launched.clone());
        assert_eq!(
            record.lockout(Some(&launched), None, |_| true),
            Lockout::Clear
        );
        record.decided.clear();
        record.began.insert(launched.clone());
        assert_eq!(
            record.lockout(Some(&launched), None, |_| true),
            Lockout::Engaged
        );
        record.reported.insert(launched.clone());
        assert_eq!(
            record.lockout(Some(&launched), None, |_| true),
            Lockout::Clear
        );
    }

    #[test]
    fn a_terminal_read_from_the_stored_log_is_deciding_only_while_its_launch_is() {
        let seeded = instance(627, EARLIER);
        let mut record = record(
            Seed::Read {
                terminal: Some(seeded.clone()),
            },
            &[],
        );
        assert_eq!(
            record.lockout(Some(&seeded), None, |_| true),
            Lockout::Deciding
        );
        record.decided.insert(seeded.clone());
        assert_eq!(
            record.lockout(Some(&seeded), None, |_| true),
            Lockout::Clear
        );
    }

    #[test]
    fn a_watched_launch_answers_for_its_terminal() {
        let launched = instance(700, LATER);
        let mut record = record(Seed::Read { terminal: None }, &[]);
        record.decided.insert(launched.clone());
        let pending = launch(Outcome::Pending);
        assert_eq!(
            record.lockout(None, Some(&pending), |_| true),
            Lockout::Deciding
        );
        assert_eq!(
            record.lockout(Some(&launched), Some(&pending), |_| true),
            Lockout::Deciding
        );
        let locked = launch(Outcome::Answered {
            terminal: launched.clone(),
            locked: true,
        });
        assert_eq!(
            record.lockout(Some(&launched), Some(&locked), |_| true),
            Lockout::Engaged
        );
        let other = launch(Outcome::Answered {
            terminal: instance(800, LATER),
            locked: true,
        });
        assert_eq!(
            record.lockout(Some(&launched), Some(&other), |_| true),
            Lockout::Clear
        );
    }

    #[test]
    fn appkits_word_settles_a_launch_still_waiting_for_its_answer() {
        let launched = instance(700, LATER);
        let pending = launch(Outcome::Pending);
        let mut record = record(Seed::Read { terminal: None }, &[]);
        record.reported.insert(launched.clone());
        assert_eq!(
            record.lockout(Some(&launched), Some(&pending), |_| true),
            Lockout::Clear
        );
        record.engaged.insert(launched.clone());
        assert_eq!(
            record.lockout(Some(&launched), Some(&pending), |_| true),
            Lockout::Engaged
        );
    }

    #[test]
    fn a_launch_by_a_process_that_died_does_not_hold() {
        let record = record(Seed::Read { terminal: None }, &[]);
        let by_alive = |instance: &Instance| instance.pid != 12;
        let pending = launch(Outcome::Pending);
        assert_eq!(
            record.lockout(None, Some(&pending), by_alive),
            Lockout::NotRunning
        );
        let failed = launch(Outcome::Failed("no such app".into()));
        assert_eq!(
            record.lockout(None, Some(&failed), by_alive),
            Lockout::NotRunning
        );
        assert!(matches!(
            record.lockout(None, Some(&failed), |_| true),
            Lockout::Unknown(reason) if reason.contains("no such app")
        ));
    }

    #[test]
    fn a_terminal_running_before_a_pending_launch_is_not_its_launch() {
        let older = instance(627, EARLIER);
        let mut record = record(
            Seed::Read {
                terminal: Some(older.clone()),
            },
            &[],
        );
        record.decided.insert(older.clone());
        assert_eq!(
            record.lockout(Some(&older), Some(&launch(Outcome::Pending)), |_| true),
            Lockout::Clear
        );
    }

    #[test]
    fn log_times_are_read_with_their_offset() {
        let line = json!({"timestamp": "2026-10-27 13:55:16.685569+0545"});
        assert_eq!(
            logged_at(&line).unwrap().to_rfc3339(),
            "2026-10-27T08:10:16.685569+00:00"
        );
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
        assert_eq!(
            locked.lockout(Some(&terminal), None, |_| true),
            Lockout::Engaged
        );
        let mut lifted = record(read, &[]);
        lifted.reported.insert(terminal.clone());
        assert_eq!(
            lifted.lockout(Some(&terminal), None, |_| true),
            Lockout::Clear
        );
    }

    #[test]
    fn a_recycled_pid_does_not_inherit_a_lockout() {
        let locked = instance(627, EARLIER);
        let successor = instance(627, LATER);
        let record = record(Seed::Read { terminal: None }, &[locked]);
        assert_eq!(
            record.lockout(Some(&successor), None, |_| true),
            Lockout::Deciding
        );
    }

    #[test]
    fn a_terminal_older_than_the_watcher_needs_its_stored_state_read() {
        let terminal = instance(627, EARLIER);
        let reading = record(Seed::Reading, &[]);
        assert_eq!(
            reading.lockout(Some(&terminal), None, |_| true),
            Lockout::Reading
        );
        let unread = record(Seed::Read { terminal: None }, &[]);
        assert!(matches!(
            unread.lockout(Some(&terminal), None, |_| true),
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
        stopped.decided.insert(launched_later.clone());
        stopped.followed_until = Some(until);
        assert_eq!(stopped.resume(&seeded), Some(until - RESUME_OVERLAP));
        assert_eq!(
            stopped.resume(&launched_later),
            Some(until - RESUME_OVERLAP)
        );
        assert_eq!(stopped.resume(&instance(800, EARLIER)), None);

        let mut resumed = record(Seed::Reading, &[]);
        resumed.carry(&stopped, &seeded);
        resumed.carry(&stopped, &launched_later);
        assert!(resumed.engaged.contains(&seeded) && resumed.reported.contains(&seeded));
        assert!(resumed.decided.contains(&launched_later));
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
    fn a_stopped_watcher_knows_nothing_and_a_closed_terminal_is_not_running() {
        let terminal = instance(37160, LATER);
        let record = record(Seed::Read { terminal: None }, &[]);
        assert!(matches!(
            record.lockout(Some(&terminal), None, |_| false),
            Lockout::Unknown(_)
        ));
        assert_eq!(record.lockout(None, None, |_| false), Lockout::NotRunning);
    }
}
