//! `update`: change a node's fields, replace its notes or append to them.

use rusqlite::types::Value;
use rusqlite::{Connection, OptionalExtension, params, params_from_iter};

use super::add::{
    AREA_RECUR, MAX_NOTES, clean_aliases, clean_date, clean_notes, clean_priority,
    clean_recur_change, clean_title, clean_updated, recur_step_error,
};
use super::{ToolResult, children, clean_agent, db_err, log_event, renew_leases, require, touch};
use crate::db::now;
use crate::model::{Kind, Stage, Status, WorkType};
use crate::tools::args::UpdateArgs;

pub fn update(conn: &mut Connection, args: UpdateArgs) -> ToolResult {
    let id = args.id.parse()?;
    let agent = clean_agent(args.agent.as_deref())?;
    if args.notes.is_some() && args.notes_append.is_some() {
        return Err(
            "pass notes (replace the body) or notes_append (add to it), not both.".to_string(),
        );
    }

    // Column assignments, validated before anything is written.
    let mut sets: Vec<(&str, Value)> = Vec::new();
    let mut changed: Vec<&str> = Vec::new();
    if let Some(t) = &args.title {
        sets.push(("title", Value::Text(clean_title(t)?)));
        changed.push("title");
    }
    if let Some(k) = args.kind {
        sets.push(("kind", Value::Text(k.as_str().to_string())));
        changed.push("kind");
    }
    if let Some(o) = args.ordered {
        sets.push(("ordered", Value::Integer(o.into())));
        changed.push("ordered");
    }
    if let Some(p) = clean_priority(args.priority.as_ref(), true)? {
        sets.push((
            "priority",
            if p == 0 {
                Value::Null
            } else {
                Value::Integer(p)
            },
        ));
        changed.push("priority");
    }
    for (field, column, raw) in [
        ("due", "due_at", &args.due),
        ("defer_until", "defer_until", &args.defer_until),
    ] {
        if let Some(raw) = raw {
            let value = if raw.trim().is_empty() {
                Value::Null
            } else {
                Value::Text(clean_date(field, raw)?)
            };
            sets.push((column, value));
            changed.push(field);
        }
    }
    if let Some(a) = &args.aliases {
        sets.push((
            "aliases",
            clean_aliases(a)?.map_or(Value::Null, Value::Text),
        ));
        changed.push("aliases");
    }
    let recur = clean_recur_change(args.recur.as_ref())?;
    if let Some(r) = &recur {
        sets.push(("recur", r.clone().map_or(Value::Null, Value::Text)));
        changed.push(if r.is_some() {
            "recur"
        } else {
            "recur cleared"
        });
    }
    if let Some(t) = clean_updated(args.work_type.as_deref(), WorkType::parse_loose)? {
        sets.push((
            "work_type",
            t.map_or(Value::Null, |t| Value::Text(t.as_str().to_string())),
        ));
        changed.push(if t.is_some() { "type" } else { "type cleared" });
    }
    if let Some(s) = clean_updated(args.stage.as_deref(), Stage::parse_loose)? {
        sets.push((
            "stage",
            s.map_or(Value::Null, |s| Value::Text(s.as_str().to_string())),
        ));
        changed.push(if s.is_some() {
            "stage"
        } else {
            "stage cleared"
        });
    }
    let replace_notes = args
        .notes
        .as_deref()
        .map(|n| clean_notes(Some(n)))
        .transpose()?;
    let append = clean_notes(args.notes_append.as_deref())?;
    if args.notes_append.is_some() && append.is_none() {
        return Err("notes_append is empty. Pass the text to add.".to_string());
    }
    if sets.is_empty() && replace_notes.is_none() && append.is_none() {
        return Err(
            "nothing to update. Pass at least one field (title, kind, type, stage, ordered, priority, due, defer_until, aliases, recur, notes, notes_append). To change status use set_status; to reparent use move."
                .to_string(),
        );
    }

    let tx = conn.transaction().map_err(db_err)?;
    let node = require(&tx, id)?;
    if let Some(kind) = args.kind
        && kind != node.kind
    {
        check_kind_change(&tx, &node, kind)?;
    }
    check_recur(&tx, &node, args.kind, args.ordered, recur.as_ref())?;

    let now = now();
    if !sets.is_empty() {
        let assignments: Vec<String> = sets
            .iter()
            .enumerate()
            .map(|(i, (col, _))| format!("{col} = ?{}", i + 1))
            .collect();
        let mut values: Vec<Value> = sets.into_iter().map(|(_, v)| v).collect();
        values.push(Value::Integer(id.0));
        let n = values.len();
        // Column names come from the fixed list above, never from input.
        tx.execute(
            &format!(
                "UPDATE nodes SET {} WHERE id = ?{n}",
                assignments.join(", ")
            ),
            params_from_iter(values),
        )
        .map_err(db_err)?;
    }

    match (replace_notes, append) {
        (Some(None), _) => {
            tx.execute("DELETE FROM notes WHERE node_id = ?1", params![id.0])
                .map_err(db_err)?;
            changed.push("notes cleared");
        }
        (Some(Some(body)), _) => {
            tx.execute(
                "INSERT INTO notes (node_id, body) VALUES (?1, ?2)
                 ON CONFLICT(node_id) DO UPDATE SET body = excluded.body",
                params![id.0, body],
            )
            .map_err(db_err)?;
            changed.push("notes replaced");
        }
        (None, Some(text)) => {
            let old: Option<String> = tx
                .query_row(
                    "SELECT body FROM notes WHERE node_id = ?1",
                    params![id.0],
                    |r| r.get(0),
                )
                .optional()
                .map_err(db_err)?;
            let body = match old {
                Some(old) => format!("{old}\n\n{text}"),
                None => text,
            };
            if body.len() > MAX_NOTES {
                return Err(format!(
                    "notes would grow past {MAX_NOTES} bytes. Replace them with a shorter summary using notes."
                ));
            }
            tx.execute(
                "INSERT INTO notes (node_id, body) VALUES (?1, ?2)
                 ON CONFLICT(node_id) DO UPDATE SET body = excluded.body",
                params![id.0, body],
            )
            .map_err(db_err)?;
            changed.push("notes appended");
        }
        (None, None) => {}
    }
    tx.execute(
        "UPDATE nodes SET updated_at = ?1 WHERE id = ?2",
        params![now, id.0],
    )
    .map_err(db_err)?;
    super::fts::reindex(&tx, id).map_err(db_err)?;
    let detail = changed.join(", ");
    log_event(&tx, id, &now, agent.as_deref(), "updated", &detail).map_err(db_err)?;
    touch(&tx, id, &now).map_err(db_err)?;
    renew_leases(&tx, id, agent.as_deref(), &now).map_err(db_err)?;
    let title = require(&tx, id)?.title;
    tx.commit().map_err(db_err)?;
    Ok(format!("Updated {id}  {title}  ({detail})"))
}

/// Only tasks repeat, and never as a step of an ordered parent (see `recur_step_error`).
fn check_recur(
    conn: &Connection,
    node: &crate::model::Node,
    kind: Option<Kind>,
    ordered: Option<bool>,
    recur: Option<&Option<String>>,
) -> Result<(), String> {
    let repeats = match recur {
        Some(r) => r.is_some(),
        None => node.recur.is_some(),
    };
    if repeats && kind.unwrap_or(node.kind) == Kind::Area {
        return Err(format!("{} {}: {AREA_RECUR}", node.id, node.title));
    }
    if matches!(recur, Some(Some(_)))
        && let Some(pid) = node.parent_id
    {
        let parent = require(conn, pid)?;
        if parent.ordered {
            return Err(recur_step_error(&parent.id.to_string(), &parent.title));
        }
    }
    if ordered == Some(true)
        && !node.ordered
        && let Some(kid) = children(conn, node.id)?
            .into_iter()
            .find(|c| c.recur.is_some() && !c.status.is_closed())
    {
        return Err(format!(
            "{} {} repeats, and steps of an ordered parent cannot repeat on their own. Stop it repeating (update recur \"\") or leave {} unordered.",
            kid.id, kid.title, node.id
        ));
    }
    Ok(())
}

/// Areas live only under areas, and areas never finish.
fn check_kind_change(
    conn: &Connection,
    node: &crate::model::Node,
    kind: Kind,
) -> Result<(), String> {
    match kind {
        Kind::Area => {
            if node.status == Status::Done {
                return Err(format!(
                    "{} is done; areas never finish. Reopen it with set_status first.",
                    node.id
                ));
            }
            if let Some(pid) = node.parent_id {
                let parent = require(conn, pid)?;
                if parent.kind == Kind::Task {
                    return Err(format!(
                        "{} sits under the task {} {}; areas can only go under areas. Move it under an area first.",
                        node.id, parent.id, parent.title
                    ));
                }
            }
        }
        Kind::Task => {
            if node.parent_id.is_none() {
                return Err(format!("{} is the root and stays an area.", node.id));
            }
            if let Some(area) = children(conn, node.id)?
                .into_iter()
                .find(|c| c.kind == Kind::Area)
            {
                return Err(format!(
                    "{} holds the area {} {}; areas can only go under areas. Move that area out, or make it a task, first.",
                    node.id, area.id, area.title
                ));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{IdArg, SettableStatus};
    use crate::store::add::tests::args;
    use crate::store::test_support::temp_db;
    use crate::store::{add, get, set_status};
    use crate::tools::args::{GetArgs, SetStatusArgs};
    use serde_json::json;

    fn up(conn: &mut Connection, v: serde_json::Value) -> ToolResult {
        update(conn, serde_json::from_value(v).unwrap())
    }

    fn get_text(conn: &Connection, id: i64, notes: bool) -> String {
        let a: GetArgs = serde_json::from_value(json!({"id": id, "notes": notes})).unwrap();
        get(conn, a, chrono_tz::UTC).unwrap()
    }

    type Row = (
        String,
        bool,
        Option<i64>,
        Option<String>,
        Option<String>,
        Option<String>,
    );

    fn row(conn: &Connection, id: i64) -> Row {
        conn.query_row(
            "SELECT title, ordered, priority, due_at, defer_until, aliases FROM nodes WHERE id = ?1",
            params![id],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                ))
            },
        )
        .unwrap()
    }

    fn notes_row(conn: &Connection, id: i64) -> Option<String> {
        conn.query_row(
            "SELECT body FROM notes WHERE node_id = ?1",
            params![id],
            |r| r.get(0),
        )
        .optional()
        .unwrap()
    }

    fn close(conn: &mut Connection, id: &str, status: SettableStatus) {
        set_status(
            conn,
            SetStatusArgs {
                ids: vec![IdArg::Text(id.to_string())],
                status,
                agent: None,
            },
            crate::format::today_in(chrono_tz::UTC),
        )
        .unwrap();
    }

    #[test]
    fn updates_and_clears_plain_fields() {
        let (mut conn, dir) = temp_db("up-fields");
        add(&mut conn, args(None, "Task", Kind::Task)).unwrap(); // 2

        let out = up(
            &mut conn,
            json!({"id": "SPOKE-2", "title": "  New title  "}),
        )
        .unwrap();
        assert_eq!(out, "Updated SPOKE-2  New title  (title)");
        assert_eq!(row(&conn, 2).0, "New title");

        let out = up(&mut conn, json!({"id": 2, "ordered": true})).unwrap();
        assert_eq!(out, "Updated SPOKE-2  New title  (ordered)");
        assert!(row(&conn, 2).1);
        up(&mut conn, json!({"id": 2, "ordered": false})).unwrap();
        assert!(!row(&conn, 2).1);

        up(&mut conn, json!({"id": 2, "priority": 2})).unwrap();
        assert_eq!(row(&conn, 2).2, Some(2));
        up(&mut conn, json!({"id": 2, "priority": "3"})).unwrap();
        assert_eq!(row(&conn, 2).2, Some(3));
        up(&mut conn, json!({"id": 2, "priority": 0})).unwrap();
        assert_eq!(row(&conn, 2).2, None);

        up(&mut conn, json!({"id": 2, "due": "2026-12-31"})).unwrap();
        assert_eq!(row(&conn, 2).3.as_deref(), Some("2026-12-31"));
        up(&mut conn, json!({"id": 2, "due": ""})).unwrap();
        assert_eq!(row(&conn, 2).3, None);

        let out = up(&mut conn, json!({"id": 2, "defer_until": "2027-01-05"})).unwrap();
        assert_eq!(out, "Updated SPOKE-2  New title  (defer_until)");
        assert_eq!(row(&conn, 2).4.as_deref(), Some("2027-01-05"));
        up(&mut conn, json!({"id": 2, "defer_until": ""})).unwrap();
        assert_eq!(row(&conn, 2).4, None);

        up(
            &mut conn,
            json!({"id": 2, "aliases": ["one", "two  words"]}),
        )
        .unwrap();
        assert_eq!(row(&conn, 2).5.as_deref(), Some("one\ntwo words"));
        up(&mut conn, json!({"id": 2, "aliases": ["only"]})).unwrap();
        assert_eq!(row(&conn, 2).5.as_deref(), Some("only"));
        up(&mut conn, json!({"id": 2, "aliases": []})).unwrap();
        assert_eq!(row(&conn, 2).5, None);

        // Several fields at once, other fields untouched.
        let out = up(
            &mut conn,
            json!({"id": 2, "title": "Multi", "priority": 1, "due": "2026-11-20"}),
        )
        .unwrap();
        assert_eq!(out, "Updated SPOKE-2  Multi  (title, priority, due)");
        let r = row(&conn, 2);
        assert_eq!(
            (r.0.as_str(), r.2, r.3.as_deref()),
            ("Multi", Some(1), Some("2026-11-20"))
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn sets_changes_and_clears_type_and_stage() {
        let (mut conn, dir) = temp_db("up-type");
        add(&mut conn, args(None, "Task", Kind::Task)).unwrap(); // 2
        let ts = |conn: &Connection| -> (Option<String>, Option<String>) {
            conn.query_row("SELECT work_type, stage FROM nodes WHERE id = 2", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap()
        };
        let out = up(
            &mut conn,
            json!({"id": 2, "type": "Bug", "stage": "In review"}),
        )
        .unwrap();
        assert_eq!(out, "Updated SPOKE-2  Task  (type, stage)");
        assert_eq!(ts(&conn), (Some("bug".into()), Some("in_review".into())));
        up(
            &mut conn,
            json!({"id": 2, "type": "chore", "stage": "done"}),
        )
        .unwrap();
        assert_eq!(ts(&conn), (Some("chore".into()), Some("done".into())));
        let status: String = conn
            .query_row("SELECT status FROM nodes WHERE id = 2", [], |r| r.get(0))
            .unwrap();
        assert_ne!(status, "done");
        let out = up(&mut conn, json!({"id": 2, "type": " ", "stage": ""})).unwrap();
        assert_eq!(out, "Updated SPOKE-2  Task  (type cleared, stage cleared)");
        assert_eq!(ts(&conn), (None, None));
        let detail: String = conn
            .query_row(
                "SELECT detail FROM events WHERE node_id = 2 AND type = 'updated' ORDER BY id DESC LIMIT 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(detail, "type cleared, stage cleared");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn rejects_bad_type_and_stage() {
        let (mut conn, dir) = temp_db("up-type-bad");
        add(&mut conn, args(None, "Task", Kind::Task)).unwrap(); // 2
        let e = up(&mut conn, json!({"id": 2, "type": "bugg"})).unwrap_err();
        assert!(e.contains("bug, feature"), "{e}");
        let e = up(&mut conn, json!({"id": 2, "title": "X", "stage": "soon"})).unwrap_err();
        assert!(e.contains("in_review"), "{e}");
        assert_eq!(row(&conn, 2).0, "Task");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn notes_replace_append_and_clear() {
        let (mut conn, dir) = temp_db("up-notes");
        add(&mut conn, args(None, "Task", Kind::Task)).unwrap(); // 2
        assert_eq!(notes_row(&conn, 2), None);

        // Append with no existing notes.
        let out = up(&mut conn, json!({"id": 2, "notes_append": "  first  "})).unwrap();
        assert_eq!(out, "Updated SPOKE-2  Task  (notes appended)");
        assert_eq!(notes_row(&conn, 2).as_deref(), Some("first"));

        // Append onto existing notes.
        up(&mut conn, json!({"id": 2, "notes_append": "second"})).unwrap();
        assert_eq!(notes_row(&conn, 2).as_deref(), Some("first\n\nsecond"));

        // Plain get only flags notes; notes:true shows the body.
        let plain = get_text(&conn, 2, false);
        assert!(plain.contains("+notes"), "{plain}");
        assert!(!plain.contains("second"), "{plain}");
        let full = get_text(&conn, 2, true);
        assert!(full.contains("NOTES\nfirst\n\nsecond"), "{full}");

        // Replace.
        let out = up(&mut conn, json!({"id": 2, "notes": "replaced"})).unwrap();
        assert_eq!(out, "Updated SPOKE-2  Task  (notes replaced)");
        assert_eq!(notes_row(&conn, 2).as_deref(), Some("replaced"));

        // Replace when there were no notes at all.
        add(&mut conn, args(None, "Other", Kind::Task)).unwrap(); // 3
        up(&mut conn, json!({"id": 3, "notes": "fresh"})).unwrap();
        assert_eq!(notes_row(&conn, 3).as_deref(), Some("fresh"));

        // Empty string clears: the row is deleted.
        let out = up(&mut conn, json!({"id": 2, "notes": ""})).unwrap();
        assert_eq!(out, "Updated SPOKE-2  Task  (notes cleared)");
        assert_eq!(notes_row(&conn, 2), None);
        assert!(!get_text(&conn, 2, false).contains("+notes"));
        assert!(get_text(&conn, 2, true).contains("(no notes)"));
        // Other node's notes untouched.
        assert_eq!(notes_row(&conn, 3).as_deref(), Some("fresh"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn kind_changes_follow_the_area_rules() {
        let (mut conn, dir) = temp_db("up-kind");
        add(&mut conn, args(None, "Home", Kind::Area)).unwrap(); // 2
        add(&mut conn, args(Some(2), "Chores", Kind::Task)).unwrap(); // 3 task under area
        add(&mut conn, args(Some(3), "Sweep", Kind::Task)).unwrap(); // 4 task under task
        add(&mut conn, args(Some(2), "Garage", Kind::Area)).unwrap(); // 5 area under area
        add(&mut conn, args(Some(2), "Old", Kind::Task)).unwrap(); // 6

        // task -> area under an area: ok.
        let out = up(&mut conn, json!({"id": 3, "kind": "area"}));
        // 3 holds a task child, which is not an area problem for kind=area.
        assert_eq!(out.unwrap(), "Updated SPOKE-3  Chores  (kind)");
        // Now 4 sits under an area; flip it back and verify the refusals.
        up(&mut conn, json!({"id": 3, "kind": "task"})).unwrap();

        // task -> area under a task: refused.
        let err = up(&mut conn, json!({"id": 4, "kind": "area"})).unwrap_err();
        assert!(err.contains("sits under the task SPOKE-3"), "{err}");

        // area -> task holding an area child: refused.
        let err = up(&mut conn, json!({"id": 2, "kind": "task"})).unwrap_err();
        assert!(err.contains("holds the area SPOKE-5 Garage"), "{err}");

        // done task -> area: refused.
        close(&mut conn, "SPOKE-6", SettableStatus::Done);
        let err = up(&mut conn, json!({"id": 6, "kind": "area"})).unwrap_err();
        assert!(err.contains("areas never finish"), "{err}");

        // root -> task: refused.
        let err = up(&mut conn, json!({"id": 1, "kind": "task"})).unwrap_err();
        assert!(err.contains("is the root"), "{err}");

        // Same-kind is a harmless no-op change; area with no area children -> task ok.
        up(&mut conn, json!({"id": 5, "kind": "task"})).unwrap();
        let kind: String = conn
            .query_row("SELECT kind FROM nodes WHERE id = 5", [], |r| r.get(0))
            .unwrap();
        assert_eq!(kind, "task");
        // Refused changes wrote nothing.
        let kind: String = conn
            .query_row("SELECT kind FROM nodes WHERE id = 4", [], |r| r.get(0))
            .unwrap();
        assert_eq!(kind, "task");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn rejects_bad_input_with_guidance() {
        let (mut conn, dir) = temp_db("up-errors");
        add(&mut conn, args(None, "Task", Kind::Task)).unwrap(); // 2
        let err = |conn: &mut Connection, v: serde_json::Value| up(conn, v).unwrap_err();

        assert!(err(&mut conn, json!({"id": 2})).contains("nothing to update"));
        assert!(
            err(
                &mut conn,
                json!({"id": 2, "notes": "a", "notes_append": "b"})
            )
            .contains("not both")
        );
        assert!(
            err(&mut conn, json!({"id": 2, "notes_append": ""})).contains("notes_append is empty")
        );
        assert!(
            err(&mut conn, json!({"id": 2, "notes_append": "   "}))
                .contains("notes_append is empty")
        );
        assert!(err(&mut conn, json!({"id": 2, "due": "next friday"})).contains("YYYY-MM-DD"));
        assert!(
            err(&mut conn, json!({"id": 2, "defer_until": "2026-13-45"})).contains("YYYY-MM-DD")
        );
        assert!(err(&mut conn, json!({"id": 2, "priority": 7})).contains("priority must be"));
        assert!(err(&mut conn, json!({"id": 2, "title": "   "})).contains("empty"));
        assert!(
            err(&mut conn, json!({"id": 99, "title": "x"})).contains("SPOKE-99 does not exist")
        );
        assert!(err(&mut conn, json!({"id": "bogus", "title": "x"})).contains("bogus"));
        // Nothing was written, no event logged.
        let events: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM events WHERE type = 'updated'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(events, 0);
        assert_eq!(row(&conn, 2).0, "Task");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn logs_updated_event_with_changed_fields() {
        let (mut conn, dir) = temp_db("up-event");
        add(&mut conn, args(None, "Task", Kind::Task)).unwrap(); // 2
        up(
            &mut conn,
            json!({"id": 2, "title": "T2", "priority": 0, "due": "", "aliases": [],
                   "notes_append": "x", "agent": "tester"}),
        )
        .unwrap();
        let (count, agent, detail): (i64, String, String) = conn
            .query_row(
                "SELECT COUNT(*), MAX(agent), MAX(detail) FROM events WHERE node_id = 2 AND type = 'updated'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(
            (count, agent.as_str(), detail.as_str()),
            (1, "tester", "title, priority, due, aliases, notes appended")
        );
        let _ = std::fs::remove_dir_all(dir);
    }
}
