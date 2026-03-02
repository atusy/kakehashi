//! Integration test for dynamically loading Lua from searchPaths

use kakehashi::config::{TreeSitterSettings, WorkspaceSettings};
use kakehashi::language::LanguageCoordinator;
use std::collections::HashMap;

/// Verify that the coordinator can dynamically load Lua when searchPaths points to deps/tree-sitter
#[test]
fn test_dynamic_lua_load_from_search_paths() {
    let coordinator = LanguageCoordinator::new();

    // Configure search paths pointing to our deps directory
    let cwd = std::env::current_dir().expect("cwd");
    let search_path = cwd.join("deps/tree-sitter").to_string_lossy().to_string();

    let settings = TreeSitterSettings {
        search_paths: Some(vec![search_path.clone()]),
        languages: HashMap::new(),
        capture_mappings: HashMap::new(),
        auto_install: None,
        language_servers: None,
    };

    // Load settings into coordinator
    let workspace_settings = WorkspaceSettings::try_from_settings(&settings, None, |_| None)
        .expect("test settings should expand without errors");
    let _summary = coordinator.load_settings(workspace_settings);

    // Verify search paths are set
    let paths = coordinator.get_search_paths();
    assert!(
        paths.is_some(),
        "Search paths should be set after load_settings"
    );
    println!("Search paths: {:?}", paths);

    // Now ensure lua is loaded
    let result = coordinator.ensure_language_loaded("lua");

    println!("Load result success: {}", result.success);
    for event in &result.events {
        println!("  Event: {:?}", event);
    }

    assert!(
        result.success,
        "Lua should load successfully from {}",
        search_path
    );

    // Verify lua is now available
    assert!(
        coordinator.has_parser_available("lua"),
        "Lua should be registered in language registry"
    );
    assert!(
        coordinator.has_queries("lua"),
        "Lua should have highlight queries"
    );
}
