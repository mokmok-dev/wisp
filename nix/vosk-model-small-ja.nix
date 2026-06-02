# Japanese small Vosk model for local Windows dev / CI (optional; ~40 MiB).
{
  lib,
  stdenvNoCC,
  fetchurl,
  unzip,
}:

let
  version = "0.22";
in
stdenvNoCC.mkDerivation {
  pname = "vosk-model-small-ja";
  inherit version;

  src = fetchurl {
    url = "https://alphacephei.com/vosk/models/vosk-model-small-ja-${version}.zip";
    hash = "sha256-76CS0oAVOndhXp4MfXKD6T5gDePRnTvsaGxX7xnVLqw=";
  };

  nativeBuildInputs = [ unzip ];

  sourceRoot = "vosk-model-small-ja-${version}";

  installPhase = ''
    runHook preInstall
    mkdir -p "$out"
    cp -r . "$out/"
    runHook postInstall
  '';

  meta = {
    description = "Vosk small Japanese speech model";
    homepage = "https://alphacephei.com/vosk/models";
    license = lib.licenses.asl20;
  };
}
