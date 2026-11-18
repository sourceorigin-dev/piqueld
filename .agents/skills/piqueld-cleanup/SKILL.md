---
name: piqueld-cleanup
description: >
  Run a repository-wide cleanup of piqueld: an audit that measures the code
  and suggests fixes, then an area pass where one agent per area opens a
  reviewed, behavior-preserving refactor PR. Use when asked for a cleanup or
  refactor pass, codebase stats or an audit, to deduplicate or simplify
  piqueld as a whole, or to repeat the October 2026 pass or the November
  2026 audit. Not for cleaning up a single PR or file.
---

# piqueld cleanup

You orchestrate a cleanup of the whole repository in two parts: an audit, then
an area pass. Run both unless the owner asks for one. Delegates write the
code. You split the work, brief them, route reviews and report.

Work from a checkout of `origin/main` that contains this skill, since the
delegates read their briefs from it. Keep a ledger in your scratchpad: each
agent, branch, PR, review state and finding. After compaction, a usage limit
or a T3 restart, check the ledger and restart only what didn't finish.

## Audit

1. Measure: `python3 -I .agents/skills/piqueld-cleanup/scripts/stats.py .
   --json <scratchpad>/stats.json`. It needs `cloc`. Chart the result with
   `html_preview` and `html_render`, including growth over time from the
   JSON.
2. Read the output, then check what the script can't count:
   - coverage, with `just coverage`
   - generated files missing from `.gitattributes`
   - which `unwrap` and `expect` calls can really panic
   - duplication built up since the last pass
3. Send the owner the charts and a ranked list of suggestions, each with the
   numbers and paths behind it. Open nothing yet.
4. For each suggestion the owner picks, start a thread with
   `t3_thread_launch`. Use Claude Opus 5.5 in a worktree off `origin/main`.
   Its message stands alone: the goal, the numbers, the scope and what to
   report. A thread that checks many sites reports a table of each site, its
   verdict and why.

Changes that move code in many areas, such as file splits, merge before the
area pass starts or go into the area agents' tasks.

## Area pass

### Split

Start from this split and adjust it to the code. Split an area over about
30,000 lines of source and tests along a module boundary, and merge small
areas. Every file has exactly one owner.

| Area | Paths | Scope |
| --- | --- | --- |
| Core | `crates/piqueld-core` | `core` |
| Store | `crates/piqueld-store` | `store` |
| Docker and config | `crates/piqueld-docker`, `crates/piqueld-config` | `docker` |
| Daemon runtime | `apps/piqueld/src/{ingress,reconcile,telemetry,git,tailnet}`, `apps/piqueld/src/certificate.rs` | `server` |
| Daemon tests | `apps/piqueld/tests` | `server` |
| Daemon API | the rest of `apps/piqueld` | `server` |
| CLI | `apps/piquelctl` | `cli` |
| Dashboard | `apps/piqueld-ui`, `tests/playwright` | `ui` |
| Tooling | every other hand-written file: the other crates, `tools`, `nix`, `deploy`, `apps/site`, `examples`, `scripts`, dot-directories, root files | none: `refactor: …` |

Nobody edits `crates/piqueld-store/migrations` or generated files. `docs/`
and Markdown belong to the docs lane. Branches are
`refactor/cleanup-<area>-<YYYY-MM-DD>`.

Builds run in parallel on a machine with no swap. Pick one
`CARGO_BUILD_JOBS` value for every agent that compiles, and pass it to each.
4 worked for 8 implementers and their reviewers on 16 cores and 58 GB.

Create one Linear issue for the pass, as `AGENTS.md` describes. Each PR says
`Part of PQD-<n>`. Tell the owner the split in one message and start without
waiting.

### Start the agents

Call `orchestrator_capabilities` once. For each area, start a `delegate_task`
on Claude Opus 5.5 with `isolation: "worktree"` and `role: "implementation"`.
The task says to follow `.agents/skills/piqueld-cleanup/references/agent-brief.md`.
It names the area, paths, packages, branch, title scope, Linear issue, what
the other areas own, and `CARGO_BUILD_JOBS`.

Start a docs lane the same way, on `docs/cleanup-pass-<YYYY-MM-DD>`. It
follows the `piqueld-docs` skill, fixes drift between the docs and the code,
runs `just docs` and opens a draft PR. It reports stale `///` comments and
clap help instead of editing them. Those comments sit in the area agents'
files and feed generated output.

### Review

As each draft PR opens, call `link_pull_request` and start a review: a
`delegate_task` on GPT 6.1 Sol with `role: "review"`,
`isolation: "worktree"` and `target.options: { reasoningEffort: "high" }`.
It follows `references/review-brief.md`. Give it the PR number, the head SHA,
`CARGO_BUILD_JOBS`, and a focus list: every "unchanged" claim in the PR
description, and every merged helper or test that could have changed
behavior.

- Check each finding against the code before acting on it.
- Send real findings to the implementer's thread with `t3_thread_send`.
- After a fix that rewrites code, run a new, narrower review.
- Judgment calls go to the owner, with your recommendation.
- If Sol is over its limit, review with Opus instead and say so.

When a PR's review is clean and it is rebased, mark it ready. Follow its CI
with `babysit-pr`, and update its `[!NOTE]` line to name who reviewed it. The
owner merges, through the merge queue.

When something lands on `main`, have each agent rebase and apply its cleanup
to the new code. Review any logic the rebase merged. Suggest merging core
first, because everything depends on it.

### Report

When every PR is reviewed and green, have Sol check every bug and parity gap
against current `main`. Then send the owner one report:

1. The PRs: area, link, lines added and removed.
2. Decisions only the owner can make, with your recommendation.
3. Deliberate behavior changes, with their PR and commit.
4. Merge order and the PRs that share files.
5. Bugs found but not fixed: `file:line`, what goes wrong, impact.
6. Design issues and cross-area duplication.
7. Features missing from one of the three surfaces (`AGENTS.md`).

Number the items `NEW-1`, `NEW-2` and so on, never `#1`, which GitHub links to
an issue or PR.

### After the merge

Rerun the stats and compare them with the audit. Then offer the follow-ups,
and start each one only when the owner asks:

1. Cross-area PRs for duplication the areas held back.
2. A doc-comment PR for what the docs lane reported. It changes generated
   output on purpose.
3. Linear issues for the bugs and parity gaps.
4. Fix rounds: at most 3 PRs at a time, sharing no files, Opus implementing
   and Sol reviewing. Check each item is still open first.
