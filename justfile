# Validation checks sources and generated artifacts without modifying them.
default: validate

validate: fmt-check lint-ci lint check test doc doc-test audit unused openapi-check boundary check-wasm test-playwright docs

build:
    @cargo build --workspace --locked

# Native CLI validation on Linux and macOS, including its shared contracts.
validate-cli:
    @cargo clippy --locked --package piquelctl --package piqueld-client --package piqueld-core --all-targets --all-features -- -D warnings
    @cargo nextest run --locked --package piquelctl --package piqueld-client --package piqueld-core
    @cargo test --locked --doc --package piqueld-client --package piqueld-core

build-cli:
    @cargo build --release --locked --package piquelctl

[positional-arguments]
run *ARGS:
    @cargo run --package piquelctl -- "$@"

[positional-arguments]
daemon *ARGS:
    @cargo run --package piqueld --bin piqueld -- "$@"

fmt:
    @cargo fmt --all

# Checks formatting without modifying sources; `fmt` applies it.
fmt-check:
    @cargo fmt --all -- --check

# Lints the GitHub workflows, running shellcheck over their `run` steps, and
# the shell scripts.
lint-ci:
    @actionlint
    @shellcheck scripts/*.sh apps/site/install.sh

# The native build compiles the browser code out, so the dashboard and client
# are linted as wasm32 too. Without --no-deps this also lints their workspace
# dependencies (piqueld-core) as wasm32. The workspace pass unifies features
# across crates, so a crate can compile there only because another crate
# enables a feature it needs: linting each crate alone catches that.
# Clippy with warnings denied: the workspace, wasm32, and each crate alone.
# `--package <crate>` arguments (or `-p <crate>`) narrow it to those crates:
# the workspace pass lints them together, each is linted alone, and the
# wasm32 pass runs only if they include the dashboard or the client.
lint *ARGS:
    #!/usr/bin/env bash
    set -euo pipefail
    packages=() wasm=false
    set -- {{ARGS}}
    while (( $# > 0 )); do
        case $1 in
            --package=*) packages+=("${1#--package=}") ;;
            --package | -p) packages+=("${2:?$1 needs a crate}"); shift ;;
            *) echo "lint takes only --package <crate> arguments, not $1" >&2; exit 2 ;;
        esac
        shift
    done
    if (( ${#packages[@]} == 0 )); then
        together=(--workspace) alone=() wasm=true
        for manifest in apps/*/Cargo.toml crates/*/Cargo.toml tools/*/Cargo.toml; do
            alone+=("--manifest-path=$manifest")
        done
    else
        together=("${packages[@]/#/--package=}") alone=("${together[@]}")
        for package in "${packages[@]}"; do
            case $package in piqueld-ui | piqueld-client) wasm=true ;; esac
        done
    fi
    cargo clippy --locked "${together[@]}" --all-targets --all-features -- -D warnings
    if $wasm; then
        cargo clippy --locked --target wasm32-unknown-unknown --package piqueld-ui --package piqueld-client -- -D warnings
    fi
    for crate in "${alone[@]}"; do
        cargo clippy --locked "$crate" --all-targets -- -D warnings
    done

check:
    @cargo check --locked --workspace --all-targets

# Compiles the browser transport, which no host-target recipe reaches.
check-wasm:
    @rustup target add wasm32-unknown-unknown
    @cargo check --locked --package piqueld-client --all-targets --target wasm32-unknown-unknown

# Runs focused transport tests in a headless browser. The development shell
# must provide wasm-bindgen-test-runner and a supported WebDriver.
test-wasm:
    @rustup target add wasm32-unknown-unknown
    @WASM_BINDGEN_TEST_ONLY_WEB=1 CARGO_TARGET_WASM32_UNKNOWN_UNKNOWN_RUNNER=wasm-bindgen-test-runner cargo test --locked --package piqueld-client --lib --target wasm32-unknown-unknown

# All features, like lint: the daemon tests run against the real embedded
# bundle, and lint, test, and doc-test share one build. ARGS go on to nextest,
# such as `--package piquelctl`, in place of `--workspace`.
test *ARGS='--workspace':
    @cargo nextest run --locked --all-features {{ARGS}}

# ARGS, such as `--package piqueld-core`, replace `--workspace`.
doc-test *ARGS='--workspace':
    @cargo test --locked --doc --all-features {{ARGS}}

# Runs `test` under coverage instrumentation, writes an HTML report to
# target/llvm-cov/html, and prints each crate's totals. ARGS go on to nextest.
# Not part of `validate`; see docs/dev/validation.md for what it leaves out.
[positional-arguments]
coverage *ARGS:
    @cargo xtask coverage "$@"

# Advisories, licenses, bans and sources, as deny.toml configures them.
audit:
    @cargo deny check

# Dependencies a crate declares but never names.
unused:
    @cargo machete

# Private items are documented, since most of the code is internal and its docs
# link to it.
# Rustdoc warnings, such as broken intra-doc links, fail the build. ARGS, such
# as `--package piqueld-core`, replace `--workspace`.
doc *ARGS='--workspace':
    @RUSTDOCFLAGS='-D warnings -A rustdoc::private_intra_doc_links' cargo doc --locked --no-deps --all-features --document-private-items {{ARGS}}

# Check generated artifacts against freshly generated endpoint, manifest,
# configuration, and command-line metadata.
openapi-check:
    @cargo run --package piqueld --example generate_openapi -- --check
    @cargo run --package piquelctl --example cli_reference -- --check

boundary:
    @cargo xtask boundary

# Reads CI's path filters in .github/filters.yml, runs the Rust checks on the
# changed crates and those depending on them, and names the slow checks it
# leaves to CI; see docs/dev/validation.md.
# CI's quick checks for what changed since the merge base with origin/main, or
# the branch given, counting uncommitted and untracked files.
[positional-arguments]
check-changed *ARGS:
    @cargo xtask check-changed "$@"

# Publishes the crates to crates.io, with the prebuilt dashboard the published
# daemon embeds; see docs/dev/releasing.md. ARGS go on to `cargo publish`,
# such as `--dry-run`.
[positional-arguments]
publish *ARGS:
    @cargo xtask publish "$@"

# Builds the static release archive the install script installs, and its
# checksums, into target/dist; see docs/dev/releasing.md. Needs musl-gcc.
dist:
    @cargo xtask dist

# Releases the workspace version from the latest main: publishes the crates,
# tags the commit, from which CI creates its GitHub release, and closes its
# milestone; see docs/dev/releasing.md. `--dry-run` runs the checks and
# `cargo publish --dry-run`, and only prints the other steps.
[positional-arguments]
release *ARGS:
    @cargo xtask release "$@"

# UI checks and embedded builds need only the wasm32 target from
# rust-toolchain.toml: the daemon build script compiles and binds the dashboard
# itself. Default all-feature Clippy builds the embedded dashboard, and
# Playwright validation builds it through its Rust fixture too.
ui-check:
    @cargo check --locked --target wasm32-unknown-unknown --package piqueld-client --package piqueld-ui

# Release daemon and CLI; the daemon build script compiles the dashboard.
# Build the shipped binaries together so Cargo can share dependencies and schedule both.
# The features are the daemon's: `piqueld/embedded-ui` would name the feature of
# the daemon's dev-dependency on itself, which this build leaves out.
build-embedded:
    @cargo build --release --package piqueld --package piquelctl --bin piqueld --bin piquelctl --features embedded-ui --locked

[positional-arguments]
daemon-embedded *ARGS:
    @cargo run --package piqueld --bin piqueld --features embedded-ui -- "$@"

# Isolated development instance for this worktree (docs/dev/development.md).
[positional-arguments]
dev *ARGS:
    @cargo xtask dev "$@"

# T3 Code runs this when it creates a worktree (t3.json). While it works,
# target/worktree-setup/running holds its PID and output.log its output; it
# then leaves `done`, or `failed` holding the end of the log. It holds an
# exclusive lock on target/worktree-setup/lock throughout, so a second run
# refuses to start, and `flock <lock> true` waits for it. `running` is written
# before the old markers go and removed after the new one is written, so a
# reader always finds one.
# Prepares a new worktree: its development configuration and a workspace build.
setup-worktree:
    #!/usr/bin/env bash
    set -euo pipefail
    markers=target/worktree-setup
    mkdir -p "$markers"
    exec 9> "$markers/lock"
    if ! flock -n 9; then
        echo "worktree setup is already running: wait with \`flock $markers/lock true\`" >&2
        exit 1
    fi
    echo $$ > "$markers/running"
    rm -f "$markers/done" "$markers/failed" "$markers/output.log"
    status=0
    # T3 Code often creates several worktrees at once: build at low priority on
    # half the cores, unless CARGO_BUILD_JOBS says otherwise.
    export CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-$(( ($(getconf _NPROCESSORS_ONLN) + 1) / 2 ))}"
    # Nothing else inherits the lock, so it ends with this script.
    { just dev config && nice -n 10 just build; } 9>&- 2>&1 | tee "$markers/output.log" 9>&- || status=$?
    if [ "$status" -eq 0 ]; then
        touch "$markers/done"
    else
        tail -n 40 "$markers/output.log" > "$markers/failed.tmp"
        mv "$markers/failed.tmp" "$markers/failed"
    fi
    rm -f "$markers/running"
    exit "$status"

# piquelctl from this worktree, connected to its development instance.
[positional-arguments]
ctl *ARGS:
    @cargo xtask dev ctl "$@"

# Generate the OpenAPI document and client, the manifest and configuration JSON
# Schemas, and the configuration reference page together, then the CLI
# reference page, whose generator builds on the regenerated client.
generate:
    @cargo run --package piqueld --example generate_openapi
    @cargo run --package piquelctl --example cli_reference

# Builds the documentation book into target/book, failing on any mdBook
# warning, then checks every link and heading anchor in it and in the
# repository's Markdown files, offline (see lychee.toml). Links to this
# repository on GitHub are checked against the checkout; the second remap
# fails those that don't point at main, which would otherwise go unchecked.
# CI runs this on every pull request; see docs/dev/documentation.md.
docs:
    #!/usr/bin/env bash
    set -euo pipefail
    if ! log=$(mdbook build 2>&1) || grep -q WARN <<<"$log"; then
        echo "$log" >&2
        exit 1
    fi
    repository='https://(github\.com/sourceorigin-dev/piqueld/(blob|tree)|raw\.githubusercontent\.com/sourceorigin-dev/piqueld)'
    git ls-files -z --cached --others --exclude-standard '*.md' \
        | xargs -0 lychee --root-dir "{{justfile_directory()}}/target/book" \
            --remap "$repository/main/ file://{{justfile_directory()}}/" \
            --remap "$repository/ file:///links-to-this-repository-must-point-at-main/" \
            target/book

# Serves the documentation book on localhost, rebuilding it on every change.
[positional-arguments]
docs-serve *ARGS:
    @mdbook serve "$@"

# Runs the Docker integration tests against a throwaway Docker-in-Docker engine.
# ARGS select a suite, `ingress` or `engine`, a shard of it, such as
# `engine --partition count:1/2`, and the engine's image store, such as
# `--store containerd`; see `cargo xtask docker-test --help`.
[positional-arguments]
docker-test *ARGS:
    @cargo xtask docker-test "$@"

# Explicit development-only setup: pinned JS packages and a containerized browser.
setup-playwright:
    @pnpm --dir tests/playwright install --frozen-lockfile --ignore-scripts
    @docker pull "mcr.microsoft.com/playwright:v$(node -p 'require("./tests/playwright/package.json").devDependencies["@playwright/test"]')-noble"

# The Rust fixture serves the embedded UI/API with isolated state and no Docker backend.
[positional-arguments]
test-playwright *ARGS: build-playwright
    @cargo xtask playwright "$@"

# Regenerates the dashboard screenshots in docs/images whose page changed,
# through the browser suite's fixture with deployments to its fake Docker
# engine. With `--update-snapshots=none`, fails on an outdated one instead:
# see docs/dev/documentation.md.
[positional-arguments]
docs-screenshots *ARGS: build-playwright
    @cargo xtask playwright --config screenshots.config.ts "$@"

# Builds what the browser runs need: the CLI and the Rust fixture, and
# type-checks the Playwright specs.
[private]
build-playwright:
    @cargo build --locked --package piquelctl
    @cargo build --locked --package piqueld --features embedded-ui --example browser_fixture
    @pnpm --dir tests/playwright check
