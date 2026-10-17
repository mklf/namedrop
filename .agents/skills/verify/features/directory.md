# On-demand directory and zero prompt context

Namedrop no longer injects a workspace roster into ordinary prompts. Agents resolve exact addresses or discover bounded candidate pages through public read-only CLI operations.

## Sub-features

- `directory-resolve` resolves exact aliases, self and groups, reports unknown names, and returns a group membership fingerprint for guarded sends.
- `directory-candidates` returns deterministic bounded pages with typed readiness and no implicit full-roster injection.
- `directory-zero-hook` leaves an ordinary or embedded-mention prompt with no model context and no stored mutation.

## How to get to it (user POV)

- Run `namedrop address resolve --input-json '{"aliases":["reviewer"],"include_persona":true}'` for an exact recipient.
- Run `namedrop address resolve --input-json '{"self":true}'` only when the current task needs its own address.
- Run `namedrop address candidates --input-json '{"pane_type":"agent","availability":"selectable","limit":8}'` for role-based discovery. Follow `next_cursor` only when more candidates are needed.
- Submit an ordinary `UserPromptSubmit` payload through the real binary. On-demand mode returns success with empty stdout.

## Driving it with control-namedrop

Preconditions:

- The debug binary builds and native Herdr is available.
- The helper creates an activated disposable workspace and two native Bash panes.

Run `.agents/skills/verify/scripts/control-namedrop.py --feature directory`.

The drive names `@lead` and `@reviewer`, saves `review-team`, resolves the exact agent, group and an unknown alias, and requires the saved persona, group type and `v1:` group fingerprint. It resolves self, then reads two one-item candidate pages with `include_self:true` and `availability:all`; the pages must be disjoint and the second must end pagination. It compares the SQLite dump before and after all reads.

Finally it invokes the real `hook user-prompt-submit` with `Ask @reviewer to inspect this.` and requires exit 0 with empty stdout/stderr. Another database comparison covers the hook. Evidence is `directory.json`, `commands.jsonl`, numbered terminal transcripts, `summary.json`, and `cleanup.json`.

## Limits

- Forced agent names in plain shell panes do not create registered agent sessions. Pagination therefore uses `availability:all`; readiness eligibility is covered by `tests/directory.rs` and preview tests.
- This drive proves public CLI behavior and real-binary hook output, not host skill activation, host consumption, or collaborator choice by a model.
- A clean feature result also requires complete fixture cleanup. Inspect `summary.json` and `cleanup.json` rather than accepting the final candidate page alone.
