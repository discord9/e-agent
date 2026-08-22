# Native e-agent Project Memory: Munin Design

**Status:** implementation-ready design record
**Scope:** native project memory for e-agent; documentation only
**Name:** Munin

This document records the agreed behavior of the native e-agent project-memory
system. **Confirmed** means implementation must preserve the behavior described
here. **Implementation choice** means the behavior is required but the concrete
schema, module, UI widget, or API shape is intentionally left to the
implementation phase. The latter must not silently change a confirmed
invariant.

## 1. Summary

Munin is a database-backed, project-scoped memory system for the GreptimeDB and
SQLite session backends. It derives coarse candidate knowledge from the
authoritative transcript, consolidates that knowledge into Markdown stored in
database rows, and makes active memory available through explicit lexical
retrieval. It does not inject memory content into ordinary model prompts.

The graph is a projection of the latest active coarse project memories and
links between them. Memory Markdown is the content source for the graph and
for retrieval; there is no second canonical set of memory documents on disk.
The graph index is rebuildable. User corrections and deletions are durable
instructions to Munin, so later processing cannot simply regenerate knowledge
that the user rejected.

Munin is not Engram and does not preserve Engram compatibility. Engram is
completely removed as part of implementation, with no migration path.

## 2. Confirmed decisions

### 2.1 Backend availability and default

- Memory is **default enabled** only when the session backend is SQLite or
  GreptimeDB.
- Memory is **opt-out** for SQLite and GreptimeDB. The implementation must
  provide an explicit opt-out and must honor it for all memory work, including
  background work and graph reads.
- JSONL is explicitly unavailable for Munin. There is no JSONL memory mode, no
  filesystem fallback, and no silent fallback to another backend. Selecting or
  running with JSONL must never create a second memory store.
- The exact configuration spelling and the user-facing behavior when an
  unavailable backend is selected are implementation choices. They must make
  unavailability explicit rather than pretending that memory is active.
- There is no Engram compatibility layer, import, migration, adapter, or
  interpretation of old Engram state. Existing Engram data is not migrated.

### 2.2 Authority and content

- The transcript is authoritative for what happened in a session. Munin is a
  derived projection and cannot rewrite, contradict, or become the source of
  truth for the transcript.
- Database rows containing Markdown are the source of memory content. The
  current Markdown in an active memory row is what retrieval and graph
  projection read.
- A filesystem document must not become a parallel source of truth for a
  memory. Export, cache, debug, or backup files, if implementation later adds
  any, are not authoritative memory content.
- Source provenance must point back to the originating session and an exact
  transcript range. Provenance is retained with a memory and exposed in the
  UI/read path.
- A capability to read old session history by that provenance is required. It
  must be possible to open the referenced historical session and transcript
  range even when that session is not the currently active session. The
  concrete read operation and presentation are implementation choices.

### 2.3 Project partition and identity

- Memories are partitioned by a project logical identity, not by an individual
  session, user prompt, or worktree path.
- Worktrees of the same Git repository share one project identity and therefore
  share the same project memories and graph.
- A different repository is a different project partition. Personal/global
  memory and cross-project memory are not part of this design.
- The exact canonical repository identity algorithm, handling of detached
  worktrees, and behavior outside a Git repository are implementation choices.
  They must preserve the same-repository-worktrees invariant.
- Session-level provenance remains distinct from project identity: several
  sessions can contribute evidence to one project memory.

### 2.4 Memory shape and graph

- A memory is coarse project knowledge represented as Markdown, with enough
  provenance for a user or Munin to inspect its evidence.
- Only the **latest active coarse project memories** are graph nodes.
- Revisions and history are not graph nodes by default. They remain history or
  audit material and may be used to explain provenance, correction, deletion,
  or merge behavior.
- Files, sessions, commits, and modules are not graph nodes. They may appear
  as provenance or text, but they are not entities in the memory graph.
- Markdown `memory://` links form graph edges. A link target identifies another
  memory in the same project partition unless a future design explicitly says
  otherwise.
- A `## Links` section may use exactly these relation labels:
  `related`, `depends_on`, `explains`, `supersedes`, and `conflicts_with`.
  No additional relation vocabulary is implied by this document.
- A `memory://` link in the ordinary Markdown body, without a relation label
  from `## Links`, is a `related` edge.
- The edge index is rebuildable from the latest active Markdown rows. It is a
  projection, not an independent authority. Rebuilding must remove stale edges
  and must not require an unrecoverable edge-only record.
- Direct user edits are Markdown edits. After an edit, the implementation
  reparses `memory://` links and updates the rebuildable edge projection.

### 2.5 Retrieval and model behavior

- Retrieval is an explicit lexical tool. The model must actively call it when
  it needs project memory; memory content is not automatically injected into
  model context.
- Retrieval searches active memories in the current project partition. It must
  not cross the project boundary or return deleted/inactive memories as active
  knowledge.
- Multi-query retrieval is supported. Multiple queries use OR/any matching by
  default.
- Phrase matching and all-query matching are explicit modes, not accidental
  changes to the default. The implementation must expose an explicit way to
  request phrase semantics and an explicit way to request that all supplied
  queries match. Exact argument names and result pagination are implementation
  choices.
- Search results must provide enough location information to highlight the
  matched Markdown and enough memory identity/provenance to open its details.
- A source-history read capability complements retrieval: a user or Munin can
  follow exact provenance to the relevant old session transcript range.
- The Main agent may explicitly emphasize a memory. This is a deliberate,
  user/model-directed action, not permission for Main to routinely save its
  observations. There is no pending-note or scratch-note state in the design.

### 2.6 Munin role and lifecycle

- The standalone summarizer role is removed. Its responsibilities merge into
  the new Munin role.
- After a session becomes **Busy -> Idle**, one Phase 1 Munin call is eligible
  for that session. That call produces both:
  1. an activity summary for the UI/pet, and
  2. a coarse candidate bundle for memory consolidation.
- The activity summary is not a memory. It is not a graph node, is not returned
  by memory retrieval as memory content, and must not become a second source of
  truth.
- Phase 1 is organized as independent watermark jobs per project/session.
  Sessions may run in parallel. Each job advances only its own durable
  watermark after successful processing and must be safe to retry without
  duplicating effective results.
- Phase 2 has one Munin writer per project. Different projects may write in
  parallel; two writers must not concurrently consolidate the same project.
- Phase 2 reads eligible Phase 1 candidate bundles, the current active project
  memories, and durable user intents, then writes the next active coarse
  Markdown memories and their provenance. It is the consolidation/write phase,
  not an automatic prompt-injection phase.
- There is no general scheduler, worker pool, priority queue, or always-on
  memory daemon. Existing lifecycle safe points are used:
  - root startup, for eligible existing work; and
  - Busy -> Idle / Finished eligibility transitions, for newly completed work.
- The precise ordering of startup work, safe-point hooks, retry backoff, and
  process-shutdown handling is an implementation choice. It must not turn the
  system into a general scheduler or lose a completed eligible watermark job.

### 2.7 Correction, deletion, and merge

- User correction and user deletion are durable intents. They are not merely
  transient UI actions and must survive restarts and later consolidation.
- Munin must read those intents before producing or retaining memory content.
  A prior transcript remains authoritative evidence of what was said, but it
  cannot by itself regenerate a contradicted or deleted current memory.
- A correction changes the accepted project knowledge; a deletion suppresses
  the rejected knowledge. The implementation must retain enough audit and
  provenance to explain the action without exposing the deleted memory as
  active retrieval content.
- Merge is a user-visible memory operation. Its resulting Markdown and
  provenance must remain project-scoped, and obsolete source memories must not
  remain active graph nodes.
- Exact intent record shape, conflict resolution for multiple corrections, and
  retention policy for inactive/deleted rows are implementation choices. The
  durable-intent and non-regeneration guarantees are not.

## 3. Non-goals and explicit exclusions

Munin deliberately does **not** provide:

- embeddings, semantic retrieval, or a vector database;
- automatic injection of memory content into model context;
- personal, global, or cross-project memory scope;
- a file/entity graph, including files, sessions, commits, or modules as graph
  nodes;
- a generic scheduler, worker pool, priority system, or concurrency framework;
- Engram migration, compatibility, or fallback behavior;
- speculative relation labels beyond the five confirmed `## Links` labels;
- double source-of-truth filesystem memory documents;
- an automatic habit of saving Main's observations;
- a pending-note or unsaved-memory queue;
- revision/history nodes in the default graph;
- persistent graph layout;
- graph-canvas editing;
- remote/shared memory synchronization or multi-user conflict management;
- JSONL-backed memory.

The graph is a first-release product surface, not a later optional visualization.

## 4. Logical model

The following are behavioral concepts, not approved database schema names. The
implementation may map them to existing storage structures or choose concrete
names and types later.

### 4.1 Project

A project is the hard logical partition identified by the canonical Git
repository identity. All memory reads, writes, graph nodes, edges, intents,
and Phase 2 locks are scoped to it. Worktree paths are useful context but do
not create separate projects.

### 4.2 Session transcript

A session transcript is the immutable-or-authoritative record of the agent
interaction as provided by the selected supported database backend. Munin
consumes transcript ranges, records exact source ranges in candidate and
memory provenance, and never replaces transcript events with a summary.

The provenance reference needs to identify at least the project, session, and
an exact range that can be read later. Whether a range is represented by
sequence values, event identifiers, timestamps plus bounds, or another stable
backend-specific locator is deliberately open.

### 4.3 Memory row

A memory row contains the current Markdown content for one project memory and
its lifecycle/provenance information. Active rows are eligible for retrieval
and graph projection. Inactive, deleted, superseded, or historical material
may remain for audit and provenance but is not an active graph node by default.

The Markdown should be human-readable and should preserve `memory://` targets.
The design does not prescribe front matter, headings other than the optional
`## Links` convention, or a fixed template. Any structured metadata needed by
implementation must not make a second Markdown file authoritative.

### 4.4 Candidate bundle

A Phase 1 candidate bundle is intermediate derived output, not an active
memory. It includes coarse candidate claims and exact transcript provenance
sufficient for Phase 2 to evaluate them. It may include the Phase 1 activity
summary as a separate output, but the summary remains UI/pet activity and is
not memory content.

### 4.5 Durable intent

A durable intent records a user's correction, deletion, explicit emphasis,
or merge direction in a form that future Munin calls can consume. It is
project-scoped and tied to the affected memory and/or source provenance as
appropriate. It is not a pending note and is not inferred from a model's
ordinary prose.

## 5. End-to-end behavior

### 5.1 Startup and eligibility

At root startup, the existing lifecycle evaluates memory-enabled database
sessions for Phase 1 and Phase 2 eligibility. A Busy -> Idle transition, or a
Finished transition where the lifecycle exposes that eligibility, performs the
same evaluation for the affected session/project. The evaluation uses durable
watermarks and durable intents; it does not scan an unbounded filesystem cache.

If memory is opted out, no Phase 1 call, Phase 2 write, retrieval result, or
graph projection is performed for that memory scope. If the backend is JSONL,
Munin is unavailable rather than redirected to another store.

### 5.2 Phase 1: per-session activity and candidates

For each eligible project/session watermark job:

1. Read the transcript range after that job's watermark, subject to the
   implementation's bounded-call policy.
2. Invoke Munin once for the Busy -> Idle eligible completion.
3. Ask the same Munin role to produce an activity summary and a coarse
   candidate bundle in separate structured outputs.
4. Require every candidate claim to carry exact session/transcript-range
   provenance, or reject it as non-writable.
5. Persist the successful intermediate result and advance the job watermark
   atomically as far as the backend permits.

The summary is suitable for immediate UI/pet display and may be discarded or
retained as operational history according to an implementation choice. It is
never searchable memory content. Candidate bundles are inputs to Phase 2 and
do not become graph nodes merely because they exist.

Independent session jobs may execute concurrently, including sessions in the
same project. They must not mutate the active project memory projection in
Phase 1.

### 5.3 Phase 2: per-project consolidation

For each project with eligible candidates or durable intents:

1. Acquire the per-project writer exclusion using the existing lifecycle/runtime
   mechanisms, not a general-purpose scheduler.
2. Read the latest active project memories, their relevant provenance, candidate
   bundles, and all unapplied durable intents for the project.
3. Ask Munin to reconcile candidates with active Markdown and intents.
4. Write the resulting current Markdown memory rows and their provenance.
5. Reparse every resulting Markdown document for `memory://` links and rebuild
   or incrementally refresh the project edge index.
6. Mark the processed inputs/intents and release the project writer
   exclusion only after the durable write succeeds.

Projects may run Phase 2 in parallel with one writer per project. A retry must
not produce two active versions of an intended final memory merely because a
process stopped after a write; the concrete transaction/idempotency mechanism
is an implementation choice and must be tested.

### 5.4 Explicit emphasis

Main can explicitly emphasize a memory by invoking the agreed memory action.
That action is durable and available to Munin when selecting or preserving
project knowledge. Main does not call this action for ordinary observations,
and no automatic observation-to-note path exists. The exact effect on
selection, ordering, or retention is an implementation choice; it must not
turn emphasis into automatic prompt injection.

## 6. Markdown and link rules

A memory's database Markdown is both its user-facing content and the input to
link parsing. The parser must recognize Markdown `memory://` links wherever
the supported Markdown syntax permits them. Link target normalization and
handling of malformed or missing targets are implementation choices, with
these constraints:

- links remain project-scoped;
- a missing target cannot create a graph node for a file, session, commit, or
  module;
- ordinary body links have relation `related`;
- an exact `## Links` section may assign only `related`, `depends_on`,
  `explains`, `supersedes`, or `conflicts_with`;
- no other relation is silently invented; and
- rebuilding the edge index from active Markdown is always possible.

A user editing Markdown edits the memory itself. Save/reparse behavior must
update link edges, preserve user text, and keep provenance/correction rules
intact. Graph-canvas manipulation is not supported: users edit Markdown, not
edges in a separate editor.

## 7. Lexical retrieval contract

The retrieval tool is active, project-scoped, and lexical. Its implementation
must support:

- one or more query strings;
- OR/any matching as the default for multiple queries;
- an explicit phrase mode;
- an explicit all-query mode;
- matching/highlighting information sufficient for the graph page and model
  result consumer; and
- a route from a result to the current memory details and exact provenance.

Search operates on active memory Markdown, not raw transcript text by default.
Transcript history is opened through the separate provenance read capability.
The tool may return an empty result when memory is opted out or unavailable,
but it must not silently search another project or backend. The exact lexical
normalization, case behavior, ranking, result limit, continuation mechanism,
tool schema, and error wording are implementation choices.

The model sees memory only after it actively requests retrieval. Retrieval
results are references/content selected by that call, not an automatically
appended memory prompt.

## 8. Graph page: first-release core

The graph page ships as part of the first release. It is a view over active
memory rows and the rebuildable edge index, not a separate persistence model.
It must provide:

- Markdown editing;
- lexical search;
- match highlighting;
- neighborhood inspection;
- filtering;
- navigation between linked memories;
- memory details;
- exact provenance display and old-session-history reading;
- correction;
- deletion; and
- merge.

Correction, deletion, and merge actions must create the durable intents or
other durable records needed for Munin to respect them later. Editing Markdown
must reparse links after save.

The first release does **not** persist node positions, zoom, or other graph
layout. It does **not** offer graph-canvas edge/node editing. Navigation and
neighborhood may be rendered in any suitable list or diagram presentation as
long as the required search, filter, details, provenance, and link behavior
remain available.

## 9. Safety, consistency, and failure behavior

- A failed Phase 1 or Phase 2 call must leave the prior durable watermark and
  active memory projection usable. It must not advance success state past
  unprocessed input.
- A malformed candidate without exact provenance is not writable as a memory.
- A failed graph-index rebuild must not corrupt active Markdown content. The
  index can be rebuilt again from active rows.
- User deletion/correction intents must be durably recorded before the UI
  reports success. A later retry must consume them before accepting candidates
  from older transcripts.
- Project isolation is checked at every read and write boundary, not only in
  the UI.
- Memory content and provenance must obey the existing session/database access
  and privacy boundaries. This design does not grant a memory feature access
  to JSONL or to another project.
- Logging and diagnostics may identify failed jobs, but must not turn the
  activity summary into retrievable memory or expose content across projects.

Exact transactional boundaries differ between SQLite and GreptimeDB and are
implementation choices. Acceptance tests must cover equivalent observable
behavior for both supported backends.

## 10. Staged implementation plan

The stages are ordered to keep the transcript and database rows authoritative
throughout implementation.

### Stage 0: remove the old integration

1. Remove Engram configuration, startup wiring, role/tool registration, and
   documentation references from the implementation surfaces.
2. Do not add an importer, compatibility parser, migration command, or fallback.
3. Confirm that no memory behavior depends on Engram being installed.

### Stage 1: capability gating and authoritative source

1. Gate Munin to SQLite and GreptimeDB, enabled by default there and opt-out.
2. Make JSONL memory availability explicit and fallback-free.
3. Define the project identity resolver so same-repository worktrees share a
   partition.
4. Define durable transcript-range provenance and the read-old-session-history
   capability.
5. Add durable per-project/session Phase 1 watermark handling at root startup
   and lifecycle safe points.

### Stage 2: Munin extraction and consolidation

1. Implement one Munin role containing the former summarizer responsibilities.
2. Implement the one-call Phase 1 output contract: UI/pet activity summary plus
   coarse candidate bundle.
3. Run independent session watermark jobs in parallel.
4. Implement one-writer-per-project Phase 2 with projects parallel.
5. Store current memory Markdown in database rows with provenance.
6. Add durable correction, deletion, emphasis, and merge intents; ensure old
   transcripts cannot regenerate contradicted/deleted active knowledge.

### Stage 3: lexical read path and graph projection

1. Implement the active lexical retrieval tool with OR-default multi-query,
   explicit phrase mode, and explicit all-query mode.
2. Parse Markdown links and construct the rebuildable active-memory edge index.
3. Enforce the five `## Links` relation labels and `related` body-link rule.
4. Ensure only latest active coarse project memories are nodes.

### Stage 4: first-release graph page

1. Add the graph page with Markdown editing, search/highlight, filtering,
   neighborhood, navigation, details, provenance/history, correction,
   deletion, and merge.
2. Reparse links after direct Markdown edits.
3. Omit persistent layout and graph-canvas editing.
4. Exercise restart, retry, opt-out, project isolation, and backend parity.

### Stage 5: hardening and rollout

1. Measure Phase 1/Phase 2 latency and safe-point behavior without introducing
   a scheduler or worker pool.
2. Verify index rebuild and database recovery from active Markdown rows.
3. Verify user-intent precedence under delayed, duplicate, and out-of-order
   candidate bundles.
4. Document the final concrete configuration/schema/tool/UI choices without
   weakening this design's confirmed decisions.

## 11. Test and acceptance criteria

### 11.1 Availability and partition tests

- SQLite starts with memory enabled by default and can opt out.
- GreptimeDB starts with memory enabled by default and can opt out.
- JSONL reports memory unavailable and never writes a fallback file/store.
- Two worktrees of one Git repository read and write the same project memory
  partition.
- Two different repositories cannot retrieve or graph one another's memories.
- An opted-out project produces no Phase 1 call, Phase 2 write, active retrieval
  content, or graph content.

### 11.2 Authority and provenance tests

- A candidate and its resulting memory retain an exact session and transcript
  range.
- The old-session-history capability opens that exact range after the original
  session is no longer active.
- Changing a summary never changes transcript events.
- Retrieval and graph details read current Markdown from database rows.
- No filesystem Markdown document is required to restore current memory
  content.

### 11.3 Pipeline and concurrency tests

- Busy -> Idle produces exactly one eligible Phase 1 call for the completed
  watermark; retrying after failure eventually processes it without duplicate
  effective memory.
- Root startup finds eligible work left by a prior process.
- Independent session jobs can run in parallel without corrupting watermarks.
- Two Phase 2 attempts for one project cannot concurrently publish conflicting
  active projections.
- Phase 2 for different projects can run in parallel.
- A Phase 1 activity summary is visible to the UI/pet but is absent from memory
  retrieval and graph nodes.
- There is no general scheduler/pool behavior in the implementation.

### 11.4 Intent and editing tests

- A correction intent survives restart and prevents the old transcript
  candidate from restoring the contradicted content.
- A deletion intent survives restart and prevents the deleted knowledge from
  reappearing as active retrieval or a graph node.
- Merge leaves the intended result active and obsolete inputs inactive.
- Explicit emphasis is durable, while ordinary Main observations do not create
  memory or pending notes.
- Editing Markdown changes the database memory content and reparses links.
- A failed write does not report correction/deletion success or advance the
  relevant processing state.

### 11.5 Retrieval and graph tests

- Lexical retrieval is an explicit tool call; an ordinary model turn receives
  no automatic memory-content injection.
- Multiple queries use OR/any by default.
- Phrase and all-query modes work only when explicitly requested.
- Search highlights matched text and opens details/provenance.
- Only latest active coarse project memories appear as graph nodes.
- Files, sessions, commits, modules, revisions, and history do not appear as
  default nodes.
- Body `memory://` links become `related` edges.
- `## Links` accepts exactly the five confirmed relations and rejects or
  handles unsupported labels without inventing graph semantics.
- Rebuilding the edge index from active Markdown produces the same graph after
  deleting the old index.
- The graph page supports all required first-release operations and does not
  persist layout or offer canvas editing.

### 11.6 Backend parity and recovery tests

Run the behavior suite against SQLite and GreptimeDB. Where the databases have
different transaction facilities, test the same externally visible guarantees:
watermarks are not falsely advanced, intents are durable before success is
reported, active Markdown survives index failure, and restart/retry converges
to one result.

## 12. Implementation choices intentionally left open

The following are not confirmed decisions and must be selected during
implementation without introducing a new product behavior:

- concrete database schema/table and column names, types, indexes, and
  migrations for the new native records;
- whether current session persistence structures are extended or adjacent
  database records are used;
- exact configuration key and UI control for opt-out/unavailable JSONL;
- canonical Git repository identity algorithm and non-Git-directory behavior;
- memory identifier format and `memory://` URI grammar beyond its role as a
  memory link;
- Markdown parser/library, supported link syntax, and `## Links` formatting;
- exact Munin prompt, structured output encoding, model selection, token/call
  limits, and candidate size limits;
- the Phase 1 watermark unit and the exact startup/Busy -> Idle/Finished hook
  ordering;
- retry, crash recovery, and per-project exclusion mechanics within existing
  runtime lifecycle safe points;
- transaction/atomicity technique for SQLite and GreptimeDB;
- retention and audit policy for candidate bundles, inactive rows, revisions,
  deleted content, and durable intents;
- conflict precedence when several durable intents or candidates address one
  memory;
- lexical tokenization, case sensitivity, ranking, phrase interpretation,
  highlighting, pagination, and tool argument/result names;
- exact provenance locator representation and the read-history UI/tool shape;
- how explicit emphasis affects Munin selection or preservation;
- graph edge direction/display for each allowed relation, while retaining the
  declared relation semantics;
- graph page route/component layout and visual presentation;
- activity-summary retention and event shape for the UI/pet; and
- observability, metrics, and user-facing error wording.

These open choices must not add embeddings, automatic injection, global scope,
a file/entity graph, a generic scheduler/pool, Engram compatibility, a second
filesystem source of truth, or extra relation labels.

## Appendix A. Codex CLI research and bounded reuse

This appendix records public research used to avoid reinventing useful pipeline
ideas. It is not a statement that Codex CLI is an authority for Munin's
product decisions.

### Public source links

1. **Codex memories README** — describes the current startup memory pipeline,
   Phase 1 per-rollout extraction, Phase 2 consolidation, and why the phases
   are split:
   <https://github.com/openai/codex/blob/main/codex-rs/memories/README.md>
2. **Codex Phase 1 implementation** — per-thread extraction and structured
   output processing:
   <https://github.com/openai/codex/blob/main/codex-rs/memories/write/src/phase1.rs>
3. **Codex Phase 2 implementation** — bounded selection, singleton lease, and
   consolidation-agent execution:
   <https://github.com/openai/codex/blob/main/codex-rs/memories/write/src/phase2.rs>
4. **Codex consolidation prompt** — current Phase 2 organizer instructions:
   <https://github.com/openai/codex/blob/main/codex-rs/memories/write/templates/memories/consolidation.md>
5. **Codex lexical search** — public multi-query, match-mode, line-window, and
   pagination implementation:
   <https://github.com/openai/codex/blob/main/codex-rs/ext/memories/src/tools/search.rs>
6. **Codex memory state schema** — Stage 1 output and job state:
   <https://github.com/openai/codex/blob/main/codex-rs/state/memory_migrations/0001_memories.sql>

### What is deliberately copied or adapted

- **Two-phase shape:** Munin adapts the useful separation between per-session
  extraction and consolidation. Phase 1 creates a summary plus coarse
  candidates; Phase 2 is the project writer.
- **Watermark-oriented eligibility:** Munin adapts the idea that background
  processing claims/advances bounded eligible work, but binds it to e-agent's
  existing startup and Busy -> Idle/Finished safe points rather than adding a
  scheduler.
- **Parallel extraction and serialized consolidation:** Munin adapts parallel
  independent session work and serial writing for one shared project, while
  allowing different projects to write in parallel.
- **Explicit lexical search controls:** Munin adapts the public Codex MCP idea
  of multi-query search and explicit match modes. Munin's required OR default,
  phrase mode, all-query mode, active-project scope, and model-invoked tool
  behavior remain the confirmed e-agent contract.
- **Human-readable memory content:** Markdown remains a useful editing and
  inspection format, but in Munin it is stored in database rows rather than a
  canonical filesystem workspace.

"Copied" here means a bounded design pattern, not code, schema, prompt text, or
an assertion that the two systems have the same behavior.

### What is deliberately not copied

- **No graph:** Codex's public memories pipeline/MCP surfaces do not define a
  memory graph. Munin adds a graph of active coarse memories and Markdown
  `memory://` edges.
- **No semantic search:** Munin does not copy or add embeddings, vector search,
  or semantic indexing.
- **No automatic memory injection:** Codex exposes a read path that can feed
  memory into agent instructions. Munin deliberately requires an explicit
  lexical retrieval tool call and does not inject memory content.
- **Per-memory correction/deletion are required here:** The public Codex
  memories MCP backend contract covers list/read/search; it does not provide
  Munin's durable per-memory correction and deletion intent model. Munin adds
  those intents and requires them to prevent transcript regeneration.
- **Hard project partition:** Codex's documented project guidance and memories
  surfaces do not establish Munin's hard database project partition with
  same-repository worktrees sharing identity. Munin does, and has no personal
  or global scope.
- **No filesystem source of truth:** Codex's public memory README describes
  filesystem artifacts such as `raw_memories.md` and rollout-summary files.
  Munin deliberately stores current Markdown memory content in database rows
  and does not maintain duplicate canonical documents.
- **No global single writer:** Codex's described global Phase 2 consolidation
  is not copied. Munin uses one writer per project and permits projects in
  parallel.
- **No Codex-specific retention, prompts, git baselines, MCP protocol, or
  rollout artifact layout:** Those are implementation details of Codex, not
  decisions for e-agent.

## Appendix B. Decision checklist for implementation review

Before implementation is accepted, reviewers should be able to answer yes to
all of the following:

- Is Engram gone, with no migration or compatibility path?
- Are SQLite and GreptimeDB opt-out/default-enabled while JSONL is explicitly
  unavailable with no fallback?
- Is the transcript authoritative, while current Markdown memory content comes
  from database rows?
- Do same-repository worktrees share one hard project partition?
- Are only latest active coarse memories graph nodes, with only permitted
  Markdown links as edges?
- Can Munin consume durable correction/deletion intents without regenerating
  rejected knowledge from old transcripts?
- Is Phase 1 one Munin call per eligible session completion and Phase 2 one
  writer per project, using only existing lifecycle safe points?
- Is retrieval lexical, multi-query OR-default, explicit for phrase/all, and
  model-invoked rather than injected?
- Can users inspect exact provenance, read old session history, edit Markdown,
  search/highlight, navigate/filter/neighborhood, correct/delete/merge, and
  reparse links from the first-release graph page?
- Are all non-goals and open implementation choices still respected?
