; Lexical name bindings for R (lexical-name-resolution ADR).
;
; Function bodies are the only scopes — braces alone don't scope, so a
; for-variable survives its loop by design. Assignments (`<-`, `=`,
; `->`) bind from the statement onward; `<<-` is super-assignment and
; merges into an enclosing visible binding instead of shadowing
; (modelled with outer-or-local — R would create the binding in the
; global environment when nothing encloses; here it lands in the
; current scope, an accepted navigation-level approximation).
; `$`/`@` member names and named-argument names stay silent.

; ── Scopes ──────────────────────────────────────────────────────────────
(function_definition) @scope.function

; ── Assignments ──────────────────────────────────────────────────────────
((binary_operator lhs: (identifier) @definition operator: "<-") @_a
 (#set! definition.visibility "after"))
((binary_operator lhs: (identifier) @definition operator: "=") @_a
 (#set! definition.visibility "after"))
; ACCEPTED LIMITATION: self-recursion (`fact <- function(n) … fact(n-1)`)
; leaves the recursive call unresolved. The name is visible only after the
; assignment (correct for a value self-reference like `x <- x + 1`, which
; reads the outer x). A recursive call needs the name visible inside its
; own body; a second `declaration`-visibility rule for function RHSs would
; register a *separate* binding (merge can't find the not-yet-visible
; `after` binding at the definition position), splitting rename/highlight —
; worse than the silence. A one-binding fix needs engine-level dedup of
; same-name definitions across visibilities.
((binary_operator rhs: (identifier) @definition operator: "->") @_a
 (#set! definition.visibility "after"))
((binary_operator lhs: (identifier) @definition operator: "<<-") @_a
 (#set! definition.visibility "after")
 (#set! definition.rebind "outer-or-local"))
((binary_operator rhs: (identifier) @definition operator: "->>") @_a
 (#set! definition.visibility "after")
 (#set! definition.rebind "outer-or-local"))

; ── Parameters and loop variables ────────────────────────────────────────
(parameter name: (identifier) @definition.parameter)
; "after" anchors to the variable itself (nothing else is captured), so
; the binding is live in the sequence expression and the body alike.
((for_statement variable: (identifier) @definition)
 (#set! definition.visibility "after"))

; ── References ───────────────────────────────────────────────────────────
; obj$member / obj@slot resolve only the object; f(name = x) names a
; parameter, not a variable.
((identifier) @reference
 (#not-has-parent? @reference "extract_operator" "argument" "parameter"))
(extract_operator lhs: (identifier) @reference)
(argument value: (identifier) @reference)
