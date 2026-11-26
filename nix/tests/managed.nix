# The piqueld module's managed updates: the package runs until `piqueld
# update` installs a build, which the service then runs, and a rollback that
# piquelctl asks the daemon for, which the module's update units run,
# returns to the package. The build installed is the package's own daemon,
# from a directory, as the VM has no internet access.
{ modules }:
{ pkgs, nodes, ... }:
let
  package = nodes.machine.services.piqueld.package;
in
{
  name = "piqueld-managed";
  nodes.machine = {
    imports = modules;
    services.piqueld = {
      enable = true;
      managedUpdates = true;
      settings.server.listen_mode = "localhost";
      settings.updates.check = false;
    };
    virtualisation.diskSize = 4096;
    virtualisation.memorySize = 2048;
  };

  testScript = ''
    exe = "readlink /proc/$(systemctl show -P MainPID piqueld)/exe"

    machine.wait_for_unit("piqueld.service")
    machine.wait_until_succeeds("piquelctl status")
    machine.wait_for_unit("piqueld-update.path")

    with subtest("Before an update, the package runs"):
        machine.succeed(f"{exe} | grep -q '^/nix/store/'")

    with subtest("piqueld update installs a build and restarts the service on it"):
        machine.succeed(
            "mkdir -p /tmp/next/files"
            " && cp ${package}/bin/.piqueld-wrapped /tmp/next/files/piqueld"
            " && cp ${package}/bin/piquelctl /tmp/next/files/"
            " && echo unit > /tmp/next/files/piqueld.service"
            " && tar -czf /tmp/next/piqueld-9.9.9-x86_64-linux.tar.gz -C /tmp/next files"
            " && cd /tmp/next && sha256sum piqueld-9.9.9-x86_64-linux.tar.gz > SHA256SUMS"
        )
        machine.succeed("piqueld update --version 9.9.9 --from /tmp/next")
        machine.succeed(f"{exe} | grep -q '^/var/lib/piqueld-builds/9.9.9/'")
        machine.succeed("piquelctl status")

    with subtest("A rollback through the daemon returns to the package"):
        machine.succeed("piquelctl update status | grep -q 'Previous build: package'")
        machine.succeed("piquelctl update rollback")
        machine.succeed(f"{exe} | grep -q '^/nix/store/'")
        machine.succeed("piquelctl update status | grep -q 'rollback to package: succeeded'")
        machine.fail("piquelctl update status | grep -q 'Previous build'")
  '';
}
