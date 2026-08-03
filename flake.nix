{
  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs?ref=nixos-unstable";
    flake-parts.url = "github:hercules-ci/flake-parts";
    treefmt-nix.url = "github:numtide/treefmt-nix";
    treefmt-nix.inputs.nixpkgs.follows = "nixpkgs";
    rust-flake.url = "github:juspay/rust-flake";
    rust-flake.inputs.nixpkgs.follows = "nixpkgs";
  };

  outputs =
    inputs:
    inputs.flake-parts.lib.mkFlake { inherit inputs; } {
      imports = [
        inputs.treefmt-nix.flakeModule
        inputs.rust-flake.flakeModules.default
        inputs.rust-flake.flakeModules.nixpkgs
      ];

      perSystem =
        {
          self',
          pkgs,
          config,
          system,
          ...
        }:
        let
          appleSwift = pkgs.writeShellScriptBin "swift" ''
            exec /usr/bin/xcrun swift "$@"
          '';
          appleXcrun = pkgs.writeShellScriptBin "xcrun" ''
            export DEVELOPER_DIR=/Applications/Xcode.app/Contents/Developer
            unset SDKROOT
            exec /usr/bin/xcrun "$@"
          '';
          workspaceRoot = toString ./.;
          swiftPrefix = "native/WispAudioKit";
          sourceFilter =
            path: type:
            let
              relativePath = pkgs.lib.removePrefix "${workspaceRoot}/" (toString path);
            in
            config.rust-project.crane-lib.filterCargoSources path type
            || relativePath == "native"
            || relativePath == swiftPrefix
            || relativePath == "${swiftPrefix}/Package.swift"
            || relativePath == "${swiftPrefix}/Sources"
            || pkgs.lib.hasPrefix "${swiftPrefix}/Sources/" relativePath;
        in
        {
          devShells.default = pkgs.mkShellNoCC {
            inputsFrom = [ self'.devShells.rust ];
            packages = [ ];
          };

          rust-project = {
            toolchain = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;
            src = pkgs.lib.cleanSourceWith {
              src = ./.;
              filter = sourceFilter;
            };
            defaults.perCrate.crane.args.nativeBuildInputs = pkgs.lib.mkAfter (
              pkgs.lib.optionals pkgs.stdenv.isDarwin [
                appleSwift
                appleXcrun
              ]
            );
          };

          treefmt = {
            projectRootFile = "flake.nix";
            programs = {
              nixfmt.enable = true;
              swift-format.enable = pkgs.stdenv.isDarwin;
              taplo.enable = true;
            };
          };
        };

      systems = [
        "aarch64-darwin"
        "aarch64-linux"
        "x86_64-linux"
        "x86_64-w64-mingw32"
      ];
    };
}
