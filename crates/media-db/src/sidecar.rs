//! Files beside the media — .nfo, .srt, .edl, posters, music.toml — are
//! the one input every service reads that anything able to write to the
//! share can shape. So reads here are bounded in size and refuse symlinks
//! (a link could aim anywhere on this host), and writes go to a temp file
//! beside the target and rename over it — never through a symlink already
//! sitting at the target, and never leaving a half-written sidecar for
//! the scanner to ingest.

use std::io::{self, Read, Write};
use std::path::Path;

/// Largest text sidecar (nfo, srt, edl, toml) read into memory.
pub const MAX_TEXT: u64 = 8 * 1024 * 1024;
/// Largest image sidecar or embedded picture read into memory.
pub const MAX_IMAGE: u64 = 32 * 1024 * 1024;

/// Whether `path` is a regular file (not a symlink) of at most `max` bytes.
pub fn is_regular_within(path: &Path, max: u64) -> bool {
    std::fs::symlink_metadata(path)
        .map(|m| m.file_type().is_file() && m.len() <= max)
        .unwrap_or(false)
}

/// The bytes of a regular file of at most `max` bytes.
pub fn read_capped(path: &Path, max: u64) -> io::Result<Vec<u8>> {
    let meta = std::fs::symlink_metadata(path)?;
    if !meta.file_type().is_file() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "not a regular file"));
    }
    if meta.len() > max {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{} bytes exceeds the {max}-byte sidecar limit", meta.len()),
        ));
    }
    // take() bounds the read as well: the file may grow between stat and read.
    let mut out = Vec::with_capacity(meta.len() as usize);
    std::fs::File::open(path)?.take(max + 1).read_to_end(&mut out)?;
    if out.len() as u64 > max {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "file grew past the sidecar limit"));
    }
    Ok(out)
}

/// `read_capped` as UTF-8 text; anything else is an error, like
/// `std::fs::read_to_string`.
pub fn read_text_capped(path: &Path, max: u64) -> io::Result<String> {
    String::from_utf8(read_capped(path, max)?)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "not UTF-8"))
}

/// Write `bytes` to `path` via a temp file in the same directory and a
/// rename, refusing a symlink at the target. The temp name starts with a
/// dot and ends in .part, which the scanner ignores.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Ok(meta) = std::fs::symlink_metadata(path) {
        if meta.file_type().is_symlink() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "refusing to write through a symlink",
            ));
        }
    }
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "sidecar".to_string());
    let temp = dir.join(format!(".{name}.{}.part", std::process::id()));
    // A stale temp from a crash would make create_new fail forever;
    // remove_file does not follow symlinks, so a planted link goes too.
    let _ = std::fs::remove_file(&temp);
    let result = (|| {
        let mut file = std::fs::File::create_new(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&temp, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("sidecar-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn reads_are_capped_and_writes_land_atomically() {
        let dir = scratch("basic");
        let small = dir.join("a.nfo");
        write_atomic(&small, b"<movie/>").unwrap();
        assert_eq!(read_text_capped(&small, MAX_TEXT).unwrap(), "<movie/>");
        assert!(read_capped(&small, 3).is_err(), "over the cap");
        assert!(is_regular_within(&small, 8));
        assert!(!is_regular_within(&small, 7));
        assert!(!is_regular_within(&dir, MAX_TEXT), "a directory is not a sidecar");
        // No temp file left behind.
        let leftovers: Vec<_> = std::fs::read_dir(&dir).unwrap().flatten().collect();
        assert_eq!(leftovers.len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_are_neither_read_nor_written_through() {
        let dir = scratch("symlink");
        let target = dir.join("target.txt");
        std::fs::write(&target, b"secret").unwrap();
        let link = dir.join("b.srt");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(read_capped(&link, MAX_TEXT).is_err());
        assert!(!is_regular_within(&link, MAX_TEXT));
        assert!(write_atomic(&link, b"x").is_err());
        assert_eq!(std::fs::read(&target).unwrap(), b"secret", "target untouched");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
