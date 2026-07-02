; Lexical name bindings for Bash (lexical-name-resolution ADR).
;
; Bash is dynamically scoped: per the ADR's sanctioned approximation,
; every definition registers at the layer root, so navigation jumps to
; the textual definition site (the same flattening ctags applies).
; Functions and variables live in separate namespaces.

; ── Variables ────────────────────────────────────────────────────────────
((variable_assignment name: (variable_name) @definition)
 (#set! definition.scope "global"))
((for_statement variable: (variable_name) @definition)
 (#set! definition.scope "global"))

; ── Functions ────────────────────────────────────────────────────────────
((function_definition name: (word) @definition.function)
 (#set! definition.scope "global")
 (#set! definition.namespace "function"))

; ── References ───────────────────────────────────────────────────────────
; $x and ${x...}; positional/special parameters are a different node type
; and stay uncaptured.
(simple_expansion (variable_name) @reference)
(expansion (variable_name) @reference)
; A command word may call a defined function.
((command_name (word) @reference)
 (#set! reference.namespace "function"))
