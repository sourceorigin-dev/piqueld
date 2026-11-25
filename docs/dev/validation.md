# Automated validation

CI runs on GitHub-hosted runners. Jobs group the recipes that compile the same
code, so each job builds the workspace once, and run in parallel. Pull requests
run only the jobs whose inputs changed, as the path filters in
`.github/filters.yml` list them, and drafts only the quick ones: Docker,
Playwright, and macOS jobs wait until a pull request is marked ready. Release
builds take too long to gate a merge, so they run after it, in the
[release checks](#release-checks).
The `standard` and `docs` jobs run on every pull request. Pull requests merge
through a merge queue, which tests each one on top of the latest main; merge
queue entries run every job but macOS, whose scarce runners would hold up the
queue. The commit the queue tested is the one that lands, so pushes to main
run no CI. A nightly run on main runs every job, macOS included; while it fails, an issue titled
*Nightly CI fails on main* stays open, as for the
[release checks](#release-checks). The `ci` job summarizes them: it fails if
any job failed. In the merge queue, the first failed job cancels the rest of
the run, so the entry leaves the queue without waiting for the slowest job.
Pull requests and nightly runs finish every job, to show every failure.

Rust build caches are scoped to jobs and saved by the nightly run on main;
pull requests and the merge queue restore them, so after a dependency change
merges they rebuild dependencies until that night. Dashboard tools are cached
by version.

| Capability | Local recipe | CI job |
| --- | --- | --- |
| Rust formatting | `fmt-check` | `checks` |
| Workflows, with shellcheck over their `run` steps, and shell scripts | `lint-ci` | `checks` |
| Dependency policy and advisories | `audit` | `checks` |
| Unused dependencies | `unused` | `checks` |
| The [engineering standard](https://crates.io/crates/sourceorigin-standard)'s rules, as `standard.toml` and `budget.toml` apply them | `standard` | `standard` |
| All-target, all-feature compilation and Clippy, plus wasm32 Clippy of the dashboard, client, and core, and Clippy of each crate alone | `lint` | `rust (lint)` |
| Rust documentation, private items included, with warnings denied | `doc` | `rust (lint)` |
| Native workspace tests, against the real embedded dashboard | `test` | `rust (test)` |
| Documentation tests | `doc-test` | `rust (test)` |
| Generated API specification, client, JSON Schemas, and command and settings references freshness | `openapi-check` | `rust (lint)` |
| Crate dependency boundaries | `boundary` | `rust (lint)` |
| Published crates on the minimum supported Rust, `rust-version` | `cargo +<rust-version> check` | `msrv` |
| Browser compilation | `ui-check`, `check-wasm` | `wasm-ui` |
| Real browser transport tests | `test-wasm` | `wasm-ui` |
| Isolated Engine integration tests | `docker-test` | `docker-integration` |
| Browser end-to-end tests | `test-playwright` | `browser-playwright` |
| Documentation screenshots match the dashboard | `docs-screenshots --update-snapshots=none` | `browser-playwright` |
| CLI on macOS | `validate-cli` | `cli (macOS)` |
| Documentation book build and link check | `docs` | `docs` |

The `rust` and `browser-playwright` jobs embed a dashboard module built once
per version of its sources, instead of compiling it each time; see
[production assets](architecture/dashboard.md#production-assets). The
*Dashboard cache* workflow builds main's module as soon as a merge changes its
sources, so the merge queue and pull requests find it.

`just` alone runs `check-changed`; `just validate` runs the full validation
set, including a single check of the generated files, and never modifies a
file. Both local
validation and CI compare the committed artifacts without rewriting them. CI
skips `check` and `build`: the stronger all-target Clippy and the test builds
cover them. `fmt` and `generate` are editing commands whose results are
checked above. `setup-playwright` installs the browser test dependencies, and
`setup-worktree` [prepares a new worktree](development.md#worktree-setup).
`run`, `ctl`, `daemon`, `daemon-embedded`, `dev`, and `docs-serve` launch
interactive processes and are not finite validation commands. `publish` and
`release` [release piqueld](releasing.md). `coverage`
measures the tests without checking a minimum; see
[test coverage](#test-coverage).

## Checking a change

Before handing work back, check what changed rather than the whole workspace:

```console
just check-changed
```

It runs the checks the quick CI jobs would run for the change on the current
branch: every path changed since the merge base with `origin/main`,
uncommitted and untracked files included. Name another branch to compare
against it instead, such as `just check-changed origin/release`.

It reads the path filters of CI's `changes` job from `.github/filters.yml`, so
it starts the jobs CI would start. In the Rust jobs, `lint`, `doc`, `test` and
`doc-test` run only on the changed crates and the crates depending on them,
passed as `--package=<crate>` arguments, and `openapi-check` runs only when
those include `piqueld` or `piquelctl`. A changed path in no crate, such as
`Cargo.lock`, runs them on the whole workspace. The slow checks, which drafts
skip too, and those that need tools a machine may lack, the oldest supported
Rust and a WebDriver, are left to CI: it names those the change needs, with
the command that runs each. A summary ends the run, with each check passed,
failed, skipped or left to CI and why, and the run fails if a check failed.

`tools/xtask/src/check_changed/ci.rs` maps the filters to the CI jobs and the
recipes they run, so change it together with `ci.yml`. A filter added to
`.github/filters.yml` fails `just check-changed` until it is added there too.

`just validate` builds the whole workspace: run it when a change is
workspace-wide, or when asked. CI owns the full suite.

## Release checks

The [release checks workflow](https://github.com/sourceorigin-dev/piqueld/blob/main/.github/workflows/release-checks.yml)
builds piqueld the way it is released. Each build takes up to ten minutes, so
these checks run after a pull request merges instead of before:

| Capability | Local recipe | Job |
| --- | --- | --- |
| Embedded daemon and CLI release builds | `nix build` | `nix packages` |
| The piqueld and piquelctl modules, in a VM: the daemon reaches Docker, serves the dashboard on localhost, and root runs `piquelctl status` over its socket | `nix build .#checks.x86_64-linux.piqueld` | `nix packages` |
| The build machine and build registry modules, in a VM: a rootless build pushed to the registry, an emulated arm64 build, and garbage collection | `nix build .#checks.x86_64-linux.piqueld-builder` | `nix packages` |
| Every published crate packaged and built from its package | `publish --dry-run` | `package` |
| The static release archive, installed and then updated by `install.sh` | `dist` | `install` |

They run on each push to `main` that changes what they build from, such as a
manifest, the lockfile, a build script, a migration, the toolchain, the flake,
the dashboard, the examples, `install.sh` or `xtask`. They also run every night at 04:00 UTC,
which catches the rest and what breaks without a commit, such as a new stable
Rust or a new runner image. Only the latest commit is checked: a push cancels
the run of the one before it.

While a check fails on `main`, an issue titled *Release checks fail on main*
stays open. The first failure opens it, each later one comments on it, and the
next run that passes closes it. Fix the failure before the next
[release](releasing.md#release-a-version).

Run a check before merging when a pull request changes what it builds: a
manifest, a build script, `flake.nix`, or `install.sh`. Run its local recipe,
or run the workflow on your branch from the Actions tab, which needs write
access to the repository:

```console
gh workflow run release-checks.yml --ref <branch>
```

The Nix job caches the store, including crane's dependency builds and the
dashboard. The `install` job's Rust cache is also the release workflow's.

## Docker test suites

Controller tests run against `FakeDocker`, an in-memory Docker engine in
`crates/piqueld-docker/src/fake.rs`. `just docker-test` runs two suites against
a real engine: a throwaway Docker-in-Docker daemon, never the host's. It
pulls Docker Hub images through Google's mirror, `mirror.gcr.io`, sparing the
anonymous Docker Hub rate limit that the CI runners' shared addresses hit, and
from Docker Hub when the mirror lacks one. Its own image comes from the mirror
alone. Images pinned by digest are still verified.

- `ingress` is the daemon library's `ingress_caddy` tests, against Caddy and
  an ACME test server in the engine.
- `engine` is `apps/piqueld/tests/docker_integration.rs`, against the engine's
  Swarm.

The tests of a suite share its engine's Swarm, so they run one at a time. Name
a suite to run only it, and add nextest's `--partition` to run a shard of it:

```console
just docker-test engine --partition count:1/2
```

CI runs three shards side by side, each against an engine of its own: the
`ingress` suite, and each half of the `engine` suite.

The engine keeps images in the classic image store, which engines upgraded
from older Docker releases still use. Fresh Docker 29 installs use the
containerd image store instead, and Docker reports images differently from
it. `--store containerd` runs the tests against an engine using it:

```console
just docker-test engine --store containerd
```

The scenarios in `apps/piqueld/tests/support/docker_contract.rs` check that the
fake behaves like the real engine. Two test binaries run each of them:
`fake_docker/main.rs` against the fake, in the `rust (test)` job, and
`docker_integration.rs` against the engine, in the `docker-integration` job.
When the controller starts relying on another engine behavior, add a scenario
for it.

When a scenario passes against the fake but fails against the engine, check
what Docker actually does. Then fix whichever is wrong: the fake, the adapter,
or the scenario.

## Test coverage

See which code the native workspace tests run:

```console
just coverage
```

The recipe runs the tests `just test` runs under
[cargo-llvm-cov](https://github.com/taiki-e/cargo-llvm-cov). It writes an HTML
report to `target/llvm-cov/html/index.html`, which marks every line of each
source file as run or not. Then it prints each crate's covered lines and
functions.

Arguments go on to nextest. On a busy machine, timing-sensitive tests can
fail, so run fewer at once and retry the ones that fail:

```console
just coverage --test-threads 8 --retries 2
```

`nix develop` provides cargo-llvm-cov, and `rust-toolchain.toml` adds the
`llvm-tools` it reads the results with. Without Nix, install it with
`cargo install --locked cargo-llvm-cov`. The instrumented build goes to
`target/llvm-cov-target`, so it doesn't replace your ordinary one.

CI doesn't run the recipe and enforces no minimum. The numbers leave out the
tests `just test` doesn't run:

- the Docker integration tests, which need an isolated engine
  (`just docker-test`).
- documentation tests (`just doc-test`).
- the browser transport tests (`just test-wasm`) and the Playwright browser
  tests (`just test-playwright`).

`piqueld-ui` counts only the modules that also compile natively, such as its
editor and terminal. Its browser code compiles only for wasm32, which
cargo-llvm-cov can't instrument, so the report doesn't list it.

The totals leave out every directory named `tests`, `examples` or `benches`,
but count other test code: `#[cfg(test)]` modules and test-only files such as
`piqueld-core/src/manifest/template_tests/`. That code always runs, so it
raises a crate's percentage. Stable Rust can't exclude it, so look at the
lines the HTML report marks as not run rather than at the percentages alone.
