# TODO

## Refactor: model background completion as a first-class turn trigger

**Context**: commit `af0d7a8` fixed completions stranding when they arrive
during a turn's final tool call. The fix works but has a smell: the
`unanswered_background` flag exists only to bridge the gap between "completion
injected into history" (turn-end) and "model sees it" (next turn). The TUI's
`run_request` loop polls this flag and runs a follow-up empty turn.

**Smell**: using a state flag to paper over a timing split. Agent tells TUI
across turns "I stuffed something into context but the model hasn't seen it".

**Cleaner direction**: make "completion arrived" a first-class turn trigger on
par with user input, instead of "sneak into history and hope the next turn
picks it up". Options:

- Let `run()` drain pending completions itself (internal loop) so it returns
  only when no unanswered completions remain — TUI goes back to "one run = one
  turn", the flag disappears.
- Or model completion as an event source the agent loop selects on directly.

**Why not done yet**: larger refactor; touches cancel/interrupt semantics
(is Ctrl-C during a completion follow-up turn cancelling the whole run or just
that turn?); current fix is small, tested, and each piece is clear. Per
AGENTS.md anti-overengineering, not worth it until the flag proves painful.

**Trigger to revisit**: if the flag causes real confusion, or if more
"inject-and-notify" cases appear (a second event source like completion).
