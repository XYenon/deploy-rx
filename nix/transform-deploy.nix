# SPDX-FileCopyrightText: 2026 Serokell <https://serokell.io/>
#
# SPDX-License-Identifier: MPL-2.0

# Returns a transformed copy of `deploy` where any profile whose `path`
# evaluated to a derivation also exposes that derivation's `drvPath` and
# selected output. This
# lets deploy-rx build content-addressed and floating-output derivations,
# whose `outPath` is only a placeholder at eval time, without users having
# to set `drvPath` manually in their flake.
#
# The deploy-rx binary loads this file via `include_str!` from `src/cli.rs`
# and applies it. The expression is self-contained so it can be parsed in
# isolation for linting, and exercised by `nix/tests/transform-deploy.nix`.
deploy:
let
  patchProfile = p:
    if (p ? path) && (builtins.isAttrs p.path) && (p.path ? drvPath)
    then p // {
      path = p.path.outPath;
      drvPath = p.path.drvPath;
      outputName = p.path.outputName or "out";
    }
    else p;
  patchNode = n: n // {
    profiles = builtins.mapAttrs (_: patchProfile) (n.profiles or { });
  };
in
if deploy ? nodes
then deploy // { nodes = builtins.mapAttrs (_: patchNode) deploy.nodes; }
else deploy
