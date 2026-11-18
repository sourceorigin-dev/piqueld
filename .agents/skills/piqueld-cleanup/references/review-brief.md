# Cleanup pass: reviewer

Review one PR from a piqueld cleanup pass. It claims to be a pure refactor: no
behavior, output, schema, format or test coverage changes. The only exceptions
are one-line fixes and deliberate changes, each in its own commit and named in
the description.

Review read-only. Don't push, comment or edit. Use `gh pr view <N>`, `gh pr
diff <N>`, and `git fetch origin main pull/<N>/head` to check out the head SHA
your task gives. Export the `CARGO_BUILD_JOBS` your task gives.

Check every claim in your focus list yourself, against `origin/main`. Then
look for, most important first:

1. Hidden behavior changes: error codes, messages, paths and precedence,
   defaults, serialization, Docker names, log fields, timeouts, locking, and
   query plans (compare `EXPLAIN QUERY PLAN`).
2. Lost coverage: dropped assertions or cases, and expected values built by
   the code under test. Compare test names and counts per file with `main`.
3. Abstractions that make the code harder to read.
4. Missed cleanup and stale doc comments in the touched files, briefly.

Report each finding with its severity (blocker, should-fix or nit),
`file:line`, the problem and a fix. Say which claims you verified and how. If
the PR is clean, say so.
