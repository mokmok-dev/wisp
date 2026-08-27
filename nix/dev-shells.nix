# Developer shells.
#
# Plain function called from flake.nix's perSystem. Returns the `devShells`
# attrset. Needs `config` for the treefmt wrapper and the pre-commit install
# hook, plus the shared `pkgs`, `rustToolchain`, and pinned `swiftformat`.
#
# `default` is the turnkey environment entered by `nix develop` / direnv:
# pinned Rust toolchain, sccache, the treefmt wrapper, and the git-hooks
# tooling. Its shellHook installs the pre-commit git hooks so a fresh clone
# is fully set up in one step.
#
# `ci` is the minimal lint shell used by the GitHub Actions workflows.
{
  config,
  pkgs,
  rustToolchain,
  swiftformat,
}:
let
  treefmt = config.treefmt.build.wrapper;

  # Nix injects its own apple-sdk + xcrun wrapper, both of which are too old
  # for what WispAudioKit and GPUI need (Speech.SpeechAnalyzer, Core Audio
  # Process Tap, the Metal Toolchain). We:
  #
  #   1. Point DEVELOPER_DIR at the real Apple install so `xcrun` and
  #      the tools it dispatches to (swift, metal, ...) pick up the
  #      right SDK.
  #   2. Unset SDKROOT so it doesn't pin the macOS SDK and break
  #      `metal`, which needs the Metal SDK (xcrun resolves it from
  #      DEVELOPER_DIR when SDKROOT is empty).
  #   3. Prepend /usr/bin so the system `xcrun` (which knows about the
  #      Metal Toolchain asset) shadows Nix's xcbuild xcrun wrapper.
  darwinToolchainHook = pkgs.lib.optionalString pkgs.stdenv.isDarwin ''
    if [ -d /Applications/Xcode.app/Contents/Developer ]; then
      export DEVELOPER_DIR=/Applications/Xcode.app/Contents/Developer
    elif [ -d /Library/Developer/CommandLineTools ]; then
      export DEVELOPER_DIR=/Library/Developer/CommandLineTools
    fi
    unset SDKROOT
    export PATH="/usr/bin:$PATH"
  '';
in
{
  ci = pkgs.mkShell {
    packages = [
      rustToolchain
      pkgs.nixfmt
      swiftformat
    ];

    shellHook = darwinToolchainHook;
  };

  default = pkgs.mkShell {
    # Hook entries use their packages via generated Nix store paths.
    # Keep the direct developer tools explicit instead of adding the
    # overlapping pre-commit.enabledPackages list a second time.
    packages = [
      rustToolchain
      pkgs.cachix
      pkgs.sccache
      treefmt
    ];

    shellHook = ''
      export RUSTC_WRAPPER="${pkgs.sccache}/bin/sccache"
    ''
    + darwinToolchainHook
    # Install the fmt/clippy hooks and expose the pre-commit CLI.
    + config.pre-commit.shellHook;
  };
}
