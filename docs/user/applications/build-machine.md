# Building on another machine

Build your applications' images on another machine of your tailnet instead of
the piqueld host, so builds don't compete with the applications the host runs
for CPU, memory and disk. The host still checks repositories out, then sends
each build to the build machine. The build machine pushes the image to a build
registry, and the host pulls it from there.

The build machine and the build registry are separate pieces. The registry
can be on the build machine, on another machine, or any registry that meets
[its requirements](#use-another-registry).

## Before you start

- Your piqueld host builds [images from Git](git-builds.md), and its `docker`
  command has the Buildx plugin.
- A [Tailscale](https://tailscale.com) tailnet, which the host and the build
  machine have joined.
- A Linux machine for the builds.
- A build registry. Keep it on a machine that is always on: the host pulls an
  image from it whenever it no longer has one, such as when you restore an
  older deployment. To run one on your tailnet, enable
  [HTTPS certificates](https://tailscale.com/kb/1153/enabling-https) for the
  tailnet: Docker requires HTTPS of a registry on another machine, and the
  registry serves the tailnet's certificate.
- Neither BuildKit nor the registry asks for credentials, and every build runs
  whatever its Dockerfile says. Only the host should reach BuildKit, and only
  the host and the build machine the registry. [Limit who reaches
  them](#limit-who-reaches-them) with your tailnet's access rules. Both also
  listen on their machine's loopback, so every local user of those machines
  can reach them too.
- Build only repositories you trust. A build step can reach BuildKit itself,
  the registry, and whatever else listens on the build machine's addresses
  other than its loopback.

## Set up the build machine

### On NixOS

piqueld's flake has a module for the build machine,
`nixosModules.piqueld-builder`. On the build machine:

1. Enable BuildKit:

   ```nix
   {
     imports = [ inputs.piqueld.nixosModules.piqueld-builder ];
     services.tailscale.enable = true;
     services.piqueld-builder = {
       enable = true;
       cacheSize = "50GB"; # the build cache's limit, 20GB by default
       emulatedSystems = [ "aarch64-linux" ]; # only for a host of another architecture
     };
   }
   ```

2. Rebuild it, such as with `nixos-rebuild switch`.

BuildKit runs as its own user, `buildkit`, without root privileges. Its build
steps run in a user namespace, as that user's subordinate IDs, and in a
network namespace of their own: they reach the internet and the tailnet as the
build machine does, but not its loopback. BuildKit listens on loopback, and the
module exposes it on port 1234 of the machine's tailnet name with
[Tailscale Serve](https://tailscale.com/kb/1312/serve), so no firewall port
opens. It keeps its cache in `/var/lib/buildkit`. The [NixOS module
reference](../reference/nixos.md#build-machine-and-build-registry) lists every
option.

The images are built for the host's architecture. When it differs from the
build machine's, such as an arm64 host and an x86_64 build machine, the build
machine emulates it, which `emulatedSystems` enables. Emulated builds are much
slower than native ones.

### Without NixOS

1. Run [BuildKit](https://github.com/moby/buildkit) without root privileges,
   as its
   [rootless guide](https://github.com/moby/buildkit/blob/master/docs/rootless.md)
   describes, listening on loopback:

   ```console
   rootlesskit --net=slirp4netns --disable-host-loopback --copy-up=/etc \
     --port-driver=builtin --publish=127.0.0.1:1234:1234/tcp \
     buildkitd --addr tcp://0.0.0.0:1234
   ```

   The build steps then run in a network namespace of their own, which can't
   reach the machine's loopback, and only BuildKit's port is published there.
   Run it as a service of its own user, which needs subordinate IDs in
   `/etc/subuid` and `/etc/subgid`.

2. Serve it on the tailnet:

   ```console
   sudo tailscale serve --bg --tcp=1234 tcp://127.0.0.1:1234
   ```

   Tailscale keeps this setting across restarts.

3. To build for another architecture, install QEMU's emulators:

   ```console
   sudo docker run --privileged --rm tonistiigi/binfmt --install arm64
   ```

## Set up the build registry

### On NixOS

piqueld's flake has a module for a build registry,
`nixosModules.piqueld-registry`. It can run on the build machine or on another
machine. On that machine:

1. Enable the registry:

   ```nix
   {
     imports = [ inputs.piqueld.nixosModules.piqueld-registry ];
     services.tailscale.enable = true;
     services.piqueld-registry.enable = true;
   }
   ```

2. Rebuild it, such as with `nixos-rebuild switch`.

The registry listens on loopback, and the module exposes it on port 5000 of the
machine's tailnet name, over HTTPS. It keeps its images in
`/var/lib/docker-registry`. It allows deletes, so images can be deleted from
it. Every Sunday at 04:00, or when the machine is next on if it was off then,
it stops for a few minutes to free the layers of the deleted images; set
`garbageCollectDates` to change when.

### Without NixOS

1. Run a registry with deletes enabled, listening on loopback:

   ```console
   sudo docker run -d --name registry --restart always \
     -p 127.0.0.1:5000:5000 -v /var/lib/registry:/var/lib/registry \
     -e REGISTRY_STORAGE_DELETE_ENABLED=true -e REGISTRY_HTTP_RELATIVEURLS=true \
     registry:3
   ```

2. Serve it on the tailnet, over HTTPS:

   ```console
   sudo tailscale serve --bg --https=5000 http://127.0.0.1:5000
   ```

3. Once a week, stop the registry to free the layers of deleted images, such
   as from a systemd timer:

   ```console
   sudo sh -c 'docker stop registry || exit
     docker run --rm -v /var/lib/registry:/var/lib/registry registry:3 \
       garbage-collect /etc/distribution/config.yml
     collected=$?
     docker start registry && exit "$collected"'
   ```

   The registry starts again even if collection fails, and the command fails
   when either does.

   Never add `--delete-untagged`: piqueld pushes its images without tags, so
   it would delete all of them. A push during collection could lose layers,
   so the registry stays stopped meanwhile.

### Use another registry

Any registry works as the build registry if:

- the build machine can push to it, and the host pull from it, under the
  one name `build.repository` gives, without credentials: piqueld has none to
  give;
- it serves HTTPS with a certificate from a public authority, such as the
  tailnet's;
- it allows deletes, if images are to be deleted from it.

piqueld pushes every image by digest, without a tag, to the one repository
`build.repository` names.

## Limit who reaches them

In your tailnet's [access rules](https://tailscale.com/kb/1393/access-control),
let only the host reach BuildKit, and only the host and the build machine reach
the registry. Rules only add access: first remove the default rule that lets
every device reach every other, and any other rule that covers these
machines' ports, or the rules below restrict nothing. With
[tags](https://tailscale.com/kb/1068/tags) for each machine, such as
`tag:piqueld`, `tag:build` and `tag:registry`:

```json
"grants": [
  { "src": ["tag:piqueld"], "dst": ["tag:build"], "ip": ["tcp:1234"] },
  { "src": ["tag:piqueld", "tag:build"], "dst": ["tag:registry"], "ip": ["tcp:5000"] }
]
```

Containers on the host reach the tailnet as the host itself. If your
applications shouldn't reach BuildKit or the registry, keep them off the
tailnet with the host's firewall.

## Point the host at them

Name BuildKit and the build registry in the host's [daemon
configuration](../reference/configuration.md#build-machine), by their tailnet
names. `repository` is a repository on the registry, such as `piqueld`. On
NixOS:

```nix
services.piqueld.settings.build = {
  builder = "tcp://build.example.ts.net:1234";
  repository = "registry.example.ts.net:5000/piqueld";
};
```

In `config.toml`:

```toml
[build]
builder = "tcp://build.example.ts.net:1234"
repository = "registry.example.ts.net:5000/piqueld"
```

The daemon only reads its configuration when it starts. On NixOS, rebuild the
host, such as with `sudo nixos-rebuild switch`, which restarts it. Otherwise,
restart it:

```console
sudo systemctl restart piqueld
```

Give each piqueld installation its own repository.

## Check that it works

On the host, check that BuildKit answers:

```console
$ piquelctl builder check
Build machine: reachable at 2026-11-18 21:04:12Z
```

In the dashboard, the top of the **Builds** page shows the build machine, and
**Check now** checks it again.

Then build an application again, with **Rebuild** on its page in the dashboard,
or with `piquelctl`:

```console
piquelctl app deploy welcome --rebuild
```

`piquelctl build list` ends the build's line with `on the build machine`, and
the dashboard lists it as **Built on** the build machine.

## While the build machine is off

The build machine only has to be on while a deployment builds. Applications
keep running without it, and so do restores, promotions, restarts and
deployments that [reuse an image](git-builds.md#how-it-builds), such as one
that only changes a secret.

A deployment that has to build fails with `builder_unavailable`. Deploy it
again once the build machine is back, or set `build.unavailable = "local"` to
build on the host instead, where builds compete with your applications.

A restore whose image the host no longer has pulls it from the build registry,
so it needs the registry to be reachable, wherever it runs.

## Troubleshooting

- **`piquelctl builder check` says the build machine is unreachable.** Check
  that BuildKit runs, with `systemctl status buildkitd` on NixOS, that
  `tailscale serve status` on the build machine lists port 1234, and that
  your access rules let the host reach it.
- **The build fails to push, or the host fails to pull.** The build machine
  or the host can't reach the registry under its name. Check that
  `curl https://registry.example.ts.net:5000/v2/` answers `{}` on both. A
  machine whose resolvers come before Tailscale's MagicDNS may not resolve
  tailnet names at all.
- **A build fails with `exec format error`.** The host's architecture differs
  from the build machine's, which doesn't emulate it. Add it to
  `emulatedSystems`, or install QEMU's emulators without NixOS.
- **`buildkitd-tailnet` or `docker-registry-tailnet` fails after a
  rebuild.** Tailscale isn't logged in yet, and the unit tries again every
  10 seconds until it is: run `sudo tailscale up` on that machine. For the
  registry, also check that the tailnet has HTTPS certificates enabled.
- **A build or a pull fails on Sunday at 04:00.** The registry the
  `piqueld-registry` module runs is collecting garbage. Deploy again a few
  minutes later, or move collection to a quieter time with
  `garbageCollectDates`.
