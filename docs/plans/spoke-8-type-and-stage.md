---
status: Planned
spoke: SPOKE-8
started:
finished:
summary: Add an optional type (bug/feature/chore/idea/epic) and stage field to Spokes nodes, settable via add/update, shown on lines, filterable in search and next, exposed in /api.
---

# SPOKE-8: type and stage fields on nodes

## Context

Spokes is replacing the GitHub Projects board (SPOKE-6). The board had a Kind column (Bug, Feature, Chore, Idea, Epic) and a Stage column. Spokes has neither, so bk-flow records stage as `Stage:` lines in notes. This change adds two optional, own-value-only (not inherited) fields to every node:

- `type`: one of `bug`, `feature`, `chore`, `idea`, `epic`. It is named `type` because `kind` already means area/task.
- `stage`: one of `idea`, `backlog`, `brainstormed`, `understood`, `planned`, `in_progress`, `in_review`, `done`. This is the board's list plus bk-flow's stages.

Agreed at Gate 1 (2026-11-23):
- **Input:** values are case-insensitive, and spaces or hyphens count as underscores ("In review" → `in_review`).
- **Stored:** values are stored lowercase snake_case.
- **Clearing:** `""` clears a field on `update`.
- **Independent of status:** stage does not touch status (stage `done` does not close a node, and `set_status done` does not set stage).

Repo: `~/dev/spokes` (Rust, rusqlite, rmcp, axum). Checks: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test`.

## Steps

### Step 1: domain types, migration, row loading, tool args (foundation)
Dependent: none. Everything else depends on it.

What to do:
- **Enums in `src/model.rs`.** Add `WorkType` (Bug, Feature, Chore, Idea, Epic) and `Stage` (Idea, Backlog, Brainstormed, Understood, Planned, InProgress, InReview, Done).
  - Each gets `as_str()` returning the stored snake_case form, and a `parse_loose(&str) -> Result<Self, String>`.
  - `parse_loose` trims, lowercases and turns spaces/hyphens into `_`. Its error lists the valid values, e.g. `unknown type 'bugg'. Use one of: bug, feature, chore, idea, epic.`
  - Both enums get `Display` and `FromStr` (FromStr = parse_loose).
- **Node fields.** Add `work_type: Option<WorkType>` and `stage: Option<Stage>` to `Node`.
- **Migration 4 in `src/db/migrations.rs`.** Append one new `Step::Sql`; never edit the existing steps:
  `ALTER TABLE nodes ADD COLUMN work_type TEXT CHECK (work_type IN ('bug','feature','chore','idea','epic')); ALTER TABLE nodes ADD COLUMN stage TEXT CHECK (stage IN ('idea','backlog','brainstormed','understood','planned','in_progress','in_review','done'));`
- **Row loading in `src/store/mod.rs`.** Append `n.work_type, n.stage` to the end of `NODE_COLS` (indexes 23, 24). Parse them in `node_from_row` the same way `kind`/`status` are parsed (optional; a bad value gives `FromSqlConversionFailure`).
- **Tool args in `src/tools/args.rs`.** Add the fields; don't wire them yet. Arg values are plain `Option<String>`, parsed later with `parse_loose`.
  - On `NodeSpec`, update and filters: `#[serde(rename = "type")] pub work_type: Option<String>` and `pub stage: Option<String>`.
  - Add both to `NodeSpec`, `UpdateArgs`, `SearchArgs` and `NextArgs`.
  - Doc comments (they become the schema text models read):
    - NodeSpec/UpdateArgs type: `What sort of work: "bug", "feature", "chore", "idea" or "epic".` (UpdateArgs adds `"" clears it.`)
    - NodeSpec/UpdateArgs stage: `Where it is: "idea", "backlog", "brainstormed", "understood", "planned", "in_progress", "in_review" or "done". Separate from status.` (UpdateArgs adds `"" clears it.`)
    - Search/Next type: `Only nodes of this type (bug, feature, chore, idea, epic).`
    - Search/Next stage: `Only nodes at this stage (e.g. in_review).`
- **Fix every place that builds a `Node` or these arg structs** (test helpers, `Default`, struct literals) so it compiles. Search with `grep -rn "NextArgs {\|SearchArgs {\|UpdateArgs {\|answered_at:" src tests`.
- **Unit tests in `src/model.rs`:** `parse_loose` accepts `"In review"`, `"in-review"`, `"IN_REVIEW"` and `"Bug"`, and rejects `"bugg"` with the value list.
- **Migration test:** opening a fresh database reaches `latest()` == 4. Extend the existing migration test, or add one in `src/db/mod.rs` if there is none.

Files: `src/model.rs`, `src/db/migrations.rs`, `src/store/mod.rs`, `src/tools/args.rs`, plus any test helpers that build these structs.
Acceptance: the crate builds and all existing tests still pass. Dead-code warnings for the new unread arg fields are acceptable until batch 2.
Verify: `cd ~/dev/spokes && cargo test`

### Step 2: set type and stage via add and update
Dependent on Step 1. Independent of Steps 3–6.

What to do:
- **`add` (`src/store/add.rs`).** In `clean_spec`, parse `spec.work_type` / `spec.stage` with `WorkType::parse_loose` / `Stage::parse_loose`. An error names the child as other fields do (`children[3] ("Push"): unknown type ...`). An empty string on add is an error that says to omit the field.
  - Carry both values in `Clean` and add them to the `INSERT` in `insert`.
  - Areas may carry a type/stage. Don't restrict it.
- **`update` (`src/store/update.rs`).**
  - **Setting:** `type: "bug"` sets `work_type`; `type: ""` (after trim) sets NULL. The same goes for `stage`.
  - **Changed labels:** push `"type"` / `"stage"` into `changed`, or `"type cleared"` / `"stage cleared"` when clearing, mirroring how recur does it.
  - **No other effects:** no status side effects.
- **Tests (in the existing `#[cfg(test)]` modules of those files):**
  - add with `type: "Bug", stage: "In review"` stores `bug` / `in_review`.
  - A nested child with a bad type writes nothing and names the child.
  - update sets, changes and clears both fields.
  - The logged `updated` event detail mentions `type` / `stage`.
  - A bad value is rejected with the list of valid values.

Files: `src/store/add.rs`, `src/store/update.rs`.
Acceptance: the cases above pass, and setting or clearing via update shows in the row.
Verify: `cd ~/dev/spokes && cargo test store::add store::update`

### Step 3: show type and stage on get lines
Dependent on Step 1. Independent of Steps 2, 4, 5, 6.

What to do:
- **Flags in `detail_flags` (`src/format.rs`).** Right after the priority flag, push the type (e.g. `bug`) and then the stage (e.g. `in_review`), only when set. Example header: `SPOKE-8 task p1 bug in_review +notes  Title`. This covers `header_line` and `child_line` (used by `get` and outlines).
- **Unit tests in `src/format.rs`'s test module:** a node with both fields shows them in order after the priority; a node with neither shows no extra flags. The existing format tests are unchanged.

Files: `src/format.rs`.
Acceptance: `get` header and child lines carry the flags.
Verify: `cd ~/dev/spokes && cargo test format`

### Step 4: filter search by type and stage, and show them on result lines
Dependent on Step 1. Independent of Steps 2, 3, 5, 6.

What to do:
- **Parse the filters** at the top of `search` (`src/store/search.rs`) from `args.work_type` / `args.stage` with `parse_loose`. A bad value is an error listing the valid values.
- **Empty query with a filter.** When the query has no keywords but at least one filter is given, don't error. Instead list every node in scope (not the root) whose own type/stage match all given filters.
  - Hide closed nodes unless `include_done` is set, keeping the hidden count note.
  - Sort by effective level (unset last), then id.
  - Header: `N matches for type bug, stage in_review:`.
  - With no keywords and no filter, keep the current error.
- **Filter keyword searches too.** With keywords and filters, run the normal search, then keep only hits whose node matches the filters.
  - The filter needs the node's work_type/stage. Either extend `Meta`/`load_meta` to select `work_type, stage`, or check the loaded `Node`. Filter before applying `limit`, so extend `Meta`.
- **Flags on result lines.** In `result_line`, push the type and stage flags right after the effective level, in the same format as Step 3: `bug`, then `in_review`.
- **Tests in the search test module:**
  - `query: "", stage: "in_review"` lists only nodes at that stage.
  - Keywords plus `type` filter drops non-matching hits.
  - A bad filter value errors.
  - An empty query with no filter still errors.
  - Result lines show the flags.

Files: `src/store/search.rs`.
Acceptance: the cases above pass, and existing search tests are unchanged.
Verify: `cd ~/dev/spokes && cargo test store::search`

### Step 5: filter next by type and stage, and show them on work lines
Dependent on Step 1. Independent of Steps 2, 3, 4, 6.

What to do:
- **Parse the filters** in `next` (`src/store/next.rs`) from `args.work_type` / `args.stage` with `parse_loose`. A bad value is an error listing the valid values.
- **Filter each section.** After the walk and before sorting or limiting, keep only entries whose own node matches every given filter. This applies to NEEDS YOU, FOCUS, DUE SOON, UP NEXT and STALE. The filter is on the line's node itself, not its `next:` step, and is not inherited.
- **Empty result.** If nothing is left, reply `Nothing under <scope id> <title> matches type bug, stage in_review.`
- **Flags on work lines.** In `work_row`, push the type and stage flags after the level/due/repeats flags.
- **Tests in next's test module:**
  - `next type: "bug"` shows only bug-typed work items.
  - `next stage: "in_review"` likewise.
  - Both filters combine with AND.
  - A bad value errors.
  - Lines show the flags.
  - Existing next tests are unchanged.

Files: `src/store/next.rs`.
Acceptance: the cases above pass.
Verify: `cd ~/dev/spokes && cargo test store::next`

### Step 6: expose type and stage in /api
Dependent on Step 1. Independent of Steps 2–5.

What to do:
- **API fields in `src/store/view.rs`.** Add `#[serde(rename = "type")] pub work_type: Option<&'static str>` and `pub stage: Option<&'static str>` to `ViewNode`. Fill them in `view_node` from `n.work_type` / `n.stage` (`as_str()`), `null` when unset.
- **Test in `tests/api_http.rs`:**
  1. Add a node via MCP with `"type": "bug", "stage": "in_review"`; this test needs Step 2.
  2. `GET /api/nodes/{id}` returns `"type": "bug"` and `"stage": "in_review"`.
  3. A node without them returns `null` for both, in `/api/tree` too.
  - If Step 2 hasn't landed when this runs, set the columns with a direct SQL `UPDATE` in the test's DB setup (the file already inserts rows directly in `ins`).
- **README:** update the API field list in the "Read-only JSON API" section, if it lists fields.

Files: `src/store/view.rs`, `tests/api_http.rs`.
Acceptance: the new test passes, and existing API tests are unchanged (the fields are additive).
Verify: `cd ~/dev/spokes && cargo test --test api_http`

### Step 7: tool descriptions, README, full checks
Dependent on Steps 2–6.

What to do:
- **Tool descriptions in `src/tools/mod.rs`.**
  - The `update` tool description lists the fields it changes; add type and stage.
  - The `search` and `next` descriptions mention they can filter by type and stage.
  - Keep them short, as the existing ones are.
- **README.**
  - Add type and stage to the `add`/`update` bullets, the `search` and `next` bullets (filters, empty query with a filter), and the `get` line example.
  - Note that stage is separate from status.
  - Note migration 4.
- **Run the full checks** and fix anything they flag.

Files: `src/tools/mod.rs`, `README.md`.
Acceptance: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings` and `cargo test` all pass.
Verify: `cd ~/dev/spokes && cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test`

## Batches

- **Batch 1:** Step 1.
- **Batch 2 (parallel):** Steps 2, 3, 4, 5 and 6. Each touches its own files. They share one working tree, so if `cargo` fails on a file outside a step's list, it's another step mid-edit: wait and rerun rather than touching that file.
- **Batch 3:** Step 7, then the main session's own verification and delivery.

## Delivery path

1. **Branch and version bump:** branch `spoke-8-type-and-stage` from `main`. The version bump comes from the repo's git hooks; make sure `git config core.hooksPath .githooks` is set before committing.
2. **Local end-to-end check:** run `SPOKES_DB_PATH=<scratchpad>/spokes.db SPOKES_BIND=127.0.0.1:8799 cargo run --release` on a copy of nothing (fresh DB). Then over `curl` JSON-RPC to `/mcp`:
   - add a node with type bug and stage In review;
   - `search` with `query: ""` and `stage: in_review`;
   - `next` with `type: bug`;
   - `GET /api/nodes/<id>`.
3. **Migration check:** run the migration against a copy of a real backup, not the original. Use `spokes backup` from the live server, or the newest file in the backup dir if reachable. The server must start and existing nodes must load with null type/stage. If no backup copy is reachable, skip this and say so.
4. **PR:** push the branch and open a PR (`Part of SPOKE-8`). CI must be green.
5. **Merge and publish:** merge fast-forward into `main`, then delete the branch locally and on the remote. The release workflow publishes `ghcr.io/ogl33t/spokes:latest` and `sha-<7>`.
6. **Your part:** redeploy the `kdev3/spokes` stack in Arcane so it pulls the new `:latest`. The stack is not sha-pinned and has no bump step. **Stop** and wait for your word that it is redeployed.
7. **Production check by real use:**
   1. Over the deployed `/mcp` (curl JSON-RPC, since this session's MCP tool schemas predate the new args), set SPOKE-8 to `type: bug, stage: in_review`.
   2. `search query: "" stage: in_review` lists SPOKE-8.
   3. `next type: bug` shows only bug-typed items.
   4. `/api/nodes/8` returns both fields.
   5. Then set SPOKE-8 to `type: feature`, stage `done`.
8. **Docs:**
   - Update this plan file with what shipped and any deviations.
   - Commit the plan update docs-only to `main`.
   - Capture follow-ups with `/idea`: "bk-flow reference/spokes.md: use the stage field instead of notes Stage lines" and "spokes stack: pin sha-<7> and add a bump-stack job".

## Stops

- Any step's verify fails in a way that is not plainly fixable inside that step's files.
- Migration 4 fails against the backup copy.
- CI fails for a reason outside this change.
- Waiting for your Arcane redeploy (delivery step 6).
- The production check shows the old behaviour after the redeploy.

## What "go ahead" authorizes

- Create and push branch `spoke-8-type-and-stage` to `ogl33t/spokes`.
- Open a PR on `ogl33t/spokes` and merge it fast-forward into `main`, then delete the branch locally and on the remote.
- Let the release workflow publish `ghcr.io/ogl33t/spokes:latest` / `sha-<7>` (triggered by the merge).
- Write to the live Spokes server: set and later reset type/stage on SPOKE-8, notes, stage lines and status on SPOKE-8 and its plan nodes, and two new `/idea` nodes.
- Commit and push docs-only plan updates to `main`.

Not authorized (stop and ask): editing `ogl33t/stacks`, redeploying in Arcane, tagging a `v*` release, touching spokes-view, editing bk-flow.

## Rollback

- **PR/merge:** revert the merge commit on `main` and push; the release workflow republishes `:latest` without the change.
- **Database:** migration 4 only adds nullable columns. An older build refuses to start on a v4 database ("schema version 4 is newer than this build supports"). To roll back the image, restore the pre-deploy backup with `spokes restore`, or keep the new build. Take a `spokes backup` before redeploying.
- **Spokes writes:** `update type: "" stage: ""` clears the test values.

## Risks

- **Rolling back past migration 4** needs a restore: older builds refuse a newer schema. Mitigation: back up before the redeploy.
- **Parallel batch 2 subagents** share one working tree and cargo target, so builds can briefly fail on another step's half-edit. Mitigation: each step's files are disjoint, and retry on foreign failures.
- **Stale tool schemas:** this session's MCP tool schemas won't show the new args until Claude Code reconnects. The production check uses curl.
- **spokes-view:** reads `/api` with fields added. It is additive, but if it decodes strictly it could break. A check in the production step: load spokes-view after the redeploy.
