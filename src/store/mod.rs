//! The tool operations, each a self-contained call against one SQLite connection.
//!
//! Every operation returns `Ok(text)` for the model or `Err(message)`, where the
//! message says what to do next. Writes run in one transaction and log an event.

mod add;
mod claims;
mod focus;
pub(crate) mod fts;
mod get;
mod moves;
mod next;
mod recent;
mod search;
mod status;
mod update;
pub mod view;

pub use add::add;
pub use claims::{answer, ask, claim, release};
pub use focus::focus;
pub use get::get;
pub use moves::move_node;
pub use next::next;
pub use recent::recent;
pub use search::search;
pub use status::set_status;
pub use update::update;

use rusqlite::{Connection, OptionalExtension, Row, params};

use crate::model::{DEFAULT_USER, Node, NodeId, ROOT_ID};

/// What a tool call yields: text for the model, or an actionable error message.
pub type ToolResult = Result<String, String>;

/// Columns for [`Node`], selected from `nodes n`.
pub(crate) const NODE_COLS: &str = "n.id, n.parent_id, n.position, n.kind, n.ordered, n.title, n.status,
    n.priority, n.due_at, n.defer_until, n.waiting_on, n.claimed_by, n.claim_expires,
    n.created_at, n.updated_at, n.focus_rank, n.touched_at,
    EXISTS(SELECT 1 FROM notes t WHERE t.node_id = n.id) AS has_notes,
    (SELECT COUNT(*) FROM nodes c WHERE c.parent_id = n.id AND c.status != 'dropped') AS child_total,
    (SELECT COUNT(*) FROM nodes c WHERE c.parent_id = n.id AND c.status = 'done') AS child_done,
    COALESCE((SELECT p.ordered AND p.kind = 'task' FROM nodes p WHERE p.id = n.parent_id), 0) AS is_step,
    n.answered_at, n.recur, n.work_type, n.stage";

pub(crate) fn node_from_row(r: &Row<'_>) -> rusqlite::Result<Node> {
    let text_enum = |idx: usize, value: String| {
        rusqlite::Error::FromSqlConversionFailure(
            idx,
            rusqlite::types::Type::Text,
            format!("bad value '{value}'").into(),
        )
    };
    let kind: String = r.get(3)?;
    let status: String = r.get(6)?;
    let work_type = match r.get::<_, Option<String>>(23)? {
        Some(v) => Some(v.parse().map_err(|_| text_enum(23, v))?),
        None => None,
    };
    let stage = match r.get::<_, Option<String>>(24)? {
        Some(v) => Some(v.parse().map_err(|_| text_enum(24, v))?),
        None => None,
    };
    Ok(Node {
        id: NodeId(r.get(0)?),
        parent_id: r.get::<_, Option<i64>>(1)?.map(NodeId),
        position: r.get(2)?,
        kind: kind.parse().map_err(|_| text_enum(3, kind))?,
        ordered: r.get(4)?,
        title: r.get(5)?,
        status: status.parse().map_err(|_| text_enum(6, status))?,
        priority: r.get(7)?,
        due_at: r.get(8)?,
        defer_until: r.get(9)?,
        waiting_on: r.get(10)?,
        claimed_by: r.get(11)?,
        claim_expires: r.get(12)?,
        created_at: r.get(13)?,
        updated_at: r.get(14)?,
        focus_rank: r.get(15)?,
        touched_at: r.get(16)?,
        has_notes: r.get(17)?,
        child_total: r.get(18)?,
        child_done: r.get(19)?,
        is_step: r.get(20)?,
        answered_at: r.get(21)?,
        recur: r.get(22)?,
        work_type,
        stage,
    })
}

/// Maps storage failures to a message; details go to the log, not the model.
pub(crate) fn db_err(e: rusqlite::Error) -> String {
    tracing::error!(error = %e, "database error");
    "Internal database error. Try again; if it persists, check the server log.".to_string()
}

pub(crate) fn load(conn: &Connection, id: NodeId) -> Result<Option<Node>, String> {
    conn.query_row(
        &format!("SELECT {NODE_COLS} FROM nodes n WHERE n.id = ?1 AND n.user_id = ?2"),
        params![id.0, DEFAULT_USER],
        node_from_row,
    )
    .optional()
    .map_err(db_err)
}

/// Loads a node or explains how to find a real one.
pub(crate) fn require(conn: &Connection, id: NodeId) -> Result<Node, String> {
    load(conn, id)?.ok_or_else(|| {
        format!("{id} does not exist. Use get on {ROOT_ID} to browse the tree, or next to see open work.")
    })
}

pub(crate) fn children(conn: &Connection, id: NodeId) -> Result<Vec<Node>, String> {
    let mut stmt = conn
        .prepare_cached(&format!(
            "SELECT {NODE_COLS} FROM nodes n WHERE n.parent_id = ?1 AND n.user_id = ?2 ORDER BY n.position"
        ))
        .map_err(db_err)?;
    stmt.query_map(params![id.0, DEFAULT_USER], node_from_row)
        .and_then(|rows| rows.collect())
        .map_err(db_err)
}

/// Ancestors of `id`, root first, not including `id` itself.
pub(crate) fn ancestors(conn: &Connection, id: NodeId) -> Result<Vec<Node>, String> {
    let mut stmt = conn
        .prepare_cached(&format!(
            "WITH RECURSIVE up(id, depth) AS (
               SELECT parent_id, 1 FROM nodes WHERE id = ?1 AND parent_id IS NOT NULL
               UNION ALL
               SELECT n.parent_id, up.depth + 1 FROM nodes n JOIN up ON n.id = up.id
               WHERE n.parent_id IS NOT NULL
             )
             SELECT {NODE_COLS} FROM up JOIN nodes n ON n.id = up.id ORDER BY up.depth DESC"
        ))
        .map_err(db_err)?;
    stmt.query_map(params![id.0], node_from_row)
        .and_then(|rows| rows.collect())
        .map_err(db_err)
}

/// Breadcrumb of ancestor titles below the root, e.g. `Hobbies / Programming`.
pub(crate) fn breadcrumb(ancestors: &[Node]) -> String {
    ancestors
        .iter()
        .filter(|a| a.parent_id.is_some())
        .map(|a| a.title.as_str())
        .collect::<Vec<_>>()
        .join(" / ")
}

/// Sets `touched_at` on the node and every ancestor (activity bubbles up).
pub(crate) fn touch(conn: &Connection, id: NodeId, now: &str) -> rusqlite::Result<()> {
    conn.execute(
        "WITH RECURSIVE up(id) AS (
           SELECT ?1
           UNION ALL
           SELECT n.parent_id FROM nodes n JOIN up ON n.id = up.id WHERE n.parent_id IS NOT NULL
         )
         UPDATE nodes SET touched_at = ?2 WHERE id IN (SELECT id FROM up)",
        params![id.0, now],
    )?;
    Ok(())
}

/// Appends to the history log. Every write calls this.
pub(crate) fn log_event(
    conn: &Connection,
    id: NodeId,
    now: &str,
    agent: Option<&str>,
    kind: &str,
    detail: &str,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO events (node_id, at, agent, type, detail) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![id.0, now, agent, kind, detail],
    )?;
    Ok(())
}

/// The lease a claim gets by default, and what any write by the claiming agent renews it to.
pub(crate) const DEFAULT_LEASE_MINUTES: i64 = 60;

/// Any write by the claiming agent on a node or beneath it renews that agent's claims on
/// the node and its ancestors: the lease runs at least [`DEFAULT_LEASE_MINUTES`] from now
/// (a longer lease is kept). A lapsed claim the agent still holds is revived the same way.
pub(crate) fn renew_leases(
    conn: &Connection,
    id: NodeId,
    agent: Option<&str>,
    now: &str,
) -> rusqlite::Result<()> {
    let Some(agent) = agent else { return Ok(()) };
    let renewed = lease_end(now, DEFAULT_LEASE_MINUTES);
    conn.execute(
        "WITH RECURSIVE up(id) AS (
           SELECT ?1
           UNION ALL
           SELECT n.parent_id FROM nodes n JOIN up ON n.id = up.id WHERE n.parent_id IS NOT NULL
         )
         UPDATE nodes SET claim_expires = MAX(COALESCE(claim_expires, ''), ?3)
         WHERE id IN (SELECT id FROM up) AND status = 'in_progress' AND claimed_by = ?2",
        params![id.0, agent, renewed],
    )?;
    Ok(())
}

/// `now` plus `minutes`, in the RFC 3339 UTC form every timestamp column uses.
pub(crate) fn lease_end(now: &str, minutes: i64) -> String {
    let base = chrono::DateTime::parse_from_rfc3339(now)
        .map(|t| t.with_timezone(&chrono::Utc))
        .unwrap_or_else(|_| chrono::Utc::now());
    (base + chrono::Duration::minutes(minutes)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// Trims an optional agent label and bounds its length.
pub(crate) fn clean_agent(agent: Option<&str>) -> Result<Option<String>, String> {
    match agent.map(str::trim).filter(|a| !a.is_empty()) {
        None => Ok(None),
        Some(a) if a.chars().count() > 64 => {
            Err("agent is too long; use a short label like \"opus-red\".".to_string())
        }
        Some(a) => Ok(Some(a.to_string())),
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use rusqlite::Connection;

    /// A migrated, seeded database in a fresh temp file.
    pub fn temp_db(name: &str) -> (Connection, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "spokes-store-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let conn = crate::db::open(&dir.join("t.db")).expect("open temp db");
        (conn, dir)
    }
}
