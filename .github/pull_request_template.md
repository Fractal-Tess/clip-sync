## Summary

<!-- What changed and why? -->

## Verification

- [ ] `cargo fmt --check`
- [ ] `cargo clippy --all-targets --all-features -- -D warnings`
- [ ] `cargo test --all-features`

## Replication and security impact

<!-- Describe behavior under retries, duplicate delivery, partitions, invalid input, and resource pressure. Write “none” only when truly unrelated. -->

- [ ] No clipboard contents, keys, filenames, previews, addresses, or search terms are logged.
- [ ] New persistent or wire data is versioned and migration/compatibility behavior is documented.
