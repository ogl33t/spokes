//! The MCP surface: tool definitions, descriptions and server instructions.

pub mod args;

use std::sync::{Arc, Mutex};

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{Implementation, ServerCapabilities, ServerConfig};
use rmcp::{ServerHandler, tool, tool_handler, tool_router};
use rusqlite::Connection;

use crate::format::today_in;
use crate::store::{self, ToolResult};
use args::{
    AddArgs, AnswerArgs, AskArgs, ClaimArgs, FocusArgs, GetArgs, MoveArgs, NextArgs, RecentArgs,
    ReleaseArgs, SearchArgs, SetStatusArgs, UpdateArgs,
};

/// The one SQLite connection, shared by every request.
pub type Db = Arc<Mutex<Connection>>;

/// Conventions every model gets on connect. Keep it to a few lines.
pub const INSTRUCTIONS: &str = "\
Spokes is one tree of to-dos. IDs look like SPOKE-12 (a bare 12 also works). The root is SPOKE-1 \"To do\".
Areas (kind area: Home, Work) hold work forever and never finish; tasks get done or dropped. Nothing is deleted.
An ordered parent's children are steps: only the first unfinished one is actionable (marked <- next).
- \"What should I do?\" -> next (scope narrows it to one subtree).
- To find something (\"my server\"), search first, with several keywords at once: names, synonyms, likely spellings. Results show each node's path; ask the user only if it is truly ambiguous.
- Search before adding, so you don't create duplicates and the node lands under the right parent.
- Write a whole plan in one add with nested children. Give every new node 2 to 5 aliases (synonyms, plain spelling, what you'd call it out loud). Long detail goes in notes, not the title.
- Priority: level 1-4 (inherits downward) for \"this matters\"; focus for the short ranked top list.
- +notes means a body exists; read it with get notes: true. Record progress with update notes_append.
- Before working on a task, claim it so other agents skip it; finish with set_status done, or release it with a note. Pass the same agent label (e.g. opus-red) on every call this session; it renews your claim. Don't mention it to the user.
- Orchestrating: claim the project, write the plan as children, hand step ids to workers; each worker claims its own step with its own agent label.
- Dates are YYYY-MM-DD in the user's time zone. defer_until hides a node until then. Repeating chores and bills: add with recur (\"every 2 weeks\"); set_status done brings it back with the next due date.
- Ask rather than guess: if you need the user, call ask with the exact question and stop work on that task. When the user replies, record it with answer.";

/// One MCP server instance. Built per request (the transport is stateless), so it only
/// holds the shared database handle and a clone of the prebuilt tool router.
#[derive(Clone)]
pub struct Spokes {
    db: Db,
    tool_router: ToolRouter<Self>,
    /// `SPOKES_TZ`: decides what "today" is for due dates and deferrals.
    tz: chrono_tz::Tz,
}

#[tool_router]
impl Spokes {
    /// The tool router, built once at startup and cloned into each request's server.
    pub fn router() -> ToolRouter<Self> {
        Self::tool_router()
    }

    pub fn new(db: Db, tool_router: ToolRouter<Self>, tz: chrono_tz::Tz) -> Self {
        Self {
            db,
            tool_router,
            tz,
        }
    }

    /// Runs a store operation on the blocking pool so SQLite never stalls the runtime.
    async fn run<F>(&self, op: F) -> ToolResult
    where
        F: FnOnce(&mut Connection) -> ToolResult + Send + 'static,
    {
        let db = self.db.clone();
        tokio::task::spawn_blocking(move || {
            // A panic mid-call rolls its transaction back, so a poisoned lock is safe to reuse.
            let mut conn = db.lock().unwrap_or_else(|p| p.into_inner());
            op(&mut conn)
        })
        .await
        .unwrap_or_else(|e| {
            tracing::error!(error = %e, "tool task failed");
            Err("Internal error. Try again; if it persists, check the server log.".to_string())
        })
    }

    #[tool(
        description = "What to work on next, one line per work item: what needs the user, the focus list, what is due or overdue, then open work by priority level, and a few stale items. Use scope to look under one node, type or stage to filter."
    )]
    async fn next(&self, Parameters(args): Parameters<NextArgs>) -> ToolResult {
        let today = today_in(self.tz);
        self.run(move |c| store::next(c, args, today)).await
    }

    #[tool(
        description = "Find nodes by keywords. Pass several at once as alternatives (\"server homelab nas proxmox\"): names, synonyms, likely spellings. Tolerates typos. Each result shows its path. Search before adding. type and stage filter; with an empty query they list every match."
    )]
    async fn search(&self, Parameters(args): Parameters<SearchArgs>) -> ToolResult {
        let today = today_in(self.tz);
        self.run(move |c| store::search(c, args, today)).await
    }

    #[tool(
        description = "The most recently added nodes, newest first, by creation time, each with id, title, path and when it was added. Use for \"the last thing I added\" or \"start the spoke I just added\", then claim or get the id."
    )]
    async fn recent(&self, Parameters(args): Parameters<RecentArgs>) -> ToolResult {
        let tz = self.tz;
        self.run(move |c| store::recent(c, args, tz)).await
    }

    #[tool(
        description = "Put a node (any depth) on the short global ranked focus list, move it (rank 1 = top, or before/after another entry), or take it off (remove: true). Keep the list to about 10."
    )]
    async fn focus(&self, Parameters(args): Parameters<FocusArgs>) -> ToolResult {
        self.run(move |c| store::focus(c, args)).await
    }

    #[tool(
        description = "Show one node, its path, who holds it, and an outline of its children (depth levels, default 1). Ordered steps are numbered and the actionable one is marked <- next. Notes only with notes: true; recent history with history: true."
    )]
    async fn get(&self, Parameters(args): Parameters<GetArgs>) -> ToolResult {
        let tz = self.tz;
        self.run(move |c| store::get(c, args, tz)).await
    }

    #[tool(
        description = "Create a task or area under a parent (default: the root), or a whole plan in one call via nested children (set ordered: true on the parent for sequential steps). Returns the new ids. Include 2 to 5 aliases. Optional type (bug, feature, chore, idea, epic) and stage (idea, backlog, brainstormed, understood, planned, in_progress, in_review, done) per node."
    )]
    async fn add(&self, Parameters(args): Parameters<AddArgs>) -> ToolResult {
        self.run(move |c| store::add(c, args)).await
    }

    #[tool(
        description = "Change a node's fields (title, kind, ordered, priority, type, stage, due, defer_until, aliases), replace its notes, or append to them with notes_append."
    )]
    async fn update(&self, Parameters(args): Parameters<UpdateArgs>) -> ToolResult {
        self.run(move |c| store::update(c, args)).await
    }

    #[tool(
        description = "Mark nodes done, dropped (won't do; there is no delete) or open (reopen). Areas can be dropped but never done. A repeating task marked done comes back with its next due date; dropping it stops it."
    )]
    async fn set_status(&self, Parameters(args): Parameters<SetStatusArgs>) -> ToolResult {
        let today = today_in(self.tz);
        self.run(move |c| store::set_status(c, args, today)).await
    }

    #[tool(
        name = "move",
        description = "Reparent a node (parent) and/or reorder it among its siblings (before or after a sibling id). Its id and children come with it."
    )]
    async fn move_node(&self, Parameters(args): Parameters<MoveArgs>) -> ToolResult {
        self.run(move |c| store::move_node(c, args)).await
    }

    #[tool(
        description = "Take a task before working on it: it leaves next for everyone else until you finish (set_status done), release or ask. The lease (default 60 min) renews with every write you make on it. Refused if another agent holds it."
    )]
    async fn claim(&self, Parameters(args): Parameters<ClaimArgs>) -> ToolResult {
        let tz = self.tz;
        self.run(move |c| store::claim(c, args, tz)).await
    }

    #[tool(
        description = "Give a claimed task back without finishing it. It returns to the queue; the note (where you got to, why) is added to its notes."
    )]
    async fn release(&self, Parameters(args): Parameters<ReleaseArgs>) -> ToolResult {
        let tz = self.tz;
        self.run(move |c| store::release(c, args, tz)).await
    }

    #[tool(
        description = "Hand a question to the user instead of guessing. The task waits at the top of next under NEEDS YOU until answered; stop work on it."
    )]
    async fn ask(&self, Parameters(args): Parameters<AskArgs>) -> ToolResult {
        let tz = self.tz;
        self.run(move |c| store::ask(c, args, tz)).await
    }

    #[tool(
        description = "Record the user's reply to a waiting task's question. The Q&A is added to its notes and the task is open again for an agent to claim."
    )]
    async fn answer(&self, Parameters(args): Parameters<AnswerArgs>) -> ToolResult {
        let tz = self.tz;
        self.run(move |c| store::answer(c, args, tz)).await
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for Spokes {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("spokes", env!("CARGO_PKG_VERSION")))
            .with_instructions(INSTRUCTIONS)
    }
}
