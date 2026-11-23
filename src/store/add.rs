//! `add`: create one node, or a whole nested plan, under a parent in one transaction.

use rusqlite::{Connection, OptionalExtension, Transaction, params};

use super::{ToolResult, clean_agent, db_err, log_event, renew_leases, require, touch};
use crate::db::{now, position};
use crate::format::parse_date;
use crate::model::{DEFAULT_USER, Kind, NodeId, NumArg, ROOT_ID, Stage, WorkType};
use crate::recur::{RecurArg, RecurChange};
use crate::tools::args::{AddArgs, NodeSpec};

const MAX_TITLE: usize = 300;
const MAX_ALIASES: usize = 10;
const MAX_ALIAS: usize = 100;
pub(crate) const MAX_NOTES: usize = 100_000;
/// Nodes one `add` call may create, and how deep `children` may nest.
const MAX_NODES: usize = 200;
const MAX_NEST: usize = 8;

/// A validated node ready to insert.
struct Clean {
    title: String,
    kind: Kind,
    work_type: Option<WorkType>,
    stage: Option<Stage>,
    ordered: bool,
    priority: Option<i64>,
    due: Option<String>,
    defer: Option<String>,
    aliases: Option<String>,
    /// The repeat rule as stored JSON.
    recur: Option<String>,
    notes: Option<String>,
    children: Vec<Clean>,
}

pub fn add(conn: &mut Connection, args: AddArgs) -> ToolResult {
    let parent_id = match &args.parent {
        Some(p) => p.parse()?,
        None => ROOT_ID,
    };
    let agent = clean_agent(args.agent.as_deref())?;
    let mut count = 0;
    let spec = clean_spec(&args.node, "", 0, &mut count)?;

    let tx = conn.transaction().map_err(db_err)?;
    let parent = require(&tx, parent_id)?;
    if parent.status.is_closed() {
        return Err(format!(
            "{} {} is {}. Reopen it with set_status, or pick another parent.",
            parent.id,
            parent.title,
            parent.status.as_str()
        ));
    }
    if spec.kind == Kind::Area && parent.kind == Kind::Task {
        return Err(format!(
            "{} {} is a task; areas can only go under areas. Add a task instead, or pick an area as parent.",
            parent.id, parent.title
        ));
    }
    if spec.recur.is_some() && parent.ordered {
        return Err(recur_step_error(&parent.id.to_string(), &parent.title));
    }

    let now = now();
    let mut ids = Vec::new();
    insert(&tx, parent_id, &spec, &now, agent.as_deref(), &mut ids)?;
    let top = ids[0];
    touch(&tx, ids[ids.len() - 1], &now).map_err(db_err)?;
    renew_leases(&tx, parent_id, agent.as_deref(), &now).map_err(db_err)?;
    tx.commit().map_err(db_err)?;

    let mut line = format!(
        "Added {top} {}  {}  (under {} {})",
        spec.kind.as_str(),
        spec.title,
        parent.id,
        parent.title
    );
    if ids.len() > 1 {
        line.push_str(&format!(
            ", with {} beneath it in the order given: {}",
            ids.len() - 1,
            id_ranges(&ids[1..])
        ));
    }
    Ok(line)
}

/// Inserts `spec` as the last child of `parent`, then its children depth-first.
fn insert(
    tx: &Transaction<'_>,
    parent: NodeId,
    spec: &Clean,
    now: &str,
    agent: Option<&str>,
    ids: &mut Vec<NodeId>,
) -> Result<(), String> {
    let last: Option<String> = tx
        .query_row(
            "SELECT MAX(position) FROM nodes WHERE parent_id = ?1",
            params![parent.0],
            |r| r.get(0),
        )
        .optional()
        .map_err(db_err)?
        .flatten();
    let pos = match last {
        Some(l) => position::after(&l),
        None => position::first(),
    };
    tx.execute(
        "INSERT INTO nodes (user_id, parent_id, position, kind, ordered, title, priority,
                            due_at, defer_until, aliases, recur, work_type, stage,
                            created_at, updated_at, touched_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?12, ?13, ?14, ?11, ?11, ?11)",
        params![
            DEFAULT_USER,
            parent.0,
            pos,
            spec.kind.as_str(),
            spec.ordered,
            spec.title,
            spec.priority,
            spec.due,
            spec.defer,
            spec.aliases,
            now,
            spec.recur,
            spec.work_type.map(WorkType::as_str),
            spec.stage.map(Stage::as_str)
        ],
    )
    .map_err(db_err)?;
    let id = NodeId(tx.last_insert_rowid());
    ids.push(id);
    if let Some(body) = &spec.notes {
        tx.execute(
            "INSERT INTO notes (node_id, body) VALUES (?1, ?2)",
            params![id.0, body],
        )
        .map_err(db_err)?;
    }
    super::fts::reindex(tx, id).map_err(db_err)?;
    log_event(tx, id, now, agent, "created", &spec.title).map_err(db_err)?;
    for child in &spec.children {
        insert(tx, id, child, now, agent, ids)?;
    }
    Ok(())
}

/// Validates a spec and its children; errors name the child that failed.
fn clean_spec(
    spec: &NodeSpec,
    path: &str,
    nest: usize,
    count: &mut usize,
) -> Result<Clean, String> {
    *count += 1;
    if *count > MAX_NODES {
        return Err(format!(
            "one add can create at most {MAX_NODES} nodes. Split the plan into several calls."
        ));
    }
    let at = |e: String| {
        if path.is_empty() {
            e
        } else {
            format!("{path} (\"{}\"): {e}", spec.title.trim())
        }
    };
    let title = clean_title(&spec.title).map_err(at)?;
    let kind = spec.kind.unwrap_or(Kind::Task);
    let priority = clean_priority(spec.priority.as_ref(), false).map_err(at)?;
    let due = spec
        .due
        .as_deref()
        .map(|d| clean_date("due", d))
        .transpose()
        .map_err(at)?;
    let defer = spec
        .defer_until
        .as_deref()
        .map(|d| clean_date("defer_until", d))
        .transpose()
        .map_err(at)?;
    let aliases = clean_aliases(spec.aliases.as_deref().unwrap_or_default()).map_err(at)?;
    let notes = clean_notes(spec.notes.as_deref()).map_err(at)?;
    let recur = clean_recur(spec.recur.as_ref(), kind).map_err(at)?;
    let work_type =
        clean_added("type", spec.work_type.as_deref(), WorkType::parse_loose).map_err(at)?;
    let stage = clean_added("stage", spec.stage.as_deref(), Stage::parse_loose).map_err(at)?;
    let ordered = spec.ordered.unwrap_or(false);

    let kids = spec.children.as_deref().unwrap_or_default();
    if !kids.is_empty() && nest >= MAX_NEST {
        return Err(at(format!(
            "children nest deeper than {MAX_NEST} levels. Add the deeper part in a second call."
        )));
    }
    let mut children = Vec::with_capacity(kids.len());
    for (i, kid) in kids.iter().enumerate() {
        let kid_path = if path.is_empty() {
            format!("children[{i}]")
        } else {
            format!("{path}.children[{i}]")
        };
        let clean = clean_spec(kid, &kid_path, nest + 1, count)?;
        if clean.kind == Kind::Area && kind == Kind::Task {
            return Err(format!(
                "{kid_path} (\"{}\") is an area under the task \"{title}\"; areas can only go under areas. Make it a task.",
                clean.title
            ));
        }
        if clean.recur.is_some() && ordered {
            return Err(format!(
                "{kid_path} (\"{}\"): {}",
                clean.title,
                recur_step_error("its parent", &title)
            ));
        }
        children.push(clean);
    }
    Ok(Clean {
        title,
        kind,
        work_type,
        stage,
        ordered,
        priority,
        due,
        defer,
        aliases,
        recur,
        notes,
        children,
    })
}

/// A type or stage given on add: parsed loosely; an empty string is an error (omit the field).
fn clean_added<T>(
    field: &str,
    raw: Option<&str>,
    parse: fn(&str) -> Result<T, String>,
) -> Result<Option<T>, String> {
    match raw {
        None => Ok(None),
        Some(r) if r.trim().is_empty() => {
            Err(format!("{field} is empty. Omit {field} to leave it unset."))
        }
        Some(r) => parse(r).map(Some),
    }
}

/// A type or stage given on update: parsed loosely; an empty string clears it.
pub(crate) fn clean_updated<T>(
    raw: Option<&str>,
    parse: fn(&str) -> Result<T, String>,
) -> Result<Option<Option<T>>, String> {
    match raw {
        None => Ok(None),
        Some(r) if r.trim().is_empty() => Ok(Some(None)),
        Some(r) => parse(r).map(|v| Some(Some(v))),
    }
}

/// `SPOKE-3..SPOKE-8, SPOKE-10`: consecutive ids collapse into ranges.
fn id_ranges(ids: &[NodeId]) -> String {
    let mut parts = Vec::new();
    let mut i = 0;
    while i < ids.len() {
        let mut j = i;
        while j + 1 < ids.len() && ids[j + 1].0 == ids[j].0 + 1 {
            j += 1;
        }
        parts.push(match j - i {
            0 => ids[i].to_string(),
            1 => format!("{}, {}", ids[i], ids[j]),
            _ => format!("{}..{}", ids[i], ids[j]),
        });
        i = j + 1;
    }
    parts.join(", ")
}

/// 1 to 4; with `allow_clear`, 0 means "unset, inherit again".
pub(crate) fn clean_priority(
    raw: Option<&NumArg>,
    allow_clear: bool,
) -> Result<Option<i64>, String> {
    match raw.map(|p| p.parse("priority")).transpose()? {
        None => Ok(None),
        Some(0) if allow_clear => Ok(Some(0)),
        Some(p) if (1..=4).contains(&p) => Ok(Some(p)),
        Some(p) if allow_clear => Err(format!(
            "priority must be 1 (highest) to 4, or 0 to clear it, not {p}."
        )),
        Some(p) => Err(format!("priority must be 1 (highest) to 4, not {p}.")),
    }
}

/// Trims notes; empty notes are no notes.
pub(crate) fn clean_notes(raw: Option<&str>) -> Result<Option<String>, String> {
    let notes = raw.map(str::trim).filter(|n| !n.is_empty());
    if notes.is_some_and(|n| n.len() > MAX_NOTES) {
        return Err(format!(
            "notes are longer than {MAX_NOTES} bytes; trim them."
        ));
    }
    Ok(notes.map(str::to_string))
}

pub(crate) fn clean_title(raw: &str) -> Result<String, String> {
    let title = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    if title.is_empty() {
        return Err("title is empty. Give the node a short title.".to_string());
    }
    if title.chars().count() > MAX_TITLE {
        return Err(format!(
            "title is longer than {MAX_TITLE} characters. Keep it short and put detail in notes."
        ));
    }
    Ok(title)
}

/// Accepts YYYY-MM-DD (or a timestamp starting with one) and stores the date.
pub(crate) fn clean_date(field: &str, raw: &str) -> Result<String, String> {
    let raw = raw.trim();
    match parse_date(raw) {
        Some(d) if raw.len() == 10 || raw.as_bytes().get(10) == Some(&b'T') => {
            Ok(d.format("%Y-%m-%d").to_string())
        }
        _ => Err(format!(
            "{field} '{raw}' is not a date. Use YYYY-MM-DD, e.g. 2026-11-20."
        )),
    }
}

/// A repeat rule as stored JSON; areas never finish, so they cannot repeat.
/// `Some(None)` means "clear it" (only meaningful for update).
pub(crate) fn clean_recur_change(raw: Option<&RecurArg>) -> Result<Option<Option<String>>, String> {
    match raw.map(RecurArg::parse).transpose()? {
        None => Ok(None),
        Some(RecurChange::Clear) => Ok(Some(None)),
        Some(RecurChange::Set(rule)) => Ok(Some(Some(rule.to_json()))),
    }
}

fn clean_recur(raw: Option<&RecurArg>, kind: Kind) -> Result<Option<String>, String> {
    let recur = clean_recur_change(raw)?.flatten();
    if recur.is_some() && kind == Kind::Area {
        return Err(AREA_RECUR.to_string());
    }
    Ok(recur)
}

pub(crate) const AREA_RECUR: &str =
    "areas never finish, so they cannot repeat. Put recur on a task under it.";

/// Steps of an ordered parent do not repeat on their own: a step that reset itself
/// would hold every later step back forever. The parent repeats instead, and its
/// finished steps reopen with it.
pub(crate) fn recur_step_error(parent: &str, title: &str) -> String {
    format!(
        "{parent} {title} has ordered steps, and a step cannot repeat on its own (it would hold the later steps back). Put recur on {title} instead: when it repeats, its steps reopen."
    )
}

/// Aliases are stored newline-separated, as the schema describes.
pub(crate) fn clean_aliases(raw: &[String]) -> Result<Option<String>, String> {
    let list: Vec<String> = raw
        .iter()
        .map(|a| a.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|a| !a.is_empty())
        .collect();
    if list.len() > MAX_ALIASES {
        return Err(format!(
            "give at most {MAX_ALIASES} aliases (2 to 5 is ideal)."
        ));
    }
    if list.iter().any(|a| a.chars().count() > MAX_ALIAS) {
        return Err(format!("each alias must be under {MAX_ALIAS} characters."));
    }
    Ok((!list.is_empty()).then(|| list.join("\n")))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::model::IdArg;
    use crate::store::test_support::temp_db;

    fn step(title: &str) -> NodeSpec {
        NodeSpec {
            title: title.into(),
            ..NodeSpec::default()
        }
    }

    pub(crate) fn args(parent: Option<i64>, title: &str, kind: Kind) -> AddArgs {
        AddArgs {
            parent: parent.map(IdArg::Number),
            node: NodeSpec {
                title: title.into(),
                kind: Some(kind),
                ..NodeSpec::default()
            },
            agent: Some("test".into()),
        }
    }

    #[test]
    fn adds_under_root_and_logs_event() {
        let (mut conn, dir) = temp_db("add");
        let out = add(&mut conn, args(None, "  Home  ", Kind::Area)).unwrap();
        assert_eq!(out, "Added SPOKE-2 area  Home  (under SPOKE-1 To do)");
        let (count, agent): (i64, String) = conn
            .query_row(
                "SELECT COUNT(*), MAX(agent) FROM events WHERE node_id = 2 AND type = 'created'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!((count, agent.as_str()), (1, "test"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn rejects_bad_input_with_guidance() {
        let (mut conn, dir) = temp_db("add-bad");
        assert!(
            add(&mut conn, args(Some(99), "x", Kind::Task))
                .unwrap_err()
                .contains("SPOKE-99 does not exist")
        );
        assert!(
            add(&mut conn, args(None, "   ", Kind::Task))
                .unwrap_err()
                .contains("empty")
        );
        let mut a = args(None, "x", Kind::Task);
        a.node.due = Some("next friday".into());
        assert!(add(&mut conn, a).unwrap_err().contains("YYYY-MM-DD"));
        let mut a = args(None, "x", Kind::Task);
        a.node.priority = Some(NumArg::Number(7));
        assert!(add(&mut conn, a).is_err());
        add(&mut conn, args(None, "Task", Kind::Task)).unwrap();
        assert!(
            add(&mut conn, args(Some(2), "Area", Kind::Area))
                .unwrap_err()
                .contains("areas can only go under areas")
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn stores_type_and_stage_loosely_parsed() {
        let (mut conn, dir) = temp_db("add-type");
        let mut a = args(None, "Fix", Kind::Task);
        a.node.work_type = Some("Bug".into());
        a.node.stage = Some("In review".into());
        add(&mut conn, a).unwrap();
        let row: (Option<String>, Option<String>) = conn
            .query_row("SELECT work_type, stage FROM nodes WHERE id = 2", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!(row, (Some("bug".into()), Some("in_review".into())));
        let mut a = args(None, "Empty", Kind::Task);
        a.node.stage = Some(" ".into());
        assert!(add(&mut conn, a).unwrap_err().contains("Omit stage"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn bad_nested_type_names_child_and_writes_nothing() {
        let (mut conn, dir) = temp_db("add-type-bad");
        let mut a = args(None, "Plan", Kind::Task);
        let mut bad = step("Push");
        bad.work_type = Some("bugg".into());
        a.node.children = Some(vec![step("ok"), bad]);
        let err = add(&mut conn, a).unwrap_err();
        assert!(
            err.starts_with("children[1] (\"Push\"): unknown type"),
            "{err}"
        );
        assert!(err.contains("bug, feature"), "{err}");
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM nodes", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn siblings_keep_insertion_order() {
        let (mut conn, dir) = temp_db("add-order");
        for t in ["a", "b", "c"] {
            add(&mut conn, args(None, t, Kind::Task)).unwrap();
        }
        let titles: Vec<String> = crate::store::children(&conn, ROOT_ID)
            .unwrap()
            .into_iter()
            .map(|n| n.title)
            .collect();
        assert_eq!(titles, ["a", "b", "c"]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn nested_plan_is_one_call_and_one_line() {
        let (mut conn, dir) = temp_db("add-nested");
        let mut a = args(None, "Deploy", Kind::Task);
        a.node.ordered = Some(true);
        let mut s2 = step("Write");
        s2.children = Some(vec![step("Draft"), step("Review")]);
        a.node.children = Some(vec![step("Map"), s2, step("Ship")]);
        let out = add(&mut conn, a).unwrap();
        assert_eq!(
            out,
            "Added SPOKE-2 task  Deploy  (under SPOKE-1 To do), with 5 beneath it in the order given: SPOKE-3..SPOKE-7"
        );
        let titles: Vec<String> = crate::store::children(&conn, NodeId(2))
            .unwrap()
            .into_iter()
            .map(|n| n.title)
            .collect();
        assert_eq!(titles, ["Map", "Write", "Ship"]);
        let draft_parent: i64 = conn
            .query_row(
                "SELECT parent_id FROM nodes WHERE title = 'Draft'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(draft_parent, 4);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn nested_errors_name_the_child_and_write_nothing() {
        let (mut conn, dir) = temp_db("add-nested-bad");
        let mut a = args(None, "Plan", Kind::Task);
        let mut bad = step("Area inside");
        bad.kind = Some(Kind::Area);
        a.node.children = Some(vec![step("ok"), bad]);
        let err = add(&mut conn, a).unwrap_err();
        assert!(err.starts_with("children[1] (\"Area inside\")"), "{err}");
        let mut a = args(None, "Plan", Kind::Task);
        a.node.children = Some(vec![step("ok"), step("  ")]);
        let err = add(&mut conn, a).unwrap_err();
        assert!(
            err.contains("children[1]") && err.contains("empty"),
            "{err}"
        );
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM nodes", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);
        assert_eq!(
            id_ranges(&[NodeId(3), NodeId(4), NodeId(5), NodeId(9), NodeId(10)]),
            "SPOKE-3..SPOKE-5, SPOKE-9, SPOKE-10"
        );
        let _ = std::fs::remove_dir_all(dir);
    }
}
