# A unit serving a local service on the machine's tailnet name, with
# Tailscale Serve, after the service starts. Serve keeps its entries in
# tailscaled's state, one per port: the unit adds its entry when it starts and
# removes it when it stops, so disabling the service takes it off the tailnet on
# the next switch. The entry stays while the service restarts, as for garbage
# collection, and connections fail meanwhile. Tailscale may still be logging in
# at boot, so a failed start is retried.
#
# Used by the build machine and registry modules:
#   serve { name = "the image registry"; service = "docker-registry.service";
#           port = 5000; tailnet = "https"; local = "http://127.0.0.1:5000"; }
{ pkgs, tailscale }:
{
  name,
  service,
  port,
  tailnet,
  local,
}:
{
  description = "Serve ${name} on the tailnet";
  after = [
    service
    "tailscaled.service"
    "tailscaled-autoconnect.service"
    "tailscaled-set.service"
  ];
  wants = [ "tailscaled.service" ];
  wantedBy = [ "multi-user.target" ];
  serviceConfig = {
    Type = "oneshot";
    RemainAfterExit = true;
    ExecStart = "${tailscale} serve --bg --${tailnet}=${toString port} ${local}";
    # Two units may stop at once, and Tailscale refuses the second of two
    # concurrent changes to Serve's configuration: try again after it.
    ExecStop = pkgs.writeShellScript "tailnet-serve-off" ''
      for attempt in 1 2 3 4 5; do
        ${tailscale} serve --${tailnet}=${toString port} off && exit 0
        sleep 1
      done
      exit 1
    '';
    Restart = "on-failure";
    RestartSec = "10s";
    TimeoutStartSec = "30s";
  };
}
