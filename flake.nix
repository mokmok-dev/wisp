{
  inputs = {
    nixpkgs.url = "https://flakehub.com/f/NixOS/nixpkgs/0.1";
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

          # cccc is built with the workspace toolchain rather than nixpkgs'
          # Rust: its own manifest requires Rust 1.95 and this keeps the binary
          # reproducible against the version the rest of the repo pins.
          rustPlatform = pkgs.makeRustPlatform {
            cargo = rustToolchain;
            rustc = rustToolchain;
          };

          cccc = rustPlatform.buildRustPackage rec {
            pname = "cccc";
            version = "1.8.5";

            src = pkgs.fetchFromGitHub {
              owner = "moznion";
              repo = "cccc";
              tag = "v${version}";
              hash = "sha256-8QfPfyg7Y05wbiWnLyGLpDe4F1SkvTJuZPLn+5ZQIpw=";
            };

            # The Kotlin adapter pulls its grammar from a git dependency, which
            # crates.io cannot publish and this repo does not analyze.
            cargoBuildFlags = [ "--no-default-features" ];
            cargoHash = "sha256-iWzC0LORv4uxacv8DrMlVB8XUK4cktrazoAv+IQYwGg=";

            # cccc's Ruby adapter runs bindgen over Ruby's vendored Prism C
            # source.
            nativeBuildInputs = [ pkgs.libclang ];
            LIBCLANG_PATH = "${pkgs.libclang.lib}/lib";

            # Upstream's cache tests shell out to git inside a work tree; the
            # build sandbox provides neither.
            doCheck = false;

            meta = {
              description = "Cognitive and cyclomatic complexity measurement";
              homepage = "https://github.com/moznion/cccc";
              license = pkgs.lib.licenses.mit;
              mainProgram = "cccc";
            };
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

          # clippy, cargo test, and the desktop binary are intentionally NOT
          # flake packages/checks: they need host Xcode (`swift` for
          # WispAudioKit, `metal` for gpui shaders), which is unreachable from
          # a pure Nix sandbox. Determinate CI builds every flake derivation,
          # so those stay on `nix develop` (see the `rust` job and release
          # workflow).

          packages = {
            inherit cccc;
          };

          checks.complexity =
            pkgs.runCommandLocal "cccc-complexity"
              {
                nativeBuildInputs = [ cccc ];
              }
              ''
                cd ${./.}
                cccc --no-cache --table .
                touch "$out"
              '';

          devShells = {
            default = pkgs.mkShellNoCC {
              inputsFrom = [ config.pre-commit.devShell ];

              packages = with pkgs; [
                # Mutation testing for the pure-logic crates (see
                # .cargo/mutants.toml). The `mutants` CI job runs this
                # straight from the dev shell.
                cargo-mutants
                cccc
                rustToolchain
                sccache
              ];

              shellHook = ''
                export RUSTC_WRAPPER="${pkgs.sccache}/bin/sccache"
              ''
              + darwinToolchainHook;
            };
          };

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

      # Nixpkgs 26.11+ and Determinate Nix no longer support Intel macOS
      # hosts, so CI and local builds target Apple Silicon only.
      systems = [
        "aarch64-darwin"
      ];
    };
}
