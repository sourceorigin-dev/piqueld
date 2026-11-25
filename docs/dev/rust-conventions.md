# Rust conventions

piqueld follows the shared engineering standard,
[sourceorigin-standard](https://crates.io/crates/sourceorigin-standard), also
used by SourceOrigin and DirkEngine. `just standard` checks its rules, and
`standard.toml` lists where piqueld departs from one, with its reason. This
page holds what piqueld adds that no rule checks.
Keep implementations simple, follow neighbouring modules, and share repeated
logic instead of copying it.

## Crates and boundaries

[Module boundaries](architecture/dependency-flow.md) describes each crate and
what it may depend on, and `just boundary` checks it. Shared contracts live in
`piqueld-core`, which knows no server, database, Docker or browser framework.
`piqueld-client` depends on no workspace crate but `piqueld-core` and
`piqueld-probe`, and on none of those frameworks, except the server framework
its tests use. The daemon's crates (`piqueld-config`, `piqueld-store`,
`piqueld-docker`) don't depend on the ones they build alongside. A new dependency between
crates updates both that page and `tools/xtask/src/boundary.rs`.

A feature reaches every surface it applies to, from the checklist in
`AGENTS.md`.

## Errors

Library crates return explicit `thiserror` errors, with their source kept by
`#[source]` or `#[from]`. Only a crate without a library, such as `xtask`,
uses `anyhow`, with `.context` on each step, as the standard's
[`anyhow-edges`](https://crates.io/crates/sourceorigin-standard)
rule says. Don't drop a source with `map_err(|_| ...)` unless it could hold a
secret, and say so in the reason of its
`#[expect(clippy::map_err_ignore, reason = "...")]`.
Log an error with `error = %error`, its `Display`, as the daemon does.

Every machine-readable code the API returns is a variant of
`piqueld_core::ApiErrorCode`: the `code` of an error response, and those of
validation errors, compilation errors and plan and status diagnostics. Never
write a code as a string. A new code is a new variant whose doc comment says
when the API returns it, and an existing variant's string never changes,
because clients match on it. Wire types keep `code` a `String`, so a client
still reads a code it doesn't know yet; compare one with
`ApiErrorCode::parse` or `==`. Operation diagnostics recorded in history have
their own `DiagnosticCode`.

Other strings that cross crates are named the same way, as a `string_enum!`
in `piqueld-core`: the kind of each history event is an `EventKind`, the name
of each message a live server-sent event stream sends a `StreamMessage`, and
the status a device login poll reports a `DeviceLoginStatus`. Record and
compare one through its variant, never its string, and in SQL bind it as a
parameter. Migrations can't name an `EventKind`, so a store test checks that
every kind the schema spells out, in an index or a view, is one. A wire field
that holds one stays a `String`, so a client can still decode values a newer
daemon adds, and history the kinds older daemons recorded: compare
`Event.kind` with `Event::known_kind` or `==`, and `DeviceToken.status` with
`DeviceLoginStatus::parse` or `==`. A stream reader likewise skips message
names it doesn't know.

Production code never panics without saying why: `expect_used`, `panic`,
`todo`, `unimplemented` and `dbg_macro` are denied outside tests. Return an
error when the input can be wrong. An invariant the types can't express keeps
its `expect`, whose message says what holds, under an
`#[expect(clippy::expect_used, reason = "...")]` on the smallest item around
it, such as `reason = "The placeholder IDs are valid constants."`. Tests may
panic; integration test crates say so once, in an `#![expect]` at their root.

Library crates never print: `print_stdout` and `print_stderr` are denied outside
tests. A library returns the text or logs it with `tracing`; binaries and
examples print, under an `#![expect]` or `#[expect]` that says so.

## Lint exceptions

Never `#[allow]`: an exception is `#[expect(lint, reason = "...")]`, which
fails once the code no longer needs it, and the reason says why the lint is
wrong there. Generated code is excepted once, by its generator: the OpenAPI
generator writes the client's header, which excepts Progenitor's own
`#[allow]`s.

## Types

Validate input once, where it becomes a value: `ApplicationId::parse`,
`EnvironmentId`, the manifest's `ValidatedTemplate`. Code that takes the
validated type never checks it again. Match domain enums exhaustively, so a
new variant fails to compile until every consumer handles it. Behaviour
belongs on the type that owns it, as a method; free functions are for small
helpers with no owner.

## Dependency pins

The workspace's `Cargo.toml` pins these for reasons it cannot enforce:

- Published crates carry the version they are published with.
  `piqueld-generated` isn't published, so the crates that use it as a
  development dependency publish without it.
- `rust-version` is the oldest Rust the crates build with, which the `msrv` CI
  job checks. Raise it when code needs a newer Rust; Cargo then prefers
  dependency versions that support it when updating the lockfile.
- `documentation` is set for the daemon and CLI, which link the book; the
  library crates leave it unset, so crates.io links their documentation on
  docs.rs.
- `opentelemetry-proto`'s generated clients export telemetry over OTLP/gRPC,
  rather than `opentelemetry-otlp`'s exporters, because the daemon queues
  records per destination itself. Tonic channels serve TCP, TLS (rustls on
  ring, like reqwest) and Unix sockets alike.
- `webauthn-rs-core` is exact, so a change to how passkeys are verified only
  arrives through a deliberate, reviewed bump: piqueld uses this low-level
  crate directly, without the checks `webauthn-rs` adds on top.
- `js-sys`, `wasm-bindgen`, `web-sys` and their companions are exact pins:
  `web-sys` patch releases regenerate bindings procedurally and move in
  lockstep with `js-sys` and `wasm-bindgen`, so one tested combination stays
  frozen until bumped deliberately. `wasm-bindgen-cli-support`, which the
  daemon build script binds the dashboard with, must match the `wasm-bindgen`
  crate exactly.
- `[profile.dev]` keeps line tables, which preserve file and line numbers in
  backtraces but drop the variable and type information that makes up most of
  a debug build's output; tests inherit it.
- `[profile.dashboard]` builds the embedded dashboard for wasm32 from the
  daemon build script, which strips host `CARGO_PROFILE_*` overrides so these
  settings alone apply. Size beats speed: it halves the gzipped module compared
  with release settings plus `wasm-opt -Oz`, so no separate wasm optimizer is
  needed.

## Tests and docs

Tests prove one behaviour each, named for it, and assert the outcome;
[automated validation](validation.md) lists the suites and what each covers.

Every item has a doc comment, which says what goes in and what comes out or
happens. Public items' `///` comments on the manifest, configuration and CLI
types are also the generated references, so keep them about what the reader
can see and do ([changing the documentation](documentation.md)). The only `//`
comments are the markers the standard's
[`rust-comments`](https://crates.io/crates/sourceorigin-standard) rule
allows. `just doc` builds the docs, private items included, with warnings
denied.

## Deviations

- `just doc` allows links from public items to private ones
  (`rustdoc::private_intra_doc_links`), since most of the code is internal and
  documented for its developers.
