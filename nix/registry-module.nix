# A build registry for piqueld (see docs/user/applications/build-machine.md):
# the registry a build machine pushes a piqueld host's images to and the host
# pulls them from, on any machine, such as an always-on one while the build
# machine is off at times. Deletes are enabled, so images no deployment needs
# can be deleted, and garbage collection frees their layers.
#
# It takes no credentials. It listens on loopback, and Tailscale Serve exposes
# it on the machine's tailnet name, over HTTPS with the tailnet's certificate,
# which Docker requires of a registry on another machine, so the tailnet's
# access rules decide who reaches it.
{
  config,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.services.piqueld-registry;
  registry = config.services.dockerRegistry;
  serve = import ./tailnet-serve.nix {
    inherit pkgs;
    tailscale = lib.getExe config.services.tailscale.package;
  };
in
{
  options.services.piqueld-registry = {
    enable = lib.mkEnableOption "an image registry a build machine pushes a piqueld host's images to and the host pulls them from, reached as `build.repository = \"<tailnet name>:<port>/<path>\"`, with deletes enabled";
    port = lib.mkOption {
      type = lib.types.port;
      default = 5000;
      description = "Port of the registry, over HTTP on loopback and over HTTPS on the machine's tailnet name.";
    };
    garbageCollectDates = lib.mkOption {
      type = lib.types.str;
      default = "Sun 04:00";
      description = "When the registry frees the layers of deleted images, in systemd.time(7) format, or when the machine is next on if it was off then. The registry stops while it collects, so a push or pull at that time fails.";
    };
  };

  config = lib.mkIf cfg.enable {
    assertions = [
      {
        assertion = config.services.tailscale.enable;
        message = "services.piqueld-registry serves the registry on the tailnet: enable services.tailscale";
      }
      {
        assertion = !config.services.dockerRegistry.enableGarbageCollect;
        message = "services.piqueld-registry collects garbage with the registry stopped: leave services.dockerRegistry.enableGarbageCollect off";
      }
    ];
    services.dockerRegistry = {
      enable = true;
      listenAddress = "127.0.0.1";
      inherit (cfg) port;
      # So that images no deployment needs can be deleted.
      enableDelete = true;
      # Behind Serve, the registry answers with paths, not URLs naming its
      # loopback address.
      extraConfig.http.relativeurls = true;
    };
    # The registry otherwise exports traces to an OpenTelemetry collector on
    # localhost:4318, logging an error each time none answers.
    systemd.services.docker-registry.environment.OTEL_TRACES_EXPORTER = lib.mkDefault "none";

    # Frees the layers no remaining image refers to. piqueld pushes images
    # by digest, without tags, so `--delete-untagged` would delete every
    # image: plain collection only frees what deleted images used. A push
    # during collection could lose layers, so the two conflict: starting
    # collection stops the registry, and anything that starts the registry
    # meanwhile, such as a system switch, stops collection first. The
    # registry starts again however collection ends.
    systemd.services.docker-registry-collect = {
      description = "Free the layers of deleted images in the registry";
      restartIfChanged = false;
      unitConfig.X-StopOnRemoval = false;
      startAt = cfg.garbageCollectDates;
      conflicts = [ "docker-registry.service" ];
      # Any ordering makes systemd stop one before it starts the other.
      after = [ "docker-registry.service" ];
      onSuccess = [ "docker-registry.service" ];
      onFailure = [ "docker-registry.service" ];
      serviceConfig = {
        Type = "oneshot";
        # Stopped by the conflict, it isn't failed: nothing is lost, and the
        # next run collects the rest.
        SuccessExitStatus = "SIGTERM";
        User = "docker-registry";
        Group = "docker-registry";
        ExecStart = "${lib.getExe registry.package} garbage-collect ${registry.configFile}";
      };
    };
    # A machine that is off at that time collects once it is back.
    systemd.timers.docker-registry-collect.timerConfig.Persistent = true;

    systemd.services.docker-registry-tailnet = serve {
      name = "the image registry";
      service = "docker-registry.service";
      inherit (cfg) port;
      tailnet = "https";
      local = "http://127.0.0.1:${toString cfg.port}";
    };
  };
}
