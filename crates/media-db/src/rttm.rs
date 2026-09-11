//! Speaker diarization as an `.rttm` sidecar (NIST Rich Transcription
//! Time Marked), the format every diarizer writes: one `SPEAKER` line per
//! stretch of speech, whitespace separated —
//!
//! ```text
//! SPEAKER <file> <channel> <start s> <duration s> <NA> <NA> <label> <NA> <NA>
//! ```
//!
//! The sidecar is time-based, never cue-based, so it stays valid when the
//! `.srt` beside it is corrected or replaced. The server joins the two at
//! request time: each caption cue takes the label whose speech overlaps
//! it the most, written into the WebVTT as a voice span (`<v Label>…</v>`),
//! the one caption construct browsers carry a speaker in. A cue whose
//! lines each open with a dash — the convention for two people in one
//! cue — is split by line, each line placed proportionally along the
//! cue's time and labelled on its own.

/// One stretch of speech.
#[derive(Debug, Clone, PartialEq)]
pub struct Segment {
    pub start: f64,
    pub end: f64,
    pub speaker: String,
}

/// The `SPEAKER` lines of an RTTM file, in time order. Other line types,
/// comments and malformed lines are skipped; a file with no usable line
/// is simply empty.
pub fn parse(text: &str) -> Vec<Segment> {
    let mut out: Vec<Segment> = text
        .lines()
        .filter_map(|line| {
            let mut f = line.split_whitespace();
            if f.next()? != "SPEAKER" {
                return None;
            }
            let _file = f.next()?;
            let _channel = f.next()?;
            let start: f64 = f.next()?.parse().ok()?;
            let dur: f64 = f.next()?.parse().ok()?;
            let _ortho = f.next()?;
            let _stype = f.next()?;
            let speaker = f.next()?;
            if !(start.is_finite() && dur.is_finite()) || start < 0.0 || dur <= 0.0 {
                return None;
            }
            Some(Segment { start, end: start + dur, speaker: clean_label(speaker) })
        })
        .collect();
    out.sort_by(|a, b| a.start.partial_cmp(&b.start).unwrap_or(std::cmp::Ordering::Equal));
    out
}

/// A label safe inside a voice tag: the WebVTT tag delimiters and the
/// entity introducer go, and anything left is at most a modest length.
fn clean_label(raw: &str) -> String {
    raw.chars()
        .filter(|c| !matches!(c, '<' | '>' | '&'))
        .take(64)
        .collect()
}

/// The speaker with the most speech inside `[start, end)`, if anyone
/// speaks there at all.
pub fn speaker_for(segments: &[Segment], start: f64, end: f64) -> Option<&str> {
    let mut best: Option<(&str, f64)> = None;
    let mut totals: Vec<(&str, f64)> = Vec::new();
    for s in segments {
        if s.end <= start {
            continue;
        }
        if s.start >= end {
            break;
        }
        let overlap = s.end.min(end) - s.start.max(start);
        if overlap <= 0.0 {
            continue;
        }
        match totals.iter_mut().find(|(who, _)| *who == s.speaker) {
            Some(t) => t.1 += overlap,
            None => totals.push((s.speaker.as_str(), overlap)),
        }
    }
    for (who, total) in totals {
        if best.is_none_or(|(_, b)| total > b) {
            best = Some((who, total));
        }
    }
    best.map(|(who, _)| who)
}

/// A WebVTT body with voice spans from `segments`: each cue's payload is
/// wrapped in `<v Label>`; the header, notes, cue identifiers and timing
/// lines pass through untouched, as does a cue nobody was found speaking
/// in. Nothing else about the text changes, so a file with no matching
/// speech comes back byte-for-byte.
pub fn tag_vtt(vtt: &str, segments: &[Segment]) -> String {
    if segments.is_empty() {
        return vtt.to_string();
    }
    let mut out = String::with_capacity(vtt.len() + vtt.len() / 8);
    let mut lines = vtt.lines().peekable();
    while let Some(line) = lines.next() {
        out.push_str(line);
        out.push('\n');
        let Some((start, end)) = timing(line) else { continue };
        // The payload: every line up to the blank that ends the block.
        let mut payload: Vec<&str> = Vec::new();
        while let Some(next) = lines.peek() {
            if next.trim().is_empty() {
                break;
            }
            payload.push(lines.next().unwrap());
        }
        if payload.is_empty() {
            continue;
        }
        let dashed = payload.len() > 1 && payload.iter().all(|l| is_dash_line(l));
        if dashed {
            // Each line takes a slice of the cue's time in proportion to
            // its length, and is labelled from its own slice.
            let total: usize = payload.iter().map(|l| l.chars().count().max(1)).sum();
            let mut at = start;
            for l in &payload {
                let share = (end - start) * l.chars().count().max(1) as f64 / total as f64;
                let (s, e) = (at, at + share);
                at = e;
                push_voiced(&mut out, l, speaker_for(segments, s, e));
            }
        } else {
            let who = speaker_for(segments, start, end);
            match who {
                Some(who) => {
                    out.push_str("<v ");
                    out.push_str(who);
                    out.push('>');
                    out.push_str(&payload.join("\n"));
                    out.push_str("</v>\n");
                }
                None => {
                    out.push_str(&payload.join("\n"));
                    out.push('\n');
                }
            }
        }
    }
    // lines() drops a final newline; the original's presence decides.
    if !vtt.ends_with('\n') {
        out.pop();
    }
    out
}

fn push_voiced(out: &mut String, line: &str, who: Option<&str>) {
    match who {
        Some(who) => {
            out.push_str("<v ");
            out.push_str(who);
            out.push('>');
            out.push_str(line);
            out.push_str("</v>\n");
        }
        None => {
            out.push_str(line);
            out.push('\n');
        }
    }
}

fn is_dash_line(line: &str) -> bool {
    let t = line.trim_start();
    // Any leading tag (<i>, <c.x>) may precede the dash.
    let t = strip_tags_prefix(t);
    t.starts_with('-') || t.starts_with('–') || t.starts_with('—')
}

fn strip_tags_prefix(mut s: &str) -> &str {
    while let Some(rest) = s.strip_prefix('<') {
        match rest.find('>') {
            Some(i) => s = rest[i + 1..].trim_start(),
            None => break,
        }
    }
    s
}

/// The start and end of a WebVTT timing line, in seconds.
fn timing(line: &str) -> Option<(f64, f64)> {
    let (a, b) = line.split_once("-->")?;
    let start = timestamp(a.trim())?;
    let end = timestamp(b.split_whitespace().next()?)?;
    (end >= start).then_some((start, end))
}

/// `hh:mm:ss.mmm` or `mm:ss.mmm` (a comma for the fraction tolerated).
fn timestamp(s: &str) -> Option<f64> {
    let mut parts: Vec<&str> = s.split(':').collect();
    if parts.is_empty() || parts.len() > 3 {
        return None;
    }
    let secs: f64 = parts.pop()?.replace(',', ".").parse().ok()?;
    let mins: f64 = parts.pop().map_or(Some(0.0), |m| m.parse().ok())?;
    let hours: f64 = parts.pop().map_or(Some(0.0), |h| h.parse().ok())?;
    let t = hours * 3600.0 + mins * 60.0 + secs;
    t.is_finite().then_some(t)
}

#[cfg(test)]
mod tests {
    use super::*;

    const RTTM: &str = "\
SPEAKER ep 1 0.50 3.00 <NA> <NA> Alice <NA> <NA>
;; a comment
SPKR-INFO ep 1 <NA> <NA> <NA> unknown Alice <NA>
SPEAKER ep 1 3.60 2.00 <NA> <NA> Bob <NA> <NA>
SPEAKER ep 1 bad 2.00 <NA> <NA> Bob <NA> <NA>
SPEAKER ep 1 10.0 1.0 <NA> <NA> <Al&ice> <NA> <NA>
";

    #[test]
    fn parses_speaker_lines_only_in_time_order() {
        let segs = parse(RTTM);
        assert_eq!(segs.len(), 3);
        assert_eq!(segs[0], Segment { start: 0.5, end: 3.5, speaker: "Alice".into() });
        assert_eq!(segs[1].speaker, "Bob");
        assert_eq!(segs[2].speaker, "Alice", "tag delimiters stripped from the label");
        assert!(parse("").is_empty());
        assert!(parse("SPEAKER ep 1 1.0 0.0 <NA> <NA> X <NA> <NA>").is_empty(), "zero duration");
    }

    #[test]
    fn the_speaker_with_the_most_overlap_wins() {
        let segs = parse(RTTM);
        assert_eq!(speaker_for(&segs, 0.0, 1.0), Some("Alice"));
        assert_eq!(speaker_for(&segs, 3.0, 4.6), Some("Bob"), "0.5 of Alice, 1.0 of Bob");
        assert_eq!(speaker_for(&segs, 3.0, 4.0), Some("Alice"), "0.5 of Alice, 0.4 of Bob");
        assert_eq!(speaker_for(&segs, 6.0, 9.0), None);
    }

    #[test]
    fn cues_get_voice_spans_and_everything_else_passes_through() {
        let segs = parse(RTTM);
        let vtt = "WEBVTT\n\nNOTE hello\n\n1\n00:00:01.000 --> 00:00:02.000\nHi there.\n\n00:00:07.000 --> 00:00:08.000 line:90%\nSilence.\n\n00:00:03.000 --> 00:00:05.000\n<i>Well,</i>\nhello.\n";
        let tagged = tag_vtt(vtt, &segs);
        assert_eq!(
            tagged,
            "WEBVTT\n\nNOTE hello\n\n1\n00:00:01.000 --> 00:00:02.000\n<v Alice>Hi there.</v>\n\n00:00:07.000 --> 00:00:08.000 line:90%\nSilence.\n\n00:00:03.000 --> 00:00:05.000\n<v Bob><i>Well,</i>\nhello.</v>\n"
        );
        assert_eq!(tag_vtt(vtt, &[]), vtt, "no segments: untouched");
        assert_eq!(tag_vtt("WEBVTT\n\n00:00:07.000 --> 00:00:08.000\nx", &segs), "WEBVTT\n\n00:00:07.000 --> 00:00:08.000\nx", "no trailing newline kept");
    }

    #[test]
    fn dashed_lines_split_a_shared_cue() {
        let segs = parse(RTTM);
        // 2.0–5.0: Alice until 3.5, Bob from 3.6. Lines split 1:1 → 2.0–3.5 Alice, 3.5–5.0 Bob.
        let vtt = "WEBVTT\n\n00:00:02.000 --> 00:00:05.000\n- Hello?\n- Hello!\n";
        assert_eq!(
            tag_vtt(vtt, &segs),
            "WEBVTT\n\n00:00:02.000 --> 00:00:05.000\n<v Alice>- Hello?</v>\n<v Bob>- Hello!</v>\n"
        );
        // A single dashed line is a normal cue.
        let one = "WEBVTT\n\n00:00:02.000 --> 00:00:03.000\n- Hello?\n";
        assert_eq!(tag_vtt(one, &segs), "WEBVTT\n\n00:00:02.000 --> 00:00:03.000\n<v Alice>- Hello?</v>\n");
    }

    #[test]
    fn timestamps_in_both_shapes() {
        assert_eq!(timestamp("00:01:02.500"), Some(62.5));
        assert_eq!(timestamp("01:02.500"), Some(62.5));
        assert_eq!(timestamp("01:02,500"), Some(62.5));
        assert_eq!(timestamp("x"), None);
        assert_eq!(timing("00:00:01.000 --> 00:00:02.000 align:start"), Some((1.0, 2.0)));
    }
}
