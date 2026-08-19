# External events V1: asynchronous reminders from opted-in local MCP servers

## Status, scope, and normative terms

This is an implementation design only. V1 adds one narrow capability to the
existing single-writer session runner: an opted-in local stdio MCP server may
send bounded asynchronous reminders to one live `WaitForInput` session that
owns that connection. The words **MUST**, **MUST NOT**, and **MAY** are
normative.

V1 uses the existing `SessionCommand`, `SessionRunner`, `SessionHandle`,
`SessionEntry`, `Agent::apply_entry_located`, `Agent::emit_event`, and
`Shared::emit_agent` paths. It adds no event-source/sink abstraction, plugin
bus, scheduler, worker pool, priority queue, or generic event framework. A
“plugin” below means only a configured local stdio MCP server.

## 1. Configuration and authorization

The only opt-in is an additive field on an existing server definition:

```toml
[mcp.inventory]
command = ["/workspace/bin/inventory-mcp"]
enabled = true
events = true
```

`events` defaults to `false`; `enabled = false` disables tools and events.
An event-enabled definition MUST have a non-empty command and MUST remain a
configured local stdio MCP server. The effective workspace configuration is
the authorization boundary. Existing per-name project/global merge semantics
apply: an effective same-name project definition that omits `events` has
`false`, rather than inheriting an opt-in from a replaced global definition.

The MCP table key is the immutable `configured_source_id` for V1. It is the
only routing identity. The child process, command, environment, instructions,
and payload MUST NOT choose or replace it. A source connection is one-to-one
with one live session and MUST NOT be shared with another session, workspace,
or handle. A session may own several distinct source connections.

Events contain only `id`, `kind`, `text`, and `payload`; `session_id`,
`workspace_id`, `source`, `source_id`, command, environment, and credentials
are forbidden envelope fields. Payload values never control routing or roles.
There is no HTTP/webhook ingress, remote MCP, historical-session delivery,
finished-session resurrection, subagent delivery, or wakeup of a
`FinishWhenIdle` session.

## 2. Bounds, identity, and outcomes

The following limits are checked before admission, parsing oversized JSON, or
writing a store entry:

| item | limit |
|---|---:|
| raw JSONL MCP line, including newline | 65,536 bytes |
| configured source id | 128 UTF-8 bytes |
| non-empty event id | 128 UTF-8 bytes |
| non-empty kind | 64 ASCII bytes, `[A-Za-z0-9._/-]+` |
| text | 8,192 UTF-8 bytes |
| compact payload JSON | 16,384 bytes |
| payload depth | 8, root is depth 1 |
| admitted pending events per session/source | 128 |
| admitted pending bytes per session/source | 1 MiB |
| source rate | token bucket, burst 20 and 60/60 seconds |
| aggregate external provider projection | 64 KiB |

The raw line limit is enforced before JSON parsing. Unknown fields, malformed
JSON, wrong JSON types, invalid methods, invalid kind characters, and any
limit violation are `Invalid`; invalid data is never persisted. A supported
notification is an id-less `notifications/e-agent/event` JSON-RPC message.
Other notifications are safely ignored or rejected as existing MCP behavior,
never interpreted as events.

The event identity is `(configured_source_id, event_id)`. Comparison content
is one deterministic compact canonical JSON encoding of `(kind, text, payload)`
with sorted object keys. `received_at_ms` and transport framing are excluded.
A same-content existing key is `Duplicate`; a different-content existing key
is `Conflict`. The first committed content remains authoritative. Neither
case consumes a pending slot, bytes, or a rate token, and neither appends or
projects a row.

Terminal admission/processing results are:

* `Invalid`: envelope or bound failed;
* `Full`: count or byte reservation would exceed its limit;
* `RateLimited`: no token is available;
* `Closed`: no eligible binding or command path is open at the immediate
  pre-admission check; it is never assigned to an admitted record;
* `Duplicate`: existing admitted or durable key has identical content;
* `Conflict`: existing admitted or durable key has different content;
* `Durable`: append and durable admission succeeded and the entry is queued
  for projection;
* `StoreFailure`: append failed before durable admission;
* `RunnerFailure`: durable admission could not complete because applying or
  emitting the appended entry failed;

Only `Durable` is a successful terminal result for an admitted event. Every
admitted command has exactly one terminal result delivered to its internal
oneshot acknowledgement, if its receiver still exists. `Duplicate`,
`Conflict`, `Invalid`, `Full`, `RateLimited`, and `Closed` are immediate
pre-admission results and have no retained reservation or admitted record.
A failed admission is retryable except that an unchanged identity remains
`Duplicate`, a changed identity remains `Conflict`, and `Full`/`RateLimited`
require capacity/token recovery. `StoreFailure` and `RunnerFailure` are
explicit durable-admission failures; their reservations are released exactly
once and a retry is evaluated afresh against the committed index. V1 offers
no source retry/ack protocol.

## 3. Concrete data and admission state machine

**Proposed V1 identifiers:** the Rust names and APIs introduced by this
revision do not exist in the current crate and are proposals, not claims about
present code. In particular, `ExternalEvent`, `ExternalEventAck`,
`AdmittedExternalEvent`, `SessionHandle::submit_external_event`,
`McpConnectionOwner`, `AgentEvent::ExternalEventNotice`, `TurnOrigin`, and
`external_events_v1` are all proposed V1 additions. `SessionCommand::ExternalEvent`,
`PendingCommand::ExternalEvent`, `SessionEntry::ExternalEvent`, and the
the bounded sender and its `try_send` operation described here are likewise
proposed V1 additions (an unrelated current use of a method with that spelling
does not make this admission API existing). Any other V1-specific identifier
in the examples is also proposed; names explicitly described as existing above
retain their current meaning.

The wire event and internal command are proposed V1 concrete values:

```rust
struct ExternalEvent {
    source_id: String,       // configured MCP table key
    event_id: String,
    kind: String,
    text: String,
    payload: serde_json::Value,
}

enum SessionCommand {
    // existing variants ...
    ExternalEvent {
        event: ExternalEvent,
        ack: tokio::sync::oneshot::Sender<ExternalEventAck>,
    },
}

enum ExternalEventAck {
    Durable, Duplicate, Conflict, Full, RateLimited, Closed, Invalid,
    StoreFailure, RunnerFailure,
}

enum PendingCommand {
    // existing variants ...
    ExternalEvent(AdmittedExternalEvent),
}

struct AdmittedExternalEvent {
    event: ExternalEvent,
    admission_seq: u64,
    ack: tokio::sync::oneshot::Sender<ExternalEventAck>,
    // proposed reservation/cancellation state, released by the runner once
    // on every terminal path
}
```

The exact field visibility may follow the crate, but no generic trait is
introduced. `SessionHandle::submit_external_event` validates the already
source-bound event, checks the dedup index, rate, and limits, inserts a
**tentative** dedup reservation, increments count/bytes, and `try_send`s the
command. It returns an immediate rejection or an internal accepted result;
the oneshot receives the one terminal result later. If the pre-admission
binding or command path is unavailable, it returns `Closed` before creating a
reservation or admitted record. The external channel is bounded by the
count/byte reservations even if the existing command channel is unbounded.

The state machine is:

```text
Received
  -> Rejected(Invalid|Full|RateLimited|Closed|Duplicate|Conflict)
  -> Admitted(reservation + tentative dedup key)
       -> Durable(entry appended and applied; reservation released,
                  dedup becomes committed)
       -> StoreFailure / RunnerFailure (durable-admission failure;
                                        reservation released)
```

A command-path send failure is resolved as the immediate pre-admission
`Closed` result before an admitted record exists.

Admission is the only point at which `Closed` can be returned. Once admitted,
transport close does not change the command's result or cancel its append:
the runner drains it through append and apply, returning `Durable` on success
or the explicit durable-admission failure (`StoreFailure` or
`RunnerFailure`) on failure. Reservation release is exactly once on every
terminal admitted path, including that close drain and a dropped
acknowledgement receiver.

There is exactly one live-close path. The reader encountering EOF, child
exit, or transport read failure MUST send a closure report to the
`SessionRunner` and return; it MUST NOT close or join anything, or resolve
acknowledgements. The runner consumes that report, marks closing and unbinds
ingress, rejects new pre-admissions, and exclusively drains every admitted
record through durable append and apply, resolving its acknowledgement and
releasing its reservation exactly once. Only after that drain is complete does
the runner invoke the idempotent, transport-only `McpConnectionOwner`
shutdown: close stdin, stop and join the already-returning reader,
terminate/reap the child, and release the connection and owner. Transport
shutdown never sends a command back to, drains, resolves an admitted record
for, or waits on the runner.

The admission record has a cancellation flag tied to the ack receiver. If the
receiver is dropped, the runner still processes the admitted command: before
append it continues with append, and after append it retains the committed
dedup index while ignoring the failed ack send. A transport close has the same
behavior and always drains the admitted command as specified above.
Duplicate/conflict lookups never mutate reservations.

The runner owns the single-writer transition. It MUST complete the ack before
forgetting the command, and MUST guard completion plus reservation release
against double execution. A store error for an attempted append sends
`StoreFailure`; a non-store runner exception while appending or applying sends
`RunnerFailure`. Both are explicit durable-admission failures, and both
release the reservation exactly once. If append succeeds but applying or
emitting the projection fails, the entry remains committed and the runner
records its existing safe failure for recovery; the admitted command's ack is
still `RunnerFailure`, not an admission result. No result is silently lost.

## 4. Recovery before Agent construction

Restoration MUST fold external entries before constructing or restoring
`Agent`; no duplicate or conflict may enter provider history first. The
concrete recovery operation is conceptually:

```text
recover_external_history(persisted entries + aligned locations)
  -> {
       audit_entries: all physical rows with original locations,
       projection_entries: first valid row for each identity in sequence order,
       provider_history: existing history with only projection_entries,
       dedup_index: every identity -> first canonical content/location,
       diagnostics: one deterministic diagnostic per duplicate/conflict identity
                      (non-durable, source/event/kind/reason only),
     }
```

The function scans persisted sequence order. The first valid row for an
identity is authoritative; later same-content rows are physical duplicates
and later different-content rows are physical conflicts. `audit_entries` and
locations remain available to history/audit inspection, but only
`projection_entries` and `provider_history` are passed to Agent restoration.
Thus a physical duplicate/conflict is never projected or provider-replayed a
second time. A malformed or conflicting persisted row is fail-closed in the
same way: keep the first valid content, exclude the later row from projection,
and emit the fixed non-durable diagnostic. Recovery does not rewrite or delete
physical rows.

Diagnostics are idempotent: one diagnostic per `(source_id,event_id)` per
recovery result, with the first-row location and a fixed reason; repeating
recovery produces the same diagnostic set and no new store entry. Recovery
itself fails with `StoreFailure` only for unreadable/corrupt storage that
prevents determining sequence/order, not merely for a duplicate or conflict.
The complete dedup index includes committed rows, while tentative admissions
start only after recovery succeeds.

The source identity remains the MCP table key even when its configured command
changes. Replacing the command under the same table key starts a new owned
connection but does not reset history or dedup: an unchanged event id/content
is `Duplicate`, and changed content is `Conflict`. A command change never
makes a second source namespace and never overwrites the original event.

## 5. Two-phase MCP/session lifecycle and ownership

MCP setup and session binding are separate phases. For each effective enabled
server, the factory performs this exact sequence:

1. **Prepare unbound:** spawn/connect the stdio child, start its one reader,
   complete `initialize` and `notifications/initialized`, then call `tools/list`.
   Responses and notifications are multiplexed by that reader, but there is
   no ingress binding. Notifications arriving before bind are rejected as
   pre-admission `Closed` and are not buffered. The existing aggregate connect
   deadline (`CONNECT_ALL_TIMEOUT`) remains in force; the existing per-request
   `CALL_TIMEOUT` remains in force. An initialization failure (including a
   failed initialize/connect operation or setup deadline) enters the canonical
   close path with no admitted records before bind and reports factory failure.
   A `tools/list` result or format failure and a per-request `tools/list`
   timeout are explicitly **nonfatal** and are **not close triggers**: retain
   the connection and server instructions, use zero tool facades, and continue
   to session construction. A valid empty list is also a healthy zero-tool
   connection. A transport read/write/send failure while requesting or
   receiving `tools/list` enters the canonical close path. A child exit or
   transport EOF also enters that path. No event is admitted before binding.
2. **Construct unbound session:** convert the successful list into concrete
   `McpTool` facades (possibly an empty `Vec<Box<dyn Tool>>`) and move those
   facades, plus any server instructions, into the new `Agent`. The facades
   hold an `Arc` to the connection for tool calls. The connection owner is
   not moved into Agent.
3. **Construct runner/handle:** construct `SessionRunner` and its
   `SessionHandle` with `IdlePolicy::WaitForInput`. The runner receives a
   concrete `McpConnectionOwner` containing the prepared connection(s), reader
   handles, and close state. The handle is not published for event ingress yet.
4. **Start then bind:** spawn the runner and wait for its start acknowledgement
   that it is in the idle `WaitForInput` receive loop. Only then atomically
   install exactly one binding per source in the owner's ingress state,
   containing the source key and that handle. Publish the usable session only
   after all requested bindings succeed.

The concrete split is mandatory: tool facades move into Agent; the live
`McpConnectionOwner` is retained by the live `SessionRunner` (one owner field,
which owns all prepared connections and their readers). No bundle is claimed
to retain both a moved `Vec<Box<dyn Tool>>` and ownership of the connection.
The owner is also held by the runner until normal finish or termination; a
factory-local owner is moved into it before the factory returns.

If Agent, runner, handle, runner start, or binding construction fails, or if
the session is abandoned before bind, each prepared connection is disposed by
transport-only shutdown (with no admitted records before bind), and the
unbound session is not published. These setup cleanup cases are not event
admission outcomes. A bind race is resolved atomically: before the bind, the
immediate pre-admission result is `Closed`; after the bind, the event is
validated against that exact source/handle. No event is accepted into a
partially constructed session, and no admitted record can later receive the
pre-admission result `Closed`.

`McpConnectionOwner::shutdown` is idempotent and transport-only. The canonical
close path has exactly these triggers: transport EOF, transport read/write/send
failure, child exit, or initialization failure. A `tools/list` result error,
malformed or late result, and ordinary post-initialization request timeouts are
not close triggers; an initialization/setup timeout is an initialization failure
and does trigger close. For
a live session, the reader reports EOF or transport failure to the runner and
returns. The runner marks closing and unbinds ingress, rejects new
pre-admissions with `Closed`, and exclusively drains every admitted record
through append and apply. Each admitted record receives `Durable` on success
or its explicit `StoreFailure`/`RunnerFailure` durable-admission failure, with
reservation release exactly once. Only after that drain does the runner invoke
`McpConnectionOwner::shutdown` to close stdin, stop and join the
already-returning reader, terminate and reap the child, and release the
connection and owner. Transport shutdown never sends a command back to,
drains, resolves an admitted record for, or waits on the runner, so the
sequence cannot recurse. Before a runner exists, initialization failure uses
the same transport-only shutdown with no admitted records. Pending MCP request
oneshots are resolved by transport shutdown with the existing
connection-closed error. Facades may remain in Agent history, but cannot
submit calls after shutdown.

For the simultaneous-EOF race, an already admitted event remains owned by the
runner: EOF is reported, ingress is unbound, the runner appends and applies the
event, sends `Durable` on success (or `StoreFailure`/`RunnerFailure` if the
append/apply step fails), releases its reservation once, and only then shuts
down transport. It never substitutes an admission outcome for that admitted event.

There is one reader for each connection. It routes id-bearing JSON-RPC
responses through the pending response map and routes only the supported
id-less event notification to the bound ingress validator. It never calls
Agent, writes a store, emits an AgentEvent, or writes the SSE/TUI broadcast.
Outbound writes are serialized. An unknown response id is safely ignored.

## 6. Safe points and exact runner ordering

External work follows the current runner's background rules. An external
command never interrupts provider streaming, a tool call, or compaction. In
particular, no external user-shaped projection may be inserted between an
assistant tool-call entry and any member of its complete sibling `ToolResult`
batch.

The runner uses these safe points:

* **Idle before a turn:** run the existing `commit_backgrounds` first. If it
  committed a background completion and there is no pending work, preserve
  current behavior by scheduling the empty-prompt background follow-up. Then
  drain ready `SessionCommand`s in channel order. A finite nonblocking drain
  snapshots every external command observed at that boundary and coalesces
  those events into one external batch; human prompts remain a separate
  prompt batch. If no prompt exists, an external batch starts a turn without
  emitting or manufacturing `UserPrompt`.
* **After a complete tool batch:** commit every sibling tool result first,
  then run the existing `commit_backgrounds` drain/append path. Only after
  both are complete may the runner drain external commands for the next
  provider operation/turn. External commands are not consumed between the
  assistant tool-call and its sibling results.
* **After a background commit:** the background entry is durably appended and
  projected using the existing path before any newly selected external batch
  is projected. Background completion follow-up behavior remains unchanged.
* **Compaction boundaries:** commands received before compaction's command
  boundary are handled in runner order before that compaction; commands
  arriving during compaction wait. Commit the compaction entry and complete
  its projection before processing the waiting external batch. Streaming
  compaction deltas remain transient and are not mixed into the durable log.
* **Cancellation/finalization:** cancellation releases the provider/tool
  operation according to existing runner rules, then drains/queues commands at
  the next boundary. It does not delete admitted external commands. On
  finalization, no new external command is admitted; the termination drain
  appends and applies every remaining admitted command, resolving `Durable` or
  its explicit durable-admission failure and releasing each reservation once.

Each successful admission receives a proposed V1 monotonic `admission_seq`
from one session-wide counter, strictly increasing in admission order;
rejected commands receive no sequence. At every safe point the runner first
captures a proposed V1 boundary watermark equal to the greatest sequence
admitted at that instant. The finite drain may observe commands concurrently,
but the runner processes only commands whose `admission_seq <= watermark` in
that safe-point batch. A command with a later sequence is retained in a
runner-owned deferred queue for the next safe point; it is never silently
removed or processed early. The watermark and sequence are bookkeeping only,
not persisted model acknowledgements.

The finite boundary snapshot is the set of external commands successfully
admitted at or below the captured watermark and observed by that nonblocking
drain (plus any already-deferred commands at or below it). Admission may
continue concurrently until the per-source 128-event/1 MiB reservation limits
or rate limit are reached; it is never unbounded. The drain uses a fixed finite
observation window: it removes only commands already queued when the boundary
opens and does not loop until empty. An event admitted after the watermark,
including one that races the last `try_recv`, waits for the next boundary.
Events admitted while provider/tool execution is in progress wait for that
next boundary. The runner preserves channel order among equal boundary
eligibility and never lets a later watermark command overtake an earlier
deferred command.

The runner handles `PendingCommand::ExternalEvent` by collecting adjacent
external pending commands from the finite snapshot, appending each in runner
order, applying each successful entry, and then making one provider request
with the resulting external batch projection. A batch is not merged with a
human prompt. If command order places a human prompt first, that prompt turn
runs first and the external batch runs at the next eligible boundary; if the
external batch is first, the inverse applies. A cancel racing admission or an
in-flight operation stops the current operation at the existing operation
boundary, leaves admitted external commands queued, and permits them at the
next `WaitForInput` boundary. A handle/channel that is unavailable at admission returns pre-admission
`Closed`; it does not create an admitted record. A `StoreFailure` or
`RunnerFailure` for one event does not fabricate a provider projection for
that event; the runner follows existing failed-turn termination/error-event
semantics, while later queued events receive their own terminal result during
the termination drain.

“Consumed” is not a durable terminal state. The durable event only establishes
that the data is available for projection. V1 may keep an in-memory batch
sequence/watermark while building one provider request, but it does not persist
model reaction acknowledgement. After a crash, recovery may submit an
external projection again, and provider/model work may repeat. This is not
exactly-once model processing or exactly-once external effect delivery.

## 7. Persistence, provenance, display, and provider context

The durable entry is:

```rust
SessionEntry::ExternalEvent {
    source_id: String,
    event_id: String,
    kind: String,
    text: String,
    payload: serde_json::Value,
    received_at_ms: u64,
}
```

The existing tagged entry format is used. JSONL, SQLite, and Greptime persist
the complete bounded payload through their existing generic entry paths;
sequence and located-entry behavior remains authoritative. No backend gets a
special event table or silently drops the payload.

After append, the runner applies the located entry and emits a bounded
`AgentEvent::ExternalEventNotice` through `Agent::emit_event` and
`Shared::emit_agent`. This is the sole display fanout. The shared log remains
the source of truth for current subscribers, late attach, TUI replay, and web
SSE replay. Renderers show a bounded preview, while history/audit inspection
can display the complete bounded entry. SSE and TUI replay use the same event
projection as live delivery; they do not re-enter MCP ingress.

An external-only model turn has explicit provenance
`TurnOrigin::ExternalEvents { entry_sequences, source_ids }`, distinct from
human `UserPrompt` provenance. The runner MUST NOT emit a `UserPrompt` merely
to wake an idle runner, and compaction MUST NOT relabel external data as a
human prompt. The batch provenance is retained in the turn projection/audit
metadata and is restored from the external entry sequences on replay. A
human-plus-external sequence remains two origins and two model-turn inputs,
never one human-shaped prompt.

Each external entry projects as one escaped, fixed-label untrusted data block:

```text
External event (untrusted data)
source_id: "inventory"
event_id: "stock-193"
kind: "stock.low"
text: "bearing reserve is low"
payload: {"scope":"factory:F2"}
```

`source_id`, `event_id`, `kind`, and `text` are escaped JSON strings and
`payload` uses the canonical JSON encoding. The block is user-shaped data,
not a system, assistant, tool, or caller-controlled role/markup. It cannot
create instructions or an orphan tool result/call. Persisted reasoning text
remains display/audit only and is not provider-replayed, except for the
existing explicit DeepSeek compatibility rule. Tool-call and complete sibling
ToolResult pairing remains unchanged.

The aggregate external blocks included in one provider context MUST be at
most 64 KiB. Before a turn, the existing compaction policy runs at its normal
safe point when the overall context needs it. Repeated compaction is allowed:
older external entries are represented only by the normal bounded compaction
projection with their untrusted provenance, and entries after the latest
compaction boundary are restored from durable rows. If the retained external
set still exceeds 64 KiB, the runner includes the newest complete blocks that
fit plus a fixed untrusted-data summary containing count and sequence range;
it never splits an event, tool call, or tool-result batch. If one complete
block cannot fit, that turn fails with a safe context-cap error and the durable
event remains in history for a later retry/compaction. Durable accepted events
are never rejected merely because provider projection is full; they remain
audit/dedup state. Resume reconstructs the same newest-first bounded
projection from the compaction boundary and later entries, and does not
duplicate physical rows.

External events are bounded and MUST NOT use `read_output` or an output
receipt/reference. `read_output` continues to resolve full-output references
for existing tool/background entries only; no external payload is replaced by
such a reference.

## 8. MCP reader failure and lifecycle details

The reader rejects oversized unterminated lines and resynchronizes at the
next newline without logging the raw line. It treats EOF, child exit, and
transport read failure as connection failure according to existing MCP error
handling, sends a closure report to the runner, and returns. The runner then
marks closing, rejects new pre-admissions, and exclusively drains admitted
records through append and apply before invoking transport-only owner
shutdown. A transport write/send failure also enters this canonical close
path. An ordinary post-initialization request timeout is not a close trigger:
it removes that request from
the pending map and returns the timeout while the transport remains usable.
`tools/list` result or format failure and `tools/list` timeout are likewise
nonfatal; they yield zero tool facades and do not prevent binding otherwise
valid events after runner start. Initialization failure never binds.

Early notifications before initialize/list completion are pre-admission
`Closed`, not queued. A valid empty list and a failed/timed-out list both
preserve the connection, reader, optional instructions, and event binding
after the runner is started. A source with `events = false` never receives a
binding.

## 9. Security and diagnostics

Diagnostics contain only configured source id, event id, kind, and a fixed
reason. They MUST NOT contain raw lines, payload JSON, instructions, command
arguments, environment values, credentials, or API keys. V1 stores accepted
payload data as configured source data; it does not promise secret detection
or redaction. Fixed labels and JSON escaping prevent payload values from
becoming roles, routing metadata, or formatting controls.

## 10. Focused test matrix

Implementation MUST add focused cases for:

* connection/list completes unbound; bind occurs only after runner start; an
  abandoned-before-bind factory uses the canonical close path;
* canonical close triggers only (transport EOF/read/write/send failure, child
  exit, and initialization failure) with pending requests/acks, including
  idempotent close; an admitted event racing transport EOF drains to `Durable`
  or its explicit durable-admission failure, and releases its reservation
  exactly once;
* pre-admission `Closed` has no reservation or admitted record; append failure
  returns `StoreFailure`, apply failure returns `RunnerFailure`, and dropped
  ack receiver plus termination drain release each reservation exactly once;
* pre-restore duplicate/conflict folding with sequence locations, complete
  dedup index, audit rows, projection rows, deterministic diagnostics, and
  repeated-recovery idempotence;
* changed command under one MCP table key preserves source identity and makes
  same-content ids duplicates and changed-content ids conflicts;
* external command handling in a finite boundary batch, a notification race
  with an assistant tool-call/complete ToolResult batch, and an event storm
  bounded by count/bytes/rate; events after the snapshot wait for next turn;
* idle external-only turn starts without `UserPrompt`, busy/cancel races,
  background commit ordering, compaction boundaries, and finalization drain;
* zero tools, valid empty list, nonfatal list result/format failure, nonfatal
  list timeout, ordinary request timeout (pending request removed and
  transport retained), initialization failure, and early-notification
  pre-admission rejection;
* oversized unterminated lines, newline resynchronization, malformed lines,
  unknown response ids, child EOF, and multiplexed response/notification
  handling;
* JSONL, SQLite, and Greptime append/recovery with locations, replay,
  repeated compaction/resume, external-only provenance, aggregate context cap,
  and durable accepted events beyond projection capacity;
* live and late-attach shared-log replay through every TUI and SSE renderer,
  full history/audit payload display, and exclusion of `read_output` references
  from external entries;
* opt-in/default/disabled config merge behavior, source/session isolation,
  and read-only events remaining disabled unless explicitly opted in;
* no reasoning replay beyond the existing provider rules, no orphan tool
  messages, and preservation of assistant tool-call/result pairing;
* rollback refusal by an old binary that does not understand V1 entries,
  feature-gated startup, ingress disable/drain/close, and retained-store
  operational rollback.

## 11. Non-goals, migration, and rollback

V1 deliberately does not include HTTP, webhooks, remote MCP, OAuth, network
listeners, arbitrary plugin processes, shared connections, source discovery,
restart/reconnect, notification buffering before bind, source retry/ack
protocols, delivery to finished/historical/subagent/`FinishWhenIdle` sessions,
exactly-once model processing, exactly-once external effects, a model-handled
ack ledger, secret redaction, caller-controlled roles/markup, MCP resources,
MCP prompts, list-change refresh, subscriptions beyond the one notification,
or concurrent initialization. It also does not add a scheduler, worker pool,
priority, preemption, event bus, or new event-source/event-sink traits.

V1 persistence is feature-gated by an `external_events_v1` store/schema
capability. A binary that does not explicitly understand V1 entries MUST
refuse to resume such a session; it may still open unrelated old sessions.
No claim of old-binary session compatibility is made. Before operational
rollback, disable new event ingress, stop admitting notifications, and use the
canonical close path for each source connection before retaining the session
store. Operators must then run a V1-aware binary or an explicit
migration/export that removes or translates V1 entries before using an older
binary. The design does not promise binary rollback compatibility or
lossless resume by an old binary.

This remains a design only: no Rust, configuration, persistence, or test
implementation is included in this document revision.

## 12. Rejected terminology and consistency guard

The following phrases are retained only as explicitly rejected/non-goal
claims, never as behavior: “exactly once” means V1 does **not** promise it;
“FinishWhenIdle wakeup” means V1 explicitly rejects it; “unbounded ingress”
means V1 explicitly bounds and rejects it; “direct Agent fanout” means the
reader is forbidden from doing it; “command-first ordering” means V1 follows
the runner's existing background commit ordering; and “bundle drops close
automatically” is rejected in favor of the explicit idempotent close sequence.
Older-binary rollback compatibility is explicitly rejected above. No
`FinishWhenIdle` wakeup or provider reaction is manufactured by an idle
boundary.
