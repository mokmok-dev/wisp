{
  craneLib,
  pkgs,
  rustToolchain,
}:
let
  crane = craneLib.overrideToolchain rustToolchain;
  src = crane.cleanCargoSource ../.;

  # The desktop target needs Xcode 26 from the host and is built by the
  # dedicated macOS workflow. These arguments match the portable Rust CI job.
  portableArgs = {
    inherit src;
    cargoExtraArgs = "--locked --workspace --exclude wisp-desktop";
    strictDeps = true;
  };
  portableArtifacts = crane.buildDepsOnly portableArgs;
in
{
  # The desktop app is macOS-only and needs the host Xcode toolchain, so it is
  # the single package on macOS. Portable checks (clippy/tests) run on Linux CI.
  packages =
    if pkgs.stdenv.hostPlatform.isDarwin then
      let
        wisp-desktop = crane.buildPackage (
          portableArgs
          // {
            cargoArtifacts = portableArtifacts;
            cargoExtraArgs = "--locked -p wisp-desktop";
            doCheck = false;
          }
        );
      in
      {
        inherit wisp-desktop;
        default = wisp-desktop;
      }
    else
      { };

  # Formatting (rustfmt + nixfmt + swiftformat) is unified under treefmt-nix;
  # see nix/parts/formatting.nix. Crane's cargoFmt is not duplicated here.
  checks = pkgs.lib.optionalAttrs pkgs.stdenv.isLinux {
    clippy = crane.cargoClippy (
      portableArgs
      // {
        cargoArtifacts = portableArtifacts;
        cargoClippyExtraArgs = "--all-targets -- -D warnings";
      }
    );
    tests = crane.cargoTest (
      portableArgs
      // {
        cargoArtifacts = portableArtifacts;
        cargoTestExtraArgs = "--all-targets";
      }
    );
  };
}
