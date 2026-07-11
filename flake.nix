{
  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs?ref=nixos-unstable";
    # Keep Linux swiftformat on the last known-good nixpkgs revision. Newer
    # nixpkgs currently builds Swift 5.10 with a Clang-only TLS flag.
    nixpkgs-swiftformat.url = "github:nixos/nixpkgs/b5aa0fbd538984f6e3d201be0005b4463d8b09f8";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay.url = "github:oxalica/rust-overlay";
    rust-overlay.inputs.nixpkgs.follows = "nixpkgs";
  };

  outputs =
    {
      self,
      nixpkgs,
      nixpkgs-swiftformat,
      flake-utils,
      rust-overlay,
    }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = import nixpkgs {
          inherit system;
          overlays = [ rust-overlay.overlays.default ];
        };
        swiftformat =
          if pkgs.stdenv.isLinux then
            (import nixpkgs-swiftformat { inherit system; }).swiftformat
          else
            pkgs.swiftformat;
        rustToolchain = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;

        # Shared by both devShells on macOS. Nix injects its own apple-sdk +
        # xcrun wrapper, both of which are too old for what WispAudioKit and
        # GPUI need (Speech.SpeechAnalyzer, Core Audio Process Tap, the
        # Metal Toolchain). We:
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
        devShells = {
          ci = pkgs.mkShell {
            packages = with pkgs; [
              rustToolchain
              nixfmt
              swiftformat
            ];

            shellHook = darwinToolchainHook;
          };

          default = pkgs.mkShell {
            packages = with pkgs; [
              rustToolchain
              sccache
            ];

            shellHook = ''
              export RUSTC_WRAPPER="${pkgs.sccache}/bin/sccache"
            ''
            + darwinToolchainHook;
          };
        };

        formatter = pkgs.nixfmt;
      }
    );
}
