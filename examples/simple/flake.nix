# SPDX-FileCopyrightText: 2020 Serokell <https://serokell.io/>
#
# SPDX-License-Identifier: MPL-2.0

{
  description = "Deploy GNU hello to localhost";

  inputs.deploy-rx.url = "github:XYenon/deploy-rx";

  outputs = { self, nixpkgs, deploy-rx }: {
    deploy.nodes.example = {
      hostname = "localhost";
      profiles.hello = {
        user = "balsoft";
        path = deploy-rx.lib.x86_64-linux.setActivate nixpkgs.legacyPackages.x86_64-linux.hello "./bin/hello";
      };
    };

    checks = builtins.mapAttrs (system: deployLib: deployLib.deployChecks self.deploy) deploy-rx.lib;
  };
}
