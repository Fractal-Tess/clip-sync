{
  lib,
  stdenvNoCC,
  fetchurl,
  autoPatchelfHook,
  makeWrapper,
  cairo,
  dbus,
  gdk-pixbuf,
  glib,
  gsettings-desktop-schemas,
  gtk3,
  libGL,
  libsoup_3,
  libxkbcommon,
  openssl,
  pango,
  wayland,
  webkitgtk_4_1,
  iproute2,
  version,
  artifact,
}:

stdenvNoCC.mkDerivation {
  pname = "clip-sync";
  inherit version;

  src = fetchurl {
    inherit (artifact) url hash;
  };

  nativeBuildInputs = [
    autoPatchelfHook
    makeWrapper
  ];

  buildInputs = [
    cairo
    dbus
    gdk-pixbuf
    glib
    gtk3
    libGL
    libsoup_3
    libxkbcommon
    openssl
    pango
    wayland
    webkitgtk_4_1
  ];

  installPhase = ''
    runHook preInstall
    install -Dm755 clip-sync "$out/bin/clip-sync"
    install -Dm644 README.md "$out/share/doc/clip-sync/README.md"
    install -Dm644 CHANGELOG.md "$out/share/doc/clip-sync/CHANGELOG.md"
    install -Dm644 LICENSE "$out/share/licenses/clip-sync/LICENSE"
    install -Dm644 ${./clip-sync.desktop} "$out/share/applications/clip-sync.desktop"
    install -Dm644 ${../desktop/src-tauri/icons/128x128.png} \
      "$out/share/icons/hicolor/128x128/apps/clip-sync.png"
    runHook postInstall
  '';

  preFixup = ''
    wrapProgram "$out/bin/clip-sync" \
      --set GDK_BACKEND x11 \
      --set WEBKIT_DISABLE_COMPOSITING_MODE 1 \
      --set WEBKIT_DISABLE_DMABUF_RENDERER 1 \
      --prefix PATH : ${lib.makeBinPath [ iproute2 ]} \
      --prefix LD_LIBRARY_PATH : ${
        lib.makeLibraryPath [
          libGL
          libxkbcommon
          wayland
          webkitgtk_4_1
        ]
      }
  '';

  meta = {
    description = "A masterless, encrypted clipboard-history mesh";
    homepage = "https://github.com/Fractal-Tess/clip-sync";
    license = lib.licenses.mit;
    mainProgram = "clip-sync";
    platforms = [ "x86_64-linux" ];
    sourceProvenance = [ lib.sourceTypes.binaryNativeCode ];
  };
}
