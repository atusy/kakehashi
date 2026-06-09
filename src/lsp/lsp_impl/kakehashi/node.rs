//! Handlers for the Node Reference Protocol (node-reference-protocol).
//!
//! See [node-reference-protocol](../../../../docs/architecture-decisions/node-reference-protocol.md) for the
//! protocol specification. Each method has its own file under `node/`:
//!
//! - [`entry`]: `kakehashi/node` — position → NodeInfo entry point
//! - [`text`]: `kakehashi/node/text` — id → current node text
//! - [`parent`]: `kakehashi/node/parent` — id → immediate-parent NodeInfo
//! - [`children`]: `kakehashi/node/children` — id → immediate-children NodeInfo[]

mod children;
mod common;
mod entry;
mod field;
mod injection_stack;
mod lookup;
mod metadata;
mod navigation;
mod parent;
mod position;
mod text;
