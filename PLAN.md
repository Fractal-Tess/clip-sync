# clip-sync development plan

## 1. Product definition

`clip-sync` is a personal, masterless clipboard-history mesh for trusted Linux devices connected through NetBird.

Each node is equal. Every node captures local clipboard changes, retains a full replica of shared history, forwards entries created by other nodes, and reconciles after offline periods. Remote entries enter history silently; they never replace a device's active clipboard automatically.

The first deployment targets the `vd` and `kiwi` NixOS hosts running Hyprland.

### Core user workflows

1. Copy ordinary content on either machine.
2. The local daemon captures it and adds it to the merged history.
3. Online peers receive it; offline peers reconcile all retained history when they return.
4. Press `SUPER+H` to open a compact egui switcher.
5. Search or cycle with the keyboard and press Enter to set the selected item as the active clipboard.
6. Open the full control center to manage history, transfers, peers, settings, and diagnostics.
7. If the current clipboard exceeds the automatic-capture threshold, explicitly share it from the control center.

### Agreed product decisions

- Linux first, specifically Hyprland/wlroots and the regular Wayland clipboard.
- Do not monitor the primary selection.
- Arbitrary clipboard MIME formats should eventually be stored and re-offered as opaque bytes.
- All nodes retain and forward the full mesh history; there is no master or origin-only dependency.
- Remote entries are history-only until deliberately activated.
- One deterministic merged timeline across all devices.
- Exact-byte content deduplication; copying the same content moves its existing entry to the top.
- Activating an old entry also moves it to the top mesh-wide.
- Full retained-history reconciliation after offline periods.
- Manual deletion propagates to every node.
- Automatic quota eviction is also mesh-wide.
- Pins replicate and are exempt from automatic eviction.
- Default logical mesh quota: 1 GiB.
- Default automatic-capture threshold: 20 MiB.
- Items over 20 MiB are not entered even into local history unless explicitly shared.
- Explicit oversized shares may exceed the quota and become quota-exempt until explicitly deleted.
- Large sharing is cancellable and automatically resumable after interruption.
- Cancellation removes the history operation and all partial chunks throughout the mesh.
- Files are reconstructed in `$XDG_RUNTIME_DIR/clip-sync/` when activated and removed after the clipboard changes, with a short safety delay.
- Synchronization is silent by default.
- Set the clipboard after switcher selection; do not simulate a paste.
- Replace the current cliphist/Rofi workflow and retain the `SUPER+H` binding.
- Personal-first implementation, with narrow clipboard and discovery interfaces for future portability.
- MIT license.

## 2. Scope boundaries

### In the first daily-driver release

- Wayland clipboard capture and ownership on Hyprland.
- Encrypted local history.
- NetBird peer discovery.
- Authenticated QUIC connections.
- Masterless operation-log replication and anti-entropy reconciliation.
- Text, images, arbitrary MIME representations, and copied file snapshots.
- Deduplication, activation, replicated deletion, pins, and quota eviction.
- Chunked, cancellable, resumable large transfers.
- Full CLI parity with machine-readable JSON.
- Optional egui switcher and control center behind the Cargo `ui` feature.
- Fast `vd` to `kiwi` smoke-test deployment without NixOS rebuilds.
- Final NixOS/SOPS/Stow/systemd/Hyprland integration.

### Explicitly deferred

- X11, GNOME, KDE, Windows, macOS, and mobile support.
- LAN multicast, public rendezvous, NAT traversal, or relay infrastructure.
- Automatic replacement of remote clipboards.
- Primary-selection capture.
- Auto-paste.
- Clipboard-source application exclusions or secret detection.
- Desktop notifications.
- cliphist history import.
- Backup/export workflows.
- Multi-user shared meshes.

## 3. Architecture

Use one Rust package containing a library and one `clip-sync` binary. Keep egui dependencies optional.

```text
clip-sync daemon
clip-sync ui switcher       # requires `ui`
clip-sync ui control        # requires `ui`
clip-sync status --json
clip-sync peers --json
clip-sync history search ... --json
clip-sync history activate <id>
clip-sync history pin <id>
clip-sync history delete <id>
clip-sync share-clipboard
clip-sync transfer cancel <id>
clip-sync device forget <id>
clip-sync config show --json
clip-sync doctor --json
clip-sync rekey --old-key-file ... --new-key-file ...
```

Suggested source layout:

```text
src/
  main.rs
  lib.rs
  cli.rs
  config.rs
  model/
  crypto/
  storage/
  clipboard/
    mod.rs
    wayland.rs
  discovery/
    mod.rs
    netbird.rs
  transport/
    quic.rs
    protocol.rs
  replication/
    log.rs
    reconcile.rs
    quota.rs
  transfer/
  ipc/
  ui/                    # cfg(feature = "ui")
tests/
  convergence.rs
  transport.rs
  storage_recovery.rs
  smoke.rs
```

### Runtime ownership

The daemon is the only process that owns:

- the Wayland clipboard connection;
- the encrypted store;
- mesh connections and transfer state;
- replicated settings;
- quota and tombstone processing.

CLI and egui processes are clients over a user-only Unix socket:

```text
$XDG_RUNTIME_DIR/clip-sync/daemon.sock
```

Create the socket directory and socket with user-only permissions. Use a versioned local protocol. The UI must never open the history database directly.

If the UI is invoked while the daemon is down, it asks systemd to start `clip-sync.service`, waits briefly for IPC, and then opens. A development fallback may spawn `clip-sync daemon --foreground` when no user unit exists.

## 4. Identity, configuration, and secrets

### Node identity

- Generate a stable random internal node UUID on first start.
- Display the system hostname (`vd`, `kiwi`) in history and peer views.
- Reject duplicate active node identities, while duplicate hostnames produce a visible diagnostic rather than becoming protocol identities.
- A forgotten node identity is rejected if it returns. Rejoining requires resetting its local identity and treating it as a new device.

### Configuration

Default path:

```text
~/.config/clip-sync/config.toml
```

In the final setup this is a Stow-managed symlink into:

```text
~/nixos/dotfiles/desktop/.config/clip-sync/config.toml
```

The control center may update this file atomically. Replicated mesh-setting changes also rewrite the effective TOML on peers, as requested. Avoid config-watch feedback loops by attaching a revision and suppressing reloads caused by the daemon's own atomic replacement.

Separate settings into:

- **Shared mesh settings:** quota, capture threshold, transfer policy, and other behavior that must converge.
- **Local bootstrap settings:** mesh-key path, state/runtime paths, NetBird command, bind policy, log level, UI window preferences, and development overrides.

Shared settings use last-writer-wins registers ordered by a hybrid logical clock plus node ID. Any authenticated node has equal authority.

### Mesh secret

Read a high-entropy secret from a configured file, for example:

```toml
mesh_key_file = "/run/secrets/clip-sync-mesh-key"
```

SOPS provisions the file with restrictive ownership and permissions. The daemon does not implement pairing or join tokens. A key-file change requires a daemon restart.

Derive domain-separated keys with HKDF-SHA-256 for:

- connection authentication;
- database/envelope encryption;
- chunk encryption and keyed identifiers;
- protocol-specific MACs.

Never log clipboard content, keys, plaintext search terms, filenames, or previews.

### Rekeying

Because history encryption is rooted in the mesh secret, rotation is a coordinated operation:

1. Stop or quiesce the daemon.
2. Run `clip-sync rekey` with old and new SOPS-provided key files.
3. Atomically re-encrypt/wrap the local store.
4. Repeat on every node.
5. Deploy the new configured secret and restart the mesh.

Use envelope encryption so rekeying normally rewrites wrapped data keys and database protection rather than every large payload chunk. Make interrupted rekey recovery transactional.

## 5. Storage and at-rest encryption

Use SQLite with SQLCipher for transactional metadata, operation logs, transfer state, settings, tombstones, and full-text search. Package a pinned SQLCipher build so behavior does not depend on the host's SQLite configuration.

Store large payloads outside SQLite as encrypted fixed-size chunks:

- XChaCha20-Poly1305 authenticated encryption;
- a fresh nonce per chunk;
- keyed BLAKE3 identifiers so plaintext hashes are not exposed;
- encrypted manifests in SQLCipher;
- atomic staging followed by rename;
- reference counting for deduplicated chunks;
- crash cleanup for uncommitted staging data.

Small payloads can remain inside encrypted database pages. Padding and fixed-size chunks should limit exact-size leakage for large content, although total encrypted disk usage cannot be completely hidden.

Suggested locations:

```text
$XDG_STATE_HOME/clip-sync/history.db
$XDG_STATE_HOME/clip-sync/chunks/
$XDG_RUNTIME_DIR/clip-sync/materialized/
```

The persistent state contains no plaintext clipboard bytes or metadata. Decrypted search results, previews, and activated file materializations exist only in process memory or runtime storage.

SQLCipher FTS5 keeps the advanced text index encrypted at rest. The query layer should support filters such as:

```text
device:kiwi type:text before:2026-08-01 pinned:true error message
```

## 6. Clipboard model

### Clipboard backend boundary

Define a narrow interface for:

- subscribing to clipboard offers;
- enumerating offered MIME types;
- streaming a representation with cancellation and size limits;
- inspecting the current offer for explicit sharing;
- becoming clipboard owner and serving several MIME representations;
- observing when clipboard ownership/content changes.

Implement it first with `wlr-data-control` for Hyprland. Do not shell out to `wl-paste`/`wl-copy` in the production backend, although those tools are useful as test oracles.

### Capturing offers

For every new regular-clipboard offer:

1. Enumerate MIME types.
2. Stream representations into encrypted staging rather than unbounded memory.
3. Enforce per-stream timeout and cancellation.
4. Compute aggregate logical size and exact-byte content identity.
5. Detect file-list MIME types and inspect referenced regular files/directories before snapshotting them.
6. If aggregate content exceeds 20 MiB, abort staging and do not create a local history entry.
7. Otherwise commit the item and emit a replicated add/touch operation.

For arbitrary formats, preserve MIME names and bytes exactly. Restoration re-offers every retained representation. Vendor-specific formats may remain unusable on another application, but clip-sync must not reinterpret or corrupt them.

### Content identity and deduplication

Calculate a BLAKE3 digest over a canonical sequence of exact MIME names, lengths, and exact bytes. Sort MIME representations by MIME name before hashing.

History state is not merely a list of blobs. It consists of immutable content plus replicated operations:

- `Add` or `Touch`
- `Pin` / `Unpin`
- `Delete`
- `BeginShare` / `CompleteShare` / `CancelShare`
- shared-setting changes
- membership/forget operations

A repeated visible content identity generates a new `Touch`, moving the existing entry to the top. A content item copied after it was deleted can become visible again through a genuinely newer add/touch operation; replaying an old operation cannot resurrect it.

### Clipboard feedback loops

When clip-sync activates and serves an item, mark the ownership generation locally. The subsequent Wayland offer must become one intentional `Touch`, not an infinite capture/replication loop. Duplicate network deliveries never create new touches.

### Files

Treat copied file URIs as snapshots, not remote path references:

- Validate paths and prevent traversal.
- Snapshot regular file bytes and safe relative names.
- Recursively size directories before automatic capture.
- Do not follow unsafe symlink escapes.
- Preserve a conservative metadata set; do not restore ownership or privileged mode bits.

When activated on a peer, materialize decrypted files in `$XDG_RUNTIME_DIR/clip-sync/materialized/<item>/`, publish local URIs, and remove the materialization after clipboard ownership changes plus a short grace delay.

Large materializations require a free-space check. Runtime storage is normally tmpfs, so very large files can fail activation even though their encrypted history is valid; show a clear error. A future FUSE-backed decrypted view can remove this limitation.

## 7. Masterless replication model

### Clocks and operation identity

- Give each node a persistent node ID and monotonic operation counter.
- Identify operations by `(node_id, counter)`.
- Use a hybrid logical clock for user-visible ordering and last-writer-wins fields.
- Track a version vector/frontier summarizing operations seen from every known member.

The operation application function must be deterministic, idempotent, and commutative. The same final state must result regardless of delivery order, duplication, reconnects, or forwarding path.

Suggested conflict rules:

- The latest valid touch controls timeline position.
- Explicit deletion dominates older add/touch/pin operations.
- A genuinely newer local copy may reintroduce the same exact content.
- Latest pin register wins unless the item is deleted.
- Transfer cancellation dominates incomplete transfer state.
- Shared-setting registers use HLC then node ID as a deterministic tie-breaker.

### Anti-entropy

On connection:

1. Authenticate the peer.
2. Exchange protocol capabilities, node identity, membership state, and version-vector frontier.
3. Request missing operations in bounded batches.
4. Apply and acknowledge operations transactionally.
5. Request missing payload chunks by encrypted chunk ID.
6. Continue live operation exchange after convergence.

Every node stores and forwards operations and payloads created elsewhere. A `vd -> kiwi -> future-node` path works even if the origin is offline.

Use periodic anti-entropy in addition to live push so dropped notifications cannot create permanent divergence.

### Deletes and retired devices

Deletes create tombstones. Keep a tombstone until all known, non-forgotten members have acknowledged a frontier containing it. A permanently lost node can therefore block cleanup.

Expose `device forget` in UI and CLI. Forgetting a device is itself a replicated operation. Once all remaining members observe it, old acknowledgements/tombstones can be compacted safely. Explain that forgetting is a history-maintenance action, not key revocation: any machine still holding the shared secret can reset its identity and join as a new node.

### Mesh-wide quota

Compute quota against unique logical payload bytes, excluding:

- pinned items;
- explicitly shared oversized items marked quota-exempt.

When over quota, deterministically select the oldest visible, unpinned, non-exempt entries and emit ordinary replicated deletions. Nodes in a partition may initiate the same deterministic eviction independently; merge and re-evaluate after reconnection.

Test concurrent-partition eviction heavily because a fully masterless mesh can temporarily exceed the quota and can evict more than the ideal minimum when partitions make decisions from different snapshots. Prefer temporary overflow over deleting non-deterministically.

## 8. Network transport and NetBird discovery

### Discovery

Run `netbird status --json` periodically and parse:

- the local NetBird address/interface;
- every currently visible peer's NetBird IP and hostname;
- connectivity changes.

Probe the configured clip-sync QUIC port on every NetBird peer. Non-members fail authentication. Keep discovery parsing behind a `PeerDiscovery` interface and preserve fixtures for NetBird JSON schema changes.

Bind only to the local NetBird address. If NetBird is unavailable, continue local capture/history and report degraded mesh status rather than exiting.

### QUIC authentication

Use Quinn/rustls for encrypted multiplexed transport. A self-signed TLS certificate alone is not device trust. Immediately perform shared-key confirmation bound to:

- both random nonces;
- both protocol hellos;
- the negotiated protocol version;
- the QUIC/TLS exporter value.

Authenticate the transcript with HMAC-SHA-256 derived from the mesh secret. Do not exchange replication metadata before key confirmation succeeds. Binding the MAC to the TLS exporter prevents a man-in-the-middle from splicing two valid encrypted sessions without knowing the shared secret.

Add replay protection, handshake timeouts, connection rate limits, bounded message sizes, and protocol-version negotiation.

Use Protobuf (`prost`) for versioned control messages. Transfer payload chunks over dedicated QUIC streams rather than embedding them in control messages.

## 9. Large-share transaction design

The control center inspects the current live clipboard even if it was ignored by automatic capture.

- At or below 20 MiB: normal Share styling.
- Above 20 MiB: yellow warning state, exact human-readable size, quota/free-space implications, and confirmation.
- Begin encrypted streaming/chunking only after confirmation.
- Insert a visible pending history/transfer entry.
- Replicate transaction metadata and chunks to online peers.
- Offline peers fetch the item when they reconnect.
- Resume verified chunks from any peer that has them after interruption.
- Show per-peer and aggregate progress in the control center and CLI.

Cancel emits a mesh-wide cancellation/tombstone, stops active streams, deletes staging and unreferenced chunks, and prevents an offline peer from later completing an old transaction.

An explicit item larger than the 1 GiB quota is quota-exempt. Before starting, still check local free disk and make the risk clear. The user must delete it explicitly to reclaim its retained space.

## 10. UI plan (`ui` Cargo feature)

Use `eframe`/egui behind an optional `ui` feature. A daemon/CLI-only build must not compile or link graphics dependencies.

### Quick switcher

Command:

```text
clip-sync ui switcher
```

Behavior:

- native dark utility style;
- undecorated floating normal Wayland window;
- Hyprland rules center and size it;
- search field focused immediately;
- newest merged entries shown first;
- arrows/Page Up/Page Down move selection;
- Enter activates and closes;
- Escape closes;
- do not close merely because focus changes;
- `SUPER+H` focuses/toggles an existing switcher instead of opening duplicates.

Each compact row should show only useful scan information:

- type icon or small preview;
- one- or two-line text/filename preview;
- source hostname;
- relative time;
- size;
- pin and transfer-state indicators.

Support advanced query filters without making the default workflow look like a database console.

### Full control center

Command:

```text
clip-sync ui control
```

Sections:

1. **History** — search, preview, activate, pin, delete, inspect MIME types and source metadata.
2. **Share current clipboard** — live type/size inspection, oversized warning, confirmation, and progress.
3. **Transfers** — current/paused/resuming/completed operations and cancellation.
4. **Peers** — online/offline state, reconciliation frontier, bytes transferred, last seen, and forget-device action.
5. **Settings** — shared and local settings, validation, effective source, and live apply.
6. **Diagnostics** — NetBird status, listener address, storage health, protocol versions, and copyable redacted diagnostics.

All mutating actions go through daemon IPC and return explicit success/failure revisions.

## 11. CLI and local IPC

The CLI should have feature parity with the control center and stable JSON output. This makes hotkeys, smoke tests, recovery, and future integrations independent of egui.

Requirements:

- human-readable output by default;
- `--json` for every query and mutation result;
- nonzero, documented exit codes;
- operation/transfer IDs in output;
- bounded previews rather than accidental full secret output;
- an explicit flag when full clipboard content is requested;
- `doctor` redacts sensitive fields by construction.

Use a versioned request/response and subscription protocol over the Unix socket. The UI subscribes to history, transfer, peer, and setting changes rather than polling aggressively.

## 12. Development milestones

### Milestone 0 — risk spikes

Prove the riskiest dependencies before building the full architecture:

1. Capture and restore several MIME representations through `wlr-data-control` on Hyprland.
2. Keep serving restored clipboard bytes after the source application exits.
3. Parse live `netbird status --json`, locate `kiwi`, and bind only to the NetBird address.
4. Establish Quinn connections over NetBird and complete TLS-exporter-bound key confirmation.
5. Build and reopen a SQLCipher database in Nix; verify plaintext strings do not occur in the file.
6. Build an egui floating window with the required Hyprland focus behavior.

Exit criterion: each spike has a tiny automated or documented repeatable test and no unresolved architecture blocker.

### Milestone 1 — project foundation

- Cargo package, optional `ui` feature, flake/dev shell, formatting/lint/test commands, tracing, config parsing, IDs, HLC, and operation types.
- Unix IPC skeleton and CLI command structure.
- Temporary development-key support with strict warnings.
- CI for `cargo fmt --check`, `cargo clippy --all-targets --all-features`, tests, and daemon-only build.

### Milestone 2 — encrypted local text history

- Wayland text capture.
- SQLCipher persistence and exact-byte dedupe.
- Local search, activate, pin, delete, quota, and restart recovery.
- Daemon/CLI split over IPC.
- Ensure activating an item creates exactly one touch and no feedback loop.

Exit criterion: clip-sync can replace cliphist locally for text from the CLI.

### Milestone 3 — two-node text mesh vertical slice

- NetBird discovery and NetBird-only bind.
- Shared-key QUIC authentication.
- Version vectors, operation exchange, full text payload replication, live push, and reconnect reconciliation.
- Store-and-forward test with three simulated nodes.
- Real `vd`/`kiwi` temporary-secret smoke test.

Exit criterion: text copied on either device appears in both histories, remains history-only remotely, survives either side being offline, and converges after reconnect.

### Milestone 4 — keyboard-first egui switcher

- Optional UI feature and IPC subscriptions.
- Search/filter parser, navigation, activation, previews, singleton/focus behavior, and daemon auto-start.
- Hyprland floating-window rules and temporary `SUPER+H` smoke-test binding.

Exit criterion: the egui switcher fully replaces the Rofi picker for daily text use.

### Milestone 5 — arbitrary MIME and file snapshots

- Multi-MIME canonical identity and restoration.
- Streaming capture with timeouts and 20 MiB aggregate threshold.
- Images and generated in-memory previews.
- File-list recognition, safe file/directory snapshots, and runtime materialization.
- Compatibility tests against browsers, terminals, editors, image tools, and a file manager.

### Milestone 6 — chunked large sharing

- Encrypted chunk store and manifests.
- Begin/progress/complete/cancel operations.
- Resumable QUIC chunk requests from any replica.
- Explicit oversized sharing, warnings, quota exemption, cleanup, free-space handling, and crash recovery.

### Milestone 7 — full convergence and retention hardening

- Replicated pins, deletes, shared settings, and mesh-wide deterministic eviction.
- Tombstone acknowledgement/compaction.
- Known-device state and manual forget workflow.
- Protocol/schema migration tests and rolling-version rejection/degradation behavior.
- Fault injection for duplicate, reordered, delayed, dropped, and corrupted traffic.

### Milestone 8 — full control center and diagnostics

- History, share, transfers, peers, settings, and diagnostics sections.
- Live settings apply and atomic replicated TOML updates.
- Redacted support bundle/status output.
- Accessibility, keyboard coverage, large-history responsiveness, and visual polish.

### Milestone 9 — daily-driver deployment

- Nix package and final module/service integration.
- SOPS secret provisioning for `vd` and `kiwi`.
- Stowed config directory.
- Remove `wl-paste --watch cliphist store` from Hyprland autostart.
- Change `SUPER+H` from `~/nixos/scripts/session/clipboard` to `clip-sync ui switcher`.
- Keep a documented rollback to cliphist during the initial soak period.
- Multi-day partition, restart, suspend/resume, and large-transfer soak tests.

## 13. Testing strategy

### Unit and property tests

Use `proptest` for state-machine invariants:

- operation application is idempotent;
- replicas converge under all operation permutations;
- duplicate delivery changes nothing;
- old operations cannot resurrect deleted data;
- intentional newer copies can reintroduce content;
- quota selection is deterministic from equal state;
- pins and quota-exempt items are never automatically evicted;
- cancellation dominates incomplete transfer operations;
- HLC remains monotonic across clock regressions.

### Integration tests

- Three or more in-process nodes over loopback QUIC.
- Random partitions and reconnection.
- Store-and-forward with origin offline.
- Interrupted and resumed chunk transfers.
- Cancel during every transfer phase.
- Peer forgotten while offline.
- Database process kill during add/delete/rekey/compaction.
- Corrupt chunk and failed authentication handling.
- Protocol-version mismatch.
- NetBird JSON fixtures and malformed command output.

### Security checks

- Search database/chunk files for known plaintext fixtures.
- Verify wrong keys fail closed without modifying state.
- Verify unauthenticated peers receive no metadata.
- Fuzz protocol decoders and clipboard MIME metadata.
- Bound allocations, message sizes, stream counts, and decompression ratios.
- Run `cargo audit` and dependency-deny policy in CI.

### Real-device smoke matrix

On `vd` and `kiwi` test:

- ASCII, Unicode, multiline text, exact whitespace, and large text;
- browser HTML + plain-text offers;
- PNG screenshots and copied images;
- small and oversized files/directories;
- source application exiting immediately after copy;
- sleep/resume and NetBird disconnect/reconnect;
- daemon restart while clipboard content is active;
- concurrent copies on both nodes;
- delete/pin/settings while partitioned;
- interrupted and cancelled large shares;
- quota pressure and explicit quota overflow.

## 14. Fast `vd` to `kiwi` development loop

Do not rebuild NixOS for each smoke test.

Create a helper such as:

```text
scripts/deploy-smoke kiwi
```

It should:

1. Build a release daemon/CLI or UI bundle on `vd`.
2. Verify architecture and runtime dependencies.
3. Package required shared libraries or create a self-contained Nix bundle/tar artifact.
4. SCP it over the NetBird address to a versioned directory under `/tmp/clip-sync-smoke/` on `kiwi`.
5. Create independent temporary config/state/runtime directories.
6. Provision the same disposable 32-byte development secret with mode `0600` on both hosts.
7. Start foreground daemons with test ports and verbose redacted logging.
8. Run CLI health, authentication, replication, partition, and cleanup checks over SSH.
9. Stop processes and remove temporary keys/state when requested.

Do not use the production SOPS secret during development. Do not assume a raw Nix-built dynamically linked binary is portable merely because both hosts are NixOS; inspect `ldd`/RPATH and bundle its closure or libraries as necessary while still using SCP as the transport.

## 15. Final NixOS integration

Once smoke testing is stable:

- Add the clip-sync package to the Nix configuration for `vd` and `kiwi`.
- Create a systemd user service with restart-on-failure and graphical-session integration.
- Ensure the service receives the Wayland/UWSM environment and starts only when a user clipboard is available.
- Depend on NetBird opportunistically, but do not make local history unavailable when NetBird is down.
- Use SOPS to materialize a user-readable, non-world-readable mesh-key file.
- Add the Stow-managed `~/.config/clip-sync/config.toml`.
- Add Hyprland floating rules for switcher/control-center app IDs.
- Replace the current cliphist autostart and `SUPER+H` command.

A system rebuild is only required for this final package/service/secret wiring, not the iterative application smoke-test loop.

## 16. Daily-driver acceptance criteria

The first personal release is complete only when all of the following hold:

1. `vd` and `kiwi` converge after arbitrary offline periods without manual repair.
2. No remote arrival changes the active local clipboard.
3. Exact duplicates produce one visible item whose latest touch is deterministic.
4. Restarting or killing a daemon cannot leave plaintext, a corrupt visible entry, or an unrecoverable store.
5. Wrong-key and unauthenticated connections fail before metadata exchange.
6. All persistent history content and searchable metadata are encrypted at rest.
7. Every large transfer can resume or be cancelled, with partial-data cleanup verified.
8. Deletes, pins, shared settings, and quota decisions converge across partitions.
9. Forgotten devices no longer block safe tombstone compaction.
10. The switcher opens quickly, is fully keyboard-operable, and restores all supported representations correctly.
11. Daemon-only builds contain no egui/windowing dependency path.
12. CLI JSON can drive every control-center operation.
13. Suspend/resume, NetBird outages, and source-application exits do not lose committed history.
14. The service runs for a multi-day soak on both hosts without unbounded memory, disk, task, or connection growth.

## 17. Primary risks and mitigations

### Wayland clipboard behavior

Clipboard ownership, lazy MIME serving, and file-manager conventions are the largest platform risk. Address them first with the Milestone 0 spike and test against actual Hyprland applications.

### Masterless delete and quota semantics

Partitions make global retention decisions inherently subtle. Use immutable operations, version vectors, deterministic state transitions, convergence property tests, and temporary overflow rather than aggressive uncertain deletion.

### Shared-secret compromise

A holder of the shared secret is an equal mesh member and can read, add, delete, pin, or change shared settings. NetBird isolation and SOPS protection are therefore part of the threat model. Future public versions may add per-device identity keys and approval.

### Huge payload resource exhaustion

Stream all data, bound concurrency, preflight free disk, authenticate before allocation, use fixed chunk limits, and keep cancellation available throughout. Quota-exempt does not mean resource-unchecked.

### At-rest encryption versus file activation

Opaque clipboard bytes can remain encrypted, but file paste protocols require usable local paths. Runtime tmpfs materialization minimizes persistence but can fail for very large files. Surface this honestly and consider FUSE later.

### NetBird CLI coupling

Keep JSON parsing isolated behind fixtures and permit an explicit-peer development fallback. A future backend can use NetBird APIs or DNS without changing replication.

### Stow-managed config mutation

UI and replicated changes intentionally modify tracked files in `~/nixos`. Use atomic formatting-preserving updates where possible, expose the exact path in the UI, and never place runtime state or secrets in the dotfiles tree.
