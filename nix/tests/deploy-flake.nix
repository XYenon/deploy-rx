# SPDX-FileCopyrightText: 2024 Serokell <https://serokell.io/>
#
# SPDX-License-Identifier: MPL-2.0

{
  inputs = {
    # real inputs are substituted in ./default.nix
##inputs##
  };

  outputs = { self, nixpkgs, deploy-rx, ... }@inputs: let
    system = "x86_64-linux";
    pkgs = inputs.nixpkgs.legacyPackages.${system};
    lib = pkgs.lib;
    user = "deploy";

    commonSshOpts = [
      "-o"
      "UserKnownHostsFile=/dev/null"
      "-o"
      "StrictHostKeyChecking=no"
    ];

    server2SshOpts = commonSshOpts ++ [
      "-p"
      "2222"
    ];

    mkCustomProfile = script: let
      activateProfile = pkgs.writeShellScriptBin "activate" ''
        set -euo pipefail
        ${script}
      '';
    in
      deploy-rx.lib.${system}.activate.custom activateProfile "$PROFILE/bin/activate";

    writeMarkerProfile = {
      marker,
      value,
      append ? false,
      extra ? "",
    }:
      mkCustomProfile ''
        ${pkgs.coreutils}/bin/mkdir -p ${lib.escapeShellArg (builtins.dirOf marker)}
        ${
          if append
          then "printf '%s\\n' ${lib.escapeShellArg value} >> ${lib.escapeShellArg marker}"
          else "printf '%s\\n' ${lib.escapeShellArg value} > ${lib.escapeShellArg marker}"
        }
        ${extra}
      '';

    failingMarkerProfile = {
      marker,
      value,
      extra ? "",
    }:
      mkCustomProfile ''
        ${pkgs.coreutils}/bin/mkdir -p ${lib.escapeShellArg (builtins.dirOf marker)}
        printf '%s\n' ${lib.escapeShellArg value} > ${lib.escapeShellArg marker}
        ${extra}
        exit 1
      '';

    modeAwareMarkerProfile = {
      marker,
      activateValue,
      dryValue,
      bootValue,
    }: let
      writeMode = value: ''
        ${pkgs.coreutils}/bin/mkdir -p ${lib.escapeShellArg (builtins.dirOf marker)}
        printf '%s\n' ${lib.escapeShellArg value} > ${lib.escapeShellArg marker}
      '';

      custom =
        deploy-rx.lib.${system}.activate.custom
        // {
          dryActivate = writeMode dryValue;
          boot = writeMode bootValue;
        };
    in custom (pkgs.writeShellScriptBin "mode-aware-base" ''
      set -euo pipefail
      :
    '') (writeMode activateValue);

    systemManagerBase = pkgs.writeShellScriptBin "activate" ''
      set -euo pipefail
      ${pkgs.coreutils}/bin/mkdir -p /tmp/system-manager
      printf '%s\n' activated > /tmp/system-manager/state
    '';

    mkServerConfiguration = extraModules:
      nixpkgs.lib.nixosSystem {
        inherit system pkgs;
        specialArgs = { inherit inputs; flakes = import inputs.enable-flakes; };
        modules = [
          ./server.nix
          ./common.nix
          (pkgs.path + "/nixos/modules/virtualisation/qemu-vm.nix")
          # Import the base config used by nixos tests
          (pkgs.path + "/nixos/lib/testing/nixos-test-base.nix")
          # Deployment breaks the network settings, so we need to restore them
          (pkgs.lib.importJSON ./network.json)
          # Deploy packages
          { environment.systemPackages = [ pkgs.figlet pkgs.hello ]; }
        ] ++ extraModules;
      };
  in {
    nixosConfigurations = {
      server = mkServerConfiguration [ ];
      serverBrokenSsh = mkServerConfiguration [
        {
          services.openssh.ports = [ 2222 ];
        }
      ];
    };

    deploy = {
      # VM tests run on a fast local link, so direct SSH copies are both quicker and
      # more deterministic than letting the destination try remote substitutes.
      fastConnection = true;

      nodes = {
      server = {
        hostname = "server";
        sshUser = "root";
        sshOpts = commonSshOpts;
        profiles.system.path = deploy-rx.lib."${system}".activate.nixos self.nixosConfigurations.server;
      };
      server-override = {
        hostname = "override";
        sshUser = "override";
        user = "override";
        sudo = "override";
        sshOpts = [ ];
        confirmTimeout = 0;
        activationTimeout = 0;
        profiles.system.path = deploy-rx.lib."${system}".activate.nixos self.nixosConfigurations.server;
      };
      profile = {
        hostname = "server";
        sshUser = "${user}";
        sshOpts = commonSshOpts;
        profiles = {
          "hello-world".path = let
            activateProfile = pkgs.writeShellScriptBin "activate" ''
              set -euo pipefail
              ${pkgs.coreutils}/bin/mkdir -p /home/${user}/.nix-profile/bin
              ${pkgs.coreutils}/bin/rm -f -- /home/${user}/.nix-profile/bin/hello /home/${user}/.nix-profile/bin/figlet
              ${pkgs.coreutils}/bin/ln -s ${pkgs.hello}/bin/hello /home/${user}/.nix-profile/bin/hello
              ${pkgs.coreutils}/bin/ln -s ${pkgs.figlet}/bin/figlet /home/${user}/.nix-profile/bin/figlet
            '';
          in deploy-rx.lib.${system}.activate.custom activateProfile "$PROFILE/bin/activate";
        };
      };

      mode-aware = {
        hostname = "server";
        sshUser = user;
        sshOpts = commonSshOpts;
        magicRollback = false;
        profiles.app.path = modeAwareMarkerProfile {
          marker = "/tmp/mode-select/result";
          activateValue = "switch";
          dryValue = "dry";
          bootValue = "boot";
        };
        profiles.app.profilePath = "/home/${user}/.local/state/nix/profiles/mode-aware";
      };

      tagged = {
        hostname = "server";
        sshUser = user;
        sshOpts = commonSshOpts;
        magicRollback = false;
        profilesOrder = [ "metrics" "logs" "traces" "api" ];
        profiles.metrics = {
          path = writeMarkerProfile {
            marker = "/tmp/tag-select/order.log";
            value = "metrics";
            append = true;
          };
          profilePath = "/home/${user}/.local/state/nix/profiles/tagged-metrics";
          tags = [ "observability" "prod" ];
        };
        profiles.logs = {
          path = writeMarkerProfile {
            marker = "/tmp/tag-select/order.log";
            value = "logs";
            append = true;
          };
          profilePath = "/home/${user}/.local/state/nix/profiles/tagged-logs";
          tags = [ "observability" "prod" ];
        };
        profiles.traces = {
          path = writeMarkerProfile {
            marker = "/tmp/tag-select/traces";
            value = "traces";
          };
          profilePath = "/home/${user}/.local/state/nix/profiles/tagged-traces";
          tags = [ "observability" ];
        };
        profiles.api = {
          path = writeMarkerProfile {
            marker = "/tmp/tag-select/api";
            value = "api";
          };
          profilePath = "/home/${user}/.local/state/nix/profiles/tagged-api";
          tags = [ "prod" ];
        };
      };

      system-manager-target = {
        hostname = "server";
        sshUser = "root";
        sshOpts = commonSshOpts;
        magicRollback = false;
        profiles."system-manager".path = deploy-rx.lib.${system}.activate.system-manager systemManagerBase;
      };

      sudo-argv = {
        hostname = "server";
        sshUser = user;
        user = "root";
        sudo = [ "sudo" "-u" ];
        sshOpts = commonSshOpts;
        magicRollback = false;
        profiles.app.path = writeMarkerProfile {
          marker = "/tmp/sudo-argv/result";
          value = "root-via-sudo";
        };
        profiles.app.profilePath = "/nix/var/nix/profiles/deploy-rx-tests/sudo-argv";
      };

      activation-baseline = {
        hostname = "server";
        sshUser = user;
        sshOpts = commonSshOpts;
        magicRollback = false;
        profiles.app.path = writeMarkerProfile {
          marker = "/tmp/activation/version";
          value = "baseline";
        };
        profiles.app.profilePath = "/home/${user}/.local/state/nix/profiles/activation";
      };

      activation-fail = {
        hostname = "server";
        sshUser = user;
        sshOpts = commonSshOpts;
        magicRollback = false;
        profiles.app.path = failingMarkerProfile {
          marker = "/tmp/activation/version";
          value = "failed";
        };
        profiles.app.profilePath = "/home/${user}/.local/state/nix/profiles/activation";
      };

      multi-rollback-baseline = {
        hostname = "server";
        sshUser = user;
        sshOpts = commonSshOpts;
        magicRollback = false;
        profilesOrder = [ "app" "bad" ];
        profiles.app.path = writeMarkerProfile {
          marker = "/tmp/multi-rollback/app";
          value = "baseline";
        };
        profiles.app.profilePath = "/home/${user}/.local/state/nix/profiles/multi-app";
        profiles.bad.path = writeMarkerProfile {
          marker = "/tmp/multi-rollback/bad";
          value = "baseline";
        };
        profiles.bad.profilePath = "/home/${user}/.local/state/nix/profiles/multi-bad";
      };

      multi-rollback-ok = {
        hostname = "server";
        sshUser = user;
        sshOpts = commonSshOpts;
        magicRollback = false;
        profiles.app.path = writeMarkerProfile {
          marker = "/tmp/multi-rollback/app";
          value = "updated";
        };
        profiles.app.profilePath = "/home/${user}/.local/state/nix/profiles/multi-app";
      };

      multi-rollback-fail = {
        hostname = "server";
        sshUser = user;
        sshOpts = commonSshOpts;
        magicRollback = false;
        profiles.bad.path = failingMarkerProfile {
          marker = "/tmp/multi-rollback/bad";
          value = "failing";
        };
        profiles.bad.profilePath = "/home/${user}/.local/state/nix/profiles/multi-bad";
      };

      broken-ssh = {
        hostname = "server";
        sshUser = "root";
        sshOpts = commonSshOpts;
        confirmTimeout = 5;
        profiles.system.path = deploy-rx.lib.${system}.activate.nixos self.nixosConfigurations.serverBrokenSsh;
      };

      multiplex = {
        hostname = "server";
        sshUser = user;
        sshOpts = commonSshOpts;
        magicRollback = false;
        profilesOrder = [ "first" "second" ];
        profiles.first.path = writeMarkerProfile {
          marker = "/tmp/multiplex/first";
          value = "first";
        };
        profiles.first.profilePath = "/home/${user}/.local/state/nix/profiles/multiplex-first";
        profiles.second.path = writeMarkerProfile {
          marker = "/tmp/multiplex/second";
          value = "second";
        };
        profiles.second.profilePath = "/home/${user}/.local/state/nix/profiles/multiplex-second";
      };

      multi-host-a-baseline = {
        hostname = "server";
        sshUser = user;
        sshOpts = commonSshOpts;
        magicRollback = false;
        profiles.app.path = writeMarkerProfile {
          marker = "/tmp/multi-host/a";
          value = "baseline";
        };
        profiles.app.profilePath = "/home/${user}/.local/state/nix/profiles/multi-host-a";
      };

      multi-host-a-updated = {
        hostname = "server";
        sshUser = user;
        sshOpts = commonSshOpts;
        magicRollback = false;
        profiles.app.path = writeMarkerProfile {
          marker = "/tmp/multi-host/a";
          value = "updated";
        };
        profiles.app.profilePath = "/home/${user}/.local/state/nix/profiles/multi-host-a";
      };

      multi-host-b-baseline = {
        hostname = "server2";
        sshUser = "root";
        sshOpts = server2SshOpts;
        magicRollback = false;
        profiles.app.path = writeMarkerProfile {
          marker = "/tmp/multi-host/b";
          value = "baseline";
        };
        profiles.app.profilePath = "/nix/var/nix/profiles/deploy-rx-tests/multi-host-b";
      };

      multi-host-b-updated = {
        hostname = "server2";
        sshUser = "root";
        sshOpts = server2SshOpts;
        magicRollback = false;
        profiles.app.path = writeMarkerProfile {
          marker = "/tmp/multi-host/b";
          value = "updated";
        };
        profiles.app.profilePath = "/nix/var/nix/profiles/deploy-rx-tests/multi-host-b";
      };

      multi-host-b-fail = {
        hostname = "server2";
        sshUser = "root";
        sshOpts = server2SshOpts;
        magicRollback = false;
        profiles.app.path = failingMarkerProfile {
          marker = "/tmp/multi-host/b";
          value = "failing";
        };
        profiles.app.profilePath = "/nix/var/nix/profiles/deploy-rx-tests/multi-host-b";
      };

      review-baseline = {
        hostname = "server";
        sshUser = user;
        sshOpts = commonSshOpts;
        magicRollback = false;
        profiles.app.path = writeMarkerProfile {
          marker = "/tmp/review/version";
          value = "base";
        };
        profiles.app.profilePath = "/home/${user}/.local/state/nix/profiles/review";
      };

      review-a = {
        hostname = "server";
        sshUser = user;
        sshOpts = commonSshOpts;
        magicRollback = false;
        profiles.app.path = writeMarkerProfile {
          marker = "/tmp/review/version";
          value = "a";
        };
        profiles.app.profilePath = "/home/${user}/.local/state/nix/profiles/review";
      };

      review-b = {
        hostname = "server";
        sshUser = user;
        sshOpts = commonSshOpts;
        magicRollback = false;
        profiles.app.path = writeMarkerProfile {
          marker = "/tmp/review/version";
          value = "b";
        };
        profiles.app.profilePath = "/home/${user}/.local/state/nix/profiles/review";
      };

      nom-profile = {
        hostname = "server";
        sshUser = user;
        sshOpts = commonSshOpts;
        magicRollback = false;
        profiles.app.path = writeMarkerProfile {
          marker = "/tmp/nom/result";
          value = "nom";
        };
        profiles.app.profilePath = "/home/${user}/.local/state/nix/profiles/nom";
      };
      };
    };
  };
}
