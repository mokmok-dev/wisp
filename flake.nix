{
  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs?ref=nixos-unstable";
    # Keep Linux swiftformat on the last known-good nixpkgs revision. Newer
    # nixpkgs currently builds Swift 5.10 with a Clang-only TLS flag.
    nixpkgs-swiftformat.url = "github:nixos/nixpkgs/b5aa0fbd538984f6e3d201be0005b4463d8b09f8";
    crane.url = "github:ipetkov/crane";
    flake-parts.url = "github:hercules-ci/flake-parts";
    rust-overlay.url = "github:oxalica/rust-overlay";
    rust-overlay.inputs.nixpkgs.follows = "nixpkgs";
  };

  outputs =
    {
      flake-parts,
      ...
    }@inputs:
    flake-parts.lib.mkFlake { inherit inputs; } {
      systems = [
        "aarch64-darwin"
        "aarch64-linux"
        "x86_64-darwin"
        "x86_64-linux"
      ];

      perSystem =
        { system, ... }:
        let
          pkgs = import inputs.nixpkgs {
            inherit system;
            overlays = [ inputs.rust-overlay.overlays.default ];
          };
          rustToolchain = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;
          nativeCraneOutputs = import ./nix/crane.nix {
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
          windowsCraneOutputs = import ./nix/crane.nix {
            pkgs = windowsPkgs;
            craneLib = inputs.crane.mkLib windowsPkgs;
            rustToolchain = p: p.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;
          };
        in
        {
          inherit (nativeCraneOutputs) checks;
          packages =
            nativeCraneOutputs.packages
            // pkgs.lib.optionalAttrs pkgs.stdenv.hostPlatform.isLinux {
              inherit (windowsCraneOutputs.packages) wisp-windows;
            };

          devShells = import ./nix/dev-shells.nix {
            inherit pkgs system;
            inherit (inputs) nixpkgs-swiftformat;
          };

          formatter = pkgs.nixfmt;
        };
    };
}
