{
  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs?ref=nixos-unstable";
    # Keep Linux swiftformat on the last known-good nixpkgs revision. Newer
    # nixpkgs currently builds Swift 5.10 with a Clang-only TLS flag.
    nixpkgs-swiftformat.url = "github:nixos/nixpkgs/b5aa0fbd538984f6e3d201be0005b4463d8b09f8";
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
        in
        {
          devShells = import ./nix/dev-shells.nix {
            inherit pkgs system;
            inherit (inputs) nixpkgs-swiftformat;
          };

          formatter = pkgs.nixfmt;
        };
    };
}
