# Backup repositories

A backup repository is where piqueld writes backups: a directory in
[restic](https://restic.net/)'s repository format, encrypted and
deduplicated. Because the format is restic's, the `restic` command opens a
repository without piqueld, on any host.

This version creates, checks and removes repositories. It doesn't write
backups to them yet: [Backups and upgrades](backups.md) describes the backups
piqueld takes today.

## Before you start

- Managing repositories needs the `backups:manage` permission; listing them
  needs `system:read` or `backups:manage`. See
  [permissions](../accounts/permissions.md#installation-wide-permissions).
- Creating a repository also needs a passkey sign-in within the last 2
  minutes, since it shows the repository's recovery key. The dashboard and
  `piquelctl` ask you to confirm with your passkey when it is older.
- Choose a directory on the daemon's host that is missing or empty, and that
  the daemon's user can write. A directory on another disk than the data
  directory survives that disk failing; a repository on the same disk only
  protects against a broken database or a bad upgrade.

## Create a repository

Each repository has two keys. piqueld keeps one, encrypted with its
[secret key](../reference/configuration.md#secret-storage). The other is the
recovery key, which piqueld shows once and never stores.

In the dashboard:

1. Open **Backups** in the sidebar.
2. Select **Create repository**.
3. Fill in **Name**, such as `local`, and **Directory on the daemon's host**,
   such as `/mnt/backup/piqueld`.
4. Select **Create repository**.
5. Copy the recovery key from **Save the recovery key of local**, and keep it
   with the directory's path somewhere other than the host. Select **Done**
   once it is saved: piqueld never shows it again.

With `piquelctl`:

```console
piquelctl backup repository create local --path /mnt/backup/piqueld
```

It prints the recovery key, even with `--quiet`. Save it the same way.

## Check a repository

A check opens the repository with piqueld's key and checks that its index,
packs and snapshots agree, without reading their data. In the dashboard,
select **Check** on the repository's row. With `piquelctl`:

```console
piquelctl backup repository check local
```

The **Last check** column, and `piquelctl backup repository list`, show when
the latest check ran and what it found. A repository piqueld can't open, such
as one whose directory was deleted, fails its check.

## Open a repository with restic

With the recovery key, `restic` opens the repository without piqueld:

```console
restic --repo /mnt/backup/piqueld snapshots
```

It asks for the repository's password: enter the recovery key. `restic key list`
shows the repository's two keys, with `piqueld` as their host and `daemon` or
`recovery` as their user.

## Remove a repository

Removing a repository only makes piqueld forget it: its directory and its
backups stay where they are, and its recovery key still opens them. In the
dashboard, select **Remove** on its row. With `piquelctl`:

```console
piquelctl backup repository delete local
```

To delete the backups too, delete the directory yourself afterwards.

## Troubleshooting

- **`backup_repository_not_empty` (409).** The directory holds files. Choose
  an empty or missing directory.
- **`backup_repository_exists` (409).** Another repository has the name or
  the directory.
- **`backup_repository_unavailable` (503).** piqueld couldn't create the
  repository, most often because its user can't write the directory. The
  daemon's log says why. Nothing is left in the directory.
- **Repositories disappeared after recovering a lost secret key.** Their keys
  were encrypted with it, so [recovery](../accounts/recovery.md#a-lost-secret-key)
  removes them from piqueld. Their backups remain, and their recovery keys
  still open them.
