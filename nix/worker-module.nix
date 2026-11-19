# A piqueld worker node: this machine joins a piqueld host's Swarm as a
# worker, and its Docker Engine answers the host over one network interface,
# such as the tailnet's. The host names it under `services.piqueld.settings.nodes`.
{
  config,
  lib,
  ...
}:
let
  cfg = config.services.piqueld-worker;
  # Docker's Swarm ports: the manager's API, node gossip and overlay traffic.
  swarmPort = 2377;
  gossipPort = 7946;
  overlayPort = 4789;
in
{
  options.services.piqueld-worker = {
    enable = lib.mkEnableOption "a piqueld worker node, joined to a piqueld host's Swarm";
    manager = lib.mkOption {
      type = lib.types.str;
      example = "100.64.0.1";
      description = "The piqueld host's docker.swarm_address, where this machine joins its Swarm.";
    };
    address = lib.mkOption {
      type = lib.types.str;
      example = "100.64.0.2";
      description = "This machine's IP address on the network the host reaches it over, such as its tailnet address. The Swarm reaches the node there.";
    };
    interface = lib.mkOption {
      type = lib.types.str;
      default = "tailscale0";
      description = "The network interface of address. The firewall opens the Docker Engine's and the Swarm's ports on it only.";
    };
    enginePort = lib.mkOption {
      type = lib.types.port;
      default = 2375;
      description = "Port the Docker Engine listens on, for the host's nodes.<name>.docker = \"tcp://<this machine>:<port>\". The connection is neither authenticated nor encrypted and controls this machine's engine, so only the host should reach it: the firewall opens it on interface only, and the network's access rules should allow nothing but the host.";
    };
    joinTokenFile = lib.mkOption {
      type = lib.types.strMatching "/.+";
      example = "/run/agenix/piqueld-worker-token";
      description = "Host file with the Swarm's worker join token, from `docker swarm join-token -q worker` on the piqueld host, such as an agenix secret. It is read to join, and once joined, never again.";
    };
  };

  config = lib.mkIf cfg.enable {
    virtualisation.docker.enable = true;
    # On every address, so the engine starts before the tailnet is up; the
    # firewall only opens the port on interface.
    virtualisation.docker.listenOptions = [
      "/run/docker.sock"
      "tcp://0.0.0.0:${toString cfg.enginePort}"
    ];
    networking.firewall.interfaces.${cfg.interface} = {
      allowedTCPPorts = [
        cfg.enginePort
        gossipPort
      ];
      allowedUDPPorts = [
        gossipPort
        overlayPort
      ];
    };
    systemd.services.piqueld-worker-join = {
      description = "Join the piqueld host's Swarm as a worker";
      wantedBy = [ "multi-user.target" ];
      requires = [ "docker.service" ];
      after = [
        "docker.service"
        "network-online.target"
      ];
      wants = [ "network-online.target" ];
      path = [ config.virtualisation.docker.package ];
      serviceConfig = {
        Type = "oneshot";
        RemainAfterExit = true;
        # The tailnet may not have its address yet: try again until it does.
        Restart = "on-failure";
        RestartSec = "30s";
        LoadCredential = "join-token:${cfg.joinTokenFile}";
      };
      script = ''
        if [ "$(docker info --format '{{.Swarm.LocalNodeState}}')" != inactive ]; then
          exit 0
        fi
        docker swarm join \
          --token "$(cat "$CREDENTIALS_DIRECTORY/join-token")" \
          --advertise-addr ${cfg.address} \
          --data-path-addr ${cfg.address} \
          ${cfg.manager}:${toString swarmPort}
      '';
    };
  };
}
