//! `next`: the queue of what needs the user and what can be worked on.
//!
//! Sections, in order: NEEDS YOU (questions, then stalled claims), FOCUS (the global
//! ranked list), DUE SOON (overdue, then due within 7 days), UP NEXT (by priority
//! level 1..4, oldest first; unset levels inherit the nearest ancestor's) and STALE
//! (up to three work items untouched for 30 days). A node is listed once at most, and
//! so is each actionable step: a line whose `next:` step (or, for a leaf, the node
//! itself) is already represented higher up is dropped, so a project and its sub-project
//! never both point at the same step.
//!
//! NEEDS YOU holds questions (`waiting`), then stalled claims (lease run out), then
//! answered tasks: open again after `answer`, kept here as "answered" until an agent
//! claims them, so the reply is picked up next rather than sinking into the queue.
//!
//! The queue lists work items, not every leaf: top-level tasks (directly under areas)
//! plus any deeper task that carries its own level or due date, and anything on the
//! focus list. Each line shows progress and the next actionable step beneath it.

use std::collections::{HashMap, HashSet};

use chrono::{Duration, NaiveDate, Utc};
use rusqlite::{Connection, params};

use super::focus::{focus_numbers, too_long_note};
use super::{NODE_COLS, ToolResult, ancestors, db_err, node_from_row, require};
use crate::db::now;
use crate::format::{Row, ago, due_flag, is_unplanned, parse_date, progress, render_rows};
use crate::model::{DEFAULT_USER, Kind, Node, NodeId, ROOT_ID, Stage, Status, WorkType};
use crate::tools::args::NextArgs;

const DEFAULT_LIMIT: i64 = 5;
const MAX_LIMIT: i64 = 50;
/// DUE SOON covers overdue items and anything due within this many days.
const DUE_SOON_DAYS: i64 = 7;
/// STALE: untouched for at least this many days, at most this many lines.
const STALE_DAYS: i64 = 30;
const STALE_MAX: usize = 3;

pub fn next(conn: &Connection, args: NextArgs, today: NaiveDate) -> ToolResult {
    let scope_id = match &args.scope {
        Some(s) => s.parse()?,
        None => ROOT_ID,
    };
    let limit = match &args.limit {
        Some(l) => l.parse("limit")?.clamp(1, MAX_LIMIT),
        None => DEFAULT_LIMIT,
    } as usize;
    let filter = Filter {
        work_type: args
            .work_type
            .as_deref()
            .map(WorkType::parse_loose)
            .transpose()?,
        stage: args.stage.as_deref().map(Stage::parse_loose).transpose()?,
    };
    let scope = require(conn, scope_id)?;
    if scope.status.is_closed() {
        return Err(format!(
            "{} {} is {}; nothing under it is in the queue. Use set_status to reopen it.",
            scope.id,
            scope.title,
            scope.status.as_str()
        ));
    }
    // The level the scope itself carries, own or inherited from above it.
    let inherited = match scope.priority {
        Some(p) => Some(p),
        None => ancestors(conn, scope.id)?
            .iter()
            .rev()
            .find_map(|a| a.priority),
    };

    let tree = Tree::load(conn)?;
    let mut queue = Queue {
        now: now(),
        today,
        needs_you: Vec::new(),
        focus: Vec::new(),
        work: Vec::new(),
    };
    queue.walk(
        &tree,
        &scope,
        Walk {
            top: true,
            reachable: true,
            level: inherited,
        },
    );

    if queue.needs_you.len() + queue.focus.len() + queue.work.len() == 0 {
        return Ok(format!(
            "Nothing actionable under {} {}. Add a task with add.",
            scope.id, scope.title
        ));
    }

    // The filters look at each line's own node, before anything is sorted or limited.
    if filter.is_set() {
        queue.needs_you.retain(|n| filter.matches(n));
        queue.focus.retain(|i| filter.matches(&i.node));
        queue.work.retain(|i| filter.matches(&i.node));
        if queue.needs_you.len() + queue.focus.len() + queue.work.len() == 0 {
            return Ok(format!(
                "Nothing under {} {} matches {}.",
                scope.id,
                scope.title,
                filter.describe()
            ));
        }
    }

    // Questions first, then stalled claims, then answered tasks (walk order within each).
    queue.needs_you.sort_by_key(|n| match n.status {
        Status::Waiting => 0,
        Status::InProgress => 1,
        _ => 2,
    });
    let ranks = focus_numbers(conn)?;
    queue
        .focus
        .sort_by(|a, b| a.node.focus_rank.cmp(&b.node.focus_rank));
    let horizon = today + Duration::days(DUE_SOON_DAYS);
    let (mut due, mut rest): (Vec<Item>, Vec<Item>) = queue.work.into_iter().partition(|i| {
        i.node
            .due_at
            .as_deref()
            .and_then(parse_date)
            .is_some_and(|d| d <= horizon)
    });
    due.sort_by(|a, b| {
        let key = |i: &Item| {
            (
                parse_date(i.node.due_at.as_deref().unwrap_or_default()),
                i.sort_level(),
                i.node.created_at.clone(),
            )
        };
        key(a).cmp(&key(b))
    });
    rest.sort_by(|a, b| {
        let key = |i: &Item| (i.sort_level(), i.node.created_at.clone(), i.node.id);
        key(a).cmp(&key(b))
    });

    // One line per actionable step, in section order: needs-you nodes first, then focus,
    // due and the rest. A due or up-next line pointing at a step already represented is
    // dropped.
    let step_of = |i: &Item| {
        tree.next_step(i.node.id, i.node.ordered, today)
            .map_or(i.node.id, |s| s.id)
    };
    let mut steps: HashSet<NodeId> = queue.needs_you.iter().map(|n| n.id).collect();
    // The focus list is the user's explicit ranking, so every entry stays; their steps
    // still count, so later sections do not repeat them.
    steps.extend(queue.focus.iter().map(step_of));
    let mut first_for_step = |items: Vec<Item>| -> Vec<Item> {
        items
            .into_iter()
            .filter(|i| steps.insert(step_of(i)))
            .collect()
    };
    let due = first_for_step(due);
    let rest = first_for_step(rest);
    let total = queue.needs_you.len() + queue.focus.len() + due.len() + rest.len();

    let now_dt = Utc::now();
    let mut budget = limit;
    let mut take = |len: usize| {
        let n = len.min(budget);
        budget -= n;
        n
    };
    let mut sections: Vec<(&str, Vec<Row>)> = Vec::new();
    let mut shown: HashSet<NodeId> = HashSet::new();

    let n = take(queue.needs_you.len());
    let rows: Vec<Row> = queue.needs_you[..n]
        .iter()
        .map(|node| needs_you_row(conn, node, now_dt))
        .collect::<Result<_, _>>()?;
    shown.extend(queue.needs_you[..n].iter().map(|node| node.id));
    sections.push(("NEEDS YOU", rows));

    let n = take(queue.focus.len());
    let rows = queue.focus[..n]
        .iter()
        .map(|i| {
            let mut row = work_row(i, &tree, today);
            row.rank = ranks.get(&i.node.id).copied();
            row
        })
        .collect();
    shown.extend(queue.focus[..n].iter().map(|i| i.node.id));
    sections.push(("FOCUS", rows));

    let n = take(due.len());
    sections.push((
        "DUE SOON",
        due[..n].iter().map(|i| work_row(i, &tree, today)).collect(),
    ));
    shown.extend(due[..n].iter().map(|i| i.node.id));

    let n = take(rest.len());
    sections.push((
        "UP NEXT",
        rest[..n]
            .iter()
            .map(|i| work_row(i, &tree, today))
            .collect(),
    ));
    shown.extend(rest[..n].iter().map(|i| i.node.id));

    // A nudge outside the limit: work items nobody has touched in a month.
    let cutoff =
        (now_dt - Duration::days(STALE_DAYS)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let mut stale: Vec<&Item> = due
        .iter()
        .chain(rest.iter())
        .filter(|i| !shown.contains(&i.node.id) && i.node.touched_at <= cutoff)
        .collect();
    stale.sort_by(|a, b| (&a.node.touched_at, a.node.id).cmp(&(&b.node.touched_at, b.node.id)));
    stale.truncate(STALE_MAX);
    let rows = stale
        .iter()
        .map(|i| Row {
            rank: None,
            id: i.node.id.to_string(),
            flags: format!(
                "untouched {}",
                ago(&i.node.touched_at, now_dt).trim_end_matches(" ago")
            ),
            rest: i.node.title.clone(),
        })
        .collect();
    shown.extend(stale.iter().map(|i| i.node.id));
    sections.push(("STALE", rows));

    let id_width = sections
        .iter()
        .flat_map(|(_, rows)| rows.iter().map(|r| r.id.len()))
        .max()
        .unwrap_or(0);
    let mut out = Vec::new();
    for (title, rows) in &sections {
        if rows.is_empty() {
            continue;
        }
        out.push((*title).to_string());
        out.extend(render_rows(rows, id_width));
    }
    if let Some(note) = too_long_note(ranks.len()) {
        out.push(format!("({note})"));
    }
    if total > shown.len() {
        out.push(format!(
            "(+{} more open. Pass scope or limit.)",
            total - shown.len()
        ));
    }
    Ok(out.join("\n"))
}

/// The `type` and `stage` filters of `next`. A node matches on its own values only.
struct Filter {
    work_type: Option<WorkType>,
    stage: Option<Stage>,
}

impl Filter {
    fn is_set(&self) -> bool {
        self.work_type.is_some() || self.stage.is_some()
    }

    fn matches(&self, n: &Node) -> bool {
        self.work_type.is_none_or(|t| n.work_type == Some(t))
            && self.stage.is_none_or(|s| n.stage == Some(s))
    }

    /// `type bug, stage in_review`, naming only the filters given.
    fn describe(&self) -> String {
        let mut parts = Vec::new();
        if let Some(t) = self.work_type {
            parts.push(format!("type {t}"));
        }
        if let Some(s) = self.stage {
            parts.push(format!("stage {s}"));
        }
        parts.join(", ")
    }
}

/// A queue entry and its effective level (own, or the nearest ancestor's).
struct Item {
    node: Node,
    level: Option<i64>,
}

impl Item {
    /// Unset levels sort after level 4.
    fn sort_level(&self) -> i64 {
        self.level.unwrap_or(5)
    }
}

/// `SPOKE-142  p1 due Nov 20  Deploy game to production (2/6)  next: SPOKE-145 Add secrets`.
fn work_row(item: &Item, tree: &Tree, today: NaiveDate) -> Row {
    let node = &item.node;
    let mut flags = Vec::new();
    if let Some(level) = item.level {
        flags.push(format!("p{level}"));
    }
    if let Some(due) = &node.due_at {
        flags.push(due_flag(due, today));
    }
    if node.recur.is_some() {
        flags.push("repeats".to_string());
    }
    if let Some(t) = node.work_type {
        flags.push(t.to_string());
    }
    if let Some(s) = node.stage {
        flags.push(s.to_string());
    }
    let mut rest = node.title.clone();
    if let Some(p) = progress(node) {
        rest.push(' ');
        rest.push_str(&p);
    }
    if is_unplanned(node) {
        rest.push_str("  unplanned");
    }
    if let Some(step) = tree.next_step(node.id, node.ordered, today) {
        rest.push_str(&format!("  next: {} {}", step.id, step.title));
    }
    Row {
        rank: None,
        id: node.id.to_string(),
        flags: flags.join(" "),
        rest,
    }
}

/// `SPOKE-88  waiting  Set up emulator: "Install to D: or E:?" (sonnet-2, 3h ago)`,
/// `SPOKE-90  stalled  ...  (claim by opus-red expired 2h ago)` or
/// `SPOKE-91  answered  ...  (answered 5m ago; claim it and read the notes)`.
fn needs_you_row(
    conn: &Connection,
    n: &Node,
    now_dt: chrono::DateTime<Utc>,
) -> Result<Row, String> {
    let (flags, rest) = if n.status == Status::Waiting {
        let mut rest = n.title.clone();
        if let Some(q) = &n.waiting_on {
            rest.push_str(&format!(": \"{q}\""));
        }
        let who = n
            .claimed_by
            .as_deref()
            .map(|a| format!("{a}, "))
            .unwrap_or_default();
        // When it was asked, from the log (updated_at moves with any later edit).
        let asked: Option<String> = conn
            .query_row(
                "SELECT MAX(at) FROM events WHERE node_id = ?1 AND type = 'asked'",
                params![n.id.0],
                |r| r.get(0),
            )
            .map_err(db_err)?;
        let asked = asked.unwrap_or_else(|| n.updated_at.clone());
        rest.push_str(&format!(" ({who}{})", ago(&asked, now_dt)));
        ("waiting", rest)
    } else if let (Status::Open, Some(at)) = (n.status, n.answered_at.as_deref()) {
        (
            "answered",
            format!(
                "{}  (answered {}; claim it and read the notes)",
                n.title,
                ago(at, now_dt)
            ),
        )
    } else {
        let who = n.claimed_by.as_deref().unwrap_or("an agent");
        let since = n
            .claim_expires
            .as_deref()
            .map(|e| ago(e, now_dt))
            .unwrap_or_default();
        (
            "stalled",
            format!("{}  (claim by {who} expired {since})", n.title),
        )
    };
    Ok(Row {
        rank: None,
        id: n.id.to_string(),
        flags: flags.to_string(),
        rest,
    })
}

/// The live (not done or dropped) nodes, indexed by parent, in sibling order.
struct Tree {
    children: HashMap<NodeId, Vec<Node>>,
}

impl Tree {
    fn load(conn: &Connection) -> Result<Self, String> {
        let mut stmt = conn
            .prepare_cached(&format!(
                "SELECT {NODE_COLS} FROM nodes n
                 WHERE n.user_id = ?1 AND n.status NOT IN ('done', 'dropped') AND n.parent_id IS NOT NULL
                 ORDER BY n.parent_id, n.position"
            ))
            .map_err(db_err)?;
        let rows: Vec<Node> = stmt
            .query_map(params![DEFAULT_USER], node_from_row)
            .and_then(|r| r.collect())
            .map_err(db_err)?;
        let mut children: HashMap<NodeId, Vec<Node>> = HashMap::new();
        for n in rows {
            if let Some(p) = n.parent_id {
                children.entry(p).or_default().push(n);
            }
        }
        Ok(Tree { children })
    }

    fn kids(&self, id: NodeId) -> &[Node] {
        self.children.get(&id).map_or(&[], Vec::as_slice)
    }

    /// The first actionable step beneath a work item, for the `next:` hint. An ordered
    /// node offers only its first unfinished child; an unordered one, its first open one.
    fn next_step(&self, id: NodeId, ordered: bool, today: NaiveDate) -> Option<&Node> {
        for kid in self.kids(id) {
            let deferred = kid
                .defer_until
                .as_deref()
                .and_then(parse_date)
                .is_some_and(|d| d > today);
            if kid.status == Status::Open && !deferred {
                if self.kids(kid.id).is_empty() {
                    if kid.kind == Kind::Task {
                        return Some(kid);
                    }
                } else if let Some(step) = self.next_step(kid.id, kid.ordered, today) {
                    return Some(step);
                }
            }
            if ordered {
                return None;
            }
        }
        None
    }
}

/// What the walk knows about the path down to the current parent.
#[derive(Clone, Copy)]
struct Walk {
    /// Still among areas: a task found here is a top-level work item.
    top: bool,
    /// No ordered parent above holds this branch behind an earlier step.
    reachable: bool,
    /// The level inherited from the nearest ancestor that sets one.
    level: Option<i64>,
}

struct Queue {
    now: String,
    today: NaiveDate,
    needs_you: Vec<Node>,
    focus: Vec<Item>,
    work: Vec<Item>,
}

impl Queue {
    /// Questions (`waiting`) anywhere beneath a live claim.
    fn questions_below(&mut self, tree: &Tree, parent: &Node) {
        for kid in tree.kids(parent.id) {
            if kid.status == Status::Waiting {
                self.needs_you.push(kid.clone());
            }
            self.questions_below(tree, kid);
        }
    }

    fn walk(&mut self, tree: &Tree, parent: &Node, at: Walk) {
        let mut earlier_open_step = false;
        for kid in tree.kids(parent.id) {
            let blocked = parent.ordered && earlier_open_step;
            earlier_open_step = true;
            // What needs the user shows even when deferred; everything else waits.
            match kid.status {
                Status::InProgress => {
                    let live = kid
                        .claim_expires
                        .as_deref()
                        .is_some_and(|e| e > self.now.as_str());
                    if live {
                        // A claimed subtree is hidden from everyone else, but a worker's
                        // question inside an orchestrator's project still reaches the user.
                        self.questions_below(tree, kid);
                    } else {
                        self.needs_you.push(kid.clone());
                    }
                    continue;
                }
                Status::Waiting => {
                    self.needs_you.push(kid.clone());
                    continue;
                }
                // Answered: ready for an agent, so it stays near the top until claimed.
                Status::Open if kid.answered_at.is_some() && kid.kind == Kind::Task => {
                    self.needs_you.push(kid.clone());
                    continue;
                }
                _ => {}
            }
            if kid
                .defer_until
                .as_deref()
                .and_then(parse_date)
                .is_some_and(|d| d > self.today)
            {
                continue;
            }
            let level = kid.priority.or(at.level);
            let reachable = at.reachable && !blocked;
            let item = || Item {
                node: kid.clone(),
                level,
            };
            if kid.focus_rank.is_some() {
                // Explicitly ranked, so it is listed even behind an earlier step.
                self.focus.push(item());
            } else if kid.kind == Kind::Task
                && reachable
                && (at.top || kid.priority.is_some() || kid.due_at.is_some())
                // A project whose every open step is claimed, waiting or deferred has
                // nothing to offer right now (claiming it would be refused anyway).
                && (tree.kids(kid.id).is_empty()
                    || tree.next_step(kid.id, kid.ordered, self.today).is_some())
            {
                self.work.push(item());
            }
            self.walk(
                tree,
                kid,
                Walk {
                    top: at.top && reachable && kid.kind == Kind::Area,
                    reachable,
                    level,
                },
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::IdArg;
    use crate::store::add;
    use crate::store::add::tests::args;
    use crate::store::test_support::temp_db;

    fn next_args(scope: Option<i64>, limit: Option<i64>) -> NextArgs {
        NextArgs {
            scope: scope.map(IdArg::Number),
            limit: limit.map(crate::model::NumArg::Number),
            work_type: None,
            stage: None,
        }
    }

    #[test]
    fn empty_tree_says_what_to_do() {
        let (conn, dir) = temp_db("next-empty");
        assert_eq!(
            next(
                &conn,
                next_args(None, None),
                crate::format::today_in(chrono_tz::UTC)
            )
            .unwrap(),
            "Nothing actionable under SPOKE-1 To do. Add a task with add."
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn lists_work_items_with_next_step_and_respects_order() {
        let (mut conn, dir) = temp_db("next");
        add(&mut conn, args(None, "Home", Kind::Area)).unwrap(); // 2
        let mut project = args(Some(2), "Fix garage", Kind::Task); // 3
        project.node.ordered = Some(true);
        project.node.priority = Some(crate::model::NumArg::Number(2));
        add(&mut conn, project).unwrap();
        add(&mut conn, args(Some(3), "Buy parts", Kind::Task)).unwrap(); // 4
        add(&mut conn, args(Some(3), "Install", Kind::Task)).unwrap(); // 5
        let mut urgent = args(Some(2), "Pay bill", Kind::Task); // 6
        urgent.node.priority = Some(crate::model::NumArg::Number(1));
        add(&mut conn, urgent).unwrap();
        add(&mut conn, args(Some(2), "Someday", Kind::Task)).unwrap(); // 7

        let out = next(
            &conn,
            next_args(None, None),
            crate::format::today_in(chrono_tz::UTC),
        )
        .unwrap();
        assert_eq!(
            out,
            "UP NEXT\n  SPOKE-6  p1  Pay bill  unplanned\n  SPOKE-3  p2  Fix garage (0/2)  next: SPOKE-4 Buy parts\n  SPOKE-7      Someday  unplanned"
        );
        let out = next(
            &conn,
            next_args(None, Some(1)),
            crate::format::today_in(chrono_tz::UTC),
        )
        .unwrap();
        assert!(
            out.ends_with("(+2 more open. Pass scope or limit.)"),
            "{out}"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn ordered_area_only_offers_first_step() {
        let (mut conn, dir) = temp_db("next-ordered");
        let mut area = args(None, "Pipeline", Kind::Area);
        area.node.ordered = Some(true);
        add(&mut conn, area).unwrap(); // 2
        add(&mut conn, args(Some(2), "First", Kind::Task)).unwrap();
        add(&mut conn, args(Some(2), "Second", Kind::Task)).unwrap();
        let out = next(
            &conn,
            next_args(Some(2), None),
            crate::format::today_in(chrono_tz::UTC),
        )
        .unwrap();
        assert_eq!(out, "UP NEXT\n  SPOKE-3  First  unplanned");
        let _ = std::fs::remove_dir_all(dir);
    }

    fn set_fields(conn: &Connection, id: i64, work_type: Option<&str>, stage: Option<&str>) {
        conn.execute(
            "UPDATE nodes SET work_type = ?1, stage = ?2 WHERE id = ?3",
            params![work_type, stage, id],
        )
        .unwrap();
    }

    fn typed_tree(name: &str) -> (Connection, std::path::PathBuf) {
        let (mut conn, dir) = temp_db(name);
        add(&mut conn, args(None, "Home", Kind::Area)).unwrap(); // 2
        for title in ["Crash", "Polish", "Ship"] {
            add(&mut conn, args(Some(2), title, Kind::Task)).unwrap(); // 3, 4, 5
        }
        set_fields(&conn, 3, Some("bug"), Some("in_review"));
        set_fields(&conn, 4, Some("bug"), Some("planned"));
        set_fields(&conn, 5, Some("feature"), Some("in_review"));
        (conn, dir)
    }

    fn run(
        conn: &Connection,
        work_type: Option<&str>,
        stage: Option<&str>,
    ) -> Result<String, String> {
        let mut a = next_args(None, None);
        a.work_type = work_type.map(str::to_string);
        a.stage = stage.map(str::to_string);
        next(conn, a, crate::format::today_in(chrono_tz::UTC))
    }

    #[test]
    fn type_filter_keeps_only_matching_items() {
        let (conn, dir) = typed_tree("next-type");
        let out = run(&conn, Some("Bug"), None).unwrap();
        assert!(out.contains("Crash") && out.contains("Polish"), "{out}");
        assert!(!out.contains("Ship"), "{out}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn stage_filter_keeps_only_matching_items() {
        let (conn, dir) = typed_tree("next-stage");
        let out = run(&conn, None, Some("in review")).unwrap();
        assert!(out.contains("Crash") && out.contains("Ship"), "{out}");
        assert!(!out.contains("Polish"), "{out}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn filters_combine_with_and() {
        let (conn, dir) = typed_tree("next-both");
        let out = run(&conn, Some("bug"), Some("in_review")).unwrap();
        assert!(out.contains("Crash"), "{out}");
        assert!(!out.contains("Polish") && !out.contains("Ship"), "{out}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn empty_filter_result_names_the_filters() {
        let (conn, dir) = typed_tree("next-none");
        assert_eq!(
            run(&conn, Some("chore"), Some("done")).unwrap(),
            "Nothing under SPOKE-1 To do matches type chore, stage done."
        );
        assert_eq!(
            run(&conn, Some("chore"), None).unwrap(),
            "Nothing under SPOKE-1 To do matches type chore."
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn bad_filter_value_errors() {
        let (conn, dir) = typed_tree("next-bad");
        let err = run(&conn, Some("bugg"), None).unwrap_err();
        assert!(err.contains("bug"), "{err}");
        assert!(run(&conn, None, Some("nope")).is_err());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn lines_show_type_and_stage_flags() {
        let (conn, dir) = typed_tree("next-flags");
        let out = run(&conn, None, None).unwrap();
        assert!(out.contains("SPOKE-3  bug in_review      Crash"), "{out}");
        assert!(out.contains("SPOKE-5  feature in_review  Ship"), "{out}");
        let _ = std::fs::remove_dir_all(dir);
    }
}
