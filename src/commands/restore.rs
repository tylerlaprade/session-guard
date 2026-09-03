use crate::commands::daemon::{self, RestoreMode};
use anyhow::{Context, Result};

pub fn run(all: bool) -> Result<()> {
    let (mode, signal) = if all {
        (RestoreMode::All, libc::SIGUSR2)
    } else {
        (RestoreMode::Manual, libc::SIGUSR1)
    };

    if let Some(pid) = daemon::running_daemon_pid()? {
        let result = unsafe { libc::kill(pid, signal) };
        if result != 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("failed to signal daemon pid {pid}"));
        }
        println!("restore requested from daemon pid {pid}");
        return Ok(());
    }

    let summary = daemon::restore_once(mode)?;
    println!("{}", summary.message());
    for error in &summary.errors {
        eprintln!("{error}");
    }
    Ok(())
}
