# Cleanup pass: area agent

You own one area of piqueld in a parallel cleanup pass. Your task names the
area, paths, packages, branch prefix, title scope, Linear issue and
`CARGO_BUILD_JOBS`, and lists what the other agents own.

Read every file in your area. Make real simplifications, not cosmetic churn,
and report bigger issues. Never add an `#[allow]`. Use
`#[expect(..., reason = "...")]` on the smallest item that needs it. A module
is `foo/mod.rs` only when it has children.

## Rules

- **Change no behavior.** That includes HTTP responses (and which error a
  request with several faults gets), OpenAPI, the manifest, CLI flags, help,
  output and exit codes, configuration, NixOS options, Docker names and
  labels, log fields, timeouts and query plans. `git diff origin/main` shows
  no change to a migration or a generated file. Don't use blanket search and
  replace.
- **Report bugs instead of fixing them.** The orchestrator opens an issue and a
  fix PR for each. If a cleanup can't be done without changing behavior, report
  it too.
- **Stay in your area.** Edit another area only when a rename forces it:
  keep it minimal and list those files in the PR description. Report what
  crosses areas: another lane cleans up between them.
- **Keep the docs references right.** If you rename something a page in
  `docs/` names, update that page (`piqueld-docs` skill) and run `just docs`.
  Don't reword `///` comments that feed generated output.
- Stop and report anything that needs a design decision.

## Checks

Export `CARGO_BUILD_JOBS` for every cargo command. For your packages, run:

- `cargo fmt`
- `cargo clippy --locked -p <pkg> --all-targets --all-features -- -D warnings`
- the wasm32 Clippy line from `just lint`, if your packages build for wasm32
- `cargo nextest run --locked --all-features -p <pkg>` over every test
  binary that covers code you changed
- `just openapi-check`, if you touched anything the generated files are built
  from
- `just setup-playwright`, then `just test-playwright` and
  `just docs-screenshots --update-snapshots=none`, if you touched the
  dashboard, its specs or the daemon's HTTP API
- `just docker-test`, if you touched anything under `crates/`, `apps/piqueld`
  or `tools/xtask`

Draft CI skips the Playwright and Docker suites, so run them here. Don't run
`just validate`, `just lint` or `just test`: they build the whole workspace.

Prove each "unchanged" claim against the PR's base: `--help` output, specs,
query plans, error codes, the NixOS module's output. Before you call a failing
test unrelated, check that it fails on `main` too. The `fake_docker` timing
tests and Playwright's `branches.spec.ts` flake under load. Rerun them once,
and never change a test just to make it pass.

## Delivery

- Split your work into focused PRs: one kind of cleanup in one part of your
  area each, small enough to review in one sitting. Branch each from a
  freshly fetched `origin/main` as `<prefix><topic>`, and keep your PRs from
  sharing files. If one needs another, stack it: branch from the other PR's
  branch, open it with that branch as its base, and say so in its
  description.
- Make small, logical commits with conventional messages and no
  `Co-Authored-By` line.
- Rebase on `origin/main` (a stacked PR on its base branch), and clean up any
  new code the rebase brings in. PRs merge by squash, so when a stacked PR's
  base merges, move only its own commits with
  `git rebase --onto origin/main <old base tip>`, and check it targets
  `main`.
- Open each PR as a **draft** with `file-pr`, despite its default, titled
  `refactor(<scope>): <what got simpler>`. Its description says:
  - `Part of PQD-<n>`
  - what was hard to read and how you fixed it
  - every test you merged or removed
  - what you compared to prove the rest unchanged
- Don't mark a PR ready.

Your final message gives:

1. The PR links, in merge order, with what each one cleans up.
2. The checks you ran for each, with their results.
3. Bugs: `file:line`, what goes wrong and how to reproduce it.
4. Issues that cross areas, and other design issues: `file:line`, what's
   wrong, why it matters and a fix.

Check every item against the source before you report it. When you get review
findings, fix them on that PR's branch, rerun the affected checks, push, and
report what changed.
