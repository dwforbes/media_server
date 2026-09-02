//! Provenance of an embedded caption track. When media-enrich muxes a
//! `.srt` sidecar into an MP4 it records the SHA-256 of the text it
//! embedded in the container's ©too ("encoding tool") atom — the one
//! atom that is meant to name the tool that wrote the file, which no
//! player displays as content. ffmpeg writes it from the `encoding_tool`
//! metadata key and reads it back as `encoder`; media-title reads the
//! atom directly. A later enrichment compares the sidecar against the
//! recorded hash and re-embeds when they differ, so a corrected sidecar
//! flows into the file. Anything that remuxes the file without the key
//! (a plain ffmpeg copy) replaces the atom with its own name, and the
//! captions are then of unknown provenance — never replaced.

/// What the tag looks like before the 64 hex digits.
pub const TAG_PREFIX: &str = "media-enrich; captions=srt:sha256:";

/// The ©too text for captions embedded from a sidecar with this hash.
pub fn tag(sha256_hex: &str) -> String {
    format!("{TAG_PREFIX}{sha256_hex}")
}

/// The recorded sidecar hash, if the tag is one of ours.
pub fn recorded_hash(encoder_tag: &str) -> Option<&str> {
    let rest = encoder_tag.trim().strip_prefix(TAG_PREFIX)?;
    let hex = rest.split(|c: char| c.is_whitespace() || c == ';').next()?;
    (hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_hexdigit())).then_some(hex)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tag_round_trips_and_rejects_strangers() {
        let hex = "ab".repeat(32);
        assert_eq!(recorded_hash(&tag(&hex)), Some(hex.as_str()));
        assert_eq!(recorded_hash(&format!("{} ; more", tag(&hex))), Some(hex.as_str()));
        assert_eq!(recorded_hash("Lavf63.1.101"), None);
        assert_eq!(recorded_hash("media-enrich; captions=srt:sha256:short"), None);
        assert_eq!(recorded_hash(""), None);
    }
}
