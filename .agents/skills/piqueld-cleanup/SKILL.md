---
name: piqueld-cleanup
description: >
  Run a repository-wide cleanup of piqueld: an audit that measures the code
  and suggests fixes, then an area pass where one agent per area opens a
  reviewed, behavior-preserving refactor PR. Use when asked for a cleanup or
  refactor pass, codebase stats or an audit, to deduplicate or simplify
  piqueld as a whole, or to repeat the October 2026 pass (#241–#249) or the
  November 2026 audit. Not for cleaning up a single PR or file.
---

# piqueld cleanup

You orchestrate a cleanup of the whole repository in two parts: an audit, then
an area pass. Run both unless the owner asks for one. Delegates write the
code. You split the work, brief them, route reviews and report.

Work from a checkout of `origin/main` that contains this skill, since the
delegates read their briefs from it. Keep a ledger in your scratchpad: each
agent, branch, PR, review state and finding. It survives compaction and
restarts.

## Audit

1. Measure: `python3 -I .agents/skills/piqueld-cleanup/scripts/stats.py .
   --json <scratchpad>/stats.json`. It needs `cloc`. Chart the result with the
   `dataviz` skill, `html_preview` and `html_render`, including growth over
   time from the JSON.
2. Look for: low coverage (`just coverage`), areas with few tests, generated
   files missing from `.gitattributes`, files to split by area, `mod.rs`
   files with no children (a module is `foo/mod.rs` only when it has
   children), `unwrap`/`expect` that can panic in production, `#[allow]` that
   should be a narrow `#[expect(..., reason = "...")]`, any `unsafe` or TODO,
   and duplication built up since the last pass.
3. Send the owner the charts and a ranked list of suggestions, each with the
   numbers and paths behind it. Open nothing yet.
4. For each suggestion the owner picks, start a thread with
   `t3_thread_launch`: Claude Opus 5.5, a worktree off `origin/main`, and a
   message that stands alone (goal, numbers, scope, what to report). An audit
   of many sites ends with a table of each site, its verdict and why.

Changes that move code in many areas, such as file splits, merge before the
area pass starts or go into the area agents' tasks. The November 2026 audit
led to #354, #355, #357, #358, #359, #361–#364 and #365.

## Area pass

### Split

Start from this split and adjust it to the code. Split an area over about
30,000 lines along a module boundary, and merge small ones. Every file has
exactly one owner.

| Area | Paths | Scope |
| --- | --- | --- |
| Core | `crates/piqueld-core` | `core` |
| Store | `crates/piqueld-store` | `store` |
| Docker and config | `crates/piqueld-docker`, `crates/piqueld-config` | `docker`, `config` |
| Daemon API | `apps/piqueld/src/{api,auth,application}`, its top-level files except `certificate.rs`, `apps/piqueld/{build.rs,build,examples}` | `server` |
| Daemon runtime | `apps/piqueld/src/{ingress,reconcile,telemetry,git,tailnet}`, `apps/piqueld/src/certificate.rs` | `server` |
| Daemon tests | `apps/piqueld/tests` | `server` |
| CLI | `apps/piquelctl` | `cli` |
| Dashboard | `apps/piqueld-ui`, `tests/playwright` | `ui` |
| Tooling | the other crates (generated code excluded), `tools`, `scripts`, `nix`, `deploy`, `apps/site`, `.github`, root build files | per change |

Nobody edits `migrations` or generated files. Markdown belongs to the docs
lane. Branches are `refactor/cleanup-<area>-<YYYY-MM-DD>`.

Builds run in parallel on a machine with no swap. Pick one
`CARGO_BUILD_JOBS` value for every agent that compiles, and pass it to each.
4 worked for 8 implementers and their reviewers on 16 cores and 58 GB.

Tell the owner the split in one message and start without waiting.

### Start the agents

Call `orchestrator_capabilities` once. For each area, start a `delegate_task`
on Claude Opus 5.5 with `isolation: "worktree"` and `role: "implementation"`.
The task says to follow `.agents/skills/piqueld-cleanup/references/agent-brief.md`,
and names the area, paths, packages, branch, title scope, what the other
areas own, and `CARGO_BUILD_JOBS`.

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
- Send real findings to the implementer.
- After a fix that rewrites code, run a new, narrower review.
- Judgment calls go to the owner, with your recommendation.
- If Sol is over its limit, review with Opus instead and say so.

When a PR's review is clean and it is rebased, mark it ready. Follow its CI
with `babysit-pr`, and update its note to name who reviewed it. The owner
merges, through the merge queue.

When something lands on `main`, have each agent rebase and apply its cleanup
to the new code. Review any logic the rebase merged. Suggest merging core
first, because everything depends on it.

### Report

When every PR is reviewed and green, send the owner one report:

1. The PRs: area, link, lines added and removed.
2. Decisions only the owner can make, with your recommendation.
3. Deliberate behavior changes, with their PR and commit.
4. Merge order and the PRs that share files.
5. Bugs found but not fixed: `file:line`, what goes wrong, impact.
6. Design issues and cross-area duplication.
7. Features missing from one of the three surfaces (`AGENTS.md`).

First have Sol check every bug and parity gap against current `main`. Number
the items `NEW-1`, `NEW-2` and so on, never `#1`, which GitHub links to an
issue or PR.

### After the merge

Rerun the stats and compare them with the audit. Then offer the follow-ups,
and start each one only when the owner asks:

1. Cross-area PRs for duplication the areas held back.
2. A doc-comment PR for what the docs lane reported. It changes generated
   output on purpose.
3. Linear issues for the bugs and parity gaps, following `AGENTS.md`.
4. Fix rounds: at most 3 PRs at a time, sharing no files, Opus implementing
   and Sol reviewing. Check each item is still open first.

## Lessons from the first pass

- Reviews caught these regressions by checking "unchanged" claims:
  - a blanket replace rewrote error paths
  - merged queries lost their index
  - an extractor changed which error wins
  - log fields were renamed
  - a test wait was cut from 10 to 2 seconds
  - assertions were dropped
  - expected test bytes were built by the code under test
  - a fix was hidden in an 8,000-line commit
- `fake_docker` timing tests and Playwright's `branches.spec.ts` flake under
  load. Rerun once and compare with `main`. Never change a test to make it
  pass.
- A usage limit or a T3 restart stops background work. Check the ledger and
  restart only what didn't finish.
- Don't hold a PR for CodeRabbit.
