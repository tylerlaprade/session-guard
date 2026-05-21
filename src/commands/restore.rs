use crate::commands::daemon;
use anyhow::{Context, Result};

pub fn run() -> Result<()> {
    if let Some(pid) = daemon::running_daemon_pid()? {
        let result = unsafe { libc::kill(pid, libc::SIGUSR1) };
        if result != 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("failed to signal daemon pid {pid}"));
        }
        println!("restore requested from daemon pid {pid}");
        return Ok(());
    }

    let summary = daemon::restore_once(daemon::RestoreMode::Manual)?;
    println!("{}", summary.message());
    for error in &summary.errors {
        eprintln!("{error}");
    }
    Ok(())
}
