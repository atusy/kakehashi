pub(crate) mod config_store;
pub(crate) mod coordinator;
pub(crate) mod events;
pub(crate) mod failed_parsers;
pub(crate) mod filetypes;
pub(crate) mod heuristic;
pub mod injection;
pub(crate) mod loader;
pub(crate) mod parser_pool;
pub(crate) mod predicate_accessor;
pub(crate) mod query_loader;
pub(crate) mod query_pattern_splitter;
pub(crate) mod query_predicates;
pub(crate) mod query_store;
pub(crate) mod region_id_tracker;
pub(crate) mod registry;

pub use config_store::ConfigStore;
pub use coordinator::LanguageCoordinator;
pub(crate) use events::{LanguageEvent, LanguageLogLevel};
pub(crate) use failed_parsers::FailedParserRegistry;
pub use filetypes::FiletypeResolver;
pub(crate) use parser_pool::DocumentParserPool;
pub use query_predicates::filter_captures;
pub use query_store::QueryStore;
pub use registry::LanguageRegistry;

// Re-export injection types for semantic tokens
pub(crate) use injection::InjectionResolver;

// Re-export region ID tracking
pub(crate) use region_id_tracker::RegionIdTracker;
