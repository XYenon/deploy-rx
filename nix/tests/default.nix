# SPDX-FileCopyrightText: 2024 Serokell <https://serokell.io/>
#
# SPDX-License-Identifier: MPL-2.0

{ pkgs , inputs , ... }:
let
  inherit (pkgs) lib;

  inherit (import "${pkgs.path}/nixos/tests/ssh-keys.nix" pkgs) snakeOilPrivateKey;

  # Include all build dependencies to be able to build profiles offline
  allDrvOutputs = pkg: pkgs.runCommand "allDrvOutputs" { refs = pkgs.writeClosure pkg.drvPath; } ''
    touch $out
    while read ref; do
      case $ref in
        *.drv)
          cat $ref >>$out
          ;;
      esac
    done <$refs
  '';

  sshWrapper = pkgs.writeShellScript "deploy-rx-test-ssh-wrapper" ''
    set -eu
    ${pkgs.coreutils}/bin/mkdir -p /tmp/deploy-rx-e2e
    printf '%s\n' "$*" >> /tmp/deploy-rx-e2e/ssh.log
    exec ${pkgs.openssh}/bin/ssh "$@"
  '';

  nixWrapper = pkgs.writeShellScript "deploy-rx-test-nix-wrapper" ''
    set -eu
    ${pkgs.coreutils}/bin/mkdir -p /tmp/deploy-rx-e2e
    printf '%s\n' "$*" >> /tmp/deploy-rx-e2e/nix.log
    exec ${pkgs.nix}/bin/nix "$@"
  '';

  nomWrapper = pkgs.writeShellScript "deploy-rx-test-nom-wrapper" ''
    set -eu
    ${pkgs.coreutils}/bin/mkdir -p /tmp/deploy-rx-e2e
    printf '%s\n' "$*" >> /tmp/deploy-rx-e2e/nom.log
    exec ${pkgs.nix-output-monitor}/bin/nom "$@"
  '';

  deployWrapper = pkgs.writeShellScript "deploy-rx-test-deploy-wrapper" ''
    exec ${pkgs.deploy-rx.deploy-rx}/bin/deploy --fast-connection true "$@"
  '';

  mkTest = {
    name ? "",
    flakes ? true,
    isLocal ? true,
    multiHost ? false,
    scenarioScript,
  }: let
    remoteNodeNames = [ "server" ] ++ lib.optionals multiHost [ "server2" ];

    mkServerNode = nodeName: extraModules: { nodes, ... }: {
      imports = [
       ./server.nix
       (import ./common.nix { inherit inputs pkgs flakes; })
      ] ++ extraModules;
      virtualisation.additionalPaths = lib.optionals (!isLocal) [
        pkgs.hello
        pkgs.figlet
        (allDrvOutputs (builtins.getAttr nodeName nodes).system.build.toplevel)
        pkgs.deploy-rx.deploy-rx
      ];
    };

    nodes = {
      server = mkServerNode "server" [ ];
      client = { nodes, ... }: {
        imports = [ (import ./common.nix { inherit inputs pkgs flakes; }) ];
        environment.systemPackages = [ pkgs.deploy-rx.deploy-rx ];
        # nix evaluation takes a lot of memory, especially in non-flake usage
        virtualisation.memorySize = lib.mkForce 4096;
        virtualisation.additionalPaths = lib.optionals isLocal (
          [
            pkgs.hello
            pkgs.figlet
          ] ++ map (nodeName: allDrvOutputs (builtins.getAttr nodeName nodes).system.build.toplevel) remoteNodeNames
        );
      };
    } // lib.optionalAttrs multiHost {
      server2 = mkServerNode "server2" [
        {
          services.openssh.ports = [ 2222 ];
        }
      ];
    };

    flakeInputs = ''
      deploy-rx.url = "${../..}";
      deploy-rx.inputs.utils.follows = "utils";
      deploy-rx.inputs.flake-compat.follows = "flake-compat";

      nixpkgs.url = "${inputs.nixpkgs}";
      utils.url = "${inputs.utils}";
      utils.inputs.systems.follows = "systems";
      systems.url = "${inputs.utils.inputs.systems}";
      flake-compat.url = "${inputs.flake-compat}";
      flake-compat.flake = false;

      enable-flakes.url = "${builtins.toFile "use-flakes" (if flakes then "true" else "false")}";
      enable-flakes.flake = false;
    '';

    flake = builtins.toFile "flake.nix"
      (lib.replaceStrings [ "##inputs##" ] [ flakeInputs ] (builtins.readFile ./deploy-flake.nix));

    flakeCompat = builtins.toFile "default.nix" ''
      (import
        (
          let
            lock = builtins.fromJSON (builtins.readFile ./flake.lock);
          in
          fetchTarball {
            url = "https://not-used-we-fetch-by-hash";
            sha256 = lock.nodes.flake-compat.locked.narHash;
          }
        )
        { src = ./.; }
      ).defaultNix
    '';

  in pkgs.testers.nixosTest {
    inherit nodes name;

    testScript = { nodes, ... }: let
      serverNetworkJSON = pkgs.writeText "server-network.json"
        (builtins.toJSON nodes.server.system.build.networkConfig);
    in ''
if True:
      import shlex

      workspace = "/root/tmp"
      raw_deploy_cmd = "${pkgs.deploy-rx.deploy-rx}/bin/.deploy-wrapped"
      ssh_wrapper_source = "${sshWrapper}"
      nix_wrapper_source = "${nixWrapper}"
      nom_wrapper_source = "${nomWrapper}"
      deploy_wrapper_source = "${deployWrapper}"

      def client_sh(command, timeout=900):
          return client.succeed(command, timeout=timeout)

      def client_fail(command, timeout=900):
          return client.fail(command, timeout=timeout)

      def work(command, timeout=900):
          return client.succeed(f"cd {workspace} && PATH=/tmp/wrappers:$PATH {command}", timeout=timeout)

      def work_fail(command, timeout=900):
          return client.fail(f"cd {workspace} && PATH=/tmp/wrappers:$PATH {command}", timeout=timeout)

      def install_wrapper(name, source):
          client.succeed(
              f"mkdir -p /tmp/wrappers && cp {shlex.quote(source)} /tmp/wrappers/{name} && chmod +x /tmp/wrappers/{name}"
          )

      def reset_logs():
          client.succeed("rm -rf /tmp/deploy-rx-e2e /tmp/wrappers && mkdir -p /tmp/deploy-rx-e2e")

      start_all()

      # Prepare
      client_sh(f"rm -rf {workspace} && mkdir -p {workspace}")
      client_sh(f"cp ${flake} {workspace}/flake.nix")
      client_sh(f"cp ${flakeCompat} {workspace}/default.nix")
      client_sh(f"cp ${./server.nix} {workspace}/server.nix")
      client_sh(f"cp ${./common.nix} {workspace}/common.nix")
      client_sh(f"cp ${serverNetworkJSON} {workspace}/network.json")
      work("nix --extra-experimental-features flakes flake lock")

      # Setup SSH key
      client_sh("mkdir -m 700 /root/.ssh")
      client_sh('cp --no-preserve=mode ${snakeOilPrivateKey} /root/.ssh/id_ed25519')
      client_sh("chmod 600 /root/.ssh/id_ed25519")

      # Test SSH connection
      server.wait_for_open_port(22)
      client.wait_for_unit("network.target")
      client_sh(
        "ssh -o UserKnownHostsFile=/dev/null -o StrictHostKeyChecking=no server 'echo hello world' >&2",
        timeout=30
      )
      server.succeed("mkdir -p /nix/var/nix/profiles/deploy-rx-tests")
      if ${if multiHost then "True" else "False"}:
          server2.wait_for_open_port(2222)
          client_sh(
              "ssh -p 2222 -o UserKnownHostsFile=/dev/null -o StrictHostKeyChecking=no server2 'echo hello world' >&2",
              timeout=30,
          )
          server2.succeed("mkdir -p /nix/var/nix/profiles/deploy-rx-tests")

      reset_logs()
      install_wrapper("deploy", deploy_wrapper_source)

${scenarioScript}
    '';
  };

  mkSimpleDeployTest = {
    name,
    deployArgs,
    user ? "root",
    flakes ? true,
    isLocal ? true,
  }:
    mkTest {
      inherit name flakes isLocal;
      scenarioScript = ''
      server.fail("su ${user} -l -c 'hello | figlet'")
      work("deploy ${deployArgs}", timeout=600)
      server.succeed("su ${user} -l -c 'hello | figlet' >&2")
      '';
    };
in {
  # Deployment with client-side build
  local-build = mkSimpleDeployTest {
    name = "local-build";
    deployArgs = "-s .#server -- --offline";
  };
  # Deployment with server-side build
  remote-build = mkSimpleDeployTest {
    name = "remote-build";
    isLocal = false;
    deployArgs = "-s .#server --remote-build -- --offline";
  };
  non-flake-remote-build = mkSimpleDeployTest {
    name = "non-flake-remote-build";
    isLocal = false;
    flakes = false;
    deployArgs = "-s .#server --remote-build";
  };
  # Deployment with overridden options
  options-overriding = mkSimpleDeployTest {
    name = "options-overriding";
    deployArgs = lib.concatStrings [
      "-s .#server-override"
      " --hostname server --profile-user root --ssh-user root --sudo 'sudo -u'"
      " --ssh-opts='-o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null'"
      " --confirm-timeout 30 --activation-timeout 30"
      " -- --offline"
    ];
  };
  # User profile deployment
  profile = mkSimpleDeployTest {
    name = "profile";
    user = "deploy";
    deployArgs = "-s .#profile -- --offline";
  };
  hyphen-ssh-opts-regression = mkSimpleDeployTest {
    name = "hyphen-ssh-opts-regression";
    user = "deploy";
    deployArgs = "-s .#profile --ssh-opts '-p 22 -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null' -- --offline";
  };
  # Deployment using a non-flake nix
  non-flake-build = mkSimpleDeployTest {
    name = "non-flake-build";
    flakes = false;
    deployArgs = "-s .#server";
  };
  non-flake-with-flakes = mkSimpleDeployTest {
    name = "non-flake-with-flakes";
    flakes = true;
    deployArgs = "--file . --targets server";
  };

  dry-activate-selects-dry-script = mkTest {
    name = "dry-activate-selects-dry-script";
    scenarioScript = ''
      server.succeed("rm -rf /tmp/mode-select /home/deploy/.local/state/nix/profiles/mode-aware")
      work("deploy -s --no-build-tree --no-review-changes --dry-activate .#mode-aware -- --offline > /tmp/dry-activate.out 2>&1", timeout=600)
      server.succeed("grep -Fx dry /tmp/mode-select/result")
      server.succeed("test ! -e /home/deploy/.local/state/nix/profiles/mode-aware")
      client_sh("grep -F 'Completed dry-activate!' /tmp/dry-activate.out")
    '';
  };

  boot-selects-boot-script = mkTest {
    name = "boot-selects-boot-script";
    scenarioScript = ''
      server.succeed("rm -rf /tmp/mode-select /home/deploy/.local/state/nix/profiles/mode-aware")
      work("deploy -s --no-build-tree --no-review-changes --boot .#mode-aware -- --offline > /tmp/boot.out 2>&1", timeout=600)
      server.succeed("grep -Fx boot /tmp/mode-select/result")
      server.succeed("test -L /home/deploy/.local/state/nix/profiles/mode-aware")
      client_sh("grep -F 'Success activating for next boot, done!' /tmp/boot.out")
    '';
  };

  tag-select-and-order = mkTest {
    name = "tag-select-and-order";
    scenarioScript = ''
      server.succeed("rm -rf /tmp/tag-select")
      work("deploy -s --no-build-tree --no-review-changes --tag observability --tag prod .#tagged -- --offline", timeout=600)
      server.succeed("test ! -e /tmp/tag-select/traces")
      server.succeed("test ! -e /tmp/tag-select/api")
      server.succeed("printf 'metrics\nlogs\n' | cmp -s - /tmp/tag-select/order.log")
    '';
  };

  tag-no-match = mkTest {
    name = "tag-no-match";
    scenarioScript = ''
      server.succeed("rm -rf /tmp/tag-select")
      work_fail("deploy -s --no-build-tree --no-review-changes --tag staging .#tagged -- --offline > /tmp/tag-no-match.out 2>&1", timeout=600)
      client_sh("grep -F 'No profiles matched the requested tags: staging' /tmp/tag-no-match.out")
      server.succeed("test ! -e /tmp/tag-select/order.log")
    '';
  };

  system-manager-profile = mkTest {
    name = "system-manager-profile";
    scenarioScript = ''
      server.succeed("rm -rf /tmp/system-manager && mkdir -p /nix/var/nix/profiles/system-manager-profiles")
      work("deploy -s --no-build-tree --no-review-changes .#system-manager-target -- --offline", timeout=600)
      server.succeed("grep -Fx activated /tmp/system-manager/state")
      server.succeed("test -L /nix/var/nix/profiles/system-manager-profiles/system-manager")
      server.succeed("test -x /nix/var/nix/profiles/system-manager-profiles/system-manager/bin/activate")
    '';
  };

  sudo-argv-root-deploy = mkTest {
    name = "sudo-argv-root-deploy";
    scenarioScript = ''
      server.succeed("rm -rf /tmp/sudo-argv")
      work("deploy -s --no-build-tree --no-review-changes .#sudo-argv -- --offline", timeout=600)
      server.succeed("grep -Fx root-via-sudo /tmp/sudo-argv/result")
      server.succeed("test \"$(stat -c '%U' /tmp/sudo-argv/result)\" = root")
    '';
  };

  activation-failure-rolls-back = mkTest {
    name = "activation-failure-rolls-back";
    scenarioScript = ''
      server.succeed("rm -rf /tmp/activation")
      work("deploy -s --no-build-tree --no-review-changes .#activation-baseline -- --offline", timeout=600)
      server.succeed("grep -Fx baseline /tmp/activation/version")
      work_fail("deploy -s --no-build-tree --no-review-changes .#activation-fail -- --offline > /tmp/activation-fail.out 2>&1", timeout=600)
      server.succeed("grep -Fx baseline /tmp/activation/version")
    '';
  };

  multi-target-rollback-succeeded = mkTest {
    name = "multi-target-rollback-succeeded";
    scenarioScript = ''
      server.succeed("rm -rf /tmp/multi-rollback")
      work("deploy -s --no-build-tree --no-review-changes .#multi-rollback-baseline -- --offline", timeout=600)
      server.succeed("grep -Fx baseline /tmp/multi-rollback/app")
      server.succeed("grep -Fx baseline /tmp/multi-rollback/bad")
      work_fail("deploy -s --no-build-tree --no-review-changes --targets .#multi-rollback-ok .#multi-rollback-fail -- --offline > /tmp/multi-rollback.out 2>&1", timeout=600)
      client_sh("grep -F 'Revoking previous deploys' /tmp/multi-rollback.out")
      server.succeed("grep -Fx baseline /tmp/multi-rollback/app")
      server.succeed("grep -Fx baseline /tmp/multi-rollback/bad")
    '';
  };

  multi-host-heterogeneous-targets = mkTest {
    name = "multi-host-heterogeneous-targets";
    multiHost = true;
    scenarioScript = ''
      server.succeed("rm -rf /tmp/multi-host")
      server2.succeed("rm -rf /tmp/multi-host")
      client_fail("ssh -o ConnectTimeout=5 -o ConnectionAttempts=1 -o UserKnownHostsFile=/dev/null -o StrictHostKeyChecking=no server2 true", timeout=10)
      client_sh("ssh -p 2222 -o UserKnownHostsFile=/dev/null -o StrictHostKeyChecking=no server2 true", timeout=30)
      work("deploy -s --no-build-tree --no-review-changes --targets .#multi-host-a-updated .#multi-host-b-updated -- --offline", timeout=600)
      server.succeed("grep -Fx updated /tmp/multi-host/a")
      server2.succeed("grep -Fx updated /tmp/multi-host/b")
    '';
  };

  multi-host-rollback-succeeded = mkTest {
    name = "multi-host-rollback-succeeded";
    multiHost = true;
    scenarioScript = ''
      server.succeed("rm -rf /tmp/multi-host")
      server2.succeed("rm -rf /tmp/multi-host")
      work("deploy -s --no-build-tree --no-review-changes --targets .#multi-host-a-baseline .#multi-host-b-baseline -- --offline", timeout=600)
      server.succeed("grep -Fx baseline /tmp/multi-host/a")
      server2.succeed("grep -Fx baseline /tmp/multi-host/b")
      work_fail("deploy -s --no-build-tree --no-review-changes --targets .#multi-host-a-updated .#multi-host-b-fail -- --offline > /tmp/multi-host-rollback.out 2>&1", timeout=600)
      client_sh("grep -F 'Revoking previous deploys' /tmp/multi-host-rollback.out")
      server.succeed("grep -Fx baseline /tmp/multi-host/a")
      server2.succeed("grep -Fx baseline /tmp/multi-host/b")
    '';
  };

  magic-rollback-default = mkTest {
    name = "magic-rollback-default";
    scenarioScript = ''
      work_fail("deploy -s --no-build-tree --no-review-changes .#broken-ssh -- --offline > /tmp/broken-ssh.out 2>&1", timeout=900)
      server.wait_for_open_port(22)
      client_sh("ssh -o UserKnownHostsFile=/dev/null -o StrictHostKeyChecking=no server true", timeout=30)
      client_fail("ssh -p 2222 -o UserKnownHostsFile=/dev/null -o StrictHostKeyChecking=no server true", timeout=30)
    '';
  };

  ssh-multiplexing-reuse = mkTest {
    name = "ssh-multiplexing-reuse";
    scenarioScript = ''
      install_wrapper("ssh", ssh_wrapper_source)
      work("PATH=/tmp/wrappers:$PATH deploy -s --no-build-tree --no-review-changes .#multiplex -- --offline > /tmp/ssh-multiplexing.out 2>&1", timeout=600)
      server.succeed("grep -Fx first /tmp/multiplex/first")
      server.succeed("grep -Fx second /tmp/multiplex/second")
      client_sh("count=$(grep -c 'ControlMaster=yes' /tmp/deploy-rx-e2e/ssh.log || true); test \"$count\" = 1")
      client_sh("count=$(grep -c 'deploy-rx-ssh-server' /tmp/deploy-rx-e2e/ssh.log || true); test \"$count\" -ge 3")
    '';
  };

  no-ssh-multiplexing = mkTest {
    name = "no-ssh-multiplexing";
    scenarioScript = ''
      install_wrapper("ssh", ssh_wrapper_source)
      work("PATH=/tmp/wrappers:$PATH deploy -s --no-build-tree --no-review-changes --no-ssh-multiplexing .#multiplex -- --offline > /tmp/no-ssh-multiplexing.out 2>&1", timeout=600)
      server.succeed("grep -Fx first /tmp/multiplex/first")
      server.succeed("grep -Fx second /tmp/multiplex/second")
      client_sh("count=$(grep -c 'ControlMaster=yes' /tmp/deploy-rx-e2e/ssh.log || true); test \"$count\" = 0")
      client_sh("count=$(grep -c 'deploy-rx-ssh-server' /tmp/deploy-rx-e2e/ssh.log || true); test \"$count\" = 0")
    '';
  };

  rollback-fresh-connection-toggle = mkTest {
    name = "rollback-fresh-connection-toggle";
    scenarioScript = ''
      work("deploy -s --no-build-tree --no-review-changes --no-rollback-fresh-connection .#broken-ssh -- --offline > /tmp/no-fresh-connection.out 2>&1", timeout=900)
      server.wait_for_open_port(2222)
      client_fail("ssh -o UserKnownHostsFile=/dev/null -o StrictHostKeyChecking=no server true", timeout=30)
      client_sh("ssh -p 2222 -o UserKnownHostsFile=/dev/null -o StrictHostKeyChecking=no server true", timeout=30)
    '';
  };

  batched-local-build = mkTest {
    name = "batched-local-build";
    scenarioScript = ''
      install_wrapper("nix", nix_wrapper_source)
      work("PATH=/tmp/wrappers:$PATH deploy -s --no-build-tree --no-review-changes .#multiplex -- --offline > /tmp/batched-local-build.out 2>&1", timeout=600)
      server.succeed("grep -Fx first /tmp/multiplex/first")
      server.succeed("grep -Fx second /tmp/multiplex/second")
      client_sh("count=$(grep -c '^build ' /tmp/deploy-rx-e2e/nix.log || true); test \"$count\" = 1")
    '';
  };

  nom-present = mkTest {
    name = "nom-present";
    scenarioScript = ''
      client_sh(f"test -x {raw_deploy_cmd}")
      install_wrapper("nom", nom_wrapper_source)
      work(f"PATH=/tmp/wrappers:/run/current-system/sw/bin {raw_deploy_cmd} --fast-connection true -s --no-review-changes .#nom-profile -- --offline > /tmp/nom-present.out 2>&1", timeout=600)
      server.succeed("grep -Fx nom /tmp/nom/result")
      client_sh("grep -F -- '--json' /tmp/deploy-rx-e2e/nom.log")
    '';
  };

  nom-absent = mkTest {
    name = "nom-absent";
    scenarioScript = ''
      client_sh(f"test -x {raw_deploy_cmd}")
      work(f"PATH=/run/current-system/sw/bin {raw_deploy_cmd} --fast-connection true -s --no-review-changes .#nom-profile -- --offline > /tmp/nom-absent.out 2>&1", timeout=600)
      server.succeed("grep -Fx nom /tmp/nom/result")
      client_sh("grep -F 'Build tree visualization requested but `nom` is not available in PATH; falling back to regular build logs' /tmp/nom-absent.out")
    '';
  };

  review-changes-on-off = mkTest {
    name = "review-changes-on-off";
    scenarioScript = ''
      server.succeed("rm -rf /tmp/review")
      work("deploy -s --no-build-tree --no-review-changes .#review-baseline -- --offline", timeout=600)
      work("deploy -s --no-build-tree .#review-a -- --offline > /tmp/review-on.out 2>&1", timeout=600)
      client_sh("grep -F 'Derivation changes for ' /tmp/review-on.out")
      work("deploy -s --no-build-tree --no-review-changes .#review-b -- --offline > /tmp/review-off.out 2>&1", timeout=600)
      client_fail("grep -F 'Derivation changes for ' /tmp/review-off.out")
      server.succeed("grep -Fx b /tmp/review/version")
    '';
  };
}
