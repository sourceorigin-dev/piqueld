# piqueld

A pure Rust infrastructure control plane for a single-node Docker Swarm: the
`piqueld` daemon, the `piquelctl` CLI, and the Leptos dashboard. See
`docs/README.md` for the documentation index.

## Conventions

Follow `docs/dev/rust-conventions.md`, which applies the shared
[engineering standard](https://gist.github.com/PiquelChips/04330e52c03efbe9ce4c889c88673a27)
to piqueld.

## Validation

`just validate` (or `just` alone) formats the code and runs every local check
CI runs, which `docs/dev/validation.md` lists. Run it before handing work back;
while iterating, run only what you changed, such as `just test -p <crate>` and
`just lint`. If a step cannot run, report the exact command and why.

T3 Code prepares a new worktree with `just setup-worktree`, which leaves a
marker in `target/worktree-setup/`. Read `failed` first if it exists: it holds
the end of the setup's output, and `output.log` all of it; fix the cause and
run the recipe again. While `running` names a live process, wait in the
background with `flock target/worktree-setup/lock true`, which returns once
setup ends. With no marker, or a `running` whose process is gone, run
`just setup-worktree` yourself.

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

## Adding features

**piqueld** has three different surfaces to access its features: the manifest,
the CLI and the dashboard. When adding a feature to one, make sure it is
appropriately added to all of them.
For example, if we add a new setting for applications, that setting should be
present in the manifest and editable through the CLI and the dashboard.

When changing/adding to configuration, make sure to update the NixOS module.

## Documentation

The documentation in `docs/` is an mdBook with a user guide (`docs/user`) and a
developer guide (`docs/dev`). Update it in the same pull request as the change
it describes, following `docs/dev/documentation.md`, and run `just docs` to
build it and check its links. The `piqueld-docs` skill in `.agents/skills`
points agents to that guide.
