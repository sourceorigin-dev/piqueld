# Volumes and files

Keep a service's data across deployments in a named volume, and give services
read-only configuration files.

## Before you start

- You have an application, such as the one from
  [Configuring services](services.md). The examples use `notes`, with a
  PostgreSQL service `db` and an nginx service `web`.
- Run the commands on this page on the host, or on any machine where
  `piquelctl` is [signed in](../getting-started/first-account.md#sign-piquelctl-in).
- Editing an application needs the `apps:write`
  [permission](../accounts/permissions.md#grants) on it, and deploying it
  `apps:deploy`.

## Store data in a volume

A container's own files are lost when it's replaced, as a deployment that
changes its service does. Data that must last, such as a database's, goes in a
named volume.

> **Warning:** the volume starts empty. If `db` already holds data you want to
> keep, [back it up](#back-up-a-volume) first, and restore it after the
> deployment.

Declare the volume, mount it in `db` at `/var/lib/postgresql/data`, then
deploy. The next deployment creates the volume. Later deployments keep its
content. Each environment gets its own volume: environments never share data.

### In the dashboard

1. On the application's **Volumes** tab, select **Add volume** under **Named
   volumes**. In the dialog, enter `data` and select **Add volume**, which
   saves it.
2. On `db`'s **Volume mounts** tab, select **Add mount**, enter `data` under
   **Volume** and `/var/lib/postgresql/data` as the **Container path**, then
   select **Save changes**.

   ![The db service's Volume mounts tab: the data volume mounted at /var/lib/postgresql/data, with Read only unchecked](../../images/applications/volume-mounts.png)

3. Select **Deploy to production**.

### With piquelctl

```console
piquelctl app volume add notes data
piquelctl app service mount set notes db data /var/lib/postgresql/data
piquelctl app deploy notes
```

### In the manifest

1. Declare the volume, at the end of `notes.toml`:

   ```toml
   [[spec.volumes]]
   name = "data"
   ```

2. Mount it in the `db` entry, after its other settings:

   ```diff
    [spec.services.healthcheck]
    type = "command"
    command = ["pg_isready", "-U", "notes"]
    interval_seconds = 5
    start_period_seconds = 30
   +
   +[[spec.services.mounts]]
   +volume = "data"
   +target = "/var/lib/postgresql/data"

    [[spec.services]]
    name = "web"
   ```

3. Deploy:

   ```console
   piquelctl app apply --file notes.toml --deploy
   ```

### Mount a volume read-only

Several services can mount the same volume. One that only reads it can mount
it read-only, such as a web server that serves the files another service
writes to an `uploads` volume. In the dashboard, check the mount's **Read
only**. With `piquelctl`, add `--read-only` to `mount set`. In the manifest,
add `read_only` to the mount:

```toml
[[spec.services.mounts]]
volume = "uploads"
target = "/usr/share/nginx/html/uploads"
read_only = true
```

A service that mounts a volume it can write to stops its old container before
starting the new one, so that two copies never write the same files. It's
briefly unavailable at each deployment; see
[rollout order](services.md#rollout-order).

## What happens to volume data

piqueld tracks every volume it creates. An environment binds each volume name
its manifest declares to one volume, and keeps that binding across
deployments. Volumes of environments are never deleted with them, but those of
previews are:

- **Removing a volume from the manifest** keeps its data, and the
  environment's binding. Declare a volume of the same name again, and the
  environment gets its data back.
- **Deleting an environment** detaches its volumes: they keep their data, and
  piqueld lists them as detached volumes. A new environment never reuses them
  on its own, even with the same name: it gets new, empty volumes, unless you
  [attach](#attach-a-detached-volume) one.
- **Deleting an application** detaches its environments' volumes in the same
  way, but deletes its [previews](previews.md), with their volumes.
- **Deleting a preview** deletes its volumes and their data.

## See an environment's volumes

Each volume has an ID, a Docker name, a size and a state: `attached` while an
environment binds a name to it, `detached` otherwise, such as after its
environment is deleted. piqueld measures each volume's size every 15 minutes.

- **In the dashboard,** the environment's **Overview** lists them under
  **Volumes**, with the name each is bound to and its size.
- **With `piquelctl`,** list the volumes of one environment, or of all of an
  application's:

  ```console
  piquelctl volume list notes production
  ```

  ```text
  volume-019abf3c0e8f7d1a9c2b4e6f80a1b2c3  attached  48.2 MB  notes/production as data
  ```

  `piquelctl volume show <volume>` shows one volume in full.

Listing every volume of the installation, with `piquelctl volume list`, needs
the `system:read` [permission](../accounts/permissions.md#grants).

## Attach a detached volume

A detached volume can serve another environment, such as one you recreated
after deleting it. Attaching it binds one of the volume names the
environment's manifest declares to it. The environment mounts it where its
manifest mounts that name, from its next deployment.

Attaching needs `apps:write` on the application and `system:operate`, since
the volume may hold another application's data. The dashboard can't attach a
volume; use `piquelctl`.

1. Find the volume's ID, and where it was bound before:

   ```console
   piquelctl volume list --detached
   ```

   ```text
   volume-019abf3c0e8f7d1a9c2b4e6f80a1b2c3  detached  48.2 MB  from notes/staging as data
   ```

2. Attach it under a name the environment doesn't bind yet:

   ```console
   piquelctl volume attach notes staging data volume-019abf3c0e8f7d1a9c2b4e6f80a1b2c3
   ```

3. Deploy the environment, with `piquelctl env deploy notes staging`, so its
   services mount the volume.

The first deployment that declares a name binds it to a new volume, and a
name the environment already binds can't take another one: attaching to it
fails with `volume_name_bound`. So attach the volume after declaring the name,
and before deploying it. A name the manifest doesn't declare fails with
`volume_name_undeclared`. Previews delete their volumes, so they take none.

## Delete a detached volume

> **Warning:** deleting a volume deletes its data. It can't be undone.

Only detached volumes can be deleted. Deleting needs `system:operate`.

- **In the dashboard,** the **Daemon status** page lists detached volumes,
  with where each was bound. Select **Delete** next to the volume, then
  confirm.
- **With `piquelctl`:**

  ```console
  piquelctl volume delete volume-019abf3c0e8f7d1a9c2b4e6f80a1b2c3
  ```

## Back up a volume

`piquelctl app exec` runs a command in the service's container and streams its
output, so you can save a backup on your machine. It needs the `apps:exec`
permission; see [running commands](commands.md).

For a database, use its own dump tool, which makes a consistent copy while it
runs:

```console
piquelctl app exec notes db -- pg_dump -U notes notes > notes.sql
```

To restore it, send the dump to an empty database:

```console
piquelctl app exec notes db -i -- psql -q -X -v ON_ERROR_STOP=1 --single-transaction -U notes -f - notes < notes.sql
```

`ON_ERROR_STOP` and `--single-transaction` make `psql` stop at the first
error and keep nothing of the dump, instead of restoring part of it. The dump
creates its tables, so restoring it into a database that already has them
fails.

For other files, archive the directory where the volume is mounted, and
extract the archive there to restore it. For a service `api` that keeps
uploaded files in a volume mounted at `/srv/uploads`:

```console
piquelctl app exec notes api -- tar -cz -C /srv/uploads . > uploads.tar.gz
piquelctl app exec notes api -i -- tar -xz -C /srv/uploads < uploads.tar.gz
```

Don't archive a running database's files this way: the copy may be
inconsistent. To restore, the volume must be mounted writable.

piqueld's own [backup](../operations/backups.md) doesn't include
volumes, so back them up separately.

## Mount a read-only file

piqueld can mount a configuration file you write, such as nginx's
`/etc/nginx/conf.d/default.conf`, into a service:

- **In the dashboard,** use the service's **Files** tab: select **Add file**,
  then enter the **Container path** and the **Content**, and select **Save
  changes**.
- **With `piquelctl`,** read the content from a file on your machine:

  ```console
  piquelctl app file add notes web /etc/nginx/conf.d/default.conf --from ./default.conf
  ```

  `--from` saves the file's content in the manifest when you run it. Use
  `--content` to give the text directly.
- **In the manifest,** add it to the `web` entry, after its other settings:

  ```diff
   [spec.services.healthcheck]
   type = "http"
   port = 80
   path = "/"
  +
  +[[spec.services.files]]
  +target = "/etc/nginx/conf.d/default.conf"
  +content = """
  +server {
  +    listen 80;
  +    location / {
  +        root /usr/share/nginx/html;
  +    }
  +}
  +"""
  ```

Deploy to mount it. Only the services that mount a changed file are replaced.

The file is read-only, owned by root, with mode `0444`: set another in
**Mode**, with `--mode`, or with `mode = "0440"`. piqueld mounts the content exactly as written: it
never replaces `${{ … }}` or anything else in it. Each file holds up to
512,000 bytes of text, and an application's files 2 MiB in total.

An application whose manifest is in a Git repository, as in
[deploying on push](deploy-on-push.md), can also mount files from that
repository; see [files](../reference/manifest.md#files).

## Check that it works

The service's **Files** tab lists its files, and `piquelctl app file list notes`
lists the application's:

```text
SERVICE  TARGET                          MODE  SOURCE
web      /etc/nginx/conf.d/default.conf  0444  inline, 85 bytes
```

To see them in the container, [run a command](commands.md):

```console
$ piquelctl app exec notes web -- ls -l /etc/nginx/conf.d/default.conf
-r--r--r--    1 root     root            85 Oct 24 11:14 /etc/nginx/conf.d/default.conf
```

## Troubleshooting

- **Removing a volume fails with `mount_volume_missing`.** A service still
  mounts it. Remove the mount first, with
  `piquelctl app service mount remove notes db /var/lib/postgresql/data`.
- **The service can't write to a file.** Files are always read-only. An image
  that rewrites its configuration when it starts needs the file copied
  somewhere writable first, for example by its command.
- **Adding a file fails with `file_target_invalid`.** Another file, volume or
  secret of the service already uses that path, or the path is in
  `/run/secrets` or `/run/piqueld`, which piqueld uses.
- **The service can't write to its volume.** A new volume takes the owner of
  the directory the image has at the mount's path, or belongs to root if the
  image has none. A service that runs as another user can only write to a
  volume mounted where its image has a directory it owns.
- **A detached volume has no environment it came from.** piqueld found it in
  Docker, labelled as its own, after its environment had been deleted, before
  piqueld tracked volumes. Its Docker name starts with `piqueld-` and the
  environment's ID.
- **A detached volume comes from an environment that still exists.** The
  environment's manifest dropped that volume name before piqueld tracked
  volumes. Declare the name again, and the environment's next deployment
  binds it to that volume, with its data.
- **A deleted volume is listed again.** Docker couldn't remove it, for example
  because a container you started yourself mounts it. piqueld lists it again as
  detached at its next measurement; delete it again once nothing uses it.
- **The database's data is gone after a new deployment.** Check that the volume
  is mounted where the image keeps its data, such as `/var/lib/postgresql/data`
  for PostgreSQL. Data written anywhere else is lost when the container is
  replaced.
