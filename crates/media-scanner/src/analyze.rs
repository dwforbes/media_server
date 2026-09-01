//! Automatic intro/credits detection for TV seasons ("layer 3"): every
//! episode of a season shares nearly identical intro and credits audio,
//! so chromaprint fingerprints of each episode's head and tail windows
//! are cross-matched pairwise and the stretches episodes have in common
//! become intro/credits segments (source 'audio'). Chapter- or
//! edl-sourced segments always win over detection.
//!
//! Fingerprints are cached in segment_prints keyed to size+mtime, so a
//! new episode decodes only itself and is matched against its siblings'
//! cached prints. A season is analyzed when any ready episode lacks a
//! current print row; storing the row (even with empty prints, for
//! undecodable audio) is what marks the work done.

use std::collections::HashMap;
use std::path::Path;
use std::process::Command;

use anyhow::{bail, Context, Result};
use media_db::queries::segments as seg_db;
use media_db::{Segment, SegmentKind};
use rusqlite::Connection;
use rusty_chromaprint::{Configuration, Fingerprinter};

/// Bump to invalidate every cached fingerprint (decode parameters or
/// matching constants changed incompatibly).
pub const PRINT_VERSION: i64 = 1;

/// Audio windows: intros live in the first ten minutes, credits in the
/// last five.
const HEAD_SECS: u32 = 600;
const TAIL_SECS: u32 = 300;
const SAMPLE_RATE: u32 = 11025;

/// Matcher acceptance. Chromaprint items whose hamming distance is at
/// most MAX_BITS (of 32) count as matching; runs may bridge gaps up to
/// MERGE_GAP_SECS on the same alignment (encoder jitter, an
/// episode-specific overlay) but must stay MIN_MATCH_FRACTION matched
/// overall; and a final run must be at least MIN_SECONDS but no more than
/// MAX_FRACTION of the window — a near-total match means duplicate
/// content (two renditions mislabeled as different episodes), not an
/// intro. Candidate alignments come from voting on 12-bit item prefixes
/// (only offsets with at least MIN_VOTES, best TOP_OFFSETS evaluated) —
/// intro and credits usually sit at different relative offsets, which is
/// why a single best global alignment is not enough.
const MAX_BITS: u32 = 8;
const MERGE_GAP_SECS: f32 = 3.0;
const MIN_MATCH_FRACTION: f32 = 0.6;
const MIN_SECONDS: f32 = 12.0;
const MAX_FRACTION: f32 = 0.8;
const MIN_VOTES: usize = 10;
const TOP_OFFSETS: usize = 8;

/// Snapping: an intro starting under 2s starts at 0; credits ending
/// within 10s of the file's end run to the end (so skipping them fires
/// 'ended' and auto-play-next takes over).
const SNAP_START_SECS: f32 = 2.0;
const SNAP_END_SECS: f32 = 10.0;

/// Analyze the first stale season that is actually analyzable (all files
/// present on disk). Returns false when nothing was analyzed — either no
/// season is stale or the stale ones have unreachable files (an unmounted
/// root: skipped without storing anything, so they are retried once the
/// files return). One season per call keeps the caller's event loop
/// responsive; call in a loop to drain.
pub fn analyze_next(conn: &mut Connection, ffmpeg: &str) -> Result<bool> {
    for (series, season) in seg_db::stale_seasons(conn, PRINT_VERSION)? {
        let files = seg_db::season_files(conn, &series, season, PRINT_VERSION)?;
        if files.iter().any(|f| !Path::new(&f.abs_path).is_file()) {
            tracing::debug!("segment analysis: {series:?} S{season} has unreachable files; skipping");
            continue;
        }
        match analyze_season(conn, ffmpeg, &series, season, files) {
            Ok(()) => return Ok(true),
            Err(err) => {
                tracing::warn!("segment analysis for {series:?} S{season}: {err:#}");
                continue;
            }
        }
    }
    Ok(false)
}

struct Episode {
    file_id: i64,
    episode: i64,
    duration_s: f32,
    /// Absolute second where the decoded tail window begins.
    tail_offset_s: f32,
    head: Vec<u32>,
    tail: Vec<u32>,
    head_spans: Vec<(f32, f32)>,
    tail_spans: Vec<(f32, f32)>,
}

fn analyze_season(
    conn: &mut Connection,
    ffmpeg: &str,
    series: &str,
    season: i64,
    files: Vec<seg_db::AnalysisFile>,
) -> Result<()> {
    let config = Configuration::preset_test2();
    let item_s = config.item_duration_in_seconds();
    let mut episodes: Vec<Episode> = Vec::new();
    let mut decoded = 0usize;
    for file in files {
        let (head, tail) = match file.prints {
            Some(prints) => prints,
            None => {
                let prints = fingerprint_file(ffmpeg, &file.abs_path, &config)
                    .unwrap_or_else(|err| {
                        // Stored empty so a permanently undecodable file
                        // (no audio track, corrupt) is not retried every
                        // pass; a changed file re-fingerprints as usual.
                        tracing::warn!(
                            "fingerprinting {}: {err:#}; marking analyzed without prints",
                            file.abs_path
                        );
                        (Vec::new(), Vec::new())
                    });
                seg_db::store_prints(
                    conn, file.file_id, file.size, file.mtime, PRINT_VERSION, &prints.0, &prints.1,
                )?;
                decoded += 1;
                prints
            }
        };
        let Some(duration_ms) = file.duration_ms.filter(|d| *d > 0) else { continue };
        let duration_s = duration_ms as f32 / 1000.0;
        episodes.push(Episode {
            file_id: file.file_id,
            episode: file.episode,
            duration_s,
            tail_offset_s: (duration_s - TAIL_SECS as f32).max(0.0),
            head,
            tail,
            head_spans: Vec::new(),
            tail_spans: Vec::new(),
        });
    }

    // Pair distinct episode numbers only: two renditions of one episode
    // are identical throughout and would "match" wall to wall.
    for i in 0..episodes.len() {
        for j in i + 1..episodes.len() {
            if episodes[i].episode == episodes[j].episode {
                continue;
            }
            // Head pass: the longest shared run that can be an intro on
            // both sides. A short episode fits entirely inside the head
            // window, so its credits match here too — starting in the
            // back half of an episode disqualifies a run as its intro.
            let intro_ok = |span: &(f32, f32), ep: &Episode| span.0 < ep.duration_s / 2.0;
            let head = common_spans(&episodes[i].head, &episodes[j].head, item_s)
                .into_iter()
                .find(|(a, b)| intro_ok(a, &episodes[i]) && intro_ok(b, &episodes[j]));
            if let Some((a, b)) = head {
                episodes[i].head_spans.push(a);
                episodes[j].head_spans.push(b);
            }
            let tail = common_spans(&episodes[i].tail, &episodes[j].tail, item_s)
                .into_iter()
                .next();
            if let Some((a, b)) = tail {
                episodes[i].tail_spans.push(a);
                episodes[j].tail_spans.push(b);
            }
        }
    }

    // An episode needs agreement from two pairings, or one when the
    // season only has one other episode to compare against.
    let need = if episodes.len() == 2 { 1 } else { 2 };
    let mut found = 0usize;
    for ep in &episodes {
        if seg_db::has_manual_segments(conn, ep.file_id)? {
            continue;
        }
        let mut rows: Vec<Segment> = Vec::new();
        if let Some((start, end)) = consensus(&ep.head_spans, need) {
            let start = if start < SNAP_START_SECS { 0.0 } else { start };
            rows.push(Segment {
                kind: SegmentKind::Intro,
                start_ms: (start * 1000.0) as i64,
                end_ms: (end * 1000.0) as i64,
            });
        }
        if let Some((start, end)) = consensus(&ep.tail_spans, need) {
            let start = ep.tail_offset_s + start;
            let end = ep.tail_offset_s + end;
            let end = if ep.duration_s - end < SNAP_END_SECS { ep.duration_s } else { end };
            // A tail match that overlaps the intro span region cannot
            // happen (windows are disjoint for files over HEAD_SECS, and
            // start_ms keys differ regardless), so store as-is.
            if start > rows.last().map(|r| r.end_ms as f32 / 1000.0).unwrap_or(0.0) {
                rows.push(Segment {
                    kind: SegmentKind::Credits,
                    start_ms: (start * 1000.0) as i64,
                    end_ms: (end * 1000.0) as i64,
                });
            }
        }
        found += rows.len();
        seg_db::replace_for_file(conn, ep.file_id, "audio", &rows)?;
    }
    tracing::info!(
        "segment analysis: {series:?} S{season}: {} episodes ({decoded} fingerprinted), \
         {found} segments detected",
        episodes.len()
    );
    Ok(())
}

/// Median start/end across the spans this episode's pairings agreed on.
fn consensus(spans: &[(f32, f32)], need: usize) -> Option<(f32, f32)> {
    if spans.len() < need || spans.is_empty() {
        return None;
    }
    let median = |mut values: Vec<f32>| -> f32 {
        values.sort_by(|a, b| a.total_cmp(b));
        values[values.len() / 2]
    };
    let start = median(spans.iter().map(|s| s.0).collect());
    let end = median(spans.iter().map(|s| s.1).collect());
    (end > start).then_some((start, end))
}

/// The sufficiently similar stretches two fingerprints share, longest
/// first, as (start, end) seconds within each window.
///
/// A single global alignment (what fingerprint comparison libraries
/// compute) cannot serve here: two episodes' intros and credits sit at
/// different relative offsets because the bodies between them differ in
/// length. So candidate alignments are found by voting — exact matches on
/// the top 12 bits of each item — and each promising alignment is walked
/// for gap-bridged runs of near-identical items; runs from different
/// alignments covering the same region collapse to the longest.
fn common_spans(a: &[u32], b: &[u32], item_s: f32) -> Vec<((f32, f32), (f32, f32))> {
    if a.len() < 8 || b.len() < 8 {
        return Vec::new();
    }
    let strip = |v: u32| v >> 20;
    let mut index: HashMap<u32, Vec<usize>> = HashMap::new();
    for (i, &v) in a.iter().enumerate() {
        index.entry(strip(v)).or_default().push(i);
    }
    // Hashes carried by a large share of items (silence, station idents)
    // vote for every alignment at once, i.e. for none.
    let cap = (a.len() / 32).max(4);
    let mut votes: HashMap<isize, usize> = HashMap::new();
    for (j, &v) in b.iter().enumerate() {
        let Some(positions) = index.get(&strip(v)) else { continue };
        if positions.len() > cap {
            continue;
        }
        for &i in positions {
            *votes.entry(i as isize - j as isize).or_default() += 1;
        }
    }
    let mut offsets: Vec<(isize, usize)> =
        votes.into_iter().filter(|(_, n)| *n >= MIN_VOTES).collect();
    offsets.sort_by_key(|(_, n)| std::cmp::Reverse(*n));
    offsets.truncate(TOP_OFFSETS);

    let gap_items = (MERGE_GAP_SECS / item_s) as usize;
    let min_items = ((MIN_SECONDS / item_s) as usize).max(1);
    let mut runs: Vec<(usize, usize, isize)> = Vec::new();
    for (offset, _) in &offsets {
        runs.extend(runs_at(a, b, *offset, gap_items, min_items));
    }
    runs.sort_by_key(|r| std::cmp::Reverse(r.1));
    // Adjacent alignments (codec framing off by an item) produce near-
    // duplicate runs; keep a run only when most of it is new territory.
    let mut kept: Vec<(usize, usize, isize)> = Vec::new();
    for run in runs {
        let duplicate = kept.iter().any(|k| {
            let lo = run.0.max(k.0);
            let hi = (run.0 + run.1).min(k.0 + k.1);
            hi > lo && (hi - lo) * 2 > run.1
        });
        if !duplicate {
            kept.push(run);
        }
    }
    let window = a.len().min(b.len()) as f32 * item_s;
    kept.into_iter()
        .filter_map(|(start, len, offset)| {
            let duration = len as f32 * item_s;
            if duration > window * MAX_FRACTION {
                return None;
            }
            let sa = start as f32 * item_s;
            let sb = (start as isize - offset) as f32 * item_s;
            Some(((sa, sa + duration), (sb, sb + duration)))
        })
        .collect()
}

/// Runs of matching items along one alignment (a[i] against
/// b[i - offset]), bridging gaps up to gap_items, as (a_start, len,
/// offset). A run must be min_items long and MIN_MATCH_FRACTION matched.
fn runs_at(
    a: &[u32],
    b: &[u32],
    offset: isize,
    gap_items: usize,
    min_items: usize,
) -> Vec<(usize, usize, isize)> {
    let first = offset.max(0) as usize;
    let last = a.len().min((b.len() as isize + offset).max(0) as usize);
    let mut out = Vec::new();
    let mut run: Option<(usize, usize, usize)> = None; // (start, last_good, matched)
    let mut flush = |run: &mut Option<(usize, usize, usize)>| {
        if let Some((start, last_good, matched)) = run.take() {
            let len = last_good - start + 1;
            if len >= min_items && matched as f32 >= len as f32 * MIN_MATCH_FRACTION {
                out.push((start, len, offset));
            }
        }
    };
    for i in first..last {
        let j = (i as isize - offset) as usize;
        if (a[i] ^ b[j]).count_ones() <= MAX_BITS {
            match &mut run {
                Some((_, last_good, matched)) => {
                    *last_good = i;
                    *matched += 1;
                }
                None => run = Some((i, i, 1)),
            }
        } else if run.is_some_and(|(_, last_good, _)| i - last_good > gap_items) {
            flush(&mut run);
        }
    }
    flush(&mut run);
    out
}

/// Fingerprint one file's head and tail audio windows.
fn fingerprint_file(
    ffmpeg: &str,
    path: &str,
    config: &Configuration,
) -> Result<(Vec<u32>, Vec<u32>)> {
    let head = decode_window(ffmpeg, path, true)?;
    let tail = decode_window(ffmpeg, path, false)?;
    Ok((fingerprint(&head, config)?, fingerprint(&tail, config)?))
}

fn fingerprint(samples: &[i16], config: &Configuration) -> Result<Vec<u32>> {
    let mut printer = Fingerprinter::new(config);
    printer
        .start(SAMPLE_RATE, 1)
        .map_err(|e| anyhow::anyhow!("chromaprint reset: {e:?}"))?;
    printer.consume(samples);
    printer.finish();
    Ok(printer.fingerprint().to_vec())
}

/// Decode one audio window to mono 11025 Hz s16le via ffmpeg: the first
/// HEAD_SECS, or (via -sseof) the last TAIL_SECS.
fn decode_window(ffmpeg: &str, path: &str, head: bool) -> Result<Vec<i16>> {
    let mut cmd = Command::new(ffmpeg);
    cmd.args(["-v", "error", "-nostdin"]);
    if head {
        cmd.args(["-t", &HEAD_SECS.to_string()]);
    } else {
        // -t as well: -sseof trusts the container's declared duration, and
        // a file whose real audio runs far past it would otherwise decode
        // to the true end into memory.
        cmd.args(["-sseof", &format!("-{TAIL_SECS}"), "-t", &(TAIL_SECS + 30).to_string()]);
    }
    cmd.arg("-i").arg(path).args([
        "-map", "0:a:0", "-ac", "1", "-ar", &SAMPLE_RATE.to_string(), "-f", "s16le", "-",
    ]);
    let out = cmd.output().with_context(|| format!("running {ffmpeg}"))?;
    if !out.status.success() {
        bail!(
            "ffmpeg failed decoding {path}: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(out
        .stdout
        .chunks_exact(2)
        .map(|c| i16::from_le_bytes([c[0], c[1]]))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Diagnostic: dump raw matcher output for the prints cached in the
    /// database at $SEG_DB. Run with --ignored --nocapture.
    #[test]
    #[ignore]
    fn debug_matcher() {
        let db = std::env::var("SEG_DB").unwrap();
        let conn = rusqlite::Connection::open(db).unwrap();
        let files = seg_db::season_files(&conn, "Noise Show", 1, PRINT_VERSION).unwrap();
        let config = Configuration::preset_test2();
        let item_s = config.item_duration_in_seconds();
        for i in 0..files.len() {
            for j in i + 1..files.len() {
                let (h1, t1) = files[i].prints.clone().unwrap();
                let (h2, t2) = files[j].prints.clone().unwrap();
                for (name, a, b) in [("head", &h1, &h2), ("tail", &t1, &t2)] {
                    let spans = common_spans(a, b, item_s);
                    eprintln!(
                        "{} vs {} {name} ({} x {} items): {} spans",
                        files[i].file_id, files[j].file_id, a.len(), b.len(), spans.len()
                    );
                    for ((sa, ea), (sb, eb)) in spans.iter().take(10) {
                        eprintln!("  a=[{sa:.1},{ea:.1}] b=[{sb:.1},{eb:.1}]");
                    }
                }
            }
        }
    }

    #[test]
    fn consensus_takes_medians_and_respects_need() {
        let spans = [(10.0, 40.0), (11.0, 41.0), (90.0, 95.0)];
        assert_eq!(consensus(&spans, 2), Some((11.0, 41.0)));
        assert_eq!(consensus(&spans[..1], 2), None);
        assert_eq!(consensus(&spans[..1], 1), Some((10.0, 40.0)));
        assert_eq!(consensus(&[], 1), None);
    }

    /// Deterministic pseudo-random fingerprint items.
    fn noise(seed: u32, n: usize) -> Vec<u32> {
        let mut state = seed.wrapping_mul(2654435761).wrapping_add(1);
        (0..n)
            .map(|_| {
                state = state.wrapping_mul(1664525).wrapping_add(1013904223);
                state
            })
            .collect()
    }

    #[test]
    fn common_span_finds_shared_run_at_different_offsets() {
        let item_s = Configuration::preset_test2().item_duration_in_seconds();
        let shared = noise(7, (30.0 / item_s) as usize); // ~30 seconds
        // a: 20s noise + shared; b: 45s different noise + shared.
        let pre_a = (20.0 / item_s) as usize;
        let pre_b = (45.0 / item_s) as usize;
        let mut a = noise(1, pre_a);
        a.extend(&shared);
        a.extend(noise(2, (400.0 / item_s) as usize));
        let mut b = noise(3, pre_b);
        b.extend(&shared);
        b.extend(noise(4, (380.0 / item_s) as usize));

        let ((sa, ea), (sb, eb)) =
            common_spans(&a, &b, item_s).into_iter().next().expect("span found");
        assert!((sa - 20.0).abs() < 3.0, "start in a: {sa}");
        assert!((sb - 45.0).abs() < 3.0, "start in b: {sb}");
        assert!((ea - sa - 30.0).abs() < 4.0, "duration: {}", ea - sa);
        assert!(((ea - sa) - (eb - sb)).abs() < 0.01);
    }

    #[test]
    fn common_span_rejects_wall_to_wall_matches() {
        let item_s = Configuration::preset_test2().item_duration_in_seconds();
        let same = noise(9, 4000);
        assert!(common_spans(&same, &same, item_s).is_empty());
    }

    #[test]
    fn common_span_rejects_dissimilar_audio() {
        let item_s = Configuration::preset_test2().item_duration_in_seconds();
        assert!(common_spans(&noise(1, 3000), &noise(2, 3000), item_s).is_empty());
    }

    #[test]
    fn common_span_finds_two_shared_runs_at_different_alignments() {
        // Intro and credits sit at different relative offsets between two
        // episodes — both must be found.
        let item_s = Configuration::preset_test2().item_duration_in_seconds();
        let items = |secs: f32| (secs / item_s) as usize;
        let intro = noise(7, items(25.0));
        let credits = noise(8, items(20.0));
        let mut a = noise(1, items(15.0));
        a.extend(&intro);
        a.extend(noise(2, items(200.0)));
        a.extend(&credits);
        let mut b = noise(3, items(40.0));
        b.extend(&intro);
        b.extend(noise(4, items(150.0)));
        b.extend(&credits);

        let spans = common_spans(&a, &b, item_s);
        assert_eq!(spans.len(), 2, "{spans:?}");
        let ((ia, _), (ib, _)) = spans[0]; // longest = intro
        assert!((ia - 15.0).abs() < 3.0 && (ib - 40.0).abs() < 3.0, "{spans:?}");
        let ((ca, _), (cb, _)) = spans[1];
        assert!((ca - 240.0).abs() < 3.0 && (cb - 215.0).abs() < 3.0, "{spans:?}");
    }
}
