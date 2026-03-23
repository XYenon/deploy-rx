# SPDX-FileCopyrightText: 2020 Serokell <https://serokell.io/>
#
# SPDX-License-Identifier: MPL-2.0

{
  description = "Deploy a full system with hello service as a separate profile";

  inputs.deploy-rx.url = "github:XYenon/deploy-rx";

  outputs = { self, nixpkgs, deploy-rx }: {
    nixosConfigurations.example-nixos-system = nixpkgs.lib.nixosSystem {
      system = "x86_64-linux";
      modules = [ ./configuration.nix ];
    };

    nixosConfigurations.bare = nixpkgs.lib.nixosSystem {
      system = "x86_64-linux";
      modules =
        [ ./bare.nix "${nixpkgs}/nixos/modules/virtualisation/qemu-vm.nix" ];
    };

    # This is the application we actually want to run
    defaultPackage.x86_64-linux = import ./hello.nix nixpkgs;

    deploy.nodes.example = {
      sshOpts = [ "-p" "2221" ];
      hostname = "localhost";
      fastConnection = true;
      interactiveSudo = true;
      profiles = {
        system = {
          sshUser = "admin";
          path =
            deploy-rx.lib.x86_64-linux.activate.nixos self.nixosConfigurations.example-nixos-system;
          user = "root";
        };
        hello = {
          sshUser = "hello";
          path = deploy-rx.lib.x86_64-linux.activate.custom self.defaultPackage.x86_64-linux "./bin/activate";
          user = "hello";
        };
      };
    };

    checks = builtins.mapAttrs (system: deployLib: deployLib.deployChecks self.deploy) deploy-rx.lib;
  };
}
