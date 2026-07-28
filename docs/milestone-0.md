# Milestone 0 findings

Milestone 0 is complete. The risk spikes were converted into production paths and validated locally before daily-driver deployment.

## NetBird discovery — validated

The daemon parses the bounded stable subset of `netbird status --json`, validates local and peer addresses, limits the peer set, binds QUIC only to the local NetBird address, and tears down stale listeners when NetBird disappears or changes address. Local clipboard history remains available while discovery is degraded.

The initial live host discovered address `100.91.0.2` and 13 peers. Parsing remains isolated behind `PeerDiscovery` and fixtures because the NetBird JSON schema is external.

## Wayland data-control — validated live

The native backend selects `ext-data-control-v1` first and falls back to `wlr-data-control-v1`. It monitors only the regular clipboard, bounds MIME metadata and aggregate bytes, drains pipes outside compositor callbacks, invalidates stale generations, reconnects after compositor disruption, and lazily serves daemon-owned multi-MIME content.

Live Hyprland validation passed against `wayland-1`:

- `ext-data-control-v1` and `wl_seat` were detected;
- duplicate MIME advertisements were normalized;
- exact text was captured before the original `wl-copy` source was terminated;
- activation served the retained bytes after source exit;
- ownership did not deadlock daemon IPC;
- the private owner marker produced at most one intentional touch;
- the encrypted database remained mode `0600` and did not contain the plaintext marker.

The repeatable test is `scripts/test-live-wayland`. WLR fallback is covered structurally but still needs a compositor that lacks the ext protocol for live coverage.

## QUIC shared-key authentication — validated

Quinn sessions perform version preflight and TLS-exporter-bound, nonce-based, role-separated HMAC confirmation before replication metadata. Handshakes, messages, streams, peer counts, clocks, and allocations are bounded. Mismatched keys, malformed membership, replayed/conflicting operations, and incompatible protocol versions fail closed.

Loopback tests cover matching and mismatched keys, two-way reconciliation, three-node store-and-forward, offline restart, forgotten identities, and dedicated authenticated chunk streams. The disposable `scripts/deploy-smoke` test covers the real NetBird path between `vd` and `kiwi`.

## Encrypted storage and rekeying — validated

SQLCipher stores metadata and operations; fixed-size XChaCha20-Poly1305 chunks store large payloads and file snapshots. Mode, ownership, symlink, wrong-key, plaintext-absence, crash-window, migration, and restart tests pass.

An authenticated `history.keyslot` wraps random database, chunk-store, and content-identity keys. `clip-sync rekey` uses an exclusive store lock, durable candidate keyslot, atomic replacement, reopen verification, and idempotent interrupted-rekey recovery. Rotation normally changes only the wrapped keyslot and mesh transport key.

## Optional egui UI — validated live

The `ui` feature provides a keyboard-first switcher and full control center. Live Hyprland smoke tests observed:

- `clip-sync-switcher`, floating and mapped at `720x420`;
- `clip-sync-control`, floating and mapped at `1040x700`;
- a second switcher invocation focused/signalled the singleton instead of opening a duplicate.

The daemon-only build has no egui/windowing dependency path. Final rules and the `SUPER+H` cutover are documented in [deployment.md](deployment.md).

## Remaining validation beyond Milestone 0

Milestone 0 has no open exit criterion. Release validation still includes the real-device compatibility matrix, actual suspend/resume cycles, and multi-day soak described in [PLAN.md](../PLAN.md).
