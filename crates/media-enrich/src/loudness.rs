//! Loudness: films mastered far below everything else in the library.
//!
//! A theatrical mix can sit 15 dB under a TV episode (Halloween (1978)
//! measures −38 LUFS against the usual −24), which on a TV or a soundbar
//! means riding the volume between programs. No player honours a gain
//! tag on a video file, so the only fix that reaches every client — VLC,
//! a TV's own app, the web player — is a track in the file.
//!
//! What this step does, and only to files that need it: measure the
//! default audio track (EBU R 128 integrated loudness and true peak, one
//! audio-only decode), and when it is well under the target *and* its
//! peaks leave room, add a copy of that track raised by a plain linear
//! gain as the new default track, the original right behind it. A source
//! wider than stereo gets two raised tracks: a stereo mixdown first, as
//! the default — the one browsers can play (Firefox skips 5.1 AAC), so
//! no separate twin is needed — and the same raise in the source's own
//! layout behind it, for receivers and VLC; each measured and raised on
//! its own, since a downmix has its own level and peaks. Nothing
//! is ever compressed or limited: the gain is capped by the track's own
//! headroom, so the mix is untouched and cannot clip. A quiet file whose
//! peaks are already near full scale is left alone — raising it would
//! take dynamic-range compression, which is a change to the mix and not
//! this tool's to make.
//!
//! Like the other steps that replace a media file: the mux goes to a
//! temp file beside the original, is verified (stream census, duration,
//! the caption record, and a measurement of the new track), and only
//! then renamed over it.

use std::path::Path;
use std::process::Command;

use anyhow::{bail, Context, Result};

/// True-peak ceiling for the raised track, dBTP. Lossy encoding moves
/// peaks by a fraction of a dB, hence −2 rather than the −1 usual for PCM.
pub const CEILING_DBTP: f64 = -2.0;
/// Never raise by more than this: past it the "programme" is mostly
/// noise floor (or a measurement of near-silence).
pub const MAX_GAIN_DB: f64 = 20.0;
/// At or under this the meter saw silence.
const SILENCE_LUFS: f64 = -69.0;
/// The handler name remux gives a stereo AAC twin.
pub const TWIN_LABEL: &str = "Stereo (AAC)";

/// When to act, from the [enrich] section.
#[derive(Debug, Clone, Copy)]
pub struct Policy {
    /// Integrated loudness to raise quiet tracks towards, LUFS.
    pub target: f64,
    /// A raise smaller than this is not worth rewriting a file for.
    pub min_gain: f64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Measurement {
    /// Integrated loudness, LUFS.
    pub integrated: f64,
    /// True peak, dBTP.
    pub true_peak: f64,
    /// Loudness range, LU.
    pub range: f64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Verdict {
    /// Within reach of the target already.
    Normal,
    /// Quiet, but the peaks leave too little room for a linear raise.
    NoHeadroom { wanted: f64, headroom: f64 },
    /// Raise by this many dB.
    Gain(f64),
}

impl Policy {
    pub fn decide(&self, m: &Measurement) -> Verdict {
        let wanted = self.target - m.integrated;
        if wanted < self.min_gain {
            return Verdict::Normal;
        }
        let headroom = CEILING_DBTP - m.true_peak;
        let gain = wanted.min(headroom).min(MAX_GAIN_DB);
        if gain < self.min_gain {
            return Verdict::NoHeadroom { wanted, headroom };
        }
        // Tenths: what the label says is what the filter was given.
        Verdict::Gain((gain * 10.0).floor() / 10.0)
    }
}

/// The label a raised track carries — MP4 shows it as the handler name,
/// Matroska as the track title; players list it in their audio menu, and
/// it is how a later run knows the file has been done.
pub fn label(gain: f64, twin: bool) -> String {
    if twin {
        format!("{TWIN_LABEL}, normalized {gain:+.1} dB (media-enrich)")
    } else {
        format!("Normalized {gain:+.1} dB (media-enrich)")
    }
}

pub fn is_marked(label: &str) -> bool {
    let l = label.to_lowercase();
    l.contains("normalized") && l.contains("(media-enrich)")
}

/// The numbers out of the ebur128 filter's closing summary.
pub fn parse_summary(stderr: &str) -> Option<Measurement> {
    let tail = &stderr[stderr.rfind("Summary:")?..];
    let value = |key: &str| -> Option<f64> {
        tail.lines()
            .map(str::trim)
            .find_map(|line| line.strip_prefix(key))
            .and_then(|rest| rest.split_whitespace().next())
            .and_then(|n| n.parse::<f64>().ok())
            .filter(|v| v.is_finite())
    };
    Some(Measurement {
        integrated: value("I:")?,
        range: value("LRA:")?,
        true_peak: value("Peak:")?,
    })
}

/// Measure one audio stream (`spec` as ffmpeg names it: "0:1", "0:a:0"),
/// after conversion to `layout` when the raised track will be encoded in
/// a layout of its own (a stereo twin's downmix has its own level and
/// peaks). Audio only: video is never decoded.
pub fn measure(ffmpeg: &str, media: &Path, spec: &str, layout: Option<&str>) -> Result<Measurement> {
    // The per-frame log goes to the verbose level and so stays unprinted;
    // the summary arrives at the default level.
    let meter = "ebur128=peak=true:framelog=verbose";
    let filter = match layout {
        Some(l) => format!("aformat=channel_layouts={l},{meter}"),
        None => meter.to_string(),
    };
    let out = Command::new(ffmpeg)
        .args(["-hide_banner", "-nostdin", "-nostats", "-vn", "-sn", "-dn", "-i"])
        .arg(media)
        .args(["-map", spec, "-af", &filter, "-f", "null", "-"])
        .output()
        .with_context(|| format!("running {ffmpeg}"))?;
    let stderr = String::from_utf8_lossy(&out.stderr);
    if !out.status.success() {
        bail!("ffmpeg could not decode the audio ({})", stderr.lines().last().unwrap_or("no output").trim());
    }
    parse_summary(&stderr).context("no loudness summary in ffmpeg's output")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Video,
    Audio,
    Subtitle,
    Other,
}

#[derive(Debug, Clone)]
struct Stream {
    index: usize,
    kind: Kind,
    codec: String,
    channels: usize,
    default: bool,
    /// Title and handler name together: whichever the container keeps.
    label: String,
    language: Option<String>,
}

#[derive(Debug, Default, Clone)]
struct Probe {
    streams: Vec<Stream>,
    duration: f64,
    /// ©too — where media_db::captions keeps its record, which a remux
    /// through ffmpeg would otherwise overwrite with its own name.
    encoder: String,
}

impl Probe {
    fn count(&self, kind: Kind) -> usize {
        self.streams.iter().filter(|s| s.kind == kind).count()
    }
    fn audio(&self) -> impl Iterator<Item = &Stream> {
        self.streams.iter().filter(|s| s.kind == Kind::Audio)
    }
}

fn probe(ffprobe: &str, path: &Path) -> Result<Probe> {
    let out = Command::new(ffprobe)
        .args([
            "-v", "error",
            "-show_entries",
            "stream=index,codec_type,codec_name,channels:stream_tags=title,handler_name,language:\
             stream_disposition=default:format=duration:format_tags=encoder",
        ])
        .arg(path)
        .output()
        .with_context(|| format!("running {ffprobe}"))?;
    if !out.status.success() {
        bail!("ffprobe failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(parse_probe(&String::from_utf8_lossy(&out.stdout)))
}

fn parse_probe(text: &str) -> Probe {
    let mut probe = Probe::default();
    let mut current: Option<Stream> = None;
    let mut in_format = false;
    for line in text.lines().map(str::trim) {
        match line {
            "[STREAM]" => {
                current = Some(Stream {
                    index: 0, kind: Kind::Other, codec: String::new(),
                    channels: 0, default: false, label: String::new(), language: None,
                });
            }
            "[/STREAM]" => probe.streams.extend(current.take()),
            "[FORMAT]" => in_format = true,
            "[/FORMAT]" => in_format = false,
            _ => {
                let Some((key, value)) = line.split_once('=') else { continue };
                // Matroska reports its tag names in capitals.
                let key = key.to_lowercase();
                if let Some(s) = current.as_mut() {
                    match key.as_str() {
                        "index" => s.index = value.parse().unwrap_or(0),
                        "codec_name" => s.codec = value.to_string(),
                        "codec_type" => {
                            s.kind = match value {
                                "video" => Kind::Video,
                                "audio" => Kind::Audio,
                                "subtitle" => Kind::Subtitle,
                                _ => Kind::Other,
                            }
                        }
                        "channels" => s.channels = value.parse().unwrap_or(0),
                        "disposition:default" => s.default = value == "1",
                        "tag:language" => s.language = Some(value.to_string()).filter(|l| !l.is_empty() && l != "und"),
                        "tag:title" | "tag:handler_name" => {
                            s.label.push_str(value);
                            s.label.push(' ');
                        }
                        _ => {}
                    }
                } else if in_format {
                    match key.as_str() {
                        "duration" => probe.duration = value.parse().unwrap_or(0.0),
                        "tag:encoder" => probe.encoder = value.to_string(),
                        _ => {}
                    }
                }
            }
        }
    }
    probe
}

/// What the rewrite will do, decided from the probe.
#[derive(Debug, Clone, PartialEq)]
struct Plan {
    /// Input stream the raised track(s) are encoded from.
    source: usize,
    /// Its channel count: wider than stereo earns a mixdown as well.
    channels: usize,
    /// An un-normalized stereo twin the raised ones replace.
    drop: Option<usize>,
    /// Audio streams kept behind the new tracks, in order.
    keep: Vec<usize>,
    hevc_mp4: bool,
    matroska: bool,
    /// Names for players' menus, by language and layout: the mixdown's
    /// ("English Stereo Mixdown") and the source layout's ("English 5.1").
    mixdown_name: String,
    wide_name: String,
}

/// One track the rewrite adds: what it is made from, how, and its names.
#[derive(Debug, Clone, PartialEq)]
pub struct Raised {
    /// Layout the source is converted to first; None keeps its own.
    pub layout: Option<&'static str>,
    pub channels: usize,
    pub gain: f64,
    /// What it measured before the raise, in the layout it is made in.
    pub measured: Measurement,
    title: String,
    handler: String,
}

/// What measuring a source decided.
#[derive(Debug, Clone, PartialEq)]
pub enum Decision {
    Silent,
    Normal(Measurement),
    NoHeadroom { measured: Measurement, wanted: f64, headroom: f64 },
    /// Raise: the tracks to add, in output order (a mixdown first when
    /// the source is wide), and the source's own measurement.
    Raise { measured: Measurement, tracks: Vec<Raised> },
}

fn is_mp4(path: &Path) -> bool {
    ext_is(path, &["mp4", "m4v", "mov"])
}

fn is_matroska(path: &Path) -> bool {
    ext_is(path, &["mkv"])
}

fn ext_is(path: &Path, any: &[&str]) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| any.iter().any(|a| e.eq_ignore_ascii_case(a)))
}

fn plan(probe: &Probe, media: &Path) -> std::result::Result<Plan, String> {
    if probe.count(Kind::Video) == 0 {
        return Err("no video stream".into());
    }
    if probe.audio().any(|s| is_marked(&s.label)) {
        return Err(DONE.into());
    }
    // What plays when nobody chooses: the default-flagged track, else the first.
    let playing = probe
        .audio()
        .find(|s| s.default)
        .or_else(|| probe.audio().next())
        .ok_or("no audio stream")?;
    // A stereo twin remux made without this step: raise afresh from the
    // Dolby or multichannel track behind it rather than re-encode an
    // encode, and drop the bare twin.
    let behind = probe.audio().skip_while(|s| s.index != playing.index).nth(1);
    let (source, drop) = match behind {
        Some(original)
            if playing.label.contains(TWIN_LABEL)
                && (matches!(original.codec.as_str(), "ac3" | "eac3") || original.channels > 2) =>
        {
            (original, Some(playing.index))
        }
        _ => (playing, None),
    };
    let hevc = probe.streams.iter().any(|s| s.kind == Kind::Video && s.codec == "hevc");
    let language = source.language.as_deref().and_then(crate::remux::language_name);
    let named = |rest: &str| match language {
        Some(lang) => format!("{lang} {rest}"),
        None => rest.to_string(),
    };
    Ok(Plan {
        source: source.index,
        channels: source.channels.max(1),
        drop,
        keep: probe.audio().map(|s| s.index).filter(|i| Some(*i) != drop).collect(),
        hevc_mp4: hevc && is_mp4(media),
        matroska: is_matroska(media),
        mixdown_name: named("Stereo Mixdown"),
        wide_name: named(&match source.channels {
            1 => "Mono".to_string(),
            2 => "Stereo".to_string(),
            n => crate::remux::layout_name(n),
        }),
    })
}

/// Measure a source and decide what to add. The source's own layout
/// decides whether it is quiet at all (one decode, the common cost);
/// only a quiet wide source is decoded a second time, as its stereo
/// downmix, for the mixdown's own gain — a downmix sits a little lower
/// and peaks differently. Shared with the remux twin path.
pub fn decide(
    ffmpeg: &str,
    media: &Path,
    spec: &str,
    channels: usize,
    policy: &Policy,
    mixdown_name: &str,
    wide_name: &str,
) -> Result<Decision> {
    let measured = measure(ffmpeg, media, spec, None)?;
    if measured.integrated <= SILENCE_LUFS {
        return Ok(Decision::Silent);
    }
    let gain = match policy.decide(&measured) {
        Verdict::Normal => return Ok(Decision::Normal(measured)),
        Verdict::NoHeadroom { wanted, headroom } => {
            return Ok(Decision::NoHeadroom { measured, wanted, headroom })
        }
        Verdict::Gain(gain) => gain,
    };
    let mut tracks = Vec::new();
    if channels > 2 {
        let downmix = measure(ffmpeg, media, spec, Some("stereo"))?;
        // Its own gain where the policy grants one; otherwise as much of
        // the source's raise as its peaks allow (a downmix of a quiet
        // track is quiet too, so this is a corner).
        let mixdown_gain = match policy.decide(&downmix) {
            Verdict::Gain(g) => g,
            _ => ((gain.min(CEILING_DBTP - downmix.true_peak).max(0.0)) * 10.0).floor() / 10.0,
        };
        tracks.push(Raised {
            layout: Some("stereo"),
            channels: 2,
            gain: mixdown_gain,
            measured: downmix,
            title: format!("{mixdown_name}, normalized {mixdown_gain:+.1} dB (media-enrich)"),
            handler: label(mixdown_gain, true),
        });
    }
    tracks.push(Raised {
        layout: None,
        channels,
        gain,
        measured,
        title: format!("{wide_name}, normalized {gain:+.1} dB (media-enrich)"),
        handler: label(gain, false),
    });
    Ok(Decision::Raise { measured, tracks })
}

/// "a +9.0 dB stereo mixdown and a +8.0 dB 5.1 track", for the log.
pub fn describe(tracks: &[Raised]) -> String {
    let one = |t: &Raised| match t.layout {
        Some(_) => format!("a {:+.1} dB stereo mixdown", t.gain),
        None => format!(
            "a {:+.1} dB {} track",
            t.gain,
            match t.channels {
                1 => "mono".to_string(),
                2 => "stereo".to_string(),
                n => crate::remux::layout_name(n),
            }
        ),
    };
    tracks.iter().map(one).collect::<Vec<_>>().join(" and ")
}

const DONE: &str = "already carries a normalized track";

fn bitrate(channels: usize) -> &'static str {
    match channels {
        0 | 1 => "96k",
        2 => "192k",
        3..=6 => "384k",
        _ => "512k",
    }
}

fn ffmpeg_args(plan: &Plan, tracks: &[Raised], input: &Path, keep_encoder: Option<&str>, output: &Path) -> Vec<std::ffi::OsString> {
    let mut args: Vec<std::ffi::OsString> = Vec::new();
    for a in ["-v", "error", "-nostdin", "-y", "-i"] {
        args.push(a.into());
    }
    args.push(input.into());
    let mut push = |a: String| args.push(a.into());
    // Video (cover art included), the raised track(s), the audio kept
    // behind them, subtitles; Matroska attachments (fonts) come along.
    push("-map".into());
    push("0:v?".into());
    for _ in tracks {
        push("-map".into());
        push(format!("0:{}", plan.source));
    }
    for idx in &plan.keep {
        push("-map".into());
        push(format!("0:{idx}"));
    }
    push("-map".into());
    push("0:s?".into());
    if plan.matroska {
        push("-map".into());
        push("0:t?".into());
    }
    push("-c".into());
    push("copy".into());
    for (n, t) in tracks.iter().enumerate() {
        push(format!("-c:a:{n}"));
        push("aac".into());
        push(format!("-b:a:{n}"));
        push(bitrate(t.channels).into());
        push(format!("-filter:a:{n}"));
        push(match t.layout {
            Some(l) => format!("aformat=channel_layouts={l},volume={:.1}dB", t.gain),
            None => format!("volume={:.1}dB", t.gain),
        });
        // The title is what players list; the handler name is the marker
        // by which this step and the twin pass know the track (the twin
        // form for a mixdown, so a later remux sees a twin ahead already).
        push(format!("-metadata:s:a:{n}"));
        push(format!("title={}", t.title));
        push(format!("-metadata:s:a:{n}"));
        push(format!("handler_name={}", t.handler));
        push(format!("-disposition:a:{n}"));
        push(if n == 0 { "default".into() } else { "0".into() });
    }
    for n in tracks.len()..tracks.len() + plan.keep.len() {
        push(format!("-disposition:a:{n}"));
        push("0".into());
    }
    if plan.hevc_mp4 {
        // ffmpeg's default hev1 tag is refused by Apple's players.
        push("-tag:v".into());
        push("hvc1".into());
    }
    if let Some(encoder) = keep_encoder {
        push("-metadata".into());
        push(format!("encoding_tool={encoder}"));
    }
    args.push(output.into());
    args
}

#[derive(Debug)]
pub enum Outcome {
    /// Not a candidate; the reason.
    Skipped(String),
    /// A raised track is already in the file.
    AlreadyNormalized,
    Normal(Measurement),
    NoHeadroom { measured: Measurement, wanted: f64, headroom: f64 },
    /// Dry run: what a real run would do.
    WouldNormalize { measured: Measurement, tracks: Vec<Raised> },
    /// Done: the source's measurement, and each added track with what
    /// it measured afterwards.
    Normalized { before: Measurement, tracks: Vec<(Raised, Measurement)> },
}

/// Measure one file and, when the policy says so, give it a raised
/// default track. With `dry_run`, measure and decide only.
pub fn normalize_if_applicable(
    ffmpeg: &str,
    ffprobe: &str,
    media: &Path,
    policy: &Policy,
    dry_run: bool,
) -> Result<Outcome> {
    if !is_mp4(media) && !is_matroska(media) {
        return Ok(Outcome::Skipped("not an MP4 or Matroska file".into()));
    }
    let before = probe(ffprobe, media)?;
    let plan = match plan(&before, media) {
        Ok(plan) => plan,
        Err(why) if why == DONE => return Ok(Outcome::AlreadyNormalized),
        Err(why) => return Ok(Outcome::Skipped(why)),
    };
    let (measured, tracks) = match decide(
        ffmpeg, media, &format!("0:{}", plan.source), plan.channels, policy, &plan.mixdown_name, &plan.wide_name,
    )? {
        Decision::Silent => return Ok(Outcome::Skipped("the audio is silent".into())),
        Decision::Normal(m) => return Ok(Outcome::Normal(m)),
        Decision::NoHeadroom { measured, wanted, headroom } => {
            return Ok(Outcome::NoHeadroom { measured, wanted, headroom })
        }
        Decision::Raise { measured, tracks } => (measured, tracks),
    };
    if dry_run {
        return Ok(Outcome::WouldNormalize { measured, tracks });
    }

    let dir = media.parent().unwrap_or_else(|| Path::new("."));
    let stem = media.file_stem().unwrap_or_default().to_string_lossy();
    let ext = media.extension().and_then(|e| e.to_str()).unwrap_or("mp4").to_lowercase();
    let temp = crate::remux::temp_beside(dir, &stem, "loudness-tmp", &ext);
    let record = media_db::captions::recorded_hash(&before.encoder).map(|_| before.encoder.as_str());
    let status = Command::new(ffmpeg)
        .args(ffmpeg_args(&plan, &tracks, media, record, &temp))
        .status()
        .with_context(|| format!("running {ffmpeg}"))?;
    if !status.success() {
        let _ = std::fs::remove_file(&temp);
        bail!("ffmpeg mux failed ({status})");
    }
    if let Ok(f) = std::fs::File::open(&temp) {
        let _ = f.sync_all();
    }

    // Verify before touching the original: the same streams plus the
    // new ones (less a replaced twin), the raised track first and
    // labelled, the caption record intact, the duration unchanged — and
    // each new track measuring where the arithmetic says it should.
    let verified = (|| -> Result<Vec<Measurement>> {
        let after = probe(ffprobe, &temp).context("probing the result")?;
        let want_audio = before.count(Kind::Audio) + tracks.len() - plan.drop.is_some() as usize;
        let first = after.audio().next();
        let sane = after.count(Kind::Video) == before.count(Kind::Video)
            && after.count(Kind::Audio) == want_audio
            && after.count(Kind::Subtitle) == before.count(Kind::Subtitle)
            && (after.duration - before.duration).abs() <= 1.0 + before.duration * 0.01
            && first.is_some_and(|s| s.codec == "aac" && s.default && is_marked(&s.label))
            && (record.is_none() || after.encoder == before.encoder);
        if !sane {
            bail!(
                "stream check failed (video {}/{}, audio {}/{want_audio}, subs {}/{}, duration {:.1}->{:.1}, record {})",
                after.count(Kind::Video), before.count(Kind::Video),
                after.count(Kind::Audio),
                after.count(Kind::Subtitle), before.count(Kind::Subtitle),
                before.duration, after.duration,
                if record.is_none() || after.encoder == before.encoder { "ok" } else { "lost" }
            );
        }
        let mut levels = Vec::new();
        for (n, t) in tracks.iter().enumerate() {
            let level = measure(ffmpeg, &temp, &format!("0:a:{n}"), None).context("measuring the new track")?;
            let expected = t.measured.integrated + t.gain;
            if (level.integrated - expected).abs() > 1.5 || level.true_peak > -0.5 {
                bail!(
                    "new track {n} measures {:.1} LUFS, peak {:.1} dBTP (expected about {expected:.1}, peak under {CEILING_DBTP:.0})",
                    level.integrated, level.true_peak
                );
            }
            levels.push(level);
        }
        Ok(levels)
    })();
    let levels = match verified {
        Ok(levels) => levels,
        Err(err) => {
            let _ = std::fs::remove_file(&temp);
            return Err(err.context("verification failed; original untouched"));
        }
    };

    // The original's mtime, as remux keeps it: nothing about when this
    // program arrived has changed.
    if let Ok(modified) = std::fs::metadata(media).and_then(|m| m.modified()) {
        if let Ok(f) = std::fs::OpenOptions::new().write(true).open(&temp) {
            let _ = f.set_modified(modified);
        }
    }
    std::fs::rename(&temp, media).with_context(|| format!("replacing {}", media.display()))?;
    Ok(Outcome::Normalized { before: measured, tracks: tracks.into_iter().zip(levels).collect() })
}

#[cfg(test)]
mod tests {
    use super::*;

    const POLICY: Policy = Policy { target: -24.0, min_gain: 4.0 };

    fn m(integrated: f64, true_peak: f64) -> Measurement {
        Measurement { integrated, true_peak, range: 10.0 }
    }

    #[test]
    fn gain_is_linear_and_capped_by_headroom() {
        // Halloween (1978): wants 14.2 dB, the peaks allow 10.27.
        assert_eq!(POLICY.decide(&m(-38.19, -12.27)), Verdict::Gain(10.2));
        // Room to spare: all the way to the target.
        assert_eq!(POLICY.decide(&m(-34.0, -20.0)), Verdict::Gain(10.0));
        // Ordinary levels are left alone, however much headroom there is.
        assert_eq!(POLICY.decide(&m(-21.8, -11.7)), Verdict::Normal);
        assert_eq!(POLICY.decide(&m(-27.0, -15.0)), Verdict::Normal);
        // Quiet with peaks near full scale: compression territory, not ours.
        assert!(matches!(POLICY.decide(&m(-35.8, -5.1)), Verdict::NoHeadroom { .. }));
        // Near-silence is not raised without limit.
        assert_eq!(POLICY.decide(&m(-60.0, -45.0)), Verdict::Gain(MAX_GAIN_DB));
    }

    #[test]
    fn parses_the_meter_summary() {
        let text = "[Parsed_ebur128_0 @ 0x1] Summary:\n\n  Integrated loudness:\n    I:         -38.2 LUFS\n    \
                    Threshold: -50.4 LUFS\n\n  Loudness range:\n    LRA:        20.2 LU\n    Threshold: -61.8 LUFS\n    \
                    LRA low:   -41.8 LUFS\n    LRA high:  -21.6 LUFS\n\n  True peak:\n    Peak:      -12.3 dBFS\n";
        assert_eq!(parse_summary(text), Some(Measurement { integrated: -38.2, true_peak: -12.3, range: 20.2 }));
        assert_eq!(parse_summary("no summary here"), None);
    }

    fn stream(index: usize, kind: Kind, codec: &str, channels: usize, default: bool, label: &str) -> Stream {
        Stream { index, kind, codec: codec.into(), channels, default, label: label.into(), language: None }
    }

    #[test]
    fn plans_the_playing_track_and_replaces_a_bare_twin() {
        let plain = Probe {
            streams: vec![
                stream(0, Kind::Video, "hevc", 0, true, ""),
                stream(1, Kind::Audio, "aac", 6, true, "SoundHandler"),
                stream(2, Kind::Subtitle, "mov_text", 0, false, ""),
            ],
            ..Default::default()
        };
        let p = plan(&plain, Path::new("x.mp4")).unwrap();
        assert_eq!((p.source, p.channels, p.drop, p.keep.clone()), (1, 6, None, vec![1]));
        assert_eq!((p.mixdown_name.as_str(), p.wide_name.as_str()), ("Stereo Mixdown", "5.1"));
        assert!(p.hevc_mp4 && !p.matroska);
        let m = Measurement { integrated: -38.0, true_peak: -12.0, range: 10.0 };
        let tracks = vec![
            Raised { layout: Some("stereo"), channels: 2, gain: 9.0, measured: m, title: "Stereo Mixdown, normalized +9.0 dB (media-enrich)".into(), handler: label(9.0, true) },
            Raised { layout: None, channels: 6, gain: 10.2, measured: m, title: "5.1, normalized +10.2 dB (media-enrich)".into(), handler: label(10.2, false) },
        ];
        let args: Vec<String> = ffmpeg_args(&p, &tracks, Path::new("x.mp4"), Some("rec"), Path::new("t.mp4"))
            .iter().map(|a| a.to_string_lossy().into_owned()).collect();
        let joined = args.join(" ");
        assert!(joined.contains("-map 0:v? -map 0:1 -map 0:1 -map 0:1 -map 0:s? -c copy -c:a:0 aac -b:a:0 192k -filter:a:0 aformat=channel_layouts=stereo,volume=9.0dB"), "{joined}");
        assert!(joined.contains("-metadata:s:a:0 title=Stereo Mixdown, normalized +9.0 dB (media-enrich) -metadata:s:a:0 handler_name=Stereo (AAC), normalized +9.0 dB (media-enrich) -disposition:a:0 default"), "{joined}");
        assert!(joined.contains("-c:a:1 aac -b:a:1 384k -filter:a:1 volume=10.2dB -metadata:s:a:1 title=5.1, normalized +10.2 dB (media-enrich) -metadata:s:a:1 handler_name=Normalized +10.2 dB (media-enrich) -disposition:a:1 0"), "{joined}");
        assert!(joined.contains("-disposition:a:2 0 -tag:v hvc1 -metadata encoding_tool=rec"), "{joined}");
        assert_eq!(describe(&tracks), "a +9.0 dB stereo mixdown and a +10.2 dB 5.1 track");
        // A stereo source is raised as it is, under the plain label.
        let stereo = Probe {
            streams: vec![stream(0, Kind::Video, "h264", 0, true, ""), stream(1, Kind::Audio, "aac", 2, true, "")],
            ..Default::default()
        };
        let p = plan(&stereo, Path::new("x.mp4")).unwrap();
        assert_eq!((p.channels, p.wide_name.as_str()), (2, "Stereo"));
        let one = vec![Raised { layout: None, channels: 2, gain: 6.0, measured: m, title: "Stereo, normalized +6.0 dB (media-enrich)".into(), handler: label(6.0, false) }];
        let j = ffmpeg_args(&p, &one, Path::new("x.mp4"), None, Path::new("t.mp4")).iter().map(|a| a.to_string_lossy().into_owned()).collect::<Vec<_>>().join(" ");
        assert!(j.contains("-map 0:v? -map 0:1 -map 0:1 -map 0:s? -c copy -c:a:0 aac -b:a:0 192k -filter:a:0 volume=6.0dB -metadata:s:a:0 title=Stereo, normalized +6.0 dB (media-enrich)"), "{j}");
        assert!(j.contains("-disposition:a:0 default -disposition:a:1 0 "), "{j}");
        assert_eq!(describe(&one), "a +6.0 dB stereo track");

        // A twin made before this step existed: a new one from the Dolby        // A twin made before this step existed: a new one from the Dolby
        // track behind it takes its place.
        let twinned = Probe {
            streams: vec![
                stream(0, Kind::Video, "h264", 0, true, ""),
                stream(1, Kind::Audio, "aac", 2, true, "Stereo (AAC)"),
                stream(2, Kind::Audio, "eac3", 6, false, "SoundHandler"),
            ],
            ..Default::default()
        };
        let p = plan(&twinned, Path::new("x.mkv")).unwrap();
        assert_eq!((p.source, p.channels, p.drop, p.keep.clone()), (2, 6, Some(1), vec![2]));
        assert!(label(9.0, true).starts_with(TWIN_LABEL) && is_marked(&label(9.0, true)));

        // Done once is done.
        let mut done = twinned.clone();
        done.streams[1].label = label(9.0, true);
        assert_eq!(plan(&done, Path::new("x.mp4")), Err(DONE.to_string()));
        assert!(!is_marked("Stereo (AAC)") && !is_marked("Commentary (normalized)"));
    }

    #[test]
    fn parses_ffprobe_output_from_both_containers() {
        let text = "[STREAM]\nindex=0\ncodec_name=h264\ncodec_type=video\nDISPOSITION:default=1\n[/STREAM]\n\
                    [STREAM]\nindex=1\ncodec_name=aac\ncodec_type=audio\nchannels=2\nDISPOSITION:default=1\n\
                    TAG:title=Normalized +15.0 dB (media-enrich)\nTAG:HANDLER_NAME=SoundHandler\n[/STREAM]\n\
                    [FORMAT]\nduration=40.000000\nTAG:encoder=media-enrich; captions=srt:sha256:ab\n[/FORMAT]\n";
        let p = parse_probe(text);
        assert_eq!((p.count(Kind::Video), p.count(Kind::Audio)), (1, 1));
        let a = p.audio().next().unwrap();
        assert!(a.default && a.channels == 2 && is_marked(&a.label));
        assert_eq!(p.duration, 40.0);
        assert_eq!(p.encoder, "media-enrich; captions=srt:sha256:ab");
    }
}
