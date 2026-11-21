# Rust conventions

piqueld follows the shared
[engineering standard](https://gist.github.com/PiquelChips/04330e52c03efbe9ce4c889c88673a27),
also used by SourceOrigin and DirkEngine. This page applies it to piqueld.
Keep implementations simple, follow neighbouring modules, and share repeated
logic instead of copying it.

## Crates and boundaries

[Module boundaries](architecture/dependency-flow.md) describes each crate and
what it may depend on, and `just boundary` checks it. Shared contracts live in
`piqueld-core`, which knows no server, database, Docker or browser framework;
the daemon's crates (`piqueld-config`, `piqueld-store`, `piqueld-docker`)
don't depend on the ones they build alongside. A new dependency between
crates updates both that page and `tools/xtask/src/boundary.rs`.

A feature reaches every surface it applies to, from the checklist in
`AGENTS.md`.

## Errors

Library crates return explicit `thiserror` errors, with their source kept by
`#[source]` or `#[from]`. `anyhow` stays in the applications (`apps/`) and in
`tools/`, with `.context` on each step. Don't drop a source with
`map_err(|_| ...)` unless it could hold a secret, and say so in a comment.
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

Production code never panics without saying why: `expect_used`, `panic`,
`todo`, `unimplemented` and `dbg_macro` are denied outside tests. Return an
error when the input can be wrong. An invariant the types can't express keeps
its `expect`, whose message says what holds, under an
`#[expect(clippy::expect_used, reason = "...")]` on the smallest item around
it, such as `reason = "The placeholder IDs are valid constants."`. Tests may
panic; integration test crates say so once, in an `#![expect]` at their root.

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
helpers with no owner. Limits and timeouts are named constants.

## Modules, tests and docs

A module with children is `foo/mod.rs` (`self_named_module_files`). Unit tests
live in `#[cfg(test)] mod tests;` with a sibling `tests.rs`; a block of 30 lines
or fewer may stay inline. Tests prove one behaviour each, named for it, and
assert the outcome; [automated validation](validation.md) lists the suites
and what each covers.

Write a test at the lowest layer that fails without the change, once: not
again in a wrapper, at each call site or on each surface. Extend before you
add: when a test or table covers the behaviour, add a row or an assertion to
it. A fix to logic, data, state or a protocol comes with a test that fails
without the fix, written first. A purely visual fix, such as spacing or
colour, is shown with before and after screenshots in the pull request
instead, and gets no test.

Tests wait for events, not time: no `thread::sleep` or `tokio::time::sleep`
to let something happen, no assertions on elapsed time, and no retries. Await
the channel, notification or state change that marks the milestone, such as
`Store::wait_until`, `Store::writer_waiting` or a `FakeDocker` counter, and
move time with `#[tokio::test(start_paused = true)]`. A wait on a real
subprocess or network peer that has nothing to await says why on the line
above, `// wall-clock: <why>`. `cargo xtask test-waits` enforces this, and
`just lint` runs it.

Every item has a doc comment. Public items' `///` comments on the manifest,
configuration and CLI types are also the generated references, so keep them
about what the reader can see and do, and put notes for developers in `//`
comments ([changing the documentation](documentation.md)). `just doc` builds
the docs, private items included, with warnings denied.

## Deviations

- No `CLAUDE.md` beside `AGENTS.md`: Claude Code reads `AGENTS.md` itself, so
  the file would add nothing.
- `unwrap_used` is off: the tests still call `unwrap` about 4,400 times. They
  move to `expect("what should have held")` crate by crate (PQD-124); new
  tests use `expect`.
- Most unit tests are inline `mod tests { ... }` blocks longer than 30 lines.
  Move a block to a sibling `tests.rs` when you change it.
- `just doc` allows links from public items to private ones
  (`rustdoc::private_intra_doc_links`), since most of the code is internal and
  documented for its developers.
- Worktree setup: agents wait on `target/worktree-setup/lock` with `flock`,
  not on the PID in `running`, which a later process may reuse.
