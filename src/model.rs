//! Core domain types: node IDs, kinds, statuses and the node record.

use std::fmt;
use std::str::FromStr;

use schemars::JsonSchema;
use serde::Deserialize;

/// The display prefix for node IDs (`SPOKE-142`).
pub const ID_PREFIX: &str = "SPOKE-";

/// The id of the seeded root node ("To do").
pub const ROOT_ID: NodeId = NodeId(1);

/// The only user until auth exists; every row carries it so auth is not a rewrite.
pub const DEFAULT_USER: i64 = 1;

/// A node's global, never-reused id. Shown as `SPOKE-<n>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NodeId(pub i64);

impl fmt::Display for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{ID_PREFIX}{}", self.0)
    }
}

impl FromStr for NodeId {
    type Err = String;

    /// Accepts `SPOKE-12`, `spoke-12`, `#12` or a bare `12`.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let t = s.trim();
        let digits =
            if t.len() >= ID_PREFIX.len() && t[..ID_PREFIX.len()].eq_ignore_ascii_case(ID_PREFIX) {
                &t[ID_PREFIX.len()..]
            } else {
                t.strip_prefix('#').unwrap_or(t)
            };
        match digits.parse::<i64>() {
            Ok(n) if n > 0 => Ok(NodeId(n)),
            _ => Err(format!(
                "'{s}' is not a node id. Use the form SPOKE-12 (or just 12)."
            )),
        }
    }
}

/// An id as a tool argument: models send either a string (`"SPOKE-12"`) or a number (`12`).
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum IdArg {
    Number(i64),
    Text(String),
}

impl IdArg {
    pub fn parse(&self) -> Result<NodeId, String> {
        match self {
            IdArg::Number(n) if *n > 0 => Ok(NodeId(*n)),
            IdArg::Number(n) => Err(format!("{n} is not a node id. Use the form SPOKE-12.")),
            IdArg::Text(s) => s.parse(),
        }
    }
}

/// A number as a tool argument: models send `5` or, now and then, `"5"`.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum NumArg {
    Number(i64),
    Text(String),
}

impl NumArg {
    pub fn parse(&self, field: &str) -> Result<i64, String> {
        match self {
            NumArg::Number(n) => Ok(*n),
            NumArg::Text(s) => s
                .trim()
                .parse()
                .map_err(|_| format!("{field} must be a whole number, not '{s}'.")),
        }
    }
}

/// `area` holds work forever and never finishes; `task` can be finished.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
#[schemars(inline)]
pub enum Kind {
    #[serde(alias = "Area", alias = "AREA")]
    Area,
    #[serde(alias = "Task", alias = "TASK")]
    Task,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Area => "area",
            Kind::Task => "task",
        }
    }
}

impl FromStr for Kind {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "area" => Ok(Kind::Area),
            "task" => Ok(Kind::Task),
            other => Err(format!("unknown kind '{other}'")),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Open,
    InProgress,
    Waiting,
    Done,
    Dropped,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Open => "open",
            Status::InProgress => "in_progress",
            Status::Waiting => "waiting",
            Status::Done => "done",
            Status::Dropped => "dropped",
        }
    }

    /// Done or dropped: no longer part of the live tree.
    pub fn is_closed(self) -> bool {
        matches!(self, Status::Done | Status::Dropped)
    }
}

impl FromStr for Status {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "open" => Ok(Status::Open),
            "in_progress" => Ok(Status::InProgress),
            "waiting" => Ok(Status::Waiting),
            "done" => Ok(Status::Done),
            "dropped" => Ok(Status::Dropped),
            other => Err(format!("unknown status '{other}'")),
        }
    }
}

/// The statuses `set_status` may set. Claims and questions have their own tools.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
#[schemars(inline)]
pub enum SettableStatus {
    #[serde(alias = "Done", alias = "DONE")]
    Done,
    #[serde(alias = "Dropped", alias = "DROPPED")]
    Dropped,
    #[serde(alias = "Open", alias = "OPEN", alias = "reopen")]
    Open,
}

impl From<SettableStatus> for Status {
    fn from(s: SettableStatus) -> Self {
        match s {
            SettableStatus::Done => Status::Done,
            SettableStatus::Dropped => Status::Dropped,
            SettableStatus::Open => Status::Open,
        }
    }
}

/// Turns free-form input into the stored snake_case form: trims, lowercases and treats
/// spaces and hyphens as underscores ("In review" -> "in_review").
fn normalize_loose(s: &str) -> String {
    s.trim().to_lowercase().replace([' ', '-'], "_")
}

/// What sort of work a node is. Stored as `work_type`; called "type" in tools.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkType {
    Bug,
    Feature,
    Chore,
    Idea,
    Epic,
}

impl WorkType {
    pub const ALL: [WorkType; 5] = [
        WorkType::Bug,
        WorkType::Feature,
        WorkType::Chore,
        WorkType::Idea,
        WorkType::Epic,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            WorkType::Bug => "bug",
            WorkType::Feature => "feature",
            WorkType::Chore => "chore",
            WorkType::Idea => "idea",
            WorkType::Epic => "epic",
        }
    }

    /// Case-insensitive; spaces and hyphens count as underscores.
    pub fn parse_loose(s: &str) -> Result<Self, String> {
        let wanted = normalize_loose(s);
        Self::ALL
            .into_iter()
            .find(|t| t.as_str() == wanted)
            .ok_or_else(|| {
                let valid: Vec<&str> = Self::ALL.iter().map(|t| t.as_str()).collect();
                format!("unknown type '{s}'. Use one of: {}.", valid.join(", "))
            })
    }
}

impl fmt::Display for WorkType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for WorkType {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse_loose(s)
    }
}

/// Where a piece of work is in its life. Independent of [`Status`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    Idea,
    Backlog,
    Brainstormed,
    Understood,
    Planned,
    InProgress,
    InReview,
    Done,
}

impl Stage {
    pub const ALL: [Stage; 8] = [
        Stage::Idea,
        Stage::Backlog,
        Stage::Brainstormed,
        Stage::Understood,
        Stage::Planned,
        Stage::InProgress,
        Stage::InReview,
        Stage::Done,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Stage::Idea => "idea",
            Stage::Backlog => "backlog",
            Stage::Brainstormed => "brainstormed",
            Stage::Understood => "understood",
            Stage::Planned => "planned",
            Stage::InProgress => "in_progress",
            Stage::InReview => "in_review",
            Stage::Done => "done",
        }
    }

    /// Case-insensitive; spaces and hyphens count as underscores.
    pub fn parse_loose(s: &str) -> Result<Self, String> {
        let wanted = normalize_loose(s);
        Self::ALL
            .into_iter()
            .find(|t| t.as_str() == wanted)
            .ok_or_else(|| {
                let valid: Vec<&str> = Self::ALL.iter().map(|t| t.as_str()).collect();
                format!("unknown stage '{s}'. Use one of: {}.", valid.join(", "))
            })
    }
}

impl fmt::Display for Stage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Stage {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse_loose(s)
    }
}

/// A node row without its notes body (listings never load bodies).
#[derive(Debug, Clone)]
pub struct Node {
    pub id: NodeId,
    pub parent_id: Option<NodeId>,
    pub position: String,
    pub kind: Kind,
    pub ordered: bool,
    pub title: String,
    pub status: Status,
    pub priority: Option<i64>,
    /// Place on the global focus list (fractional key); `None` = not on it.
    pub focus_rank: Option<String>,
    pub due_at: Option<String>,
    pub defer_until: Option<String>,
    pub waiting_on: Option<String>,
    pub claimed_by: Option<String>,
    pub claim_expires: Option<String>,
    /// Set by `answer`; cleared when the task is claimed, asked again or closed.
    pub answered_at: Option<String>,
    /// Repeat rule as stored JSON (see [`crate::recur::Rule`]); `None` = one-off.
    pub recur: Option<String>,
    /// What sort of work it is; own value only, not inherited.
    pub work_type: Option<WorkType>,
    /// Where the work is; own value only, separate from status.
    pub stage: Option<Stage>,
    pub created_at: String,
    pub updated_at: String,
    /// Last activity on the node or anything beneath it (bubbles up).
    pub touched_at: String,
    pub has_notes: bool,
    /// A step of a written plan: the parent is a task with ordered children.
    pub is_step: bool,
    /// Direct children that are not dropped.
    pub child_total: i64,
    /// Direct children that are done.
    pub child_done: i64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_id_forms() {
        assert_eq!("SPOKE-12".parse::<NodeId>(), Ok(NodeId(12)));
        assert_eq!("spoke-12".parse::<NodeId>(), Ok(NodeId(12)));
        assert_eq!(" 7 ".parse::<NodeId>(), Ok(NodeId(7)));
        assert_eq!("#3".parse::<NodeId>(), Ok(NodeId(3)));
        assert!("SPOKE-".parse::<NodeId>().is_err());
        assert!("0".parse::<NodeId>().is_err());
        assert!("abc".parse::<NodeId>().is_err());
        assert_eq!(IdArg::Number(5).parse(), Ok(NodeId(5)));
        assert!(IdArg::Number(-1).parse().is_err());
        assert_eq!(NumArg::Text(" 5 ".into()).parse("depth"), Ok(5));
        assert!(NumArg::Text("five".into()).parse("depth").is_err());
    }

    #[test]
    fn parses_work_type_and_stage_loosely() {
        assert_eq!(Stage::parse_loose("In review"), Ok(Stage::InReview));
        assert_eq!(Stage::parse_loose("in-review"), Ok(Stage::InReview));
        assert_eq!(Stage::parse_loose("IN_REVIEW"), Ok(Stage::InReview));
        assert_eq!(WorkType::parse_loose("Bug"), Ok(WorkType::Bug));
        assert_eq!("feature".parse::<WorkType>(), Ok(WorkType::Feature));
        assert_eq!(
            WorkType::parse_loose("bugg"),
            Err("unknown type 'bugg'. Use one of: bug, feature, chore, idea, epic.".into())
        );
        assert_eq!(
            Stage::parse_loose("reviewing"),
            Err("unknown stage 'reviewing'. Use one of: idea, backlog, brainstormed, understood, planned, in_progress, in_review, done.".into())
        );
        assert_eq!(Stage::InProgress.to_string(), "in_progress");
        assert_eq!(WorkType::Epic.as_str(), "epic");
    }

    #[test]
    fn displays_id() {
        assert_eq!(NodeId(142).to_string(), "SPOKE-142");
    }
}
