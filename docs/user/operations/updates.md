# Updating piqueld

piqueld checks for newer versions of itself and tells you when one is out. It
never updates itself: you choose when to install the update.

## Before you start

Reading the update status needs `system:read`. Checking now and choosing a
channel need `system:operate`; see [permissions](../accounts/permissions.md).

## Choose a channel

An installation follows one of two release channels:

- **Stable**: the versions the maintainers release. It is the default.
- **Nightly**: the latest `main`, built every night once its checks pass.
  Nightlies get fixes and features first, and their breaking changes too.

On the dashboard, open **Daemon status**, then choose the channel in the
**Updates** panel. With `piquelctl`:

```console
piquelctl update channel nightly
```

piqueld checks the new channel at once. Moving from nightly to stable keeps
the nightly you run until stable has a newer build: piqueld can't go back to
an older version, as each version may update its database.

On NixOS, the channel is part of your flake. Follow the channel's tag with
the piqueld input, and set the same channel in the module, `stable` by
default, which then can't be changed from the dashboard or `piquelctl`:

```nix
{
  inputs.piqueld.url = "github:sourceorigin-dev/piqueld/nightly";
  # In your NixOS configuration:
  services.piqueld.settings.updates.channel = "nightly";
}
```

Elsewhere, set [`updates.channel`](../reference/configuration-settings.md) in
the daemon configuration to fix the channel the same way.

## See whether an update is out

piqueld checks its channel every six hours. When a newer build is out, the
dashboard's sidebar shows **Update available** and its version, and destinations
receiving the **Updates** [notification category](notifications.md) get one
notification for it, and a reminder if it is still the newest once piqueld's
[history](history.md) no longer holds the first.

The **Updates** panel on **Daemon status** shows the version you run, the
newest build on your channel with its release notes, and when piqueld last
checked. Select **Check now** to check at once. With `piquelctl`:

```console
piquelctl update check
```

```text
piqueld 0.2.0 follows the stable channel
Update available: 0.3.0
Release notes: https://github.com/sourceorigin-dev/piqueld/releases/tag/v0.3.0
To update: Run the install script for this build: curl -fsSL https://piqueld.dev/install.sh | PIQUELD_VERSION=0.3.0 sh
Last checked: 2026-11-25 09:12:44Z
```

`piquelctl update status` shows the same without checking.

## Install the update

Read the release notes first: before 1.0, a minor version such as 0.3.0 may
need changes to your configuration. Then install it the way you installed
piqueld, as the panel and `piquelctl` say:

- With the install script, run it for the build piqueld names, with
  `PIQUELD_VERSION`. Without it, the script installs the newest stable
  release, which is older than a nightly; see
  [updating with the script](../getting-started/install.md#update-piqueld).
- On NixOS, follow the channel's tag with the flake input, as in
  [choose a channel](#choose-a-channel), update it, then rebuild:

  ```console
  nix flake update piqueld
  sudo nixos-rebuild switch
  ```

The daemon backs up its data before its first start on a new version; see
[backups and upgrades](backups.md#upgrade).

## Troubleshooting

- **The last check failed.** The panel and `piquelctl update status` say why,
  and `piquelctl update check` exits with an error. piqueld tries again ten
  minutes later, then less often while it keeps failing.
  piqueld reads the channel's feed from GitHub, at
  [`updates.feed_url`](../reference/configuration-settings.md); allow the host
  to reach `github.com` over HTTPS, or point it at a mirror.
- **The channel can't be changed.** `updates.channel` in the daemon
  configuration sets it. Change it there, then restart the daemon.
- **You want no checks**, such as on a host without internet access. Set
  `updates.check = false` in the daemon configuration. **Check now** still
  checks on request.
