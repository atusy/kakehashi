# Runtime Query Install Dependencies

## Status

Accepted.

## Context

The query loader follows `inherits` modelines in runtime files, including
`extends` overlays outside the install data directory. Auto-install previously
examined only the data directory, so an otherwise successful installation could
leave a query unloadable because an overlay's parent was missing (#1059).

The installer deliberately takes a language-level union across highlights and
injections. Per-kind dependency traversal was considered in #1060 and deferred
because its maintenance cost exceeded the demonstrated benefit.

## Decision

During LSP auto-install, configured `searchPaths` are trusted sources of query
dependency names. Parents declared by their `highlights.scm` and
`injections.scm` files are included recursively, using the existing modeline
rules, including the exclusion of parenthesized parents when inherited.
Downloads use the existing upstream query source and data-directory destination.
Runtime paths do not provide download URLs or change the parser source.
Languages with an explicit query list (including an empty list) do not use
runtime paths as dependency inputs.

Opening a document also checks an already-loaded managed parser's query chain.
Query-only repair preserves parsing with that parser while installation runs.
Injected languages are checked on initial lifecycle passes or fresh loads;
cached edit passes do not rescan the dependency graph. A parser selected from
outside the managed data directory is not a query-repair target.

Each request captures its search paths. Concurrent requests for the same
language share an outcome only when those inputs match; a request with different
paths waits, then evaluates its own dependencies. Installation remains serialized
per language. This does not make an in-flight request track later settings
changes automatically.

Retain the language-level union and stage the discovered languages into the
data directory, including parents already available elsewhere. The standalone
CLI installation remains scoped to the data directory; this decision adds no
configuration discovery to that command. Explicit query-path lists, inline
queries, bindings, and captures kinds remain outside installer discovery.

## Invariants

> The invariants below are normative; the mechanisms that satisfy them are
> deliberately unspecified.

- External runtime files must not be overwritten by an install. They belong to
  the user, and only dependency names are taken from them.
- Missing parents discovered in runtime files must not be ignored by an install
  completeness check. Otherwise auto-install can incorrectly skip repair.
- Failed dependency preparation must not replace the requested language's
  existing parser or queries.
- Managed dependencies must retain the existing protection against concurrent
  install/uninstall. External user edits are not serialized by those locks;
  pre-publication checks detect changes already visible then, not future edits.

## Considered Options

1. Keep external parents manual. This avoids expanding the inputs that can
   trigger network access, but leaves the loader and installer inconsistent.
2. Stage the union declared by configured runtime files into the data directory
   (chosen). This extends the existing installation contract without teaching
   it to treat unmanaged query files as installed artifacts.
3. Resolve completeness per query kind across all runtime paths. This could
   avoid extra downloads and support custom parents absent upstream, but would
   also require changing staging and concurrent publication checks consistently.
   It remains a separate design change, as discussed in #1060.

## Consequences

An auto-install attempt can satisfy an external overlay's query dependencies.
Configured runtime paths can now cause additional downloads, and the
conservative union can still over-fetch or fail offline. A custom parent
available only outside the data directory and absent upstream is not treated as
an installed dependency. Users who maintain their query assets themselves can
disable auto-install.

No file watcher or automatic retry on overlay edits is introduced. An edit that
races the last dependency check may require another installation attempt.

## Confirmation

Local HTTP fixtures cover external and transitive dependencies, optional
parents, existing installations, unavailable parents, and forced replacement.
Completeness tests check that a missing overlay parent prevents an early
success. Publication tests mutate an overlay after staging to verify that a
new dependency visible before publication is rejected.
