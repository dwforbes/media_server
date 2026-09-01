use anyhow::Result;
use rusqlite::{params, Connection};

use crate::models::{Segment, SegmentKind};

/// Replace every stored segment for a file with `segments`, all from one
/// `source` ('chapters', 'edl', or the detector's 'audio'). Called on
/// every extraction, so a vanished sidecar or renamed chapter clears its
/// segments — with one carve-out: a deliberate source with nothing to say
/// leaves 'audio' rows alone. Extraction re-runs for reasons unrelated to
/// segments (a changed .nfo, a schema poke), and the file's fingerprints
/// stay current through it, so wiped detector segments would never be
/// re-derived. Deliberate rows still take the file over when present:
/// the detector refuses such files, so stale 'audio' rows can't linger
/// beside them.
pub fn replace_for_file(
    conn: &Connection,
    file_id: i64,
    source: &str,
    segments: &[Segment],
) -> Result<()> {
    if segments.is_empty() && source != "audio" {
        conn.execute(
            "DELETE FROM segments WHERE file_id = ?1 AND source != 'audio'",
            [file_id],
        )?;
        return Ok(());
    }
    conn.execute("DELETE FROM segments WHERE file_id = ?1", [file_id])?;
    let mut stmt = conn.prepare(
        "INSERT OR REPLACE INTO segments (file_id, start_ms, end_ms, kind, source)
         VALUES (?1, ?2, ?3, ?4, ?5)",
    )?;
    for s in segments {
        stmt.execute(params![file_id, s.start_ms, s.end_ms, s.kind.as_str(), source])?;
    }
    Ok(())
}

/// Segments for one file, in timeline order.
pub fn for_file(conn: &Connection, file_id: i64) -> Result<Vec<Segment>> {
    let mut stmt = conn.prepare(
        "SELECT kind, start_ms, end_ms FROM segments WHERE file_id = ?1 ORDER BY start_ms",
    )?;
    let rows = stmt.query_map([file_id], |r| {
        Ok((r.get::<_, String>(0)?, r.get(1)?, r.get(2)?))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (kind, start_ms, end_ms) = row?;
        let Some(kind) = SegmentKind::parse(&kind) else { continue };
        out.push(Segment { kind, start_ms, end_ms });
    }
    Ok(out)
}

/// Whether a file has segments from a deliberate source (chapter markers
/// or a .edl sidecar) that the audio detector must not overwrite.
pub fn has_manual_segments(conn: &Connection, file_id: i64) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM segments WHERE file_id = ?1 AND source != 'audio')",
        [file_id],
        |r| r.get(0),
    )?)
}

// ------------------------------------------------- audio-detector support

/// (series, season) pairs the detector should look at: at least two ready
/// episode files, at least one of them without a current fingerprint row.
pub fn stale_seasons(conn: &Connection, ver: i64) -> Result<Vec<(String, i64)>> {
    let mut stmt = conn.prepare(
        "SELECT te.series, te.season
         FROM tv_episodes te
         JOIN files f ON f.id = te.file_id AND f.status = 'ready'
         LEFT JOIN segment_prints p
                ON p.file_id = f.id AND p.size = f.size AND p.mtime = f.mtime AND p.ver = ?1
         GROUP BY te.series COLLATE NOCASE, te.season
         HAVING COUNT(*) >= 2 AND COUNT(p.file_id) < COUNT(*)
         ORDER BY te.series, te.season",
    )?;
    let rows = stmt.query_map([ver], |r| Ok((r.get(0)?, r.get(1)?)))?;
    Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
}

/// One episode file as the audio detector sees it: identity, where it
/// lives, and its cached fingerprints when they are current.
pub struct AnalysisFile {
    pub file_id: i64,
    pub abs_path: String,
    pub size: i64,
    pub mtime: i64,
    pub episode: i64,
    pub duration_ms: Option<i64>,
    /// Some when a current segment_prints row exists (possibly empty
    /// prints, meaning the audio was undecodable).
    pub prints: Option<(Vec<u32>, Vec<u32>)>,
}

fn decode_print(blob: Vec<u8>) -> Vec<u32> {
    blob.chunks_exact(4)
        .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

fn encode_print(print: &[u32]) -> Vec<u8> {
    print.iter().flat_map(|v| v.to_le_bytes()).collect()
}

/// The ready episode files of one season, with current cached prints.
pub fn season_files(
    conn: &Connection,
    series: &str,
    season: i64,
    ver: i64,
) -> Result<Vec<AnalysisFile>> {
    let mut stmt = conn.prepare(
        "SELECT f.id, r.path || '/' || f.rel_path, f.size, f.mtime, te.episode, f.duration_ms,
                p.head, p.tail
         FROM tv_episodes te
         JOIN files f ON f.id = te.file_id AND f.status = 'ready'
         JOIN roots r ON r.id = f.root_id
         LEFT JOIN segment_prints p
                ON p.file_id = f.id AND p.size = f.size AND p.mtime = f.mtime AND p.ver = ?3
         WHERE te.series = ?1 COLLATE NOCASE AND te.season = ?2
         ORDER BY te.episode, f.id",
    )?;
    let rows = stmt.query_map(params![series, season, ver], |r| {
        let head: Option<Vec<u8>> = r.get(6)?;
        let tail: Option<Vec<u8>> = r.get(7)?;
        Ok(AnalysisFile {
            file_id: r.get(0)?,
            abs_path: r.get(1)?,
            size: r.get(2)?,
            mtime: r.get(3)?,
            episode: r.get(4)?,
            duration_ms: r.get(5)?,
            prints: head.zip(tail).map(|(h, t)| (decode_print(h), decode_print(t))),
        })
    })?;
    Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
}

/// Store (or refresh) a file's fingerprints. Empty prints are stored too:
/// the row is what marks the file analyzed.
pub fn store_prints(
    conn: &Connection,
    file_id: i64,
    size: i64,
    mtime: i64,
    ver: i64,
    head: &[u32],
    tail: &[u32],
) -> Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO segment_prints (file_id, size, mtime, ver, head, tail)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![file_id, size, mtime, ver, encode_print(head), encode_print(tail)],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::MediaKind;
    use crate::queries::files;

    fn seg(start_ms: i64, kind: SegmentKind) -> Segment {
        Segment { kind, start_ms, end_ms: start_ms + 60_000 }
    }

    #[test]
    fn empty_deliberate_writes_spare_detector_segments() {
        let dir = std::env::temp_dir().join(format!("media-db-segtest-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let conn = crate::open::open_rw(&dir.join("t.db")).unwrap();
        let roots = files::sync_roots(&conn, &[("/t".to_string(), MediaKind::Tv)]).unwrap();
        let id = files::upsert_pending(&conn, roots[0].id, "e1.mkv", 1, 1, MediaKind::Tv, "video/x-matroska").unwrap();

        // Detector found an intro; a later re-extraction sees no chapters.
        replace_for_file(&conn, id, "audio", &[seg(0, SegmentKind::Intro)]).unwrap();
        replace_for_file(&conn, id, "chapters", &[]).unwrap();
        assert_eq!(for_file(&conn, id).unwrap().len(), 1, "audio segment survives");

        // A sidecar with content takes the file over completely.
        replace_for_file(&conn, id, "edl", &[seg(300_000, SegmentKind::Commercial)]).unwrap();
        let segs = for_file(&conn, id).unwrap();
        assert_eq!(segs.len(), 1);
        assert_eq!(segs[0].kind, SegmentKind::Commercial);

        // The detector's own empty write still clears stale audio rows.
        replace_for_file(&conn, id, "audio", &[seg(0, SegmentKind::Intro)]).unwrap();
        replace_for_file(&conn, id, "audio", &[]).unwrap();
        assert!(for_file(&conn, id).unwrap().is_empty());
    }
}
