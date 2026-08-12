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

Contract / invariant / mechanism — classify EVERY normative statement before
writing it, and again whenever review pressure adds one. The operative test:
*could a competent implementer do this differently and still satisfy
everything observable?* If yes, it is mechanism.

1. **Contract — keep, normative.** Externally observable behavior: wire
   shapes, status values and their meaning, error discriminators, ordering
   promises, what an enumeration shows. The test is *observability*, not how
   low-level the wording sounds — "settling entries enumerate as `stopping`"
   is a contract; "the entry keeps an escrow slot" is not.
2. **Invariant — keep, but state *what must hold*, never *how*.** The trap
   catalog adversarial review actually discovers: LSP forbids any client
   message before the initialize response; a child process must never exist
   outside owned records; SIGKILL delivery is not termination, only a reaped
   `wait` is. One or two sentences each, always with the *why*. These are the
   durable value of review — keep the traps, drop the machinery that closed
   them.
3. **Mechanism — delete.** Anything an implementer could legitimately do
   differently while satisfying 1 and 2. Body prose naming a concrete API
   (`JoinSet`, `oneshot`, `select!`, a specific channel or guard type) outside
   a code fence is almost always mechanism, as is any timing constant that is
   not itself a promise to a peer.

Why this matters: design-document review asks "what if X races Y", and the
tempting answer is to specify a mechanism. Doing that repeatedly accretes an
implementation the ADR was never meant to hold, and it never converges —
every closed interleaving exposes the next. Recording the *trap* instead
converges, because the trap catalog is finite.
-->

## Context

What is the issue motivating this decision? What constraints and forces apply?

## Decision

What is the change we are proposing and/or doing?

## Invariants

<!-- Include this section whenever the decision carries invariants — in
practice, whenever adversarial review has surfaced traps the implementation
must not fall into. Open it with this note, verbatim and byte-identical
across ADRs so it stays greppable:

> The invariants below are normative; the mechanisms that satisfy them are
> deliberately unspecified.

Then list the traps, one or two sentences each, each saying what must hold and
why it bites. Nothing here may say *how*. The note is the anti-regress device:
a later review finding that demands a mechanism is answered by pointing at it,
not by specifying machinery. -->

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
