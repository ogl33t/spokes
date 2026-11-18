# Spokes Todo MCP

[![ci](https://github.com/ogl33t/spokes/actions/workflows/ci.yml/badge.svg)](https://github.com/ogl33t/spokes/actions/workflows/ci.yml) [![codeql](https://github.com/ogl33t/spokes/actions/workflows/codeql.yml/badge.svg)](https://github.com/ogl33t/spokes/actions/workflows/codeql.yml)

One small self-hosted server that holds one tree of to-dos, which any MCP-capable model can read, plan into and work from. A fresh chat can ask "what should I do next?" and get a short, ranked answer. Parallel agents can each take a task without colliding.

The full product spec is [docs/spec.md](docs/spec.md).

## End goal

The finished v1 is for one person (Brandon) and the models and agents working for him: Claude Code, OpenCode and open models on the home network.

- **One node type in one tree.** Home, Work, a project and a step in a deploy pipeline are all the same record with a parent. Depth is unlimited.
- **Areas never finish; tasks do.** Areas (Home, House maintenance) hold work forever. Tasks get done or dropped. Nothing is deleted; an append-only event log records who did what.
- **Ordered or unordered children.** Ordered parents release one step at a time.
- **Stable short IDs** like `SPOKE-142`, global and never reused.
- **Two-tier reads.** Listings are one compact plain-text line per node. Long notes come back only when asked for.
- **A ranked queue (`next`)**: what needs you (questions, stalled claims), the focus list, what is due, priority levels, and a few stale items.
- **Search** that copes with half-remembered names: aliases written at creation, normalization, trigram matching.
- **Claims for parallel agents.** A claimed task leaves the queue; `ask` brings it back to the top as needing you.
- **Time:** due dates, `defer_until`, recurring tasks reset in place, staleness nudges, nightly backup of the database file.
- **Thirteen tools:** `next`, `get`, `search`, `recent`, `add`, `update`, `set_status`, `move`, `focus`, `claim`, `release`, `ask`, `answer`.

## Phases

| Phase | Scope | Status |
| --- | --- | --- |
| **M0, walking skeleton** | `add`, `get`, `next`, `set_status` on SQLite (WAL), stateless Streamable HTTP on `/mcp`, Docker image. Done when a fresh chat can add three areas and list them | **Done** |
| **M1, the tree** | Ordered/unordered children, `move`, `update`, notes apart from listings, nested `add`, breadcrumb paths, the full compact output format. Done when a model can write and walk a six-step plan in three calls | **Done** |
| **M2, finding and ranking** | `search` (aliases, normalization, trigram FTS5), priority inheritance, `focus`, due dates with `SPOKES_TZ`, the full `next` ordering. Done when "priority tasks for my server" finds the right project from a blank context | **Done** |
| **M3, agents** | `claim`, `release`, `ask`, `answer`, leases, event log surfaced. Done when two terminals asking "what's next" get different tasks | **Done** |
| **M4, time and reach** | Recurrence (reset in place), `defer_until` everywhere, staleness, nightly in-process backup, `spokes backup` / `spokes restore`. Done when a restore from backup works | **Done** |
| After v1 | Auth and multiple users (OAuth, `user_id` already on every row), links (blocks/related), notifications, read-only web view, semantic search | In progress (**current**): the read-only web view's API half is done (`/api`, see [Read-only JSON API](#read-only-json-api)); the viewer itself is the companion repo `spokes-view`. The rest is not started |

### What works today (M0 to M4, plus the read-only API)

All thirteen tools: `next`, `get`, `search`, `recent`, `add`, `update`, `set_status`, `move`, `focus`, `claim`, `release`, `ask`, `answer`. Plus a read-only JSON API on `/api` (`GET /api/tree`, `GET /api/nodes/{id}`) for the web view; see [Read-only JSON API](#read-only-json-api).

- `add`: one node, or a whole plan in one call with nested `children` (any depth up to 8, at most 200 nodes, one transaction). Returns one line with every new id, e.g. `Added SPOKE-5 task  Deploy  (under SPOKE-4 Skyforge), with 6 beneath it in the order given: SPOKE-6..SPOKE-11`. A bad child names itself (`children[3] ("Push"): ...`) and nothing is written.
- `get`: the node's header line, its breadcrumb (`Hobbies / Programming / Skyforge`), who holds it (`claimed by opus-red until 14:20`, `stalled: claim by ... expired ...`, `waiting on the user: "..." (asked by sonnet-2)`, `answered 14:02, ready to claim`) and an outline `depth` levels deep (default 1, max 10), indented per level. Ordered children are numbered and the actionable step is marked `<- next`; unordered children are bulleted. The outline is capped at 60 lines and ends with `(+N more ...)` lines for what was cut by the cap or lies below the depth, plus a count of hidden dropped nodes. A child whose due date is later than a parent's shows `(after parent's Nov 20)`. `notes: true` appends the body; `history: true` appends the node's last 10 events, newest first (`14:02  claimed by opus-red: until ...`).
- Levels: `priority` 1 to 4 on any node; unset nodes inherit the nearest ancestor's level. `get` shows a node's own level; `next` and `search` show the effective one.
- `update`: title, kind, ordered, priority (`0` clears), due / defer_until (`""` clears), aliases (`[]` clears), recur (`""` stops it), `notes` (replace; `""` clears) or `notes_append` (adds a paragraph). Kind changes keep the rule that areas only live under areas and never finish.
- `move`: reparent (`parent`) and/or reorder (`before` / `after` a sibling; without either it goes last). Writes one row via fractional positions. Refuses moving the root, cycles, areas under tasks and closed parents.
- `next`: the queue, in sections, one line per work item, each node and each actionable step at most once:
  `NEEDS YOU` (questions with agent and age, then stalled claims whose lease ran out, then answered tasks waiting for an agent), `FOCUS` (the ranked list, numbered by global rank), `DUE SOON` (overdue first, then due within 7 days), `UP NEXT` (priority level 1 to 4, then unset, oldest first) and `STALE` (up to 3 work items untouched for 30+ days, a nudge outside `limit`). Work items are top-level tasks (directly under areas), deeper tasks that carry their own level or due date, and anything on the focus list; later steps of an ordered parent are held back. A due or up-next line whose `next:` step (or, for a leaf, the node itself) is already shown higher up is dropped, so a project and its due sub-project never both point at the same step; focus entries are always kept. A project whose open steps are all claimed or waiting is not offered. Lines show the effective level (own, or inherited from the nearest ancestor), `due Nov 20` / `due today` / `overdue Nov 10`, progress and `next: SPOKE-7 ...` (the next actionable step). Columns are aligned as in the spec sample. `scope` limits it to a subtree; `limit` (default 5) caps the first four sections, ending with `(+N more open. Pass scope or limit.)`. Warns when the focus list passes 10 entries.
- `search`: several keywords in one `query`, matched as alternatives (`server homelab nas proxmox`). Text is normalized (lowercase, accents folded, punctuation stripped, plus a squashed no-space form, so `home-lab` / `home lab` / `homelab` match) and indexed in an FTS5 trigram table. Typos still hit (edit distance 1-2 or shared trigrams); keywords under 3 letters (`pc`, `tv`) match whole words only; filler words (`for`, `my`, `tasks`) are ignored, but a whole multi-word title or alias found in the query (`my server`) earns a bonus. Ranked title > aliases > notes, ties by level. Each line has flags, the effective level, `focus #N`, the breadcrumb (`in Hobbies / Homelab`) and, when the title did not match, why it did (`(alias "nas")`, `(in notes)`). Default 10 results (`limit` up to 50), `scope`, `include_done` (done/dropped nodes and everything under them are hidden by default, with a count).
- `recent`: the most recently added nodes, newest first by creation time, one line each with id, kind, title, path and when it was added (`added 3h ago, 16:03`). `limit` (default 10, max 50), `scope`, `include_done`. For "start the spoke I just added".
- `focus`: put a node at any depth (task or area) on the global ranked list: append, `rank` (1 = top, clamps to the end), or `before` / `after` another entry; `remove: true` takes it off. Fractional `focus_rank` keys, one row per write. Closing a node (`done`/`dropped`) takes it off the list. Warns past 10 entries.
- `set_status`: `done`, `dropped` or `open` on several ids at once. Areas cannot be done. After closing a step it names the next step of an ordered parent, or, when it was the last open step, suggests closing the parent (it never closes it itself).
- Notes live in their own table and never appear in listings; `+notes` says a body exists.
- `unplanned` marks a task with no children and no notes, except steps of an ordered task (the parent is their plan).
- IDs are shown as `SPOKE-<n>`; `SPOKE-12`, `12` and `"12"` are all accepted, and numeric arguments also accept numeric strings (`"depth": "3"`).
- Claims (`claim`, `release`, lease renewal):
  - `claim(id, agent, lease_minutes=60)` sets `in_progress` with a lease (1 to 1440 minutes) in one conditional `UPDATE` (an `IMMEDIATE` transaction), so two agents cannot both win. The node and its whole subtree leave `next` for everyone.
  - Refused, with what to do next: areas and the root, done/dropped tasks, waiting tasks (answer first), a task held by another agent with a live lease (`SPOKE-142 is claimed by opus-red until 14:20. Pick another task, or ask the user.`, times in `SPOKES_TZ`), or one with a step beneath it claimed by another agent.
  - Orchestrator and workers: claiming a step inside another agent's claimed project is allowed (the spec's orchestrator flow: the orchestrator claims the project and writes the plan, each worker claims its own step under its own label). The reply says whose project it is part of; the step stays hidden from `next` because its ancestor is claimed; `get` shows `claimed by <worker>` on the step and `in_progress by <worker>` in the parent's outline. Writes renew only the writer's own claims (the worker's step, the orchestrator's project). A worker's question (`ask`) inside a claimed project still shows under NEEDS YOU.
  - Claiming again as the same agent renews (`Renewed ...`); claiming a lapsed lease takes it over and logs `took over from <agent>, whose lease expired ...`.
  - Any write (`add`, `update`, `move`, `set_status`, `focus`) passing the holder's `agent` on the node or beneath it renews the lease to at least 60 minutes from now (a longer lease is kept; a lapsed claim the agent still holds is revived).
  - Expiry is evaluated when `next` or `get` is read; there is no background job. A lapsed claim shows under NEEDS YOU as `stalled  Title  (claim by opus-red expired 2h ago)`.
  - `release(id, agent, note)` puts it back to `open`; the note is appended to the notes (`Released by opus-red (2026-11-14): ...`) and logged. Only the holder can release a live claim; anyone can release a lapsed one. The asking agent can also withdraw its question this way.
  - `set_status done/dropped/open` clears any claim, question and answered mark.
- Questions (`ask`, `answer`):
  - `ask(id, agent, question)` sets `waiting` with the question in `waiting_on`; NEEDS YOU shows `SPOKE-88  waiting  Set up emulator: "Install to D: or E:?" (sonnet-2, 3h ago)` (age from the `asked` event). Refused on areas, closed tasks, another agent's live claim or another agent's open question.
  - `answer(id, text)` appends `Question from sonnet-2 (date): ...` / `Answer from the user (date): ...` to the notes (searchable), clears the question and claim, and reopens the task.
  - Design choice for "back near the top": an answered task stays under NEEDS YOU, after questions and stalled claims, as `answered  Title  (answered 5m ago; claim it and read the notes)` until an agent claims it (or it is closed or asked again). It is stored as `answered_at` (schema step 3). Claiming it says to read the notes.
- Recurrence (`recur` on `add` / `update`): `{"every": 2, "unit": "week", "from": "completion"}` (unit `day`, `week`, `month`, `year`; `from` `completion`, the default, or `schedule`), or short text: `"every 2 weeks"`, `"weekly"`, `"monthly from schedule"`. `update recur: ""` stops it repeating. Lines show `repeats`; `get` adds `repeats every 2 weeks, counted from completion`. Repeating tasks are never flagged `unplanned`.
  - `set_status done` on a repeating task resets it in place (same id, focus place kept): logs a `recurred` event, reopens it with the next due date, clears claim, question and answered mark, reopens its finished steps (a repeating checklist), and replies `SPOKE-7 done  Check oil  (repeats every 2 weeks, counted from completion: back in the queue Nov 25, due Nov 28)`. `dropped` ends it (`(it stops repeating)`).
  - Next due date: `completion` counts from today (in `SPOKES_TZ`); `schedule` counts from the previous due date (from today if it has none) and skips periods already gone by when done late, so it never comes back overdue. Months clamp (Jan 31 + 1 month = Feb 28/29); a `schedule` rule remembers its day of month (`day` in the stored JSON), so the 31st comes back on Mar 31, not Mar 28.
  - Defer window: after a reset the task is hidden until a lead time before the next due date of a quarter of the interval, at most 14 days (daily: the due day; weekly: 1 day before; every 2 weeks: 3; monthly: 7; yearly: 14). If that day has already come it shows at once.
  - Areas cannot repeat, and neither can a step of an ordered parent (a step that reset itself would hold every later step back forever): `add`, `update` (including making a parent `ordered`) and `move` refuse it and say to put `recur` on the parent instead, whose steps reopen each time it repeats.
- `defer_until` (YYYY-MM-DD, in `SPOKES_TZ`) hides a node and its subtree from `next`, from `next:` step hints, from `<- next` in `get` and from STALE until that day. In an ordered parent a deferred step still holds the later steps back. Questions (`waiting`) and stalled claims show under NEEDS YOU even when deferred. Lines show `deferred until Nov 20`.
- Staleness: STALE lists up to 3 open work items (tasks, never areas) that are not deferred and whose `touched_at` (which bubbles up from anything beneath) is 30+ days old, oldest first, as `SPOKE-67  untouched 41d  Workbench build`, outside `limit` and never repeating a line shown above. Any write beneath a project, deferring it or dropping it stops the nudge.
- `get history: true` shows every timestamp, including those inside event details (`claimed by red: until 03:01`), as wall-clock time in `SPOKES_TZ`.
- Every write logs an event (`created`, `updated`, `status`, `moved`, `focus`, `claimed`, `released`, `asked`, `answered`) with the `agent` label.

## Running locally

Needs the Rust toolchain (`~/.cargo/bin` on `PATH`).

```sh
cargo run --release
# or, explicitly:
SPOKES_BIND=127.0.0.1:8787 SPOKES_DB_PATH=data/spokes.db cargo run --release
```

The database is created at `data/spokes.db` (gitignored) with the root node "To do" (`SPOKE-1`). Stop it with Ctrl-C. Check it with `curl http://127.0.0.1:8787/health`.

### Docker

```sh
docker build -t spokes:dev .
docker run -d --name spokes -p 8787:8787 -v spokes-data:/data spokes:dev
```

The image runs as uid 65532 (distroless, no shell), stores the database in the `/data` volume, and has a `HEALTHCHECK` that runs `spokes healthcheck`. Ports and volumes are chosen by whoever deploys it (the compose file in `stacks/`), not by this repo.

### Configuration

Environment variables only:

| Variable | Default | Meaning |
| --- | --- | --- |
| `SPOKES_BIND` | `127.0.0.1:8787` (image: `0.0.0.0:8787`) | Listen address |
| `SPOKES_DB_PATH` | `data/spokes.db` (image: `/data/spokes.db`) | SQLite file; its directory is created if missing |
| `SPOKES_ALLOWED_HOSTS` | unset (any `Host`) | Comma-separated `Host` header values to accept on `/mcp` and `/api`, e.g. `spokes.lan,192.168.5.100`. `*` or unset accepts any |
| `SPOKES_ALLOWED_ORIGINS` | unset (refuse any `Origin`) | Comma-separated browser origins to accept on `/mcp` and `/api`. By default every request that carries an `Origin` header is refused (403), which blocks browser-driven attacks such as DNS rebinding. MCP clients like Claude Code and OpenCode send no `Origin` |
| `SPOKES_TZ` | `UTC` | IANA time zone name (e.g. `America/Chicago`). Decides what "today" is for due dates, `overdue` and `defer_until`. An unknown name stops startup with an error |
| `SPOKES_BACKUP_DIR` | unset (no backups) | Directory for automatic backups (created if missing), e.g. `/data/backups` inside the volume |
| `SPOKES_BACKUP_KEEP` | `14` | How many backup files to keep; older ones are deleted after each backup |
| `SPOKES_BACKUP_INTERVAL_SECS` | `86400` | **Test only.** Seconds between backups; shorten it to watch a backup happen |
| `SPOKES_LOG` | `info` | `error`, `warn`, `info`, `debug` or `trace` |

There is no auth in v1: run it only on the LAN or a private Tailscale network.

## Backup and restore

With `SPOKES_BACKUP_DIR` set, the server backs up its database once a day by itself (an in-process timer; the only scheduled job in spokes, since recurrence, deferral, leases and staleness are all evaluated when read). Each backup is a consistent snapshot taken with SQLite's `VACUUM INTO` on a separate read-only connection, so tool calls are not blocked, written as `spokes-YYYYMMDD-HHMMSS.db` (UTC) via a `.partial` file and a rename. After each one the oldest files beyond `SPOKES_BACKUP_KEEP` are deleted; only `spokes-*.db` / `spokes-*.partial` files are ever touched. On start it backs up at once unless the newest backup is younger than the interval. Results are logged (`backup written path=... bytes=... removed=...`, or `backup failed: ...`). Keep the backup directory on the same volume as the database, and copy it offsite if you want protection from disk loss.

A one-off backup, safe while the server runs:

```sh
spokes backup /path/to/dir-or-file.db
docker exec spokes /usr/local/bin/spokes backup /data/backups   # in the container
```

Restore (the server must be stopped; `spokes restore` refuses while `/health` answers on `SPOKES_BIND`):

```sh
# 1. stop the server (Ctrl-C, or: docker stop spokes)
# 2. restore; it checks the file (integrity_check, a nodes table, a schema this build knows),
#    moves the current database and its -wal/-shm aside as spokes.db.before-restore-<time>,
#    then puts the backup in place
spokes restore data/backups/spokes-20261114-080113.db
docker run --rm -v spokes-data:/data spokes:dev restore /data/backups/spokes-20261114-080113.db
# 3. start the server again
```

To undo a restore, stop the server and move the `before-restore` file(s) back.

## Deployment

This is the `ogl33t` fork of [brandonnorsworthy/spokes](https://github.com/brandonnorsworthy/spokes). The homelab runs the published image from the `kdev3/spokes/` stack in [ogl33t/stacks](https://github.com/ogl33t/stacks) (Arcane Git Sync; the stack README covers ports, UIDs and data): `ghcr.io/ogl33t/spokes:latest`, port `10050` -> `8787`, UID `10010`, `./data` bind mount for the database and backups. The server never builds; it pulls the image CI published.

## CI and releases

- `.github/workflows/ci.yml` (check name `ci`): `cargo fmt --check`, `cargo clippy --all-targets -D warnings`, `cargo test --locked`, and a non-pushing `docker build`, on every PR, push to `main` and weekly. Dependency review on PRs.
- `.github/workflows/codeql.yml`: CodeQL for Rust and for the workflows (`actions`), on PRs, `main` and weekly.
- `.github/workflows/release.yml`: on push to `main` and on `v*` tags, reruns `ci`, then builds and pushes `ghcr.io/ogl33t/spokes` (the image is named after the repo owner) (`latest` and `sha-<short>` from `main`; `X.Y.Z` and `sha-<short>` from a tag) and scans it with Trivy (HIGH/CRITICAL fail the job).
- `.github/dependabot.yml`: weekly cargo, GitHub Actions and Docker base image updates. Actions are pinned to commit SHAs.

Release process: the version in `Cargo.toml` (it is what `initialize` reports as `serverInfo.version`) is bumped by the git hooks, not by hand (see Development). Push to `main`, wait for green, then tag that commit `vX.Y.Z` with the version it carries and push the tag. Deploy by bumping the pinned tag in `stacks/spokes/compose.yml`; roll back by pinning the previous tag.

## Connecting clients

The endpoint is `http://<host>:8787/mcp` (Streamable HTTP, stateless; both the 2026-07-28 protocol and older initialize-based clients work).

Claude Code:

```sh
claude mcp add --transport http spokes http://127.0.0.1:8787/mcp
```

OpenCode (`opencode.json`):

```json
{
  "mcp": {
    "spokes": { "type": "remote", "url": "http://127.0.0.1:8787/mcp", "enabled": true }
  }
}
```

Then ask: "add Home, Work and Hobbies, then tell me what's next".

## Read-only JSON API

For the web view (the companion repo `spokes-view`), whose backend calls it server to server. Two `GET` endpoints under `/api` on the same port as `/mcp`. It is strictly read-only: it reads through its own SQLite connection opened read-only (`SQLITE_OPEN_READ_ONLY` and `query_only`), so it cannot write, renew a lease or log an event, and since WAL lets it read while MCP writes, it never holds the MCP connection's lock. Each response is built inside one read transaction (a consistent snapshot) on the blocking pool, from a handful of queries whatever the size of the tree.

### Rules for every request

- **GET only.** Any other method gets `405`; an unknown path under `/api` gets `404`.
- **Origin and Host, as on `/mcp`.** A request that carries an `Origin` header is refused with `403` unless the origin is in `SPOKES_ALLOWED_ORIGINS` (same matching as `/mcp`: scheme and host must match; an entry with `:*` or with no port matches any port, an explicit port only that port). With `SPOKES_ALLOWED_HOSTS` set, the `Host` header must match one entry (an entry without a port matches any port), else `403`. No CORS headers are sent: the API is meant for a backend, not for a browser calling it directly.
- **Errors** are JSON, `{"error": "..."}`, with `400` (bad parameter: unknown or repeated name, bad id, depth out of range, a boolean that is not `true`/`false`/`1`/`0`), `403`, `404` (unknown node or endpoint), `405` or `500`.
- **Caching.** Every `200` carries a weak `ETag` (`W/"..."`) and `Cache-Control: no-cache`. Send it back as `If-None-Match` (a single tag, a list, or `*`) and an unchanged answer is `304` with no body, which costs two small queries. The tag covers the query, the revision (any write), SQLite's change counter for the API's connection (any commit, even one made outside spokes), "today" in `SPOKES_TZ` (dates and deferrals roll over at midnight) and the number of lapsed claims (a lease running out turns a node `stalled` with no write), plus a per-start id, so a restart or a restore never answers `304` from an old tag. Treat the tag as opaque.
- **Compression.** With `Accept-Encoding: gzip` the body is gzipped (a 2000-node slice is about 1.1 MB of JSON and 15 KB gzipped, since nodes repeat the same keys).

### `GET /api/tree`

A slice of the tree, for lazy loading: load a few levels, then fetch a branch when the user opens it.

| Parameter | Default | Meaning |
| --- | --- | --- |
| `root` | `1` | The node to start from: `SPOKE-12` or `12` |
| `depth` | `3` | Levels below `root` to return, `0` (the root alone) to `10` |
| `include_closed` | `false` | `true` also returns done and dropped nodes and everything under them. When `false` they and their whole subtrees are left out. The `root` itself is always returned, even when it is closed |

The response holds at most **2000 nodes** (`node_limit`). Nodes are expanded breadth-first, so the shallow levels fill first, and a node's children are sent all together or not at all. At the first node whose children do not fit, expansion stops and `truncated` is `true`; that node and every node not expanded have `children_loaded: false`, so fetch them with `root=<id>`. The one exception: when the root alone has more visible children than fit, its first 1999 are sent (in order) with `children_loaded: false` on the root; this version has no paging past that.

Top-level fields:

| Field | Meaning |
| --- | --- |
| `rev` | Revision: the largest event id. Every write logs an event, so it changes whenever anything is written. Compare for equality only |
| `version` | The server version (`Cargo.toml`) |
| `time_zone` | `SPOKES_TZ`, e.g. `America/Chicago` |
| `today` | Today in that zone, `YYYY-MM-DD` (what `deferred` and overdue are judged against) |
| `root`, `depth`, `include_closed` | The query as applied |
| `node_limit` | The cap, `2000` |
| `path` | The root's ancestors, top first, each `{"id", "title"}`, starting with `{"id": 1, "title": "To do"}`. Empty for the root node |
| `truncated` | `true` when the cap stopped expansion |
| `nodes` | The nodes in pre-order (a node, then its children in their stored position order, each followed by its own subtree). Rebuild the tree from `parent` |

Each node (every field is always present; `null` when unset):

| Field | Type | Meaning |
| --- | --- | --- |
| `id` | number | `12` for `SPOKE-12` |
| `parent` | number or null | Parent id; `null` only for the root node `1` |
| `kind` | string | `area` or `task` |
| `ordered` | bool | Its children are sequential steps |
| `title` | string | |
| `status` | string | `open`, `in_progress`, `waiting`, `done`, `dropped` |
| `priority` | 1-4 or null | Its own level |
| `effective_priority` | 1-4 or null | Its own level, or the nearest ancestor's (as `next` and `search` show it) |
| `focus` | number or null | 1-based rank on the focus list |
| `due`, `defer_until` | `YYYY-MM-DD` or null | As stored |
| `deferred` | bool | `defer_until` is after `today` |
| `claimed_by`, `claim_expires` | string or null | The claiming agent and the lease end (RFC 3339 UTC) |
| `stalled` | bool | `in_progress` and the lease has run out |
| `waiting_on` | string or null | The agent's open question (`status` is `waiting`) |
| `answered` | bool | The user answered a question and no agent has claimed it since |
| `repeat` | string or null | The repeat rule in words, e.g. `every 2 weeks, counted from completion` |
| `has_notes` | bool | A notes body exists (bodies are never in `/api/tree`) |
| `unplanned` | bool | An open task with no children and no notes, as `+unplanned` in the tools |
| `is_step` | bool | Its parent is an ordered task |
| `is_next` | bool | It is the actionable step of its ordered parent (`<- next` in `get`): the first unfinished child, if that one is open and not deferred |
| `progress` | `{"done", "total"}` or null | For a task with children: done / not-dropped direct children (`(2/6)`) |
| `created_at`, `updated_at`, `touched_at` | string | RFC 3339 UTC. `touched_at` bubbles up from anything beneath |
| `child_count` | number | Direct children visible under this query (closed ones count only with `include_closed=true`) |
| `descendant_count` | number | All visible nodes beneath it, at any depth |
| `closed_child_count` | number | Direct children that are done or dropped, whether or not they are shown |
| `children_loaded` | bool | All of its visible children are in this response (`true` for a node with none). `false` means fetch `root=<id>` to see them |

Example (trimmed):

```sh
curl -s 'http://127.0.0.1:8787/api/tree?depth=2'
```

```json
{"rev":10,"version":"0.2.0","time_zone":"UTC","today":"2026-11-15","root":1,"depth":2,
 "include_closed":false,"node_limit":2000,"path":[],"truncated":false,"nodes":[
  {"id":1,"parent":null,"kind":"area","ordered":false,"title":"To do","status":"open",
   "priority":null,"effective_priority":null,"focus":null,"due":null,"defer_until":null,
   "deferred":false,"claimed_by":null,"claim_expires":null,"stalled":false,"waiting_on":null,
   "answered":false,"repeat":null,"has_notes":false,"unplanned":false,"is_step":false,
   "is_next":false,"progress":null,"created_at":"2026-11-15T19:08:55Z",
   "updated_at":"2026-11-15T19:08:55Z","touched_at":"2026-11-15T19:08:56Z",
   "child_count":1,"descendant_count":5,"closed_child_count":0,"children_loaded":true},
  {"id":2,"parent":1,"kind":"area","title":"Home","priority":2,"effective_priority":2,
   "child_count":1,"descendant_count":4,"closed_child_count":0,"children_loaded":true, "...": "..."},
  {"id":3,"parent":2,"kind":"task","ordered":true,"title":"Deploy","effective_priority":2,
   "focus":1,"repeat":"every 2 weeks, counted from completion","has_notes":true,
   "progress":{"done":1,"total":3},"child_count":2,"descendant_count":3,
   "closed_child_count":1,"children_loaded":false, "...": "..."}
 ]}
```

### `GET /api/nodes/{id}`

One node in full: `{id}` is `SPOKE-12` or `12`. Optional `include_closed` (default `false`) decides how its counts treat closed children, as above.

| Field | Meaning |
| --- | --- |
| `rev`, `version`, `time_zone`, `today` | As in `/api/tree` |
| `path` | Its ancestors, top first, `{"id", "title"}` |
| `node` | Every node field above, plus `notes`: the notes body, or `null` |
| `events` | Its last 10 events, newest first (what `get history: true` shows), each `{"id", "at", "agent", "type", "detail"}`: `at` is RFC 3339 UTC, `agent` may be `null`, `type` is one of `created`, `updated`, `status`, `moved`, `focus`, `claimed`, `released`, `asked`, `answered`, `recurred`; `detail` is the full text (timestamps in it are UTC) |

```sh
curl -s http://127.0.0.1:8787/api/nodes/SPOKE-3
```

```json
{"rev":10,"version":"0.2.0","time_zone":"UTC","today":"2026-11-15",
 "path":[{"id":1,"title":"To do"},{"id":2,"title":"Home"}],
 "node":{"id":3,"parent":2,"kind":"task","title":"Deploy", "...": "...", "notes":"the plan"},
 "events":[{"id":10,"at":"2026-11-15T19:08:56Z","agent":null,"type":"focus","detail":"#1 of 1"},
           {"id":3,"at":"2026-11-15T19:08:56Z","agent":null,"type":"created","detail":"Deploy"}]}
```

### Size and speed

Measured on the dev PC with the release build and a seeded tree of 20,411 nodes (10 areas, 400 projects, 20,000 tasks): `GET /api/tree` (depth 3 from the root, capped at 1,961 nodes, `truncated: true`) answers in about 30 ms; a branch (`root=<project>&depth=1`) in about 4 ms; a `304` in under 1 ms. A slice reads the skeleton of the root's whole subtree (id, parent, status) to get exact counts, so its cost grows with the subtree, not with the response.

## Development

Turn the version hooks on once per clone:

```sh
git config core.hooksPath .githooks
```

They bump the version in `Cargo.toml` and `Cargo.lock` from the commit message and fold the bump into the commit: `feat:` bumps the minor version, `feat!:` (any `type!:`, or a `BREAKING CHANGE:` line) the major, any other commit that changes code the patch. Commits that touch only docs, `.github/` or `.githooks/` do not bump, and neither do merges, rebases, or a commit that already changes the version. CI fails a pull request that changes code without changing the version (`.githooks/check-version-bump`; Dependabot PRs are exempt).

```sh
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
```

`tests/mcp_http.rs` starts the real server on a random port with a temp database and drives `/mcp` over HTTP: the M0 skeleton (initialize, tools/list, tools/call, the 2026-07-28 stateless form, Origin refusal, restart persistence), the M1 tree (a six-step plan written and walked in three calls, depth, notes, update, move, error paths) M2 (search normalization, typos, short keywords, ranking, scope and include_done over a tree with decoys; level inheritance; focus placement and errors; due ordering and every `next` section; `SPOKES_TZ`) M3 (two clients reading `next` and claiming different tasks; claim refusals and lease renewal by every write tool; 24 concurrent claims over HTTP with exactly one winner; ask, NEEDS YOU, answer, takeover of an expired lease, `get` history; the M2 retro regression where a project and its due sub-project both pointed at the same step) and M4 (recurrence in both `from` modes with month clamping, the day-of-month anchor, late completions, the defer window, repeating checklists and the refusals; `defer_until` everywhere; STALE ordering, cap and how deferring, dropping and activity stop it; the orchestrator/worker claim pattern, including the next-step hint a worker gets inside the orchestrator's claimed ordered project; history times in `SPOKES_TZ`). `tests/api_http.rs` drives `/api` the same way: slices by root and depth (pre-order, stored order after a `move`, every count, `path`), closed nodes hidden and shown, every node field (inherited level, focus rank, deferral, `is_step`/`is_next`, claims, questions, answers, repeats, notes, progress), the 2000-node cap (no node ever sends part of its children; a root with 2500 children), node detail with notes and the last 10 events, ETag/304 in every `If-None-Match` form (a write, a direct database change and a lease running out each change the tag), Origin and Host rules, 405/404, every bad parameter, that reading never changes a row, and a timed depth-3 slice of a 20,410-node tree seeded by SQL (about 160 ms in the debug test build). `tests/backup.rs` runs the server with a 1-second backup interval and `keep: 2`, checks rotation, wipes the database, restores the newest backup and reads the data back over MCP. Unit tests cover the recurrence date arithmetic (`src/recur.rs`) and backup rotation and restore validation (`src/backup.rs`). Lease expiry and question age are tested by moving timestamps into the past through a second connection to the database file. `src/store/claims.rs` also races 8 separate SQLite connections (as separate processes would) on one claim.

### Layout

| Path | What |
| --- | --- |
| `src/main.rs` | Process entry: config, logging, signals, `healthcheck`, `backup` and `restore` subcommands |
| `src/backup.rs` | Backups (`VACUUM INTO`), rotation, the daily timer, restore |
| `src/recur.rs` | Repeat rules: parsing, next due date, defer window |
| `src/config.rs` | Environment variables |
| `src/server.rs` | axum router: `/mcp` (rmcp Streamable HTTP, stateless), `/api` and `/health` |
| `src/api.rs` | The read-only JSON API: routes, parameters, Host/Origin guard, ETag/304, gzip; its own read-only SQLite connection |
| `src/tools/` | MCP surface: tool definitions, argument schemas, server instructions |
| `src/store/` | One file per operation (`add`, `get`, `next`, `search`, `focus`, `status`, `update`, `moves`, `claims` for claim/release/ask/answer), `fts.rs` (keeps the search index in step), `view.rs` (the API's tree slices and node detail, as JSON-ready structs), plus shared queries |
| `src/text.rs` | Search normalization, keywords and fuzzy matching |
| `src/db/` | Opening SQLite (WAL), migrations (`PRAGMA user_version` steps), root seed, fractional position keys |
| `src/model.rs`, `src/format.rs` | Domain types and the one-line-per-node text format |

Schema changes: append a step to `STEPS` in `src/db/migrations.rs` (SQL, or a Rust function for backfills); never edit a shipped step. Step 2 creates `node_fts` and backfills it; step 3 adds `nodes.answered_at`. The index is written from Rust (`store::fts::reindex`) by `add` and `update`, not by triggers, because the text is normalized in Rust; status and position live only in `nodes`, which search joins.

The MCP SDK (`rmcp`) is pinned to an exact version; read its changelog before bumping.
