# ADR-0005: Language Detection Fallback Chain

## Status

Accepted (Supersedes [ADR-0002](0002-filetype-detection-via-extension-mapping.md)). Alias resolution (`aliases` field, `build_alias_map()`, `resolve_alias()`) is superseded by [ADR-0024](0024-base-language-inheritance.md)'s `base` field. The detection fallback chain (languageId → token → first-line) and syntect normalization remain unchanged.

## Context

ADR-0002 established extension-based document-level language detection as the primary method with LSP languageId as fallback. However, this approach has limitations:

1. **LSP clients are authoritative**: Modern LSP clients (VS Code, Neovim, etc.) already perform sophisticated language detection and send accurate `languageId` values
2. **Extension mapping is redundant**: Duplicating what clients already do creates maintenance burden and potential conflicts
3. **Missing heuristic layer**: Files without extensions (e.g., `Dockerfile`, scripts with shebangs) aren't handled well

Additionally, PBI-061 removed the `filetypes` configuration field entirely, eliminating the ability to configure extension mappings in the server. This forces a rethinking of the detection strategy.

The key insight is: **detection should find an *available* Tree-sitter parser, not just identify a language name**. If the detected language has no parser loaded, detection should continue to the next method.

This applies to both document-level language detection and injected language resolution (e.g., code blocks in Markdown).

## Decision

**Implement a fallback chain that continues until an available Tree-sitter parser is found.** This applies at two levels:

1. **Document-level**: Detecting the primary language when a file is opened
2. **Injection-level**: Resolving embedded languages within a parsed document

```
1. LSP languageId  →  Try direct  →  Try alias  →  If available: use it
                                                →  If no: continue
2. Token detection →  syntect     →  Try alias  →  If available: use it
                  →  raw token   →  Try alias  →  If available: use it
                                                →  If no: continue
3. First line      →  Try direct  →  Try alias  →  If available: use it
                                                →  If no: return None
```

### Priority Order Rationale

Each detection method follows the **detect → alias resolution → availability check** pattern:

1. **LSP languageId (highest priority)**
   - Client has full context: file path, content, user preferences, workspace settings
   - Already handles complex cases: `.tsx` vs `.ts`, polyglot files, user overrides
   - Trust the client—it knows best

2. **Token-based detection (middle priority)**

   Tokens are extracted from either explicit identifiers (code fence markers) or file paths:
   - **Explicit token**: Injection identifiers like `py`, `js`, `bash` from code fences
   - **Path-derived token**: Extension (`file.rs` → `rs`) or basename (`Makefile` → `Makefile`)

   Token resolution uses syntect's `find_syntax_by_token` for normalization:
   - `py` → `python`, `js` → `javascript`, `rs` → `rust`
   - `Makefile` → `make`, `.bashrc` → `bash`

   If syntect doesn't recognize the token, it's tried directly as an alias candidate.
   This handles extensions like `jsx`, `tsx` that syntect doesn't know but may be
   configured as aliases (e.g., `jsx` → `javascript`).

3. **First-line detection (lowest priority)**
   - Shebang detection: `#!/usr/bin/env python` → python
   - Magic comments: `# -*- mode: ruby -*-` → ruby
   - Implementation: syntect's `find_syntax_by_first_line`
   - Fallback when token detection fails (e.g., extensionless files without special names)

### Alias Resolution as Sub-step

Alias resolution is applied **after each detection method**, not as a separate step in the chain. This is configured via the `aliases` field in language config:

```toml
[languages.markdown]
aliases = ["rmd", "qmd"]
```

This ensures:
- **Consistent behavior**: All detection paths apply the same alias logic
- **User control**: Users can define mappings that work at any detection level
- **Alignment with injection**: Document-level and injection-level detection behave the same way

Example scenarios:
- Editor sends `languageId: "rmd"` → alias resolves to `markdown` → parser found
- Token `py` (from code fence or `.py` extension) → syntect normalizes to `python` → parser found
- Token `jsx` (from `.jsx` extension) → syntect unknown → direct alias to `javascript` → parser found
- Shebang `#!/usr/bin/env python3` → syntect returns `python` → parser found

### Availability Check

Each detection method tries direct match first, then alias resolution:

```
detect_language(path, content, token, language_id):
    // 1. Try languageId (skip "plaintext")
    if language_id exists and != "plaintext":
        if try_with_alias_fallback(language_id) succeeds:
            return result

    // 2. Token-based detection
    effective_token = token OR extract_token_from_path(path)
    if effective_token exists:
        // Try syntect normalization (py → python, Makefile → make)
        if syntect recognizes token:
            if try_with_alias_fallback(normalized) succeeds:
                return result
        // Try raw token as alias (handles jsx, tsx that syntect doesn't know)
        if try_with_alias_fallback(raw_token) succeeds:
            return result

    // 3. First-line detection (shebang, mode line)
    if syntect detects from first line:
        if try_with_alias_fallback(detected) succeeds:
            return result

    return None

try_with_alias_fallback(candidate):
    if parser_available(candidate):
        return candidate
    if alias_configured(candidate) AND parser_available(alias):
        return alias
    return None
```

This means:
- If client sends `languageId: "rmd"` and alias maps `rmd` → `markdown`, use the markdown parser
- If token `py` is normalized by syntect to `python`, use the python parser
- If token `jsx` is not recognized by syntect but alias maps `jsx` → `javascript`, use the javascript parser
- If no match, continue to the next detection method

### Language Injection

The fallback chain also applies to **injected languages** (e.g., code blocks in Markdown, JavaScript inside HTML). Injection queries extract a language identifier, but this identifier needs resolution:

```
Document (markdown) ──parse──▶ AST ──injection query──▶ "py" ──detect──▶ python
                                                      ▶ "sh" ──detect──▶ bash
```

For example, a Markdown code fence with ` ```py ` provides the identifier `"py"`, which must be resolved to an available parser. This resolution follows a fallback pattern:

1. **Try the identifier directly**: Check if a parser named `"py"` is available
2. **Normalize via syntect**: Use `detect_from_token("py")` which returns `"python"`
3. **Try config-based alias**: If syntect doesn't recognize it, check user-configured aliases
4. **Skip if unavailable**: If no parser matches, the region is skipped

This means:
- Injected languages benefit from the same graceful degradation
- A Markdown file can have some code blocks with semantic tokens and others without, depending on installed parsers
- Token normalization via syntect handles common aliases (`py`, `js`, `sh`) automatically

## Consequences

### Positive

- **Respects client authority**: LSP clients invest heavily in language detection
- **No configuration needed**: Works out of the box without `filetypes` mapping
- **Graceful degradation**: Missing parsers don't block detection entirely
- **Handles edge cases**: Shebangs, magic comments, extensionless files
- **Simpler configuration**: Removed redundant `filetypes` field (PBI-061)

### Negative

- **Heuristic overhead**: Reading file content for shebang detection adds I/O
- **Non-deterministic**: Same file might use different parsers on different systems (based on available parsers)
- **Heuristic maintenance**: Shebang patterns need ongoing updates
- **languageId naming variance**: Clients may send languageIds that differ from parser names (e.g., `shellscript` vs `bash`); normalization may be needed later

### Neutral

- **Token-based detection includes extensions**: Extensions are treated as tokens, not a separate detection step
- **Parser availability matters**: Detection result depends on what's installed
- **Auto-install interaction**: Detection completes first (returning None if no parser found); auto-install runs asynchronously afterward, making the parser available for subsequent requests
- **Caching**: Detection result is stored per-document; cache invalidates on content change or `languageId` change from client
- **syntect dependency**: Uses syntect's Sublime Text syntax definitions for token normalization and first-line detection

## Migration from ADR-0002

The `filetypes` configuration field has been removed (PBI-061). Users who relied on custom extension mappings should:

1. Configure their LSP client to send the correct `languageId`
2. Use file associations in their editor (e.g., VS Code's `files.associations`)
3. Add shebangs or magic comments to extensionless files

This aligns with the principle: **configure at the source (client), not the sink (server)**.
