# Changelog

All notable changes to ClipSync are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.4.0] - 2026-09-27

Every host must run 0.4 to sync with the others: the stored and network
operation formats changed. A host's database is converted on first start and
cannot be opened by 0.3 afterwards.

### Added

- Large copies stay on the host that copied them. Copied files are never read or duplicated at copy time; other hosts see what they are and fetch them directly from the original files when you paste. Non-file copies above `inline_limit_bytes` (5 MiB) stay in the copying host's store the same way. Nothing large fans out to every host.
- A file edited or moved after copying is refused rather than sent half old and half new.
- `[local] clipboard = false` runs a headless host as a pure history relay, with no Wayland connection attempts.

- History cards show the entry's size and how long ago it was copied.
- `Ctrl+A` selects the whole search query, so the next keystroke replaces it and `Backspace` clears it.
- Added `packages.<system>.clip-sync`, `apps.<system>.default`, and a Home Manager module for the per-user `services.clip-sync` service.
  The NixOS and Home Manager service modules share their option definitions.
  `environment` replaces `extraEnvironment`, and `autoStart = false` retains a
  manually startable unit without adding session-target startup edges.
- Clicking a history card activates it and closes the picker.
- Pinned entries move into a dedicated side column with an animated accent transition.

### Changed

- Operations are stored and exchanged as Protobuf instead of JSON. JSON encoded bytes as lists of numbers, and decoding a 2.6 MB item briefly needed about 136 MB; decoding now costs about the size of the item.
- Clipboard bytes are no longer held in memory; they are read from storage when an item is pasted or previewed. On a real 57 MB history the daemon went from 118 MB to 20 MB resident and starts in about half the time.
- Peers are exactly `peer_addresses`, dialled from the configured interfaces. Multicast beacons and the unicast subnet probing that replaced them on VPN interfaces are gone.
- The database key is a random per-host `history.key` file, migrated once from the old keyslot. Changing the mesh secret no longer touches local storage.
- History search is plain words, all of which must appear in an item's preview, type, or device.
- The history quota, inline limit, and fetch cache size are per-host `[local]` settings; nothing replicates settings any more.
- The picker and control centre are drawn 18% larger, and the window grew with them so the grid keeps its column count.
- Bottom-edge keyboard instructions use larger, higher-contrast text.
- History cards use a pointing cursor to make their click behavior visible.
- `Escape` now closes the window from both the picker and control centre.

### Removed

- Explicit sharing (`share-clipboard`), transfers and their tab, the encrypted chunk store, and quota-exempt shares. Large items are handled by fetching instead.
- Replicated shared settings, the config file watcher, and the `config` command.
- The `d:`, `t:`, `p:`, `before:`, and size filters in history search.
- `clip-sync rekey` and the keyslot it maintained.
- The Transfers and Settings tabs of the control centre.

### Fixed

- A copy made while a sync was half-way through could fail and be lost; the quota check no longer errors on an item whose add has not arrived yet.
- Large automatic clipboard captures now fit the authenticated reconciliation frame instead of permanently blocking all later operations.
- Stable `peer_addresses` keep VPN peers connected when the selected interface does not route multicast discovery.

## [0.3.0] - 2026-09-13

### Changed

- Replaced the Tauri/SvelteKit desktop window with a native egui window rendered on a CPU rasterizer. The picker reaches first pixel in roughly 30 ms instead of waiting on a browser engine, and the package no longer depends on WebKitGTK, GTK3, libsoup, or a Node toolchain.
- The control centre is now a second view inside the same window, reached with `F1` and left with `Escape`. `clip-sync desktop --control` opens it directly.

### Removed

- The prewarmed desktop process, its systemd user unit, and the `services.clip-sync.prewarmDesktop` option. Launch is fast enough that keeping a process resident no longer pays for itself.
- `clip-sync desktop --background`, which existed only to feed the prewarm unit.

## [0.2.4] - 2026-08-05

### Fixed

- Refresh retained history whenever the persistent desktop window becomes visible or regains focus, so newly copied items appear immediately after opening ClipSync.

## [0.2.3] - 2026-08-04

### Changed

- Replaced the original clipboard mark with a faceted low-poly identity across the README, desktop UI, application bundles, and Linux launcher.

## [0.2.2] - 2026-08-04

### Changed

- History items are deleted immediately from their context-menu action without a confirmation dialog.

### Fixed

- Floating desktop-window position and size are persisted when the window hides and across restarts.

## [0.2.1] - 2026-08-04

### Added

- A hidden, prewarmed desktop process managed by the graphical-session systemd user target.
- Single-instance desktop activation so launcher requests reveal the existing window immediately.
- A desktop launcher entry and application icon for Linux application menus.

### Changed

- Closing the desktop window now hides it while keeping the initialized webview available.

### Fixed

- Interface discovery now has the netlink access required by the hardened NixOS user service.
- Linux launchers and Hyprland rules now use the desktop window's actual application class.

## [0.2.0] - 2026-08-03

### Added

- A unified `clip-sync` executable that runs the desktop, daemon, and CLI modes.
- Interface-scoped, mesh-secret-authenticated peer discovery using UDP multicast.
- Bounded authenticated unicast discovery fallback for point-to-point tunnel interfaces.
- Multi-interface QUIC listeners and configurable Linux peer interfaces.
- Desktop controls and CLI commands for updating peer interfaces without restarting the daemon.
- GitHub Actions validation and tagged release automation for x86_64 Linux binaries.
- A Nix prebuilt-release package path driven by a checksummed release manifest.

### Changed

- The Peers view now shows only live, authenticated mesh connections.
- Status, diagnostics, and desktop IPC bindings now report selected local interface addresses.
- The desktop management workspace and settings presentation were refined.
- IPC protocol version increased to 6.

### Removed

- NetBird discovery, configuration, runtime dependencies, diagnostics, and documentation.

### Security

- Discovery beacons are authenticated with a key derived independently from the mesh secret.
- Hostname and application metadata remain unavailable until the QUIC mesh handshake succeeds.

[Unreleased]: https://github.com/Fractal-Tess/clip-sync/compare/v0.3.0...HEAD
[0.3.0]: https://github.com/Fractal-Tess/clip-sync/compare/v0.2.4...v0.3.0
[0.2.4]: https://github.com/Fractal-Tess/clip-sync/compare/v0.2.3...v0.2.4
[0.2.3]: https://github.com/Fractal-Tess/clip-sync/compare/v0.2.2...v0.2.3
[0.2.2]: https://github.com/Fractal-Tess/clip-sync/compare/v0.2.1...v0.2.2
[0.2.1]: https://github.com/Fractal-Tess/clip-sync/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/Fractal-Tess/clip-sync/releases/tag/v0.2.0
