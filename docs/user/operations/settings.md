# Changing daemon settings

Daemon settings are your installation's policy: how long history and the
audit trail are kept, how many previews may exist, how often piqueld checks
for drift. Change them from the dashboard or `piquelctl`, and they apply at
once, without restarting the daemon.

The daemon's other settings describe its host, such as its listeners, its
Docker socket and its DNS provider accounts. Those only change in its
[configuration file](../reference/configuration.md), with a restart.

## Before you start

- Reading settings needs the `system:read` permission.
- Changing them needs `settings:manage`. It can shorten how long the audit
  trail is kept, so granting it needs a recent passkey sign-in, like
  `accounts:manage`; see [permissions](../accounts/permissions.md).

## See the settings

In the dashboard, open **Settings** under System. Each section of daemon
settings, such as **History** or **Previews**, lists its settings, each with
what it does and its value in a field. A setting someone changed shows its
default under the field, and a button beside it to reset it.

![The Settings page: the History section lists each retention setting with its key, what it does and its value in a field; Application events (days), just changed to 30, says Saved and its default, 90, under the field, with a reset button beside it](../../images/operations/settings.png)

The host's settings follow, read-only.

On your machine, `piquelctl settings` lists the same, with where each value
comes from:

- **default**: nobody changed it.
- **changed**: someone changed it from the dashboard, `piquelctl` or the API.
- **configuration file**: the daemon's configuration file sets it, which
  pins it (see [below](#settings-the-configuration-file-pins)).

```console
piquelctl settings
```

```text
History:
  retention.finished_operation_days: 10 (default)
  retention.event_days: 30 (changed)
  retention.audit_days: 365 (configuration file)
```

Show one setting with its range, default and what it does:

```console
piquelctl settings get retention.event_days
```

## Change a setting

In the dashboard, type the new value in the setting's field, then press
Enter or select **Save** under the field. Until then the field says
**Not saved**, and leaving it saves nothing: Escape, or **Cancel**, brings
the setting's value back. A switch or a choice saves as soon as you change
it. A setting with no limit, such as **Longest token lifetime (days)**,
takes an empty field.

With `piquelctl`:

```console
piquelctl settings set retention.event_days 30
```

Settings that allow no limit take `none`, switches `true` or `false`, and
choices one of their values:

```console
piquelctl settings set auth.max_token_days none
piquelctl settings set ingress.traffic false
piquelctl settings set build.unavailable local
```

The new value applies at once. Pruning uses new retention periods on its next
pass, the reconciliation scan and image cleanup move to their new interval,
new previews, tokens and uploads are checked against the new limits, the
gateway starts or stops counting traffic, and the next build follows
`build.unavailable`.
Lowering a limit never removes what already exists: existing previews keep
running, and existing tokens keep their lifetime.

Keeping less history does remove it. Lowering a setting under **History**,
or **Output kept (days)**, deletes what the new value no longer keeps the
next time piqueld prunes, and nothing brings it back. The dashboard asks you
to confirm such a change, and a reset that does the same. `piquelctl settings
set` doesn't ask.

Every change is recorded in the [audit trail](audit.md), with the old and the
new value.

[Daemon configuration settings](../reference/configuration-settings.md) lists
every setting, with its range and default. Daemon settings are the tables
marked as such there, `auth.max_token_days`, `build.unavailable` and
`ingress.traffic`.

Some settings apply only with what the installation has:
`build.unavailable` needs a [build machine](../applications/build-machine.md).
Without one, the dashboard shows its value without a field, `piquelctl
settings` says what it needs, and changing it fails with
`setting_unavailable`. A value changed while the build machine was there can
still be reset.

## Reset a setting

A changed setting goes back to its default with the reset button beside its
field in the dashboard, which then says what the value was, or:

```console
piquelctl settings reset retention.event_days
```

## Settings the configuration file pins

A daemon setting the [configuration file](../reference/configuration.md)
sets is pinned to that value. The dashboard shows it without a field, as
**Set in the configuration file**, and `piquelctl settings set` fails with
`setting_pinned`. To change it,
edit the file and restart the daemon, or remove it from the file to manage it
from the dashboard again; the value changed there before applies once more.

This keeps a declarative configuration declarative. On NixOS, a daemon
setting you set under `services.piqueld.settings` is pinned, and one you leave
unset is managed from the dashboard; see the [NixOS module](../reference/nixos.md).

A configuration file copied from an older example may set every daemon
setting to its default, which pins them all. Comment out or remove those you
want to manage from the dashboard, then restart the daemon.

The configuration file can't pin a setting to no limit, such as
`auth.max_token_days`: leaving it out leaves the setting to the dashboard.

## Check that it works

`piquelctl settings get <key>` shows the value in effect and its source. The
[audit trail](audit.md) shows who changed it, and when:

```console
piquelctl audit
```

## Troubleshooting

- **`setting_pinned`.** The configuration file sets this setting. Change it
  there, or remove it from the file and restart the daemon.
- **`setting_invalid`.** The value is out of the setting's range. The error,
  and `piquelctl settings get <key>`, say which values it takes.
- **`setting_unavailable`.** The installation lacks what the setting needs,
  such as a build machine for `build.unavailable`.
- **`setting_unknown`.** No setting has this key. `piquelctl settings` lists
  them.
- **`permission_denied`.** Changing a setting needs `settings:manage`.
