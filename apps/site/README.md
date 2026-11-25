# piqueld.dev

The marketing site for piqueld, served at <https://piqueld.dev>. It is plain
static HTML and CSS, with a few lines of inline script that copy the install
command on a click, and no build step or server runtime, so any static
file server that follows symlinks can serve this directory as it is. The
`site` service of `deploy/piqueld-dev.toml` serves it with
`deploy/site/Dockerfile`; see
[publishing piqueld.dev](../../docs/dev/publishing.md).

`index.html` is the whole site: piqueld's manual page, as a terminal shows
it, with the sections a manual page has. Its commands, their output and its
manifest are the user guide's, shortened, with `…` wherever something is
left out. Its footer names the released version and the month of its
release; [releasing](../../docs/dev/releasing.md#release-a-version) says
when to change them.

The other files:

- `install.sh`, served at <https://piqueld.dev/install.sh>, installs piqueld
  from its latest GitHub release; the `install` job of the
  [release checks](https://github.com/sourceorigin-dev/piqueld/blob/main/docs/dev/validation.md#release-checks)
  runs it against binaries built from `main`.
- `dashboard.png` is the dashboard's list of an application's environments,
  and `dashboard-narrow.png` the same list as a phone shows it. The page
  draws the narrow one at its real size and the other a little larger, so
  both are saved at three device pixels per CSS pixel. The `width` and
  `height` the page gives them, and the width it caps the narrow one at, are
  their sizes divided by three: change them when a picture changes size.
  `just docs-screenshots` generates them with the documentation's
  screenshots, and CI fails when one shows an older version of that list;
  see [screenshots](../../docs/dev/documentation.md#screenshots).
- `favicon.svg` and `favicon.png`, piqueld's logo, link to the dashboard's in
  `apps/piqueld-ui/assets`, and the image copies them from there.
- `fonts/jetbrains-mono.woff2` is the Latin subset of JetBrains Mono, an
  OFL-licensed variable font, with its weights from regular to bold. It is
  served from this site so that the page makes no third-party requests; its
  licence is in `fonts/OFL.txt`.

## Preview

```console
python3 -m http.server --directory apps/site 8000
```

Then open <http://localhost:8000>. To see it served as it is published, with
the book next to it, deploy your worktree to a preview of `piqueld-dev`; see
[previewing a branch](../../docs/dev/publishing.md#preview-a-branch).
