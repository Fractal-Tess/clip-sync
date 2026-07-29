# Changelog

All notable changes will be documented here. The project is pre-alpha; storage, IPC, and network formats may change without backward compatibility until explicitly stabilized.

## Unreleased

### Added

- Public product plan, security policy, contribution guide, multi-size application/tray icon system, and branded mesh splash art.
- Deterministic operation model with HLC ordering and gap-aware frontiers.
- Validated TOML configuration, NetBird discovery, and Protobuf Unix IPC.
- SQLCipher encrypted operation log with restart recovery and private file permissions.
- Authenticated envelope keyslots with offline mesh-secret rotation and interrupted-rekey recovery.
- Native Wayland ext-data-control watcher with WLR fallback and bounded multi-MIME capture.
- TLS-exporter-bound QUIC shared-key authentication with NetBird-only listeners.
- Durable bounded anti-entropy, offline reconciliation, membership acknowledgements, forgotten-device rejection, and tombstone compaction.
- Fixed-size encrypted chunk storage, resumable/cancellable dedicated QUIC chunk streams, and non-origin relay forwarding.
- Automatic and explicit safe file/directory snapshots with private runtime materialization and cleanup.
- Replicated pins, deletes, quota eviction, quota exemptions, capture policy, and atomically mirrored Stow configuration.
- Full JSON CLI and IPC parity for status, peers, diagnostics, history, sharing, transfers, settings, and devices.
- Typed, quoted, bounded history search across CLI, IPC, and egui with deterministic newest-first results and an in-memory metadata index.
- Keyboard-first singleton egui switcher, functional control center, and persistent StatusNotifier tray with launch actions.
- Compact button-free switcher cards with four-line wrapped previews, bottom-aligned metadata footers, bounded lazy image previews, grid-aware keyboard navigation, `Enter` activation, `Ctrl+P` pinning, abbreviated comma-chainable filters, autocomplete, and remembered per-window size and position.
- Crash-safe authenticated keyslots and offline `rekey` with interrupted-operation recovery.
- Local security/audit checks, live Wayland validation, disposable two-node deployment testing, Nix UI/daemon packages, and a hardened NixOS user-service module.
