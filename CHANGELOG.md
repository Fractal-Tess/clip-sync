# Changelog

All notable changes to ClipSync are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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

[Unreleased]: https://github.com/Fractal-Tess/clip-sync/compare/v0.2.3...HEAD
[0.2.3]: https://github.com/Fractal-Tess/clip-sync/compare/v0.2.2...v0.2.3
[0.2.2]: https://github.com/Fractal-Tess/clip-sync/compare/v0.2.1...v0.2.2
[0.2.1]: https://github.com/Fractal-Tess/clip-sync/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/Fractal-Tess/clip-sync/releases/tag/v0.2.0
