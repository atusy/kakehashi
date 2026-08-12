//! Merged end-to-end test binary.
//!
//! Every `e2e_*` module below used to be a standalone integration-test
//! target. Merging them means the test executable links once instead of 57
//! times — a lib edit previously cost ~290s of CPU relinking binaries. The
//! price is that all tests share one process; the helpers were already
//! designed for that (process-wide `OnceLock` shared install dir and
//! per-process isolated config dir in `helpers::lsp_client`).
//!
//! Run everything:  `cargo test --features e2e --test e2e`
//! Run one module:  `cargo test --features e2e --test e2e e2e_semantic::`

#![cfg(feature = "e2e")]

mod helpers;

mod e2e_cancel_request;
mod e2e_cli_diagnose;
mod e2e_cli_format;
mod e2e_code_action;
mod e2e_code_lens_resolve;
mod e2e_concatenated_formatting;
mod e2e_config_file;
mod e2e_config_relative_paths;
mod e2e_data_dir;
mod e2e_deprecation_warning;
mod e2e_didchange_forwarding;
mod e2e_didclose_forwarding;
mod e2e_host_bridge;
mod e2e_incremental_sync;
mod e2e_kakehashi_captures;
mod e2e_kakehashi_node;
mod e2e_kakehashi_node_accessors;
mod e2e_layers_and_allowlist;
mod e2e_lsp_capability;
mod e2e_lsp_init_supersede;
mod e2e_lsp_lua_color_presentation;
mod e2e_lsp_lua_completion;
mod e2e_lsp_lua_declaration;
mod e2e_lsp_lua_definition;
mod e2e_lsp_lua_diagnostic;
mod e2e_lsp_lua_document_color;
mod e2e_lsp_lua_document_highlight;
mod e2e_lsp_lua_document_link;
mod e2e_lsp_lua_document_symbol;
mod e2e_lsp_lua_folding_range;
mod e2e_lsp_lua_formatting;
mod e2e_lsp_lua_hover;
mod e2e_lsp_lua_implementation;
mod e2e_lsp_lua_inlay_hint;
mod e2e_lsp_lua_moniker;
mod e2e_lsp_lua_range_formatting;
mod e2e_lsp_lua_references;
mod e2e_lsp_lua_rename;
mod e2e_lsp_lua_signature_help;
mod e2e_lsp_lua_type_definition;
mod e2e_lsp_outside_injection;
mod e2e_lsp_protocol;
mod e2e_native_definition;
mod e2e_nonblocking_init;
mod e2e_notification_timeout;
mod e2e_on_type_formatting;
mod e2e_organize_imports;
mod e2e_push_diagnostics;
mod e2e_selection_range;
mod e2e_semantic;
mod e2e_semantic_blockquote;
mod e2e_semantic_tokens_refresh;
mod e2e_shared_instance;
mod e2e_stable_region_id;
mod e2e_synthetic_push_diagnostic;
mod e2e_window_notifications;
mod e2e_workspace_folder_config;
