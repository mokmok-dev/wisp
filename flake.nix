{
  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs?ref=nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay.url = "github:oxalica/rust-overlay";
    rust-overlay.inputs.nixpkgs.follows = "nixpkgs";
  };

  outputs =
    {
      self,
      nixpkgs,
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
        rustToolchain = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;
      in
      {
        devShells = {
          ci = pkgs.mkShell {
            packages = with pkgs; [
              rustToolchain
              nixfmt
              swiftformat
            ];
          };

          default = pkgs.mkShell {
            packages = with pkgs; [
              rustToolchain
              sccache
            ];

            shellHook = ''
              export RUSTC_WRAPPER="${pkgs.sccache}/bin/sccache"
            '';
          };
        };

        formatter = pkgs.nixfmt;
      }
    );
}
