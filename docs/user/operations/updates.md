# Updating piqueld

piqueld checks for newer versions of itself and tells you when one is out. It
never updates itself: you choose when to install the update.

## Before you start

Reading the update status needs `system:read`. Checking now, choosing a
channel, and installing an update or going back need `system:operate`; see
[permissions](../accounts/permissions.md).

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
To update: Install it with piquelctl update apply or the dashboard, or on the host: sudo piqueld update
Last checked: 2026-11-25 09:12:44Z
```

`piquelctl update status` shows the same without checking.

## Install the update

Read the release notes first: before 1.0, a minor version such as 0.3.0 may
need changes to your configuration. Then install it the way you installed
piqueld, as the panel and `piquelctl` say:

- With the install script, select **Update** next to the newest build in
  the **Updates** panel, or run:

  ```console
  piquelctl update apply
  ```

  The host's updater downloads the newest build of your channel, checks it
  against its checksum and checks your configuration with it. Then it
  restarts piqueld on it, and waits until the daemon answers. If the daemon
  doesn't start, it goes back to the version you ran, unless the new one
  already updated the database; see [troubleshooting](#troubleshooting). The
  panel's **Last update** row, and `piquelctl`, which waits for it, say how
  it went.

  The updater downloads the build from its GitHub release. On the host,
  `sudo piqueld update` does the same, and also reads the archive a mirror
  at `updates.feed_url` names. Add `--version` to
  install another published version, such as `--version 0.3.0`, and `--from`
  to install from a directory holding a release's archive and `SHA256SUMS`,
  such as on a host without internet access.
- On NixOS, follow the channel's tag with the flake input, as in
  [choose a channel](#choose-a-channel), update it, then rebuild:

  ```console
  nix flake update piqueld
  sudo nixos-rebuild switch
  ```

The daemon backs up its data before its first start on a new version; see
[backups and upgrades](backups.md#upgrade).

## Go back to the previous version

With the install script, an update keeps the version it replaced, which the
**Updates** panel shows as **Previous build**. Go back to it, and restart
piqueld on it, with its **Roll back** button, or:

```console
piquelctl update rollback
```

On the host, `sudo piqueld update rollback` does the same.

It refuses when the version you run took the newest backup before updating
the database, which older versions can't read. Then go forward instead with
`piqueld update --version`, or [restore the backup](backups.md#roll-back),
which undoes every change since. On NixOS, roll back the system:
`sudo nixos-rebuild switch --rollback`.

## Troubleshooting

- **The last check failed.** The panel and `piquelctl update status` say why,
  and `piquelctl update check` exits with an error. piqueld tries again ten
  minutes later, then less often while it keeps failing.
  piqueld reads the channel's feed from GitHub, at
  [`updates.feed_url`](../reference/configuration-settings.md); allow the host
  to reach `github.com` over HTTPS, or point it at a mirror.
- **The dashboard says piqueld updates itself only where the install script
  set it up.** Install the update the way you installed piqueld, as the
  panel says.
- **The dashboard says this host lacks the units that apply updates.** It was
  installed before piqueld could update itself: run the install script again
  once, which installs them.
- **The update says the new version didn't start after updating the
  database.** It leaves that version installed, as the previous one can't
  read the database. Read why it didn't start with `journalctl -u piqueld`,
  then fix the cause, or [restore the backup](backups.md#roll-back) it names
  and run `sudo piqueld update rollback`.
- **The channel can't be changed.** `updates.channel` in the daemon
  configuration sets it. Change it there, then restart the daemon.
- **You want no checks**, such as on a host without internet access. Set
  `updates.check = false` in the daemon configuration. **Check now** still
  checks on request.
