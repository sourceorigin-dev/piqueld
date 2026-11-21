# Development instances

`just dev` runs an isolated piqueld for the current worktree: its own
configuration, data, Unix socket, localhost port, and Docker engine. Any
number of worktrees can run one at once, and none of them touches the host's
Docker Engine or another instance's state.

## Start

```console
just dev
```

This watches the daemon, dashboard, and crate sources, the migrations, the
Cargo manifests and build settings (`.cargo/config.toml`,
`rust-toolchain.toml`), and `piqueld.local.toml` (hidden files aside),
rebuilds the daemon with the
embedded dashboard, and restarts it on every save. Stopping it gives the
daemon its graceful shutdown period before terminating any remaining processes.
`just dev start` does the same in the background and returns once the instance
is ready, printing its dashboard URL.

On first use, `just dev` writes the gitignored `piqueld.local.toml`.
`just dev config` writes it ahead of time
([`just setup-worktree`](#worktree-setup) does), and
`just dev config --force` regenerates it:

| Setting | Value |
| --- | --- |
| `server.data_dir` | `~/.local/state/piqueld-dev/<instance>` (under `$XDG_STATE_HOME` when set), outside `/tmp`, which systemd ages out |
| `server.runtime_dir` | `/tmp/piqueld-dev-<uid>/<instance>`, holding the sockets and logs |
| `server.listen_mode`, `server.port` | `localhost` on the lowest free port from 7846 that no other worktree uses |
| `server.allowed_hosts` | `<instance>.localhost`, the instance's own [browser](#browser) host |
| `auth.public_url` | `http://<instance>.localhost:<port>`: the only origin where passkeys work |
| `docker.socket` | the instance's own engine (below) |

The instance is named after the worktree directory, or `main` for the main
checkout, followed by a hash of its path, so separate clones never share an
instance. Edit the file freely: `just dev` reads the directories, port, and
Docker socket from it.

`/tmp/piqueld-dev-<uid>` holds every instance's runtime directory. Before
any `just dev` command touches a file in the instance's runtime directory, it
creates the directory and its parent if needed and checks that only you can
access them. It refuses to run otherwise, such as when another user created
the directory first.

`just dev` warns about a `runtime_dir` in the shared `/tmp/piqueld-dev`, and
about a `[tailscale]` section that serves on the shared development node
older checkouts ran. To replace either, run `just dev stop`, then
`just dev config --force`; `config --force` refuses to replace the
configuration of a running instance. The next `just dev` recreates the
instance's engine for new directories and keeps its images, Swarm, and
services.

## Browser

Open `<public_url>/dashboard/`, as printed by `just dev start` and
`just dev status`. `public_url` is `http://<instance>.localhost:<port>`, and
passkeys and browser sign-in only work there.

Browsers resolve every `*.localhost` name to the local machine and allow
passkeys on it over HTTP. Use the instance's host, not `localhost` or
`127.0.0.1`: browsers send a host's cookies to all its ports, so on a shared
host every instance would receive the others' session cookies.

### From another machine

When the instance runs on another machine, such as a T3 Code environment over
SSH, forward its port to your own:

1. In the T3 Code desktop app, turn on forwarding for the instance's port in
   the thread's Servers list, or open **Forwarded ports** from the command
   palette and enter the port.
2. Open the same `http://<instance>.localhost:<port>/dashboard/` on your
   machine.

The forward listens on your machine's loopback and relays each connection to
the instance, so the address your browser uses is still `public_url`. Without
T3 Code, `ssh -L <port>:localhost:<port> <host>` does the same.

The forward must keep the instance's port. When something on your machine,
such as an instance of your own, already uses it, T3 Code forwards another
port instead, and sign-in fails there because the address no longer matches
`public_url`. Free the port, or give the instance another one: change
`server.port` and the port in `auth.public_url` in `piqueld.local.toml`.

The daemon runs as your user, so `just ctl` acts as the
[host operator](../user/accounts/recovery.md#the-host-operator) without signing in
(unless `PIQUELD_TOKEN` or a saved login selects an account), and
`just ctl sign-in-link` prints a one-time link that signs the browser in the
same way, even on a new instance. The link works once within 10 minutes; the
session lasts 12 hours and survives restarts. Only features that need an
account, such as passkeys, tokens, and invitations, need one: open the setup
link from `just ctl setup-link` and register its passkey.

## Tailnet features

Instances are only on localhost. To try a feature that needs the daemon's
[tailnet node](../user/reference/configuration.md#tailnet-node), such as
tailnet-bound tokens, give the instance a node of its own in
`piqueld.local.toml`, and remove `auth.public_url` so it becomes the node's
HTTPS URL:

```toml
[tailscale]
enabled = true
hostname = "piqueld-dev-<instance>"
```

The daemon then starts its own `tailscaled`, with its state in the instance's
data directory. On its first start, it logs a login URL to `daemon.log` and
waits until you approve the node, so `just dev start` doesn't report the
instance ready until then. To log in without approval, set `auth_key_file`
to the absolute path of a file holding an auth key. `tailscale` and
`tailscaled` must be on `PATH`, and the tailnet needs MagicDNS and HTTPS
certificates.

## Docker

Each instance runs a private Docker-in-Docker engine in the privileged
container `piqueld-dev-<instance>`, listening on
`<runtime_dir>/docker/docker.sock`. Its images, Swarm, and services persist in
a volume of the same name across restarts. Inspect it with:

```console
DOCKER_HOST=unix:///tmp/piqueld-dev-<uid>/<instance>/docker/docker.sock docker service ls
```

Set `docker.socket = "/var/run/docker.sock"` to use the host engine instead;
`just dev` then starts no engine.

## Commands

| Command | Effect |
| --- | --- |
| `just dev` | Watch, rebuild, and restart in the foreground |
| `just dev start [SECS]` | Start in the background and wait until ready |
| `just dev wait [SECS]` | Wait until the current sources are built and serving, and print the URL (default 900 s) |
| `just dev status` | Print the state, URL, socket, and log files |
| `just dev config [--force]` | Write `piqueld.local.toml` unless it exists, or replace it |
| `just dev stop` | Stop the daemon and watcher; the engine keeps running |
| `just dev clean` | Stop, then delete the engine, its volume, and the instance's state; a container of that name that `just dev` did not create for this worktree is kept |
| `just dev prune` | Stop and clean the instances of worktrees that no longer exist |
| `just ctl ARGS` | Run this worktree's `piquelctl` against the instance |

T3 Code runs [`just setup-worktree`](#worktree-setup) when it creates a
worktree and `just dev stop` when its thread settles (see `t3.json`). An
instance also stops itself once its worktree is removed; `just dev prune`
then deletes its engine, volume, and directories.

`just dev wait` considers sources saved after the current build started as not
yet built, so it is safe to call right after an edit. It fails with the
compiler's output when the build fails and with the daemon's error when it
exits.

## Worktree setup

`just setup-worktree` prepares a new worktree: it runs `just dev config`, then
`just build`. T3 Code runs it when it creates a worktree. It leaves a marker
in `target/worktree-setup/`, so an agent starting in the worktree knows
whether to wait:

| Marker | Meaning |
| --- | --- |
| `running` | Setup is running, as the process whose PID the file holds. If that process is gone, setup was interrupted. |
| `done` | Setup succeeded. |
| `failed` | Setup failed. The file holds the last 40 lines of its output. |

`output.log`, beside the marker, keeps the latest run's whole output. Run
`just setup-worktree` again to retry a failed or interrupted setup, or to
prepare a worktree T3 Code didn't create. A run holds a lock on
`target/worktree-setup/lock`, so a second one refuses to start, and
`flock target/worktree-setup/lock true` waits for it to end.

## Logs

The runtime directory keeps:

- `output.log`: the latest build's output, followed by the daemon's errors.
- `daemon.log`: the daemon's logs as JSON lines, from `piqueld --log-file`, e.g.
  `grep '"level":"ERROR"'`. It keeps every run; `just dev clean` removes it.
- `dev.log`: the terminal output of an instance started with `just dev start`.

The terminal keeps the daemon's usual human-readable output.

## Implementation

`just dev`, `just docker-test`, `just test-playwright`, and `just boundary` run
Rust tasks from `tools/xtask`: `cargo xtask --help` lists them. They talk to
Docker through its API, so they need no Docker CLI beyond resolving the current
context.
