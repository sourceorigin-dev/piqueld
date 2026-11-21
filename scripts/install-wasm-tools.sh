#!/usr/bin/env bash
# Install the CLI and browser-test runner matching Cargo.lock into the CI cache.
set -euo pipefail

# The release archive's SHA-256, pinned here rather than read from the same
# release, so a replaced archive fails the check. Bumping wasm-bindgen in
# Cargo.lock means updating both.
pinned_version=0.2.126
pinned_sha256=064948d58e2d6c0a745216477a639ba696216d6309aaa902939d1b865b1d869d

version="$(awk '$0 == "name = \"wasm-bindgen\"" { getline; gsub(/"/, "", $3); print $3; exit }' Cargo.lock)"
[[ "$version" == "$pinned_version" ]] || {
  echo "Cargo.lock has wasm-bindgen $version, but $0 pins $pinned_version: update its version and checksum" >&2
  exit 1
}
bindir="${RUNNER_TEMP:?}/piqueld-tools/wasm-bindgen-$version"
if [[ ! -x "$bindir/wasm-bindgen" || ! -x "$bindir/wasm-bindgen-test-runner" ]]; then
  download="$(mktemp -d "${RUNNER_TEMP}/wasm-bindgen.XXXXXXXX")"
  trap 'rm -rf "$download"' EXIT
  archive="wasm-bindgen-$version-x86_64-unknown-linux-musl.tar.gz"
  release="https://github.com/wasm-bindgen/wasm-bindgen/releases/download/$version"
  curl --fail --location --silent --show-error "$release/$archive" -o "$download/$archive"
  printf '%s  %s\n' "$pinned_sha256" "$archive" | (cd "$download" && sha256sum --check --strict -)
  tar -xzf "$download/$archive" -C "$download"
  mkdir -p "$bindir"
  install -m755 "$download/wasm-bindgen-$version-x86_64-unknown-linux-musl/wasm-bindgen" "$bindir/"
  install -m755 "$download/wasm-bindgen-$version-x86_64-unknown-linux-musl/wasm-bindgen-test-runner" "$bindir/"
fi
printf '%s\n' "$bindir" >> "${GITHUB_PATH:?}"
