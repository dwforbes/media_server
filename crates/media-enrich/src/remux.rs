//! Remux MKV files to MP4 without touching the video.
//!
//! Matroska trips up browsers (Firefox won't range-stream it, Safari won't
//! open it) while the streams inside are usually fine. Equivalent to
//!   ffmpeg -i in.mkv -map 0 -c copy -c:s mov_text -tag:v hvc1 -movflags +faststart out.mp4
//! with one addition: a stereo AAC twin inserted *ahead* of an audio track
//! as the default, for Dolby Digital (AC-3 / E-AC-3, which Chrome and
//! Firefox cannot decode in any container) and for a default track with
//! more than two channels (Firefox skips a 5.1 AAC track and plays
//! whatever comes next, a commentary as likely as not). The original
//! track is kept for players and receivers that prefer it. Nothing is
//! ever re-encoded except that added audio track.
//!
//! The same pass looks at .mp4 files, since a rip can arrive as one:
//! when its default track would earn a twin, the file is rewritten in
//! place with the twin ahead. An .mp4 whose default track already plays
//! in browsers is left alone.
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
    /// Audio: channel count as ffprobe reports it (0 when unknown).
    pub channels: usize,
    /// The default disposition: what plays when nobody chooses.
    pub default: bool,
    /// Title and handler name, where a twin of ours says what it is.
    pub label: String,
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
            "stream=index,codec_type,codec_name,channels:stream_tags=language,title,handler_name:\
             stream_disposition=default:stream_side_data=side_data_type:format=duration",
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
                    channels: 0,
                    default: false,
                    label: String::new(),
                })
            }
            "[/STREAM]" => probe.streams.extend(current.take()),
            _ => {
                let Some((key, v)) = line.split_once('=') else { continue };
                if key == "duration" {
                    probe.duration = v.parse().unwrap_or(0.0);
                    continue;
                }
                let Some(s) = current.as_mut() else { continue };
                // Matroska reports its tag names in capitals.
                match key.to_ascii_lowercase().as_str() {
                    "index" => s.index = v.parse().unwrap_or(0),
                    "codec_type" => {
                        s.kind = match v {
                            "video" => StreamKind::Video,
                            "audio" => StreamKind::Audio,
                            "subtitle" => StreamKind::Subtitle,
                            _ => StreamKind::Other,
                        }
                    }
                    "codec_name" => s.codec = v.to_string(),
                    "channels" => s.channels = v.parse().unwrap_or(0),
                    "disposition:default" => s.default = v == "1",
                    "tag:language" => s.language = Some(v.to_string()).filter(|l| !l.is_empty() && l != "und"),
                    "tag:title" | "tag:handler_name" => {
                        s.label.push_str(v);
                        s.label.push(' ');
                    }
                    "side_data_type" => s.dovi |= v.to_ascii_lowercase().contains("dovi"),
                    _ => {}
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
/// Audio codecs that always get a browser-playable AAC twin. A track of
/// any codec gets one when it is the default and wider than stereo.
const AUDIO_TWIN: &[&str] = &["ac3", "eac3"];

/// A channel count as a person would say it.
pub fn layout_name(channels: usize) -> String {
    match channels {
        0 => String::new(),
        1 => "mono".into(),
        2 => "stereo".into(),
        6 => "5.1".into(),
        8 => "7.1".into(),
        n => format!("{n}ch"),
    }
}
const TEXT_SUBS: &[&str] = &["subrip", "srt", "ass", "ssa", "mov_text", "webvtt", "text", "subviewer"];
const BITMAP_SUBS: &[&str] = &["hdmv_pgs_subtitle", "dvd_subtitle", "dvb_subtitle", "xsub"];

/// What the remux will do, decided from the probe (and whether a usable
/// .srt sidecar sits beside the file).
#[derive(Debug, Default, PartialEq)]
pub struct Plan {
    pub video: Vec<usize>,
    /// (input index, gets an AAC twin)
    pub audio: Vec<(usize, bool)>,
    /// With loudness normalization on: each twin's measured level (of
    /// its stereo downmix) and the gain it is encoded with, if any — a
    /// twin is an encode already, so raising a quiet one costs nothing.
    pub twin_levels: Vec<TwinLevel>,
    /// What each twin stands in for ("eac3 5.1", "aac 5.1"), in order.
    pub twin_reasons: Vec<String>,
    /// The track that plays by default is among the twinned: the case
    /// that matters for browsers, and the only one worth rewriting an
    /// .mp4 for.
    pub default_twin: bool,
    pub subtitles: Vec<usize>,
    /// Mux the .srt sidecar in as the mov_text subtitle track.
    pub embed_srt: bool,
    pub hevc: bool,
    /// Human-readable caveats worth a line in the log (styling lost, ...).
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TwinLevel {
    /// Input index of the track the twin is made from.
    pub index: usize,
    pub measured: crate::loudness::Measurement,
    pub gain: Option<f64>,
}

impl Plan {
    pub fn twins(&self) -> usize {
        self.audio.iter().filter(|(_, twin)| *twin).count()
    }

    fn twin_gain(&self, index: usize) -> Option<f64> {
        self.twin_levels.iter().find(|t| t.index == index).and_then(|t| t.gain)
    }
}

/// Decide whether the file is a clean remux candidate. Err carries the
/// reason it is not. `srt_sidecar` says a usable same-stem .srt exists:
/// with one, bitmap subtitle tracks are dropped instead of disqualifying
/// the file, and it is embedded whenever no text track would survive.
///
/// Twins: every Dolby track, and the track that plays by default (the
/// default-flagged one, else the first) when it is wider than stereo —
/// unless a twin of ours already sits ahead of it, which is how a file
/// this pass made earlier is recognised and left alone.
pub fn plan(probe: &Probe, srt_sidecar: bool) -> std::result::Result<Plan, String> {
    let mut plan = Plan::default();
    let audio = || probe.streams.iter().filter(|s| s.kind == StreamKind::Audio);
    let playing = audio().find(|s| s.default).or_else(|| audio().next()).map(|s| s.index);
    let mut previous: Option<&Stream> = None;
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
                let is_playing = Some(s.index) == playing;
                let twinned = previous.is_some_and(|p| p.label.contains(crate::loudness::TWIN_LABEL));
                let twin = !twinned && (AUDIO_TWIN.contains(&s.codec.as_str()) || (is_playing && s.channels > 2));
                if twin {
                    plan.twin_reasons.push(format!("{} {}", s.codec, layout_name(s.channels)).trim_end().to_string());
                    plan.default_twin |= is_playing;
                }
                plan.audio.push((s.index, twin));
                previous = Some(s);
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
/// `captions_hash` is the sidecar's hash to record in the file when it is
/// embedded (media_db::captions), so a later correction of the sidecar
/// can be recognised and re-embedded.
pub fn ffmpeg_args(
    plan: &Plan,
    input: &Path,
    srt: Option<&Path>,
    captions_hash: Option<&str>,
    output: &Path,
) -> Vec<std::ffi::OsString> {
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
            // MP4 has no per-track title; the handler name is what
            // players list in their audio-track menu. A raised twin says
            // so there (which is also how the loudness step knows).
            let gain = plan.twin_gain(*idx);
            let name = match gain {
                Some(g) => crate::loudness::label(g, true),
                None => crate::loudness::TWIN_LABEL.to_string(),
            };
            twin_opts.extend([
                format!("-c:a:{audio_out}"), "aac".into(),
                format!("-b:a:{audio_out}"), "192k".into(),
                format!("-ac:a:{audio_out}"), "2".into(),
                format!("-metadata:s:a:{audio_out}"), format!("handler_name={name}"),
                format!("-disposition:a:{audio_out}"), if audio_out == 0 { "default".into() } else { "0".into() },
            ]);
            if let Some(g) = gain {
                // A linear gain commutes with the downmix that follows.
                twin_opts.extend([format!("-filter:a:{audio_out}"), format!("volume={g:.1}dB")]);
            }
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
        if let Some(hash) = captions_hash {
            push("-metadata".into());
            push(format!("encoding_tool={}", media_db::captions::tag(hash)));
        }
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
    /// An .mkv left alone, and why.
    Skipped(String),
    /// An .mp4 whose default track already plays in browsers.
    Fine,
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

fn ext_is(path: &Path, ext: &str) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case(ext))
}

fn is_mkv(path: &Path) -> bool {
    ext_is(path, "mkv")
}

/// Remux one file if it qualifies: an .mkv to .mp4, or an .mp4 in place
/// when its default track needs a stereo twin. With `dry_run`, probe and
/// plan only. With a loudness policy, each stereo twin's downmix is
/// measured first and a quiet one is encoded with the gain the policy
/// allows.
pub fn remux_if_applicable(
    ffmpeg: &str,
    ffprobe: &str,
    media: &Path,
    dry_run: bool,
    loudness: Option<&crate::loudness::Policy>,
) -> Result<Outcome> {
    let mkv = is_mkv(media);
    if !mkv && !ext_is(media, "mp4") {
        return Ok(Outcome::Skipped("not mkv or mp4".into()));
    }
    let target = if mkv { media.with_extension("mp4") } else { media.to_path_buf() };
    if mkv && target.exists() {
        return Ok(Outcome::Skipped("an .mp4 with this name already exists".into()));
    }
    // A usable .srt sidecar lets bitmap subtitles be dropped rather than
    // disqualify the file, and is embedded when no text track survives.
    // Usable means the same bar the embed step sets: non-empty and in a
    // recognizable encoding (mov_text needs clean UTF-8; see subtitles.rs).
    let srt = media.with_extension("srt");
    let srt_bytes = media_db::sidecar::read_capped(&srt, media_db::sidecar::MAX_TEXT).unwrap_or_default();
    let srt_text = if srt_bytes.is_empty() { None } else { decode_subtitle_text(&srt_bytes) };

    let before = probe(ffprobe, media)?;
    let mut plan = match plan(&before, srt_text.is_some()) {
        Ok(plan) => plan,
        Err(why) if mkv => return Ok(Outcome::Skipped(why)),
        Err(_) => return Ok(Outcome::Fine),
    };
    // An .mp4 is only worth rewriting for the track browsers will play;
    // sidecars alone are the embed step's business.
    if !mkv && !plan.default_twin {
        return Ok(Outcome::Fine);
    }
    if dry_run {
        return Ok(Outcome::WouldRemux(plan));
    }
    if let Some(policy) = loudness {
        let twinned: Vec<usize> = plan.audio.iter().filter(|(_, twin)| *twin).map(|(i, _)| *i).collect();
        for index in twinned {
            // A failed measurement costs the raise, not the remux.
            match crate::loudness::measure(ffmpeg, media, &format!("0:{index}"), Some("stereo")) {
                Ok(measured) => {
                    let gain = match policy.decide(&measured) {
                        crate::loudness::Verdict::Gain(g) => Some(g),
                        _ => None,
                    };
                    plan.twin_levels.push(TwinLevel { index, measured, gain });
                }
                Err(err) => eprintln!("{}: twin loudness not measured: {err:#}", media.display()),
            }
        }
    }

    let dir = media.parent().unwrap_or_else(|| Path::new("."));
    let stem = media.file_stem().unwrap_or_default().to_string_lossy();
    let temp = temp_beside(dir, &stem, "remux-tmp", "mp4");
    // Mux from a UTF-8 temp copy when the sidecar needed decoding; the
    // original .srt is never modified.
    let captions_hash = if plan.embed_srt {
        srt_text.as_deref().map(crate::subtitles::sidecar_hash)
    } else {
        None
    };
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
        .args(ffmpeg_args(&plan, media, srt_input.as_deref(), captions_hash.as_deref(), &temp))
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
    if mkv {
        if let Err(err) = std::fs::remove_file(media) {
            // Both files now exist; the catalog will merge them as renditions
            // until the .mkv goes. Loud, but not fatal.
            eprintln!("{}: remuxed, but the original could not be removed: {err}", media.display());
        }
    }
    Ok(Outcome::Remuxed(plan))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(index: usize, kind: StreamKind, codec: &str) -> Stream {
        Stream { index, kind, codec: codec.into(), language: None, dovi: false, channels: 0, default: false, label: String::new() }
    }

    fn audio(index: usize, codec: &str, channels: usize, default: bool, label: &str) -> Stream {
        Stream { channels, default, label: label.into(), ..s(index, StreamKind::Audio, codec) }
    }

    fn joined(plan: &Plan) -> String {
        ffmpeg_args(plan, Path::new("in.mkv"), None, None, Path::new("out.mp4"))
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join(" ")
    }

    #[test]
    fn a_wide_default_track_gets_a_twin_whatever_its_codec() {
        // The Bourne layout: 5.1 AAC default, stereo AAC commentary.
        let p = Probe {
            streams: vec![
                s(0, StreamKind::Video, "hevc"),
                audio(1, "aac", 6, true, "Surround AAC 5.1 "),
                audio(2, "aac", 2, false, "Commentary "),
                s(3, StreamKind::Subtitle, "subrip"),
            ],
            duration: 1.0,
        };
        let plan = plan(&p, false).unwrap();
        assert_eq!(plan.audio, vec![(1, true), (2, false)]);
        assert!(plan.default_twin);
        assert_eq!(plan.twin_reasons, vec!["aac 5.1"]);
        let j = joined(&plan);
        assert!(j.contains("-map 0:0 -map 0:1 -map 0:1 -map 0:2 -map 0:3"), "{j}");
        assert!(j.contains("-c:a:0 aac -b:a:0 192k -ac:a:0 2"), "{j}");
        assert!(j.contains("-disposition:a:0 default"), "{j}");
        assert!(j.contains("-disposition:a:1 0"), "{j}");
        assert!(j.contains("-disposition:a:2 0"), "{j}");
    }

    #[test]
    fn wide_tracks_that_do_not_play_by_default_are_left_alone() {
        // Stereo AAC default, a 5.1 AAC alternate: browsers are fine.
        let p = Probe {
            streams: vec![
                s(0, StreamKind::Video, "h264"),
                audio(1, "aac", 2, true, ""),
                audio(2, "aac", 6, false, ""),
            ],
            duration: 1.0,
        };
        let stereo_first = plan(&p, false).unwrap();
        assert_eq!(stereo_first.audio, vec![(1, false), (2, false)]);
        assert!(!stereo_first.default_twin);
        // No default flag anywhere: the first track is the one that plays.
        let p = Probe {
            streams: vec![s(0, StreamKind::Video, "h264"), audio(1, "aac", 6, false, ""), audio(2, "aac", 6, false, "")],
            duration: 1.0,
        };
        let unflagged = plan(&p, false).unwrap();
        assert_eq!(unflagged.audio, vec![(1, true), (2, false)]);
        assert!(unflagged.default_twin);
    }

    #[test]
    fn an_earlier_twin_is_recognised_and_not_doubled() {
        // The shape this pass leaves behind: our twin, then the original.
        let p = Probe {
            streams: vec![
                s(0, StreamKind::Video, "h264"),
                audio(1, "aac", 2, true, "Stereo (AAC) "),
                audio(2, "eac3", 6, false, ""),
                audio(3, "aac", 6, false, "Isolated score "),
            ],
            duration: 1.0,
        };
        let done = plan(&p, false).unwrap();
        assert_eq!(done.audio, vec![(1, false), (2, false), (3, false)]);
        assert!(!done.default_twin);
        assert!(done.twin_reasons.is_empty());
        // A raised twin counts the same.
        let p = Probe {
            streams: vec![
                s(0, StreamKind::Video, "h264"),
                audio(1, "aac", 2, true, "Stereo (AAC), normalized +6.0 dB (media-enrich) "),
                audio(2, "aac", 6, false, ""),
            ],
            duration: 1.0,
        };
        assert_eq!(plan(&p, false).unwrap().twins(), 0);
    }

    #[test]
    fn only_the_first_twin_is_default() {
        let p = Probe {
            streams: vec![s(0, StreamKind::Video, "h264"), audio(1, "eac3", 6, true, ""), audio(2, "ac3", 2, false, "Commentary ")],
            duration: 1.0,
        };
        let plan = plan(&p, false).unwrap();
        assert_eq!(plan.twins(), 2);
        assert_eq!(plan.twin_reasons, vec!["eac3 5.1", "ac3 stereo"]);
        let j = joined(&plan);
        assert!(j.contains("-disposition:a:0 default"), "{j}");
        assert!(j.contains("-disposition:a:1 0"), "{j}");
        assert!(j.contains("-disposition:a:2 0"), "{j}");
        assert!(j.contains("-disposition:a:3 0"), "{j}");
        assert_eq!(layout_name(8), "7.1");
        assert_eq!(layout_name(3), "3ch");
    }

    #[test]
    fn parses_ffprobe_default_output_with_side_data() {
        let text = "[STREAM]\nindex=0\ncodec_name=hevc\ncodec_type=video\nDISPOSITION:default=1\nTAG:language=und\n[SIDE_DATA]\nside_data_type=DOVI configuration record\n[/SIDE_DATA]\n[/STREAM]\n[STREAM]\nindex=1\ncodec_name=eac3\ncodec_type=audio\nchannels=6\nDISPOSITION:default=1\nTAG:language=eng\nTAG:title=Surround\n[/STREAM]\n[STREAM]\nindex=2\ncodec_name=aac\ncodec_type=audio\nchannels=2\nDISPOSITION:default=0\nTAG:handler_name=Stereo (AAC)\n[/STREAM]\n[FORMAT]\nduration=5400.123000\n[/FORMAT]\n";
        let p = parse_probe(text);
        assert_eq!(p.streams.len(), 3);
        assert!(p.streams[0].dovi);
        assert_eq!(p.streams[0].language, None);
        assert_eq!(p.streams[1].language.as_deref(), Some("eng"));
        assert_eq!(p.streams[1].kind, StreamKind::Audio);
        assert_eq!(p.streams[1].channels, 6);
        assert!(p.streams[1].default && !p.streams[2].default);
        assert_eq!(p.streams[1].label, "Surround ");
        assert_eq!(p.streams[2].label, "Stereo (AAC) ");
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

        let args = ffmpeg_args(&plan, Path::new("in.mkv"), None, None, Path::new("out.mp4"));
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

        let hash = "cd".repeat(32);
        let args = ffmpeg_args(&plan, Path::new("in.mkv"), Some(Path::new("in.srt")), Some(&hash), Path::new("out.mp4"));
        let joined = args.iter().map(|a| a.to_string_lossy().into_owned()).collect::<Vec<_>>().join(" ");
        assert!(joined.contains("-i in.mkv -i in.srt"), "{joined}");
        // The bitmap track (0:2) is not mapped; the sidecar is the one sub.
        assert!(joined.contains("-map 0:0 -map 0:1 -map 1:0"), "{joined}");
        assert!(!joined.contains("-map 0:2"), "{joined}");
        assert!(joined.contains("-c:s mov_text"), "{joined}");
        assert!(joined.contains("-metadata:s:s:0 language=eng"), "{joined}");
        assert!(joined.contains(&format!("-metadata encoding_tool=media-enrich; captions=srt:sha256:{hash}")), "{joined}");
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
        let args = ffmpeg_args(&plan, Path::new("in.mkv"), None, None, Path::new("out.mp4"));
        let joined = args.iter().map(|a| a.to_string_lossy().into_owned()).collect::<Vec<_>>().join(" ");
        assert!(joined.contains("-tag:v hvc1"), "{joined}");
        assert!(joined.contains("-disposition:a:0 default"), "{joined}");
    }
}
