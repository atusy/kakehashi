use libloading::{Library, Symbol};
use path_clean::PathClean;
use std::collections::HashMap;
use std::error::Error;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use tree_sitter::Language;

use crate::error::LockResultExt;

/// Process-global cache of loaded grammar libraries, keyed by normalized path.
///
/// Entries are `&'static` on purpose: a `Language`, every tree parsed with it,
/// and every parser cached in thread-local storage (the semantic fan-out's
/// `PARSER_CACHE`) hold raw pointers into the mapped library, and none of
/// those references are tracked here. Dropping a `Library` runs `dlclose`,
/// turning all of them into dangling code pointers — the SIGSEGV class seen
/// when a test's `LanguageCoordinator` dropped its loader while the shared
/// compute pool's thread-local parsers still referenced its grammars. In
/// production the coordinator (and thus every loaded library) already lives
/// for the process lifetime, so immortality changes nothing there.
static LOADED_LIBRARIES: OnceLock<Mutex<HashMap<PathBuf, &'static Library>>> = OnceLock::new();

/// Load (or fetch the already-mapped) library at `normalized_path`, leaking it
/// into the process-global cache so it can never be unmapped.
fn immortal_library(normalized_path: &Path) -> Result<&'static Library, ParserLoadError> {
    // Canonicalize so symlinked/relative spellings of the same library file
    // share one leaked entry (PathClean alone is lexical); fall back to the
    // cleaned path when the file cannot be resolved — `Library::new` will
    // then report the real error.
    let key = normalized_path
        .canonicalize()
        .unwrap_or_else(|_| normalized_path.to_path_buf());
    let cache = LOADED_LIBRARIES.get_or_init(|| Mutex::new(HashMap::new()));
    let mut map = cache.lock().recover_poison("loader::immortal_library");
    if let Some(library) = map.get(&key) {
        return Ok(library);
    }
    let library: &'static Library = Box::leak(Box::new(unsafe { Library::new(&key)? }));
    map.insert(key, library);
    Ok(library)
}

/// A wrapper around dynamic library loading for Tree-sitter language parsers
#[derive(Default)]
pub(crate) struct ParserLoader {
    /// Per-loader name→library index into the process-global cache.
    loaded_libraries: HashMap<String, &'static Library>,
}

#[derive(Debug)]
pub(crate) enum ParserLoadError {
    LibraryLoadError(libloading::Error),
    SymbolNotFound(String),
    CacheError(String),
}

impl fmt::Display for ParserLoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ParserLoadError::LibraryLoadError(e) => write!(f, "Failed to load library: {e}"),
            ParserLoadError::SymbolNotFound(func) => write!(f, "Symbol not found: {func}"),
            ParserLoadError::CacheError(msg) => write!(f, "Cache error: {msg}"),
        }
    }
}

impl Error for ParserLoadError {}

impl From<libloading::Error> for ParserLoadError {
    fn from(err: libloading::Error) -> Self {
        ParserLoadError::LibraryLoadError(err)
    }
}

impl ParserLoader {
    /// Create a new ParserLoader instance
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Load a Tree-sitter language from a dynamic library.
    ///
    /// The exported symbol is derived as `tree_sitter_{lang_name}`, so `lang_name`
    /// must match the grammar's symbol, not necessarily the registry key.
    pub(crate) fn load_language(
        &mut self,
        path: &Path,
        lang_name: &str,
    ) -> Result<Language, ParserLoadError> {
        // Normalize the path before loading
        let normalized_path = path.clean();

        // Derive function name from language name using standard convention
        let func_name = format!("tree_sitter_{lang_name}");

        // Load the library if not already loaded
        if !self.loaded_libraries.contains_key(lang_name) {
            let library = immortal_library(&normalized_path)?;
            self.loaded_libraries.insert(lang_name.to_string(), library);
        }

        // Get the library from cache
        let library = *self.loaded_libraries.get(lang_name).ok_or_else(|| {
            ParserLoadError::CacheError(format!("Failed to get library for {lang_name}"))
        })?;

        // Get the language function from the library
        let language_fn: Symbol<unsafe extern "C" fn() -> Language> = unsafe {
            library
                .get(func_name.as_bytes())
                .map_err(|_| ParserLoadError::SymbolNotFound(func_name.clone()))?
        };

        // Call the function to get the Language
        let language = unsafe { language_fn() };

        Ok(language)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parser_loader_creation() {
        let loader = ParserLoader::new();
        assert!(loader.loaded_libraries.is_empty());
    }

    #[test]
    fn test_error_display() {
        let err = ParserLoadError::SymbolNotFound("tree_sitter_rust".to_string());
        assert_eq!(err.to_string(), "Symbol not found: tree_sitter_rust");

        let err = ParserLoadError::CacheError("test error".to_string());
        assert_eq!(err.to_string(), "Cache error: test error");
    }
}
