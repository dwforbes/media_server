/// Media file extensions recognized by the scanner, with their MIME types.
/// Extension matching is case-insensitive; compare against lowercased input.
pub const VIDEO_EXTENSIONS: &[(&str, &str)] = &[
    ("mkv", "video/x-matroska"),
    ("mp4", "video/mp4"),
    ("m4v", "video/mp4"),
    ("mov", "video/quicktime"),
    ("avi", "video/x-msvideo"),
    ("wmv", "video/x-ms-wmv"),
    ("webm", "video/webm"),
    ("mpg", "video/mpeg"),
    ("mpeg", "video/mpeg"),
    ("ts", "video/mp2t"),
    ("m2ts", "video/mp2t"),
    ("flv", "video/x-flv"),
];

pub const AUDIO_EXTENSIONS: &[(&str, &str)] = &[
    ("mp3", "audio/mpeg"),
    ("flac", "audio/flac"),
    ("m4a", "audio/mp4"),
    ("aac", "audio/aac"),
    ("ogg", "audio/ogg"),
    ("opus", "audio/ogg"),
    ("wav", "audio/wav"),
    ("aiff", "audio/aiff"),
    ("wma", "audio/x-ms-wma"),
];

/// MIME type for a lowercase extension, restricted to video or audio
/// depending on what the containing root holds.
pub fn mime_for_extension(ext: &str, video: bool) -> Option<&'static str> {
    let table = if video { VIDEO_EXTENSIONS } else { AUDIO_EXTENSIONS };
    table.iter().find(|(e, _)| *e == ext).map(|(_, m)| *m)
}
