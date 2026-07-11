# Language Detection Fallback Chain

> The `aliases` field (`build_alias_map()`, `resolve_alias()`) is superseded by the `base` field (see base-language-inheritance); the detection fallback chain (languageId → token → first-line) and syntect normalization remain unchanged.

## Context

An earlier decision established extension-based document-level language detection as the primary method with LSP languageId as fallback. However, this approach has limitations:

1. **LSP clients are authoritative**: Modern LSP clients (VS Code, Neovim, etc.) already perform sophisticated language detection and send accurate `languageId` values
2. **Extension mapping is redundant**: Duplicating what clients already do creates maintenance burden and potential conflicts
3. **Missing heuristic layer**: Files without extensions (e.g., `Dockerfile`, scripts with shebangs) aren't handled well

Additionally, PBI-061 removed the `filetypes` configuration field entirely, eliminating the ability to configure extension mappings in the server. This forces a rethinking of the detection strategy.

The key insight is: **detection should find an *available* Tree-sitter parser, not just identify a language name**. If the detected language has no parser loaded, detection should continue to the next method.

This applies to parser selection for both document-level language detection and
injected-language analysis. Bridge selection has a separate stability
constraint: its canonical language key and virtual URI must be known before a
parser is installed and must not change when parser availability changes.

## Decision

**Implement a fallback chain that continues until an available Tree-sitter parser is found.** This applies at two levels:

1. **Document-level**: Detecting the primary language when a file is opened
2. **Injection-level**: Resolving embedded languages within a parsed document

For bridge routing and virtual-document identity, first derive a
parser-independent canonical injection candidate. Parser loading remains a
later, separate decision.

```
1. LSP languageId  →  Try direct  →  Try base   →  If available: use it
                                                →  If no: continue
2. Token detection →  syntect     →  Try base   →  If available: use it
                  →  raw token   →  Try base   →  If available: use it
                                                →  If no: continue
3. First line      →  Try direct  →  Try base   →  If available: use it
                                                →  If no: return None
```

### Priority Order Rationale

Each detection method follows the **detect → base resolution → availability check** pattern:

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

   If syntect doesn't recognize the token, it's tried directly as a base candidate.
   This handles extensions like `jsx`, `tsx` that syntect doesn't know but may be
   configured with a base (e.g., `[languages.jsx] base = "javascript"`).

3. **First-line detection (lowest priority)**
   - Shebang detection: `#!/usr/bin/env python` → python
   - Magic comments: `# -*- mode: ruby -*-` → ruby
   - Implementation: syntect's `find_syntax_by_first_line`
   - Fallback when token detection fails (e.g., extensionless files without special names)

### Base Resolution as Sub-step

Base resolution is applied **after each detection method**, not as a separate
step in the chain. This is configured via the `base` field:

```toml
[languages.rmd]
base = "markdown"
```

This ensures:
- **Consistent behavior**: All detection paths apply the same base logic
- **User control**: Users can define mappings that work at any detection level
- **Alignment with injection**: Document-level and injection-level detection behave the same way

Example scenarios:
- Editor sends `languageId: "rmd"` → base resolves to `markdown` → parser found
- Token `py` (from code fence or `.py` extension) → syntect normalizes to `python` → parser found
- Token `jsx` (from `.jsx` extension) → syntect unknown → configured base `javascript` → parser found
- Shebang `#!/usr/bin/env python3` → syntect returns `python` → parser found

### Availability Check

Each detection method tries a direct match first, then base resolution:

```
detect_language(path, content, token, language_id):
    // 1. Try languageId (skip "plaintext")
    if language_id exists and != "plaintext":
        if try_with_base_fallback(language_id) succeeds:
            return result

    // 2. Token-based detection
    effective_token = token OR extract_token_from_path(path)
    if effective_token exists:
        // Try syntect normalization (py → python, Makefile → make)
        if syntect recognizes effective_token:
            if try_with_base_fallback(normalized) succeeds:
                return result
        // Try raw token with base fallback (handles jsx, tsx)
        if try_with_base_fallback(effective_token) succeeds:
            return result

    // 3. First-line detection (shebang, mode line)
    if syntect detects from first line:
        if try_with_base_fallback(detected) succeeds:
            return result

    return None

try_with_base_fallback(candidate):
    if parser_available(candidate):
        return candidate
    if base_configured(candidate) AND parser_available(base):
        return base
    return None
```

This means:
- If client sends `languageId: "rmd"` and its base is `markdown`, use the markdown parser
- If token `py` is normalized by syntect to `python`, use the python parser
- If token `jsx` is not recognized by syntect but its base is `javascript`, use the javascript parser
- If no match, continue to the next detection method

### Language Injection

The fallback chain also applies to **injected languages** (e.g., code blocks in Markdown, JavaScript inside HTML). Injection queries extract a language identifier, but this identifier needs resolution:

```
Document (markdown) ──parse──▶ AST ──injection query──▶ "py" ──detect──▶ python
                                                      ▶ "sh" ──detect──▶ bash
```

For example, a Markdown code fence with ` ```py ` provides the identifier
`"py"`. Two related operations intentionally have different acceptance rules:

- **Bridge canonicalization** produces a stable language key even when no
  parser exists yet. It checks an explicit configured base, then syntect token
  normalization (and that candidate's base), then first-line detection (and
  that candidate's base), and finally the raw identifier. Thus `py` remains
  `python` before and after parser installation.
- **Parser resolution** may then load or select the canonical language/base and
  can still fail when no grammar is available.

Parser resolution follows this fallback pattern:

1. **Try identifier then configured base**: Load `"py"` directly, then its
   configured base before considering generic normalization
2. **Normalize via syntect, then try its base**: Use `detect_from_token("py")`
   which returns `"python"`, and load that candidate or its configured base
3. **Try first-line candidate then its base**: Use shebang/mode-line detection
   only after the explicit identifier paths fail
4. **Skip parser-backed analysis if unavailable**: Bridge routing can still use
   the canonical candidate, but semantic/parser work skips the region when no
   parser matches

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
- **Parser selection can vary**: Same file might use different parsers on
  different systems, while bridge language keys and virtual URIs remain stable
- **Heuristic maintenance**: Shebang patterns need ongoing updates
- **languageId naming variance**: Clients may send languageIds that differ from parser names (e.g., `shellscript` vs `bash`); normalization may be needed later

### Neutral

- **Token-based detection includes extensions**: Extensions are treated as tokens, not a separate detection step
- **Parser availability is scoped**: Parser-backed detection depends on what's
  installed; bridge canonicalization does not
- **Auto-install interaction**: Detection completes first (returning None if no parser found); auto-install runs asynchronously afterward, making the parser available for subsequent requests
- **Caching**: Detection result is stored per-document; cache invalidates on content change or `languageId` change from client
- **syntect dependency**: Uses syntect's Sublime Text syntax definitions for token normalization and first-line detection

## Migration from Extension-Based Detection

The `filetypes` configuration field has been removed (PBI-061). Users who relied on custom extension mappings should:

1. Configure their LSP client to send the correct `languageId`
2. Use file associations in their editor (e.g., VS Code's `files.associations`)
3. Add shebangs or magic comments to extensionless files

This aligns with the principle: **configure at the source (client), not the sink (server)**.
