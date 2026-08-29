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
      systems = [
        "aarch64-darwin"
        "aarch64-linux"
        "x86_64-darwin"
        "x86_64-linux"
      ];

      # treefmt-nix and git-hooks contribute the `treefmt` and `pre-commit`
      # perSystem options that the root perSystem below fills in. Everything
      # else is wired explicitly in that perSystem so the data flow (which
      # `pkgs`/toolchain feeds which output) is readable top to bottom.
      imports = [
        inputs.treefmt-nix.flakeModule
        inputs.git-hooks.flakeModule
      ];

      perSystem =
        { config, system, ... }:
        let
          # 1. Per-system foundations. Built once here and passed by name into
          #    every helper below, so there is a single source for the nixpkgs
          #    instance, the pinned Rust toolchain, and the pinned SwiftFormat.
          pkgs = import inputs.nixpkgs {
            inherit system;
            overlays = [ inputs.rust-overlay.overlays.default ];
          };
          rustToolchain = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;
          swiftformat = pkgs.swiftformat;

          # 2. Crane packages + checks. Returns { packages, checks }.
          crane = import ./nix/packages.nix {
            inherit
              inputs
              pkgs
              system
              rustToolchain
              ;
          };

          # 3. treefmt + git-hooks settings. Returns { treefmt, pre-commit }
          #    attrsets that are handed to the imported flake modules below.
          formatting = import ./nix/formatting.nix {
            inherit rustToolchain swiftformat;
          };
        in
        {
          # Packages: wisp-desktop (macOS-only) on macOS hosts.
          inherit (crane) packages;

          # Formatting: feed the settings to the treefmt-nix / git-hooks modules.
          inherit (formatting) treefmt;
          pre-commit = formatting.pre-commit;

          # Checks: Crane's package check plus clippy/tests (Linux), and the
          # treefmt run exposed under its original `formatting` name. treefmt-nix
          # additionally exposes the standard `checks.<system>.treefmt`.
          checks = crane.checks // {
            formatting = config.treefmt.build.check config.treefmt.projectRoot;
          };

          # Dev shells: `default` (turnkey) and `ci` (minimal lint shell). Needs
          # `config` for the treefmt wrapper and the pre-commit install hook.
          devShells = import ./nix/dev-shells.nix {
            inherit
              config
              pkgs
              rustToolchain
              swiftformat
              ;
          };
        };
    };
}
