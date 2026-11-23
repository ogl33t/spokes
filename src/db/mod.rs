//! SQLite storage: opening the file, pragmas, schema migrations and the root seed.

mod migrations;
pub mod position;

use std::path::Path;

use anyhow::Context;
use rusqlite::Connection;

use crate::model::{DEFAULT_USER, ROOT_ID};

/// Opens (creating if needed) the database file, applies pragmas and migrations,
/// and seeds the root node on first start.
pub fn open(path: &Path) -> anyhow::Result<Connection> {
    if let Some(dir) = path.parent()
        && !dir.as_os_str().is_empty()
    {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("creating database directory {}", dir.display()))?;
    }
    let mut conn =
        Connection::open(path).with_context(|| format!("opening database {}", path.display()))?;
    configure(&conn)?;
    migrations::apply(&mut conn)?;
    seed_root(&conn)?;
    Ok(conn)
}

/// The newest schema version this build knows (what `PRAGMA user_version` is after migrating).
pub fn schema_version() -> i64 {
    migrations::latest()
}

fn configure(conn: &Connection) -> anyhow::Result<()> {
    // journal_mode returns a row, so it goes through query_row.
    let mode: String = conn.query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0))?;
    if !mode.eq_ignore_ascii_case("wal") {
        tracing::warn!(mode, "SQLite did not switch to WAL mode");
    }
    conn.execute_batch(
        "PRAGMA foreign_keys = ON;
         PRAGMA synchronous = NORMAL;
         PRAGMA busy_timeout = 5000;",
    )?;
    Ok(())
}

/// The single root ("To do") is created once; its id is always 1 on a fresh file.
fn seed_root(conn: &Connection) -> anyhow::Result<()> {
    let exists: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM nodes WHERE parent_id IS NULL)",
        [],
        |r| r.get(0),
    )?;
    if exists {
        return Ok(());
    }
    let now = now();
    let tx = conn.unchecked_transaction()?;
    tx.execute(
        "INSERT INTO nodes (id, user_id, parent_id, position, kind, title, created_at, updated_at, touched_at)
         VALUES (?1, ?2, NULL, ?3, 'area', 'To do', ?4, ?4, ?4)",
        rusqlite::params![ROOT_ID.0, DEFAULT_USER, position::first(), now],
    )?;
    tx.execute(
        "INSERT INTO events (node_id, at, agent, type, detail) VALUES (?1, ?2, 'spokes', 'created', 'To do')",
        rusqlite::params![ROOT_ID.0, now],
    )?;
    crate::store::fts::reindex(&tx, ROOT_ID)?;
    tx.commit()?;
    Ok(())
}

/// Current time as an RFC 3339 UTC string, the format every timestamp column uses.
pub fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_is_idempotent_and_seeds_root_once() {
        let dir = std::env::temp_dir().join(format!("spokes-db-test-{}", std::process::id()));
        let path = dir.join("t.db");
        let _ = std::fs::remove_dir_all(&dir);
        {
            let conn = open(&path).unwrap();
            let mode: String = conn
                .query_row("PRAGMA journal_mode", [], |r| r.get(0))
                .unwrap();
            assert_eq!(mode, "wal");
        }
        let conn = open(&path).unwrap();
        let (count, title): (i64, String) = conn
            .query_row(
                "SELECT COUNT(*), MAX(title) FROM nodes WHERE parent_id IS NULL",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!((count, title.as_str()), (1, "To do"));
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, migrations::latest());
        assert_eq!(migrations::latest(), 4);
        let new_cols: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('nodes') WHERE name IN ('work_type', 'stage')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(new_cols, 2);
        drop(conn);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
