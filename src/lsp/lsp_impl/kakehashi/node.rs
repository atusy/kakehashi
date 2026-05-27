//! Handlers for the ADR-0025 Node Reference Protocol.
//!
//! See [ADR-0025](../../../../../docs/adr/0025-node-reference-protocol.md) for the
//! protocol specification. Each method has its own file under `node/`:
//!
//! - [`entry`]: `kakehashi/node` — position → NodeInfo entry point
//! - [`text`]: `kakehashi/node/text` — id → current node text
//!
//! Future PRs will add `parent` and `children` handlers alongside these.

mod entry;
mod text;
