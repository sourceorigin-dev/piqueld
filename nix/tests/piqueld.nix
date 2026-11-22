# The piqueld and piquelctl modules, as the flake's `default` module enables
# them: the daemon starts as its own user, reaches Docker, serves the
# dashboard on localhost, and root runs piquelctl over its socket.
{ modules }:
{ pkgs, ... }:
{
  name = "piqueld";
  nodes.machine = {
    imports = modules;
    services.piqueld = {
      enable = true;
      settings.server.listen_mode = "localhost";
    };
    environment.systemPackages = [ pkgs.curl ];
    virtualisation.diskSize = 4096;
    virtualisation.memorySize = 2048;
  };

  testScript = ''
    import json

    machine.wait_for_unit("piqueld.service")
    machine.wait_for_open_port(7845)

    with subtest("The daemon runs as its own user"):
        machine.succeed("test \"$(ps -o user= -p \"$(systemctl show -P MainPID piqueld)\")\" = piqueld")

    with subtest("It initializes Docker as a single-node Swarm"):
        machine.wait_until_succeeds("test \"$(docker info --format '{{.Swarm.LocalNodeState}}')\" = active")

    with subtest("It serves its health check and dashboard on localhost"):
        machine.succeed("curl -fsS http://127.0.0.1:7845/health")
        machine.succeed("curl -fsSL http://127.0.0.1:7845/ | grep -qi '<html'")

    with subtest("Root runs piquelctl over the socket without signing in"):
        status = json.loads(machine.succeed("piquelctl --json status"))
        assert status["status"] == "running", status
  '';
}
