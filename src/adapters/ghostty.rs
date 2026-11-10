use super::{Focus, Selection, TerminalAdapter, run_checked};
use crate::{TerminalKind, process};
use anyhow::{Context, Result};
use std::cell::RefCell;
use std::path::Path;
use std::process::Command;
use std::thread;
use std::time::Duration;

// A wedged Ghostty can take the whole window this long.
const OPEN_TAB_TIMEOUT: Duration = Duration::from_secs(15);
const FOCUS_TIMEOUT: Duration = Duration::from_secs(2);

/// One restore pass's tabs stay together. The first goes into the front
/// window, or a new one when none is open, and the rest follow it by the
/// window's id: a window that comes to the front mid-restore, such as one a
/// restored session opens for itself, must not collect the remaining tabs.
#[derive(Default)]
pub struct Ghostty {
    restore_window: RefCell<Option<String>>,
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
fn restore_tab_command(
    directory: &str,
    input: &str,
    window: &str,
    selection: Selection,
) -> Command {
    let selection = match selection {
        Selection::Take => "take",
        Selection::Keep => "keep",
    };
    let mut osa = Command::new("osascript");
    osa.args([
        "-e",
        include_str!("ghostty_restore_tab.applescript"),
        directory,
        input,
        window,
        selection,
    ]);
    osa
}

impl TerminalAdapter for Ghostty {
    fn open_tab(&self, directory: &Path, command: &str, selection: Selection) -> Result<()> {
        if crate::fresh_tab::fill(directory, command)? {
            return Ok(());
        }
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
            selection,
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

    // Ghostty handles scripted commands one at a time, in the order they
    // arrive, so once a later one answers, any tab asked for before it exists.
    fn caught_up(&self) -> bool {
        if !self.is_running() {
            return true;
        }
        let mut osa = Command::new("osascript");
        osa.args(["-e", r#"tell application "Ghostty" to count windows"#]);
        super::run_capture_timeout(&mut osa, "asking Ghostty to catch up", OPEN_TAB_TIMEOUT).is_ok()
    }

    fn launch(&self) -> Result<()> {
        let mut command = Command::new("open");
        command.args(["-g", "-a", "Ghostty", "--args", "--initial-window=false"]);
        run_checked(command, "launching Ghostty")
    }

    // Ghostty's scripting activates it whenever it adds a tab, with no option
    // to skip that.
    fn focus(&self) -> Option<Focus> {
        let mut osa = Command::new("osascript");
        osa.args(["-e", include_str!("ghostty_front_window.applescript")]);
        let window =
            super::run_capture_timeout(&mut osa, "reading Ghostty's front window", FOCUS_TIMEOUT)
                .ok()
                .filter(|window| !window.is_empty());
        Some(Focus {
            app: foreground_app(),
            window,
        })
    }

    // Someone who moved to another app while the tabs opened stays there.
    fn return_focus(&self, focus: &Focus) -> Result<()> {
        if foreground_app().as_deref() != Some(TerminalKind::Ghostty.bundle_id()) {
            return Ok(());
        }
        super::run_capture_timeout(
            &mut return_focus_command(focus),
            "giving the focus back after a restore",
            FOCUS_TIMEOUT,
        )
        .map(drop)
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

    // One `text:` action writes Ctrl-U, the command, and Return to the tab in
    // order. Separate key events queue behind a paste, so a Ctrl-U sent ahead
    // of `input text` erases the command it was meant to make room for.
    fn run_in_tab(&self, tty: &str, command: &str) -> Result<bool> {
        let mut osa = Command::new("osascript");
        osa.args([
            "-e",
            include_str!("ghostty_run_in_tab.applescript"),
            tty,
            &command.replace('\\', "\\\\"),
        ]);
        let reply = super::run_capture_timeout(
            &mut osa,
            "running a command in a Ghostty tab",
            Duration::from_secs(2),
        )?;
        Ok(reply.trim() == "ran")
    }
}

/// The app in front, when it is an ordinary one. A system alert can hold the
/// front as a `UIElement` app, and asking to activate one stalled a script for
/// 119 seconds (dotfiles' ghostty-tab).
fn foreground_app() -> Option<String> {
    let front = lsappinfo(&["front"])?;
    if lsappinfo_value(&lsappinfo(&["info", "-only", "ApplicationType", &front])?).as_deref()
        != Some("Foreground")
    {
        return None;
    }
    lsappinfo_value(&lsappinfo(&["info", "-only", "bundleID", &front])?)
}

fn lsappinfo(arguments: &[&str]) -> Option<String> {
    let mut command = Command::new("lsappinfo");
    command.args(arguments);
    super::run_capture_timeout(&mut command, "asking which app is in front", FOCUS_TIMEOUT).ok()
}

/// The value of a `"key"="value"` line from `lsappinfo info -only key`.
fn lsappinfo_value(reply: &str) -> Option<String> {
    let (_, value) = reply.trim().split_once('=')?;
    let value = value.trim_matches('"');
    (!value.is_empty() && value != "[ NULL ]").then(|| value.to_string())
}

fn return_focus_command(focus: &Focus) -> Command {
    let mut osa = Command::new("osascript");
    osa.args([
        "-e",
        include_str!("ghostty_return_focus.applescript"),
        focus.window.as_deref().unwrap_or_default(),
        focus.app.as_deref().unwrap_or_default(),
    ]);
    osa
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
    fn lsappinfo_replies_are_read_for_their_value() {
        assert_eq!(
            lsappinfo_value("\"ApplicationType\"=\"Foreground\"\n").as_deref(),
            Some("Foreground")
        );
        assert_eq!(
            lsappinfo_value("\"CFBundleIdentifier\"=\"com.example.train\"").as_deref(),
            Some("com.example.train")
        );
        assert_eq!(lsappinfo_value("\"bundleID\"=[ NULL ]"), None);
        assert_eq!(lsappinfo_value(""), None);
    }

    /// The window comes back before the app, as in dotfiles' ghostty-tab:
    /// activating the app last leaves it, not Ghostty, in front.
    #[test]
    fn focus_returns_to_the_window_and_then_the_app() {
        let arguments = |focus: &Focus| -> Vec<String> {
            return_focus_command(focus)
                .get_args()
                .skip(2)
                .map(|argument| argument.to_string_lossy().into_owned())
                .collect()
        };
        assert_eq!(
            arguments(&Focus {
                app: Some("com.example.train".to_string()),
                window: Some("tab-group-1".to_string()),
            }),
            ["tab-group-1", "com.example.train"]
        );
        assert_eq!(
            arguments(&Focus {
                app: None,
                window: None,
            }),
            ["", ""]
        );
        let script = include_str!("ghostty_return_focus.applescript");
        assert!(
            script.find("activate window").unwrap()
                < script
                    .find("tell application id frontAppId to activate")
                    .unwrap()
        );
    }

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
        let window_argument = |command: &Command| command.get_args().nth(4).unwrap().to_owned();
        let first = restore_tab_command("/tmp", "launch\n", "", Selection::Take);
        assert_eq!(window_argument(&first), "");
        ghostty.remember_window("tab-group-1\ttab-7").unwrap();
        let remembered = ghostty.restore_window.borrow().clone().unwrap();
        let second = restore_tab_command("/tmp", "launch\n", &remembered, Selection::Take);
        assert_eq!(window_argument(&second), "tab-group-1");
        assert!(ghostty.remember_window("tab-7").is_err());
        assert!(ghostty.remember_window("\ttab-7").is_err());
    }

    #[test]
    fn the_tab_script_is_told_whether_to_keep_the_selection() {
        let selection = |selection| {
            restore_tab_command("/tmp", "launch\n", "", selection)
                .get_args()
                .last()
                .unwrap()
                .to_owned()
        };
        assert_eq!(selection(Selection::Take), "take");
        assert_eq!(selection(Selection::Keep), "keep");
    }
}
