{
  lib,
  stdenvNoCC,
  fetchurl,
  autoPatchelfHook,
  makeWrapper,
  libxkbcommon,
  wayland,
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

  installPhase = ''
    runHook preInstall
    install -Dm755 clip-sync "$out/bin/clip-sync"
    install -Dm644 README.md "$out/share/doc/clip-sync/README.md"
    install -Dm644 CHANGELOG.md "$out/share/doc/clip-sync/CHANGELOG.md"
    install -Dm644 LICENSE "$out/share/licenses/clip-sync/LICENSE"
    install -Dm644 ${./clip-sync.desktop} "$out/share/applications/clip-sync.desktop"
    install -Dm644 ${../assets/clip-sync.png} \
      "$out/share/icons/hicolor/128x128/apps/clip-sync.png"
    runHook postInstall
  '';

  # The window links nothing but libc: winit and softbuffer dlopen Wayland and
  # xkbcommon, so they have to be on the runtime search path rather than in
  # buildInputs.
  preFixup = ''
    wrapProgram "$out/bin/clip-sync" \
      --prefix PATH : ${lib.makeBinPath [ iproute2 ]} \
      --prefix LD_LIBRARY_PATH : ${
        lib.makeLibraryPath [
          libxkbcommon
          wayland
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
