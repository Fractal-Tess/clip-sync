# Deployment

## NixOS module

The flake exports `nixosModules.default` and two packages:

- `packages.<system>.default` / `with-ui`: daemon, CLI, and egui UI;
- `packages.<system>.daemon`: daemon and CLI without graphics dependencies.

Add the flake input and module, then enable the user service:

```nix
{
  inputs.clip-sync.url = "github:Fractal-Tess/clip-sync";
  inputs.clip-sync.inputs.nixpkgs.follows = "nixpkgs";

  # In the host module list:
  imports = [ inputs.clip-sync.nixosModules.default ];

  services.clip-sync.enable = true;
}
```

By default, each user service reads `%h/.config/clip-sync/config.toml`. The file may be a writable Stow symlink. The service starts with `graphical-session.target`, restarts on failure, uses a `0077` umask, and does not require NetBird to provide local history.

The graphical session must import `WAYLAND_DISPLAY` into the systemd user manager. UWSM normally does this. Verify with:

```console
systemctl --user show-environment | grep WAYLAND_DISPLAY
```

## Secret provisioning

Provision the same high-entropy 32-byte raw or 64-character hexadecimal secret on every mesh member. The file must be owned by the desktop user and mode `0600`. For sops-nix:

```nix
sops.secrets.clip_sync_mesh_key = {
  sopsFile = ./secrets.json;
  format = "json";
  owner = "your-user";
  mode = "0600";
};
```

Reference its runtime path from the local config:

```toml
[shared]
mesh_quota_bytes = 1073741824
capture_threshold_bytes = 20971520

[local]
mesh_key_file = "/run/secrets/clip_sync_mesh_key"
listen_port = 24892
discovery_interval_seconds = 15
reconcile_interval_seconds = 5
reconnect_min_seconds = 1
reconnect_max_seconds = 60
netbird_command = "netbird"
```

Restart every daemon after rotating the shared secret. Never use the production secret for smoke tests.

## Hyprland

Recommended rules:

```ini
windowrule = match:class ^(clip-sync-switcher)$, float on, center on, size 720 420
windowrule = match:class ^(clip-sync-control)$, float on, center on, size 1040 700
bind = $mainMod, H, exec, clip-sync ui switcher
```

Remove any `wl-paste --watch cliphist store` autostart only after `scripts/test-live-wayland` and the two-node smoke test pass. To roll back, stop `clip-sync.service`, restore that autostart command, and point the hotkey back to the previous picker.

## Validation

```console
./scripts/check --nix
WAYLAND_DISPLAY=wayland-1 ./scripts/test-live-wayland
systemctl --user status clip-sync
clip-sync doctor --json
clip-sync status --json
clip-sync peers --json
```

The live Wayland test uses isolated temporary state, restores the previous text clipboard when possible, verifies source-exit ownership, checks database mode, and scans the encrypted database for its plaintext marker.
