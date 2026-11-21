# piqueld

A pure Rust infrastructure control plane for a single-node Docker Swarm: the
`piqueld` daemon, the `piquelctl` CLI, and the Leptos dashboard. See
`docs/README.md` for the documentation index.

## Glossary

The user guide's [concepts](docs/user/introduction.md#concepts) define the
terms; these are the easiest to mix up:

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
  images of their own.
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

Follow `docs/dev/rust-conventions.md`, which applies the shared
[engineering standard](https://gist.github.com/PiquelChips/04330e52c03efbe9ce4c889c88673a27)
to piqueld. `just lint` enforces its lints, and `just boundary` the crate
boundaries.

Read a dependency's real source in `~/.cargo/registry/src`, at the version in
`Cargo.lock`, rather than trusting memory of a fast-moving crate's API, such as
Leptos, axum or Progenitor.

If a rule in this file fights the task, say so plainly and get the owner's sign-off
before breaking it.

## Validation

`just validate` (or `just` alone) formats the code and runs every local check
CI runs, which `docs/dev/validation.md` lists. Run it before handing work back;
while iterating, run only what you changed, such as `just test -p <crate>` and
`just lint`. If a step cannot run, report the exact command and why.

## Testing

The `piqueld-dev` skill in `.agents/skills` (linked from `.claude/skills`)
runs this worktree's isolated development instance (`just dev`, see
`docs/dev/development.md`) on its own localhost port, for testing changes through
the manifest, CLI, and dashboard preview.

## Issues

Issues live in Linear, in the `PQD` team of the `sourceorigin` workspace, which
also holds `sourceorigin-dev/infra`. Read, create and update them through the
Linear MCP server. New issues go to Triage with a Type label (Feature, Bug, Task,
Security or Epic) and a title following the commit convention below.

GitHub issues are only for outside reports. Linear copies new ones into Triage,
so never triage or close them on GitHub.

## Commits & Pull Requests

Titles should follow the `feat(server): add docker swarm reconciliation` convention.
Keep pull requests small and focused: one change each, which a reviewer can
read in one sitting. Split refactors from behaviour changes, and stack
dependent pull requests, each on the last.
A pull request names its Linear issue in the description: `Fixes PQD-123`
closes the issue when it merges, and `Part of PQD-123` only links it.

Open pull requests as drafts and mark them ready for review once the work is
done. Drafts run only the quick CI jobs; marking a pull request ready runs the
slow ones (Docker, Playwright, macOS), which it must pass
before merging. Release builds (Nix, packaging, install) run after merging; see
"Release checks" in `docs/dev/validation.md` to run them on a branch that
changes a manifest, build script, the flake or `install.sh`.

`main` merges through a merge queue: `gh pr merge` adds a pull request to it,
and the queue reruns CI on top of the latest main. Never merge with `--admin`.

`docs/dev/releasing.md` describes versions and releases.

## Surfaces

A feature reaches every surface it applies to. Before handing back a feature,
walk this list and say which entries applied:

- **Manifest**: its types in `piqueld-core`.
- **HTTP API**: the daemon's endpoint, the permission it requires
  (`docs/dev/architecture/authorization.md`), and the OpenAPI document and
  generated client.
- **CLI**: a `piquelctl` command.
- **Dashboard**: `apps/piqueld-ui`.
- **Daemon configuration**: `piqueld-config`, and the NixOS module,
  `nix/module.nix` (`nix/cli-module.nix` for `piquelctl` profiles).
- **Documentation**: each surface in the user guide, and its screenshots.

`just generate` writes the OpenAPI document, the client, the JSON Schemas and
the manifest, configuration and command references, and `just openapi-check`
fails while one is stale.

A way in needs a way out and a way to see it: create needs delete, enable
needs disable, pause needs resume, and each state shows in `piquelctl` and the
dashboard.

## Documentation

The documentation in `docs/` is an mdBook with a user guide (`docs/user`) and a
developer guide (`docs/dev`). Update it in the same pull request as the change
it describes, following `docs/dev/documentation.md`, and run `just docs` to
build it and check its links. The `piqueld-docs` skill in `.agents/skills`
points agents to that guide.
