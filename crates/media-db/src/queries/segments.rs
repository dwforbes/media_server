use anyhow::Result;
use rusqlite::{params, Connection};

use crate::models::{Segment, SegmentKind};

/// Replace every stored segment for a file with `segments`, all from one
/// `source` ('chapters' or 'edl'; later detectors add their own). Called
/// on every extraction, so a vanished sidecar or renamed chapter clears
/// its segments.
pub fn replace_for_file(
    conn: &Connection,
    file_id: i64,
    source: &str,
    segments: &[Segment],
) -> Result<()> {
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
