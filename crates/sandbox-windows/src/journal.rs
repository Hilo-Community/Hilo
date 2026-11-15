//! A record of what a run changed outside its process, so that it can be undone even if the
//! Host dies before undoing it.
//!
//! Before a run creates its AppContainer profile and grants it access to directories, it writes
//! one file in the journal directory naming the profile and those directories, and keeps that
//! file open without sharing. Once the run has cleaned up, the file is deleted. If the Host dies
//! first, the kernel closes the file and it stays behind: the next start of the sandbox
//! ([`recover_stale_runs`]) undoes what it names. A file that cannot be opened is still held by
//! a live Host and is left alone, so two Hosts never undo each other's runs.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::windows::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use crate::acl::revoke_access;
use crate::container::{delete_profile, is_valid_name, Sid};

const EXTENSION: &str = "run";

/// The journal file of one live run.
#[derive(Debug)]
pub(crate) struct JournalEntry {
    path: PathBuf,
    /// Held open, unshared, while the run lives: this is what marks it as live.
    file: File,
}

impl JournalEntry {
    /// Records that the profile `profile` will be created and given access to `dirs`.
    pub(crate) fn create(journal_dir: &Path, profile: &str, dirs: &[PathBuf]) -> io::Result<Self> {
        let mut text = format!("{profile}\n");
        for dir in dirs {
            let dir = dir.to_str().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "a writable directory is not valid Unicode",
                )
            })?;
            text.push_str(dir);
            text.push('\n');
        }
        fs::create_dir_all(journal_dir)?;
        let path = journal_dir.join(format!("{profile}.{EXTENSION}"));
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .share_mode(0)
            .open(&path)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        Ok(Self { path, file })
    }

    /// Everything the run changed has been undone: the record is no longer needed. Without this
    /// call the file stays, and the next recovery undoes the run again.
    pub(crate) fn complete(self) {
        drop(self.file);
        let _ = fs::remove_file(&self.path);
    }
}

/// Undoes the runs whose Host died before cleaning up: removes their access from the directories
/// they were granted and deletes their AppContainer profile. Returns how many were undone.
/// Records held by a live Host are skipped; a record that cannot be read or undone completely
/// is kept for the next attempt, so that one bad record never blocks the sandbox. Only failing
/// to list the journal directory is an error.
pub fn recover_stale_runs(journal_dir: &Path) -> io::Result<usize> {
    let entries = match fs::read_dir(journal_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error),
    };
    let mut recovered = 0;
    for entry in entries {
        let path = entry?.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some(EXTENSION) {
            continue;
        }
        let file = OpenOptions::new().read(true).share_mode(0).open(&path);
        // A sharing violation means a live Host holds the record; any other failure leaves it
        // for the next attempt.
        let Ok(mut file) = file else {
            continue;
        };
        let mut text = String::new();
        if file.read_to_string(&mut text).is_err() {
            continue;
        }
        let mut lines = text.lines();
        let profile = lines.next().unwrap_or_default();
        // A name this crate would not have written is left to a human.
        if !is_valid_name(profile) {
            continue;
        }
        let Ok(sid) = Sid::derive(profile) else {
            continue;
        };
        let mut undone = true;
        for dir in lines.filter(|line| !line.is_empty()) {
            undone &= revoke_access(Path::new(dir), &sid).is_ok();
        }
        undone &= delete_profile(profile).is_ok();
        drop(file);
        if undone && fs::remove_file(&path).is_ok() {
            recovered += 1;
        }
    }
    Ok(recovered)
}
