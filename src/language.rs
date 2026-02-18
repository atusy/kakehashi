pub mod config_store;
pub mod coordinator;
pub mod events;
pub mod failed_parsers;
pub mod filetypes;
pub mod heuristic;
pub mod injection;
pub mod loader;
pub mod parser_pool;
pub mod predicate_accessor;
pub mod query_loader;
pub(crate) mod query_pattern_splitter;
pub(crate) mod query_predicates;
pub mod query_store;
pub(crate) mod region_id_tracker;
pub mod registry;

pub use config_store::ConfigStore;
pub use coordinator::LanguageCoordinator;
pub use events::{LanguageEvent, LanguageLoadResult, LanguageLoadSummary, LanguageLogLevel};
pub use failed_parsers::FailedParserRegistry;
pub use filetypes::FiletypeResolver;
pub use loader::ParserLoader;
pub use parser_pool::{DocumentParserPool, ParserFactory};
pub use query_loader::QueryLoader;
pub use query_predicates::filter_captures;
pub use query_store::QueryStore;
pub use registry::LanguageRegistry;

// Re-export injection types for semantic tokens
pub use injection::{
    InjectionRegionInfo, InjectionResolver, ResolvedInjection, collect_all_injections,
};

// Re-export region ID tracking
pub(crate) use region_id_tracker::RegionIdTracker;
