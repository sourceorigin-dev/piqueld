# Adding a worker node

A worker node is another machine joined to the host's Docker Swarm, such as
a larger machine on your tailnet. The host stays the Swarm's only manager and
keeps running the daemon. piqueld reaches each worker through its Docker
Engine, for what lives on that machine.

This page joins a machine named `nixosbtw` to the Swarm of a host whose
tailnet address is `100.64.0.1`. The worker's tailnet address is
`100.64.0.2`.

## Before you start

- A second Linux machine with Docker Engine, and a network between it and
  the host that encrypts its traffic, such as a
  [tailnet](https://tailscale.com). Its access rules should let only the
  host reach the worker's Docker Engine, and only the two machines reach
  each other's Swarm ports: 2377/tcp on the host, and 7946/tcp, 7946/udp
  and 4789/udp on both.
- Root on both machines.
- Anyone who controls the host controls every worker: the Swarm's manager
  can run any container on them, and piqueld reaches their Docker Engine
  without credentials.

## Put the Swarm on the tailnet

Workers reach the Swarm at the address its manager advertises. piqueld
creates the Swarm on 127.0.0.1 unless
[`docker.swarm_address`](../reference/configuration-settings.md#dockerswarm_address)
names another address, and Swarm can't move a manager to another address
later.

1. On the host, set `docker.swarm_address` to the host's tailnet address in
   `/etc/piqueld/config.toml`:

   ```toml
   [docker]
   swarm_address = "100.64.0.1"
   ```

   With the [NixOS module](../reference/nixos.md), set
   `services.piqueld.settings.docker.swarm_address`.

2. If piqueld already runs on the host, its Swarm is on 127.0.0.1: re-create
   it on the new address.

   **This stops every application for a few minutes.** It deletes every
   service, network, secret and config of the Swarm. Your volumes and images
   stay, and piqueld keeps its own copy of everything it deployed, secret
   values included.

   ```console
   sudo systemctl stop piqueld
   sudo -u piqueld piqueld --config /etc/piqueld/config.toml swarm recreate --yes
   sudo systemctl start piqueld
   ```

   On NixOS, pass the generated configuration instead, the `--config` file in
   `systemctl cat piqueld`. Once it starts, piqueld deploys every environment
   and preview again, from what it last deployed.

## Set up the worker

The worker's Docker Engine listens on the tailnet, and the machine joins the
Swarm as a worker.

### With the NixOS module

Import `inputs.piqueld.nixosModules.piqueld-worker` in the worker's
configuration:

```nix
{
  imports = [ inputs.piqueld.nixosModules.piqueld-worker ];
  services.piqueld-worker = {
    enable = true;
    manager = "100.64.0.1";
    address = "100.64.0.2";
    joinTokenFile = "/run/agenix/piqueld-worker-token";
  };
}
```

`joinTokenFile` holds the Swarm's join token for workers. Print it on the
host, and save it as a secret on the worker, such as with agenix:

```console
sudo docker swarm join-token --quiet worker
```

The module opens the Docker Engine on port 2375 and the Swarm's ports on
`tailscale0` only, and joins the Swarm once. The
[NixOS module reference](../reference/nixos.md#worker-nodes) lists its
options.

### By hand

1. Make the worker's Docker Engine listen on port 2375 too, in
   `/etc/docker/daemon.json`, then restart Docker:

   ```json
   { "hosts": ["unix:///var/run/docker.sock", "tcp://100.64.0.2:2375"] }
   ```

   Open port 2375 to the host only.

2. Print the join command on the host:

   ```console
   sudo docker swarm join-token worker
   ```

3. Run it on the worker, adding its tailnet address:

   ```console
   sudo docker swarm join --token <token> --advertise-addr 100.64.0.2 --data-path-addr 100.64.0.2 100.64.0.1:2377
   ```

## Name the worker

piqueld only accepts workers its configuration names. While another machine
is in the Swarm under no name, deployments fail with
`swarm_topology_unsupported`.

1. On the host, name the worker and its Docker Engine:

   ```toml
   [nodes.nixosbtw]
   docker = "tcp://100.64.0.2:2375"
   ```

   With the NixOS module, set
   `services.piqueld.settings.nodes.nixosbtw.docker`. The module then opens
   the Swarm's ports on `tailscale0`, or the interface
   `services.piqueld.swarmInterface` names.

2. Restart piqueld:

   ```console
   sudo systemctl restart piqueld
   ```

Before its next change, piqueld asks each named worker's Docker Engine for
its Swarm node, and labels that node with the worker's name. It then knows
the worker by name, even while the machine is off.

The dashboard and `piquelctl` show worker nodes, but don't change them:
they're part of the daemon's configuration.

## Check that it works

In the dashboard, **System status**, on the **Overview** and **Daemon
status** pages, lists the Swarm and each worker. **Host settings** shows the
configuration's **Worker nodes**.

With `piquelctl`:

```console
$ piquelctl status
…
Swarm address: 100.64.0.1
Worker node: nixosbtw at tcp://100.64.0.2:2375 (ready, engine reachable)
```

A worker is `down` while Swarm doesn't hear from it, such as while the
machine is off. What runs on the host carries on.

## Remove a worker

1. On the worker, leave the Swarm:

   ```console
   sudo docker swarm leave
   ```

2. On the host, remove its node, then its `[nodes]` table, and restart
   piqueld:

   ```console
   sudo docker node rm nixosbtw
   sudo systemctl restart piqueld
   ```

## Troubleshooting

- **Deployments fail with `swarm_topology_unsupported`.** The Swarm has a
  node piqueld doesn't accept: another manager, a worker no `[nodes]` table
  names, or one whose Docker Engine didn't answer when it joined. The
  daemon's log (`journalctl -u piqueld`) names the node. Name the worker,
  make its engine reachable, or remove the node with `docker node rm`.
- **Status says the Swarm must move to `docker.swarm_address`.** The Swarm
  advertises another address, where workers can't stay joined:
  [re-create it](#put-the-swarm-on-the-tailnet).
- **A worker is `not joined`, with its engine unreachable.** piqueld can't
  reach its Docker Engine to match it. Check the `docker` address in
  `[nodes]`, the worker's firewall, and the tailnet's access rules.
- **A worker stays `down` after it restarts.** It can't reach the host's
  port 2377. Check the host's firewall and the tailnet's access rules.
- **A worker that joined again shows up twice.** Joining again gives it a new
  node. piqueld removes the earlier one, while it is down, once it matches
  the new one.
