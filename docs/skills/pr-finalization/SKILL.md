---
name: pr-finalization
description: Prepare and finalize a pull request for submission — read the target repo's PR template, trim diff-scoped comment noise and behavior-less tests with evidence, verify before/after, and produce a strictly completed PR description. Use when asked to finalize, clean up, or prepare a PR. Never publishes GitHub content.
---

# PR Finalization

Prepare a change set for PR submission: complete the PR description against the target repo's real template, remove only diff-scoped noise (unnecessary comments, behavior-less tests), and prove nothing was lost. The skill prepares content; it never publishes.

## When to use

- The user asks to "finalize", "clean up", "polish", or "prepare" a PR.
- A PR description must be written or completed for an existing change set.
- A draft diff needs review for removable comment/test noise before submission.

Do NOT use when the task is to publish (open, comment on, review, or merge a PR), fix CI, or change behavior.

## Non-goals

- **No self-publishing.** Never create, comment on, review, or merge a PR, and never call GitHub messaging APIs — those actions require explicit user approval. Pushing branches is the orchestrator's responsibility, not the skill's. Output is a completed description and a clean diff.
- No CI/coverage policy changes: never lower thresholds, disable jobs, or add skips.
- No behavioral code changes; the diff under review is frozen except for comment/test cleanup. No history rewriting (rebase, amend, force-push). No work outside the diff under review.

## Workflow

### 1. Discover the PR template (strict)

- Identify the target repository (usually the workspace root). If `.github/PULL_REQUEST_TEMPLATE.md` exists, it is authoritative — use it as-is.
- Otherwise find the repo's actual template: case variants (`pull_request_template.md`), templates under `.github/`, `docs/`, or the repo root, or one referenced by `CONTRIBUTING.md` or a recent merged PR body.
- If the target repo is not the current workspace, read the **target** repo's template, never this repo's.
- Only if no template exists anywhere, fall back to a built-in structure and say so explicitly.

### 2. Strict completion

- Fill every section; no placeholder left unfilled. Keep the template's own headings and ordering; do not rename or drop sections.
- Verification must list exact commands with their real output. Do not invent required issue numbers; fill only fields the template itself asks for.

### 3. Diff-scoped comment cleanup

- Delete: comments that merely restate the code; outdated/misleading comments that no longer match behavior; commented-out code blocks (dead code).
- Keep: comments explaining why (rationale, trade-offs, non-obvious behavior); invariants, contracts, and safety properties; SPDX license headers; generated-code markers; lint directives (`#[allow(...)]`, `// noqa`, `// nolint`, pragmas).
- When intent is unclear, treat the comment as "why" and keep it.

### 4. Test evidence gate

- A test may be deleted only if every behavior it protected is still covered by a surviving test — evidence-based, never count-based.
- For each deleted test, record: name and file; behaviors protected; surviving test(s) covering them (name + file, or the exact assertion), verified by actually running them; the run output.
- Forbidden as evidence: coverage percentages or line/function counts, "the code is obviously fine", slowness/flakiness/annoyance.
- Eligible: exact duplicates (name the surviving twin), mock-only implementation-detail tests, assertion-free tests, tautologies, dead tests (not wired to a runner).
- Cannot prove coverage → keep.

### 5. Hard keep list

Never delete, even with apparent coverage elsewhere: regression tests for fixed bugs; boundary/edge-case tests (empty input, max values, off-by-one); protocol/wire-format tests; error-path tests; security tests (auth, injection, permissions, secrets); concurrency/race tests; tests referenced by CI/coverage config or docs.

### 6. Pre/post verification

- Before cleanup, run the target repo's own verification (the commands in its docs/CI) and record the baseline.
- After cleanup, re-run the same commands; results must be identical or better. Record both runs.
- Never weaken CI jobs, coverage thresholds, or lint levels; no `#[ignore]`, `skip`/`xfail`, or commented-out failing tests.

### 7. Draft report before deletion

- Write a short report: what will be deleted (file:line, test name), why it qualifies, and which surviving test covers each protected behavior (names, not counts).
- If the report cannot name covering tests, do not delete.

### 8. Produce the final PR description

Fill the discovered template with the final content; summarize the cleanup in What Changed, with the evidence table from section 4.

## Checklist

- [ ] Target repo's PR template read; every section completed, no placeholders left
- [ ] Restating/outdated/commented-out removed; why/invariant/contract/SPDX/generated/lint kept
- [ ] Every deleted test has a named surviving test with actual run output
- [ ] Hard-keep tests intact; baseline and post-cleanup verification recorded
- [ ] No CI weakening or skips; no GitHub create/comment/review/merge; branch push left to the orchestrator
