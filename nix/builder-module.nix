# A build machine for piqueld (see docs/user/applications/build-machine.md):
# a BuildKit daemon that builds a piqueld host's images and pushes them to a
# build registry, which may be on another machine and needn't be this flake's
# (see registry-module.nix).
#
# It takes no credentials. It listens on loopback, and Tailscale Serve exposes
# it on the machine's tailnet name, so the tailnet's access rules decide who
# reaches it.
{
  config,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.services.piqueld-builder;
  serve = import ./tailnet-serve.nix {
    inherit pkgs;
    tailscale = lib.getExe config.services.tailscale.package;
  };
  buildkitConfig = (pkgs.formats.toml { }).generate "buildkitd.toml" {
    # In rootlesskit's network namespace, which publishes only this port on
    # the machine's loopback.
    grpc.address = [ "tcp://0.0.0.0:${toString cfg.port}" ];
    worker.oci = {
      enabled = true;
      # rootlesskit runs the daemon in a user namespace of its own.
      rootless = true;
      gc = true;
      # BuildKit removes the least recently used cache beyond this.
      maxUsedSpace = cfg.cacheSize;
    };
    worker.containerd.enabled = false;
  };
in
{
  options.services.piqueld-builder = {
    enable = lib.mkEnableOption "a BuildKit daemon that builds a piqueld host's images, reached as `build.builder = \"tcp://<tailnet name>:<port>\"`";
    package = lib.mkPackageOption pkgs "buildkit" { };
    port = lib.mkOption {
      type = lib.types.port;
      default = 1234;
      description = "Port BuildKit listens on, on loopback and on the machine's tailnet name; 1024 or above, since BuildKit runs without root privileges.";
    };
    cacheSize = lib.mkOption {
      type = lib.types.str;
      default = "20GB";
      example = "100GB";
      description = "Most disk the build cache may take. BuildKit removes the least recently used cache beyond it.";
    };
    emulatedSystems = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      default = [ ];
      example = [ "aarch64-linux" ];
      description = "Systems this machine emulates with QEMU, so it can build for a piqueld host of another architecture, such as `aarch64-linux` from an x86_64 machine. Sets `boot.binfmt.emulatedSystems` with static emulators, which work inside build containers. Emulated builds are much slower than native ones.";
    };
  };

  config = lib.mkIf cfg.enable {
    assertions = [
      {
        assertion = config.services.tailscale.enable;
        message = "services.piqueld-builder serves BuildKit on the tailnet: enable services.tailscale";
      }
      {
        # RootlessKit publishes it as the unprivileged buildkit user.
        assertion = cfg.port >= 1024;
        message = "services.piqueld-builder.port must be 1024 or above: BuildKit runs without root privileges";
      }
    ];
    users.groups.buildkit = { };
    users.users.buildkit = {
      isSystemUser = true;
      group = "buildkit";
      home = "/var/lib/buildkit";
      # The user namespace's IDs, which build steps run as.
      autoSubUidGidRange = true;
    };

    boot.binfmt = lib.mkIf (cfg.emulatedSystems != [ ]) {
      inherit (cfg) emulatedSystems;
      # The kernel loads a static emulator when it registers it, so it
      # needn't exist inside the containers that run emulated binaries.
      preferStaticEmulators = lib.mkDefault true;
    };

    # Runs any build it is sent, without credentials, so it listens on
    # loopback, and on the tailnet through Serve. It runs rootless: build
    # steps get the buildkit user's subordinate IDs, never root's.
    systemd.services.buildkitd = {
      description = "BuildKit daemon for piqueld's builds";
      wantedBy = [ "multi-user.target" ];
      after = [ "network.target" ];
      # rootlesskit maps the user namespace's IDs with the setuid newuidmap
      # and newgidmap from /run/wrappers; BuildKit runs build steps with runc,
      # and fetches Git sources, such as `ADD <repository>.git`, with git.
      path = [
        "/run/wrappers"
        pkgs.rootlesskit
        # slirp4netns, and nsenter and ip, which set its device up.
        pkgs.slirp4netns
        pkgs.util-linux
        pkgs.iproute2
        pkgs.runc
        pkgs.git
        cfg.package
      ];
      environment = {
        HOME = "/var/lib/buildkit";
        XDG_RUNTIME_DIR = "/run/buildkit";
      };
      serviceConfig = {
        # Build steps share the daemon's network namespace. slirp4netns
        # gives it one of its own, which reaches the network as the machine
        # does but not the machine's loopback, where local services, such
        # as a registry on this machine, listen. Only BuildKit's port is
        # published there.
        ExecStart = lib.concatStringsSep " " [
          (lib.getExe' pkgs.rootlesskit "rootlesskit")
          "--state-dir=/run/buildkit/rootlesskit"
          "--net=slirp4netns"
          "--disable-host-loopback"
          "--copy-up=/etc"
          "--port-driver=builtin"
          "--publish=127.0.0.1:${toString cfg.port}:${toString cfg.port}/tcp"
          (lib.getExe' cfg.package "buildkitd")
          "--config=${buildkitConfig}"
          "--root=/var/lib/buildkit"
        ];
        User = "buildkit";
        Group = "buildkit";
        StateDirectory = "buildkit";
        StateDirectoryMode = "0700";
        RuntimeDirectory = "buildkit";
        RuntimeDirectoryMode = "0700";
        # The build steps' cgroups belong to the daemon's user.
        Delegate = true;
        KillMode = "mixed";
        Restart = "on-failure";
        RestartSec = "5s";
        # newuidmap is setuid, so the unit can't set NoNewPrivileges, and
        # runc mounts a fresh /proc for each build step, which the kernel
        # refuses while anything is mounted over parts of /proc, as
        # ProtectKernelTunables, ProtectKernelLogs, ProtectProc and
        # ProtectHostname do.
        ProtectSystem = "strict";
        ProtectHome = true;
        PrivateTmp = true;
        ProtectKernelModules = true;
        RestrictRealtime = true;
        LockPersonality = true;
        LimitNOFILE = 1048576;
      };
    };

    systemd.services.buildkitd-tailnet = serve {
      name = "the BuildKit daemon";
      service = "buildkitd.service";
      inherit (cfg) port;
      tailnet = "tcp";
      local = "tcp://127.0.0.1:${toString cfg.port}";
    };
  };
}
