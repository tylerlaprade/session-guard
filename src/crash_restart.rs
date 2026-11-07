use crate::TerminalKind;
use std::collections::{HashMap, HashSet};
use std::process::ExitStatus;

/// The terminals that crashed in a way their own exit says is worth a
/// restart, and the instance the daemon started itself, so a restarted
/// terminal that crashes again is left for the user: a crash that comes back
/// with its sessions would otherwise repeat.
#[derive(Debug, Default)]
pub struct CrashRestarts {
    watched: HashMap<i32, TerminalKind>,
    crashed: HashSet<TerminalKind>,
    started: HashMap<TerminalKind, Option<i32>>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Exit {
    Crashed,
    CrashedAfterRestart,
    Ended,
}

impl CrashRestarts {
    /// The running instance of `terminal`; true when the watch has not seen it.
    pub fn running(&mut self, terminal: TerminalKind, pid: i32) -> bool {
        self.crashed.remove(&terminal);
        match self.started.get(&terminal).copied() {
            Some(None) => {
                self.started.insert(terminal, Some(pid));
            }
            Some(Some(started)) if started == pid => {}
            Some(Some(_)) | None => {
                self.started.remove(&terminal);
            }
        }
        if self.watched.contains_key(&pid) {
            return false;
        }
        self.watched.insert(pid, terminal);
        true
    }

    /// Forgets an instance that exited before it could be watched.
    pub fn unwatch(&mut self, pid: i32) {
        self.watched.remove(&pid);
    }

    pub fn exited(&mut self, pid: i32, status: ExitStatus) -> Option<(TerminalKind, Exit)> {
        let terminal = self.watched.remove(&pid)?;
        if !terminal.restarts_after(status) {
            return Some((terminal, Exit::Ended));
        }
        if self.started.get(&terminal) == Some(&Some(pid)) {
            self.started.remove(&terminal);
            return Some((terminal, Exit::CrashedAfterRestart));
        }
        self.crashed.insert(terminal);
        Some((terminal, Exit::Crashed))
    }

    pub fn crashed(&self) -> Vec<TerminalKind> {
        self.crashed.iter().copied().collect()
    }

    pub fn restarting(&mut self, terminal: TerminalKind) {
        self.crashed.remove(&terminal);
        self.started.insert(terminal, None);
    }

    pub fn nothing_to_restore(&mut self, terminal: TerminalKind) {
        self.crashed.remove(&terminal);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::ExitStatusExt;

    fn exited_with(code: i32) -> ExitStatus {
        ExitStatus::from_raw(code << 8)
    }

    #[test]
    fn only_a_crash_by_the_terminals_own_account_asks_for_a_restart() {
        let mut restarts = CrashRestarts::default();
        for (pid, status) in [
            (1, exited_with(0)),
            (2, ExitStatus::from_raw(libc::SIGKILL)),
        ] {
            assert!(restarts.running(TerminalKind::Rex, pid));
            assert_eq!(
                restarts.exited(pid, status),
                Some((TerminalKind::Rex, Exit::Ended))
            );
        }
        assert_eq!(restarts.crashed(), []);
        assert!(restarts.running(TerminalKind::Ghostty, 3));
        assert_eq!(
            restarts.exited(3, exited_with(2)),
            Some((TerminalKind::Ghostty, Exit::Ended))
        );
        assert!(restarts.running(TerminalKind::Rex, 4));
        assert!(!restarts.running(TerminalKind::Rex, 4));
        assert_eq!(
            restarts.exited(4, exited_with(2)),
            Some((TerminalKind::Rex, Exit::Crashed))
        );
        assert_eq!(restarts.crashed(), [TerminalKind::Rex]);
        assert_eq!(restarts.exited(4, exited_with(2)), None);
    }

    #[test]
    fn a_terminal_the_daemon_restarted_is_left_alone_when_it_crashes_again() {
        let mut restarts = CrashRestarts::default();
        restarts.running(TerminalKind::Rex, 1);
        restarts.exited(1, exited_with(2));
        restarts.restarting(TerminalKind::Rex);
        assert_eq!(restarts.crashed(), []);
        restarts.running(TerminalKind::Rex, 2);
        assert_eq!(
            restarts.exited(2, exited_with(2)),
            Some((TerminalKind::Rex, Exit::CrashedAfterRestart))
        );
        assert_eq!(restarts.crashed(), []);

        // Started by anyone else, it is the user's again, and so is its next crash's restart.
        restarts.running(TerminalKind::Rex, 3);
        assert_eq!(
            restarts.exited(3, exited_with(2)),
            Some((TerminalKind::Rex, Exit::Crashed))
        );
    }

    #[test]
    fn a_terminal_started_again_meanwhile_needs_no_restart() {
        let mut restarts = CrashRestarts::default();
        restarts.running(TerminalKind::Rex, 1);
        restarts.exited(1, exited_with(2));
        restarts.running(TerminalKind::Rex, 2);
        assert_eq!(restarts.crashed(), []);
    }
}
