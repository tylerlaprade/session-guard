use super::{TerminalAdapter, run_checked};
use crate::process;
use anyhow::{Context, Result};
use std::cell::RefCell;
use std::path::Path;
use std::process::Command;
use std::thread;
use std::time::Duration;

// A wedged Ghostty can take the whole window this long.
const OPEN_TAB_TIMEOUT: Duration = Duration::from_secs(15);

/// One restore pass's tabs stay together. The first goes into the front
/// window, or a new one when none is open, and the rest follow it by the
/// window's id: a window that comes to the front mid-restore, such as one a
/// restored session opens for itself, must not collect the remaining tabs.
#[derive(Default)]
pub struct Ghostty {
    restore_window: RefCell<Option<String>>,
}

pub(crate) fn is_only_terminal(tty: &str) -> Result<bool> {
    let mut command = Command::new("osascript");
    command.args([
        "-l",
        "JavaScript",
        "-e",
        include_str!("ghostty_first_terminal.js"),
        tty,
    ]);
    super::run_capture_timeout(
        &mut command,
        "checking first Ghostty terminal",
        Duration::from_secs(2),
    )
    .map(|result| result == "true")
}

impl Ghostty {
    /// Keep the window a restore script reported for the pass's next tab.
    fn remember_window(&self, reply: &str) -> Result<()> {
        let (window_id, _) = reply
            .split_once('\t')
            .filter(|(window_id, tab_id)| !window_id.is_empty() && !tab_id.is_empty())
            .context("Ghostty returned no window and tab id")?;
        *self.restore_window.borrow_mut() = Some(window_id.to_string());
        Ok(())
    }
}

/// Opens one restored tab: in `window` while it is open, otherwise in the
/// front window, or a fresh one when there is none. At login-time restore
/// Ghostty is often running with no window yet, and `new tab in front window`
/// fails with -1728 ("Can't get front window") in that state. It answers with
/// the window's id and the tab's, tab-separated.
fn restore_tab_command(directory: &str, input: &str, window: &str) -> Command {
    let mut osa = Command::new("osascript");
    osa.args([
        "-e",
        include_str!("ghostty_restore_tab.applescript"),
        directory,
        input,
        window,
    ]);
    osa
}

impl TerminalAdapter for Ghostty {
    fn open_tab(&self, directory: &Path, command: &str) -> Result<()> {
        if !self.is_running() {
            self.launch()?;
            thread::sleep(Duration::from_millis(700));
        }

        let dir_str = directory
            .to_str()
            .context("Ghostty restore requires a UTF-8 directory path")?;
        let mut osa = restore_tab_command(
            dir_str,
            &format!("{command}\n"),
            &self.restore_window.borrow().clone().unwrap_or_default(),
        );
        // Ghostty scripting can hang when the app is busy/recovering; do not
        // block the daemon (or its sessions lock) forever. A tab whose surface
        // never starts cannot register the restored session's owner, so the
        // restore's owner check fails it and the session stays recoverable.
        let reply = super::run_capture_timeout(&mut osa, "opening Ghostty tab", OPEN_TAB_TIMEOUT)?;
        self.remember_window(&reply)
    }

    fn is_running(&self) -> bool {
        process::app_is_running("Ghostty")
    }

    fn launch(&self) -> Result<()> {
        let mut command = Command::new("open");
        command.args(["-a", "Ghostty", "--args", "--initial-window=false"]);
        run_checked(command, "launching Ghostty")
    }

    fn tab_positions(&self) -> Result<Vec<((u32, u32), String)>> {
        let mut osa = Command::new("osascript");
        osa.args(["-e", include_str!("ghostty_tab_order.applescript")]);
        let reply = super::run_capture_timeout(
            &mut osa,
            "reading Ghostty tab order",
            Duration::from_secs(2),
        )?;
        Ok(parse_tab_positions(&reply))
    }
}

fn parse_tab_positions(reply: &str) -> Vec<((u32, u32), String)> {
    reply
        .lines()
        .filter_map(|line| {
            let mut fields = line.split('\t');
            let window = fields.next()?.parse().ok()?;
            let tab = fields.next()?.parse().ok()?;
            Some(((window, tab), fields.next()?.to_string()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tab_positions_pair_each_tty_with_its_window_and_tab() {
        assert_eq!(
            parse_tab_positions(
                "1\t1\t/dev/ttys000\n1\t2\t/dev/ttys003\n2\t1\t/dev/ttys009\nnoise"
            ),
            vec![
                ((1, 1), "/dev/ttys000".to_string()),
                ((1, 2), "/dev/ttys003".to_string()),
                ((2, 1), "/dev/ttys009".to_string()),
            ]
        );
    }

    /// A pass's first tab goes wherever Ghostty puts it; the rest go into
    /// the window that first tab landed in, by that window's id.
    #[test]
    fn a_pass_keeps_its_later_tabs_in_its_first_tab_window() {
        let ghostty = Ghostty::default();
        let window_argument = |command: &Command| command.get_args().last().unwrap().to_owned();
        let first = restore_tab_command("/tmp", "launch\n", "");
        assert_eq!(window_argument(&first), "");
        ghostty.remember_window("tab-group-1\ttab-7").unwrap();
        let remembered = ghostty.restore_window.borrow().clone().unwrap();
        let second = restore_tab_command("/tmp", "launch\n", &remembered);
        assert_eq!(window_argument(&second), "tab-group-1");
        assert!(ghostty.remember_window("tab-7").is_err());
        assert!(ghostty.remember_window("\ttab-7").is_err());
    }
}
