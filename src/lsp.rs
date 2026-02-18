pub mod auto_install;
mod bridge;
mod cache;
mod client;
mod debounced_diagnostics;
pub(crate) mod in_progress_set;
mod settings_manager;
mod synthetic_diagnostics;
mod text_sync;

mod aggregation;
mod lsp_impl;
mod progress;
mod request_id;
mod semantic_request_tracker;
mod settings;

pub use bridge::LanguageServerPool;
pub use lsp_impl::Kakehashi;
pub(crate) use request_id::get_current_request_id;
pub use request_id::{CancelForwarder, CancelReceiver, CancelSubscriptionGuard, RequestIdCapture};
pub(crate) use settings::{SettingsEvent, SettingsEventKind, SettingsSource, load_settings};
