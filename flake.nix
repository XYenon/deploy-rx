# SPDX-FileCopyrightText: 2020 Serokell <https://serokell.io/>
# SPDX-FileCopyrightText: 2020 Andreas Fuchs <asf@boinkor.net>
#
# SPDX-License-Identifier: MPL-2.0

{
  description = "A Simple multi-profile Nix-flake deploy tool.";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    utils.url = "github:numtide/flake-utils";
    flake-compat = {
      url = "github:edolstra/flake-compat";
      flake = false;
    };
  };

  outputs = { self, nixpkgs, utils, ... }@inputs:
  {
    overlays.default = final: prev:
    {
      deploy-rx = {

        deploy-rx = final.rustPlatform.buildRustPackage {
          pname = "deploy-rx";
          version = "0.1.0";

          src = final.lib.sourceByRegex ./. [
            "Cargo.lock"
            "Cargo.toml"
            "src"
            "src/bin"
            ".*.rs$"
          ];

          cargoLock.lockFile = ./Cargo.lock;

          nativeBuildInputs = [ final.makeWrapper ];

          postFixup = ''
            wrapProgram "$out/bin/deploy" \
              --prefix PATH : ${final.lib.makeBinPath [ final."nix-output-monitor" ]}
          '';

          meta = {
            description = "A Simple multi-profile Nix-flake deploy tool";
            mainProgram = "deploy";
          };
        };

        lib = rec {

          setActivate = builtins.trace
            "deploy-rx#lib.setActivate is deprecated, use activate.noop, activate.nixos, activate.darwin, activate.home-manager, activate.system-manager or activate.custom instead"
            activate.custom;

          activate = rec {
            custom =
              {
                __functor = customSelf: base: activate:
                  final.buildEnv {
                    name = ("activatable-" + base.name);
                    paths =
                      [
                        base
                        (final.writeTextFile {
                          name = base.name + "-activate-path";
                          text = ''
                            #!${final.runtimeShell}
                            set -euo pipefail

                            if [[ "''${DRY_ACTIVATE:-}" == "1" ]]
                            then
                                ${customSelf.dryActivate or "echo ${final.writeScript "activate" activate}"}
                            elif [[ "''${BOOT:-}" == "1" ]]
                            then
                                ${customSelf.boot or "echo ${final.writeScript "activate" activate}"}
                            else
                                ${activate}
                            fi
                          '';
                          executable = true;
                          destination = "/deploy-rx-activate";
                        })
                        (final.writeTextFile {
                            name = base.name + "-activate-rs";
                            text = ''
                            #!${final.runtimeShell}
                            exec ${final.deploy-rx.deploy-rx}/bin/activate "$@"
                          '';
                          executable = true;
                          destination = "/activate-rs";
                        })
                      ];
                  };
              };

            nixos = base:
              (custom // {
                dryActivate = "$PROFILE/bin/switch-to-configuration dry-activate";
                boot = "$PROFILE/bin/switch-to-configuration boot";
              })
              base.config.system.build.toplevel
              ''
                # work around https://github.com/NixOS/nixpkgs/issues/73404
                cd /tmp

                $PROFILE/bin/switch-to-configuration switch

                # https://github.com/serokell/deploy-rs/issues/31
                ${with base.config.boot.loader;
                final.lib.optionalString systemd-boot.enable
                "sed -i '/^default /d' ${efi.efiSysMountPoint}/loader/loader.conf"}
              '';

            home-manager = base: custom base.activationPackage "$PROFILE/activate";

            # Activation script for `system-manager.lib.makeSystemConfig`.
            system-manager = base: custom base "$PROFILE/bin/activate";

            # Activation script for 'darwinSystem' from nix-darwin.
            # 'HOME=/var/root' is needed because 'sudo' on darwin doesn't change 'HOME' directory,
            # while 'darwin-rebuild' (which is invoked under the hood) performs some nix-channel
            # checks that rely on 'HOME'. As a result, if 'sshUser' is different from root,
            # deployment may fail without explicit 'HOME' redefinition.
            darwin = base: custom base.config.system.build.toplevel "HOME=/var/root $PROFILE/activate";

            noop = base: custom base ":";

            # Install a package into the target's nix3 profile (`nix profile`).
            # Unlike the other activators this does not just `nix-env --set` the
            # closure into deploy-rs' own profile, it also (re)installs `base`
            # into the calling user's `nix profile`, replacing a previous
            # generation with the same name. Useful for declaratively rolling
            # out a package (or a `buildEnv`) to a machine.
            profile = base: custom base ''
              export PATH="${final.lib.makeBinPath [ final.nix ]}:$PATH"
              nixFlags=(--extra-experimental-features "nix-command flakes")
              # Best-effort removal of a previously-installed element of the same
              # name (no-op on first deploy), then install the new closure.
              nix "''${nixFlags[@]}" profile remove "${base.name}" 2>/dev/null || true
              nix "''${nixFlags[@]}" profile install "${base}"
            '';
          };

          deployChecks = deploy: builtins.mapAttrs (_: check: check deploy) {
            deploy-schema = deploy: final.runCommand "jsonschema-deploy-system" { } ''
              ${final.check-jsonschema}/bin/check-jsonschema --schemafile ${./interface.json} ${final.writeText "deploy.json" (builtins.toJSON deploy)} && touch $out
            '';

            deploy-activate = deploy:
              let
                profiles = builtins.concatLists (final.lib.mapAttrsToList (nodeName: node: final.lib.mapAttrsToList (profileName: profile: [ (toString profile.path) nodeName profileName ]) node.profiles) deploy.nodes);
              in
              final.runCommand "deploy-rx-check-activate" { } ''
                for x in ${builtins.concatStringsSep " " (map (p: builtins.concatStringsSep ":" p) profiles)}; do
                  profile_path=$(echo $x | cut -f1 -d:)
                  node_name=$(echo $x | cut -f2 -d:)
                  profile_name=$(echo $x | cut -f3 -d:)

                  test -f "$profile_path/deploy-rx-activate" || (echo "#$node_name.$profile_name is missing the deploy-rx-activate activation script" && exit 1);

                  test -f "$profile_path/activate-rs" || (echo "#$node_name.$profile_name is missing the activate-rs activation script" && exit 1);
                done

                touch $out
              '';
          };
        };
      };
    };
  } //
    utils.lib.eachSystem (utils.lib.defaultSystems ++ ["aarch64-darwin"]) (system:
      let
        pkgs = import nixpkgs {
          inherit system;
          overlays = [ self.overlays.default ];
        };

        # make a matrix to use in GitHub pipeline
        mkMatrix = name: attrs: {
          include = map (v: { ${name} = v; }) (pkgs.lib.attrNames attrs);
        };
      in
      {
        packages.default = self.packages."${system}".deploy-rx;
        packages.deploy-rx = pkgs.deploy-rx.deploy-rx;

        apps.default = self.apps."${system}".deploy-rx;
        apps.deploy-rx = {
          type = "app";
          program = "${self.packages."${system}".default}/bin/deploy";
        };

        devShells.default = pkgs.mkShell {
          inputsFrom = [ self.packages.${system}.deploy-rx ];
          RUST_SRC_PATH = "${pkgs.rust.packages.stable.rustPlatform.rustLibSrc}";
          buildInputs = with pkgs; [
            nix
            cargo
            rustc
            rust-analyzer
            rustfmt
            clippy
            reuse
            rust.packages.stable.rustPlatform.rustLibSrc
          ];
        };

        checks = {
          deploy-rx = self.packages.${system}.default.overrideAttrs (super: { doCheck = true; });

          # Lint the Rust sources with clippy, failing on any warning.
          clippy = self.packages.${system}.default.overrideAttrs (super: {
            pname = "deploy-rx-clippy";
            nativeBuildInputs = (super.nativeBuildInputs or [ ]) ++ [ pkgs.clippy ];
            buildPhase = "cargo clippy --all-targets -- --deny warnings";
            doCheck = false;
            installPhase = "touch $out";
            postFixup = "";
          });

          # Enforce `cargo fmt` formatting.
          rustfmt = pkgs.runCommandLocal "deploy-rx-rustfmt" {
            nativeBuildInputs = [ pkgs.cargo pkgs.rustfmt ];
          } ''
            export HOME="$TMPDIR"
            cd ${self.packages.${system}.default.src}
            cargo fmt --check
            touch $out
          '';

          # Enforce REUSE licensing compliance.
          reuse = pkgs.runCommandLocal "deploy-rx-reuse" {
            nativeBuildInputs = [ pkgs.reuse ];
          } ''
            reuse --root ${self} lint
            touch $out
          '';
        } // (pkgs.lib.optionalAttrs (pkgs.lib.elem system ["x86_64-linux"]) (import ./nix/tests {
          inherit inputs pkgs;
          deployRxSrc = self.outPath;
        }));

        inherit (pkgs.deploy-rx) lib;

        check-matrix = mkMatrix "check" self.checks.${system};
      });
}
