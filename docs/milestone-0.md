# Milestone 0 findings

Milestone 0 validates the riskiest platform and security assumptions before clip-sync commits to production storage, transport, clipboard, or UI APIs.

## NetBird discovery — validated

The daemon parses the stable subset of `netbird status --json`, validates local and peer addresses, and degrades without stopping local service when discovery fails.

Live result on the initial Hyprland host:

- local NetBird address discovered successfully;
- 13 peers parsed;
- discovery exposed over redacted local IPC status.

The parser remains isolated behind `PeerDiscovery` and fixture tests because the CLI JSON schema is external and may change.

## Wayland data-control — native implementation landed, live mutation test remains

The native probe connects directly through `wayland-client`, performs a registry round trip, and selects:

1. `ext-data-control-v1` when available;
2. deprecated `wlr-data-control-v1` as fallback.

The live Hyprland session advertises `ext-data-control-v1` and a seat. The pure clipboard model enforces MIME-name/count bounds, the 20 MiB aggregate threshold, offer generations, and a strict distinction between regular clipboard and primary selection.

The watcher now binds `ext-data-control-v1` first with WLR v1 fallback, receives regular-clipboard offers, drains MIME pipes off the Wayland callback path with a shared aggregate budget, invalidates stale generations, serves daemon-owned multi-MIME content lazily, and uses a private marker MIME to suppress its compositor echo.

Pure and integration tests cover capture bounds, lazy representation lookup, ownership commands, and one-shot feedback handling. A real Wayland ownership test exists but remains ignored/manual because it intentionally replaces the user's active clipboard. WLR fallback has not yet been exercised on a compositor that lacks the ext protocol.

## QUIC shared-key authentication — validated on loopback

The transport spike provides a fixed-size versioned handshake with:

- fresh client and server nonces;
- canonical client/server transcript ordering;
- TLS-exporter binding;
- HKDF-SHA-256 key separation;
- role-separated HMAC-SHA-256 proofs;
- constant-time verification;
- five-second timeout and generic peer-visible failure reason;
- an `AuthenticatedConnection` gate before application metadata.

Real Quinn loopback tests prove matching keys authenticate and mismatched keys fail. Certificate-chain identity is intentionally not used; the custom test verifier still verifies TLS handshake signatures. Production endpoint limits, duplicate-connection resolution, protocol negotiation, and NetBird binding remain Milestone 3 work.

## SQLCipher encrypted storage — validated

The storage spike uses the bundled SQLCipher source and verifies at open time:

- a nonempty SQLCipher version;
- FTS5 support;
- in-memory temporary storage;
- foreign-key enforcement;
- schema compatibility.

Database files are created/restricted to mode `0600`. Tests write a unique plaintext marker, checkpoint and close the database, scan the database/WAL/shared-memory files for that marker, reopen with the correct key, and reject an incorrect key without modifying persistent files.

This is not yet the production history schema. Envelope keyslots, the dedicated storage actor, migrations, operation persistence, encrypted chunk manifests, and transactional rekeying remain open.

## Optional egui window — validated

The `ui` Cargo feature builds an eframe/egui Glow application without enabling WGPU. Both planned modes exist:

```console
clip-sync ui switcher
clip-sync ui control
```

A live Wayland smoke test opened the switcher with app ID `clip-sync-switcher` and title `clip-sync switcher`. The UI is a deliberate shell: it demonstrates sizing, dark styling, tabs, search focus, and Escape handling, but does not claim history functionality before Milestone 2.

The Nix development shell supplies Wayland, libxkbcommon, and OpenGL dynamic libraries. Final package wrapping and Hyprland floating rules remain deployment work.

## Remaining exit criterion

Milestone 0 stays open until the ignored live Wayland test is run in a disposable compositor/session, proving that native ownership remains available after the original source exits and that compositor echoes cannot create capture loops. The implementation and bounded model tests are already in place; the remaining gap is non-destructive live validation.
