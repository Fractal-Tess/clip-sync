{
  bun,
  cacert,
  lib,
  nodejs,
  src,
  stdenvNoCC,
}:

stdenvNoCC.mkDerivation {
  pname = "clip-sync-desktop-frontend";
  version = "0.1.0";
  inherit src;

  nativeBuildInputs = [
    bun
    nodejs
  ];

  dontConfigure = true;

  buildPhase = ''
    runHook preBuild
    export HOME="$TMPDIR/home"
    export SSL_CERT_FILE="${cacert}/etc/ssl/certs/ca-bundle.crt"
    mkdir -p "$HOME"
    cp -R "$src/desktop" ./desktop
    chmod -R u+w ./desktop
    cd desktop
    bun install --frozen-lockfile
    patchShebangs node_modules
    bun run build
    runHook postBuild
  '';

  installPhase = ''
    runHook preInstall
    mkdir -p "$out"
    cp -R build/. "$out/"
    runHook postInstall
  '';

  outputHashMode = "recursive";
  outputHashAlgo = "sha256";
  outputHash = "sha256-TAYdbxhMGGWfWlbQJxKjxTJTe6cuJ4uDmg/czSKOKJo=";
}
