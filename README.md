# e-agent

e-agent is a small streaming Rust coding agent for OpenAI-compatible
`/chat/completions` APIs. It sends a prompt to a model, executes requested tools
one at a time inside a workspace, returns each result to the model, and prints
the model's final answer.

## Run

```sh
export OPENAI_API_KEY=...
export OPENAI_BASE_URL=https://api.openai.com/v1
export OPENAI_MODEL=gpt-4o-mini
cargo run -- "inspect src/main.rs and explain it"
```

When no prompt argument is supplied and stdout is a terminal, e-agent starts
an interactive TUI (scrollback plus an input line with proper Unicode editing);
use `--repl` for a plain line-based REPL instead. If stdin is piped, the prompt
is read from standard input. Sessions persist to
`.e-agent/sessions/<name>.json` in the workspace (default name `default`) and
are restored and replayed in the TUI on startup. Optional CLI overrides are `--base-url URL`,
`--model MODEL`, `--workspace PATH`, `--session NAME`, and `--max-rounds N`.

The available tools are `read_file`, `write_file`, `edit_file`, and `bash`.
The three file tools use a capability-relative directory rooted at the
canonical workspace (the current directory by default). Tool calls and their
results are displayed as they happen; the final answer ends the turn.

## Safety boundaries

The workspace capability constrains only the agent's file tools. It permits
symlinks that remain inside the workspace and rejects symlink escapes. It does
not constrain hardlink inode origins, does not make a read-then-write sequence
atomic, and is not a sandbox for the whole process.

`bash` runs `/bin/bash -lc` with the workspace as its current directory. It is
not sandboxed: commands can access files outside the workspace, environment
variables, and the network. On Unix, timed-out commands are killed as a process
group; on non-Unix platforms only the direct child is killed. stdout and stderr
are each captured up to 64 KiB, then drained and marked as truncated.

## Background tasks

The `bash` tool accepts `background: true` to run one command without blocking.
Only one background task may run at a time; it has the same workspace cwd,
process-group handling, and per-stream 64 KiB output limit as foreground Bash,
with a 30-minute timeout. Completions surface as a TUI notification while a
turn is active, and the complete output is injected as a clearly labelled user
message at the next model call — whether that is the next round of an active
turn or the start of the next turn. An idle agent never starts a new turn by
itself. In one-shot mode a still-running task is reported before exit;
the process then best-effort terminates its process group. Uninjected background
results are not persisted in sessions.

## Compaction

Use `/compact` in the TUI or `--repl` to manually summarize earlier history.
The agent keeps the most recent four messages intact, summarizes an older
user-aligned prefix, and stores that summary as an ordinary user message in the
session. Compaction has no automatic trigger.

## Environment

- `OPENAI_API_KEY` — required API key.
- `OPENAI_BASE_URL` — API base URL, default: `https://api.openai.com/v1`.
- `OPENAI_MODEL` — model name, default: `gpt-4o-mini`.
- `RUST_BACKTRACE=1` — optional; appends a backtrace to printed error chains.

Errors print their causal chain (for example `cannot decode provider
response: error decoding response body: ...`), and provider HTTP failures
include a preview of the response body. Provider requests time out after
600 seconds in total, including the complete streaming response body.

## Non-goals

This is deliberately not a daemon, JSONL protocol,
automatic compaction trigger, database, event store, subagent
framework, permission framework, MCP/plugin host, multi-provider client,
parallel tool executor, task scheduler, priority system, or concurrency pool.
It has one model seam, one tool seam, and a configurable tool-round limit
(default 32). Reasoning-model `reasoning_content` is display-only; it is never
persisted in a transcript/session or sent back to the API.
