//! Integration and property tests for the anti-entropy replication core.

#![allow(clippy::cast_possible_truncation, clippy::trivially_copy_pass_by_ref)]
//!
//! These tests exercise multi-node convergence, store-and-forward relaying,
//! idempotency under duplication/reordering, and resource-bound enforcement
//! without any networking.

#[path = "anti_entropy/batching.rs"]
mod batching;
#[path = "anti_entropy/convergence.rs"]
mod convergence;
#[path = "anti_entropy/properties.rs"]
mod properties;
#[path = "anti_entropy/support.rs"]
mod support;
