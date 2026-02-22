{
  description = "A very basic flake";

  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs?ref=nixos-unstable";
    crane.url = "github:ipetkov/crane";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, crane, flake-utils }:
     flake-utils.lib.eachDefaultSystem (
          system:
          let
            pkgs = nixpkgs.legacyPackages.${system};

            inherit (pkgs) lib;

            craneLib = crane.mkLib pkgs;
            src = craneLib.cleanCargoSource ./.;

            # Common arguments can be set here to avoid repeating them later
            commonArgs = {
              inherit src;
              strictDeps = true;

              buildInputs =  with pkgs; [
                wayland
                wayland-protocols
                # Add additional build inputs here
              ];
              propagetedBuildInputs = with pkgs; [
                wayland
                wayland-protocols
                
              ];
            };
        # Build *just* the cargo dependencies (of the entire workspace),
        # so we can reuse all of that work (e.g. via cachix) when running in CI
        # It is *highly* recommended to use something like cargo-hakari to avoid
        # cache misses when building individual top-level-crates
        cargoArtifacts = craneLib.buildDepsOnly commonArgs;

        individualCrateArgs = commonArgs // {
          inherit cargoArtifacts;
          inherit (craneLib.crateNameFromCargoToml { inherit src; }) version;
          doCheck = false;
        };
        libPath = with pkgs;
          lib.makeLibraryPath [ fontconfig libxkbcommon libGL wayland ];
        wrapper = name: {
            buildInputs = [pkgs.makeWrapper ];
            postInstall = ''
                wrapProgram $out/bin/${name} \
                  --prefix LD_LIBRARY_PATH : ${libPath}
              '';
          
        };

        infoscreen-bus-schedule = craneLib.buildPackage (
          individualCrateArgs  // (wrapper "infoscreen-bus-schedule") // rec {
            pname = "infoscreen-bus-schedule";
            cargoExtraArgs = "-p infoscreen-bus-schedule --all-features";
            src = ./.;
            meta.mainProgram = pname;
          }
        );
        infoscreen-todo-list = craneLib.buildPackage (
          individualCrateArgs  // (wrapper "infoscreen-todo-list") // rec {
            pname = "infoscreen-todo-list";
            cargoExtraArgs = "-p infoscreen-todo-list --all-features";
            src = ./.;
            meta.mainProgram = pname;
          }
        );
        infoscreen-timetable = craneLib.buildPackage (
          individualCrateArgs // (wrapper "infoscreen-timetable") // rec {
            pname = "infoscreen-timetable";
            cargoExtraArgs = "--all-features";
            src = ./.;
            meta.mainProgram = pname;
          }
        );
    in {
      devShells.default = craneLib.devShell {
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

      packages = {
        inherit infoscreen-bus-schedule infoscreen-todo-list infoscreen-timetable;
      };
    });
}
