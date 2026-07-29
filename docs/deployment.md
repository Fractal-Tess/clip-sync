# Deployment

## NixOS module

The flake exports `nixosModules.default` and two packages:

- `packages.<system>.default` / `with-ui`: daemon, CLI, egui UI, and StatusNotifier tray;
- `packages.<system>.daemon`: daemon and CLI without graphics dependencies.

Add the flake input and module, then enable the user service:

```nix
{
  inputs.clip-sync.url = "github:Fractal-Tess/clip-sync";
  inputs.clip-sync.inputs.nixpkgs.follows = "nixpkgs";

  # In the host module list:
  imports = [ inputs.clip-sync.nixosModules.default ];

  services.clip-sync.enable = true;
  # services.clip-sync.tray.enable = false; # for daemon-only or tray-less desktops
}
```

By default, each user service reads `%h/.config/clip-sync/config.toml`. The file may be a writable Stow symlink. The daemon and tray start with `graphical-session.target`, restart on failure, use a `0077` umask, and do not require NetBird to provide local history. The tray opens the switcher on left click and can launch either UI; set `services.clip-sync.tray.enable = false` when selecting the daemon-only package or when no StatusNotifier host is available.

The graphical session must import `WAYLAND_DISPLAY` into the systemd user manager. UWSM normally does this. Verify with:

```console
systemctl --user show-environment | grep WAYLAND_DISPLAY
```

## Secret provisioning

Provision the same high-entropy 32-byte raw or 64-character hexadecimal secret on every mesh member. The target file must be owned by the desktop user with no group/other permissions (`0400` or `0600`). Stable sops-nix symlinks are supported after descriptor-level target validation:

```nix
sops.secrets.clip_sync_mesh_key = {
  sopsFile = ./secrets.json;
  format = "json";
  owner = "your-user";
  mode = "0400";
};
```

Reference its runtime path from the local config:

```toml
[shared]
mesh_quota_bytes = 1073741824
capture_threshold_bytes = 20971520
revision = ""

[local]
mesh_key_file = "/run/secrets/clip_sync_mesh_key"
listen_port = 24892
discovery_interval_seconds = 15
reconcile_interval_seconds = 5
reconnect_min_seconds = 1
reconnect_max_seconds = 60
netbird_command = "netbird"
maximum_explicit_share_bytes = 4294967296
transfer_free_space_reserve_bytes = 67108864
materialization_free_space_reserve_bytes = 8388608
max_concurrent_chunk_streams = 4
```

## Mesh-secret rotation

Rotate every retained node before deploying the replacement secret as its
configured `mesh_key_file`:

```console
systemctl --user stop clip-sync
clip-sync rekey \
  --old-key-file /run/secrets/clip_sync_mesh_key_old \
  --new-key-file /run/secrets/clip_sync_mesh_key_new
```

The command and daemon take the same non-waiting exclusive state lock, so rekey
fails if the daemon is still running. The owner-only
`$XDG_STATE_HOME/clip-sync/history.keyslot` authenticates and wraps stable local
SQLCipher and chunk-store keys plus the existing mesh content-identity key.
Normal rotations replace only this small mode-`0600` sidecar; database pages,
content IDs, and chunk payloads are reopened and verified but not rewritten.

An existing database that predates the keyslot is migrated automatically under
the old secret. Its SQLCipher key is changed transactionally to a random local
data key. An existing chunk store keeps its former derived root during this
one-time migration so keyed identifiers and payload ciphertext remain stable.

If the command is interrupted, rerun the same command. A durable
`history.keyslot.next` is either resumed before the atomic commit or the
already-committed new keyslot is recognized idempotently. Do not delete or edit
either sidecar during recovery. After every node reports a verified rotation,
deploy the new configured secret and restart the daemons.

Never use the production secret for smoke tests.

## Hyprland

Recommended rules:

```ini
windowrule = match:class ^(clip-sync-switcher)$, float on, center on, size 720 420
windowrule = match:class ^(clip-sync-control)$, float on, center on, size 1040 700
bind = $mainMod, H, exec, clip-sync ui switcher
```

Remove any `wl-paste --watch cliphist store` autostart only after `scripts/test-live-wayland` and the two-node smoke test pass. Keep `cliphist` installed during the initial soak so rollback does not depend on a network fetch.

To roll back the current `vd`/`kiwi` deployment, stop both clip-sync user services, restore the pre-cutover picker and autostart from the NixOS repository, apply the dotfiles, and start the watcher for the current session:

```console
cd ~/nixos
systemctl --user stop clip-sync clip-sync-tray
git restore --source=e8ebc69^ -- \
  dotfiles/desktop/.config/hypr/hyprconfigs/hyprautostart.conf \
  scripts/session/clipboard
~/nixos/scripts/system/dotfiles apply
wl-paste --watch cliphist store &
```

The restored `SUPER+H` binding still invokes `~/nixos/scripts/session/clipboard`, which is then the cliphist/Rofi picker. Reapply the clip-sync commit and restart its services to roll forward.

## Validation

```console
./scripts/check --nix --security
WAYLAND_DISPLAY=wayland-1 ./scripts/test-live-wayland
WAYLAND_DISPLAY=wayland-1 ./scripts/deploy-smoke kiwi
systemctl --user status clip-sync
clip-sync doctor --json
clip-sync status --json
clip-sync peers --json
```

The live Wayland test uses isolated temporary state, restores the previous text clipboard when possible, verifies source-exit ownership, checks database mode, and scans the encrypted database for its plaintext marker.
