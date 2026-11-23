//! `search`: find nodes by several keywords, matched as alternatives.
//!
//! Candidates come from the trigram FTS5 index (`node_fts`), which holds normalized
//! title, aliases and notes. Each candidate is then scored in Rust per keyword:
//! whole word > word prefix > substring > typo (edit distance or shared trigrams), and
//! title (x3) > aliases (x2) > notes (x1, exact matches only). Keywords under three
//! letters, which trigrams cannot see, match whole words in titles and aliases only.

use std::collections::HashMap;

use chrono::NaiveDate;
use rusqlite::{Connection, params};

use super::focus::focus_numbers;
use super::{ToolResult, db_err, require};
use crate::format::{due_flag, is_unplanned, parse_date, progress, short_date};
use crate::model::{DEFAULT_USER, Node, NodeId, ROOT_ID, Stage, Status, WorkType};
use crate::text::{keywords, match_strength, normalize, trigrams};
use crate::tools::args::SearchArgs;

const DEFAULT_LIMIT: i64 = 10;
const MAX_LIMIT: i64 = 50;
const MAX_KEYWORDS: usize = 12;
/// FTS candidates scored per call; ranked by bm25 first, so the best survive the cut.
const MAX_CANDIDATES: i64 = 500;

const W_TITLE: f64 = 3.0;
const W_ALIAS: f64 = 2.0;
const W_NOTES: f64 = 1.0;

/// Normalized index text of one node.
struct Doc {
    title: String,
    aliases: String,
    notes: String,
}

/// The parts of every node needed for scope, closed ancestors, paths and levels.
struct Meta {
    parent: Option<NodeId>,
    title: String,
    status: Status,
    priority: Option<i64>,
    work_type: Option<WorkType>,
    stage: Option<Stage>,
}

#[derive(Clone, Copy, PartialEq)]
enum Field {
    Title,
    Alias,
    Notes,
}

struct Hit {
    id: NodeId,
    score: f64,
    /// Set when nothing matched the title: why this node is in the results.
    via: Option<String>,
}

pub fn search(conn: &Connection, args: SearchArgs, today: NaiveDate) -> ToolResult {
    let work_type = args
        .work_type
        .as_deref()
        .map(WorkType::parse_loose)
        .transpose()?;
    let stage = args.stage.as_deref().map(Stage::parse_loose).transpose()?;
    let mut kws = keywords(&args.query);
    if kws.is_empty() && work_type.is_none() && stage.is_none() {
        return Err(
            "query has no words. Pass a few keywords as alternatives, e.g. \"server homelab nas\"."
                .to_string(),
        );
    }
    kws.truncate(MAX_KEYWORDS);
    let phrase = format!(" {} ", normalize(&args.query));
    let limit = match &args.limit {
        Some(l) => l.parse("limit")?.clamp(1, MAX_LIMIT),
        None => DEFAULT_LIMIT,
    } as usize;
    let scope = match &args.scope {
        Some(s) => require(conn, s.parse()?)?.id,
        None => ROOT_ID,
    };
    let include_done = args.include_done.unwrap_or(false);

    let meta = load_meta(conn)?;
    let mut hits = if kws.is_empty() {
        // Filters only: every node whose own type/stage match, ranked by level then id.
        meta.keys()
            .map(|id| Hit {
                id: *id,
                score: 0.0,
                via: None,
            })
            .collect()
    } else {
        keyword_hits(conn, &kws, &phrase)?
    };
    let mut hidden_closed = 0;
    hits.retain(|h| {
        if h.id == ROOT_ID
            || !within(&meta, h.id, scope)
            || !matches_filters(&meta, h.id, work_type, stage)
        {
            return false;
        }
        if !include_done && closed_here_or_above(&meta, h.id) {
            hidden_closed += 1;
            return false;
        }
        true
    });
    hits.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then_with(|| {
                let level = |id| effective_level(&meta, id).unwrap_or(5);
                level(a.id).cmp(&level(b.id))
            })
            .then(a.id.cmp(&b.id))
    });

    let hidden_note = (hidden_closed > 0).then(|| {
        format!(
            "({hidden_closed} done or dropped matches hidden; pass include_done: true to see them.)"
        )
    });
    let subject = if kws.is_empty() {
        filter_label(work_type, stage)
    } else {
        kws.join(", ")
    };
    if hits.is_empty() {
        let mut out = if kws.is_empty() {
            format!("No matches for {subject}. Browse with get {ROOT_ID}.")
        } else {
            format!(
                "No matches for {subject}. Try synonyms or other spellings, or browse with get {ROOT_ID}."
            )
        };
        if let Some(note) = hidden_note {
            out.push('\n');
            out.push_str(&note);
        }
        return Ok(out);
    }

    let focus = focus_numbers(conn)?;
    let mut out = vec![format!(
        "{} {} for {}:",
        hits.len(),
        if hits.len() == 1 { "match" } else { "matches" },
        subject
    )];
    for hit in hits.iter().take(limit) {
        let node = require(conn, hit.id)?;
        out.push(format!(
            "  {}",
            result_line(&node, &meta, &focus, hit, today)
        ));
    }
    if hits.len() > limit {
        out.push(format!(
            "(+{} more matches. Add or change keywords, or pass scope.)",
            hits.len() - limit
        ));
    }
    if let Some(note) = hidden_note {
        out.push(note);
    }
    Ok(out.join("\n"))
}

/// Candidate hits for keywords: FTS scoring plus the typo fallback.
fn keyword_hits(conn: &Connection, kws: &[String], phrase: &str) -> Result<Vec<Hit>, String> {
    let docs = candidates(conn, kws)?;
    let mut hits: Vec<Hit> = docs
        .iter()
        .filter_map(|(id, doc)| score(*id, doc, kws, phrase))
        .collect();
    // Typo fallback: a keyword nothing resembles in a title or alias gets a full scan
    // of titles and aliases (short columns), since a typo may share no trigram at all.
    let unmatched: Vec<String> = kws
        .iter()
        .filter(|k| k.chars().count() >= 4)
        .filter(|k| {
            !docs
                .values()
                .any(|d| best(k, &d.title, true) >= 0.6 || best(k, &d.aliases, true) >= 0.6)
        })
        .cloned()
        .collect();
    if !unmatched.is_empty() {
        for (id, doc) in scan_titles(conn)? {
            if docs.contains_key(&id) {
                continue;
            }
            if let Some(hit) = score(id, &doc, kws, phrase) {
                hits.push(hit);
            }
        }
    }

    Ok(hits)
}

/// The node's own type and stage must equal every filter given.
fn matches_filters(
    meta: &HashMap<NodeId, Meta>,
    id: NodeId,
    work_type: Option<WorkType>,
    stage: Option<Stage>,
) -> bool {
    let Some(m) = meta.get(&id) else {
        return false;
    };
    work_type.is_none_or(|t| m.work_type == Some(t)) && stage.is_none_or(|s| m.stage == Some(s))
}

/// `type bug, stage in_review`.
fn filter_label(work_type: Option<WorkType>, stage: Option<Stage>) -> String {
    let mut parts = Vec::new();
    if let Some(t) = work_type {
        parts.push(format!("type {t}"));
    }
    if let Some(s) = stage {
        parts.push(format!("stage {s}"));
    }
    parts.join(", ")
}

/// `SPOKE-30 task p1 (0/4)  Homelab server migration  in Hobbies / Homelab  (alias "nas")`.
fn result_line(
    node: &Node,
    meta: &HashMap<NodeId, Meta>,
    focus: &HashMap<NodeId, usize>,
    hit: &Hit,
    today: NaiveDate,
) -> String {
    let mut flags = vec![node.id.to_string(), node.kind.as_str().to_string()];
    if node.status != Status::Open {
        flags.push(node.status.as_str().to_string());
    }
    if let Some(level) = effective_level(meta, node.id) {
        flags.push(format!("p{level}"));
    }
    if let Some(t) = node.work_type {
        flags.push(t.to_string());
    }
    if let Some(st) = node.stage {
        flags.push(st.to_string());
    }
    if let Some(n) = focus.get(&node.id) {
        flags.push(format!("focus #{n}"));
    }
    if let Some(due) = &node.due_at {
        flags.push(due_flag(due, today));
    }
    if let Some(defer) = node.defer_until.as_deref()
        && parse_date(defer).is_some_and(|d| d > today)
    {
        flags.push(format!("deferred until {}", short_date(defer, today)));
    }
    if node.recur.is_some() {
        flags.push("repeats".to_string());
    }
    if let Some(p) = progress(node) {
        flags.push(p);
    }
    if node.has_notes {
        flags.push("+notes".to_string());
    }
    if is_unplanned(node) {
        flags.push("unplanned".to_string());
    }
    let path = path_of(meta, node.id);
    let mut line = format!(
        "{}  {}  in {}",
        flags.join(" "),
        node.title,
        if path.is_empty() {
            "To do (top level)".to_string()
        } else {
            path
        }
    );
    if let Some(via) = &hit.via {
        line.push_str(&format!("  ({via})"));
    }
    line
}

/// FTS candidates for keywords of three letters or more (any shared trigram, best bm25
/// first), plus whole-word title/alias candidates for shorter keywords.
fn candidates(conn: &Connection, kws: &[String]) -> Result<HashMap<NodeId, Doc>, String> {
    let mut docs = HashMap::new();
    let mut grams: Vec<String> = Vec::new();
    for kw in kws {
        for g in trigrams(kw) {
            if !grams.contains(&g) {
                grams.push(g);
            }
        }
    }
    let read = |r: &rusqlite::Row<'_>| -> rusqlite::Result<(NodeId, Doc)> {
        Ok((
            NodeId(r.get(0)?),
            Doc {
                title: r.get(1)?,
                aliases: r.get(2)?,
                notes: r.get(3)?,
            },
        ))
    };
    if !grams.is_empty() {
        // Trigrams are alphanumeric (normalized), so quoting them is safe.
        let expr = grams
            .iter()
            .map(|g| format!("\"{g}\""))
            .collect::<Vec<_>>()
            .join(" OR ");
        let mut stmt = conn
            .prepare_cached(
                "SELECT rowid, title, aliases, notes FROM node_fts WHERE node_fts MATCH ?1
                 ORDER BY bm25(node_fts, 3.0, 2.0, 1.0) LIMIT ?2",
            )
            .map_err(db_err)?;
        let rows = stmt
            .query_map(params![expr, MAX_CANDIDATES], read)
            .and_then(|rows| rows.collect::<rusqlite::Result<Vec<_>>>())
            .map_err(db_err)?;
        docs.extend(rows);
    }
    for kw in kws.iter().filter(|k| k.chars().count() < 3) {
        let mut stmt = conn
            .prepare_cached(
                "SELECT rowid, title, aliases, notes FROM node_fts
                 WHERE title LIKE ?1 OR aliases LIKE ?1 LIMIT ?2",
            )
            .map_err(db_err)?;
        let rows = stmt
            .query_map(params![format!("%{kw}%"), MAX_CANDIDATES], read)
            .and_then(|rows| rows.collect::<rusqlite::Result<Vec<_>>>())
            .map_err(db_err)?;
        docs.extend(rows);
    }
    Ok(docs)
}

/// Every node's title and aliases (no notes), for the typo fallback.
fn scan_titles(conn: &Connection) -> Result<Vec<(NodeId, Doc)>, String> {
    let mut stmt = conn
        .prepare_cached("SELECT rowid, title, aliases FROM node_fts")
        .map_err(db_err)?;
    stmt.query_map([], |r| {
        Ok((
            NodeId(r.get(0)?),
            Doc {
                title: r.get(1)?,
                aliases: r.get(2)?,
                notes: String::new(),
            },
        ))
    })
    .and_then(|rows| rows.collect())
    .map_err(db_err)
}

/// The best strength of `kw` over the lines of one index column (each alias, and each
/// squashed form, is its own line).
fn best(kw: &str, column: &str, fuzzy: bool) -> f64 {
    column
        .lines()
        .map(|line| match_strength(kw, line, fuzzy))
        .fold(0.0, f64::max)
}

fn best_line<'a>(kw: &str, column: &'a str) -> Option<&'a str> {
    column
        .lines()
        .map(|line| (line, match_strength(kw, line, true)))
        .filter(|(_, s)| *s > 0.0)
        .max_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(line, _)| line)
}

/// Scores one node. `phrase` is the whole normalized query padded with spaces: a
/// multi-word title or alias that appears in it whole (`my server` in "tasks for my
/// server") earns a bonus, so the user's own words beat a bare keyword hit.
fn score(id: NodeId, doc: &Doc, kws: &[String], phrase: &str) -> Option<Hit> {
    let mut total = 0.0;
    let mut title_hit = false;
    let mut via: Option<String> = None;
    let contains_phrase = |column: &str| {
        column
            .lines()
            .find(|line| line.contains(' ') && phrase.contains(&format!(" {line} ")))
            .map(str::to_string)
    };
    if contains_phrase(&doc.title).is_some() {
        total += W_TITLE;
        title_hit = true;
    }
    if let Some(alias) = contains_phrase(&doc.aliases) {
        total += W_ALIAS;
        via = Some(format!("alias \"{alias}\""));
    }
    for kw in kws {
        let options = [
            (W_TITLE * best(kw, &doc.title, true), Field::Title),
            (W_ALIAS * best(kw, &doc.aliases, true), Field::Alias),
            // Notes are long prose: exact words and fragments only, no typo matching.
            (W_NOTES * best(kw, &doc.notes, false), Field::Notes),
        ];
        let (s, field) = options
            .into_iter()
            .max_by(|a, b| a.0.total_cmp(&b.0))
            .expect("three options");
        if s <= 0.0 {
            continue;
        }
        total += s;
        match field {
            Field::Title => title_hit = true,
            Field::Alias if via.is_none() => {
                via = best_line(kw, &doc.aliases).map(|a| format!("alias \"{a}\""));
            }
            Field::Notes if via.is_none() => via = Some("in notes".to_string()),
            _ => {}
        }
    }
    (total > 0.0).then_some(Hit {
        id,
        score: total,
        via: if title_hit { None } else { via },
    })
}

fn load_meta(conn: &Connection) -> Result<HashMap<NodeId, Meta>, String> {
    let mut stmt = conn
        .prepare_cached(
            "SELECT id, parent_id, title, status, priority, work_type, stage
             FROM nodes WHERE user_id = ?1",
        )
        .map_err(db_err)?;
    stmt.query_map(params![DEFAULT_USER], |r| {
        let status: String = r.get(3)?;
        Ok((
            NodeId(r.get(0)?),
            Meta {
                parent: r.get::<_, Option<i64>>(1)?.map(NodeId),
                title: r.get(2)?,
                status: status.parse().unwrap_or(Status::Open),
                priority: r.get(4)?,
                work_type: r
                    .get::<_, Option<String>>(5)?
                    .and_then(|t| WorkType::parse_loose(&t).ok()),
                stage: r
                    .get::<_, Option<String>>(6)?
                    .and_then(|t| Stage::parse_loose(&t).ok()),
            },
        ))
    })
    .and_then(|rows| rows.collect())
    .map_err(db_err)
}

/// Walks from `id` up to the root, yielding `id` first. Bounded so a corrupt cycle
/// cannot hang the call.
fn lineage(meta: &HashMap<NodeId, Meta>, id: NodeId) -> impl Iterator<Item = (NodeId, &Meta)> {
    let mut cur = Some(id);
    std::iter::from_fn(move || {
        let id = cur?;
        let m = meta.get(&id)?;
        cur = m.parent;
        Some((id, m))
    })
    .take(10_000)
}

fn within(meta: &HashMap<NodeId, Meta>, id: NodeId, scope: NodeId) -> bool {
    scope == ROOT_ID || lineage(meta, id).skip(1).any(|(a, _)| a == scope)
}

fn closed_here_or_above(meta: &HashMap<NodeId, Meta>, id: NodeId) -> bool {
    lineage(meta, id).any(|(_, m)| m.status.is_closed())
}

/// The node's own level, or the nearest ancestor's.
fn effective_level(meta: &HashMap<NodeId, Meta>, id: NodeId) -> Option<i64> {
    lineage(meta, id).find_map(|(_, m)| m.priority)
}

/// Breadcrumb below the root: `Hobbies / Homelab`.
fn path_of(meta: &HashMap<NodeId, Meta>, id: NodeId) -> String {
    let mut titles: Vec<&str> = lineage(meta, id)
        .skip(1)
        .filter(|(_, m)| m.parent.is_some())
        .map(|(_, m)| m.title.as_str())
        .collect();
    titles.reverse();
    titles.join(" / ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Kind;
    use crate::store::add::{self, tests::args};
    use crate::store::test_support::temp_db;

    fn search_args(query: &str, work_type: Option<&str>, stage: Option<&str>) -> SearchArgs {
        SearchArgs {
            query: query.into(),
            scope: None,
            include_done: None,
            limit: None,
            work_type: work_type.map(String::from),
            stage: stage.map(String::from),
        }
    }

    fn today() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 1, 1).unwrap()
    }

    fn seeded(name: &str) -> (Connection, std::path::PathBuf) {
        let (mut conn, dir) = temp_db(name);
        add::add(&mut conn, args(None, "Crash bug", Kind::Task)).unwrap();
        add::add(&mut conn, args(None, "Crash feature", Kind::Task)).unwrap();
        add::add(&mut conn, args(None, "Plain crash", Kind::Task)).unwrap();
        conn.execute(
            "UPDATE nodes SET work_type = 'bug', stage = 'in_review' WHERE title = 'Crash bug'",
            [],
        )
        .unwrap();
        conn.execute(
            "UPDATE nodes SET work_type = 'feature', stage = 'idea' WHERE title = 'Crash feature'",
            [],
        )
        .unwrap();
        (conn, dir)
    }

    #[test]
    fn empty_query_with_stage_lists_only_that_stage() {
        let (conn, dir) = seeded("search-stage");
        let out = search(&conn, search_args("", None, Some("In review")), today()).unwrap();
        assert!(out.starts_with("1 match for stage in_review:"), "{out}");
        assert!(out.contains("Crash bug"));
        assert!(!out.contains("Crash feature") && !out.contains("Plain crash"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn empty_query_combines_filters_with_and() {
        let (conn, dir) = seeded("search-and");
        let out = search(&conn, search_args("", Some("bug"), Some("idea")), today()).unwrap();
        assert!(
            out.starts_with("No matches for type bug, stage idea."),
            "{out}"
        );
        let out = search(
            &conn,
            search_args("", Some("bug"), Some("in_review")),
            today(),
        )
        .unwrap();
        assert!(
            out.starts_with("1 match for type bug, stage in_review:"),
            "{out}"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn type_filter_drops_non_matching_keyword_hits() {
        let (conn, dir) = seeded("search-type");
        let out = search(&conn, search_args("crash", None, None), today()).unwrap();
        assert!(out.starts_with("3 matches for crash:"), "{out}");
        let out = search(&conn, search_args("crash", Some("bug"), None), today()).unwrap();
        assert!(out.starts_with("1 match for crash:"), "{out}");
        assert!(out.contains("Crash bug") && !out.contains("Crash feature"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn bad_filter_value_errors() {
        let (conn, dir) = seeded("search-bad");
        let err = search(&conn, search_args("crash", Some("gremlin"), None), today()).unwrap_err();
        assert!(err.contains("Use one of"), "{err}");
        let err = search(&conn, search_args("", None, Some("nope")), today()).unwrap_err();
        assert!(err.contains("Use one of"), "{err}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn empty_query_without_filter_still_errors() {
        let (conn, dir) = seeded("search-empty");
        let err = search(&conn, search_args("", None, None), today()).unwrap_err();
        assert!(err.contains("query has no words"), "{err}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn result_lines_show_type_and_stage() {
        let (conn, dir) = seeded("search-flags");
        let out = search(&conn, search_args("crash bug", None, None), today()).unwrap();
        let line = out.lines().find(|l| l.contains("Crash bug")).unwrap();
        assert!(line.contains(" task bug in_review "), "{line}");
        let _ = std::fs::remove_dir_all(dir);
    }
}
