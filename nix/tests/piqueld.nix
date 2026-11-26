# The piqueld and piquelctl modules, as the flake's `default` module enables
# them: the daemon starts as its own user, after checking whether to roll
# back, reaches Docker, serves the dashboard on localhost, and root runs
# piquelctl over its socket. Then a newer daemon's database rolls back as the
# unit starts, and a database no backup fits stops it.
{ modules }:
{ pkgs, ... }:
let
  # A container to run as one of the daemon's ingress containers, from an
  # image loaded into the engine, which has no network.
  sleeper = pkgs.dockerTools.buildImage {
    name = "sleeper";
    tag = "latest";
    copyToRoot = [ pkgs.busybox ];
    config.Cmd = [
      "sleep"
      "100000"
    ];
  };
in
{
  name = "piqueld";
  nodes.machine = {
    imports = modules;
    services.piqueld = {
      enable = true;
      rollBack = true;
      settings.server.listen_mode = "localhost";
    };
    environment.systemPackages = [
      pkgs.curl
      pkgs.sqlite
    ];
    virtualisation.diskSize = 4096;
    virtualisation.memorySize = 2048;
  };

  testScript = ''
    import json
    import shlex

    D = "/var/lib/piqueld"

    def as_piqueld(command):
        return machine.succeed(f"runuser -u piqueld -- sh -c {shlex.quote(command)}")

    def schema(database):
        return int(machine.succeed(f"sqlite3 {database} 'PRAGMA user_version'").strip())

    def migrate_to(version):
        as_piqueld(f"sqlite3 {D}/piqueld.db 'PRAGMA user_version={version}; UPDATE instance_metadata SET schema_version={version};'")

    def running(container):
        return machine.succeed(f"docker inspect -f '{{{{.State.Running}}}}' {container}").strip() == "true"

    machine.wait_for_unit("piqueld.service")
    machine.wait_for_open_port(7845)

    with subtest("The daemon runs as its own user"):
        machine.succeed("test \"$(ps -o user= -p \"$(systemctl show -P MainPID piqueld)\")\" = piqueld")

    with subtest("It checks whether to roll back first, which a fresh database doesn't need"):
        machine.succeed("journalctl -u piqueld.service | grep -q 'nothing to roll back'")

    with subtest("It initializes Docker as a single-node Swarm"):
        machine.wait_until_succeeds("test \"$(docker info --format '{{.Swarm.LocalNodeState}}')\" = active")

    with subtest("It serves its health check and dashboard on localhost"):
        machine.succeed("curl -fsS http://127.0.0.1:7845/health")
        machine.succeed("curl -fsSL http://127.0.0.1:7845/ | grep -qi '<html'")

    with subtest("Root runs piquelctl over the socket without signing in"):
        status = json.loads(machine.succeed("piquelctl --json status"))
        assert status["status"] == "running", status

    machine.succeed("systemctl stop piqueld.service")
    current = schema(f"{D}/piqueld.db")
    instance = machine.succeed(f"sqlite3 {D}/piqueld.db 'SELECT instance_id FROM instance_metadata'").strip()
    backup = f"{D}/backups/pre-{current}-1000"

    with subtest("A newer daemon migrated the database after backing it up"):
        as_piqueld(f"mkdir -p -m 0700 {backup} && sqlite3 {D}/piqueld.db \".backup {backup}/piqueld.db\"")
        as_piqueld(f"if [ -f {D}/secrets.key ]; then cp {D}/secrets.key {backup}/; fi")
        manifest = json.dumps({"format": 1, "schema_version": current, "daemon_version": "test", "instance_id": instance, "created_at_ms": 1000})
        as_piqueld(f"printf '%s' {shlex.quote(manifest)} > {backup}/backup.json")
        migrate_to(current + 1)
        machine.succeed("docker load < ${sleeper}")
        machine.succeed(f"docker run -d --name ours --label io.piqueld.ingress=true --label io.piqueld.instance={instance} sleeper:latest")
        machine.succeed("docker run -d --name other --label io.piqueld.ingress=true --label io.piqueld.instance=instance-other sleeper:latest")

    with subtest("Starting the unit rolls back, then the daemon serves on the restored database"):
        machine.succeed("systemctl start piqueld.service")
        machine.wait_for_open_port(7845)
        machine.succeed("curl -fsS http://127.0.0.1:7845/health")
        machine.succeed("journalctl -u piqueld.service | grep -q 'restored backup'")
        assert schema(f"{D}/piqueld.db") == current
        aside = machine.succeed(f"ls -d {D}/rolled-back-*").strip()
        assert schema(f"{aside}/piqueld.db") == current + 1
        machine.succeed(f"test -d {backup} && test \"$(stat -c %U {D}/piqueld.db)\" = piqueld")
        assert not running("ours") and running("other")

    with subtest("Without a fitting backup, the unit fails and changes nothing"):
        machine.succeed("systemctl stop piqueld.service")
        migrate_to(current + 1)
        as_piqueld(f"rm -rf {backup}")
        listing = f"cd {D} && find . -not -name '*-wal' -not -name '*-shm' | sort"
        before = machine.succeed(listing)
        machine.execute("systemctl start piqueld.service")
        machine.wait_until_succeeds("journalctl -u piqueld.service | grep -q 'holds no backup it supports'")
        machine.succeed("systemctl stop piqueld.service")
        assert machine.succeed(listing) == before
  '';
}
