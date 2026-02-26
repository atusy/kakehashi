# ADR-0004: Keep tree-sitter-cli as Parser Compilation Dependency

| | |
|---|---|
| **Status** | superseded by [ADR-0022](0022-replace-tree-sitter-cli-with-loader.md) |
| **Date** | 2025-12-18 |
| **Decision-makers** | atusy |
| **Consulted** | Claude Code |
| **Informed** | kakehashi users |
| **Supersedes** | [ADR-0003](0003-parser-compilation-fallback-strategy.md) |

## Context and Problem Statement

ADR-0003 proposed a fallback chain for parser compilation: direct C compilation first, then tree-sitter-cli as fallback. The goal was to minimize dependencies by allowing users with only a C compiler to compile parsers without installing the Rust toolchain.

During backlog refinement, we identified a critical issue: direct C compilation requires `tree_sitter/parser.h` header files. How should these headers be provided to users?

## Decision Drivers

* **Minimize complexity**: Avoid maintaining parallel compilation paths
* **Ecosystem alignment**: Follow established patterns in the Tree-sitter ecosystem
* **User experience**: Provide clear, consistent dependency requirements
* **Maintenance burden**: Reduce testing and debugging surface area

## Considered Options

1. **Embed tree-sitter headers in binary** - Bundle `parser.h` inside kakehashi
2. **Download headers on demand** - Fetch from GitHub when compilation is needed
3. **Document headers as user requirement** - Require users to provide headers manually
4. **Keep tree-sitter-cli as sole dependency** - Abandon direct C compilation

## Decision Outcome

**Chosen option**: "Keep tree-sitter-cli as sole dependency", because:

1. **nvim-treesitter uses the same approach** - This is the established pattern in the ecosystem. If the most popular Tree-sitter integration requires tree-sitter-cli, users already expect this dependency.

2. **Header management adds significant complexity**:
   - Embedding: Increases binary size, requires updates when tree-sitter API changes
   - Downloading: Requires network, subject to GitHub rate limits
   - User-provided: Friction is comparable to just installing tree-sitter-cli

3. **Marginal benefit doesn't justify complexity** - The "C compiler only" path would only help users who have a C compiler but refuse to install Rust. This is a small subset of users.

4. **Maintenance burden** - Two compilation paths means double the testing, double the edge cases, and double the user support burden.

### Consequences

**Positive:**

* Simpler codebase with single compilation path
* Clear, consistent dependency requirements
* Aligned with nvim-treesitter ecosystem expectations
* Reduced maintenance and testing burden

**Negative:**

* Users must install Rust toolchain to get tree-sitter-cli
* Higher barrier to entry than "C compiler only" would have been

**Neutral:**

* Documentation already lists tree-sitter-cli as a requirement
* No code changes needed (current implementation already uses tree-sitter-cli)

## More Information

* [nvim-treesitter requirements](https://github.com/nvim-treesitter/nvim-treesitter) - Also requires tree-sitter-cli v0.26.1+
* [ADR-0003](0003-parser-compilation-fallback-strategy.md) - Superseded decision
