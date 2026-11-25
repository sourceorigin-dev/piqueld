# piqueld

A pure Rust infrastructure control plane for a single-node Docker Swarm: the
`piqueld` daemon, the `piquelctl` CLI, and the Leptos dashboard. See
`docs/README.md` for the documentation index.

## Glossary

The user guide's [concepts](docs/user/introduction.md#concepts) define the
terms; the easiest to mix up:

- **Environment**: a running copy of an application (`production`,
  `staging`), and the unit piqueld deploys. A **preview** is a short-lived
  environment for a branch or local changes.
- **Service**: a manifest service, run as one Swarm service per environment.
  A **job**, declared under `spec.jobs`, runs a command to completion before
  a rollout, as a Swarm job service.
- **Deployment**: one version deployed to an environment. Its **release** is
  what it ran, down to the images, which promotion reuses. An **operation** is
  the daemon's record of work on an environment (`apply`, `refresh`,
  `delete`).
- **Route**: an HTTPS address whose destination is a service or a redirect.
  A notification **destination** is unrelated: an installation-wide webhook.
- **Link**: a private network connection from one application's services to
  a service another application exports, not a URL.
- **Host operator**: root or the daemon's own user, whom `piquelctl` over the
  daemon's socket acts as without signing in.
- **Instance**: a running daemon; a development instance is the one
  `just dev` runs for a worktree.

## Crates

- `piqueld` (`apps/piqueld`): the daemon, the binary and the embedded dashboard.
- `piquelctl` (`apps/piquelctl`) and `piqueld-ui` (`apps/piqueld-ui`): the CLI
  and the Leptos dashboard.
- `piqueld-core`: manifest, API and domain contracts, with no I/O.
- `piqueld-client`: typed HTTP client, with the generated endpoint bindings.
- `piqueld-config`: configuration, DNS provider accounts and state directories.
- `piqueld-store`: SQLite state, command acceptance and secret encryption.
- `piqueld-docker`: the Docker Engine and Swarm boundary, with a fake engine.
- `piqueld-probe`: the static in-container health probe.
- `piqueld-build`: nested Cargo builds for build scripts.
- `piqueld-generated`: writes the files `just generate` produces.
- `xtask` (`tools/xtask`): development tasks, run as `cargo xtask`.

Boundaries between them: `docs/dev/architecture/dependency-flow.md`.

## Hazards

This machine runs more than your worktree. Each hazard says why, so you can
recognise a case the list doesn't name.

- **The host's Docker Engine is shared**: it holds every worktree's engine
  (`piqueld-dev-*`) and other agents' containers, and on the server, the
  production piqueld that serves piqueld.dev. Never run `docker` against the
  default socket or context: pass `-H unix://<docker socket>`, the socket
  under `/tmp/piqueld-dev-<uid>/` that `just dev status` prints, which no
  `DOCKER_HOST` or `DOCKER_CONTEXT` overrides. `just dev`, `just docker-test`
  and the Playwright recipes use the host engine only for containers and
  images of their own. `just daemon` and `just daemon-embedded` run a piqueld
  on the engine its configuration names, the host's by default and in
  `examples/piqueld.toml`: use `just dev`.
- **Bare `piquelctl`, and `just run`, reach a real piqueld**: the default
  profile with your saved logins, which on the owner's machines is
  production, else the host daemon's socket. Use `just ctl`, which targets
  your instance.
- **`just dev clean` deletes your instance's data, engine and images**, which
  take minutes to rebuild. Run it only when asked.
- **Staging and production piqueld, tailnet node approvals, and credentials**
  (tokens, auth keys, passkeys, `~/.config/piqueld`) are the owner's. Don't
  deploy to them, approve nodes, or create, read or move credentials unless
  asked.
- **Stop only a process you started**, by the PID you captured, or the one
  listening on your own instance's port. Never kill by name or pattern
  (`pkill -f`, `killall`): your own agent process has this worktree's path in
  its arguments, and other worktrees run the same `piqueld`, `cargo` and
  `just` binaries.

## Conventions

Follow [sourceorigin-standard](https://crates.io/crates/sourceorigin-standard)
and `docs/dev/rust-conventions.md`, which applies it. `just standard` checks its
rules (the `standard` skill says what to do on a failure), `just lint` its
lints, and `just boundary` the crate boundaries.

Read a dependency's real source in `~/.cargo/registry/src`, at the version in
`Cargo.lock`, rather than trusting memory of a fast-moving crate's API, such as
Leptos, axum or Progenitor. If a rule in this file fights the task, say so plainly and get the owner's
sign-off before breaking it.

## Validation

Run `just check-changed` (or `just` alone) before handing work back: CI's
quick checks for what changed since the merge base with `origin/main`,
uncommitted files included, naming the slow checks it leaves to CI. While
iterating, run only the changed crate's checks, such as
`just test --package <crate>` and `just lint --package <crate>`.
`just validate` runs every local check on the whole workspace
(`docs/dev/validation.md` lists them; `just fmt` formats): run it for a workspace-wide change or
when asked. If a step cannot run, report the exact command and why. Under
load, the `fake_docker` timing tests and Playwright's `branches.spec.ts`
flake: rerun one once before calling it a failure. `just coverage` measures the
native tests' coverage. T3 Code prepares a new worktree with
`just setup-worktree`; run it yourself if the worktree has no
`piqueld.local.toml` or build yet.

## Testing

The `piqueld-dev` skill in `.agents/skills` (linked from `.claude/skills`)
runs this worktree's isolated development instance (`just dev`, `docs/dev/development.md`) on its own
localhost port, to test changes through the manifest, CLI and dashboard preview.

## Issues

Issues live in Linear (`PQD` team, `sourceorigin` workspace, which also holds
`sourceorigin-dev/infra`): use the Linear MCP server. New issues go to Triage
with a Type label (Feature, Bug, Task, Security or Epic) and a title following
the commit convention below. GitHub issues are only for outside reports, which
Linear copies into Triage: never triage or close them on GitHub.

## Commits & Pull Requests

Titles follow the `feat(server): add docker swarm reconciliation` convention.
Keep pull requests small and focused: one change a reviewer can read in one
sitting. Split refactors from behaviour changes, and stack dependent pull
requests. A pull request names its Linear issue in the description: `Fixes PQD-123`
closes the issue when it merges, and `Part of PQD-123` only links it.

Open pull requests as drafts and mark them ready for review once the work is
done: drafts run only the quick CI jobs, and ready runs the slow ones (Docker,
Playwright, macOS), which must pass before merging. Release builds run after
merging; "Release checks" in `docs/dev/validation.md` says when to run them on
a branch.

`main` merges through a merge queue: `gh pr merge` adds a pull request to it,
and the queue reruns CI on top of the latest main. Never merge with `--admin`.
`docs/dev/releasing.md` describes releases.

## Surfaces

A feature reaches every surface it applies to (manifest, HTTP API, CLI,
dashboard, daemon configuration, documentation). Before handing it back, walk
`docs/dev/surfaces.md` and say which entries applied. A way in needs a way out
and a way to see it; a refactor changes none of the surfaces.

## Documentation

`docs/` is an mdBook: a user guide (`docs/user`) and a developer guide
(`docs/dev`). Update it in the same pull request as the change, following
`docs/dev/documentation.md` and the `docs` skill, and run `just docs`.
