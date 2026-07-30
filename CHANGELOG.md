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
- One keyboard-first native egui process/singleton for Quick History and management, preserving the `ui switcher`, `ui control`, and StatusNotifier tray routes under the canonical `clip-sync-switcher` app ID.
- Compact History cards with four-line wrapped previews, invariant absolute-bottom MIME/size/PIN/source/time metadata regions, accessible labels/tooltips, bounded lazy image previews, 2–3-column grid navigation, `Enter` activation, `Ctrl+P` pinning, abbreviated comma-chainable filters, autocomplete, and one shared owner-only geometry file with switcher-first migration.
- Bounded protocol-neutral open-History live refresh with focused/unfocused cadence, one in-flight plus one coalesced request, stale-card failure handling, bounded backoff, and content-ID selection preservation.
- Bounded-concurrent UI IPC that preserves read-only History/status responsiveness while mutually gating Share and other mutation dispatch, with disabled-state explanations and generation-safe Share inspection/confirmation.
- Native `hyprland_global_shortcuts_v1` Quick-close registration generated from a vendored BSD-licensed protocol XML: app ID `clip-sync`, shortcut ID `close-quick`, anonymous pressed events only, clean shutdown, graceful fallback, and no input-device keylogger or process spawned per keypress. The same-user `ui close-quick` signal remains for compatibility/debugging.
- Image-focused activation that preserves complete encrypted MIME bundles in history while advertising only image representations to paste targets.
- Crash-safe authenticated keyslots and offline `rekey` with interrupted-operation recovery.
- Local security/audit checks, live Wayland validation, disposable two-node deployment testing, Nix UI/daemon packages, and a hardened NixOS user-service module.

### Changed

- Split oversized UI, daemon, IPC, storage, mesh, clipboard, payload, CLI, model, envelope, transfer, and integration-test implementations into cohesive submodules, keeping files near 500 lines where the boundaries improve maintainability; production UI modules now use explicit imports and re-exports instead of a broad private glob prelude.
