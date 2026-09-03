use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaKind {
    Movies,
    Music,
    Tv,
}

impl MediaKind {
    pub fn as_str(self) -> &'static str {
        match self {
            MediaKind::Movies => "movies",
            MediaKind::Music => "music",
            MediaKind::Tv => "tv",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "movies" => Some(MediaKind::Movies),
            "music" => Some(MediaKind::Music),
            "tv" => Some(MediaKind::Tv),
            _ => None,
        }
    }

    /// True when files under this root are video.
    pub fn is_video(self) -> bool {
        !matches!(self, MediaKind::Music)
    }
}

impl fmt::Display for MediaKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What a skippable segment is, which decides the label on the player's
/// skip button. Stored as text in the segments table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SegmentKind {
    Intro,
    Credits,
    Recap,
    Commercial,
}

impl SegmentKind {
    pub fn as_str(self) -> &'static str {
        match self {
            SegmentKind::Intro => "intro",
            SegmentKind::Credits => "credits",
            SegmentKind::Recap => "recap",
            SegmentKind::Commercial => "commercial",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "intro" => Some(SegmentKind::Intro),
            "credits" => Some(SegmentKind::Credits),
            "recap" => Some(SegmentKind::Recap),
            "commercial" => Some(SegmentKind::Commercial),
            _ => None,
        }
    }
}

/// A skippable stretch of one video file's timeline, ingested from named
/// chapter markers or a .edl sidecar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Segment {
    pub kind: SegmentKind,
    pub start_ms: i64,
    pub end_ms: i64,
}

#[derive(Debug, Clone)]
pub struct Root {
    pub id: i64,
    pub path: String,
    pub kind: MediaKind,
}

/// A row in `files`, as stored.
#[derive(Debug, Clone)]
pub struct FileRow {
    pub id: i64,
    pub root_id: i64,
    pub rel_path: String,
    pub size: i64,
    pub mtime: i64,
    pub kind: MediaKind,
    pub mime: String,
    pub status: String,
}

/// Technical attributes extracted from the media itself.
#[derive(Debug, Clone, Default)]
pub struct TechInfo {
    pub container: Option<String>,
    pub duration_ms: Option<i64>,
    pub width: Option<i64>,
    pub height: Option<i64>,
    /// Average frames per second of the video stream (23.976, 25, ...).
    pub frame_rate: Option<f64>,
    pub video_codec: Option<String>,
    pub audio_codec: Option<String>,
    /// Human codec label for the (first) audio stream: "AAC LC", "FLAC".
    pub audio_profile: Option<String>,
    /// kbps
    pub audio_bitrate: Option<i64>,
    /// Hz
    pub audio_sample_rate: Option<i64>,
    /// Bits per sample; lossless/PCM formats only.
    pub audio_bit_depth: Option<i64>,
    pub audio_channels: Option<i64>,
}

/// One playable file of an item. An item usually has exactly one; a movie
/// present in several qualities carries the extras here, best-first.
#[derive(Debug, Clone)]
pub struct Rendition {
    pub file_id: i64,
    pub mime: String,
    pub size: i64,
    pub duration_ms: Option<i64>,
    pub width: Option<i64>,
    pub height: Option<i64>,
}

impl Rendition {
    /// Ordering key: pixel count, then file size.
    pub fn quality(&self) -> (i64, i64) {
        (self.width.unwrap_or(0) * self.height.unwrap_or(0), self.size)
    }
}

/// Everything the server needs to render one playable item in DIDL-Lite.
/// Populated by kind-specific joins; unrelated fields stay None.
#[derive(Debug, Clone)]
pub struct BrowseItem {
    pub file_id: i64,
    pub kind: MediaKind,
    pub title: String,
    pub mime: String,
    pub size: i64,
    pub duration_ms: Option<i64>,
    pub width: Option<i64>,
    pub height: Option<i64>,
    /// Additional lower-quality copies of the same item, best-first.
    pub renditions: Vec<Rendition>,
    /// The item has artwork servable at /art/{art_file_id or file_id}.
    pub has_art: bool,
    /// When the file was last extracted (files.updated_at) — artwork
    /// changes re-extract, so this versions the art URL for long caching.
    pub art_version: Option<i64>,
    /// Set when the artwork lives on a merged rendition, not the primary.
    pub art_file_id: Option<i64>,
    // movies / music
    pub year: Option<i64>,
    pub genre: Option<String>,
    // movies
    pub director: Option<String>,
    pub rating: Option<f64>,
    // music
    pub artist: Option<String>,
    pub album: Option<String>,
    pub track_no: Option<i64>,
    // tv
    pub series: Option<String>,
    pub season: Option<i64>,
    pub episode: Option<i64>,
}

impl BrowseItem {
    pub fn new(file_id: i64, kind: MediaKind, title: String, mime: String, size: i64) -> Self {
        BrowseItem {
            file_id,
            kind,
            title,
            mime,
            size,
            duration_ms: None,
            width: None,
            height: None,
            renditions: Vec::new(),
            has_art: false,
            art_version: None,
            art_file_id: None,
            year: None,
            genre: None,
            director: None,
            rating: None,
            artist: None,
            album: None,
            track_no: None,
            series: None,
            season: None,
            episode: None,
        }
    }
}

impl BrowseItem {
    pub fn primary_rendition(&self) -> Rendition {
        Rendition {
            file_id: self.file_id,
            mime: self.mime.clone(),
            size: self.size,
            duration_ms: self.duration_ms,
            width: self.width,
            height: self.height,
        }
    }

    pub fn set_primary(&mut self, r: Rendition) {
        self.file_id = r.file_id;
        self.mime = r.mime;
        self.size = r.size;
        self.duration_ms = r.duration_ms;
        self.width = r.width;
        self.height = r.height;
    }
}

/// What the streaming endpoint needs to serve a file.
#[derive(Debug, Clone)]
pub struct ServableFile {
    pub abs_path: std::path::PathBuf,
    pub mime: String,
}
