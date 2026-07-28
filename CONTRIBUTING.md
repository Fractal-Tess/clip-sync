# Contributing to clip-sync

clip-sync is currently personal-first and pre-alpha. Before implementing a large feature, open a discussion or issue so it can be checked against the replication and security model in [PLAN.md](PLAN.md).

## Development principles

- Preserve masterless behavior: no hidden coordinator or privileged peer.
- Fail closed when authentication, decryption, or validation fails.
- Stream clipboard and transfer data; do not trust payload sizes.
- Keep persistent clipboard content and metadata encrypted.
- Make replicated state transitions deterministic, idempotent, and testable under reordering.
- Keep graphical dependencies behind the `ui` feature.
- Never log clipboard contents, filenames, previews, keys, or plaintext search queries.
- Avoid `unsafe` unless a platform boundary requires it and the invariant is documented.

## Checks

Before submitting a pull request, run:

```console
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
```

Add property or integration tests for replication-state changes. Security-sensitive changes should explain failure behavior and resource bounds in the pull request.

## Commit style

Use focused commits with imperative subjects. Prefer one completed milestone slice per commit over mixing refactors, generated assets, and behavior changes.

## AI-generated contributions

AI assistance is allowed, but contributors remain responsible for understanding, testing, licensing, and reviewing submitted code. Do not submit generated cryptographic constructions without careful human review.
