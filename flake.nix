{
  inputs = {
    nixpkgs.url = "https://flakehub.com/f/NixOS/nixpkgs/0.1";
    flake-parts.url = "github:hercules-ci/flake-parts";
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
          lib,
          pkgs,
          ...
        }:
        let
          # The sandbox exposes the host toolchain through /usr/bin, but SwiftPM
          # cannot use its own sandbox under Nix, and its default caches sit in
          # places the build user cannot write. Every Swift gate therefore works
          # on a writable copy with the caches redirected into the build tree.
          swiftPackage = ''
            export PATH=/usr/bin:/bin:/usr/sbin:/sbin
            export HOME=$PWD/home
            mkdir -p "$HOME"
            export CLANG_MODULE_CACHE_PATH=$PWD/clang-cache
            export SWIFT_MODULE_CACHE_PATH=$PWD/swift-cache
            cp -R "$src" pkg
            chmod -R u+w pkg
            cd pkg
          '';

          # A Nix builder does not inherit the invoking shell's environment, so
          # the XCTest opt-out has to be read at evaluation time and handed to
          # the derivation. `getEnv` yields "" under pure evaluation, which keeps
          # `nix flake check` on the fail-closed default; the opt-out needs
          # `--impure` to be visible at all.
          allowSkippedTests = builtins.getEnv "WISP_ALLOW_SKIPPED_TESTS";
        in
        {
          packages =
            let
              version = lib.trim (builtins.readFile ./VERSION);

              # Both bundles differ only in their binary and Info.plist.
              mkApp =
                name: configuration:
                {
                  binary,
                  infoPlist,
                  entitlements,
                  icon ? null,
                }:
                pkgs.runCommandLocal "${name}-${configuration}"
                  {
                    src = ./.;
                  }
                  ''
                    set -euo pipefail
                    ${swiftPackage}
                    swift build -c ${configuration} --disable-sandbox \
                      --scratch-path "$PWD/.scratch" >&2
                    bin="$(swift build -c ${configuration} --show-bin-path \
                      --disable-sandbox --scratch-path "$PWD/.scratch")"

                    app="$out/${name}.app"
                    mkdir -p "$app/Contents/MacOS"
                    ${lib.optionalString (icon != null) ''
                      mkdir -p "$app/Contents/Resources"
                      cp ${icon} "$app/Contents/Resources/AppIcon.icns"
                    ''}
                    cp "$bin/${binary}" "$app/Contents/MacOS/${binary}"
                    chmod +x "$app/Contents/MacOS/${binary}"
                    sed "s/__VERSION__/${version}/g" ${infoPlist} \
                      > "$app/Contents/Info.plist"
                    plutil -lint "$app/Contents/Info.plist" >&2

                    codesign --force --deep --options runtime \
                      --entitlements ${entitlements} \
                      --sign - "$app"
                    codesign --verify --strict "$app"
                  '';

              wisp = mkApp "Wisp";
              wispctl = mkApp "Wispctl";
            in
            {
              Wisp = wisp "release" {
                binary = "WispDesktop";
                infoPlist = ./App/Info.plist;
                entitlements = ./App/Wisp.entitlements;
                icon = ./App/AppIcon.icns;
              };
              Wisp-debug = wisp "debug" {
                binary = "WispDesktop";
                infoPlist = ./App/Info.plist;
                entitlements = ./App/Wisp.entitlements;
                icon = ./App/AppIcon.icns;
              };

              wispctl = wispctl "release" {
                binary = "wispctl";
                infoPlist = ./App/wispctl/Info.plist;
                entitlements = ./App/wispctl/wispctl.entitlements;
              };
              default = config.packages.Wisp;
            };

          # A `Wisp.app` launched straight from the store skips LaunchServices,
          # so only the CLI gets a `run` target; `open result/Wisp.app` covers
          # the app.
          apps.wispctl = {
            type = "app";
            program = "${config.packages.wispctl}/Wispctl.app/Contents/MacOS/wispctl";
          };

          checks = {
            build = pkgs.runCommandLocal "wisp-build" { src = ./.; } ''
              set -euo pipefail
              ${swiftPackage}
              swift build --disable-sandbox --scratch-path "$PWD/.scratch" >&2
              touch "$out"
            '';

            # XCTest ships with Xcode, not with the Command Line Tools, and
            # without it SwiftPM cannot even build a test target. Probe once so
            # a toolchain-only host reports a skip instead of a fake failure.
            #
            # A skip is not a pass, so it is refused unless the caller opts in.
            # The opt-out is read at evaluation time into the derivation's env
            # (see `allowSkippedTests`): a builder script cannot see the shell
            # that invoked it, so opting in also needs `--impure`.
            tests =
              pkgs.runCommandLocal "wisp-tests"
                {
                  src = ./.;
                  env.WISP_ALLOW_SKIPPED_TESTS = allowSkippedTests;
                }
                ''
                  set -euo pipefail
                  export PATH=/usr/bin:/bin:/usr/sbin:/sbin
                  export HOME=$PWD/home
                  mkdir -p "$HOME" "$out"
                  export CLANG_MODULE_CACHE_PATH=$PWD/clang-cache
                  export SWIFT_MODULE_CACHE_PATH=$PWD/swift-cache

                  # A bare `import XCTest` fails even with Xcode selected: the
                  # module lives under the SDK platform, which swiftc only searches
                  # when told to. Resolve that platform first, then probe with it.
                  probe=$PWD/xctest-probe.swift
                  echo 'import XCTest' > "$probe"
                  platform=$(/usr/bin/xcrun --sdk macosx --show-sdk-platform-path 2>/dev/null || true)
                  probe_flags=()
                  if [ -n "$platform" ] && [ -d "$platform/Developer/usr/lib" ]; then
                    probe_flags+=(-F "$platform/Developer/Library/Frameworks")
                    probe_flags+=(-I "$platform/Developer/usr/lib")
                  fi

                  if ! swiftc -typecheck "''${probe_flags[@]}" "$probe" > "$PWD/xctest-probe.log" 2>&1; then
                    if grep -qE "no such module 'XCTest'|unable to resolve module dependency: 'XCTest'" "$PWD/xctest-probe.log"; then
                      echo "skipped: XCTest is not available to the Swift compiler" > "$out/status"
                      cp "$PWD/xctest-probe.log" "$out/xctest-probe.log"
                      if [ "''${WISP_ALLOW_SKIPPED_TESTS:-}" != "1" ]; then
                        {
                          echo "wisp-tests: FAILED — this host has no XCTest module, so no suite ran."
                          echo "wisp-tests: XCTest ships with Xcode, not the Command Line Tools."
                          echo "wisp-tests: point DEVELOPER_DIR at a full Xcode, or opt in explicitly with"
                          echo "wisp-tests:   WISP_ALLOW_SKIPPED_TESTS=1 nix build --impure .#checks.aarch64-darwin.tests"
                          echo "wisp-tests: (the opt-out is evaluated by Nix, so --impure is required)"
                          echo "wisp-tests: probe output:"
                          sed 's/^/    /' "$PWD/xctest-probe.log"
                        } >&2
                        exit 1
                      fi
                      {
                        echo "wisp-tests: SKIPPED — the active developer directory has no XCTest module."
                        echo "wisp-tests: XCTest ships with Xcode, not the Command Line Tools, so SwiftPM cannot build any test target here."
                        echo "wisp-tests: this run is not evidence that the suites pass; CI runs it against a full Xcode."
                        echo "wisp-tests: opt-in accepted (WISP_ALLOW_SKIPPED_TESTS=1). Probe output:"
                        sed 's/^/    /' "$PWD/xctest-probe.log"
                      } >&2
                      exit 0
                    fi
                    cat "$PWD/xctest-probe.log" >&2
                    exit 1
                  fi

                  ${swiftPackage}
                  # A run that executes no suite still exits 0, so the report is
                  # part of the gate.
                  swift test --disable-sandbox --scratch-path "$PWD/.scratch" 2>&1 | tee "$PWD/test.log"
                  grep -qE "Executed [0-9]+ tests" "$PWD/test.log"
                  echo "passed: XCTest suites executed" > "$out/status"
                '';
          };

          devShells = {
            default = pkgs.mkShell {
              inputsFrom = [ config.pre-commit.devShell ];
              packages = with pkgs; [
                clang
                swift
                swiftPackages.swiftpm
                swift-format
              ];

              shellHook = ''
                if [ -d /Applications/Xcode.app/Contents/Developer ]; then
                  export DEVELOPER_DIR=/Applications/Xcode.app/Contents/Developer
                elif [ -d /Library/Developer/CommandLineTools ]; then
                  export DEVELOPER_DIR=/Library/Developer/CommandLineTools
                fi
                unset SDKROOT
                export PATH="/usr/bin:$PATH"
              '';
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
              # Swift uses the tool's stock style; the repository commits no
              # `.swift-format`, so upstream defaults are the contract.
              swift-format = {
                enable = true;
                includes = [
                  "Sources/**/*.swift"
                  "Tests/**/*.swift"
                  "App/**/*.swift"
                ];
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
