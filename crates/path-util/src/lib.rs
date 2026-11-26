//! Shared platform path conversions, independent of the Host and its services.
use std::path::{Path, PathBuf};

/// Removes the verbatim prefix from absolute Windows drive paths for programs such as Node
/// and cmd that reject it. UNC, device, relative and non-Unicode paths remain unchanged.
pub fn plain_path(path: impl AsRef<Path>) -> PathBuf {
    let path = path.as_ref();
    if let Some(rest) = path.to_str().and_then(|text| text.strip_prefix(r"\\?\")) {
        let bytes = rest.as_bytes();
        if bytes.len() >= 3
            && bytes[0].is_ascii_alphabetic()
            && bytes[1] == b':'
            && bytes[2] == b'\\'
        {
            return PathBuf::from(rest);
        }
    }
    path.to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_absolute_verbatim_drive_paths_are_converted() {
        assert_eq!(plain_path(r"\\?\C:\work"), Path::new(r"C:\work"));
        for path in [
            r"\\?\UNC\server\share",
            r"\\.\pipe\agent",
            r"\\?\C:relative",
            r"C:\work",
            "relative",
        ] {
            assert_eq!(plain_path(path), Path::new(path));
        }
    }
}
