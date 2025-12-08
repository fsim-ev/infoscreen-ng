{ rustPlatform, lib, ... }:

rustPlatform.buildRustPackage {
  pname = "pkg";
  version = "1.0";

  src = ./.;

  cargoHash = "";

  meta = with lib; {
    description = "A program";
    license = licenses.gpl2;
    platforms = platforms.all;
  };
}
