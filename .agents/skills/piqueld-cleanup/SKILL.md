---
name: piqueld-cleanup
description: >
  Run a repository-wide cleanup of piqueld: an audit that measures the code
  and suggests fixes, then a cleanup pass where one agent per area opens
  focused, reviewed refactor PRs, a boundary lane cleans up between crates,
  and every bug found gets a Linear issue and a fix PR. Use when asked for a
  cleanup or refactor pass, codebase stats or an audit, to deduplicate or
  simplify piqueld as a whole, or to repeat the October 2026 pass or the
  November 2026 audit. Not for cleaning up a single PR or file.
---

# piqueld cleanup

You orchestrate a cleanup of the whole repository in two parts: an audit, then
a cleanup pass. Run both unless the owner asks for one. Delegates write the
code. You split the work, brief them, route reviews and report.

Work from a checkout of `origin/main` that contains this skill, since the
delegates read their briefs from it. Keep a ledger in your scratchpad: each
agent, branch, PR, review state and finding, and the files each task holds.
Files are free once the task holding them has reported and its PRs touching them
have merged. Start no task on files that aren't free. A forced edit outside an
area (a renamed item's callers, a docs reference) is the exception: the owner of
those files rebases over it. After compaction, a usage limit or a T3 restart,
check the ledger and restart only what didn't finish.

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
cleanup pass starts or go into the area agents' tasks.

## Cleanup pass

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
and Markdown belong to the docs lane. Each area agent opens several focused
PRs, on branches named `refactor/cleanup-<YYYY-MM-DD>-<area>-<topic>`.

Builds run in parallel on a machine with no swap. Pick one
`CARGO_BUILD_JOBS` value for every agent that compiles, and pass it to each.
4 worked for 8 implementers and their reviewers on 16 cores and 58 GB. Run
at most 8 reviews at a time.

Create one Linear issue for the pass, as `AGENTS.md` describes. Each PR says
`Part of PQD-<n>`. Tell the owner the split in one message and start without
waiting.

### Start the agents

Call `orchestrator_capabilities` once. For each area, start a `delegate_task`
on Claude Opus 5.5 with `isolation: "worktree"` and `role: "implementation"`.
The task says to follow `.agents/skills/piqueld-cleanup/references/agent-brief.md`.
It names the area, paths, packages, branch prefix, title scope, Linear issue,
what the other areas own, and `CARGO_BUILD_JOBS`.

Start a docs lane the same way, on `docs/cleanup-pass-<YYYY-MM-DD>`. It
follows the `piqueld-docs` skill, fixes drift between the docs and the code,
runs `just docs` and opens a draft PR. It reports stale `///` comments and
clap help instead of editing them. Those comments sit in the area agents'
files and feed generated output.

### Review

Review each focused PR on its own as its agent reports it. Call
`link_pull_request` and start a review: a `delegate_task` on GPT 6.1 Sol with
`role: "review"`, `isolation: "worktree"` and
`target.options: { reasoningEffort: "high" }`.
It follows `.agents/skills/piqueld-cleanup/references/review-brief.md`.
Give it the PR number, the head SHA, the base branch (the parent PR's branch
for a stacked PR), `CARGO_BUILD_JOBS`, and a focus list:
every "unchanged" claim in the PR description, and every merged helper or
test that could have changed behavior.

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

### Boundary lane

The area agents each see one area. Alongside them, start one more Opus
delegate, `role: "research"`, to look at how the areas fit together:

- the dependency direction and layering between the crates, and logic that
  sits in the wrong crate
- public items that no other crate needs, and internals that leak across a
  boundary
- types, conversions and helpers duplicated across crates
- how the daemon, CLI and dashboard each use core and the client, and what
  each surface reimplements
- errors that lose their context at a crate boundary

It edits nothing. It reports themes, each with the files involved and the
change it proposes. Add the cross-area issues the area agents report to that
list. Themes that need a design decision go to the owner first.

Once a theme's files are free, start one Opus implementation delegate per
theme. Merge themes that share files, or run them one after another. Each
follows the agent brief, with the theme's files as its area, runs
`just boundary` too, and opens focused PRs on
`refactor/cleanup-<YYYY-MM-DD>-boundary-<topic>`. Review them like the others.

### Bugs

Every bug an agent or reviewer reports gets an issue and a fix PR:

1. Check it against current `main`, with a Sol delegate when there are many.
   Merge duplicate reports, and ask the owner about anything that might be
   intended. A vulnerability stops here: tell the owner only, and open no
   issue or PR (`SECURITY.md`).
2. Open a Linear issue as `AGENTS.md` describes, with type Bug, or reuse the
   one that exists (`list_issues`). Include the `file:line`, what goes wrong
   and how to reproduce it.
3. Once its files are free, start an Opus implementation delegate in a worktree,
   on `fix/<topic>` from a freshly fetched `origin/main`, with the issue and
   `CARGO_BUILD_JOBS`. It writes a test that fails without the fix, makes the
   smallest fix, runs the agent brief's checks, and opens a draft PR with
   `file-pr` that says `Fixes PQD-<n>`.
4. Review and ship it as in Review, but without the review brief, which
   treats any behavior change as a finding. The reviewer's task says to review
   read-only at the head SHA, check that the test fails without the fix, the
   fix is correct and minimal, and nothing else changed, and report findings
   with severity and `file:line`.

Run at most 3 fixes at a time.

### Report

When every area and docs PR is reviewed and green, send the owner one
report:

1. The PRs by area: link, lines added and removed, merge order, and the PRs
   that are stacked.
2. Decisions only the owner can make, with your recommendation.
3. The bugs: each one's issue, fix PR and state.
4. The boundary themes, and which ones wait for the area PRs.
5. Other design issues.
6. Features missing from one of the three surfaces (`AGENTS.md`), checked
   against current `main`.

Number the items `NEW-1`, `NEW-2` and so on, never `#1`, which GitHub links to
an issue or PR.

### After the merge

Rerun the stats and compare them with the audit. Report again when the boundary
and fix PRs are reviewed and green. Then offer these follow-ups, and start each
one only when the owner asks:

1. A doc-comment PR for what the docs lane reported. It changes generated
   output on purpose.
2. Linear issues for the parity gaps.
