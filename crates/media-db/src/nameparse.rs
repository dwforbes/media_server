use std::sync::OnceLock;

use regex::Regex;

/// Normalize scene-style names: dots/underscores to spaces, collapse runs,
/// trim stray separators.
pub fn clean_name(s: &str) -> String {
    let replaced: String = s
        .chars()
        .map(|c| if c == '.' || c == '_' { ' ' } else { c })
        .collect();
    replaced
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .trim_matches(|c: char| c == '-' || c == ' ' || c == '(' || c == '[')
        .to_string()
}

/// Sort key: ignore leading punctuation ("Wuthering Heights" — the 2026
/// film's official title includes the quotes — must sort under W), then
/// drop a leading English article.
pub fn sort_title(title: &str) -> String {
    let stripped = title.trim_start_matches(|c: char| !c.is_alphanumeric());
    let base = if stripped.is_empty() { title } else { stripped };
    let lower = base.to_lowercase();
    for article in ["the ", "a ", "an "] {
        if lower.starts_with(article) {
            return base[article.len()..].to_string();
        }
    }
    base.to_string()
}

fn year_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    // Bare year; delimiter boundaries are checked manually so that adjacent
    // year-like tokens ("Blade.Runner.2049.2017") each get considered —
    // a regex that consumes its delimiters would swallow the next token's
    // leading delimiter and miss it.
    RE.get_or_init(|| Regex::new(r"(19|20)\d{2}").unwrap())
}

fn is_name_delimiter(byte: u8) -> bool {
    matches!(byte, b'.' | b' ' | b'_' | b'-' | b'(' | b')' | b'[' | b']')
}

/// Release-tag words that sometimes precede the year in scene names
/// ("Wall.Street.REMASTERED.1987") and would pollute the title. Stripped
/// only from the end of a parsed title, and never down to nothing.
const JUNK_WORDS: &[&str] = &[
    "remastered", "unrated", "extended", "uncut", "repack", "proper", "imax",
    "theatrical", "criterion", "restored", "bluray", "brrip", "webrip",
    "hdrip", "dvdrip", "2160p", "1080p", "720p", "4k", "x264", "x265", "hevc",
];

fn strip_junk(title: String) -> String {
    let mut words: Vec<&str> = title.split(' ').collect();
    while words.len() > 1 {
        let last = words.last().unwrap().to_lowercase();
        if JUNK_WORDS.contains(&last.as_str()) {
            words.pop();
        } else {
            break;
        }
    }
    words.join(" ")
}

/// Parse a movie filename stem like "Heat (1995)" or "Heat.1995.1080p".
/// Returns (title, year). The year must appear after some title text so a
/// name that IS a year ("2012.mkv") stays a title.
pub fn movie(stem: &str) -> (String, Option<i64>) {
    let bytes = stem.as_bytes();
    let mut best: Option<(usize, i64)> = None;
    for m in year_re().find_iter(stem) {
        let delimited_before = m.start() > 0 && is_name_delimiter(bytes[m.start() - 1]);
        let delimited_after = m.end() == stem.len() || is_name_delimiter(bytes[m.end()]);
        if delimited_before && delimited_after {
            best = Some((m.start(), m.as_str().parse().unwrap()));
        }
    }
    if let Some((position, year)) = best {
        let title = strip_junk(clean_name(&stem[..position]));
        if !title.is_empty() {
            return (title, Some(year));
        }
    }
    (strip_junk(clean_name(stem)), None)
}

fn episode_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    // The trailing (?:E\d+)* consumes multi-episode markers (S05E24E25) so
    // they don't pollute the title; the file is catalogued as its first
    // episode, the usual convention.
    RE.get_or_init(|| {
        Regex::new(r"(?i)(?:S(\d{1,2})[\. _-]*E(\d{1,3})(?:[\. _-]*E\d{1,3})*)|(?:(\d{1,2})x(\d{2,3}))")
            .unwrap()
    })
}

/// Drop a trailing year — "The Boys (2019)", "A Woman of Substance 2026" —
/// so release-name variants of the same show group together.
fn strip_series_year(series: String) -> String {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"\s*\(?(19|20)\d{2}\)?\s*$").unwrap());
    let stripped = re.replace(&series, "").trim().to_string();
    if stripped.is_empty() {
        series
    } else {
        stripped
    }
}

/// Truncate an episode title at the first release-junk token, so
/// "Grave Danger 1080p Web-DL x264-OFT" becomes "Grave Danger" and a
/// title that is pure junk becomes empty (callers fall back to Episode N).
fn cut_release_junk(title: &str) -> String {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(
            r"(?i)^(2160p|1080p|720p|480p|4k|webrip|web-?dl|web|bluray|blu-ray|brrip|bdrip|hdtv|hdrip|dvdrip|hevc|x264|x265|h\.?264|h\.?265|av1|aac\d*|ac3|dd[p+]?[\d\.]*|dts(-?hd)?|atmos|repack|proper|internal|remastered|extended|uncut|unrated|amzn|nf|hulu|dsnp|hmax|10bit|8bit|hdr\d*|dv|sdr|multi|dubbed|subbed)\b",
        )
        .unwrap()
    });
    let mut kept: Vec<&str> = Vec::new();
    for token in title.split_whitespace() {
        let bare = token.trim_start_matches(['(', '[']);
        if re.is_match(bare) {
            break;
        }
        kept.push(token);
    }
    kept.join(" ")
        .trim_matches(|c: char| c == '-' || c == ' ' || c == '(' || c == '[')
        .to_string()
}

#[derive(Debug, PartialEq)]
pub struct ParsedEpisode {
    pub series: String,
    pub season: i64,
    pub episode: i64,
    pub title: String,
}

/// Parse a TV episode from its stem plus ancestor directory names, e.g.
/// "The Wire S01E03 The Buys" or "The Wire/Season 1/1x03 The Buys".
/// Falls back to directory names for the series when the stem has none.
pub fn episode(stem: &str, parent_dirs: &[&str]) -> Option<ParsedEpisode> {
    let caps = episode_re().captures(stem)?;
    let m = caps.get(0).unwrap();
    let (season, episode) = if let (Some(s), Some(e)) = (caps.get(1), caps.get(2)) {
        (s.as_str().parse().ok()?, e.as_str().parse().ok()?)
    } else {
        (
            caps.get(3)?.as_str().parse().ok()?,
            caps.get(4)?.as_str().parse().ok()?,
        )
    };

    let mut series = strip_series_year(clean_name(&stem[..m.start()]));
    if series.is_empty() {
        // Nearest ancestor that isn't a season folder.
        for dir in parent_dirs.iter().rev() {
            let cleaned = clean_name(dir);
            let lower = cleaned.to_lowercase();
            if !lower.starts_with("season") && !lower.starts_with("series") && !cleaned.is_empty()
            {
                series = strip_series_year(cleaned);
                break;
            }
        }
    }
    if series.is_empty() {
        series = "Unknown Series".to_string();
    }

    let mut title = cut_release_junk(&clean_name(&stem[m.end()..]));
    if title.is_empty() {
        title = format!("Episode {episode}");
    }
    Some(ParsedEpisode { series, season, episode, title })
}

/// Fallback music metadata from an "Artist/Album/NN - Title" style path.
/// Returns (artist, album, track_no, title).
pub fn music_from_path(
    stem: &str,
    parent_dirs: &[&str],
) -> (Option<String>, Option<String>, Option<i64>, String) {
    static TRACK_RE: OnceLock<Regex> = OnceLock::new();
    let track_re =
        TRACK_RE.get_or_init(|| Regex::new(r"^(\d{1,3})[\. _-]+(.+)$").unwrap());

    let (track_no, title) = match track_re.captures(stem) {
        Some(caps) => (
            caps.get(1).unwrap().as_str().parse().ok(),
            clean_name(caps.get(2).unwrap().as_str()),
        ),
        None => (None, clean_name(stem)),
    };
    let n = parent_dirs.len();
    let album = if n >= 1 { Some(clean_name(parent_dirs[n - 1])) } else { None };
    let artist = if n >= 2 { Some(clean_name(parent_dirs[n - 2])) } else { None };
    (artist, album, track_no, title)
}
