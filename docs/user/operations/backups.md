# Backups and upgrades

Back up piqueld's state, so you can recover from a lost disk or roll back an
upgrade.

## Before you start

- Run these steps on the piqueld host, as root: the data directory is
  readable only by the daemon's user.
- The commands use `/var/lib/piqueld`, its default location. If you set
  [`server.data_dir`](../reference/configuration-settings.md#serverdata_dir),
  use its value instead.
- The commands need `sqlite3`, from the `sqlite` package. Backing up a running
  daemon also needs `rsync`.

## What to back up

The data directory,
[`server.data_dir`](../reference/configuration-settings.md#serverdata_dir),
holds everything piqueld needs: the database `piqueld.db`, the secret key
`secrets.key`, and the state of its tailnet nodes and ingress gateway. Back it
up whole:

- Without `secrets.key`, the secrets, webhook secrets and notification
  destinations in the database can't be decrypted (see
  [secret storage](../reference/configuration.md#secret-storage)).
- Without the tailnet state, the daemon joins the tailnet as a new node, which
  may get another name, and passkeys registered for the
  [tailnet node's origin](../getting-started/install.md#piquelds-own-tailnet-node)
  only work with the old name.

Your applications' data lives in Docker volumes, which aren't part of it. Back
them up separately.

## Back up

With the daemon stopped:

1. Stop it:

   ```console
   sudo systemctl stop piqueld
   ```

2. If [ingress](../networking/routes.md#enable-the-gateway) is enabled, stop the containers piqueld
   runs for it, which keep writing to the data directory. Your applications'
   routes stay unreachable until the daemon starts again.

   ```console
   instance=$(sudo sqlite3 /var/lib/piqueld/piqueld.db 'SELECT instance_id FROM instance_metadata')
   sudo docker stop $(sudo docker ps --quiet --filter label=io.piqueld.ingress=true --filter label=io.piqueld.instance=$instance)
   ```

3. Create the backup directory, then copy the data directory's contents into
   it, keeping their owner and permissions. If the backup directory already
   exists, `mkdir` fails and nothing is copied, rather than mixing two
   backups:

   ```console
   sudo mkdir --mode=0700 /safe/location/piqueld-backup &&
     sudo cp -a /var/lib/piqueld/. /safe/location/piqueld-backup/
   ```

4. Start it again:

   ```console
   sudo systemctl start piqueld
   ```

5. [Check the backup](#check-that-it-works).

While the daemon runs, the database must be copied with SQLite's backup
command, which includes changes still in its write-ahead log. Copy the
database before the other files: the daemon may create `secrets.key` while you
copy, and the backup then includes it.

> **Warning:** don't run `piquelctl secrets recover-key` or replace
> `secrets.key` until the backup is done. The backup could otherwise hold a key
> that can't decrypt its database. If you can't rule that out, stop the daemon
> and back up as above.

The ingress gateway and the tailnet nodes keep writing their state while the
daemon runs, so `rsync` may copy one of their files as it changes, or a newer
one than the database knows of. For a backup that's consistent throughout,
stop the daemon and back up as above.

1. Create the backup directory:

   ```console
   sudo mkdir --mode=0700 /safe/location/piqueld-backup
   ```

2. Copy the database:

   ```console
   sudo sqlite3 /var/lib/piqueld/piqueld.db ".backup '/safe/location/piqueld-backup/piqueld.db'"
   ```

3. Give the copy the database's owner and permissions. Without them, the
   daemon can't write to a restored copy.

   ```console
   sudo chown piqueld:piqueld /safe/location/piqueld-backup/piqueld.db
   sudo chmod 0600 /safe/location/piqueld-backup/piqueld.db
   ```

4. Copy the other files, keeping their owner and permissions:

   ```console
   sudo rsync -a --exclude 'piqueld.db*' /var/lib/piqueld/ /safe/location/piqueld-backup/
   ```

5. [Check the backup](#check-that-it-works).

## Check that it works

Check the backup's database, however you took it:

```console
sudo sqlite3 /safe/location/piqueld-backup/piqueld.db 'PRAGMA integrity_check; PRAGMA user_version;'
```

It prints `ok`, then the database's version.

## Upgrade

The daemon updates its database when a new version first starts. These updates
only go forward: an older daemon refuses a database a newer one has updated, so
going back to the previous binary alone isn't a rollback.

Before it updates the database, the daemon backs up the data directory into
`/var/lib/piqueld/backups/pre-<schema>-<time>`, where `<schema>` is the
database's version and `<time>` when it was taken, in Unix milliseconds. It
keeps the last three. If the backup fails, the daemon doesn't start, and its
log says why. This backup holds the database, `secrets.key`, and the tailnet
and ingress state. It is on the same disk as the data directory, so it doesn't
replace your own backup.

1. [Back up](#back-up) the data directory.
2. Keep the previous version and its configuration. On NixOS, the previous
   system generation keeps them.
3. Install the new version.

## Roll back

To go back to the previous version after an upgrade, go back to its binary,
then let `piqueld roll-back` restore the backup the daemon took before it
updated the database.

> **Warning:** restoring a backup also undoes every change made since it was
> taken: application edits, accounts and deployment history.

1. Stop the upgraded daemon:

   ```console
   sudo systemctl stop piqueld
   ```

2. Go back to the previous version: on NixOS, run
   `sudo nixos-rebuild switch --rollback`; elsewhere, reinstall the previous
   binary.
3. Roll back, as the daemon's user, with the previous version's `piqueld`:

   ```console
   sudo -u piqueld piqueld --config /etc/piqueld/config.toml roll-back
   ```

   On NixOS, skip this step if the generation you went back to sets
   [`rollBack`](#roll-back-on-every-start): the daemon does it as it starts.
   Otherwise, [roll back by hand](#roll-back-by-hand): the command reads the
   configuration's credential files, which on NixOS only the unit receives.

   It prints where the state it replaced went:

   ```text
   restored backup /var/lib/piqueld/backups/pre-<schema>-<time> (schema <schema>); the state it replaced is in /var/lib/piqueld/rolled-back-<time>
   ```

4. Start the daemon:

   ```console
   sudo systemctl start piqueld
   ```

`piqueld roll-back` does nothing when the installed `piqueld` can open the
database, or there is none. Otherwise it:

1. picks the newest backup in `/var/lib/piqueld/backups` that this version
   can open, and checks it as [`piqueld restore`](#roll-back-by-hand) does: the
   database's integrity, that the database is the one the backup describes,
   and that `secrets.key` decrypts its secrets;
2. stops this installation's [ingress](../networking/routes.md#enable-the-gateway)
   containers, which use the data directory. Your applications' routes stay
   unreachable until the daemon starts again, and starts them;
3. moves the data directory's state into
   `/var/lib/piqueld/rolled-back-<time>`, where `<time>` is when it rolled
   back, in Unix milliseconds. These stay where they are:
   - the backups, so a later roll-back still finds them;
   - the directory telemetry collectors
     [listen in](telemetry.md#receive-telemetry-in-an-application), so they
     keep receiving;
   - the caches the daemon reuses: Git checkouts in `checkouts`, the
     snapshots of local changes that previews deploy in `snapshots`, and the
     build machine's registration in `buildx`;
   - earlier `rolled-back-<time>` directories;
4. restores the backup.

If no backup fits, or the one it picked fails a check, it fails before it
stops or moves anything. If it stops part way once it has started moving
state, the daemon refuses to start, and the next `piqueld roll-back`
finishes the roll-back from where it stopped. The ingress containers stay
stopped until the daemon starts.

It doesn't delete the state it replaced: the database, with every change since
the backup, and the rest of that state stay in `rolled-back-<time>`. It keeps
the three newest of these directories, deleting older ones when it rolls back
again. A directory you name `rolled-back-<anything>` yourself stays until you
delete it.

### What a roll-back undoes

Restoring a backup returns piqueld's records to when the backup was taken,
just before the upgrade. Rolled back within minutes of an upgrade, that is
next to nothing. Rolled back later, it is everything since:

- **Records:** applications, environments, deployments, accounts, API
  tokens, sessions, secret values and notification destinations go back to
  the backup. A token or session revoked since works again, and a secret set
  since has its earlier value.
- **Deployments:** each environment returns to the deployment the backup
  records. A deployment in progress during the upgrade carries on, and its
  [jobs](../reference/manifest.md#jobs) may run a second time if the newer
  version had already run them: write jobs that can, as migration tools
  that record what they applied do.
- **Applications created since:** piqueld no longer knows them. Their
  services keep running without routes, and their volumes keep their data.
  List the services that remain with
  `sudo docker service ls --filter label=io.piqueld.instance=$instance`, using
  the instance ID from [stopping ingress](#roll-back-by-hand); remove the ones
  piqueld doesn't list once you no longer need their data.
- **DNS records:** ones piqueld created since for new routes stay, pointing at
  this host.
- **Tailnet nodes:** a node that logged in again since needs to log in again,
  which it does by itself with an auth key.

Your applications' data in Docker volumes is never deleted by a roll-back.

### Roll back on every start

A host can run `piqueld roll-back` each time the daemon starts, so that going
back to a previous version, such as a NixOS rollback, also rolls back its
state. It only covers going back to a version that does this too, so set it up
before you upgrade.

On NixOS, set:

```nix
services.piqueld.rollBack = true;
```

Elsewhere, add it to the unit with `sudo systemctl edit piqueld`:

```ini
[Service]
ExecStartPre=/usr/local/bin/piqueld --config /etc/piqueld/config.toml roll-back
```

It must run as the daemon's user, as the unit's `User=` does, not as root with
systemd's `+` prefix. The daemon's user owns the data directory, which
`piqueld roll-back` checks, and the files it restores.

### Roll back by hand

When the previous version doesn't have `piqueld roll-back`, or to restore a
backup you [took yourself](#back-up), restore it by hand:

1. Stop the upgraded daemon:

   ```console
   sudo systemctl stop piqueld
   ```

2. If [ingress](../networking/routes.md#enable-the-gateway) is enabled, stop the containers piqueld
   runs for it, which keep using the data directory you're about to move.
   Your applications' routes stay unreachable until the daemon starts again.
   The filter on this installation's ID leaves any other piqueld on the same
   Docker alone:

   ```console
   instance=$(sudo sqlite3 /var/lib/piqueld/piqueld.db 'SELECT instance_id FROM instance_metadata')
   sudo docker stop $(sudo docker ps --quiet --filter label=io.piqueld.ingress=true --filter label=io.piqueld.instance=$instance)
   ```

3. Move the data directory aside, and create an empty one for the daemon's
   user:

   ```console
   sudo mv /var/lib/piqueld /var/lib/piqueld.upgraded
   sudo install -d -o piqueld -g piqueld -m 0700 /var/lib/piqueld
   ```

4. Find the backup the daemon took before the upgrade. Its name holds the
   database's version before the upgrade:

   ```console
   sudo ls /var/lib/piqueld.upgraded/backups
   ```

5. Restore it, as the daemon's user:

   ```console
   sudo -u piqueld piqueld restore --data-dir /var/lib/piqueld /var/lib/piqueld.upgraded/backups/pre-<schema>-<time>
   ```

   Use the upgraded version's `piqueld`: the previous one may not have this
   command. It runs the same checks as `piqueld roll-back`, and restores
   nothing if one fails. Add `--check` to run the checks only. The data
   directory must be empty, but for what `piqueld roll-back` leaves in place:
   `backups`, `telemetry`, `checkouts`, `snapshots`, `buildx` and
   `rolled-back-<time>` directories.

   To restore a backup you [took yourself](#back-up) instead, copy it in
   place, without the upgraded database's `-wal` and `-shm` files, then give
   the daemon's user every restored file. On a new host, its user ID may
   differ from the backup's:

   ```console
   sudo cp -a /safe/location/piqueld-backup/. /var/lib/piqueld/
   sudo chown -R piqueld:piqueld /var/lib/piqueld
   ```

6. Go back to the previous version: on NixOS, run
   `sudo nixos-rebuild switch --rollback`; elsewhere, reinstall the previous
   binary.
7. Start the daemon:

   ```console
   sudo systemctl start piqueld
   ```

   It starts the ingress containers again, on the restored directory.

8. If an application [receives telemetry](telemetry.md#receive-telemetry-in-an-application),
   restart its collector, which still listens in the directory you moved
   aside:

   ```console
   sudo docker restart $(sudo docker ps --quiet --filter volume=/run/piqueld/telemetry)
   ```

   It may fail to start a few times, until piqueld removes the stale socket
   the backup holds.

However you roll back, piqueld then compares what Docker runs with the
deployments in the restored database, and repairs the difference, as it
repairs any drift.

## Troubleshooting

- **The integrity check doesn't print `ok`.** The copy is damaged: take the
  backup again.
- **The previous version refuses to start after an upgrade.** The new version
  updated the database, which older versions refuse. [Roll back](#roll-back)
  to the backup taken before upgrading.
- **The daemon doesn't start after an upgrade, and its log says it failed to
  back up the data directory.** It hasn't updated anything. Fix the cause the
  log names, such as a full disk, then start it again.
- **`piqueld roll-back` fails because no backup has a version it supports.**
  The daemon keeps its last three backups, and each holds the database as it
  was before an upgrade. If they are all newer than the installed version,
  none fits. Run a newer version again, or
  [restore a backup you took yourself](#roll-back-by-hand).
- **The daemon refuses to start because of an unfinished restore.** A
  `piqueld restore` or `piqueld roll-back` stopped part way, or failed, such
  as on a full disk. The next `piqueld roll-back` finishes it from where it
  stopped. Fix the cause its error names, then:

  - On a host that [rolls back on every start](#roll-back-on-every-start),
    start the daemon: `sudo systemctl start piqueld`.
  - Elsewhere, run `piqueld roll-back` as in [rolling back](#roll-back),
    step 3, then start the daemon.

  If you can't run `piqueld roll-back`, such as on NixOS without `rollBack`,
  finish it by hand. The `.restore-<time>` or `.publish-<time>` directory the
  error names only holds a copy of the backup, so nothing is lost by
  removing it. A roll-back already moved most of the state it replaced into
  the `rolled-back-<time>` directory with the same time.

  1. Stop the daemon, so it doesn't start on what's left:

     ```console
     sudo systemctl stop piqueld
     ```

  2. Create a directory named `rolled-back-<anything>` inside the data
     directory, which `piqueld restore` leaves in place:

     ```console
     sudo install -d -o piqueld -g piqueld -m 0700 /var/lib/piqueld/rolled-back-recovered
     ```

     Move the data directory's other contents into it, but for `backups`,
     `telemetry`, `checkouts`, `snapshots`, `buildx`, the
     `rolled-back-<time>` directories and the directory the error names.
  3. Remove the directory the error names.
  4. Restore the backup again, as the daemon's user. A roll-back's backup is
     in `/var/lib/piqueld/backups`; for another restore, use the backup you
     were restoring:

     ```console
     sudo -u piqueld piqueld restore --data-dir /var/lib/piqueld /var/lib/piqueld/backups/pre-<schema>-<time>
     ```

  5. Start the daemon.
- **The daemon can't write to a restored database.** Its files must belong to
  the daemon's user: run `sudo chown -R piqueld:piqueld /var/lib/piqueld`.
