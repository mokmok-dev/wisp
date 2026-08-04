# Crane-backed packages and portable checks.
#
# Plain function called from flake.nix's perSystem with the shared `pkgs`,
# `system`, and `rustToolchain`. `nix/crane.nix` is imported twice: once against
# the native `pkgs` and once against a mingw32 cross nixpkgs for the Windows
# build. Crane's buildDepsOnly artifacts are shared inside each invocation (see
# nix/crane.nix), maximising local cache reuse between the package, clippy and
# test derivations.
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

  windowsPkgs = import inputs.nixpkgs {
    localSystem = system;
    crossSystem = {
      config = "x86_64-w64-mingw32";
      libc = "msvcrt";
    };
    overlays = [ inputs.rust-overlay.overlays.default ];
  };
  windowsCraneOutputs = import ./crane.nix {
    pkgs = windowsPkgs;
    craneLib = inputs.crane.mkLib windowsPkgs;
    rustToolchain = p: p.rust-bin.fromRustupToolchainFile ../rust-toolchain.toml;
  };
in
{
  inherit (nativeCraneOutputs) checks;
  packages =
    nativeCraneOutputs.packages
    // pkgs.lib.optionalAttrs pkgs.stdenv.hostPlatform.isLinux {
      inherit (windowsCraneOutputs.packages) wisp-windows;
    };
}
