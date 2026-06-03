# ADR-0023: HTTP Archive as Primary Source Acquisition for Parsers

| | |
|---|---|
| **Status** | accepted |
| **Date** | 2026-02-26 |
| **Decision-makers** | atusy |
| **Consulted** | Claude Code |
| **Informed** | kakehashi users |

## Context and Problem Statement

Parser installation used a 3-step git clone process (`clone --depth 1`, `fetch revision`, `checkout FETCH_HEAD`). This required git to be installed and was slower than necessary — git protocol negotiation, ref advertisement, and packfile creation add overhead for what is essentially "download these source files."

GitHub serves repository archives at deterministic URLs (`/archive/{revision}.tar.gz`) containing only the source tree, with no git metadata.

## Decision Drivers

* **Minimize external dependencies**: Fewer required tools means easier setup
* **Speed**: Single HTTP GET is faster than 3 git protocol round-trips
* **Reliability**: HTTP downloads are simpler than git protocol interactions
* **Graceful degradation**: Must not break for users who rely on git-based workflows

## Considered Options

1. **Keep git clone only** (status quo)
2. **HTTP archive primary, git clone fallback**
3. **Replace git entirely with HTTP archive** (no fallback)

## Decision Outcome

**Chosen option**: "HTTP archive primary, git clone fallback", because:

1. **Fastest for the common case**: All nvim-treesitter parsers are on GitHub, so archive download covers 100% of standard usage
2. **Git becomes optional**: Users without git can still install parsers from GitHub
3. **Graceful fallback**: Non-GitHub URLs or download failures transparently fall back to git clone
4. **No regressions**: The git clone path is preserved and well-tested

### Implementation

The `fetch_source()` function implements the strategy:

1. Construct archive URL: `https://github.com/{owner}/{repo}/archive/{revision}.tar.gz`
2. Download and extract tarball, stripping the root directory prefix (`{repo}-{revision_no_v}/`)
3. On any failure, clean up and fall back to `clone_repo()`
4. For non-GitHub URLs, skip directly to git clone

### Consequences

**Positive:**

* Git is no longer required for GitHub-hosted parsers (the vast majority)
* Faster downloads: HTTP GET of compressed tarball vs 3 git commands
* Smaller data transfer: no `.git` metadata, packfiles, or refs

**Negative:**

* Two new dependencies: `flate2` and `tar` crates (well-established, ~10 transitive crates)
* Archive root directory naming depends on GitHub's convention (`{repo}-{revision_no_v}/`)

**Neutral:**

* Monorepo handling is unchanged: full archive is extracted, `location` subdirectory is used
* The compile step is unaffected (works on a source directory regardless of acquisition method)

## More Information

* [ADR-0022](0022-replace-tree-sitter-cli-with-loader.md) — Related: replaced tree-sitter-cli with tree-sitter-loader
* GitHub archive URL format: `https://github.com/{owner}/{repo}/archive/{ref}.tar.gz`
