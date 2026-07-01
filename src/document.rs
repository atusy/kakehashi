pub(crate) mod store;

pub(crate) mod injections;

pub(crate) mod model;

// Re-export main types
pub(crate) use injections::{DiscoveredRegion, DiscoveredRegionCache};
pub(crate) use model::Document;
pub use store::{DocumentHandle, DocumentStore};
