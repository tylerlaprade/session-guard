use crate::adapters::ghostty;
use crate::commands::{daemon, launch, restore};
use crate::paths;
use crate::process::{self, ProcessSnapshot};
use crate::sessions::{self, SessionRecord};
use anyhow::Result;
use fs2::FileExt;
use std::ffi::CStr;
use std::io::IsTerminal;

pub fn run() -> Result<()> {
    if std::env::var("TERM_PROGRAM").as_deref() != Ok("ghostty")
        || !std::io::stdin().is_terminal()
        || !paths::launch_agent_plist()?.is_file()
    {
        return Ok(());
    }
    let restore_lock = restore::lock()?;
    match restore_lock.try_lock_exclusive() {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => return Ok(()),
        Err(error) => return Err(error.into()),
    }
    let path = paths::sessions_file()?;
    let processes = ProcessSnapshot::capture()?;
    if !sessions::read_sessions(&path)?
        .iter()
        .any(|session| eligible(session, &processes))
    {
        return Ok(());
    }
    let mut tty = [0_i8; 1024];
    let result = unsafe { libc::ttyname_r(0, tty.as_mut_ptr(), tty.len()) };
    if result != 0 {
        return Err(std::io::Error::from_raw_os_error(result).into());
    }
    let tty = unsafe { CStr::from_ptr(tty.as_ptr()) }.to_str()?;
    if !ghostty::is_only_terminal(tty)? {
        return Ok(());
    }
    let owner = i32::try_from(std::process::id())?;
    let started = process::process_start_identity(owner)?;
    let record = sessions::with_sessions_mut(&path, |sessions| {
        let processes = ProcessSnapshot::capture()?;
        let Some(session) = sessions
            .iter_mut()
            .find(|session| eligible(session, &processes))
        else {
            return Ok(None);
        };
        if unsafe { libc::tcgetpgrp(0) } != unsafe { libc::getpgrp() } || input_waiting(0)? {
            return Ok(None);
        }
        launch::claim(session, owner, &started);
        Ok(Some(session.clone()))
    })?;
    drop(restore_lock);
    if let Some(record) = record {
        daemon::log_line(&format!(
            "reusing fresh terminal {tty}: {} {}",
            record.tool, record.session_id
        ))?;
        launch::run_claimed(&record)?;
    }
    Ok(())
}

fn eligible(session: &SessionRecord, processes: &ProcessSnapshot) -> bool {
    daemon::needs_terminal_restore(session, processes)
        && session
            .shell_pid_started_at
            .as_deref()
            .and_then(process::parse_identity)
            .is_some()
        && daemon::ending_verdict(session, processes, Some("ghostty"))
            == daemon::EndingVerdict::Recoverable
        && session.directory.is_dir()
        && !crate::scan::is_unused_spare(session.tool, &session.session_id)
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
