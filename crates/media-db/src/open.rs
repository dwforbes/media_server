use std::path::Path;

use anyhow::{bail, Context, Result};
use rusqlite::{Connection, OpenFlags};

use crate::schema::{MIGRATIONS, SCHEMA_VERSION};

/// Default database location: on the internal disk (not the media volume),
/// since SQLite WAL locking is unreliable on external/network filesystems.
pub fn default_db_path() -> std::path::PathBuf {
    #[allow(deprecated)]
    let home = std::env::home_dir().unwrap_or_else(|| std::path::PathBuf::from("."));
    home.join("Library/Application Support/mediaserver/media.db")
}

fn apply_pragmas(conn: &Connection) -> Result<()> {
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.busy_timeout(std::time::Duration::from_millis(5000))?;
    Ok(())
}

fn user_version(conn: &Connection) -> Result<i32> {
    Ok(conn.query_row("PRAGMA user_version", [], |r| r.get(0))?)
}

/// Open read-write (scanner). Creates the file and parent directory if
/// missing and applies any pending migrations.
pub fn open_rw(path: &Path) -> Result<Connection> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("creating {}", dir.display()))?;
    }
    let conn = Connection::open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    // journal_mode is a property of the database file; set it once here on
    // the writer. Returns the resulting mode as a row, hence query_row.
    let _mode: String = conn.query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0))?;
    apply_pragmas(&conn)?;

    let mut version = user_version(&conn)?;
    if version > SCHEMA_VERSION {
        bail!(
            "database schema version {version} is newer than this build supports ({SCHEMA_VERSION})"
        );
    }
    while version < SCHEMA_VERSION {
        let sql = MIGRATIONS[version as usize];
        tracing::info!("migrating database schema {} -> {}", version, version + 1);
        conn.execute_batch(&format!(
            "BEGIN; {sql}; PRAGMA user_version = {}; COMMIT;",
            version + 1
        ))?;
        version += 1;
    }
    Ok(conn)
}

/// Open read-only (server). Fails if the database is missing or its schema
/// version does not match; run the scanner once to create/migrate it.
pub fn open_ro(path: &Path) -> Result<Connection> {
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("opening {} read-only (run media-scanner once to create it)", path.display()))?;
    apply_pragmas(&conn)?;
    let version = user_version(&conn)?;
    if version != SCHEMA_VERSION {
        bail!(
            "database schema version {version} != expected {SCHEMA_VERSION}; run media-scanner once to migrate"
        );
    }
    Ok(conn)
}
