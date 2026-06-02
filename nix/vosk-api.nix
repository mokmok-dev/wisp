# Prebuilt libvosk from https://github.com/alphacep/vosk-api/releases (v0.3.45).
# WASAPI is provided by the Rust `wasapi` crate at compile time on Windows only;
# it does not need a separate Nix package.
{
  lib,
  stdenv,
  fetchurl,
  unzip,
}:

let
  version = "0.3.45";

  srcInfo =
    if stdenv.hostPlatform.isLinux && stdenv.hostPlatform.isx86_64 then
      {
        url = "https://github.com/alphacep/vosk-api/releases/download/v${version}/vosk-linux-x86_64-${version}.zip";
        hash = "sha256-u9yO2FxDl59kQxQoiXcOqVy/vFbP+1xdzXOvqHXF+7I=";
        subdir = "vosk-linux-x86_64-${version}";
      }
    else if stdenv.hostPlatform.isLinux && stdenv.hostPlatform.isAarch64 then
      {
        url = "https://github.com/alphacep/vosk-api/releases/download/v${version}/vosk-linux-aarch64-${version}.zip";
        hash = "sha256-ReldN3Vd6wdWjnlJfX/rqMA67lqeBx3ymWGqAj/ZRUE=";
        subdir = "vosk-linux-aarch64-${version}";
      }
    else if stdenv.hostPlatform.isWindows then
      {
        url = "https://github.com/alphacep/vosk-api/releases/download/v${version}/vosk-win64-${version}.zip";
        hash = "sha256-8dzJzKRgYw+B6o9xeU9pyAvtZVbSpOYje1eF4dLf80s=";
        subdir = "vosk-win64-${version}";
      }
    else
      throw "vosk-api: unsupported platform ${stdenv.hostPlatform.system} (no official prebuilt; macOS uses Swift SpeechAnalyzer)";

in
stdenv.mkDerivation {
  pname = "vosk-api";
  inherit version;

  src = fetchurl {
    inherit (srcInfo) url hash;
  };

  nativeBuildInputs = [ unzip ];

  sourceRoot = srcInfo.subdir;

  installPhase = ''
    runHook preInstall
    mkdir -p "$out/lib" "$out/include"
    cp -L libvosk.so libvosk.dll libvosk.lib "$out/lib/" 2>/dev/null || true
    cp lib*.dll "$out/lib/" 2>/dev/null || true
    cp vosk_api.h "$out/include/"
    runHook postInstall
  '';

  meta = {
    description = "Vosk offline speech recognition library (prebuilt)";
    homepage = "https://alphacephei.com/vosk/";
    license = lib.licenses.asl20;
    platforms = lib.platforms.linux ++ lib.platforms.windows;
  };
}
