; Lexical name bindings for Haskell (lexical-name-resolution ADR).
;
; Every binding group is order-independent (letrec everywhere): default
; whole-scope visibility. A type signature and its equations merge into
; one binding, so rename touches the signature too. Pattern coverage is
; deliberately shallow — a direct variable and one constructor-argument
; level; deeper pattern nests, do-notation binds, records, operators,
; and the type language stay silent, bridge-owned.

; ── Scopes ──────────────────────────────────────────────────────────────
; A function equation holds its parameters and its where-clause; a bind
; holds its where-clause; let/lambda/case-alternative confine likewise.
(function) @scope.function
(bind) @scope
(let_in) @scope
(lambda) @scope
(alternative) @scope

; ── Definitions ──────────────────────────────────────────────────────────
; Names belong to the scope enclosing the declaration node (top level,
; a where clause's function, or a let's binds).
((function name: (variable) @definition.function)
 (#set! definition.scope "parent"))
((bind name: (variable) @definition)
 (#set! definition.scope "parent"))
; The signature merges into the same binding as the equations.
(signature name: (variable) @definition)

; ── Parameters and pattern binders ───────────────────────────────────────
(function patterns: (patterns (variable) @definition.parameter))
(function patterns: (patterns (apply argument: (variable) @definition.parameter)))
(lambda patterns: (patterns (variable) @definition.parameter))
(lambda patterns: (patterns (apply argument: (variable) @definition.parameter)))
(alternative pattern: (variable) @definition)
(alternative pattern: (apply argument: (variable) @definition))

; ── References ───────────────────────────────────────────────────────────
; Constructors and qualified names are different node kinds — silent.
(variable) @reference
