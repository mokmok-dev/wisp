{
  nixpkgs-swiftformat,
  pkgs,
  system,
}:
if pkgs.stdenv.isLinux then
  (import nixpkgs-swiftformat { inherit system; }).swiftformat
else
  pkgs.swiftformat
