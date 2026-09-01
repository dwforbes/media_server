//! Remux MKV files to MP4 without touching the video.
//!
//! Matroska trips up browsers (Firefox won't range-stream it, Safari won't
//! open it) while the streams inside are usually fine. Equivalent to
//!   ffmpeg -i in.mkv -map 0 -c copy -c:s mov_text -tag:v hvc1 -movflags +faststart out.mp4
//! with one addition: Dolby Digital (AC-3 / E-AC-3) audio, which Chrome and
//! Firefox cannot decode in any container, gets a stereo AAC twin inserted
//! *ahead* of it as the default track. The original track is kept for
//! players and receivers that prefer it. Nothing is ever re-encoded except
//! that added audio track.
//!
//! Like subtitle embedding this replaces a whole media file, so the same
//! discipline applies: strict preconditions (only codecs MP4 carries
//! natively, no Dolby Vision), mux into a temp file in the same directory,
//! ffprobe verification, then rename into place and remove the original.
//! Sidecars (.nfo, -poster.jpg, .srt) share the stem and remain valid.
//!
//! Bitmap subtitles (PGS/VobSub) cannot live in MP4 and normally
//! disqualify a file — unless a usable same-stem .srt sidecar exists, in
//! which case the bitmap tracks are dropped and the sidecar takes over.
//! Whenever no text subtitle track survives and a sidecar exists, the
//! remux embeds it as the mov_text track in the same pass (the separate
//! embed step would otherwise rewrite the new .mp4 a second time).

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use media_db::textenc::decode_subtitle_text;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamKind {
    Video,
    Audio,
    Subtitle,
    /// Attachments (fonts, cover art) and data tracks: dropped, MP4 has no
    /// place for them.
    Other,
}

#[derive(Debug, Clone)]
pub struct Stream {
    /// Absolute stream index within the file (ffmpeg's 0:N).
    pub index: usize,
    pub kind: StreamKind,
    pub codec: String,
    pub language: Option<String>,
    /// Carries a Dolby Vision configuration record.
    pub dovi: bool,
}

/// Stream census plus duration, as ffprobe reports them.
#[derive(Debug, Default)]
pub struct Probe {
    pub streams: Vec<Stream>,
    pub duration: f64,
}

pub fn probe(ffprobe: &str, path: &Path) -> Result<Probe> {
    let output = Command::new(ffprobe)
        .args([
            "-v", "error",
            "-show_entries",
            "stream=index,codec_type,codec_name:stream_tags=language:stream_side_data=side_data_type:format=duration",
            // Wrapped form: the [STREAM]/[SIDE_DATA] markers delimit streams.
            "-of", "default",
        ])
        .arg(path)
        .output()
        .with_context(|| format!("running {ffprobe}"))?;
    if !output.status.success() {
        bail!("ffprobe failed: {}", String::from_utf8_lossy(&output.stderr).trim());
    }
    Ok(parse_probe(&String::from_utf8_lossy(&output.stdout)))
}

/// ffprobe's default writer emits one [STREAM] block per stream with its
/// side-data blocks nested inside; the format block comes last.
fn parse_probe(text: &str) -> Probe {
    let mut probe = Probe::default();
    let mut current: Option<Stream> = None;
    for line in text.lines() {
        match line {
            "[STREAM]" => {
                current = Some(Stream {
                    index: 0,
                    kind: StreamKind::Other,
                    codec: String::new(),
                    language: None,
                    dovi: false,
                })
            }
            "[/STREAM]" => probe.streams.extend(current.take()),
            _ => {
                if let Some(d) = line.strip_prefix("duration=") {
                    probe.duration = d.parse().unwrap_or(0.0);
                    continue;
                }
                let Some(s) = current.as_mut() else { continue };
                if let Some(v) = line.strip_prefix("index=") {
                    s.index = v.parse().unwrap_or(0);
                } else if let Some(v) = line.strip_prefix("codec_type=") {
                    s.kind = match v {
                        "video" => StreamKind::Video,
                        "audio" => StreamKind::Audio,
                        "subtitle" => StreamKind::Subtitle,
                        _ => StreamKind::Other,
                    };
                } else if let Some(v) = line.strip_prefix("codec_name=") {
                    s.codec = v.to_string();
                } else if let Some(v) = line.strip_prefix("TAG:language=") {
                    s.language = Some(v.to_string()).filter(|l| !l.is_empty() && l != "und");
                } else if let Some(v) = line.strip_prefix("side_data_type=") {
                    if v.to_ascii_lowercase().contains("dovi") {
                        s.dovi = true;
                    }
                }
            }
        }
    }
    probe
}

/// Video codecs MP4 carries and browsers can (in some combination) play.
const VIDEO_OK: &[&str] = &["h264", "hevc", "av1"];
/// Audio codecs copied as-is. FLAC, Vorbis, DTS, TrueHD and PCM are either
/// experimental in MP4 or not carried at all — files with those are skipped.
const AUDIO_COPY: &[&str] = &["aac", "mp3", "opus", "alac", "ac3", "eac3"];
/// Audio codecs that also get a browser-playable AAC twin.
const AUDIO_TWIN: &[&str] = &["ac3", "eac3"];
const TEXT_SUBS: &[&str] = &["subrip", "srt", "ass", "ssa", "mov_text", "webvtt", "text", "subviewer"];
const BITMAP_SUBS: &[&str] = &["hdmv_pgs_subtitle", "dvd_subtitle", "dvb_subtitle", "xsub"];

/// What the remux will do, decided from the probe (and whether a usable
/// .srt sidecar sits beside the file).
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Plan {
    pub video: Vec<usize>,
    /// (input index, gets an AAC twin)
    pub audio: Vec<(usize, bool)>,
    pub subtitles: Vec<usize>,
    /// Mux the .srt sidecar in as the mov_text subtitle track.
    pub embed_srt: bool,
    pub hevc: bool,
    /// Human-readable caveats worth a line in the log (styling lost, ...).
    pub notes: Vec<String>,
}

impl Plan {
    pub fn twins(&self) -> usize {
        self.audio.iter().filter(|(_, twin)| *twin).count()
    }
}

/// Decide whether the file is a clean remux candidate. Err carries the
/// reason it is not. `srt_sidecar` says a usable same-stem .srt exists:
/// with one, bitmap subtitle tracks are dropped instead of disqualifying
/// the file, and it is embedded whenever no text track would survive.
pub fn plan(probe: &Probe, srt_sidecar: bool) -> std::result::Result<Plan, String> {
    let mut plan = Plan::default();
    for s in &probe.streams {
        match s.kind {
            StreamKind::Video => {
                if s.dovi {
                    return Err("Dolby Vision (MP4 signalling is unreliable)".into());
                }
                if !VIDEO_OK.contains(&s.codec.as_str()) {
                    return Err(format!("video codec {} is not MP4/browser material", s.codec));
                }
                plan.hevc |= s.codec == "hevc";
                plan.video.push(s.index);
            }
            StreamKind::Audio => {
                if !AUDIO_COPY.contains(&s.codec.as_str()) {
                    return Err(format!("audio codec {} is not MP4-safe", s.codec));
                }
                plan.audio.push((s.index, AUDIO_TWIN.contains(&s.codec.as_str())));
            }
            StreamKind::Subtitle => {
                if BITMAP_SUBS.contains(&s.codec.as_str()) {
                    if !srt_sidecar {
                        return Err(format!("bitmap subtitles ({}) cannot live in MP4", s.codec));
                    }
                    plan.notes
                        .push(format!("{} bitmap subtitles dropped, .srt sidecar covers them", s.codec));
                    continue;
                }
                if !TEXT_SUBS.contains(&s.codec.as_str()) {
                    return Err(format!("subtitle codec {} unknown", s.codec));
                }
                if s.codec == "ass" || s.codec == "ssa" {
                    plan.notes.push("ASS subtitle styling reduced to plain mov_text".into());
                }
                plan.subtitles.push(s.index);
            }
            StreamKind::Other => {}
        }
    }
    if plan.video.is_empty() {
        return Err("no video stream".into());
    }
    if plan.audio.is_empty() {
        return Err("no audio stream".into());
    }
    if srt_sidecar && plan.subtitles.is_empty() {
        plan.embed_srt = true;
        plan.notes.push(".srt sidecar embedded as the subtitle track".into());
    }
    plan.notes.dedup();
    Ok(plan)
}

/// The ffmpeg invocation for a plan. Output streams are ordered video,
/// audio (each AAC twin immediately before its original), subtitles.
/// `srt` is the sidecar to mux in when the plan says embed_srt — a second
/// input, mapped as the only subtitle track.
pub fn ffmpeg_args(plan: &Plan, input: &Path, srt: Option<&Path>, output: &Path) -> Vec<std::ffi::OsString> {
    let mut args: Vec<std::ffi::OsString> = Vec::new();
    let mut push = |a: &str| args.push(a.into());
    for a in ["-v", "error", "-nostdin", "-y", "-i"] {
        push(a);
    }
    args.push(input.into());
    if let Some(srt) = srt {
        args.push("-i".into());
        args.push(srt.into());
    }
    let mut push = |a: String| args.push(a.into());
    for idx in &plan.video {
        push("-map".into());
        push(format!("0:{idx}"));
    }
    // Map first, then codec/disposition options by output audio ordinal.
    let mut audio_out = 0usize;
    let mut twin_opts: Vec<String> = Vec::new();
    for (idx, twin) in &plan.audio {
        if *twin {
            push("-map".into());
            push(format!("0:{idx}"));
            twin_opts.extend([
                format!("-c:a:{audio_out}"), "aac".into(),
                format!("-b:a:{audio_out}"), "192k".into(),
                format!("-ac:a:{audio_out}"), "2".into(),
                // MP4 has no per-track title; the handler name is what
                // players list in their audio-track menu.
                format!("-metadata:s:a:{audio_out}"), "handler_name=Stereo (AAC)".into(),
                format!("-disposition:a:{audio_out}"), "default".into(),
            ]);
            audio_out += 1;
        }
        push("-map".into());
        push(format!("0:{idx}"));
        // Originals: default only when nothing precedes them.
        twin_opts.extend([
            format!("-disposition:a:{audio_out}"),
            if audio_out == 0 { "default".into() } else { "0".into() },
        ]);
        audio_out += 1;
    }
    for idx in &plan.subtitles {
        push("-map".into());
        push(format!("0:{idx}"));
    }
    if srt.is_some() {
        push("-map".into());
        push("1:0".into());
    }
    for a in ["-c", "copy", "-c:s", "mov_text"] {
        push(a.into());
    }
    if srt.is_some() {
        // Same convention as the embed step: the sidecar collection is
        // English, and an untagged track shows as "Unknown" in menus.
        push("-metadata:s:s:0".into());
        push("language=eng".into());
    }
    for a in twin_opts {
        push(a);
    }
    if plan.hevc {
        // ffmpeg's default hev1 tag is unplayable in QuickTime/Safari.
        push("-tag:v".into());
        push("hvc1".into());
    }
    // faststart moves the index to the front so browsers can begin playing
    // after one request; the second pass it costs is fine for a one-off.
    push("-movflags".into());
    push("+faststart".into());
    args.push(output.into());
    args
}

pub enum Outcome {
    Skipped(String),
    /// Dry run: what a real run would do.
    WouldRemux(Plan),
    Remuxed(Plan),
}

/// A fresh temp path beside the media file: `.{stem}.{tag}-{pid}.{ext}`,
/// dot-prefixed so the scanner ignores it and pid-suffixed so no other
/// process can ever write to the same name. Leftovers from earlier runs
/// (`.{stem}.{tag}*`, e.g. after a crash) are removed first — with the run
/// lock held nothing else can be using them.
pub(crate) fn temp_beside(dir: &Path, stem: &str, tag: &str, ext: &str) -> PathBuf {
    let stale_prefix = format!(".{stem}.{tag}");
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            if entry.file_name().to_string_lossy().starts_with(&stale_prefix) {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
    dir.join(format!(".{stem}.{tag}-{}.{ext}", std::process::id()))
}

fn is_mkv(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("mkv"))
}

/// Remux one file if it qualifies. With `dry_run`, probe and plan only.
pub fn remux_if_applicable(ffmpeg: &str, ffprobe: &str, media: &Path, dry_run: bool) -> Result<Outcome> {
    if !is_mkv(media) {
        return Ok(Outcome::Skipped("not mkv".into()));
    }
    let target = media.with_extension("mp4");
    if target.exists() {
        return Ok(Outcome::Skipped("an .mp4 with this name already exists".into()));
    }
    // A usable .srt sidecar lets bitmap subtitles be dropped rather than
    // disqualify the file, and is embedded when no text track survives.
    // Usable means the same bar the embed step sets: non-empty and in a
    // recognizable encoding (mov_text needs clean UTF-8; see subtitles.rs).
    let srt = media.with_extension("srt");
    let srt_bytes = std::fs::read(&srt).unwrap_or_default();
    let srt_text = if srt_bytes.is_empty() { None } else { decode_subtitle_text(&srt_bytes) };

    let before = probe(ffprobe, media)?;
    let plan = match plan(&before, srt_text.is_some()) {
        Ok(plan) => plan,
        Err(why) => return Ok(Outcome::Skipped(why)),
    };
    if dry_run {
        return Ok(Outcome::WouldRemux(plan));
    }

    let dir = media.parent().unwrap_or_else(|| Path::new("."));
    let stem = media.file_stem().unwrap_or_default().to_string_lossy();
    let temp = temp_beside(dir, &stem, "remux-tmp", "mp4");
    // Mux from a UTF-8 temp copy when the sidecar needed decoding; the
    // original .srt is never modified.
    let srt_input: Option<PathBuf> = match (plan.embed_srt, srt_text) {
        (true, Some(text)) if text.as_bytes() != srt_bytes.as_slice() => {
            let converted = temp.with_extension("srt");
            std::fs::write(&converted, &text)
                .with_context(|| format!("writing {}", converted.display()))?;
            Some(converted)
        }
        (true, _) => Some(srt.clone()),
        (false, _) => None,
    };
    let status = Command::new(ffmpeg)
        .args(ffmpeg_args(&plan, media, srt_input.as_deref(), &temp))
        .status()
        .with_context(|| format!("running {ffmpeg}"))?;
    if let Some(converted) = srt_input.filter(|p| *p != srt) {
        let _ = std::fs::remove_file(converted);
    }
    if !status.success() {
        let _ = std::fs::remove_file(&temp);
        bail!("ffmpeg remux failed ({status})");
    }
    if let Ok(f) = std::fs::File::open(&temp) {
        let _ = f.sync_all();
    }

    // Verify before touching the original: every video stream, every audio
    // stream plus its twins, every text subtitle, duration unchanged.
    let after = match probe(ffprobe, &temp) {
        Ok(after) => after,
        Err(err) => {
            let _ = std::fs::remove_file(&temp);
            return Err(err.context("verifying the remuxed file; original untouched"));
        }
    };
    let count = |kind: StreamKind| after.streams.iter().filter(|s| s.kind == kind).count();
    let want_audio = plan.audio.len() + plan.twins();
    let want_subs = plan.subtitles.len() + plan.embed_srt as usize;
    let sane = count(StreamKind::Video) == plan.video.len()
        && count(StreamKind::Audio) == want_audio
        && count(StreamKind::Subtitle) == want_subs
        && (after.duration - before.duration).abs() <= 1.0 + before.duration * 0.01;
    if !sane {
        let _ = std::fs::remove_file(&temp);
        bail!(
            "remux verification failed (video {}/{}, audio {}/{}, subs {}/{}, duration {:.1}->{:.1}); original untouched",
            count(StreamKind::Video), plan.video.len(),
            count(StreamKind::Audio), want_audio,
            count(StreamKind::Subtitle), want_subs,
            before.duration, after.duration
        );
    }

    // Keep the original's mtime: date-sorted views elsewhere shouldn't see
    // a "new" file (the catalog carries added_at across the rename itself).
    if let Ok(modified) = std::fs::metadata(media).and_then(|m| m.modified()) {
        if let Ok(f) = std::fs::OpenOptions::new().write(true).open(&temp) {
            let _ = f.set_modified(modified);
        }
    }
    std::fs::rename(&temp, &target).with_context(|| format!("placing {}", target.display()))?;
    if let Err(err) = std::fs::remove_file(media) {
        // Both files now exist; the catalog will merge them as renditions
        // until the .mkv goes. Loud, but not fatal.
        eprintln!("{}: remuxed, but the original could not be removed: {err}", media.display());
    }
    Ok(Outcome::Remuxed(plan))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(index: usize, kind: StreamKind, codec: &str) -> Stream {
        Stream { index, kind, codec: codec.into(), language: None, dovi: false }
    }

    #[test]
    fn parses_ffprobe_default_output_with_side_data() {
        let text = "[STREAM]\nindex=0\ncodec_name=hevc\ncodec_type=video\nTAG:language=und\n[SIDE_DATA]\nside_data_type=DOVI configuration record\n[/SIDE_DATA]\n[/STREAM]\n[STREAM]\nindex=1\ncodec_name=eac3\ncodec_type=audio\nTAG:language=eng\n[/STREAM]\n[FORMAT]\nduration=5400.123000\n[/FORMAT]\n";
        let p = parse_probe(text);
        assert_eq!(p.streams.len(), 2);
        assert!(p.streams[0].dovi);
        assert_eq!(p.streams[0].language, None);
        assert_eq!(p.streams[1].language.as_deref(), Some("eng"));
        assert_eq!(p.streams[1].kind, StreamKind::Audio);
        assert!((p.duration - 5400.123).abs() < 1e-6);
    }

    #[test]
    fn plan_copies_aac_and_twins_dolby_digital() {
        let p = Probe {
            streams: vec![
                s(0, StreamKind::Video, "h264"),
                s(1, StreamKind::Audio, "eac3"),
                s(2, StreamKind::Audio, "aac"),
                s(3, StreamKind::Subtitle, "subrip"),
                s(4, StreamKind::Other, "ttf"),
            ],
            duration: 1.0,
        };
        let plan = plan(&p, false).unwrap();
        assert_eq!(plan.video, vec![0]);
        assert_eq!(plan.audio, vec![(1, true), (2, false)]);
        assert_eq!(plan.subtitles, vec![3]);
        assert_eq!(plan.twins(), 1);
        assert!(!plan.hevc);

        let args = ffmpeg_args(&plan, Path::new("in.mkv"), None, Path::new("out.mp4"));
        let args: Vec<String> = args.iter().map(|a| a.to_string_lossy().into_owned()).collect();
        let joined = args.join(" ");
        // Twin mapped before its original, then the aac track, then subs.
        assert!(joined.contains("-map 0:0 -map 0:1 -map 0:1 -map 0:2 -map 0:3"), "{joined}");
        assert!(joined.contains("-c:a:0 aac"), "{joined}");
        assert!(joined.contains("-disposition:a:0 default"), "{joined}");
        assert!(joined.contains("-disposition:a:1 0"), "{joined}");
        assert!(joined.contains("-disposition:a:2 0"), "{joined}");
        assert!(joined.contains("-c:s mov_text"), "{joined}");
        assert!(!joined.contains("hvc1"), "{joined}");
        assert!(joined.ends_with("-movflags +faststart out.mp4"), "{joined}");
    }

    #[test]
    fn plan_refuses_what_mp4_cannot_carry() {
        let refuse = |streams: Vec<Stream>| plan(&Probe { streams, duration: 1.0 }, false).unwrap_err();
        assert!(refuse(vec![s(0, StreamKind::Video, "h264"), s(1, StreamKind::Audio, "dts")]).contains("dts"));
        assert!(refuse(vec![s(0, StreamKind::Video, "h264"), s(1, StreamKind::Audio, "flac")]).contains("flac"));
        assert!(refuse(vec![
            s(0, StreamKind::Video, "h264"),
            s(1, StreamKind::Audio, "aac"),
            s(2, StreamKind::Subtitle, "hdmv_pgs_subtitle"),
        ])
        .contains("bitmap"));
        assert!(refuse(vec![s(0, StreamKind::Video, "vp9"), s(1, StreamKind::Audio, "aac")]).contains("vp9"));
        let mut dv = s(0, StreamKind::Video, "hevc");
        dv.dovi = true;
        assert!(refuse(vec![dv, s(1, StreamKind::Audio, "aac")]).contains("Dolby Vision"));
        assert!(refuse(vec![s(0, StreamKind::Audio, "aac")]).contains("no video"));
    }

    #[test]
    fn srt_sidecar_forgives_bitmap_subs_and_gets_embedded() {
        let p = Probe {
            streams: vec![
                s(0, StreamKind::Video, "h264"),
                s(1, StreamKind::Audio, "aac"),
                s(2, StreamKind::Subtitle, "hdmv_pgs_subtitle"),
            ],
            duration: 1.0,
        };
        let plan = plan(&p, true).unwrap();
        assert!(plan.subtitles.is_empty());
        assert!(plan.embed_srt);
        assert!(plan.notes.iter().any(|n| n.contains("bitmap subtitles dropped")), "{:?}", plan.notes);

        let args = ffmpeg_args(&plan, Path::new("in.mkv"), Some(Path::new("in.srt")), Path::new("out.mp4"));
        let joined = args.iter().map(|a| a.to_string_lossy().into_owned()).collect::<Vec<_>>().join(" ");
        assert!(joined.contains("-i in.mkv -i in.srt"), "{joined}");
        // The bitmap track (0:2) is not mapped; the sidecar is the one sub.
        assert!(joined.contains("-map 0:0 -map 0:1 -map 1:0"), "{joined}");
        assert!(!joined.contains("-map 0:2"), "{joined}");
        assert!(joined.contains("-c:s mov_text"), "{joined}");
        assert!(joined.contains("-metadata:s:s:0 language=eng"), "{joined}");
    }

    #[test]
    fn internal_text_subs_win_over_the_sidecar() {
        // Bitmap dropped, subrip kept, nothing embedded on top of it.
        let p = Probe {
            streams: vec![
                s(0, StreamKind::Video, "h264"),
                s(1, StreamKind::Audio, "aac"),
                s(2, StreamKind::Subtitle, "hdmv_pgs_subtitle"),
                s(3, StreamKind::Subtitle, "subrip"),
            ],
            duration: 1.0,
        };
        let plan = plan(&p, true).unwrap();
        assert_eq!(plan.subtitles, vec![3]);
        assert!(!plan.embed_srt);
    }

    #[test]
    fn sidecar_is_embedded_when_the_mkv_has_no_subs() {
        let p = Probe {
            streams: vec![s(0, StreamKind::Video, "h264"), s(1, StreamKind::Audio, "aac")],
            duration: 1.0,
        };
        assert!(plan(&p, true).unwrap().embed_srt);
        assert!(!plan(&p, false).unwrap().embed_srt);
    }

    #[test]
    fn hevc_gets_the_apple_tag_and_a_lone_original_stays_default() {
        let p = Probe {
            streams: vec![s(0, StreamKind::Video, "hevc"), s(1, StreamKind::Audio, "aac")],
            duration: 1.0,
        };
        let plan = plan(&p, false).unwrap();
        assert!(plan.hevc);
        let args = ffmpeg_args(&plan, Path::new("in.mkv"), None, Path::new("out.mp4"));
        let joined = args.iter().map(|a| a.to_string_lossy().into_owned()).collect::<Vec<_>>().join(" ");
        assert!(joined.contains("-tag:v hvc1"), "{joined}");
        assert!(joined.contains("-disposition:a:0 default"), "{joined}");
    }
}
