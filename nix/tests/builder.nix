# The build machine and registry modules, on one machine. Tailscale can't
# log in without a coordination server, so this reaches BuildKit on loopback,
# as Tailscale Serve would from the tailnet, and BuildKit pushes to the
# registry by the machine's name, as it would by its tailnet name; Serve itself
# never runs.
{ modules }:
{ lib, pkgs, ... }:
let
  # A build context whose RUN steps need no base image: a static busybox, for
  # this machine and for an emulated arm64 one.
  context =
    busybox:
    pkgs.runCommand "build-context" { } ''
      mkdir $out
      cp ${busybox}/bin/busybox $out/busybox
      cat > $out/Dockerfile <<'EOF'
      FROM scratch
      COPY busybox /busybox
      RUN ["/busybox", "sh", "-c", "/busybox uname -m > /arch"]
      # Build steps reach the registry by the machine's address, but not on
      # its loopback, whether as their own or through slirp4netns' gateway.
      ARG ADDRESS
      RUN ["/busybox", "sh", "-c", "/busybox wget -q -T 5 -O /dev/null http://$ADDRESS:5000/v2/ && for host in 127.0.0.1 10.0.2.2; do ! /busybox wget -q -T 5 -O /dev/null http://$host:5000/v2/ || exit 1; done"]
      EOF
    '';
  native = context pkgs.pkgsStatic.busybox;
  arm64 = context pkgs.pkgsCross.aarch64-multiplatform.pkgsStatic.busybox;
in
{
  name = "piqueld-builder";
  nodes.machine = {
    imports = modules;
    services.tailscale.enable = true;
    services.piqueld-builder = {
      enable = true;
      emulatedSystems = [ "aarch64-linux" ];
    };
    services.piqueld-registry.enable = true;
    # Also on the machine's address, standing in for Serve, which BuildKit
    # reaches outside its own loopback.
    services.dockerRegistry.listenAddress = lib.mkForce "0.0.0.0";
    environment.systemPackages = [
      pkgs.buildkit
      pkgs.curl
    ];
    virtualisation.diskSize = 4096;
    virtualisation.memorySize = 2048;
  };

  testScript =
    { nodes, ... }:
    ''
      import json

      accept = "-H 'Accept: application/vnd.oci.image.manifest.v1+json' -H 'Accept: application/vnd.docker.distribution.manifest.v2+json'"

      def buildctl(context, platform, output):
          machine.succeed(
              "buildctl --addr tcp://127.0.0.1:1234 build --frontend dockerfile.v0"
              f" --local context={context} --local dockerfile={context}"
              f" --opt platform={platform} --opt build-arg:ADDRESS={address} --output {output}"
              " --metadata-file /tmp/metadata.json"
          )

      def build(context, platform, tag):
          """Builds `context` for `platform` on BuildKit, pushes it under `tag`
          as piqueld does, and returns its digest."""
          buildctl(context, platform, f"type=image,name={address}:5000/piqueld:{tag},push=true,registry.insecure=true")
          return json.loads(machine.succeed("cat /tmp/metadata.json"))["containerimage.digest"]

      def tags():
          """The repository's tags, as piqueld's cleanup lists them."""
          listing = json.loads(machine.succeed("curl -fsS http://127.0.0.1:5000/v2/piqueld/tags/list"))
          return listing["tags"] or []

      def arch(context, platform):
          """The machine its RUN step ran on, from the cache of its build."""
          machine.succeed("rm -rf /tmp/files")
          buildctl(context, platform, "type=local,dest=/tmp/files")
          return machine.succeed("cat /tmp/files/arch").strip()

      # The machine's own address: its name is also 127.0.0.2's.
      address = "${nodes.machine.networking.primaryIPAddress}"

      machine.wait_for_unit("buildkitd.service")
      machine.wait_for_unit("docker-registry.service")
      machine.wait_for_open_port(1234)
      # RootlessKit publishes the port before BuildKit listens behind it.
      machine.wait_until_succeeds("buildctl --addr tcp://127.0.0.1:1234 debug workers")
      machine.wait_for_open_port(5000)

      with subtest("BuildKit runs rootless, as its own user"):
          machine.succeed("test \"$(ps -o user= -C buildkitd)\" = buildkit")
          machine.succeed("grep -q '^buildkit:' /etc/subuid")

      with subtest("A build runs its steps and pushes the image under its tag"):
          digest = build("${native}", "linux/amd64", "instance-1.native")
          machine.succeed(f"curl -fsS {accept} http://127.0.0.1:5000/v2/piqueld/manifests/{digest}")
          assert "instance-1.native" in tags(), tags()

      with subtest("An arm64 build runs its steps under emulation"):
          build("${arm64}", "linux/arm64", "instance-1.arm64")
          assert arch("${arm64}", "linux/arm64") == "aarch64"
          assert arch("${native}", "linux/amd64") == "x86_64"

      with subtest("Deleted images' tags go, their layers are freed, and the registry starts again"):
          blobs = "find /var/lib/docker-registry/docker/registry/v2/blobs -name data | wc -l"
          before = int(machine.succeed(blobs))
          machine.succeed(f"curl -fsS -X DELETE http://127.0.0.1:5000/v2/piqueld/manifests/{digest}")
          # Deleting by digest removes the tag, which cleanup relies on.
          assert tags() == ["instance-1.arm64"], tags()
          # A registry started during collection, such as by a switch, stops
          # it first.
          machine.succeed("systemctl show -p Conflicts docker-registry-collect.service | grep -F docker-registry.service")
          machine.succeed("systemctl start docker-registry-collect.service")
          machine.wait_for_unit("docker-registry.service")
          machine.wait_for_open_port(5000)
          after = int(machine.succeed(blobs))
          assert after < before, f"collection freed nothing: {before} blobs before, {after} after"
          machine.fail(f"curl -fsS {accept} http://127.0.0.1:5000/v2/piqueld/manifests/{digest}")

      with subtest("Both are served on the tailnet"):
          machine.succeed("systemctl cat buildkitd-tailnet.service | grep -F 'serve --bg --tcp=1234 tcp://127.0.0.1:1234'")
          machine.succeed("systemctl cat docker-registry-tailnet.service | grep -F 'serve --bg --https=5000 http://127.0.0.1:5000'")
    '';
}
