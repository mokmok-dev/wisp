# Unified formatting and git hooks.
#
# Plain function called from flake.nix's perSystem with the shared `rustToolchain`
# and pinned `swiftformat`. Returns the `treefmt` and `pre-commit` option values
# that flake.nix hands to the treefmt-nix / git-hooks flake modules.
#
# treefmt-nix drives every formatter from one config and is surfaced both as the
# flake `formatter` (`nix fmt`) and a `treefmt` flake check (`nix flake check`),
# replacing the previous ad-hoc `nixfmt` formatter and Crane's cargoFmt check.
#
# git-hooks.nix (pre-commit) reuses that same treefmt wrapper plus a clippy hook.
# Its sandboxed flake check is disabled on purpose: clippy/tests already run as
# dedicated Crane checks, and running cargo inside the pre-commit check would
# need network/registry access the Nix sandbox denies. The hooks are instead
# installed into the dev shell (see nix/dev-shells.nix) for turnkey setup.
{
  rustToolchain,
  swiftformat,
}:
{
  treefmt = {
    projectRootFile = "flake.nix";

    # Never touch vendored third-party code or build output.
    settings.global.excludes = [
      "*.lock"
      "vendor/**"
      "target/**"
      "result"
      "result-*"
    ];
    programs.nixfmt.enable = true;

    # Pin rustfmt to the workspace toolchain and honour the repo-level
    # .rustfmt.toml, keeping formatter versions and style aligned with CI.
    programs.rustfmt = {
      enable = true;
      package = rustToolchain;
      edition = "2024";
    };

    # nicklockwood/SwiftFormat.
    # treefmt-nix only ships the unrelated apple/swift-format module, so the
    # formatter is wired up by hand. Repo-level .swiftformat is honoured.
    #
    # Scoped to the WispAudioKit library + tests to match the existing swift
    # CI (`nix develop .#ci --command swiftformat --lint …`); the Package.swift
    # manifest and the standalone icon helper are intentionally left alone.
    settings.formatter.swiftformat = {
      command = "${swiftformat}/bin/swiftformat";
      includes = [
        "native/WispAudioKit/Sources/**/*.swift"
        "native/WispAudioKit/Tests/**/*.swift"
      ];
    };
  };

  pre-commit = {
    # See the module header: the Crane checks own clippy/tests.
    check.enable = false;

    settings.hooks = {
      treefmt.enable = true;

      clippy = {
        enable = true;
        packageOverrides = {
          cargo = rustToolchain;
          clippy = rustToolchain;
        };
        settings = {
          denyWarnings = true;
          # A fresh clone must be able to fetch missing registry sources.
          offline = false;
          # Match the portable Crane clippy check on both macOS and Linux:
          # default features, --all-targets, workspace minus the desktop
          # crate (which needs the dedicated Xcode workflow).
          allFeatures = false;
          extraArgs = "--workspace --exclude wisp-desktop --all-targets";
        };
      };
    };
  };
}
