//! Skippable-segment discovery for video files: recognizably named
//! chapter markers, and Kodi-style .edl sidecars (comskip output).

use std::path::{Path, PathBuf};

use media_db::{Segment, SegmentKind};

use super::video::Chapter;

/// Best available segment source for one file: the .edl sidecar when one
/// exists (deliberate, usually tool- or hand-written — even an empty one
/// silences chapter guesses), else named chapter markers. Returns the
/// source tag stored beside the segments.
pub fn discover(
    abs: &Path,
    duration_ms: Option<i64>,
    chapters: &[Chapter],
) -> (&'static str, Vec<Segment>) {
    if let Ok(text) = std::fs::read_to_string(edl_path(abs)) {
        return ("edl", parse_edl(&text, duration_ms));
    }
    ("chapters", from_chapters(chapters, duration_ms))
}

fn edl_path(abs: &Path) -> PathBuf {
    abs.with_extension("edl")
}

/// mtime of the .edl sidecar next to a media file, if one exists — the
/// same staleness signal as extract::nfo_mtime.
pub fn edl_mtime(abs: &Path) -> Option<i64> {
    let meta = std::fs::metadata(edl_path(abs)).ok()?;
    let mtime = meta
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs() as i64;
    Some(mtime)
}

/// What a chapter title marks, if anything. Recap is matched first
/// ("Previously on…"), then intro before credits so "Opening Credits"
/// lands as the intro it is.
fn classify(title: &str) -> Option<SegmentKind> {
    let t = title.trim().to_lowercase();
    if t.contains("recap") || t.contains("previously") {
        Some(SegmentKind::Recap)
    } else if t.contains("intro")
        || t.contains("opening")
        || t.contains("main title")
        || t.contains("title sequence")
        || t == "op"
    {
        Some(SegmentKind::Intro)
    } else if t.contains("credit") || t.contains("ending") || t.contains("outro") || t == "ed" {
        Some(SegmentKind::Credits)
    } else {
        None
    }
}

/// Cap on what a chapter marker may claim as skippable: real files carry
/// garbage chapter tables (a recognizably named chapter spanning the
/// whole episode), and skipping such a "segment" jumps to the end. A
/// span longer than this, or than half the file, is not an intro or
/// credits — discard it, which also leaves the file open for the audio
/// detector to have a say.
const MAX_CHAPTER_SEGMENT_MS: i64 = 15 * 60 * 1000;

fn from_chapters(chapters: &[Chapter], duration_ms: Option<i64>) -> Vec<Segment> {
    chapters
        .iter()
        .filter(|c| c.end_ms > c.start_ms && c.start_ms >= 0)
        .filter(|c| {
            let len = c.end_ms - c.start_ms;
            len <= MAX_CHAPTER_SEGMENT_MS && !duration_ms.is_some_and(|d| len * 2 > d)
        })
        .filter_map(|c| {
            classify(&c.title).map(|kind| Segment {
                kind,
                start_ms: c.start_ms,
                end_ms: c.end_ms,
            })
        })
        .collect()
}

/// Kodi-style EDL: one "start stop action" line per segment, seconds
/// (fractional allowed), '#' comments. Cut (0) and commercial-break (3)
/// lines are skippable; mute (1) and scene markers (2) are not. EDL
/// carries no intro/credits notion, so the kind is inferred from
/// position: reaching the file's tail = credits, starting within the
/// first five minutes = intro, anything else a commercial break.
fn parse_edl(text: &str, duration_ms: Option<i64>) -> Vec<Segment> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut parts = line.split_whitespace();
        let mut secs = || parts.next().and_then(|s| s.parse::<f64>().ok());
        let (Some(start), Some(end)) = (secs(), secs()) else { continue };
        let action = secs().map(|a| a as i64).unwrap_or(0);
        if action != 0 && action != 3 {
            continue;
        }
        let (start_ms, end_ms) = ((start * 1000.0) as i64, (end * 1000.0) as i64);
        if start_ms < 0 || end_ms <= start_ms {
            continue;
        }
        let kind = if duration_ms.is_some_and(|d| end_ms >= d - 10_000) {
            SegmentKind::Credits
        } else if start_ms <= 300_000 {
            SegmentKind::Intro
        } else {
            SegmentKind::Commercial
        };
        out.push(Segment { kind, start_ms, end_ms });
    }
    out.sort_by_key(|s| s.start_ms);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chapter(start_ms: i64, end_ms: i64, title: &str) -> Chapter {
        Chapter { start_ms, end_ms, title: title.to_string() }
    }

    #[test]
    fn named_chapters_classify() {
        let segments = from_chapters(
            &[
                chapter(0, 25_000, "Previously On"),
                chapter(25_000, 115_000, "Opening Credits"),
                chapter(115_000, 2_500_000, "Part 1"),
                chapter(2_500_000, 2_580_000, "End Credits"),
            ],
            Some(2_580_000),
        );
        let kinds: Vec<SegmentKind> = segments.iter().map(|s| s.kind).collect();
        assert_eq!(
            kinds,
            [SegmentKind::Recap, SegmentKind::Intro, SegmentKind::Credits]
        );
        assert_eq!(segments[1].start_ms, 25_000);
        assert_eq!(segments[1].end_ms, 115_000);
    }

    #[test]
    fn unnamed_and_ordinary_chapters_yield_nothing() {
        let segments = from_chapters(
            &[chapter(0, 60_000, ""), chapter(60_000, 120_000, "Chapter 2")],
            None,
        );
        assert!(segments.is_empty());
    }

    #[test]
    fn absurd_chapter_spans_are_discarded() {
        // A named chapter spanning nearly the whole episode (real-world
        // garbage chaptering) must not become a skip target.
        let whole = from_chapters(
            &[chapter(5_000, 3_500_000, "Intro")],
            Some(3_600_000),
        );
        assert!(whole.is_empty());
        // Over half the file, even under the absolute cap.
        let half = from_chapters(&[chapter(0, 600_000, "Intro")], Some(1_100_000));
        assert!(half.is_empty());
        // Without a known duration the absolute cap still applies…
        let long = from_chapters(&[chapter(0, 1_000_000, "Intro")], None);
        assert!(long.is_empty());
        // …and a normal intro is kept.
        let ok = from_chapters(&[chapter(0, 90_000, "Intro")], None);
        assert_eq!(ok.len(), 1);
    }

    #[test]
    fn edl_kinds_follow_position() {
        let text = "# comskip output\n\
                    10.5 95 3\n\
                    700 880 3\n\
                    2510.25 2600 0\n\
                    100 200 2\n\
                    junk line\n";
        let segments = parse_edl(text, Some(2_600_000));
        let kinds: Vec<SegmentKind> = segments.iter().map(|s| s.kind).collect();
        assert_eq!(
            kinds,
            [SegmentKind::Intro, SegmentKind::Commercial, SegmentKind::Credits]
        );
        assert_eq!(segments[0].start_ms, 10_500);
        assert_eq!(segments[2].end_ms, 2_600_000);
    }

    #[test]
    fn edl_without_duration_never_claims_credits() {
        let segments = parse_edl("2510 2600 0\n", None);
        assert_eq!(segments[0].kind, SegmentKind::Commercial);
    }
}
