# Crane-backed packages and portable checks.
#
# Plain function called from flake.nix's perSystem with the shared `pkgs`,
# `system`, and `rustToolchain`. `nix/crane.nix` is imported once against the
# native `pkgs`. Crane's buildDepsOnly artifacts are shared inside that
# invocation (see nix/crane.nix), maximising local cache reuse between the
# package, clippy and test derivations.
#
# Returns: { packages, checks }.
{
  inputs,
  pkgs,
  system,
  rustToolchain,
}:
let
  nativeCraneOutputs = import ./crane.nix {
    inherit pkgs rustToolchain;
    craneLib = inputs.crane.mkLib pkgs;
  };
in
{
  inherit (nativeCraneOutputs) checks packages;
}
