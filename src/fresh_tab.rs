use crate::paths;
use crate::process::{self, ProcessIdentityStatus};
use anyhow::{Context, Result};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::fs::{self, OpenOptions};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

/// A fresh Ghostty tab offering itself to the daemon's next restore pass.
/// The daemon alone decides what to restore; the tab only runs what the pass
/// would otherwise have opened in a new tab.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Offer {
    pub pid: i32,
    pub started: String,
    pub answer: Option<Answer>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Answer {
    Launch { directory: PathBuf, command: String },
    Decline,
}

fn offer_file() -> Result<PathBuf> {
    Ok(paths::config_dir()?.join("fresh-tab.json"))
}

fn with_offer<T>(path: &Path, update: impl FnOnce(&mut Option<Offer>) -> T) -> Result<T> {
    let directory = path.parent().context("offer file has no directory")?;
    fs::create_dir_all(directory)?;
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .open(directory.join("fresh-tab.lock"))
        .context("failed to open fresh tab lock")?;
    lock.lock_exclusive()?;
    let mut offer = match fs::read(path) {
        Ok(contents) => serde_json::from_slice(&contents).ok(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    let result = update(&mut offer);
    match &offer {
        Some(offer) => fs::write(path, serde_json::to_vec(offer)?)?,
        None => match fs::remove_file(path) {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                return Err(error.into());
            }
            _ => {}
        },
    }
    Ok(result)
}

pub fn offer(pid: i32, started: &str) -> Result<()> {
    offer_at(&offer_file()?, pid, started)
}

fn offer_at(path: &Path, pid: i32, started: &str) -> Result<()> {
    with_offer(path, |offer| {
        *offer = Some(Offer {
            pid,
            started: started.to_string(),
            answer: None,
        });
    })
}

/// The daemon's answer to this tab's offer, consuming it. With `withdraw`, an
/// unanswered offer is taken back so no later pass can fill it.
pub fn take_answer(pid: i32, withdraw: bool) -> Result<Option<Answer>> {
    take_answer_at(&offer_file()?, pid, withdraw)
}

fn take_answer_at(path: &Path, pid: i32, withdraw: bool) -> Result<Option<Answer>> {
    with_offer(path, |offer| {
        let current = offer.as_ref().filter(|current| current.pid == pid)?;
        let answer = current.answer.clone();
        if answer.is_some() || withdraw {
            *offer = None;
        }
        answer
    })
}

pub fn waiting() -> Result<bool> {
    waiting_at(&offer_file()?)
}

fn waiting_at(path: &Path) -> Result<bool> {
    if !path.exists() {
        return Ok(false);
    }
    with_offer(path, |offer| {
        offer
            .as_ref()
            .is_some_and(|offer| offer.answer.is_none() && offer_is_live(offer))
    })
}

/// Hands the waiting tab this launch, when one is waiting.
pub fn fill(directory: &Path, command: &str) -> Result<bool> {
    answer_at(
        &offer_file()?,
        Answer::Launch {
            directory: directory.to_path_buf(),
            command: command.to_string(),
        },
    )
}

pub fn decline() -> Result<bool> {
    answer_at(&offer_file()?, Answer::Decline)
}

fn answer_at(path: &Path, answer: Answer) -> Result<bool> {
    if !path.exists() {
        return Ok(false);
    }
    with_offer(path, |offer| match offer {
        Some(waiting) if waiting.answer.is_none() && offer_is_live(waiting) => {
            waiting.answer = Some(answer);
            true
        }
        _ => false,
    })
}

fn offer_is_live(offer: &Offer) -> bool {
    process::process_identity_status(offer.pid, &offer.started) == ProcessIdentityStatus::Alive
}

#[cfg(test)]
mod tests {
    use super::*;

    fn own_offer(path: &Path) -> i32 {
        let pid = std::process::id() as i32;
        offer_at(path, pid, &process::process_start_identity(pid).unwrap()).unwrap();
        pid
    }

    fn launch() -> Answer {
        Answer::Launch {
            directory: PathBuf::from("/tmp/project"),
            command: "session-guard launch --session-id 'abc'".to_string(),
        }
    }

    #[test]
    fn a_pass_fills_a_waiting_tab_once_and_the_tab_consumes_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fresh-tab.json");
        assert!(!waiting_at(&path).unwrap());
        let pid = own_offer(&path);
        assert!(waiting_at(&path).unwrap());
        assert_eq!(take_answer_at(&path, pid, false).unwrap(), None);
        assert!(answer_at(&path, launch()).unwrap());
        assert!(!waiting_at(&path).unwrap());
        assert!(!answer_at(&path, Answer::Decline).unwrap());
        assert_eq!(take_answer_at(&path, pid, false).unwrap(), Some(launch()));
        assert!(!path.exists());
    }

    #[test]
    fn a_withdrawn_offer_cannot_be_filled() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fresh-tab.json");
        let pid = own_offer(&path);
        assert_eq!(take_answer_at(&path, pid, true).unwrap(), None);
        assert!(!answer_at(&path, launch()).unwrap());
    }

    #[test]
    fn an_offer_from_a_dead_tab_is_never_filled() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fresh-tab.json");
        offer_at(&path, i32::MAX, "Wed Jan 1 00:00:00 2020").unwrap();
        assert!(!waiting_at(&path).unwrap());
        assert!(!answer_at(&path, launch()).unwrap());
    }
}
