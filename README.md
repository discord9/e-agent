# e-agent

e-agent is a small, non-streaming Rust coding agent for OpenAI-compatible
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

When no prompt argument is supplied, e-agent reads the prompt from standard
input. If run interactively in a terminal, it starts a REPL instead; type
`/exit` (or `/quit`) or send EOF to leave it. Optional CLI overrides are
`--base-url URL`, `--model MODEL`, and `--workspace PATH`.

The available tools are `read_file`, `write_file`, `edit_file`, and `bash`.
The three file tools use a capability-relative directory rooted at the
canonical workspace (the current directory by default). Tool calls are briefly
reported on stderr; the final answer is printed on stdout.

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

## Environment

- `OPENAI_API_KEY` — required API key.
- `OPENAI_BASE_URL` — API base URL, default: `https://api.openai.com/v1`.
- `OPENAI_MODEL` — model name, default: `gpt-4o-mini`.

## Non-goals

This is deliberately not a streaming client, daemon, TUI, JSONL protocol,
session/resume system, compaction system, database, event store, subagent
framework, permission framework, MCP/plugin host, multi-provider client, or
parallel tool executor. It has one model seam, one tool seam, and a fixed
32-round tool-call limit.
