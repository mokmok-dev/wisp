{
  lib,
  craneLib,
  rustToolchain,
}:
let
  crane = craneLib.overrideToolchain rustToolchain;
  src = crane.cleanCargoSource ../../.;

  # The desktop target needs Xcode 26 from the host and is built by the
  # dedicated macOS workflow. These arguments match the portable Rust CI job.
  portableArgs = {
    inherit src;
    cargoExtraArgs = "--locked --workspace --exclude wisp-desktop";
    strictDeps = true;
  };
  portableArtifacts = crane.buildDepsOnly portableArgs;
in
crane.buildPackage (
  portableArgs
  // {
    cargoArtifacts = portableArtifacts;
    cargoExtraArgs = "--locked -p wisp-desktop";
    doCheck = false;
    meta = {
      description = "wisp desktop app";
      mainProgram = "wisp-desktop";
      platforms = lib.platforms.darwin;
    };
  }
)
