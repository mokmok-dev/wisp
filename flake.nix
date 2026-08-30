{
  inputs = {
    nixpkgs.url = "https://flakehub.com/f/NixOS/nixpkgs/0.1";
    crane.url = "github:ipetkov/crane";
    flake-parts.url = "github:hercules-ci/flake-parts";
    rust-overlay.url = "github:oxalica/rust-overlay";
    rust-overlay.inputs.nixpkgs.follows = "nixpkgs";
    treefmt-nix.url = "github:numtide/treefmt-nix";
    treefmt-nix.inputs.nixpkgs.follows = "nixpkgs";
    git-hooks.url = "github:cachix/git-hooks.nix";
    git-hooks.inputs.nixpkgs.follows = "nixpkgs";
  };

  outputs =
    { flake-parts, ... }@inputs:
    flake-parts.lib.mkFlake { inherit inputs; } {
      imports = [
        inputs.treefmt-nix.flakeModule
        inputs.git-hooks.flakeModule
      ];

      perSystem =
        {
          config,
          pkgs,
          system,
          ...
        }:
        let
          rustToolchain = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;
          craneLib = (inputs.crane.mkLib pkgs).overrideToolchain rustToolchain;
          wisp-desktop = import ./apps/wisp-desktop/package.nix {
            lib = pkgs.lib;
            inherit craneLib rustToolchain;
          };

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
          _module.args = {
            pkgs = import inputs.nixpkgs {
              inherit system;
              overlays = [ inputs.rust-overlay.overlays.default ];
            };
          };

          # clippy and the workspace test are intentionally NOT exposed as
          # flake checks: a pure derivation build on macOS would need Xcode's
          # `metal` (gpui compiles shaders with it), which is unreachable from
          # inside a Nix sandbox. They are run through `nix develop` in CI
          # instead (see .github/workflows/rust.yaml and rust-macos.yaml).

          devShells = {
            default = pkgs.mkShellNoCC {
              inputsFrom = [ config.pre-commit.devShell ];

              packages = with pkgs; [
                rustToolchain
                sccache
                # shiguredo_opus can build xiph/opus from source via CMake
                # (its `source-build` feature) and needs the CMake toolchain.
                cmake
              ];

              shellHook = ''
                export RUSTC_WRAPPER="${pkgs.sccache}/bin/sccache"
              ''
              + darwinToolchainHook;
            };
          };

          packages.wisp-desktop = wisp-desktop;

          pre-commit.settings = {
            hooks = {
              treefmt.enable = true;
            };
          };

          treefmt = {
            projectRootFile = "flake.nix";
            programs = {
              nixfmt.enable = true;
              rustfmt.enable = true;
              rustfmt.package = rustToolchain;
            };
            settings = {
              formatter = {
                swiftformat = {
                  command = "${pkgs.swiftformat}/bin/swiftformat";
                  includes = [
                    "native/**/*.swift"
                  ];
                };
              };
            };
          };
        };

      systems = [
        "aarch64-darwin"
        "x86_64-darwin"
      ];
    };
}
