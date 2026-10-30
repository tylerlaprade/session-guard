//! Whether the screen is locked. A window that enters native fullscreen while
//! it is locked drops straight back out and leaves its tab group inconsistent,
//! and tab changes there end in `AppKit` asserting "expected no items" once
//! someone unlocks and picks the window (2026-10-28: Ghostty aborted that way
//! after scripts opened windows and tabs behind the lock screen). loginwindow
//! keeps the lock state in a notification's shared state and posts a second
//! notification on unlock.

use anyhow::Result;
use std::ffi::{CString, c_char, c_int};
use std::os::fd::RawFd;

const NOTICES: &str = "com.apple.sessionagent";
// The tests stand in for loginwindow's notifications under other names.
const NOTICES_VARIABLE: &str = "SESSION_GUARD_SCREEN_NOTICES";
const NOTIFY_STATUS_OK: u32 = 0;

unsafe extern "C" {
    fn notify_register_check(name: *const c_char, out_token: *mut c_int) -> u32;
    fn notify_register_file_descriptor(
        name: *const c_char,
        notify_fd: *mut c_int,
        flags: c_int,
        out_token: *mut c_int,
    ) -> u32;
    fn notify_get_state(token: c_int, state: *mut u64) -> u32;
    fn notify_cancel(token: c_int) -> u32;
}

fn notice(name: &str) -> Result<CString> {
    let prefix = std::env::var(NOTICES_VARIABLE).unwrap_or_else(|_| NOTICES.to_string());
    Ok(CString::new(format!("{prefix}.{name}"))?)
}

fn check(status: u32, what: &str) -> Result<()> {
    if status == NOTIFY_STATUS_OK {
        Ok(())
    } else {
        anyhow::bail!("{what} failed with notify status {status}")
    }
}

pub fn is_locked() -> Result<bool> {
    let name = notice("screenIsLocked")?;
    let mut token = 0;
    check(
        unsafe { notify_register_check(name.as_ptr(), &raw mut token) },
        "registering for the screen lock state",
    )?;
    let mut state = 0;
    let read = check(
        unsafe { notify_get_state(token, &raw mut state) },
        "reading the screen lock state",
    );
    unsafe {
        notify_cancel(token);
    }
    read.map(|()| state != 0)
}

/// Each unlock from the moment this is made. Watch before reading the lock
/// state, so an unlock between the two still counts.
pub struct Unlocks {
    fd: RawFd,
    token: c_int,
}

impl Unlocks {
    pub fn watch() -> Result<Self> {
        let name = notice("screenIsUnlocked")?;
        let (mut fd, mut token) = (0, 0);
        check(
            unsafe {
                notify_register_file_descriptor(name.as_ptr(), &raw mut fd, 0, &raw mut token)
            },
            "registering for screen unlocks",
        )?;
        Ok(Self { fd, token })
    }

    /// Blocks until the next unlock.
    pub fn wait(&self) -> Result<()> {
        self.posted(-1).map(drop)
    }

    /// Whether the screen unlocked since the last look, without waiting.
    pub fn take(&self) -> Result<bool> {
        self.posted(0)
    }

    fn posted(&self, timeout_ms: c_int) -> Result<bool> {
        let mut any = false;
        let mut timeout_ms = timeout_ms;
        loop {
            let mut ready = libc::pollfd {
                fd: self.fd,
                events: libc::POLLIN,
                revents: 0,
            };
            match unsafe { libc::poll(&raw mut ready, 1, timeout_ms) } {
                -1 => {
                    let error = std::io::Error::last_os_error();
                    if error.kind() == std::io::ErrorKind::Interrupted {
                        continue;
                    }
                    return Err(error.into());
                }
                0 => return Ok(any),
                _ => {}
            }
            // Each post writes the registration's token, four bytes.
            let mut token: c_int = 0;
            let read = unsafe {
                libc::read(
                    self.fd,
                    (&raw mut token).cast(),
                    std::mem::size_of::<c_int>(),
                )
            };
            if read < 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            any = true;
            timeout_ms = 0;
        }
    }
}

impl Drop for Unlocks {
    // Cancelling the last registration on a descriptor closes it.
    fn drop(&mut self) {
        unsafe {
            notify_cancel(self.token);
        }
    }
}
