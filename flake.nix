{
  description = "A masterless, encrypted clipboard-history mesh";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs =
    { self, nixpkgs, ... }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
      forAllSystems = nixpkgs.lib.genAttrs systems;
      packageFor =
        system: withUi:
        let
          pkgs = nixpkgs.legacyPackages.${system};
        in
        pkgs.rustPlatform.buildRustPackage {
          pname = "clip-sync";
          version = "0.1.0";
          src = ./.;
          cargoLock.lockFile = ./Cargo.lock;
          cargoBuildFeatures = pkgs.lib.optionals withUi [ "ui" ];
          RUST_MIN_STACK = "16777216";
          nativeBuildInputs = with pkgs; [
            perl
            pkg-config
          ];
          buildInputs = pkgs.lib.optionals withUi (
            with pkgs;
            [
              libGL
              libxkbcommon
              wayland
            ]
          );

          meta = {
            description = "A masterless, encrypted clipboard-history mesh";
            homepage = "https://github.com/Fractal-Tess/clip-sync";
            license = pkgs.lib.licenses.mit;
            mainProgram = "clip-sync";
            platforms = pkgs.lib.platforms.linux;
          };
        };
    in
    {
      packages = forAllSystems (system: {
        default = packageFor system true;
        daemon = packageFor system false;
        with-ui = packageFor system true;
      });

      checks = forAllSystems (system: {
        package = packageFor system true;
        daemon = packageFor system false;
      });

      nixosModules.default = { pkgs, lib, ... }: {
        imports = [ ./nix/module.nix ];
        services.clip-sync.package = lib.mkDefault self.packages.${pkgs.system}.default;
      };

      devShells = forAllSystems (
        system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
        in
        {
          default = pkgs.mkShell {
            packages = with pkgs; [
              cargo
              cargo-audit
              cargo-deny
              clippy
              libGL
              libxkbcommon
              pkg-config
              protobuf
              rustc
              rustfmt
              wayland
            ];
            LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath [
              pkgs.libGL
              pkgs.libxkbcommon
              pkgs.wayland
            ];
          };
        }
      );

      formatter = forAllSystems (system: nixpkgs.legacyPackages.${system}.nixfmt);
    };
}
