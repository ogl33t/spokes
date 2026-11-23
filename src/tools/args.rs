//! Tool arguments. Doc comments become the JSON Schema descriptions models read,
//! so they stay short. IDs are declared as strings but bare numbers are accepted too;
//! numbers are declared as integers but numeric strings ("5") are accepted too.

use schemars::JsonSchema;
use serde::Deserialize;

use crate::model::{IdArg, Kind, NumArg, SettableStatus};
use crate::recur::RecurArg;

/// One node to create, with optional nested children (a whole plan in one call).
#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
pub struct NodeSpec {
    /// Short title.
    pub title: String,
    /// "task" (can be finished, the default) or "area" (a permanent bucket like Home or Work that never finishes).
    pub kind: Option<Kind>,
    /// true if this node's children are steps to do in order (only the first unfinished one is actionable).
    pub ordered: Option<bool>,
    /// 1 (highest) to 4. Omit to inherit.
    #[schemars(with = "Option<i64>")]
    pub priority: Option<NumArg>,
    /// Deadline as YYYY-MM-DD.
    pub due: Option<String>,
    /// Hide from the queue until this date, YYYY-MM-DD.
    pub defer_until: Option<String>,
    /// 2 to 5 other phrases this could be searched by: synonyms, plain spelling, what you'd call it out loud.
    pub aliases: Option<Vec<String>>,
    /// Repeat rule for a task, e.g. {"every": 2, "unit": "week", "from": "completion"} or "every 2 weeks". from "schedule" counts from the due date (bills). Marking it done resets it with the next due date.
    pub recur: Option<RecurArg>,
    /// What sort of work: "bug", "feature", "chore", "idea" or "epic".
    #[serde(rename = "type")]
    pub work_type: Option<String>,
    /// Where it is: "idea", "backlog", "brainstormed", "understood", "planned", "in_progress", "in_review" or "done". Separate from status.
    pub stage: Option<String>,
    /// Longer body: plan, research, links. Never shown in listings.
    pub notes: Option<String>,
    /// Nodes to create beneath this one, in order. Each takes the same fields (title, kind, ordered, notes, children...).
    pub children: Option<Vec<NodeSpec>>,
}

// The `agent` label is optional on plain writes (it logs who acted and renews that agent's
// claim) and says "no need to mention it": in the M0 usage test a model kept telling the
// user which name it had logged under.

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AddArgs {
    /// Parent id, e.g. "SPOKE-1". Omit to add under the root ("To do").
    #[schemars(with = "Option<String>")]
    pub parent: Option<IdArg>,
    #[serde(flatten)]
    pub node: NodeSpec,
    /// Your agent label, the same all session (e.g. "opus-red"). Optional; it logs who acted and renews your claim. No need to mention it to the user.
    pub agent: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetArgs {
    /// Node id, e.g. "SPOKE-12". Omit for the root.
    #[schemars(with = "Option<String>")]
    pub id: Option<IdArg>,
    /// Outline levels to show: 1 (default) = direct children, 2 = grandchildren too, up to 10.
    #[schemars(with = "Option<u32>")]
    pub depth: Option<NumArg>,
    /// Include the notes body (default false).
    pub notes: Option<bool>,
    /// Include the node's last 10 history events: who claimed, asked, answered, changed it (default false).
    pub history: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct NextArgs {
    /// Only look under this node, e.g. "SPOKE-3". Omit for everything.
    #[schemars(with = "Option<String>")]
    pub scope: Option<IdArg>,
    /// Max lines (default 5), not counting up to 3 stale nudges.
    #[schemars(with = "Option<u32>")]
    pub limit: Option<NumArg>,
    /// Only nodes of this type (bug, feature, chore, idea, epic).
    #[serde(rename = "type")]
    pub work_type: Option<String>,
    /// Only nodes at this stage (e.g. in_review).
    pub stage: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SetStatusArgs {
    /// Node ids, e.g. ["SPOKE-12", "SPOKE-13"].
    #[schemars(with = "Vec<String>")]
    pub ids: Vec<IdArg>,
    /// "done" (finished), "dropped" (won't do; replaces delete) or "open" (reopen).
    pub status: SettableStatus,
    /// Your agent label, the same all session (e.g. "opus-red"). Optional; it logs who acted and renews your claim. No need to mention it to the user.
    pub agent: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct UpdateArgs {
    /// Node id, e.g. "SPOKE-12".
    #[schemars(with = "String")]
    pub id: IdArg,
    /// New title.
    pub title: Option<String>,
    /// "task" or "area".
    pub kind: Option<Kind>,
    /// true if the children are steps in order.
    pub ordered: Option<bool>,
    /// 1 (highest) to 4; 0 clears it so it inherits again.
    #[schemars(with = "Option<i64>")]
    pub priority: Option<NumArg>,
    /// Deadline YYYY-MM-DD; "" clears it.
    pub due: Option<String>,
    /// Hide until YYYY-MM-DD; "" clears it.
    pub defer_until: Option<String>,
    /// Replaces all aliases; [] clears them.
    pub aliases: Option<Vec<String>>,
    /// Repeat rule, e.g. {"every": 1, "unit": "month", "from": "schedule"} or "every 2 weeks"; "" stops it repeating.
    pub recur: Option<RecurArg>,
    /// What sort of work: "bug", "feature", "chore", "idea" or "epic". "" clears it.
    #[serde(rename = "type")]
    pub work_type: Option<String>,
    /// Where it is: "idea", "backlog", "brainstormed", "understood", "planned", "in_progress", "in_review" or "done". Separate from status. "" clears it.
    pub stage: Option<String>,
    /// Replaces the whole notes body; "" clears it. To add to it, use notes_append instead.
    pub notes: Option<String>,
    /// Text appended to the end of the notes (e.g. what you did, findings).
    pub notes_append: Option<String>,
    /// Your agent label, the same all session (e.g. "opus-red"). Optional; it logs who acted and renews your claim. No need to mention it to the user.
    pub agent: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct MoveArgs {
    /// The node to move, e.g. "SPOKE-12".
    #[schemars(with = "String")]
    pub id: IdArg,
    /// New parent. Omit to stay under the current parent (or the parent of before/after).
    #[schemars(with = "Option<String>")]
    pub parent: Option<IdArg>,
    /// Place just before this sibling.
    #[schemars(with = "Option<String>")]
    pub before: Option<IdArg>,
    /// Place just after this sibling. Without before or after the node goes last.
    #[schemars(with = "Option<String>")]
    pub after: Option<IdArg>,
    /// Your agent label, the same all session (e.g. "opus-red"). Optional; it logs who acted and renews your claim. No need to mention it to the user.
    pub agent: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SearchArgs {
    /// Several keywords at once, matched as alternatives: names, synonyms, likely misspellings, e.g. "server homelab nas proxmox".
    pub query: String,
    /// Only search under this node, e.g. "SPOKE-3". Omit for everything.
    #[schemars(with = "Option<String>")]
    pub scope: Option<IdArg>,
    /// Also find done and dropped nodes (default false).
    pub include_done: Option<bool>,
    /// Max results (default 10).
    #[schemars(with = "Option<u32>")]
    pub limit: Option<NumArg>,
    /// Only nodes of this type (bug, feature, chore, idea, epic).
    #[serde(rename = "type")]
    pub work_type: Option<String>,
    /// Only nodes at this stage (e.g. in_review).
    pub stage: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RecentArgs {
    /// Only look under this node, e.g. "SPOKE-3". Omit for everything.
    #[schemars(with = "Option<String>")]
    pub scope: Option<IdArg>,
    /// Also list done and dropped nodes (default false).
    pub include_done: Option<bool>,
    /// Max results (default 10, up to 50).
    #[schemars(with = "Option<u32>")]
    pub limit: Option<NumArg>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct FocusArgs {
    /// The node to place on (or take off) the focus list, at any depth, e.g. "SPOKE-12".
    #[schemars(with = "String")]
    pub id: IdArg,
    /// Place it at this position, 1 = top. Without rank, before or after it goes last.
    #[schemars(with = "Option<u32>")]
    pub rank: Option<NumArg>,
    /// Place it just above this focus-list entry.
    #[schemars(with = "Option<String>")]
    pub before: Option<IdArg>,
    /// Place it just below this focus-list entry.
    #[schemars(with = "Option<String>")]
    pub after: Option<IdArg>,
    /// true takes it off the focus list (its priority level stays).
    pub remove: Option<bool>,
    /// Your agent label, the same all session (e.g. "opus-red"). Optional; it logs who acted and renews your claim. No need to mention it to the user.
    pub agent: Option<String>,
}

// Claims, questions and answers. Here `agent` is required: it is who holds the task.

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ClaimArgs {
    /// The task to take, e.g. "SPOKE-12".
    #[schemars(with = "String")]
    pub id: IdArg,
    /// Your agent label, the same one for the whole session (e.g. "opus-red").
    pub agent: String,
    /// How long the claim lasts without activity (default 60, max 1440). Any write you make on the task or beneath it renews it.
    #[schemars(with = "Option<u32>")]
    pub lease_minutes: Option<NumArg>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ReleaseArgs {
    /// The task to give back, e.g. "SPOKE-12".
    #[schemars(with = "String")]
    pub id: IdArg,
    /// Your agent label (the one you claimed with).
    pub agent: String,
    /// Why, and where you got to. Appended to the task's notes for the next agent.
    pub note: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AskArgs {
    /// The task the question is about, e.g. "SPOKE-12".
    #[schemars(with = "String")]
    pub id: IdArg,
    /// Your agent label.
    pub agent: String,
    /// The question for the user, short and specific (e.g. "Install to D: or E:?").
    pub question: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AnswerArgs {
    /// The waiting task, e.g. "SPOKE-12".
    #[schemars(with = "String")]
    pub id: IdArg,
    /// The user's reply, in their words. Appended to the task's notes with the question.
    pub text: String,
}
