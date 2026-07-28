# Changelog

All notable changes will be documented here. The project is pre-alpha; storage, IPC, and network formats may change without backward compatibility until explicitly stabilized.

## Unreleased

### Added

- Public product plan, security policy, contribution guide, logo, and splash art.
- Deterministic operation model with HLC ordering and gap-aware frontiers.
- Validated TOML configuration, NetBird discovery, and Protobuf Unix IPC.
- SQLCipher encrypted operation log with restart recovery and private file permissions.
- Authenticated envelope keyslots with offline mesh-secret rotation and interrupted-rekey recovery.
- Native Wayland ext-data-control watcher with WLR fallback and bounded multi-MIME capture.
- TLS-exporter-bound QUIC shared-key authentication.
- Transport-independent bounded anti-entropy and store-and-forward tests.
- CLI daemon status, diagnostics, history listing, and activation commands.
- Optional egui switcher and control-center shell.
- Nix development shell and package.
