pub mod store;

pub(crate) mod model;

// Re-export main types
pub(crate) use model::Document;
pub(crate) use store::DocumentHandle;
pub use store::DocumentStore;
