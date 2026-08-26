//! Subtitle track census and selection, shared by the server's on-demand
//! extraction and media-enrich's sidecar extraction so both pick the same
//! track: full English captions (SDH preferred) over a "forced" track that
//! only covers foreign-language passages.
//!
//! Run ffprobe with [`ffprobe_args`] and feed its stdout to
//! [`parse_ffprobe`]; [`best_text_track`] picks the track to extract.

use std::cmp::Reverse;

/// Codecs ffmpeg can turn into SubRip/WebVTT text. Bitmap formats (PGS,
/// VobSub) are not extractable as text.
pub const TEXT_CODECS: &[&str] =
    &["subrip", "srt", "ass", "ssa", "mov_text", "webvtt", "text", "subviewer"];

/// MP4 stores a track title as the handler name; this is ffmpeg's own
/// placeholder there, not a title.
const FFMPEG_DEFAULT_HANDLER: &str = "SubtitleHandler";

/// ffprobe arguments listing every subtitle stream, in stream order, in
/// the wrapped `default` writer form that [`parse_ffprobe`] reads.
pub fn ffprobe_args() -> [&'static str; 8] {
    [
        "-v", "error",
        "-select_streams", "s",
        "-show_entries",
        "stream=codec_name:stream_tags=language,title,handler_name:stream_disposition=default,forced,hearing_impaired",
        "-of", "default",
    ]
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Track {
    /// Position among the file's subtitle streams: ffmpeg's `0:s:N`.
    pub ordinal: usize,
    pub codec: String,
    pub language: Option<String>,
    pub title: Option<String>,
    pub default: bool,
    pub forced: bool,
    pub hearing_impaired: bool,
}

impl Track {
    pub fn is_text(&self) -> bool {
        TEXT_CODECS.contains(&self.codec.as_str())
    }

    fn title_words(&self) -> Vec<String> {
        self.title
            .as_deref()
            .unwrap_or("")
            .split(|c: char| !c.is_alphanumeric())
            .filter(|w| !w.is_empty())
            .map(|w| w.to_ascii_lowercase())
            .collect()
    }

    pub fn is_english(&self) -> bool {
        match self.language.as_deref().map(|l| l.to_ascii_lowercase()) {
            Some(l) if l == "eng" || l == "en" => true,
            // "und"/missing: fall back to the title ("English", "English (SDH)").
            Some(l) if l != "und" => false,
            _ => self.title_words().iter().any(|w| w == "english"),
        }
    }

    /// Subtitles for the deaf and hard of hearing: the full dialogue plus
    /// sound cues. Flagged in the container, or named in the title
    /// ("English (SDH)", "English [CC]", "HoH").
    pub fn is_sdh(&self) -> bool {
        self.hearing_impaired
            || self
                .title_words()
                .iter()
                .any(|w| matches!(w.as_str(), "sdh" | "cc" | "hoh" | "hearing"))
    }

    /// Short human description: "eng, SDH", "fra, forced", "eng".
    pub fn describe(&self) -> String {
        let mut parts = vec![self.language.clone().unwrap_or_else(|| "und".into())];
        if self.is_sdh() {
            parts.push("SDH".into());
        }
        if self.forced {
            parts.push("forced".into());
        }
        parts.join(", ")
    }
}

/// Parse the `-of default` stream listing produced with [`ffprobe_args`].
pub fn parse_ffprobe(text: &str) -> Vec<Track> {
    let mut tracks = Vec::new();
    let mut current: Option<Track> = None;
    for line in text.lines() {
        let line = line.trim();
        match line {
            "[STREAM]" => {
                current = Some(Track { ordinal: tracks.len(), ..Track::default() });
            }
            "[/STREAM]" => {
                if let Some(track) = current.take() {
                    tracks.push(track);
                }
            }
            _ => {
                let Some(track) = current.as_mut() else { continue };
                let Some((key, value)) = line.split_once('=') else { continue };
                match key {
                    "codec_name" => track.codec = value.to_string(),
                    "TAG:language" => track.language = Some(value.to_string()),
                    "TAG:title" => track.title = Some(value.to_string()),
                    "TAG:handler_name" => {
                        if track.title.is_none() && value != FFMPEG_DEFAULT_HANDLER {
                            track.title = Some(value.to_string());
                        }
                    }
                    "DISPOSITION:default" => track.default = value == "1",
                    "DISPOSITION:forced" => track.forced = value == "1",
                    "DISPOSITION:hearing_impaired" => track.hearing_impaired = value == "1",
                    _ => {}
                }
            }
        }
    }
    tracks
}

/// The text track worth extracting, if any. Forced tracks (foreign-language
/// passages only) rank below everything else; among the rest, English
/// beats other languages and SDH beats plain captions; ties go to the
/// container's default flag, then to the earlier track.
pub fn best_text_track(tracks: &[Track]) -> Option<&Track> {
    tracks
        .iter()
        .filter(|t| t.is_text())
        .max_by_key(|t| (!t.forced, t.is_english(), t.is_sdh(), t.default, Reverse(t.ordinal)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stream(codec: &str, lang: &str, title: Option<&str>, default: bool, forced: bool, hi: bool) -> String {
        let mut s = format!(
            "[STREAM]\ncodec_name={codec}\nDISPOSITION:default={}\nDISPOSITION:forced={}\nDISPOSITION:hearing_impaired={}\nTAG:language={lang}\n",
            default as u8, forced as u8, hi as u8
        );
        if let Some(title) = title {
            s.push_str(&format!("TAG:title={title}\n"));
        }
        s.push_str("[/STREAM]\n");
        s
    }

    #[test]
    fn parses_streams_in_order() {
        let text = stream("mov_text", "eng", None, true, true, false) + &stream("subrip", "fra", Some("Français"), false, false, false);
        let tracks = parse_ffprobe(&text);
        assert_eq!(tracks.len(), 2);
        assert_eq!(tracks[0].ordinal, 0);
        assert!(tracks[0].forced && tracks[0].default);
        assert_eq!(tracks[1].language.as_deref(), Some("fra"));
        assert_eq!(tracks[1].title.as_deref(), Some("Français"));
    }

    #[test]
    fn sdh_beats_a_forced_track_listed_first() {
        let text = stream("mov_text", "eng", None, true, true, false)
            + &stream("mov_text", "eng", None, false, false, false)
            + &stream("mov_text", "eng", None, false, false, true);
        let tracks = parse_ffprobe(&text);
        assert_eq!(best_text_track(&tracks).unwrap().ordinal, 2);
    }

    #[test]
    fn sdh_is_recognized_from_the_title() {
        let text = stream("subrip", "eng", Some("English"), true, false, false)
            + &stream("subrip", "eng", Some("English (SDH)"), false, false, false);
        let tracks = parse_ffprobe(&text);
        assert_eq!(best_text_track(&tracks).unwrap().ordinal, 1);
        assert_eq!(best_text_track(&tracks).unwrap().describe(), "eng, SDH");
    }

    #[test]
    fn plain_english_beats_other_languages_and_forced_is_last_resort() {
        let text = stream("subrip", "fra", None, false, false, true) + &stream("subrip", "eng", None, false, false, false);
        assert_eq!(best_text_track(&parse_ffprobe(&text)).unwrap().ordinal, 1);
        let text = stream("subrip", "eng", None, true, true, false) + &stream("subrip", "fra", None, false, false, false);
        assert_eq!(best_text_track(&parse_ffprobe(&text)).unwrap().ordinal, 1);
        let text = stream("subrip", "eng", None, true, true, false);
        assert_eq!(best_text_track(&parse_ffprobe(&text)).unwrap().ordinal, 0);
    }

    #[test]
    fn bitmap_only_yields_nothing_and_handler_name_stands_in_for_title() {
        let text = stream("hdmv_pgs_subtitle", "eng", None, true, false, false);
        assert!(best_text_track(&parse_ffprobe(&text)).is_none());
        let text = "[STREAM]\ncodec_name=mov_text\nTAG:language=und\nTAG:handler_name=English (SDH)\n[/STREAM]\n";
        let tracks = parse_ffprobe(text);
        assert!(tracks[0].is_english() && tracks[0].is_sdh());
        let text = "[STREAM]\ncodec_name=mov_text\nTAG:language=und\nTAG:handler_name=SubtitleHandler\n[/STREAM]\n";
        assert!(parse_ffprobe(text)[0].title.is_none());
    }
}
