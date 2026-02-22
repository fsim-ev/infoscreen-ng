{ rustPlatform, fetchFromGitHub, lib, ... }:

{
  bus-schedule = rustPlatform.buildRustPackage rec {
    pname = "bus-schedule";
    version = "0.2.0";

    src = ./.;
    sourceRoot = "bus-schedule";
    buildNoDefaultFeatures = true;
    buildFeatues = ["qt" "wayland" "x11"];
    # cargoBuildHook = ''
    #     cargo build --release --workspace --all-features
    #   '';
    cargoHash = "sha256-ZAty04BIdGuYH0YLpFaTXqBn7edlo+6pLIc1Sj4xU3Y=";


    meta = with lib; {
      description = "A timetable program";
      license = licenses.gpl2;
      platforms = platforms.all;
    };
  };
  todo-list = rustPlatform.buildRustPackage rec {
    pname = "todo-list";
    version = "0.2.0";

    # src = fetchFromGitHub{
    #   owner = "haennes";
    #   repo = "infoscreen-ng-mirror-tmp";
    #   hash = "sha256-EsMBu6ifl+P8zCQnzkY1pN4lCLTmixvKS8qDGxiuqi8=";
    #   rev = "807682b4695814f91eabb110bd0886a945c362e2";
    # };
    src = ./.;
    sourceRoot = "source/todo-list";
    # cargoRoot = "..";
    buildNoDefaultFeatures = true;
    buildFeatues = ["qt" "wayland" "x11"];
    # cargoBuildHook = ''
    #     cargo build --release --workspace --all-features
    #   '';
    cargoHash = "sha256-ZAty04BIdGuYH0YLpFaTXqBn7edlo+6pLIc1Sj4xU3Y=";


    meta = with lib; {
      description = "A timetable program";
      license = licenses.gpl2;
      platforms = platforms.all;
    };
  };
  timetable = rustPlatform.buildRustPackage {
    pname = "timetable";
    version = "0.2.0";

    src = ./.;
    buildNoDefaultFeatures = true;
    buildFeatues = ["qt" "wayland" "x11"];
    # cargoBuildHook = ''
    #     cargo build --release --workspace --all-features
    #   '';
    cargoHash = "sha256-ZAty04BIdGuYH0YLpFaTXqBn7edlo+6pLIc1Sj4xU3Y=";


    meta = with lib; {
      description = "A timetable program";
      license = licenses.gpl2;
      platforms = platforms.all;
    };
  };
}
