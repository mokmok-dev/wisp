{
  craneLib,
  pkgs,
  rustToolchain,
}:
let
  crane = craneLib.overrideToolchain rustToolchain;
  src = crane.cleanCargoSource ../.;
  isWindows = pkgs.stdenv.hostPlatform.isWindows;

  wispMcpArgs = {
    inherit src;
    cargoExtraArgs = "--locked -p wisp-mcp";
    strictDeps = true;
  };
  wispMcpArtifacts = crane.buildDepsOnly wispMcpArgs;
  wisp-mcp = crane.buildPackage (
    wispMcpArgs
    // {
      cargoArtifacts = wispMcpArtifacts;
    }
  );

  wispWindowsArgs = {
    inherit src;
    CARGO_BUILD_JOBS = "1";
    cargoExtraArgs = "--locked -p wisp-desktop -p wisp-mcp";
    doCheck = false;
    strictDeps = true;
  };
  wispWindowsArtifacts = crane.buildDepsOnly wispWindowsArgs;
  wisp-windows = crane.buildPackage (
    wispWindowsArgs
    // {
      cargoArtifacts = wispWindowsArtifacts;
    }
  );

  # The desktop target needs Xcode 26 from the host and is built by the
  # dedicated macOS workflow. These checks match the portable Rust CI job.
  portableArgs = {
    inherit src;
    cargoExtraArgs = "--locked --workspace --exclude wisp-desktop";
    strictDeps = true;
  };
  portableArtifacts = crane.buildDepsOnly portableArgs;
in
{
  packages = {
    inherit wisp-mcp;
    default = if isWindows then wisp-windows else wisp-mcp;
  }
  // pkgs.lib.optionalAttrs isWindows {
    inherit wisp-windows;
  };

  checks = {
    formatting = crane.cargoFmt {
      inherit src;
    };
  }
  // (
    if isWindows then
      {
        inherit wisp-windows;
      }
    else
      {
        inherit wisp-mcp;
      }
  )
  // pkgs.lib.optionalAttrs pkgs.stdenv.isLinux {
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
