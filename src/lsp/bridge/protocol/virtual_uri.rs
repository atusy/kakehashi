//! Virtual document URI for injection regions.
//!
//! This module provides the `VirtualDocumentUri` type for encoding host URI,
//! injection language, and region ID into a URI that downstream language
//! servers can use to identify virtual documents.
//!
//! For most URIs (file://, https://, etc.), the virtual URI preserves the
//! original scheme and directory. For "cannot-be-a-base" URIs (untitled:,
//! mailto:, data:), a kakehashi:// scheme fallback is used.

/// Prefix used for virtual document filenames.
///
/// This distinctive prefix identifies virtual URIs and prevents collisions with real files.
const VIRTUAL_URI_PREFIX: &str = "kakehashi-virtual-uri-";

/// Virtual document URI for injection regions.
///
/// Encodes host URI + injection language + region ID into a URI that
/// downstream language servers can use to identify virtual documents.
///
/// ## URI Format
///
/// For normal URIs (file://, https://, etc.):
/// - Format: `{scheme}:///{host_dir}/kakehashi-virtual-uri-{region_id}.{ext}`
/// - Example: `file:///project/docs/kakehashi-virtual-uri-01ARZ3NDEKTSV4.lua`
///
/// For cannot-be-a-base URIs (untitled:, mailto:, data:):
/// - Format: `kakehashi:///virtual/{encoded_host}/kakehashi-virtual-uri-{region_id}.{ext}`
/// - Example: `kakehashi:///virtual/untitled%3AUntitled-1/kakehashi-virtual-uri-REGION.lua`
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct VirtualDocumentUri {
    host_uri: tower_lsp_server::ls_types::Uri,
    language: String,
    region_id: String,
}

impl VirtualDocumentUri {
    /// Create a new virtual document URI for an injection region.
    ///
    /// # Arguments
    /// * `host_uri` - The URI of the host document (e.g., markdown file)
    /// * `language` - The injection language (e.g., "lua", "python"). Must not be empty.
    /// * `region_id` - Unique identifier for this injection region within the host. Must not be empty.
    ///
    /// # Panics (debug builds only)
    /// Panics if `language` or `region_id` is empty. These are programming errors
    /// as callers should always provide valid identifiers.
    ///
    /// # Upstream Guarantees
    /// In practice, these parameters are guaranteed valid by upstream sources:
    /// - `region_id` comes from ULID generation (26-char alphanumeric strings)
    /// - `language` comes from Tree-sitter injection queries (non-empty language names)
    ///
    /// In release builds, invalid inputs are accepted without validation to avoid
    /// runtime overhead. Unknown languages produce `.txt` extensions as a safe fallback.
    pub(crate) fn new(
        host_uri: &tower_lsp_server::ls_types::Uri,
        language: &str,
        region_id: &str,
    ) -> Self {
        debug_assert!(!language.is_empty(), "language must not be empty");
        debug_assert!(!region_id.is_empty(), "region_id must not be empty");

        Self {
            host_uri: host_uri.clone(),
            language: language.to_string(),
            region_id: region_id.to_string(),
        }
    }

    /// Get the region_id.
    pub(crate) fn region_id(&self) -> &str {
        &self.region_id
    }

    /// Get the language.
    pub(crate) fn language(&self) -> &str {
        &self.language
    }

    /// Check if a URI string represents a virtual document.
    ///
    /// Virtual document URIs have the filename pattern `kakehashi-virtual-uri-{region_id}.{ext}`.
    /// This is used to distinguish virtual URIs from real file URIs in responses.
    ///
    /// Checks the filename (not path) for the `kakehashi-virtual-uri-` prefix with at least
    /// one `.` for the extension. The prefix is distinctive enough to avoid false positives
    /// with real files.
    ///
    /// Uses proper URL parsing to handle edge cases like query strings containing slashes
    /// (e.g., `?path=/foo/bar`) which would confuse naive string splitting.
    pub(crate) fn is_virtual_uri(uri: &str) -> bool {
        // Parse URI using the url crate for proper RFC 3986 handling
        let Ok(url) = url::Url::parse(uri) else {
            return false;
        };

        // Extract the last path segment (filename)
        let Some(filename) = url.path_segments().and_then(|mut s| s.next_back()) else {
            return false;
        };

        // Check for {VIRTUAL_URI_PREFIX}{region_id}.{ext} pattern
        // Requires non-empty extension after the last dot
        filename.starts_with(VIRTUAL_URI_PREFIX)
            && filename
                .get(VIRTUAL_URI_PREFIX.len()..)
                .and_then(|s| s.rsplit_once('.'))
                .is_some_and(|(_name, ext)| !ext.is_empty())
    }

    /// Convert to a URI string.
    ///
    /// Format: `file:///{host_dir}/kakehashi-virtual-uri-{region_id}.{ext}`
    ///
    /// Uses file:// scheme placing the virtual file in the same directory as the host
    /// document. This enables downstream language servers to:
    /// - Resolve relative imports (e.g., `from .utils import foo`)
    /// - Find project configuration files (pyproject.toml, tsconfig.json, etc.)
    ///
    /// The `kakehashi-virtual-uri-` prefix is distinctive and unlikely to conflict with
    /// real files. The region_id (ULID) provides global uniqueness.
    ///
    /// The file extension is derived from the language to help downstream language servers
    /// recognize the file type (e.g., lua-language-server needs `.lua` extension).
    ///
    /// The region_id is percent-encoded by the url crate to ensure URI-safe characters.
    /// While ULIDs only contain alphanumeric characters, this provides defense-in-depth.
    pub(crate) fn to_uri_string(&self) -> String {
        let extension = Self::language_to_extension(&self.language);
        let virtual_filename = format!("{VIRTUAL_URI_PREFIX}{}.{extension}", self.region_id);

        // Try to parse and modify the host URI (works for file://, https://, etc.)
        if let Ok(mut url) = url::Url::parse(self.host_uri.as_str()) {
            // path_segments_mut() returns Err for cannot-be-a-base URIs (untitled:, mailto:, data:)
            let modified = url
                .path_segments_mut()
                .map(|mut segments| {
                    segments.pop(); // Remove the host filename
                    segments.push(&virtual_filename); // Add the virtual filename
                })
                .is_ok();

            if modified {
                return url.to_string();
            }
        }

        // Fallback for cannot-be-a-base URIs or parse errors
        // Use kakehashi:// scheme with encoded host URI for traceability
        let encoded_host = percent_encoding::utf8_percent_encode(
            self.host_uri.as_str(),
            percent_encoding::NON_ALPHANUMERIC,
        );
        format!("kakehashi:///virtual/{encoded_host}/{virtual_filename}")
    }

    /// Map language name to file extension.
    ///
    /// Downstream language servers (e.g., lua-language-server) often use file extension
    /// to determine file type and enable appropriate language features.
    ///
    /// # Returns
    /// The file extension (without leading dot) for the given language.
    /// For unknown languages, returns the language name itself as the extension,
    /// which works for many languages where the name matches the extension
    /// (e.g., "zig" → ".zig", "nix" → ".nix").
    fn language_to_extension(language: &str) -> &str {
        match language {
            "python" => "py",
            "rust" => "rs",
            "javascript" => "js",
            "typescript" => "ts",
            "ruby" => "rb",
            "kotlin" => "kt",
            "haskell" => "hs",
            "ocaml" => "ml",
            "elixir" => "ex",
            "erlang" => "erl",
            "clojure" => "clj",
            "julia" => "jl",
            "markdown" => "md",
            "powershell" => "ps1",
            _ => language, // Use language name as extension
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;
    use tower_lsp_server::ls_types::Uri;
    use url::Url;

    // Helper function to convert url::Url to tower_lsp_server::ls_types::Uri for tests
    fn url_to_uri(url: &Url) -> Uri {
        crate::lsp::lsp_impl::url_to_uri(url).expect("test URL should convert to URI")
    }

    // ==========================================================================
    // to_uri_string tests
    // ==========================================================================

    #[test]
    fn to_uri_string_uses_host_directory() {
        let host_uri = Url::parse("file:///project/docs/README.md").unwrap();
        let virtual_uri = VirtualDocumentUri::new(&url_to_uri(&host_uri), "lua", "01ARZ3NDEKTSV4");

        let uri_string = virtual_uri.to_uri_string();

        // Should be in same directory as host, with kakehashi-virtual-uri- prefix
        assert!(
            uri_string.starts_with("file:///project/docs/kakehashi-virtual-uri-"),
            "URI should be in host directory with kakehashi-virtual-uri- prefix: {}",
            uri_string
        );
        assert!(
            uri_string.ends_with(".lua"),
            "URI should have .lua extension: {}",
            uri_string
        );
        // Verify full format
        assert_eq!(
            uri_string, "file:///project/docs/kakehashi-virtual-uri-01ARZ3NDEKTSV4.lua",
            "URI should follow format: file:///<host_dir>/kakehashi-virtual-uri-<region_id>.<ext>"
        );
    }

    #[test]
    fn to_uri_string_root_level_host() {
        let host_uri = Url::parse("file:///README.md").unwrap();
        let virtual_uri = VirtualDocumentUri::new(&url_to_uri(&host_uri), "python", "REGION123");

        let uri_string = virtual_uri.to_uri_string();

        assert_eq!(
            uri_string, "file:///kakehashi-virtual-uri-REGION123.py",
            "Root-level host should produce root-level virtual URI"
        );
    }

    #[test]
    fn to_uri_string_preserves_https_scheme() {
        // https:// URIs should preserve scheme and path structure
        let host_uri = Url::parse("https://example.com/path/to/file.md").unwrap();
        let virtual_uri = VirtualDocumentUri::new(&url_to_uri(&host_uri), "lua", "REGION1");

        let uri_string = virtual_uri.to_uri_string();

        assert!(
            uri_string.starts_with("https://example.com/path/to/kakehashi-virtual-uri-"),
            "https:// URI should preserve scheme and host: {}",
            uri_string
        );
        assert!(
            uri_string.ends_with(".lua"),
            "https:// URI should have language extension: {}",
            uri_string
        );
    }

    #[test]
    fn to_uri_string_preserves_custom_scheme_with_authority() {
        // Custom schemes with authority (special:///) should work like file://
        let host_uri: Uri = "special:///path/to/file.md".parse().unwrap();
        let virtual_uri = VirtualDocumentUri::new(&host_uri, "python", "REGION2");

        let uri_string = virtual_uri.to_uri_string();

        assert!(
            uri_string.starts_with("special:///path/to/kakehashi-virtual-uri-"),
            "Custom scheme with authority should preserve scheme: {}",
            uri_string
        );
        assert!(
            uri_string.ends_with(".py"),
            "Custom scheme URI should have language extension: {}",
            uri_string
        );
    }

    #[test]
    fn to_uri_string_preserves_vscode_notebook_cell_scheme() {
        // vscode-notebook-cell:// is used by VSCode for notebook cells
        let host_uri: Uri = "vscode-notebook-cell://authority/path/to/notebook.ipynb#cell-id"
            .parse()
            .unwrap();
        let virtual_uri = VirtualDocumentUri::new(&host_uri, "python", "REGION3");

        let uri_string = virtual_uri.to_uri_string();

        assert!(
            uri_string
                .starts_with("vscode-notebook-cell://authority/path/to/kakehashi-virtual-uri-"),
            "vscode-notebook-cell:// should preserve scheme and authority: {}",
            uri_string
        );
        // Note: fragments are preserved by the url crate, so the URI ends with ".py#cell-id"
        assert!(
            uri_string.contains(".py"),
            "vscode-notebook-cell:// URI should have language extension: {}",
            uri_string
        );
        // Verify fragment is preserved (this is url crate behavior)
        assert!(
            uri_string.ends_with("#cell-id"),
            "Fragment should be preserved: {}",
            uri_string
        );
    }

    #[test]
    fn to_uri_string_preserves_percent_encoding_in_path() {
        let host_uri = Url::parse("file:///my%20project/docs/file.md").unwrap();
        let virtual_uri = VirtualDocumentUri::new(&url_to_uri(&host_uri), "lua", "REGION1");

        let uri_string = virtual_uri.to_uri_string();

        assert!(
            uri_string.starts_with("file:///my%20project/docs/kakehashi-virtual-uri-"),
            "URI should preserve percent-encoding in path: {}",
            uri_string
        );
    }

    #[test]
    fn includes_language_extension() {
        let host_uri = Url::parse("file:///project/doc.md").unwrap();
        let virtual_uri = VirtualDocumentUri::new(&url_to_uri(&host_uri), "lua", "region-0");

        let uri_string = virtual_uri.to_uri_string();
        assert!(
            uri_string.ends_with(".lua"),
            "URI should have .lua extension: {}",
            uri_string
        );
    }

    #[test]
    fn region_id_accessor_returns_stored_value() {
        let host_uri = Url::parse("file:///project/doc.md").unwrap();
        let virtual_uri =
            VirtualDocumentUri::new(&url_to_uri(&host_uri), "lua", "01ARZ3NDEKTSV4RRFFQ69G5FAV");

        assert_eq!(virtual_uri.region_id(), "01ARZ3NDEKTSV4RRFFQ69G5FAV");
    }

    #[test]
    fn language_accessor_returns_stored_value() {
        let host_uri = Url::parse("file:///project/doc.md").unwrap();
        let virtual_uri = VirtualDocumentUri::new(&url_to_uri(&host_uri), "python", "region-0");

        assert_eq!(virtual_uri.language(), "python");
    }

    #[test]
    fn percent_encodes_special_characters_in_region_id() {
        let host_uri = Url::parse("file:///project/doc.md").unwrap();
        // Test with characters that need encoding: space, slash, question mark
        let virtual_uri = VirtualDocumentUri::new(&url_to_uri(&host_uri), "lua", "region/0?test");

        let uri_string = virtual_uri.to_uri_string();
        // "/" should be encoded as %2F, "?" should be encoded as %3F
        assert!(
            uri_string.contains("region%2F0%3Ftest"),
            "Special characters should be percent-encoded: {}",
            uri_string
        );
    }

    #[test]
    fn preserves_alphanumeric_and_safe_chars_in_region_id() {
        let host_uri = Url::parse("file:///project/doc.md").unwrap();
        // RFC 3986 unreserved characters: A-Z a-z 0-9 - . _ ~
        let virtual_uri =
            VirtualDocumentUri::new(&url_to_uri(&host_uri), "lua", "ABC-xyz_123.test~v2");

        let uri_string = virtual_uri.to_uri_string();
        assert!(
            uri_string.contains("ABC-xyz_123.test~v2.lua"),
            "Unreserved characters should not be encoded: {}",
            uri_string
        );
    }

    #[test]
    fn same_inputs_produce_same_output() {
        let host_uri = Url::parse("file:///project/doc.md").unwrap();
        let uri1 = VirtualDocumentUri::new(&url_to_uri(&host_uri), "lua", "region-0");
        let uri2 = VirtualDocumentUri::new(&url_to_uri(&host_uri), "lua", "region-0");

        assert_eq!(
            uri1.to_uri_string(),
            uri2.to_uri_string(),
            "Same inputs should produce deterministic output"
        );
    }

    #[test]
    fn different_region_ids_produce_different_uris() {
        let host_uri = Url::parse("file:///project/doc.md").unwrap();
        let uri1 = VirtualDocumentUri::new(&url_to_uri(&host_uri), "lua", "region-0");
        let uri2 = VirtualDocumentUri::new(&url_to_uri(&host_uri), "lua", "region-1");

        assert_ne!(
            uri1.to_uri_string(),
            uri2.to_uri_string(),
            "Different region_ids should produce different URIs"
        );
    }

    #[test]
    fn different_languages_produce_different_extensions() {
        let host_uri = Url::parse("file:///project/doc.md").unwrap();
        let lua_uri = VirtualDocumentUri::new(&url_to_uri(&host_uri), "lua", "region-0");
        let python_uri = VirtualDocumentUri::new(&url_to_uri(&host_uri), "python", "region-0");

        assert!(lua_uri.to_uri_string().ends_with(".lua"));
        assert!(python_uri.to_uri_string().ends_with(".py"));
    }

    #[test]
    fn language_to_extension_maps_common_languages() {
        // Test a representative sample of the supported languages
        let test_cases = [
            ("lua", "lua"),
            ("python", "py"),
            ("rust", "rs"),
            ("javascript", "js"),
            ("typescript", "ts"),
            ("go", "go"),
            ("c", "c"),
            ("cpp", "cpp"),
            ("java", "java"),
            ("ruby", "rb"),
            ("bash", "bash"),
            ("sh", "sh"),
        ];

        let host_uri = Url::parse("file:///project/doc.md").unwrap();

        for (language, expected_ext) in test_cases {
            let uri = VirtualDocumentUri::new(&url_to_uri(&host_uri), language, "region-0");
            let uri_string = uri.to_uri_string();
            assert!(
                uri_string.ends_with(&format!(".{}", expected_ext)),
                "Language '{}' should produce extension '{}', got: {}",
                language,
                expected_ext,
                uri_string
            );
        }
    }

    #[test]
    fn language_to_extension_uses_language_name_for_unknown() {
        let host_uri = Url::parse("file:///project/doc.md").unwrap();

        // Unknown languages should use the language name as extension
        let unknown_cases = ["zig", "nix", "foobar"];

        for language in unknown_cases {
            let uri = VirtualDocumentUri::new(&url_to_uri(&host_uri), language, "region-0");
            let uri_string = uri.to_uri_string();
            assert!(
                uri_string.ends_with(&format!(".{}", language)),
                "Unknown language '{}' should use itself as extension, got: {}",
                language,
                uri_string
            );
        }
    }

    #[test]
    fn same_region_id_in_same_directory_produces_identical_uri() {
        // VirtualDocumentUri uses only the directory from host_uri, not the filename.
        // This test verifies URI generation is deterministic: identical (directory,
        // language, region_id) inputs produce identical output, regardless of which
        // host document they came from.
        //
        // Note: In practice, region_ids are ULIDs unique per (host_uri, position_key),
        // so two different hosts would never share a region_id. This tests the
        // struct's deterministic behavior in isolation.
        let host1 = Url::parse("file:///project/doc1.md").unwrap();
        let host2 = Url::parse("file:///project/doc2.md").unwrap();
        let uri1 = VirtualDocumentUri::new(&url_to_uri(&host1), "lua", "region-0");
        let uri2 = VirtualDocumentUri::new(&url_to_uri(&host2), "lua", "region-0");

        assert_eq!(
            uri1.to_uri_string(),
            uri2.to_uri_string(),
            "Same (directory, language, region_id) should produce identical URI"
        );
    }

    #[test]
    fn different_directories_produce_different_virtual_uris() {
        let host1 = Url::parse("file:///project/dir1/doc.md").unwrap();
        let host2 = Url::parse("file:///project/dir2/doc.md").unwrap();
        let uri1 = VirtualDocumentUri::new(&url_to_uri(&host1), "lua", "region-0");
        let uri2 = VirtualDocumentUri::new(&url_to_uri(&host2), "lua", "region-0");

        assert_ne!(
            uri1.to_uri_string(),
            uri2.to_uri_string(),
            "Different host directories should produce different virtual URIs"
        );
    }

    #[test]
    fn equality_checks_all_fields() {
        let host1 = Url::parse("file:///project/doc1.md").unwrap();
        let host2 = Url::parse("file:///project/doc2.md").unwrap();

        let uri1 = VirtualDocumentUri::new(&url_to_uri(&host1), "lua", "region-0");
        let uri2 = VirtualDocumentUri::new(&url_to_uri(&host1), "lua", "region-0");
        let uri3 = VirtualDocumentUri::new(&url_to_uri(&host2), "lua", "region-0");
        let uri4 = VirtualDocumentUri::new(&url_to_uri(&host1), "python", "region-0");
        let uri5 = VirtualDocumentUri::new(&url_to_uri(&host1), "lua", "region-1");

        assert_eq!(uri1, uri2, "Same fields should be equal");
        assert_ne!(uri1, uri3, "Different host_uri should not be equal");
        assert_ne!(uri1, uri4, "Different language should not be equal");
        assert_ne!(uri1, uri5, "Different region_id should not be equal");
    }

    #[test]
    #[should_panic(expected = "language must not be empty")]
    #[cfg(debug_assertions)]
    fn panics_on_empty_language_in_debug() {
        let host_uri = Url::parse("file:///project/doc.md").unwrap();
        let _ = VirtualDocumentUri::new(&url_to_uri(&host_uri), "", "region-0");
    }

    #[test]
    #[should_panic(expected = "region_id must not be empty")]
    #[cfg(debug_assertions)]
    fn panics_on_empty_region_id_in_debug() {
        let host_uri = Url::parse("file:///project/doc.md").unwrap();
        let _ = VirtualDocumentUri::new(&url_to_uri(&host_uri), "lua", "");
    }

    #[test]
    fn to_uri_string_contains_region_id_in_filename() {
        // Verify that the region_id appears in the URI
        // Format: kakehashi-virtual-uri-{region_id}.{ext}
        let host_uri = Url::parse("file:///project/doc.md").unwrap();
        let region_id = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
        let virtual_uri = VirtualDocumentUri::new(&url_to_uri(&host_uri), "lua", region_id);

        let uri_string = virtual_uri.to_uri_string();

        // Extract filename from the URI path
        let filename = uri_string.rsplit('/').next().unwrap();
        // Filename format: kakehashi-virtual-uri-{region_id}.{ext}
        // Remove kakehashi-virtual-uri- prefix and .ext suffix
        let without_prefix = filename.strip_prefix("kakehashi-virtual-uri-").unwrap();
        let extracted_id = without_prefix
            .rsplit_once('.')
            .map(|(name, _)| name)
            .unwrap();

        assert_eq!(
            extracted_id, region_id,
            "Region ID should be extractable from URI"
        );
    }

    #[test]
    fn to_uri_string_produces_valid_uri() {
        let host_uri = Url::parse("file:///project/doc.md").unwrap();
        let virtual_uri = VirtualDocumentUri::new(&url_to_uri(&host_uri), "lua", "region-0");

        let uri_string = virtual_uri.to_uri_string();

        // Verify the output is a valid URL
        let parsed = Url::parse(&uri_string);
        assert!(
            parsed.is_ok(),
            "to_uri_string() should produce a valid URL: {}",
            uri_string
        );

        let parsed = parsed.unwrap();
        assert_eq!(parsed.scheme(), "file");
        // Path should be in host directory with kakehashi-virtual-uri- prefix filename
        let filename = parsed.path().rsplit('/').next().unwrap();
        assert!(
            filename.starts_with("kakehashi-virtual-uri-"),
            "Filename should start with kakehashi-virtual-uri-: {}",
            filename
        );
    }

    // ==========================================================================
    // is_virtual_uri tests
    // ==========================================================================

    #[rstest]
    // Detects valid virtual URIs
    #[case::new_format_lua("file:///project/kakehashi-virtual-uri-01ARZ3NDEK.lua", true)]
    #[case::new_format_py("file:///project/docs/kakehashi-virtual-uri-REGION123.py", true)]
    #[case::new_format_root("file:///kakehashi-virtual-uri-01JPMQ8ZYYQA.txt", true)]
    #[case::percent_encoded_region("file:///project/kakehashi-virtual-uri-region%2F0.lua", true)]
    #[case::with_query_string("file:///project/kakehashi-virtual-uri-REGION.lua?query=value", true)]
    #[case::with_fragment("file:///project/kakehashi-virtual-uri-REGION.lua#section", true)]
    #[case::with_query_and_fragment(
        "file:///project/kakehashi-virtual-uri-REGION.lua?query=value#section",
        true
    )]
    #[case::query_with_slash(
        "file:///project/kakehashi-virtual-uri-REGION.lua?path=/foo/bar",
        true
    )]
    #[case::percent_encoded_slash_in_filename(
        "file:///project/kakehashi-virtual-uri-region%2Fsubregion.lua",
        true
    )]
    // Rejects real URIs and malformed input
    #[case::real_lua_file("file:///home/user/project/main.lua", false)]
    #[case::real_py_file("file:///C:/Users/dev/code.py", false)]
    #[case::untitled_scheme("untitled:Untitled-1", false)]
    #[case::kakehashi_in_dir("file:///home/.kakehashi/config.lua", false)]
    #[case::kakehashi_prefix_no_virtual("file:///project/kakehashi.config.lua", false)]
    #[case::no_extension("file:///project/kakehashi-virtual-uri-lua", false)]
    #[case::not_a_url("not a url", false)]
    #[case::empty_string("", false)]
    #[case::missing_scheme("://missing-scheme", false)]
    fn is_virtual_uri(#[case] uri: &str, #[case] expected: bool) {
        assert_eq!(VirtualDocumentUri::is_virtual_uri(uri), expected);
    }

    // ==========================================================================
    // cannot-be-a-base URI fallback tests
    // ==========================================================================

    #[test]
    fn to_uri_string_falls_back_to_kakehashi_scheme_for_cannot_be_a_base_uri() {
        // "untitled:Untitled-1" is a cannot-be-a-base URI (no authority, opaque path)
        // These are used by VSCode for unsaved files
        let host_uri: Uri = "untitled:Untitled-1".parse().unwrap();
        let virtual_uri = VirtualDocumentUri::new(&host_uri, "lua", "REGION123");

        let uri_string = virtual_uri.to_uri_string();

        // Should use kakehashi:// scheme as fallback
        assert!(
            uri_string.starts_with("kakehashi:///virtual/"),
            "Cannot-be-a-base URI should fall back to kakehashi:// scheme: {}",
            uri_string
        );
        // Should contain encoded host URI for traceability
        assert!(
            uri_string.contains("untitled"),
            "Fallback URI should contain encoded host URI: {}",
            uri_string
        );
        // Should end with proper extension
        assert!(
            uri_string.ends_with(".lua"),
            "Fallback URI should have language extension: {}",
            uri_string
        );
        // Should contain the virtual filename with region_id
        assert!(
            uri_string.contains("kakehashi-virtual-uri-REGION123"),
            "Fallback URI should contain virtual filename: {}",
            uri_string
        );
    }

    #[test]
    fn to_uri_string_fallback_produces_valid_uri() {
        let host_uri: Uri = "untitled:Untitled-1".parse().unwrap();
        let virtual_uri = VirtualDocumentUri::new(&host_uri, "python", "REGION456");

        let uri_string = virtual_uri.to_uri_string();

        // Verify the output is a valid URL that can be parsed
        let parsed = Url::parse(&uri_string);
        assert!(
            parsed.is_ok(),
            "Fallback URI should be a valid URL: {}",
            uri_string
        );

        let parsed = parsed.unwrap();
        assert_eq!(parsed.scheme(), "kakehashi");
    }

    #[test]
    fn to_uri_string_fallback_is_detected_as_virtual_uri() {
        let host_uri: Uri = "untitled:Untitled-1".parse().unwrap();
        let virtual_uri = VirtualDocumentUri::new(&host_uri, "lua", "REGION789");

        let uri_string = virtual_uri.to_uri_string();

        // The fallback URI should still be detected by is_virtual_uri
        assert!(
            VirtualDocumentUri::is_virtual_uri(&uri_string),
            "Fallback URI should be detected as virtual: {}",
            uri_string
        );
    }
}
