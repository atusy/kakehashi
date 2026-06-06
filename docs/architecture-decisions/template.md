# Title Matching the Slug

<!--
File naming: use a descriptive slug, e.g. `node-reference-protocol.md`.
The H1 above should read as the Title Case of that slug.

Supersession is NOT recorded here. When a decision replaces another, delete the
old record — its history stays in `git log` (use `git log --follow <file>`).
Keep the alternatives that were genuinely considered: this directory records the
*decisions*, including the options weighed and why the chosen one won.

Cross-references between ADRs (this rule covers ADR-to-ADR `[slug](slug.md)`
links only; links to non-ADR targets — external URLs, source files, README —
are unaffected and may appear anywhere):
- In body prose, refer to another ADR by its **bare slug in plain text**
  (e.g. `see node-reference-protocol`), NOT as a markdown link. Add a section
  reference inline when useful (e.g. `ls-bridge-message-ordering § Cancellation Forwarding`).
- Reserve ADR-slug markdown links (`[slug](slug.md)`) for the curated
  related-decisions block — written as a top-of-file `**Related Decisions**:` /
  `**Related**:` list or a footer `## Related Decisions` section — and for
  footer link lists (e.g. `## More Information`).
- Rationale: ADRs are deleted when superseded (delete-on-supersede), so inline
  links rot into clickable 404s. Plain-text mentions degrade gracefully to a
  stale-but-harmless proper noun, keeping the curated link block the single
  place a link can rot.
-->

## Context

What is the issue motivating this decision? What constraints and forces apply?

## Decision

What is the change we are proposing and/or doing?

## Considered Options

Alternatives that were genuinely evaluated, and why each was or was not chosen.

## Consequences

What becomes easier or harder as a result?

### Positive

### Negative

### Neutral

## Decision–Implementation Gap

<!-- Optional. Include ONLY when the implementation diverges from this decision.
Summarize the gap concisely: what is deferred, partial, or done differently. -->
