{
  craneLib,
  pkgs,
  rustToolchain,
}:
let
  crane = craneLib.overrideToolchain rustToolchain;
  src = crane.cleanCargoSource ../.;
  isWindows = pkgs.stdenv.hostPlatform.isWindows;
  sherpaLinuxArchiveName =
    if pkgs.stdenv.hostPlatform.isAarch64 then
      "sherpa-onnx-v1.13.4-linux-aarch64-static-lib.tar.bz2"
    else
      "sherpa-onnx-v1.13.4-linux-x64-static-lib.tar.bz2";
  sherpaLinuxArchive = pkgs.fetchurl {
    url = "https://github.com/k2-fsa/sherpa-onnx/releases/download/v1.13.4/${sherpaLinuxArchiveName}";
    hash =
      if pkgs.stdenv.hostPlatform.isAarch64 then
        "sha256-I7M2Fnh8yUnVsUOOl5RVD4BeIIoBTFwiRUgyB8WLvA8="
      else
        "sha256-mLDjGZZCb254JE284ZVVSPLGTo8BxL51uFr3zaoujVw=";
  };
  sherpaLinuxArchiveDir = pkgs.runCommand "sherpa-onnx-archives" { } ''
    mkdir -p "$out"
    ln -s ${sherpaLinuxArchive} "$out/${sherpaLinuxArchiveName}"
  '';

  # The desktop target needs Xcode 26 from the host and is built by the
  # dedicated macOS workflow. These arguments match the portable Rust CI job.
  portableArgs = {
    inherit src;
    cargoExtraArgs = "--locked --workspace --exclude wisp-desktop";
    strictDeps = true;
  }
  // pkgs.lib.optionalAttrs pkgs.stdenv.isLinux {
    SHERPA_ONNX_ARCHIVE_DIR = sherpaLinuxArchiveDir;
    nativeBuildInputs = [
      pkgs.pkg-config
      pkgs.rustPlatform.bindgenHook
    ];
    buildInputs = [ pkgs.pipewire ];
  };
  portableArtifacts = crane.buildDepsOnly portableArgs;

  wispMcpArgs = {
    inherit src;
    cargoExtraArgs = "--locked -p wisp-mcp";
    strictDeps = true;
  };
  # Linux checks already build every portable workspace dependency, including
  # wisp-mcp's. Reusing that artifact derivation means the package, clippy and
  # tests share one dependency build. On other hosts only wisp-mcp is checked,
  # so retain its smaller package-specific dependency derivation.
  wispMcpArtifacts =
    if pkgs.stdenv.isLinux then portableArtifacts else crane.buildDepsOnly wispMcpArgs;
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
in
{
  packages = {
    inherit wisp-mcp;
    default = if isWindows then wisp-windows else wisp-mcp;
  }
  // pkgs.lib.optionalAttrs isWindows {
    inherit wisp-windows;
  };

  # Formatting (rustfmt + nixfmt + swiftformat) is unified under treefmt-nix;
  # see nix/parts/formatting.nix. Crane's cargoFmt is not duplicated here.
  checks =
    (
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
