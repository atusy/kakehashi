# Aggregation Priorities Wildcard

**Related Decisions**:
- [ls-bridge-server-pool-coordination](ls-bridge-server-pool-coordination.md) — Fan-out/fan-in mechanics that consume `priorities`
- [language-server-bridge-request-strategies](language-server-bridge-request-strategies.md) — Per-method strategies and merging rules
- [concatenated-formatting-pipeline](concatenated-formatting-pipeline.md) — The formatting pipeline whose allowlist semantics this decision generalizes
- [cross-layer-aggregation](cross-layer-aggregation.md) — Layer ordering (`order`), which adopts the same allowlist rule

## Implementation Status

Implemented. Expansion lives in `src/lsp/aggregation/server/priority.rs`
(`PriorityEntry`, `expand_priorities`); both fan-out and the `preferred`
fan-in consume the same expansion. The formatting pipeline's `"*"` rejection
and the no-explicit-names settings-apply warning live in
`plan_region_format` / `concatenated_formatting_pairs`.

## Context

`AggregationConfig.priorities` currently has **two semantics depending on the
strategy**:

- Under `preferred` (the common case), it is a *preference order only*:
  servers absent from the list still participate via first-win fallback
  (`fan_in/preferred.rs` — "when all prioritized servers are exhausted, fall
  back to first-win among unprioritized servers").
- Under the `concatenated` formatting pipeline, it is an *allowlist plus
  application order*: servers absent from the list do not run at all.
  concatenated-formatting-pipeline documents this as a "behavioral nuance"
  in its Negative consequences.

Two problems with keeping the dual semantics:

1. **Inconsistency compounds.** cross-layer-aggregation introduces a second
   ordered list (`layers.<method>.order`). Every new list must pick a side,
   and users must remember which list means which.
2. **Exclusion and demotion are inexpressible under `preferred`.** A user
   cannot say "only foo" (fallback always re-admits the rest), nor "foo
   first, everyone else, and zzz dead last" (the rest always outranks
   nothing — unlisted servers form one undifferentiated fallback group that
   cannot be positioned).

## Decision Drivers

- **One rule for every ordered list**: `priorities` (server names) and
  `order` (layers) should share semantics, differing only in element type.
- **Express exclusion and demotion**, not just promotion.
- **Defaults preserve behavior**: configs without `priorities` must behave
  exactly as today.
- **Sigil discipline**: `_` is established for wildcard *keys* that carry
  field-level inheritance (`resolve_with_wildcard`). A list *element* meaning
  "the rest" has no inheritance semantics and should not reuse that sigil.

## Decision Outcome

**Chosen approach**: `priorities` becomes an **ordered allowlist**. A `"*"`
element expands to "all candidate servers not named elsewhere in the list".

```toml
priorities = ["foo", "bar", "*", "zzz"]
# foo first; then bar; then every unlisted server (first-win group);
# zzz only if all of the above return nothing — demotion below the rest.
```

### Resolution Rules

| Configured value | Meaning |
|---|---|
| absent (`None` after merge) | ≡ `["*"]` — all candidates, today's default behavior |
| `["*"]` | explicit form of the default |
| `["foo"]` | **only** foo runs (BREAKING: was "prefer foo, fall back to the rest") |
| `["foo", "*"]` | the old `["foo"]` behavior: foo preferred, rest as fallback |
| `[]` | empty allowlist — no fan-out; the method is disabled for this bridge target (BREAKING: was pure first-win) |

- At most one `"*"` per list: the first occurrence is honored and the rest
  are ignored (logged at debug).
- A name matching no configured server is dropped and logged at **debug**,
  not warn: expansion runs on every dispatch (completion fires per
  keystroke), and an unmatched name is routine rather than necessarily a
  typo — priorities inherited from a `bridge._` wildcard entry legitimately
  name servers that exist only for other injection languages of the same
  host. A settings-apply-time typo validation (the
  `concatenated_formatting_pairs` pattern) is the upgrade path if silent
  exclusion proves costly in practice.
- `max_fan_out` applies after `"*"` expansion. `max_fan_out = 0` now overlaps
  `priorities = []`; both are kept (no deprecation in this decision).

### Per-Strategy Interplay

| Strategy | Role of the list | `"*"` handling |
|---|---|---|
| `preferred` | walk in order; first non-empty wins | a first-win group over the unlisted candidates at that position |
| `concatenated`, parallel merge (diagnostics, references) | membership only — merge order is irrelevant | includes the unlisted rest in the merge |
| `concatenated`, sequential formatting pipeline | membership **and** application order | **rejected**: `"*"` has no deterministic expansion order, and a formatter pipeline must be reproducible (concatenated-formatting-pipeline). Warn and ignore the element; if no explicit names remain, the existing empty-`priorities` misconfiguration fallback to `preferred` applies |

### Migration

Existing configs with a non-empty `priorities` that relied on implicit
fallback append `"*"`:

```toml
priorities = ["pyright"]        # before: pyright preferred, others fall back
priorities = ["pyright", "*"]   # after: identical behavior
```

Configs with no `priorities` are unaffected. Configs with an explicit `[]`
flip from "first-win among all" to "disabled" — the new reading doubles as a
per-method bridge kill switch, which had no spelling before.

## Consequences

### Positive

- **One list semantics everywhere**: the formatting pipeline's
  allowlist+order behavior stops being an exception and becomes the general
  rule; cross-layer `order` adopts the same rule with a closed element set.
- **New expressiveness**: per-method exclusion (`[]`, or omission from the
  list) and demotion below the unlisted rest (`["*", "zzz"]`) were previously
  unwritable.
- **Defaults unchanged**: absent `priorities` still fans out to everyone.

### Negative

- **Breaking change** for configs with non-empty `priorities`: `["foo"]`
  narrows from preference to allowlist, and `[]` flips from first-win to
  disabled. Must be called out in the changelog with the `"*"` migration.
- **Per-strategy caveat**: `"*"` is invalid in the sequential formatting
  pipeline — one more rule to document, though it replaces the larger
  "two meanings of `priorities`" rule it retires.

### Neutral

- `max_fan_out = 0` and `priorities = []` now express the same thing;
  redundancy tolerated until one proves unnecessary.
- The fan-in implementation keeps its buffered-win machinery; `"*"` is an
  expansion step at config-resolution time plus a group marker in the walk,
  not a new fan-in algorithm.

## Alternatives Considered

### A. Keep the dual semantics (status quo)

**Rejected because**: every new ordered list (starting with cross-layer
`order`) would have to pick a side, and the `preferred` reading cannot
express exclusion or demotion at all.

### B. `exclusive = true` flag beside `priorities`

A boolean switching the list from preference to allowlist.

**Rejected because**: it cannot express demotion below the rest
(`["foo", "*", "zzz"]` has no flag equivalent — the rest's *position* is the
point), and it adds a second knob whose interaction with the first must be
documented anyway.

### C. `_` as the wildcard element

Consistent with the `_` wildcard keys used across the config.

**Rejected because**: `_` keys carry field-level inheritance semantics
(`resolve_with_wildcard`); a "rest of the candidates" list element does not.
Reusing the sigil would suggest inheritance where none exists. Glob-like
`"*"` matches the actual semantics (match-everything-else) and keeps the two
mechanisms visually distinct.

### D. Separate `exclude` list

Keep `priorities` as preference order, add `exclude = ["zzz"]`.

**Rejected because**: two lists to reconcile (what does a name in both
mean?), and the rest's ordering relative to explicit entries remains
inexpressible.
