use crate::commands::daemon;
use crate::fresh_tab::{self, Answer};
use crate::paths;
use crate::process;
use anyhow::{Context, Result};
use std::io::IsTerminal;
use std::os::unix::process::CommandExt;
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

// The daemon answers within a second when idle; a restore pass already
// opening tabs can take longer to reach this one.
const OFFER_TIMEOUT: Duration = Duration::from_secs(15);
const ANSWER_POLL_INTERVAL: Duration = Duration::from_millis(100);

pub fn run() -> Result<()> {
    if std::env::var("TERM_PROGRAM").as_deref() != Ok("ghostty")
        || !std::io::stdin().is_terminal()
        || !untouched()?
        || !sole_terminal_of_its_ghostty()
        || !paths::launch_agent_plist()?.is_file()
        || daemon::running_daemon_pid()?.is_none()
    {
        return Ok(());
    }
    let owner = i32::try_from(std::process::id())?;
    fresh_tab::offer(owner, &process::process_start_identity(owner)?)?;
    let deadline = Instant::now() + OFFER_TIMEOUT;
    let answer = loop {
        let withdraw = Instant::now() >= deadline || !untouched()?;
        if let Some(answer) = fresh_tab::take_answer(owner, withdraw)? {
            break answer;
        }
        if withdraw {
            return Ok(());
        }
        thread::sleep(ANSWER_POLL_INTERVAL);
    };
    let Answer::Launch { directory, command } = answer else {
        return Ok(());
    };
    if !untouched()? {
        return Ok(());
    }
    Err(Command::new("/bin/sh")
        .args(["-c", &command])
        .current_dir(&directory)
        .exec())
    .with_context(|| format!("failed to run the restore in {}", directory.display()))
}

// Each Ghostty terminal, tab or split, is its own child process of Ghostty.
fn sole_terminal_of_its_ghostty() -> bool {
    let terminals: Vec<Vec<i32>> = process::pids_named("ghostty")
        .into_iter()
        .map(process::child_pids)
        .collect();
    std::iter::successors(Some(unsafe { libc::getppid() }), |&pid| {
        process::parent_pid(pid).filter(|&parent| parent > 1)
    })
    .find_map(|ancestor| {
        terminals
            .iter()
            .find(|children| children.contains(&ancestor))
    })
    .is_some_and(|children| children.len() == 1)
}

fn untouched() -> Result<bool> {
    Ok(unsafe { libc::tcgetpgrp(0) } == unsafe { libc::getpgrp() } && !input_waiting(0)?)
}

fn input_waiting(fd: i32) -> Result<bool> {
    let mut original = std::mem::MaybeUninit::<libc::termios>::uninit();
    if unsafe { libc::tcgetattr(fd, original.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let original = unsafe { original.assume_init() };
    let mut immediate = original;
    immediate.c_lflag &= !libc::ICANON;
    if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &raw const immediate) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let mut bytes = 0;
    let result = unsafe { libc::ioctl(fd, libc::FIONREAD, &raw mut bytes) };
    let error = std::io::Error::last_os_error();
    if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &raw const original) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    if result != 0 {
        return Err(error.into());
    }
    Ok(bytes != 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::io::{Read, Write};
    use std::os::fd::{AsRawFd, FromRawFd};

    #[test]
    fn unfinished_input_is_detected_without_consuming_it_or_changing_terminal_mode() {
        let (mut master, mut slave) = (-1, -1);
        assert_eq!(
            unsafe {
                libc::openpty(
                    &raw mut master,
                    &raw mut slave,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            },
            0
        );
        let mut master = unsafe { File::from_raw_fd(master) };
        let mut slave = unsafe { File::from_raw_fd(slave) };
        for descriptor in [master.as_raw_fd(), slave.as_raw_fd()] {
            assert_eq!(
                unsafe { libc::fcntl(descriptor, libc::F_SETFD, libc::FD_CLOEXEC) },
                0
            );
        }
        let fd = slave.as_raw_fd();
        let mut before = std::mem::MaybeUninit::<libc::termios>::uninit();
        assert_eq!(unsafe { libc::tcgetattr(fd, before.as_mut_ptr()) }, 0);
        let before = unsafe { before.assume_init() };
        assert_ne!(before.c_lflag & libc::ICANON, 0);
        assert!(!input_waiting(fd).unwrap());
        master.write_all(b"unfinished input").unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
        while !input_waiting(fd).unwrap() {
            assert!(std::time::Instant::now() < deadline);
            std::thread::yield_now();
        }
        let mut after = std::mem::MaybeUninit::<libc::termios>::uninit();
        assert_eq!(unsafe { libc::tcgetattr(fd, after.as_mut_ptr()) }, 0);
        let after = unsafe { after.assume_init() };
        let configured_flags = !libc::PENDIN;
        assert_eq!(
            before.c_lflag & configured_flags,
            after.c_lflag & configured_flags
        );
        assert_eq!(before.c_cc, after.c_cc);
        master.write_all(b"\n").unwrap();
        let mut input = [0; 17];
        slave.read_exact(&mut input).unwrap();
        assert_eq!(&input, b"unfinished input\n");
    }
}
