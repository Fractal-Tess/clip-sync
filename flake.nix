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
      workspace = builtins.fromTOML (builtins.readFile ./Cargo.toml);
      version = workspace.workspace.package.version;
      releaseArtifacts = builtins.fromJSON (builtins.readFile ./nix/release-artifacts.json);
      sourcePackageFor =
        system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
          rustSource = pkgs.lib.fileset.toSource {
            root = ./.;
            fileset = pkgs.lib.fileset.unions [
              ./Cargo.lock
              ./Cargo.toml
              ./deny.toml
              ./assets
              ./crates
              ./nix
            ];
          };
        in
        pkgs.rustPlatform.buildRustPackage {
          pname = "clip-sync";
          inherit version;
          src = rustSource;
          cargoLock.lockFile = ./Cargo.lock;
          RUST_MIN_STACK = "16777216";

          nativeBuildInputs = with pkgs; [
            makeWrapper
            perl
            removeReferencesTo
          ];

          cargoBuildFlags = [
            "-p"
            "clip-sync-desktop"
            "--bin"
            "clip-sync"
            "--locked"
          ];
          doCheck = false;

          postInstall = ''
            find "$out/bin" -maxdepth 1 -type f ! -name clip-sync -delete
            install -Dm644 ${./nix/clip-sync.desktop} "$out/share/applications/clip-sync.desktop"
            install -Dm644 ${./assets/clip-sync.png} \
              "$out/share/icons/hicolor/128x128/apps/clip-sync.png"

            # SQLCipher's vendored OpenSSL bakes its build compiler's path into
            # a banner string, which otherwise drags the whole GCC toolchain
            # into the runtime closure.
            remove-references-to -t ${pkgs.stdenv.cc} "$out/bin/clip-sync"
          '';

          # winit and softbuffer dlopen Wayland and xkbcommon, so they have to be
          # on the runtime search path rather than in buildInputs.
          postFixup = ''
            wrapProgram "$out/bin/clip-sync" \
              --prefix PATH : ${pkgs.lib.makeBinPath [ pkgs.iproute2 ]} \
              --prefix LD_LIBRARY_PATH : ${
                pkgs.lib.makeLibraryPath [
                  pkgs.libxkbcommon
                  pkgs.wayland
                ]
              }
          '';

          meta = {
            description = "A masterless, encrypted clipboard-history mesh";
            homepage = "https://github.com/Fractal-Tess/clip-sync";
            license = pkgs.lib.licenses.mit;
            mainProgram = "clip-sync";
            platforms = pkgs.lib.platforms.linux;
          };
        };
      packageFor =
        system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
          artifact =
            if releaseArtifacts.version == version then releaseArtifacts.systems.${system} or null else null;
        in
        if artifact == null then
          sourcePackageFor system
        else
          pkgs.callPackage ./nix/prebuilt-package.nix {
            inherit artifact version;
          };
    in
    {
      packages = forAllSystems (system: {
        default = packageFor system;
        source = sourcePackageFor system;
      });

      checks = forAllSystems (system: {
        package = self.packages.${system}.default;
        source = self.packages.${system}.source;
      });

      nixosModules.default =
        { pkgs, lib, ... }:
        {
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
              iproute2
              libxkbcommon
              perl
              rustc
              rustfmt
              wayland
            ];
            LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath [
              pkgs.libxkbcommon
              pkgs.wayland
            ];
          };
        }
      );

      formatter = forAllSystems (system: nixpkgs.legacyPackages.${system}.nixfmt);
    };
}
