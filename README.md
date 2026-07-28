<p align="center">
  <img src="assets/logo.png" alt="clip-sync logo" width="180" />
</p>

<h1 align="center">clip-sync</h1>

<p align="center">
  A masterless, encrypted clipboard-history mesh written in Rust.
</p>

<p align="center">
  <strong>Early development:</strong> the protocol and storage format are not yet stable or ready for sensitive data.
</p>

![A masterless network of clipboard peers](assets/splash.png)

## Why clip-sync?

Most clipboard sync tools assume a central service or immediately replace every connected device's clipboard. clip-sync is being built around a different model:

- **No master node.** Every peer stores and forwards retained history.
- **History before interruption.** Remote copies enter a merged history without replacing the active clipboard.
- **Offline reconciliation.** Peers catch up after reconnecting.
- **Private-network first.** The initial transport targets trusted devices connected through NetBird.
- **Encrypted persistence.** Clipboard payloads and searchable metadata are designed to remain encrypted at rest.
- **Keyboard first.** An optional egui switcher is planned for fast search and activation.

## Initial target

The first daily-driver target is NixOS on Hyprland/wlroots, synchronizing two personal devices over NetBird. The architecture keeps clipboard and peer-discovery boundaries narrow so other platforms can be added later.

The first end-to-end milestone is intentionally smaller: authenticated two-node synchronization of encrypted text history.

## Planned commands

```console
clip-sync daemon
clip-sync ui switcher
clip-sync ui control
clip-sync status --json
clip-sync peers --json
clip-sync history search "device:kiwi error" --json
clip-sync share-clipboard
clip-sync doctor --json
```

The UI commands will only be available when built with the optional `ui` Cargo feature.

## Status and roadmap

| Phase | Goal | Status |
| --- | --- | --- |
| 0 | Validate Wayland, NetBird, QUIC authentication, encrypted storage, and egui integration | In progress |
| 1 | Project foundation, config, model, IPC, CLI, and CI | Planned |
| 2 | Encrypted local text history | Planned |
| 3 | Two-node text mesh vertical slice | Planned |
| 4 | Keyboard-first egui switcher | Planned |
| 5 | Arbitrary MIME and safe file snapshots | Planned |
| 6 | Chunked, cancellable, resumable large sharing | Planned |
| 7 | Retention and convergence hardening | Planned |
| 8 | Full control center and diagnostics | Planned |
| 9 | NixOS daily-driver deployment | Planned |

See [PLAN.md](PLAN.md) for the detailed architecture, threat model, milestones, test strategy, and acceptance criteria.

## Security warning

Clipboard history routinely contains passwords, tokens, private keys, personal messages, and proprietary data. **Do not use the current development version for sensitive clipboard contents.** Security-sensitive behavior will remain clearly marked until the storage and network designs have been independently reviewed and hardened.

The initial trust model uses one high-entropy shared mesh secret supplied through a file (for example, by SOPS). Anyone holding that secret is an equal mesh member and can read or mutate shared history.

Please report vulnerabilities according to [SECURITY.md](SECURITY.md).

## Development

The repository will use stable Rust and expose the standard checks:

```console
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
```

Nix development and packaging support will be added as the Rust foundation lands.

## Contributing

The project is personal-first but intended to become reusable. Design discussion and focused contributions are welcome; read [CONTRIBUTING.md](CONTRIBUTING.md) before opening a pull request.

## License

MIT © Fractal-Tess. See [LICENSE](LICENSE).
