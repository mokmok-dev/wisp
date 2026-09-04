{
  lib,
  craneLib,
  rustToolchain,
}:
let
  crane = craneLib.overrideToolchain rustToolchain;
  # Cargo sources plus the web UI bundle: `web_shell.rs` embeds `ui/dist`
  # (apps/wisp-desktop/ui) into the binary at compile time. When `ui/dist`
  # has no build output, a placeholder page is embedded instead.
  src = lib.cleanSourceWith {
    src = ../../.;
    filter =
      path: type:
      craneLib.filterCargoSources path type || type == "directory" || lib.hasInfix "ui/dist/" path;
  };

  # The desktop target needs Xcode 26 from the host and is built by the
  # dedicated macOS workflow. These arguments match the portable Rust CI job.
  portableArgs = {
    inherit src;
    # wisp-webview links lb-wry (WKWebView/WebKitGTK); the portable Linux
    # sandbox has no WebKit, so both desktop-only crates are excluded here.
    cargoExtraArgs = "--locked --workspace --exclude wisp-desktop --exclude wisp-webview";
    strictDeps = true;
  };
  portableArtifacts = crane.buildDepsOnly portableArgs;
in
crane.buildPackage (
  portableArgs
  // {
    cargoArtifacts = portableArtifacts;
    cargoExtraArgs = "--locked -p wisp-desktop";
    doCheck = false;
    meta = {
      description = "wisp desktop app";
      mainProgram = "wisp-desktop";
      platforms = lib.platforms.darwin;
    };
  }
)
