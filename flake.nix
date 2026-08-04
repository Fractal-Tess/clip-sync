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
              ./desktop
              ./nix
            ];
          };
          frontendSource = pkgs.lib.fileset.toSource {
            root = ./.;
            fileset = pkgs.lib.fileset.difference ./desktop ./desktop/src-tauri;
          };
          desktopFrontend = pkgs.callPackage ./nix/desktop-frontend.nix {
            src = frontendSource;
          };
        in
        pkgs.rustPlatform.buildRustPackage {
          pname = "clip-sync";
          inherit version;
          src = rustSource;
          cargoLock.lockFile = ./Cargo.lock;
          RUST_MIN_STACK = "16777216";

          nativeBuildInputs = with pkgs; [
            perl
            pkg-config
            wrapGAppsHook3
          ];

          buildInputs = with pkgs; [
            cairo
            dbus
            gdk-pixbuf
            glib
            gsettings-desktop-schemas
            gtk3
            libGL
            libsoup_3
            libxkbcommon
            openssl
            pango
            wayland
            webkitgtk_4_1
          ];

          preBuild = ''
            rm -rf desktop/build
            mkdir -p desktop/build
            cp -R ${desktopFrontend}/. desktop/build/
          '';

          cargoBuildFlags = [
            "-p"
            "clip-sync"
            "--bin"
            "clip-sync"
            "--locked"
          ];
          doCheck = false;

          postInstall = ''
            find "$out/bin" -maxdepth 1 -type f ! -name clip-sync -delete
            install -Dm644 ${./nix/clip-sync.desktop} "$out/share/applications/clip-sync.desktop"
            install -Dm644 ${./desktop/src-tauri/icons/128x128.png} \
              "$out/share/icons/hicolor/128x128/apps/clip-sync.png"
          '';

          preFixup = ''
            gappsWrapperArgs+=(
              --set GDK_BACKEND x11
              --set WEBKIT_DISABLE_COMPOSITING_MODE 1
              --set WEBKIT_DISABLE_DMABUF_RENDERER 1
              --prefix PATH : ${pkgs.lib.makeBinPath [ pkgs.iproute2 ]}
              --prefix LD_LIBRARY_PATH : ${
                pkgs.lib.makeLibraryPath [
                  pkgs.libGL
                  pkgs.libxkbcommon
                  pkgs.wayland
                  pkgs.webkitgtk_4_1
                ]
              }
            )
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
              bun
              cargo
              cargo-audit
              cargo-deny
              clippy
              dbus
              gsettings-desktop-schemas
              gtk3
              iproute2
              libGL
              librsvg
              libsoup_3
              libxkbcommon
              openssl
              pkg-config
              rustc
              rustfmt
              wayland
              webkitgtk_4_1
            ];
            LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath [
              pkgs.dbus
              pkgs.gtk3
              pkgs.libGL
              pkgs.librsvg
              pkgs.libsoup_3
              pkgs.libxkbcommon
              pkgs.openssl
              pkgs.wayland
              pkgs.webkitgtk_4_1
            ];
            GDK_BACKEND = "x11";
            WEBKIT_DISABLE_COMPOSITING_MODE = "1";
            WEBKIT_DISABLE_DMABUF_RENDERER = "1";
            shellHook = ''
              export XDG_DATA_DIRS=${pkgs.gsettings-desktop-schemas}/share/gsettings-schemas/${pkgs.gsettings-desktop-schemas.name}:${pkgs.gtk3}/share/gsettings-schemas/${pkgs.gtk3.name}:''${XDG_DATA_DIRS:-}
            '';
          };
        }
      );

      formatter = forAllSystems (system: nixpkgs.legacyPackages.${system}.nixfmt);
    };
}
