# NixOS module

Import the flake module and enable the service:

```nix
{
  imports = [ inputs.piqueld.nixosModules.default ];
  services.piqueld.enable = true;
  users.users.alice.extraGroups = [ "piqueld" ];
}
```

The default package embeds the dashboard. `services.piqueld.package` can select
`inputs.piqueld.packages.${pkgs.system}.daemon` for API-only operation.
The default module installs the CLI when the daemon is enabled; set
`programs.piquelctl.enable = false` to omit it. For a remote-management machine
with only the CLI, import `inputs.piqueld.nixosModules.piquelctl` and enable
`programs.piquelctl`. This module does not enable Docker or create the daemon
service or user.

Connection profiles use the same typed-settings style as the daemon:

```nix
{
  imports = [ inputs.piqueld.nixosModules.piquelctl ];
  programs.piquelctl = {
    enable = true;
    settings.default = "production";
    settings.profiles.production = {
      url = "http://127.0.0.1:7845";
      timeout = "2m";
    };
  };
}
```

Each profile must set exactly one of `socket` or `url`; `timeout` is optional.
`settings.default`, also optional, names the profile `piquelctl` uses when
neither `--profile` nor `PIQUELD_PROFILE` selects one.
The module writes `/etc/piqueld/profiles.toml` and installs the unwrapped CLI.
Every user and every CLI build, including `cargo run -p piquelctl`, discovers
these profiles without environment setup. User profiles can add or replace named
system profiles; `--profiles-file` and `PIQUELD_PROFILES_FILE` bypass discovery.
Run `piquelctl profiles` to see the effective names and endpoints.

For nix-darwin, import `inputs.piqueld.darwinModules.piquelctl` and use the same
`programs.piquelctl` options. The shared CLI module does not require the daemon.

When migrating from the wrapper-based module, rebuild the machine configuration
and remove old `PIQUELD_PROFILES_FILE` exports or custom wrappers unless you
intentionally want an exclusive file override. Remove duplicate TOML generation
and per-user profile symlinks/activation scripts used to expose system profiles;
the generated system file now serves all users. Keep intentional user overrides
in the user configuration file. Profile settings remain in the public Nix store
and must not contain credentials.

The module runs [`piqueld check-config`](configuration.md) on the generated
file while building the system, so an invalid setting fails `nixos-rebuild
build` instead of `piqueld.service`. Credentials are not read during the check.
Set `services.piqueld.checkConfig = false;` when the package cannot run on the
build machine, such as when cross-compiling.

`dataDir` defaults to `/var/lib/piqueld`, with mode `0700`, holding `piqueld.db`.
The daemon keeps the backups it takes before an upgrade in its `backups`
directory. Set `services.piqueld.rollBack = true;` to run
[`piqueld roll-back`](../operations/backups.md#roll-back) as the `piqueld`
user before each start: after a system rollback to a generation that also
sets it, it restores the newest backup that generation's piqueld can open,
and moves the state it replaces into `dataDir`'s `rolled-back-<time>`
directory. It does nothing while piqueld can open the database. Without it,
roll back by hand with `piqueld restore`, as the `piqueld` user, with
`--data-dir`, which doesn't read the configuration.
The module puts the daemon's package, with `piqueld`, on the system `PATH`.
`runtimeDir` defaults to `/run/piqueld`, prepared by systemd with mode `0750`.
The socket is `/run/piqueld/piqueld.sock`, owned by `piqueld:piqueld` with mode
`0660`. After logging in again to refresh group membership, users can connect
to the socket without sudo. Membership permits only a socket connection; run
`piquelctl login` to authenticate an account before `piquelctl status` or other
API operations. It grants no direct access to private state or permission to
replace the socket. Root (`sudo piquelctl …`) and the `piqueld` user act as the
[host operator](../accounts/recovery.md#the-host-operator) without logging in.
Only the existing `piqueld.service` is needed; no proxy or socket unit is required.

Custom `runtimeDir` values must be dedicated directories below `/run`. Configure
CLI profiles or `--socket` explicitly when overriding this default.
The module enables Docker and grants the service user Docker access, which is
host-administrative authority. To run Docker some other way, set
`virtualisation.docker.enable = false;` and point `settings.docker.socket` at
a socket the `piqueld` user can reach. The service starts after Docker but does not
require it: a Docker failure or restart leaves piqueld serving, and it resumes
deploying once Docker is back. The module opens no firewall port, and
`networking.firewall` doesn't govern the HTTPS gateway's ports 80 and 443,
which Docker publishes through its own iptables rules: see
[enabling the gateway](../networking/routes.md#enable-the-gateway). TCP is
disabled by default. Use `settings.server.listen_mode = "localhost";` for local HTTP,
or configure Tailscale explicitly:

```nix
services.tailscale.enable = true;
services.piqueld.settings.server = {
  listen_mode = "tailscale"; # or "both" to also serve localhost
  port = 7845;
};
```

Join the host to your tailnet separately. The module orders piqueld after
`tailscaled` for these modes, which piqueld reads through its socket; it does
not enable or configure Tailscale itself. Ordering does not guarantee connectivity: if
Tailscale is unavailable at startup, piqueld warns and requires a restart to
activate remote listening. Ensure your firewall and tailnet policy permit the
selected port on the Tailscale interface. All callers must authenticate. Configure
`settings.auth.public_url` with a stable HTTPS hostname and terminate TLS externally;
see [your own reverse proxy](../getting-started/install.md#your-own-reverse-proxy).

For HTTPS without a proxy, let piqueld join the tailnet as its own node instead.
piqueld runs a dedicated, unprivileged `tailscaled` from
`services.tailscale.package` rather than using the host's, and its state lives
in `dataDir`. The host does not need `services.tailscale.enable`:

```nix
services.piqueld.settings.tailscale = {
  enabled = true;
  hostname = "piqueld"; # https://piqueld.<tailnet>.ts.net
  auth_key_file = config.age.secrets.piqueld-ts-auth-key.path; # first login only
};
```

`settings.auth.public_url` defaults to the node's HTTPS URL. See
[the tailnet node](configuration.md#tailnet-node) for details.

`settings` declares typed options for `server.listen_mode`, `server.port`,
`server.allowed_hosts`, `auth.public_url`, `auth.max_token_days`, `tailscale.enabled`,
`tailscale.hostname`, `tailscale.auth_key_file`, `tailscale.https_port`,
`tailscale.socket`, `docker.socket`,
`docker.auto_initialize_swarm`, `build.parallel`, `build.builder`,
`build.repository` (set both, or neither), `build.unavailable`, `ingress.enabled`, `ingress.public_addresses`,
`ingress.acme.directory`,
`ingress.acme.email`, `ingress.private.enabled`, `ingress.private.hostname`,
`ingress.private.auth_key_file` (passed as a systemd credential, like the other
`_file` settings), `ingress.private.dashboard_hostname`, `ingress.tunnel.enabled`, `ingress.tunnel.credentials_file`
(also a systemd credential, required while the tunnel is enabled),
`ingress.webhook_hostname`, `ingress.traffic`, `dns.providers`, all three `reconciliation`
intervals/timeouts, the `retention` periods, both `build_history` limits,
the five [`previews`](configuration.md#previews) limits,
[`snapshots.max_upload_bytes`](configuration.md#local-snapshots),
[`checkouts.max_bytes`](configuration.md#git-checkouts), the three `images` settings,
`telemetry.endpoint`, `telemetry.headers_file` (a systemd credential),
and `telemetry.containers`, with the daemon's defaults. Reconciliation
values must be 1–86400 seconds; retention values are nonnegative days, or a
count for `retention.deployments_per_environment`, with zero disabling pruning. Unknown settings are rejected.

[Notification destinations](../operations/notifications.md) are managed with
`piquelctl notification` or the dashboard, not the module; the former
`settings.notifications` options are removed.

[Telemetry export](../operations/telemetry.md) pushes traces, metrics
and logs to an OTLP/gRPC collector, along with the
[logs and resource usage of every container](../operations/telemetry.md#what-piqueld-sends)
unless `containers = false`. A collector token goes in `headers_file`, which
systemd passes to the daemon as the `otlp-headers` credential:

```nix
services.piqueld.settings.telemetry = {
  endpoint = "https://otel.example.com:4317"; # or "unix:///run/otel/otlp.sock"
  headers_file = config.age.secrets.otlp-headers.path; # `authorization: Bearer …`
  containers = true; # the default
};
```

[DNS providers](configuration.md#dns-providers) take their credentials only as
files:

```nix
services.piqueld.settings = {
  ingress.acme.email = "admin@example.com"; # optional
  dns.providers = [
    {
      kind = "cloudflare";
      api_token_file = config.age.secrets.cloudflare-dns-token.path;
      manage_records = true; # optional: maintain routes' DNS records
    }
    {
      kind = "ovh";
      endpoint = "ovh-eu";
      application_key_file = config.age.secrets.ovh-application-key.path;
      application_secret_file = config.age.secrets.ovh-application-secret.path;
      consumer_key_file = config.age.secrets.ovh-consumer-key.path;
    }
  ];
};
```

Settings enter the world-readable Nix store, so credentials are only accepted
as `_file` settings taking a host path, such as an agenix or sops-nix secret:
the Tailscale auth key as `auth_key_file`, and DNS provider credentials as
their `_file` settings. The module passes every `_file` path to systemd
`LoadCredential=`: systemd reads the file as root and hands the service a
private copy, so the secret can stay root-only. Restart piqueld after a secret
changes.

The module always supplies
`server.data_dir` from `dataDir` and `server.runtime_dir` from `runtimeDir`. It does not configure a registry, Traefik,
TLS termination or an external UI directory. Git, SSH, and Docker executables
are present on the service PATH. Configure SSH credentials and known hosts for
the service user, not the interactive operator; host home directories are protected.

## Build machine and build registry

Two modules set up the machines a host with a [build
machine](configuration.md#build-machine) uses, as [building on another
machine](../applications/build-machine.md) describes; the default module
includes neither. Import `piqueld-builder` on the build machine, not on the
host. The build registry can run on the build machine, on the host, on
another machine, or be any other registry.

`inputs.piqueld.nixosModules.piqueld-builder` runs the build machine's BuildKit
daemon:

```nix
services.piqueld-builder = {
  enable = true;
  port = 1234; # the default
  cacheSize = "20GB"; # the default
  emulatedSystems = [ "aarch64-linux" ]; # none by default
};
```

It runs `buildkitd.service` as the `buildkit` user, without root privileges,
through RootlessKit, with an automatically allocated range of subordinate IDs.
Build steps get their own network namespace, through slirp4netns, which
doesn't reach the machine's loopback. Its state and build cache are in
`/var/lib/buildkit`, and it removes the least recently used cache beyond
`cacheSize`. `package` selects the BuildKit package. `emulatedSystems` sets
`boot.binfmt.emulatedSystems`, with static emulators, which work inside build
containers, unless `boot.binfmt.preferStaticEmulators` is set to `false`.

`inputs.piqueld.nixosModules.piqueld-registry` runs a build registry:

```nix
services.piqueld-registry = {
  enable = true;
  port = 5000; # the default
  garbageCollectDates = "Sun 04:00"; # the default
};
```

It enables `services.dockerRegistry` on loopback, with deletes enabled, so its
storage and package are that module's options. `docker-registry-collect.service`
runs at `garbageCollectDates`, or when the machine is next on if it was off
then: it stops the registry, frees the layers no remaining image uses, and
starts it again. The module refuses
`services.dockerRegistry.enableGarbageCollect`, which collects while the
registry accepts pushes.

Both listen on loopback only. `buildkitd-tailnet.service` and
`docker-registry-tailnet.service` expose them on the machine's tailnet name
with Tailscale Serve, BuildKit as TCP and the registry over HTTPS with the
tailnet's certificate, and remove their Serve entries when they stop. Both
modules require `services.tailscale.enable`, and open no firewall port.

## Packages

Nix packages and development shells support x86_64 Linux and aarch64 macOS,
where only the CLI package is available. Configuration and
package changes require rebuilding NixOS; application state remains under
`dataDir`.
