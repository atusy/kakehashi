#[cfg(feature = "allocation-profile")]
#[doc(hidden)]
pub mod allocation_profile;
pub(crate) mod analysis;
pub(crate) mod cancel;
pub mod cli;
pub(crate) mod compute_pool;
pub mod config;
pub(crate) mod document;
pub(crate) mod error;
pub(crate) mod experimental;
pub mod install;
pub(crate) mod language;
pub mod lsp;
pub(crate) mod text;
