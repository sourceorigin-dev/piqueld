# CI's quick checks for only what changed; `validate` runs everything.
default: check-changed

# Every local check CI runs, without modifying files.
validate: fmt-check lint-ci standard lint check test doc doc-test audit unused openapi-check boundary check-wasm test-playwright docs

build:
    @cargo build --workspace --locked

# Native CLI validation on Linux and macOS.
validate-cli:
    @cargo clippy --locked --package piquelctl --package piqueld-client --package piqueld-core --all-targets --all-features -- -D warnings
    @cargo nextest run --locked --package piquelctl --package piqueld-client --package piqueld-core
    @cargo test --locked --doc --package piqueld-client --package piqueld-core

build-cli:
    @cargo build --release --locked --package piquelctl

[positional-arguments]
run *ARGS:
    @cargo run --locked --package piquelctl -- "$@"

[positional-arguments]
daemon *ARGS:
    @cargo run --locked --package piqueld --bin piqueld -- "$@"

fmt:
    @cargo fmt --all

# Checks formatting without modifying sources; `fmt` applies it.
fmt-check:
    @cargo fmt --all -- --check

# Lints the GitHub workflows and the shell scripts.
lint-ci:
    @actionlint
    @shellcheck scripts/*.sh apps/site/install.sh

# Clippy, warnings denied: workspace, wasm32 and each crate alone.
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

# Runs the transport tests in a headless browser (needs a WebDriver).
test-wasm:
    @rustup target add wasm32-unknown-unknown
    @WASM_BINDGEN_TEST_ONLY_WEB=1 CARGO_TARGET_WASM32_UNKNOWN_UNKNOWN_RUNNER=wasm-bindgen-test-runner cargo test --locked --package piqueld-client --lib --target wasm32-unknown-unknown

# Runs nextest; ARGS, such as `--package piquelctl`, replace `--workspace`.
test *ARGS='--workspace':
    @cargo nextest run --locked --all-features {{ARGS}}

# ARGS, such as `--package piqueld-core`, replace `--workspace`.
doc-test *ARGS='--workspace':
    @cargo test --locked --doc --all-features {{ARGS}}

# Runs `test` under coverage, reporting to target/llvm-cov/html.
[positional-arguments]
coverage *ARGS:
    @cargo xtask coverage "$@"

# Advisories, licenses, bans and sources (deny.toml).
audit:
    @cargo deny check

# Dependencies a crate declares but never names.
unused:
    @cargo machete

# Rustdoc, private items included; ARGS replace `--workspace`.
doc *ARGS='--workspace':
    @RUSTDOCFLAGS='-D warnings -A rustdoc::private_intra_doc_links' cargo doc --locked --no-deps --all-features --document-private-items {{ARGS}}

# Checks the generated artifacts against freshly generated ones.
openapi-check:
    @cargo run --locked --package piqueld --example generate_openapi -- --check
    @cargo run --locked --package piquelctl --example cli_reference -- --check

boundary:
    @cargo xtask boundary

# The engineering standard's rules; `cargo xtask standard rules` lists them.
standard:
    @cargo xtask standard

# CI's quick checks for what changed since the merge base with origin/main, or
# the branch given.
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

# Type-checks the dashboard and client for wasm32.
ui-check:
    @cargo check --locked --target wasm32-unknown-unknown --package piqueld-client --package piqueld-ui

# Release daemon and CLI with the dashboard embedded.
build-embedded:
    @cargo build --release --package piqueld --package piquelctl --bin piqueld --bin piquelctl --features embedded-ui --locked

[positional-arguments]
daemon-embedded *ARGS:
    @cargo run --locked --package piqueld --bin piqueld --features embedded-ui -- "$@"

# Isolated development instance for this worktree (docs/dev/development.md).
[positional-arguments]
dev *ARGS:
    @cargo xtask dev "$@"

# Prepares a new worktree; T3 Code runs it (t3.json).
setup-worktree:
    #!/usr/bin/env bash
    set -euo pipefail
    # Worktrees are often created together: build on half the CPUs unless
    # `build.jobs` is set (`config get` is unstable, hence RUSTC_BOOTSTRAP).
    if ! RUSTC_BOOTSTRAP=1 cargo -Zunstable-options config get build.jobs >/dev/null 2>&1; then
        export CARGO_BUILD_JOBS="$(( ($(nproc 2>/dev/null || getconf _NPROCESSORS_ONLN 2>/dev/null || echo 1) + 1) / 2 ))"
    fi
    just dev config
    nice -n 10 just build

# piquelctl from this worktree, connected to its development instance.
[positional-arguments]
ctl *ARGS:
    @cargo xtask dev ctl "$@"

# Generates the OpenAPI document, client, schemas and reference pages.
generate:
    @cargo run --locked --package piqueld --example generate_openapi
    @cargo run --locked --package piquelctl --example cli_reference

# Builds the documentation book and checks its links offline.
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

# Docker integration tests on a throwaway engine; see `docker-test --help`.
[positional-arguments]
docker-test *ARGS:
    @cargo xtask docker-test "$@"

# Development-only setup: pinned JS packages and a containerized browser.
setup-playwright:
    @pnpm --dir tests/playwright install --frozen-lockfile --ignore-scripts
    @docker pull "mcr.microsoft.com/playwright:v$(node -p 'require("./tests/playwright/package.json").devDependencies["@playwright/test"]')-noble"

[positional-arguments]
test-playwright *ARGS: build-playwright
    @cargo xtask playwright "$@"

# Regenerates the dashboard screenshots in docs/images.
[positional-arguments]
docs-screenshots *ARGS: build-playwright
    @cargo xtask playwright --config screenshots.config.ts "$@"

[private]
build-playwright:
    @cargo build --locked --package piquelctl
    @cargo build --locked --package piqueld --features embedded-ui --example browser_fixture
    @pnpm --dir tests/playwright check
