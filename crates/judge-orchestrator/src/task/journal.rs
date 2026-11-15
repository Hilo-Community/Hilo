//! The append-only journal of one task: framed, checksummed records, each flushed to disk
//! before it counts.
//!
//! Frame: `CLTJ` magic, payload length (u32, little-endian), BLAKE3 of the payload, payload
//! (`bincode`). A last frame cut short or failing its checksum is a write the process did not
//! finish (it was never acknowledged): it is dropped and the file truncated. A bad frame followed
//! by more data is corruption and is reported, never skipped.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use serde::de::DeserializeOwned;
use serde::Serialize;

use super::RepositoryError;

const MAGIC: [u8; 4] = *b"CLTJ";
const HEADER_LEN: usize = 4 + 4 + 32;
/// A single record is small (a task spec at most); anything larger is corruption.
const MAX_PAYLOAD: u32 = 16 * 1024 * 1024;

pub(super) struct Journal {
    path: PathBuf,
    file: File,
}

fn io_error(path: &Path) -> impl FnOnce(io::Error) -> RepositoryError + '_ {
    move |source| RepositoryError::Io {
        path: path.to_path_buf(),
        source,
    }
}

impl Journal {
    /// Creates a new, empty journal. Fails if one already exists.
    pub(super) fn create(path: &Path) -> Result<Self, RepositoryError> {
        let file = OpenOptions::new()
            .read(true)
            .append(true)
            .create_new(true)
            .open(path)
            .map_err(io_error(path))?;
        // The new file name must survive a crash too.
        sync_parent(path)?;
        Ok(Self {
            path: path.to_path_buf(),
            file,
        })
    }

    /// Opens an existing journal and reads every complete record.
    pub(super) fn open<R: DeserializeOwned>(
        path: &Path,
    ) -> Result<(Self, Vec<R>), RepositoryError> {
        let mut file = OpenOptions::new()
            .read(true)
            .append(true)
            .open(path)
            .map_err(io_error(path))?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).map_err(io_error(path))?;
        let (records, valid_len) = decode_all(path, &bytes)?;
        if valid_len < bytes.len() {
            // An unfinished last write: it was never acknowledged, so dropping it loses nothing.
            file.set_len(valid_len as u64).map_err(io_error(path))?;
            file.sync_all().map_err(io_error(path))?;
            file.seek(SeekFrom::End(0)).map_err(io_error(path))?;
        }
        Ok((
            Self {
                path: path.to_path_buf(),
                file,
            },
            records,
        ))
    }

    /// Appends one record and flushes it to disk. Only then may the caller act on it.
    pub(super) fn append<R: Serialize>(&mut self, record: &R) -> Result<(), RepositoryError> {
        let payload = bincode::serialize(record)?;
        let len = u32::try_from(payload.len())
            .ok()
            .filter(|len| *len <= MAX_PAYLOAD)
            .ok_or_else(|| RepositoryError::RecordTooLarge {
                path: self.path.clone(),
                bytes: payload.len(),
            })?;
        let mut frame = Vec::with_capacity(HEADER_LEN + payload.len());
        frame.extend_from_slice(&MAGIC);
        frame.extend_from_slice(&len.to_le_bytes());
        frame.extend_from_slice(blake3::hash(&payload).as_bytes());
        frame.extend_from_slice(&payload);
        self.file.write_all(&frame).map_err(io_error(&self.path))?;
        self.file.sync_data().map_err(io_error(&self.path))
    }
}

/// Records decoded from `bytes`, and how many bytes they span.
fn decode_all<R: DeserializeOwned>(
    path: &Path,
    bytes: &[u8],
) -> Result<(Vec<R>, usize), RepositoryError> {
    let corrupt = |offset: usize, reason: &str| RepositoryError::Corrupt {
        path: path.to_path_buf(),
        offset: offset as u64,
        reason: reason.to_string(),
    };
    let mut records = Vec::new();
    let mut offset = 0;
    while offset < bytes.len() {
        let rest = &bytes[offset..];
        // A frame that does not fit is the last write, cut short.
        if rest.len() < HEADER_LEN {
            break;
        }
        if rest[..4] != MAGIC {
            return Err(corrupt(offset, "bad frame magic"));
        }
        let len = u32::from_le_bytes([rest[4], rest[5], rest[6], rest[7]]);
        if len > MAX_PAYLOAD {
            return Err(corrupt(offset, "frame length out of range"));
        }
        let end = HEADER_LEN + len as usize;
        if rest.len() < end {
            break;
        }
        let payload = &rest[HEADER_LEN..end];
        if blake3::hash(payload).as_bytes() != &rest[8..HEADER_LEN] {
            if offset + end == bytes.len() {
                // The last frame, written partly: unfinished, not corrupt.
                break;
            }
            return Err(corrupt(offset, "checksum mismatch"));
        }
        let record = bincode::deserialize(payload)
            .map_err(|_| corrupt(offset, "a record cannot be decoded"))?;
        records.push(record);
        offset += end;
    }
    Ok((records, offset))
}

#[cfg(unix)]
fn sync_parent(path: &Path) -> Result<(), RepositoryError> {
    if let Some(parent) = path.parent() {
        File::open(parent)
            .and_then(|dir| dir.sync_all())
            .map_err(io_error(parent))?;
    }
    Ok(())
}

/// Windows has no directory handle to flush; NTFS journals the metadata itself.
#[cfg(not(unix))]
#[allow(clippy::unnecessary_wraps)]
fn sync_parent(_path: &Path) -> Result<(), RepositoryError> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(dir: &tempfile::TempDir) -> PathBuf {
        dir.path().join("task.journal")
    }

    #[test]
    fn records_survive_a_reopen_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let mut journal = Journal::create(&path(&dir)).unwrap();
        for value in [1u32, 2, 3] {
            journal.append(&value).unwrap();
        }
        drop(journal);
        let (_, records) = Journal::open::<u32>(&path(&dir)).unwrap();
        assert_eq!(records, vec![1, 2, 3]);
    }

    #[test]
    fn an_unfinished_last_write_is_dropped_and_the_file_repaired() {
        let dir = tempfile::tempdir().unwrap();
        let mut journal = Journal::create(&path(&dir)).unwrap();
        journal.append(&7u64).unwrap();
        journal.append(&8u64).unwrap();
        drop(journal);
        let full = std::fs::read(path(&dir)).unwrap();
        for cut in 1..(HEADER_LEN + 8) {
            std::fs::write(path(&dir), &full[..full.len() - cut]).unwrap();
            let (mut journal, records) = Journal::open::<u64>(&path(&dir)).unwrap();
            assert_eq!(records, vec![7], "cut {cut}");
            // Appending after the repair produces a valid journal again.
            journal.append(&9u64).unwrap();
            drop(journal);
            let (_, records) = Journal::open::<u64>(&path(&dir)).unwrap();
            assert_eq!(records, vec![7, 9], "cut {cut}");
        }
    }

    #[test]
    fn corruption_in_the_middle_is_an_error_not_a_silent_skip() {
        let dir = tempfile::tempdir().unwrap();
        let mut journal = Journal::create(&path(&dir)).unwrap();
        journal.append(&1u64).unwrap();
        journal.append(&2u64).unwrap();
        drop(journal);
        let mut bytes = std::fs::read(path(&dir)).unwrap();
        // Flip a bit in the first record's payload.
        bytes[HEADER_LEN] ^= 1;
        std::fs::write(path(&dir), &bytes).unwrap();
        assert!(matches!(
            Journal::open::<u64>(&path(&dir)),
            Err(RepositoryError::Corrupt { offset: 0, .. })
        ));
    }

    #[test]
    fn a_journal_is_never_created_twice() {
        let dir = tempfile::tempdir().unwrap();
        Journal::create(&path(&dir)).unwrap();
        assert!(Journal::create(&path(&dir)).is_err());
    }
}
