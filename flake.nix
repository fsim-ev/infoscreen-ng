{
  description = "A very basic flake";

  inputs = { nixpkgs.url = "github:nixos/nixpkgs?ref=nixos-unstable"; };

  outputs = { self, nixpkgs, }@inputs:
    let
      pkgs = import nixpkgs { system = "x86_64-linux"; };
      libPath = with pkgs;
        lib.makeLibraryPath [ fontconfig libxkbcommon libGL wayland ];
    in {
      devShells.x86_64-linux.default = pkgs.mkShell {
        nativeBuildInputs = with pkgs; [
          rustc
          cargo
          clippy
          # openssl
          # pkg-config
          rust-analyzer
          rustfmt # formatter
        ];

        LD_LIBRARY_PATH = "$LD_LIBRARY_PATH:${libPath}";

        # uncomment this is you get some kind of ssl error, usually on anything networking related using reqwest
        # PKG_CONFIG_PATH = "${pkgs.openssl.dev}/lib/pkgconfig";
      };

      packages.x86_64-linux.default = pkgs.callPackage ./pkg.nix { };
    };
}
