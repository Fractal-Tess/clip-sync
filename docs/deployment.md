# Deployment

## NixOS module

The flake exports `nixosModules.default` and two packages:

- `packages.<system>.default` / `with-ui`: daemon, CLI, egui UI, and StatusNotifier tray;
- `packages.<system>.daemon`: daemon and CLI without graphics dependencies.

The UI package generates its Hyprland global-shortcut bindings from the vendored BSD-3-Clause protocol XML with the optional Rust `wayland-scanner` dependency. The daemon-only feature graph does not include that generator or egui/windowing crates.

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

By default, each user service reads `%h/.config/clip-sync/config.toml`. The file may be a writable Stow symlink. The daemon and tray start with `graphical-session.target`, restart on failure, use a `0077` umask, and do not require NetBird to provide local history. The tray preserves its History Switcher and Control Center routes, but both target one native `clip-sync-switcher` process/window: left click opens Quick History and Control Center focuses management presentation. Set `services.clip-sync.tray.enable = false` when selecting the daemon-only package or when no StatusNotifier host is available.

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

Recommended rules for Hyprland configurations using hyprlang syntax (through 0.54):

```ini
windowrule = match:class ^(clip-sync-switcher)$, float on, center on, size 720 480
bind = $mainMod, H, exec, clip-sync ui switcher
bindn = , Escape, global, clip-sync:close-quick
```

The `n` flag is required: it is Hyprland's **non-consuming** bind flag, so Escape is still delivered to the focused application when ClipSync is absent or is showing management presentation. The UI registers app ID `clip-sync` and shortcut ID `close-quick` with the native `hyprland_global_shortcuts_v1` client protocol. The compositor supplies only anonymous pressed/released events: ClipSync forwards only pressed events into its existing signal channel, Quick closes, and management ignores the signal. Focused egui Escape remains unchanged. There is no spawned process per keypress, `evdev`/`libinput` reader, or global keylogger. Missing Wayland or protocol support is a non-fatal no-op.

For a Home Manager/Nix Hyprland settings block, use the same compositor dispatcher rather than an `exec` binding:

```nix
wayland.windowManager.hyprland.settings = {
  bind = [ "$mainMod, H, exec, clip-sync ui switcher" ];
  bindn = [ ", Escape, global, clip-sync:close-quick" ];
};
```

`clip-sync ui close-quick` remains available as a compatibility/debug command. It never starts the UI, validates the owner-only runtime path, and sends one bounded same-user Unix-socket message, but declarative bindings should use `global, clip-sync:close-quick`.

For Hyprland 0.55+ Lua configuration, the equivalent non-consuming binding is:

```lua
hl.window_rule({
  match = { class = "^(clip-sync-switcher)$" },
  float = true,
  center = true,
  size = { 720, 480 },
})
hl.bind("SUPER + H", hl.dsp.exec_cmd("clip-sync ui switcher"))
hl.bind("Escape", hl.dsp.global("clip-sync:close-quick"), { non_consuming = true })
```

The unified shell has one geometry file at `$XDG_STATE_HOME/clip-sync/window.json`, defaults to 720×480, and enforces a 480×300 minimum. On first run it migrates a valid `switcher-window.json` before considering `control-window.json`, then removes the legacy geometry files. Wayland does not expose client positioning, so saved placement is still restored through `hyprctl` when available.

The local UI singleton signal changed when the former switcher and Control Center were unified. Treat this UI-only release as a coordinated cutover: before switching the package, close any mapped `clip-sync-switcher` and `clip-sync-control` windows, then let the declarative activation restart `clip-sync-tray.service`. Do not leave an old standalone UI process running across the switch. Daemon IPC remains version 5, so daemon/mesh interoperability is unchanged.

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
