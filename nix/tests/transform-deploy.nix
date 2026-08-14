# SPDX-FileCopyrightText: 2026 Serokell <https://serokell.io/>
#
# SPDX-License-Identifier: MPL-2.0

# Pure-evaluation test for `../transform-deploy.nix`. The test runs without a
# VM and without building anything beyond a trivial `runCommand`, so it is
# cheap enough to run on every `nix flake check`.
#
# Asserts:
#   - a derivation-typed `path` is rewritten to `{ path = outPath; drvPath = drvPath; }`.
#   - other profile attrs are preserved.
#   - top-level deploy attrs are preserved.
#   - a hand-written string-typed `path` passes through unchanged.

{ pkgs }:
let
  transformDeploy = import ../transform-deploy.nix;

  # A real, cheap derivation to stand in for what `activate.nixos cfg` returns.
  fakeProfile = pkgs.runCommand "fake-deploy-rx-profile" { } "touch $out";

  derivationDeploy = {
    sshUser = "deployer";
    nodes.demo = {
      hostname = "demo.example";
      profiles.system = {
        path = fakeProfile;
        sshUser = "root";
      };
    };
  };

  stringDeploy = {
    nodes.demo = {
      hostname = "demo.example";
      profiles.system = {
        path = "/nix/store/0000000000000000000000000000000000-handwritten";
      };
    };
  };

  drvOut = transformDeploy derivationDeploy;
  strOut = transformDeploy stringDeploy;

  drvProfile = drvOut.nodes.demo.profiles.system;
  strProfile = strOut.nodes.demo.profiles.system;
in
# A derivation-typed path is split into outPath and drvPath.
assert drvProfile.path == fakeProfile.outPath;
assert drvProfile.drvPath == fakeProfile.drvPath;
# Sibling attrs are preserved.
assert drvProfile.sshUser == "root";
assert drvOut.sshUser == "deployer";
assert drvOut.nodes.demo.hostname == "demo.example";
# A string-typed path is left untouched, and no drvPath is synthesised.
assert strProfile.path == "/nix/store/0000000000000000000000000000000000-handwritten";
assert !(strProfile ? drvPath);

pkgs.runCommand "transform-deploy-test" { } "touch $out"
