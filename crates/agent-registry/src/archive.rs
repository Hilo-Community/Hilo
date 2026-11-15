//! Unpacking a downloaded archive without trusting it. Extraction is our own loop over the
//! entries: a name outside the destination, a link, a duplicate or an oversized file stops it,
//! and nothing but plain files and folders is ever created.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};

use crate::error::RegistryError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Zip,
    TarGz,
}

/// What an archive may contain.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub max_entries: usize,
    pub max_file_bytes: u64,
    pub max_total_bytes: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_entries: 20_000,
            max_file_bytes: 1024 * 1024 * 1024,
            max_total_bytes: 2 * 1024 * 1024 * 1024,
        }
    }
}

/// The format named by the end of a URL's path.
pub fn format_of(url: &str) -> Option<Format> {
    let path = url
        .split(['?', '#'])
        .next()
        .unwrap_or(url)
        .to_ascii_lowercase();
    if path.ends_with(".zip") {
        Some(Format::Zip)
    } else if path.ends_with(".tar.gz") || path.ends_with(".tgz") {
        Some(Format::TarGz)
    } else {
        None
    }
}

/// A name inside the folder it is joined to: no root, no drive, no `..`. `./x` is `x`. Both
/// separators count, so a Windows path cannot hide a `..` on another OS.
pub fn safe_relative(name: &str) -> Option<PathBuf> {
    // A leading separator makes a name absolute: refused, not quietly made relative.
    if name.is_empty() || name.contains('\0') || name.contains(':') || name.starts_with(['/', '\\'])
    {
        return None;
    }
    let mut clean = PathBuf::new();
    for part in name.split(['/', '\\']) {
        match part {
            "" | "." => {}
            ".." => return None,
            part => clean.push(part),
        }
    }
    let ok = clean.components().next().is_some()
        && clean
            .components()
            .all(|component| matches!(component, Component::Normal(_)));
    ok.then_some(clean)
}

fn unsafe_archive(reason: impl Into<String>) -> RegistryError {
    RegistryError::UnsafeArchive(reason.into())
}

/// Creates `destination/relative` with at most `limit` bytes from `source`.
fn write_file(
    destination: &Path,
    relative: &Path,
    source: &mut dyn Read,
    mode: Option<u32>,
    limit: u64,
) -> Result<u64, RegistryError> {
    let target = destination.join(relative);
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent).map_err(RegistryError::io("Creating a folder of the agent"))?;
    }
    // `create_new`: a name repeated in the archive, or one that lands on an existing link, fails.
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&target)
        .map_err(|error| match error.kind() {
            io::ErrorKind::AlreadyExists => {
                unsafe_archive(format!("{} appears twice", relative.display()))
            }
            _ => RegistryError::Io {
                context: format!("Creating {}", relative.display()),
                source: error,
            },
        })?;
    let written = io::copy(&mut source.take(limit + 1), &mut file)
        .map_err(RegistryError::io(format!("Writing {}", relative.display())))?;
    if written > limit {
        return Err(unsafe_archive(format!(
            "{} is too large",
            relative.display()
        )));
    }
    drop(file);
    set_mode(&target, mode)?;
    Ok(written)
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: Option<u32>) -> Result<(), RegistryError> {
    use std::os::unix::fs::PermissionsExt;
    // Only the permission bits, never set-id or sticky, and always readable by the owner.
    let mode = mode.map_or(0o644, |mode| (mode & 0o777) | 0o600);
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
        .map_err(RegistryError::io("Setting permissions"))
}

#[cfg(not(unix))]
#[allow(clippy::unnecessary_wraps)]
fn set_mode(_path: &Path, _mode: Option<u32>) -> Result<(), RegistryError> {
    Ok(())
}

/// Tracks what an extraction has produced so far.
struct Budget {
    limits: Limits,
    entries: usize,
    bytes: u64,
}

impl Budget {
    fn entry(&mut self) -> Result<(), RegistryError> {
        self.entries += 1;
        if self.entries > self.limits.max_entries {
            return Err(unsafe_archive("it has too many entries"));
        }
        Ok(())
    }

    fn file(&mut self, bytes: u64) -> Result<(), RegistryError> {
        self.bytes = self.bytes.saturating_add(bytes);
        if self.bytes > self.limits.max_total_bytes {
            return Err(unsafe_archive("it expands to too much data"));
        }
        Ok(())
    }

    fn remaining(&self) -> u64 {
        self.limits
            .max_file_bytes
            .min(self.limits.max_total_bytes.saturating_sub(self.bytes))
    }
}

/// Unpacks `archive` into the existing folder `destination`.
pub fn extract(
    archive: &Path,
    format: Format,
    destination: &Path,
    limits: Limits,
) -> Result<(), RegistryError> {
    let file = File::open(archive).map_err(RegistryError::io("Opening the download"))?;
    let mut budget = Budget {
        limits,
        entries: 0,
        bytes: 0,
    };
    match format {
        Format::Zip => extract_zip(file, destination, &mut budget),
        Format::TarGz => extract_tar(flate2::read::GzDecoder::new(file), destination, &mut budget),
    }
}

fn extract_zip(file: File, destination: &Path, budget: &mut Budget) -> Result<(), RegistryError> {
    let mut zip = zip::ZipArchive::new(file).map_err(|error| unsafe_archive(error.to_string()))?;
    for index in 0..zip.len() {
        budget.entry()?;
        let mut entry = zip
            .by_index(index)
            .map_err(|error| unsafe_archive(error.to_string()))?;
        let name = entry.name().to_owned();
        let relative = safe_relative(&name)
            .ok_or_else(|| unsafe_archive(format!("the name {name:?} leaves the folder")))?;
        let mode = entry.unix_mode();
        // The file type lives in the high bits of the Unix mode: links and devices are refused.
        if let Some(kind) = mode.map(|mode| mode & 0o170_000) {
            if !matches!(kind, 0 | 0o100_000 | 0o040_000) {
                return Err(unsafe_archive(format!(
                    "{name:?} is a link or a special file"
                )));
            }
        }
        if entry.is_dir() {
            fs::create_dir_all(destination.join(&relative))
                .map_err(RegistryError::io("Creating a folder"))?;
            continue;
        }
        let written = write_file(destination, &relative, &mut entry, mode, budget.remaining())?;
        budget.file(written)?;
    }
    Ok(())
}

fn extract_tar(
    reader: impl Read,
    destination: &Path,
    budget: &mut Budget,
) -> Result<(), RegistryError> {
    let mut tar = tar::Archive::new(reader);
    let entries = tar
        .entries()
        .map_err(|error| unsafe_archive(error.to_string()))?;
    for entry in entries {
        budget.entry()?;
        let mut entry = entry.map_err(|error| unsafe_archive(error.to_string()))?;
        let name = String::from_utf8_lossy(&entry.path_bytes()).into_owned();
        let relative = safe_relative(&name)
            .ok_or_else(|| unsafe_archive(format!("the name {name:?} leaves the folder")))?;
        let kind = entry.header().entry_type();
        if kind.is_dir() {
            fs::create_dir_all(destination.join(&relative))
                .map_err(RegistryError::io("Creating a folder"))?;
        } else if kind.is_file() {
            let mode = entry.header().mode().ok();
            let written = write_file(destination, &relative, &mut entry, mode, budget.remaining())?;
            budget.file(written)?;
        } else {
            return Err(unsafe_archive(format!(
                "{name:?} is a link or a special file"
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    fn zip_with(entries: &[(&str, &[u8], Option<u32>)]) -> Vec<u8> {
        let mut writer = zip::ZipWriter::new(io::Cursor::new(Vec::new()));
        for (name, data, mode) in entries {
            let mut options = zip::write::SimpleFileOptions::default();
            if let Some(mode) = mode {
                options = options.unix_permissions(*mode);
            }
            writer.start_file(*name, options).unwrap();
            writer.write_all(data).unwrap();
        }
        writer.finish().unwrap().into_inner()
    }

    /// A tar whose first entry is named by raw bytes, since the builder itself refuses `..`.
    fn tar_with(entries: &[(&str, &[u8], tar::EntryType)]) -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        for (name, data, kind) in entries {
            let mut header = tar::Header::new_gnu();
            header.as_old_mut().name[..name.len()].copy_from_slice(name.as_bytes());
            header.set_entry_type(*kind);
            header.set_size(data.len() as u64);
            header.set_mode(0o755);
            if kind.is_symlink() {
                header.set_link_name("/etc/passwd").unwrap();
            }
            header.set_cksum();
            builder.append(&header, *data).unwrap();
        }
        let tar = builder.into_inner().unwrap();
        let mut gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        gzip.write_all(&tar).unwrap();
        gzip.finish().unwrap()
    }

    fn unpack(
        bytes: &[u8],
        format: Format,
        limits: Limits,
    ) -> (tempfile::TempDir, Result<(), RegistryError>) {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("archive");
        fs::write(&archive, bytes).unwrap();
        let destination = dir.path().join("out");
        fs::create_dir(&destination).unwrap();
        let result = extract(&archive, format, &destination, limits);
        (dir, result)
    }

    fn reason(result: Result<(), RegistryError>) -> String {
        result.expect_err("must be refused").to_string()
    }

    #[test]
    fn names_that_leave_the_folder_are_refused() {
        for name in [
            "../x",
            "a/../../x",
            "/etc/passwd",
            "C:/x",
            "a\\..\\..\\x",
            "",
            "..",
            "a/./..",
        ] {
            assert_eq!(safe_relative(name), None, "{name:?}");
        }
        assert_eq!(safe_relative("./agent"), Some(PathBuf::from("agent")));
        assert_eq!(safe_relative("bin/agent"), Some(PathBuf::from("bin/agent")));
        assert_eq!(
            safe_relative("bin\\agent.exe"),
            Some(PathBuf::from("bin/agent.exe"))
        );
    }

    #[test]
    fn formats_come_from_the_end_of_the_path() {
        assert_eq!(format_of("https://x/a.zip"), Some(Format::Zip));
        assert_eq!(format_of("https://x/a.TAR.GZ?token=1"), Some(Format::TarGz));
        assert_eq!(format_of("https://x/a.tgz"), Some(Format::TarGz));
        assert_eq!(format_of("https://x/a.tar.bz2"), None);
        assert_eq!(format_of("https://x/a.zip.exe"), None);
        assert_eq!(format_of("https://x/zip"), None);
    }

    #[cfg(unix)]
    #[test]
    fn both_formats_unpack_files_and_keep_only_safe_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let zip = zip_with(&[
            ("./agent", b"#!/bin/sh\n", Some(0o4755)),
            ("docs/readme.txt", b"hi", None),
        ]);
        let tar = tar_with(&[
            ("agent", b"#!/bin/sh\n", tar::EntryType::Regular),
            ("docs/", b"", tar::EntryType::Directory),
            ("docs/readme.txt", b"hi", tar::EntryType::Regular),
        ]);
        for (bytes, format) in [(zip, Format::Zip), (tar, Format::TarGz)] {
            let (dir, result) = unpack(&bytes, format, Limits::default());
            result.unwrap();
            let out = dir.path().join("out");
            assert_eq!(fs::read(out.join("docs/readme.txt")).unwrap(), b"hi");
            let mode = fs::metadata(out.join("agent"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(
                mode & 0o7777,
                0o755,
                "{format:?}: set-id bits are dropped, execute kept"
            );
        }
    }

    #[test]
    fn an_entry_outside_the_folder_stops_the_extraction_in_both_formats() {
        let zip = zip_with(&[("../evil", b"x", None)]);
        let (dir, result) = unpack(&zip, Format::Zip, Limits::default());
        assert!(reason(result).contains("leaves the folder"));
        assert!(!dir.path().join("evil").exists());
        let tar = tar_with(&[("../evil", b"x", tar::EntryType::Regular)]);
        let (dir, result) = unpack(&tar, Format::TarGz, Limits::default());
        assert!(reason(result).contains("leaves the folder"));
        assert!(!dir.path().join("evil").exists());
        let tar = tar_with(&[("/tmp/evil", b"x", tar::EntryType::Regular)]);
        assert!(
            reason(unpack(&tar, Format::TarGz, Limits::default()).1).contains("leaves the folder")
        );
    }

    #[test]
    fn links_and_special_files_are_refused() {
        let symlink = tar_with(&[("link", b"", tar::EntryType::Symlink)]);
        assert!(reason(unpack(&symlink, Format::TarGz, Limits::default()).1).contains("link"));
        let hardlink = tar_with(&[("link", b"", tar::EntryType::Link)]);
        assert!(reason(unpack(&hardlink, Format::TarGz, Limits::default()).1).contains("link"));
        let device = tar_with(&[("null", b"", tar::EntryType::Char)]);
        assert!(reason(unpack(&device, Format::TarGz, Limits::default()).1).contains("special"));
        let mut writer = zip::ZipWriter::new(io::Cursor::new(Vec::new()));
        writer
            .add_symlink(
                "link",
                "/etc/passwd",
                zip::write::SimpleFileOptions::default(),
            )
            .unwrap();
        let zip = writer.finish().unwrap().into_inner();
        assert!(reason(unpack(&zip, Format::Zip, Limits::default()).1).contains("link"));
    }

    #[test]
    fn a_repeated_name_does_not_overwrite_the_first_file() {
        let tar = tar_with(&[
            ("agent", b"first", tar::EntryType::Regular),
            ("agent", b"second", tar::EntryType::Regular),
        ]);
        let (dir, result) = unpack(&tar, Format::TarGz, Limits::default());
        assert!(reason(result).contains("twice"));
        assert_eq!(fs::read(dir.path().join("out/agent")).unwrap(), b"first");
    }

    #[test]
    fn size_and_count_limits_hold_even_when_the_header_lies() {
        let tight = Limits {
            max_entries: 2,
            max_file_bytes: 10,
            max_total_bytes: 15,
        };
        let big = zip_with(&[("a", &[0; 11], None)]);
        assert!(reason(unpack(&big, Format::Zip, tight).1).contains("too large"));
        let total = tar_with(&[
            ("a", &[0; 8], tar::EntryType::Regular),
            ("b", &[0; 8], tar::EntryType::Regular),
        ]);
        assert!(reason(unpack(&total, Format::TarGz, tight).1).contains("too large"));
        let many = tar_with(&[
            ("a", b"", tar::EntryType::Regular),
            ("b", b"", tar::EntryType::Regular),
            ("c", b"", tar::EntryType::Regular),
        ]);
        assert!(reason(unpack(&many, Format::TarGz, tight).1).contains("too many"));
    }

    #[test]
    fn a_corrupt_archive_is_an_error_not_a_panic() {
        assert!(unpack(b"not an archive", Format::Zip, Limits::default())
            .1
            .is_err());
        assert!(unpack(b"not an archive", Format::TarGz, Limits::default())
            .1
            .is_err());
    }
}
