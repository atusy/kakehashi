# Deprecation Removal Deadlines

**Related Decisions**:
- [configuration-merging-strategy](configuration-merging-strategy.md)
- [semantic-token-capture-mapping-config](semantic-token-capture-mapping-config.md)
- [ls-bridge-server-pool-coordination](ls-bridge-server-pool-coordination.md)

## Context

kakehashi accepts several deprecated configuration shapes so users can migrate
without an immediate breaking change. Those compatibility paths identify
themselves only through prose such as "may be removed in a future release",
which neither distinguishes the release generation that introduced a
deprecation nor prevents an obsolete path from surviving indefinitely.

The v0 compatibility paths are intentionally allowed to remain throughout v1,
but must not ship in v2. Future deprecations need their own independently
reviewable deadline instead of inheriting that schedule accidentally.

## Decision

Every retained path explicitly designated as deprecated declares both the major
version in which it was deprecated and the major version in which it must be
removed. A build at or after the removal major fails while that declaration
remains, naming the expired compatibility path and its deadline. Compatibility
fallbacks that are not deprecated are outside this removal policy.

All compatibility paths deprecated during v0 declare v2 as their removal
major. They remain valid in v0 and v1. A compatibility path first deprecated in
v1 uses a distinct declaration and must state its own removal major.

User-facing deprecation notices state the scheduled removal major. Reaching the
deadline removes the legacy parser, normalization, warning, tests, and deadline
declaration together; deleting only the declaration does not satisfy the
policy.

## Invariants

> The invariants below are normative; the mechanisms that satisfy them are
> deliberately unspecified.

- A release must not build at or after a compatibility path's declared removal
  major while that path remains supported. This makes a major-version bump an
  unavoidable deprecation inventory check.
- Deprecations introduced in different major versions remain distinguishable
  even when they affect the same subsystem, so one generation's deadline does
  not force or postpone another's removal.
- A notice and its compatibility behavior have the same deadline. Users must
  not be promised support beyond the release in which the implementation is
  required to disappear.

## Considered Options

### Record only when the path was deprecated

Rejected because version metadata alone documents history but does not encode
when compatibility must end. Rust's standard deprecation metadata also warns
about use rather than rejecting a stale declaration.

### Audit deprecations manually during a major release

Rejected because the check is easy to omit and prose does not provide a
machine-checkable inventory or boundary.

### Scan source text in CI for version-shaped comments

Rejected because comments and formatting are not a stable contract between the
compatibility implementation and the check. It would also allow a local build
to succeed while the release gate is already known to be violated.

### Enforce explicit deadlines during compilation

Chosen because the declaration stays beside the compatibility warning, works
in ordinary local builds and CI, and makes the package major-version bump fail
at the exact boundary that requires removal.

## Consequences

### Positive

- v0 and v1 deprecations can be inventoried and scheduled independently.
- A v2 version bump cannot silently retain any registered v0 compatibility
  path.
- Notices give users a concrete migration deadline.

### Negative

- Adding a deprecation requires choosing and maintaining a removal major.
- A major-version bump may be blocked until several compatibility paths and
  their tests are removed together.

### Neutral

- The policy does not change which deprecated configuration shapes are
  accepted in v0 or v1.
- Runtime handling after removal continues to follow each configuration
  source's unknown-key policy.
