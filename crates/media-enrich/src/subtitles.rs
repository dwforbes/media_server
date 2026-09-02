//! Sidecar subtitles in both directions: embed a sidecar .srt into MP4
//! files that have none, and extract an embedded text track to a sidecar
//! .srt so every player (and the web player, without on-demand work) can
//! use it.
//!
//! Embedding: equivalent to the manual recipe
//!   ffmpeg -i in.mp4 -i in.srt -c copy -c:s mov_text out.mp4
//! — existing streams are copied untouched; only a subtitle track is
//! added. Unlike every other enrichment step this replaces a whole media
//! file, so it is deliberately conservative: strict preconditions, a mux
//! into a temp file in the same directory, ffprobe verification of the
//! result, then a single atomic rename over the original.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use media_db::textenc::decode_subtitle_text;

pub enum Outcome {
    /// Not a candidate (wrong format, no .srt, already subtitled, ...).
    Skipped(&'static str),
    /// The sidecar (its hash) is now the file's track, with the record.
    Embedded(String),
    /// The sidecar changed since it was embedded: the track was replaced.
    Replaced(String),
    /// The track already says what the sidecar says.
    UpToDate(String),
    /// A track without a record — embedded before records existed, or by
    /// something else — says the same as the sidecar: the sidecar is
    /// taken as canonical from here on, the file untouched.
    Adopted(String),
    /// Dry run: what a real run would do.
    WouldEmbed,
    WouldReplace,
}

fn is_mp4(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_lowercase())
        .is_some_and(|e| e == "mp4" || e == "m4v")
}

/// Stream census of a file, plus its ©too "encoding tool" text — where
/// media_db::captions records which sidecar an embedded track came from.
#[derive(Debug, Default, Clone)]
struct Probe {
    video: usize,
    audio: usize,
    /// Text subtitle streams.
    subtitle: usize,
    /// ffmpeg's `0:s:N` of the first text subtitle stream.
    first_text_ordinal: Option<usize>,
    duration: f64,
    encoder: String,
}

const TEXT_SUB_CODECS: &[&str] =
    &["subrip", "srt", "ass", "ssa", "mov_text", "webvtt", "text", "subviewer"];

fn probe(ffprobe: &str, path: &Path) -> Result<Probe> {
    let output = Command::new(ffprobe)
        .args([
            "-v", "error",
            "-show_entries", "stream=codec_name,codec_type:format=duration:format_tags=encoder",
            "-of", "default=nw=1",
        ])
        .arg(path)
        .output()
        .with_context(|| format!("running {ffprobe}"))?;
    if !output.status.success() {
        bail!("ffprobe failed: {}", String::from_utf8_lossy(&output.stderr).trim());
    }
    Ok(parse_probe(&String::from_utf8_lossy(&output.stdout)))
}

fn parse_probe(text: &str) -> Probe {
    let mut p = Probe::default();
    // Per stream, codec_name precedes codec_type in the output.
    let mut last_codec = String::new();
    let mut sub_streams = 0usize;
    for line in text.lines() {
        if let Some(name) = line.strip_prefix("codec_name=") {
            last_codec = name.to_string();
        } else if let Some(kind) = line.strip_prefix("codec_type=") {
            match kind {
                "video" => p.video += 1,
                "audio" => p.audio += 1,
                "subtitle" => {
                    // Only text subtitles count: a bitmap-only (PGS/VobSub)
                    // file still deserves the .srt embedded.
                    if TEXT_SUB_CODECS.contains(&last_codec.as_str()) {
                        p.subtitle += 1;
                        p.first_text_ordinal.get_or_insert(sub_streams);
                    }
                    sub_streams += 1;
                }
                _ => {}
            }
        } else if let Some(d) = line.strip_prefix("duration=") {
            p.duration = d.parse().unwrap_or(0.0);
        } else if let Some(tool) = line.strip_prefix("TAG:encoder=") {
            p.encoder = tool.to_string();
        }
    }
    p
}

/// SHA-256, as hex, of the text that gets embedded — the decoded UTF-8,
/// so a sidecar re-saved in another encoding (or merely touched) is not
/// a change; only different captions are.
pub fn sidecar_hash(text: &str) -> String {
    let digest = ring::digest::digest(&ring::digest::SHA256, text.as_bytes());
    digest.as_ref().iter().map(|b| format!("{b:02x}")).collect()
}

/// A subtitle track as SRT text, via ffmpeg.
fn track_text(ffmpeg: &str, media: &Path, ordinal: usize) -> Result<String> {
    let output = Command::new(ffmpeg)
        .args(["-v", "error", "-nostdin", "-i"])
        .arg(media)
        .args(["-map", &format!("0:s:{ordinal}"), "-f", "srt", "-"])
        .output()
        .with_context(|| format!("running {ffmpeg}"))?;
    if !output.status.success() {
        bail!("ffmpeg could not read the subtitle track: {}", String::from_utf8_lossy(&output.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Whether two SRT texts carry the same captions: the same cues at the
/// same times with the same words. Indices, styling tags, line breaks
/// within a cue and surrounding whitespace do not count — an srt →
/// mov_text → srt round trip changes exactly those.
pub fn cues_match(a: &str, b: &str) -> bool {
    normalized_cues(a) == normalized_cues(b)
}

fn normalized_cues(text: &str) -> Vec<(i64, i64, String)> {
    use std::sync::OnceLock;
    static TAGS: OnceLock<regex::Regex> = OnceLock::new();
    let tags = TAGS.get_or_init(|| regex::Regex::new(r"<[^>]*>|\{\\[^}]*\}").unwrap());
    let mut cues = Vec::new();
    let mut lines = text.lines().map(|l| l.trim()).peekable();
    while let Some(line) = lines.next() {
        let Some((start, end)) = line.split_once("-->") else { continue };
        let (Some(start), Some(end)) = (parse_timestamp(start), parse_timestamp(end)) else {
            continue;
        };
        let mut words: Vec<String> = Vec::new();
        while let Some(next) = lines.peek() {
            if next.is_empty() {
                break;
            }
            let clean = tags.replace_all(next, " ");
            words.extend(clean.split_whitespace().map(str::to_string));
            lines.next();
        }
        if !words.is_empty() {
            cues.push((start, end, words.join(" ")));
        }
    }
    cues
}

/// "HH:MM:SS,mmm" / "HH:MM:SS.mmm" / "MM:SS,mmm" (anything after a space,
/// such as SRT position hints, ignored) as milliseconds.
fn parse_timestamp(s: &str) -> Option<i64> {
    let s = s.trim().split_whitespace().next()?;
    let (clock, millis) = s.split_once([',', '.'])?;
    let mut ms: i64 = millis.parse::<String>().ok()?.chars().take(3).collect::<String>().parse().ok()?;
    if millis.len() < 3 {
        ms *= 10i64.pow((3 - millis.len().min(3)) as u32);
    }
    let mut total = 0i64;
    for part in clock.split(':') {
        total = total * 60 + part.parse::<i64>().ok()?;
    }
    Some(total * 1000 + ms)
}

/// Embed `<stem>.srt` into an MP4 with no text subtitle stream — or, when
/// the file's captions are known to come from that sidecar and it has
/// since changed, replace them with it. "Known" is the record in the file
/// (media_db::captions), else `known`, the hash the caller remembers for
/// it (captions_state). A single track with neither is compared to the
/// sidecar cue by cue: a match adopts the sidecar as canonical; a
/// mismatch is left alone unless `adopt_all` says the sidecar wins.
pub fn embed_if_applicable(
    ffmpeg: &str,
    ffprobe: &str,
    media: &Path,
    dry_run: bool,
    known: Option<&str>,
    adopt_all: bool,
) -> Result<Outcome> {
    if !is_mp4(media) {
        return Ok(Outcome::Skipped("not mp4"));
    }
    let srt = media.with_extension("srt");
    if !media_db::sidecar::is_regular_within(&srt, media_db::sidecar::MAX_TEXT) {
        return Ok(Outcome::Skipped("no .srt sidecar"));
    }
    // mov_text needs clean UTF-8 text. Decode the unambiguous encodings
    // (UTF-16 via BOM, mostly-ASCII Windows-1252/Latin-1) rather than
    // embedding mojibake; anything unclear is skipped.
    let srt_bytes = media_db::sidecar::read_capped(&srt, media_db::sidecar::MAX_TEXT)?;
    if srt_bytes.is_empty() {
        return Ok(Outcome::Skipped("srt is empty"));
    }
    let Some(srt_text) = decode_subtitle_text(&srt_bytes) else {
        return Ok(Outcome::Skipped("srt encoding unrecognized"));
    };
    // Mux from a UTF-8 temp sidecar when conversion was needed; the
    // original .srt is never modified.
    let needs_conversion = srt_text.as_bytes() != srt_bytes.as_slice();
    let hash = sidecar_hash(&srt_text);

    let before = probe(ffprobe, media)?;
    if before.video == 0 {
        return Ok(Outcome::Skipped("no video stream"));
    }
    let replace = if before.subtitle == 0 {
        false
    } else {
        let recorded = media_db::captions::recorded_hash(&before.encoder).or(known);
        match recorded {
            Some(recorded) if recorded == hash => return Ok(Outcome::UpToDate(hash)),
            _ if before.subtitle != 1 => {
                return Ok(Outcome::Skipped(if recorded.is_some() {
                    "subtitle layout changed since embedding; not replacing"
                } else {
                    "already has text subtitles"
                }))
            }
            Some(_) => true,
            None => {
                // One track, no provenance: embedded before records
                // existed, or by something else entirely. If it says what
                // the sidecar says, the sidecar is its source for all
                // practical purposes; if not, only an explicit adopt-all
                // may decide the sidecar wins.
                let ordinal = before.first_text_ordinal.unwrap_or(0);
                let embedded = track_text(ffmpeg, media, ordinal)?;
                if cues_match(&embedded, &srt_text) {
                    return Ok(Outcome::Adopted(hash));
                }
                if !adopt_all {
                    return Ok(Outcome::Skipped("already has text subtitles; they differ from the sidecar"));
                }
                true
            }
        }
    };
    if dry_run {
        return Ok(if replace { Outcome::WouldReplace } else { Outcome::WouldEmbed });
    }

    // Mux to a temp file beside the original (same filesystem => atomic
    // rename), with a name the scanner ignores (leading dot).
    let dir = media.parent().unwrap_or_else(|| Path::new("."));
    let stem = media.file_stem().unwrap_or_default().to_string_lossy();
    let temp = crate::remux::temp_beside(dir, &stem, "subtitles-tmp", "mp4");
    let srt_input: PathBuf = if needs_conversion {
        let converted = temp.with_extension("srt");
        std::fs::write(&converted, &srt_text)
            .with_context(|| format!("writing {}", converted.display()))?;
        converted
    } else {
        srt.clone()
    };
    // A plain single-pass stream copy, exactly like the manual recipe.
    // No +faststart: it rewrites the whole file a second time to move
    // the moov atom, doubling I/O and leaving a window where the output
    // can lack its moov entirely; range-request streaming doesn't need it.
    let mut cmd = Command::new(ffmpeg);
    cmd.args(["-v", "error", "-nostdin", "-i"])
        .arg(media)
        .arg("-i")
        .arg(&srt_input)
        .args(["-map", "0:v", "-map", "0:a?"]);
    if !replace {
        // First embedding: whatever (non-text) subtitle streams the file
        // has come along. A replacement drops the one text track.
        cmd.args(["-map", "0:s?"]);
    }
    cmd.args([
        "-map", "1",
        "-c", "copy", "-c:s", "mov_text",
        "-metadata:s:s:0", "language=eng",
        "-metadata",
    ])
    .arg(format!("encoding_tool={}", media_db::captions::tag(&hash)))
    .arg("-y")
    .arg(&temp);
    let status = cmd.status().with_context(|| format!("running {ffmpeg}"))?;
    if needs_conversion {
        let _ = std::fs::remove_file(&srt_input);
    }
    if !status.success() {
        let _ = std::fs::remove_file(&temp);
        bail!("ffmpeg mux failed ({status})");
    }
    // Make sure the mux is fully on disk before probing or renaming.
    if let Ok(f) = std::fs::File::open(&temp) {
        let _ = f.sync_all();
    }

    // Verify before replacing anything: every video/audio stream preserved,
    // a subtitle present, duration unchanged, and the record written.
    // Data/attachment streams are ignored on both sides — faithful copies
    // of odd containers carry them.
    let after = match probe(ffprobe, &temp) {
        Ok(after) => after,
        Err(err) => {
            let _ = std::fs::remove_file(&temp);
            return Err(err.context("verifying the muxed file; original untouched"));
        }
    };
    let sane = after.subtitle >= 1
        && after.video == before.video
        && after.audio == before.audio
        && (after.duration - before.duration).abs() <= 1.0 + before.duration * 0.01
        && media_db::captions::recorded_hash(&after.encoder) == Some(hash.as_str());
    if !sane {
        let _ = std::fs::remove_file(&temp);
        bail!(
            "mux verification failed (video {}->{}, audio {}->{}, subs {}, \
             duration {:.1}->{:.1}, record {}); original untouched",
            before.video, after.video, before.audio, after.audio, after.subtitle,
            before.duration, after.duration,
            if media_db::captions::recorded_hash(&after.encoder).is_some() { "ok" } else { "missing" }
        );
    }

    std::fs::rename(&temp, media)
        .with_context(|| format!("replacing {}", media.display()))?;
    Ok(if replace { Outcome::Replaced(hash) } else { Outcome::Embedded(hash) })
}

pub enum ExtractOutcome {
    Skipped(&'static str),
    /// Dry run: the track that would be extracted.
    WouldExtract(media_db::subtitles::Track),
    Extracted(media_db::subtitles::Track),
}

/// Extract the best embedded text subtitle track to `{stem}.srt` when no
/// such sidecar exists. Never overwrites: a sidecar that is already there,
/// hand-made or not, is left alone. Forced tracks lose to full captions,
/// SDH preferred (see media_db::subtitles).
pub fn extract_if_missing(ffmpeg: &str, ffprobe: &str, media: &Path, dry_run: bool) -> Result<ExtractOutcome> {
    let srt = media.with_extension("srt");
    if srt.exists() {
        return Ok(ExtractOutcome::Skipped("sidecar exists"));
    }
    let output = Command::new(ffprobe)
        .args(media_db::subtitles::ffprobe_args())
        .arg(media)
        .output()
        .with_context(|| format!("running {ffprobe}"))?;
    if !output.status.success() {
        bail!("ffprobe failed: {}", String::from_utf8_lossy(&output.stderr).trim());
    }
    let tracks = media_db::subtitles::parse_ffprobe(&String::from_utf8_lossy(&output.stdout));
    let Some(track) = media_db::subtitles::best_text_track(&tracks).cloned() else {
        return Ok(ExtractOutcome::Skipped("no text subtitle track"));
    };
    if dry_run {
        return Ok(ExtractOutcome::WouldExtract(track));
    }

    let dir = media.parent().unwrap_or_else(|| Path::new("."));
    let stem = media.file_stem().unwrap_or_default().to_string_lossy();
    let temp = crate::remux::temp_beside(dir, &stem, "subtitles-extract", "srt");
    let status = Command::new(ffmpeg)
        .args(["-v", "error", "-nostdin", "-i"])
        .arg(media)
        .args(["-map", &format!("0:s:{}", track.ordinal), "-f", "srt", "-y"])
        .arg(&temp)
        .status()
        .with_context(|| format!("running {ffmpeg}"))?;
    if !status.success() {
        let _ = std::fs::remove_file(&temp);
        bail!("ffmpeg extraction failed ({status})");
    }
    // A track can exist yet carry no cues (empty placeholder tracks are
    // not rare); don't leave an empty sidecar that would mask the fallback.
    let text = std::fs::read_to_string(&temp).unwrap_or_default();
    if !text.contains("-->") {
        let _ = std::fs::remove_file(&temp);
        bail!("extracted track {} has no cues", track.ordinal);
    }
    std::fs::rename(&temp, &srt).with_context(|| format!("placing {}", srt.display()))?;
    Ok(ExtractOutcome::Extracted(track))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_reads_streams_and_the_record() {
        let p = parse_probe(
            "codec_name=h264\ncodec_type=video\ncodec_name=aac\ncodec_type=audio\n\
             codec_name=mov_text\ncodec_type=subtitle\nduration=3.0\n\
             TAG:title=Scene.Release\nTAG:encoder=media-enrich; captions=srt:sha256:ab\n",
        );
        assert_eq!((p.video, p.audio, p.subtitle), (1, 1, 1));
        assert_eq!(p.encoder, "media-enrich; captions=srt:sha256:ab");
        assert_eq!(media_db::captions::recorded_hash(&p.encoder), None, "too short to be ours");
    }

    #[test]
    fn cues_match_ignores_what_a_mov_text_round_trip_changes() {
        let sidecar = "1\n00:00:01,000 --> 00:00:02,500\nHello there,\n<i>General</i> Kenobi.\n\n\
                       2\n00:00:03,000 --> 00:00:04,000\n{\\an8}[door slams]\n\n";
        let extracted = "1\r\n00:00:01,000 --> 00:00:02,500\r\nHello there, General Kenobi.\r\n\r\n\
                         2\r\n00:00:03,000 --> 00:00:04,000\r\n[door slams]\r\n\r\n";
        assert!(cues_match(sidecar, extracted));
        let retimed = extracted.replace("00:00:03,000", "00:00:03,200");
        assert!(!cues_match(sidecar, &retimed), "timing is content");
        let reworded = extracted.replace("Kenobi", "Grievous");
        assert!(!cues_match(sidecar, &reworded), "words are content");
        assert!(!cues_match(sidecar, "1\n00:00:01,000 --> 00:00:02,500\nHello there, General Kenobi.\n"), "a missing cue is content");
        assert_eq!(parse_timestamp("00:01:02,5"), Some(62_500));
        assert_eq!(parse_timestamp("01:02.250 X1:0"), Some(62_250));
    }

    #[test]
    fn probe_finds_the_first_text_track_among_subtitle_streams() {
        let p = parse_probe(
            "codec_name=h264\ncodec_type=video\ncodec_name=dvd_subtitle\ncodec_type=subtitle\n\
             codec_name=mov_text\ncodec_type=subtitle\nduration=3.0\n",
        );
        assert_eq!((p.subtitle, p.first_text_ordinal), (1, Some(1)));
    }

    #[test]
    fn hash_is_of_the_decoded_text() {
        let a = sidecar_hash("1\n00:00:01,000 --> 00:00:02,000\nhi\n");
        assert_eq!(a.len(), 64);
        assert_eq!(a, sidecar_hash("1\n00:00:01,000 --> 00:00:02,000\nhi\n"));
        assert_ne!(a, sidecar_hash("1\n00:00:01,500 --> 00:00:02,000\nhi\n"));
        assert!(media_db::captions::recorded_hash(&media_db::captions::tag(&a)) == Some(a.as_str()));
    }
}
