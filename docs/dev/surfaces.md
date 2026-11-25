# Surfaces

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

A refactor changes none of the surfaces: the OpenAPI document, the manifest,
the CLI's help and output, the NixOS module's options and output, and the
names and labels of what piqueld creates in Docker stay as they were.
