pub(crate) mod store;

pub(crate) mod model;

// Re-export main types
pub(crate) use model::Document;
pub use store::{DocumentHandle, DocumentStore};
