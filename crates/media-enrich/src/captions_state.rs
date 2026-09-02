//! Per-pair memory for the caption steps, kept beside the catalog as
//! `enrich-captions.json`: for every video with a same-name .srt, the size
//! and mtime of both files at the last look, and what was concluded. A
//! pair whose stamps have not moved is skipped without reading the
//! sidecar or probing the video, so a run over an untouched library costs
//! its directory walk and nothing more. Anything that changes either file
//! — a corrected sidecar, a remux, a title strip — moves a stamp and earns
//! a fresh look. Dry runs read the memory but never write it, and a run
//! that fails on a file records nothing for it, so it is retried.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use serde::{Deserialize, Serialize};

const FILE_NAME: &str = "enrich-captions.json";
const VERSION: u32 = 1;

/// Size and modification time of a file, as last seen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stamp {
    pub size: u64,
    pub mtime_secs: i64,
    pub mtime_nanos: u32,
}

impl Stamp {
    /// None when the file cannot be stat'ed (gone, or unreadable).
    pub fn of(path: &Path) -> Option<Stamp> {
        let meta = std::fs::metadata(path).ok()?;
        let since_epoch = meta.modified().ok()?.duration_since(UNIX_EPOCH).ok()?;
        Some(Stamp {
            size: meta.len(),
            mtime_secs: since_epoch.as_secs() as i64,
            mtime_nanos: since_epoch.subsec_nanos(),
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Entry {
    media: Stamp,
    srt: Stamp,
    /// What the last look concluded ("embedded", "replaced", "extracted",
    /// or the skip reason) — for a human reading the file; the decision
    /// to skip rests on the stamps alone.
    note: String,
}

#[derive(Default, Serialize, Deserialize)]
struct FileFormat {
    version: u32,
    files: HashMap<String, Entry>,
}

pub struct CaptionsState {
    path: PathBuf,
    entries: HashMap<String, Entry>,
    seen: HashSet<String>,
    dirty: bool,
}

fn key(media: &Path) -> String {
    media.to_string_lossy().into_owned()
}

impl CaptionsState {
    /// The memory in `state_dir` (the catalog's directory); missing or
    /// unreadable means empty, which only costs one full look.
    pub fn load(state_dir: &Path) -> CaptionsState {
        let path = state_dir.join(FILE_NAME);
        let entries = std::fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<FileFormat>(&bytes).ok())
            .filter(|f| f.version == VERSION)
            .map(|f| f.files)
            .unwrap_or_default();
        CaptionsState { path, entries, seen: HashSet::new(), dirty: false }
    }

    /// Whether this pair looks exactly as it did at the last look. Marks
    /// the pair as seen either way.
    pub fn unchanged(&mut self, media: &Path, media_stamp: Stamp, srt_stamp: Stamp) -> bool {
        let k = key(media);
        self.seen.insert(k.clone());
        self.entries
            .get(&k)
            .is_some_and(|e| e.media == media_stamp && e.srt == srt_stamp)
    }

    pub fn record(&mut self, media: &Path, media_stamp: Stamp, srt_stamp: Stamp, note: &str) {
        let k = key(media);
        self.seen.insert(k.clone());
        let entry = Entry { media: media_stamp, srt: srt_stamp, note: note.to_string() };
        self.entries.insert(k, entry);
        self.dirty = true;
    }

    /// Drop entries under `roots` that this run did not encounter: the
    /// video or its sidecar is gone. Roots not walked keep their entries.
    pub fn forget_unseen_under(&mut self, roots: &[PathBuf]) {
        let before = self.entries.len();
        let seen = &self.seen;
        self.entries.retain(|k, _| {
            seen.contains(k) || !roots.iter().any(|r| Path::new(k).starts_with(r))
        });
        if self.entries.len() != before {
            self.dirty = true;
        }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Write the memory if anything changed (atomically, beside the catalog).
    pub fn save(&self) -> std::io::Result<()> {
        if !self.dirty {
            return Ok(());
        }
        let file = FileFormat { version: VERSION, files: self.entries.clone() };
        let json = serde_json::to_vec_pretty(&file)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
        media_db::sidecar::write_atomic(&self.path, &json)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("captions-state-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn remembers_pairs_across_loads_and_forgets_the_gone() {
        let dir = scratch("basic");
        let movie = dir.join("Movies").join("a.mp4");
        std::fs::create_dir_all(movie.parent().unwrap()).unwrap();
        std::fs::write(&movie, b"mp4").unwrap();
        let srt = movie.with_extension("srt");
        std::fs::write(&srt, b"1\n").unwrap();
        let (ms, ss) = (Stamp::of(&movie).unwrap(), Stamp::of(&srt).unwrap());

        let mut state = CaptionsState::load(&dir);
        assert!(!state.unchanged(&movie, ms, ss), "nothing remembered yet");
        state.record(&movie, ms, ss, "embedded");
        state.save().unwrap();

        let mut again = CaptionsState::load(&dir);
        assert!(again.unchanged(&movie, ms, ss));
        let moved = Stamp { size: ss.size + 1, ..ss };
        assert!(!again.unchanged(&movie, ms, moved), "a changed sidecar is a change");

        // A run that never sees the pair under a walked root drops it;
        // under an unwalked root it stays.
        let mut prune = CaptionsState::load(&dir);
        prune.forget_unseen_under(&[dir.join("Series")]);
        assert_eq!(prune.len(), 1);
        prune.forget_unseen_under(&[dir.join("Movies")]);
        assert_eq!(prune.len(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_foreign_or_broken_file_means_empty() {
        let dir = scratch("broken");
        std::fs::write(dir.join(FILE_NAME), b"{not json").unwrap();
        assert_eq!(CaptionsState::load(&dir).len(), 0);
        std::fs::write(dir.join(FILE_NAME), br#"{"version":99,"files":{}}"#).unwrap();
        assert_eq!(CaptionsState::load(&dir).len(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
