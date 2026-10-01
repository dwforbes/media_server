//! Memory for the loudness step, kept beside the catalog as
//! `enrich-loudness.json`: for every file it has looked at, the file's
//! size and mtime at the time and what was found. Measuring costs a full
//! decode of the audio, so a file whose stamp has not moved is never
//! measured twice; a changed file earns a fresh look (and one that
//! already carries a raised track is recognised from the track itself,
//! so losing this memory costs time, never a second raised track).
//!
//! It also holds `since`: the step is for media arriving from now on, and
//! when the config names no date, "now" is the first run, remembered here.
//!
//! The stereo-twin pass over .mp4 files keeps its memory here as well
//! (`twin_since`, and a stamp per file it has looked at), being the same
//! kind of going-forward step over the same files.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::captions_state::Stamp;
use crate::loudness::Measurement;

const FILE_NAME: &str = "enrich-loudness.json";
const VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Entry {
    /// The file as it was named when looked at (the key drops the
    /// extension, so an .mkv remuxed to .mp4 keeps its entry).
    file: String,
    stamp: Stamp,
    /// What was concluded — for a human reading the file.
    note: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    integrated: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    true_peak: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    range: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    gain: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TwinLook {
    file: String,
    stamp: Stamp,
}

#[derive(Default, Serialize, Deserialize)]
struct FileFormat {
    version: u32,
    #[serde(default)]
    since: Option<i64>,
    #[serde(default)]
    files: HashMap<String, Entry>,
    #[serde(default)]
    twin_since: Option<i64>,
    #[serde(default)]
    twins: HashMap<String, TwinLook>,
}

pub struct LoudnessState {
    path: PathBuf,
    since: Option<i64>,
    entries: HashMap<String, Entry>,
    twin_since: Option<i64>,
    twins: HashMap<String, TwinLook>,
    dirty: bool,
}

fn key(media: &Path) -> String {
    media.with_extension("").to_string_lossy().into_owned()
}

impl LoudnessState {
    /// Missing or unreadable means empty, which costs fresh looks only.
    pub fn load(state_dir: &Path) -> LoudnessState {
        let path = state_dir.join(FILE_NAME);
        let stored = std::fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<FileFormat>(&bytes).ok())
            .filter(|f| f.version == VERSION)
            .unwrap_or_default();
        LoudnessState {
            path,
            since: stored.since,
            entries: stored.files,
            twin_since: stored.twin_since,
            twins: stored.twins,
            dirty: false,
        }
    }

    /// Whether an earlier run has already fixed the starting point.
    pub fn started(&self) -> bool {
        self.since.is_some()
    }

    /// The remembered first-run time, set to `now` on the first call.
    pub fn since_or_start(&mut self, now: i64) -> i64 {
        *self.since.get_or_insert_with(|| {
            self.dirty = true;
            now
        })
    }

    /// Whether the twin pass has fixed its starting point.
    pub fn twin_started(&self) -> bool {
        self.twin_since.is_some()
    }

    /// The twin pass's remembered first-run time, set to `now` on the
    /// first call.
    pub fn twin_since_or_start(&mut self, now: i64) -> i64 {
        *self.twin_since.get_or_insert_with(|| {
            self.dirty = true;
            now
        })
    }

    /// The twin pass looked at the file already, and it is unchanged.
    pub fn twin_settled(&self, media: &Path) -> bool {
        match (self.twins.get(&key(media)), Stamp::of(media)) {
            (Some(look), Some(stamp)) => look.stamp == stamp,
            _ => false,
        }
    }

    /// The twin pass is done with the file as it is on disk now.
    pub fn twin_record(&mut self, media: &Path) {
        let Some(stamp) = Stamp::of(media) else { return };
        self.twins.insert(key(media), TwinLook { file: media.to_string_lossy().into_owned(), stamp });
        self.dirty = true;
    }

    /// Another step rewrote a file this one had measured: the measurement
    /// stands (the audio it describes is still there, behind whatever
    /// was added), so refresh the stamp rather than decode it again.
    pub fn restamp(&mut self, media: &Path) {
        let Some(stamp) = Stamp::of(media) else { return };
        if let Some(entry) = self.entries.get_mut(&key(media)) {
            entry.stamp = stamp;
            entry.file = media.to_string_lossy().into_owned();
            self.dirty = true;
        }
    }

    /// Looked at already, and unchanged since.
    pub fn settled(&self, media: &Path) -> bool {
        match (self.entries.get(&key(media)), Stamp::of(media)) {
            (Some(entry), Some(stamp)) => entry.stamp == stamp,
            _ => false,
        }
    }

    /// Remember the conclusion for the file as it is on disk now.
    pub fn record(&mut self, media: &Path, note: &str, measured: Option<&Measurement>, gain: Option<f64>) {
        let Some(stamp) = Stamp::of(media) else { return };
        self.entries.insert(
            key(media),
            Entry {
                file: media.to_string_lossy().into_owned(),
                stamp,
                note: note.to_string(),
                integrated: measured.map(|m| m.integrated),
                true_peak: measured.map(|m| m.true_peak),
                range: measured.map(|m| m.range),
                gain,
            },
        );
        self.dirty = true;
    }

    /// The file was found already done: keep what an earlier look
    /// measured and refresh the stamp, or note it if this is news.
    pub fn confirm_done(&mut self, media: &Path) {
        let Some(stamp) = Stamp::of(media) else { return };
        match self.entries.get_mut(&key(media)) {
            Some(entry) if entry.gain.is_some() => {
                entry.stamp = stamp;
                entry.file = media.to_string_lossy().into_owned();
                self.dirty = true;
            }
            _ => self.record(media, "normalized earlier", None, None),
        }
    }

    /// Drop entries whose file is gone.
    pub fn forget_missing(&mut self) {
        let before = self.entries.len() + self.twins.len();
        self.entries.retain(|_, e| Path::new(&e.file).exists());
        self.twins.retain(|_, t| Path::new(&t.file).exists());
        self.dirty |= self.entries.len() + self.twins.len() != before;
    }

    pub fn save(&self) -> std::io::Result<()> {
        if !self.dirty {
            return Ok(());
        }
        let json = serde_json::to_vec_pretty(&FileFormat {
            version: VERSION,
            since: self.since,
            files: self.entries.clone(),
            twin_since: self.twin_since,
            twins: self.twins.clone(),
        })
        .map_err(std::io::Error::other)?;
        media_db::sidecar::write_atomic(&self.path, &json)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remembers_by_stem_until_the_file_changes() {
        let dir = std::env::temp_dir().join(format!("enrich-loudness-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mkv = dir.join("Film (1978).mkv");
        std::fs::write(&mkv, b"one").unwrap();

        let mut state = LoudnessState::load(&dir);
        assert_eq!(state.since_or_start(1000), 1000);
        assert_eq!(state.since_or_start(2000), 1000, "the first run stays the start");
        assert!(!state.settled(&mkv));
        let m = Measurement { integrated: -30.0, true_peak: -3.0, range: 12.0 };
        state.record(&mkv, "within range", Some(&m), None);
        assert!(state.settled(&mkv));
        state.save().unwrap();

        let mut state = LoudnessState::load(&dir);
        assert_eq!(state.since_or_start(3000), 1000);
        assert!(state.settled(&mkv), "survives a reload");

        // Changed on disk: a fresh look is due.
        std::fs::write(&mkv, b"one, and more").unwrap();
        assert!(!state.settled(&mkv));

        // Remuxed to .mp4 and recorded again: same entry, new name.
        let mp4 = dir.join("Film (1978).mp4");
        std::fs::rename(&mkv, &mp4).unwrap();
        state.record(&mp4, "normalized", Some(&m), Some(6.0));
        assert!(state.settled(&mp4));
        assert_eq!(state.entries.len(), 1);

        // The twin pass's own memory, and a rewrite that keeps a measurement.
        assert_eq!(state.twin_since_or_start(5000), 5000);
        assert!(!state.twin_settled(&mp4));
        state.twin_record(&mp4);
        assert!(state.twin_settled(&mp4));
        std::fs::write(&mp4, b"rewritten with a twin").unwrap();
        assert!(!state.twin_settled(&mp4) && !state.settled(&mp4));
        state.restamp(&mp4);
        assert!(state.settled(&mp4), "the measurement is kept under the new stamp");
        state.save().unwrap();
        let state2 = LoudnessState::load(&dir);
        assert!(state2.twin_started() && state2.twin_settled(&mp4) == false);

        std::fs::remove_file(&mp4).unwrap();
        state.forget_missing();
        assert!(state.entries.is_empty() && state.twins.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
