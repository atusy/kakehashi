; Lexical name bindings for legacy Vim script (lexical-name-resolution ADR).
;
; The scope prefix is part of the syntax, not the tree shape: `s:`/`g:`
; names register at the layer root in a "script"/"global" namespace
; regardless of where the `let` executes (a `g:` set inside a function
; is visible everywhere after it). `l:` and function-local bare `let` names
; are default-namespace locals — `l:t` and bare `t` are the same variable.
; Script-level bare `let` names are global, addressable as both `x` and `g:x`.
; Function arguments answer `a:name` references. Bare identifier reads search
; locals first, then globals. `b:`/`w:`/`t:`/`v:` names, `{curly-braces}` names,
; and Vim9 syntax stay silent.

; ── Scopes ──────────────────────────────────────────────────────────────
(function_definition) @scope.function

; ── let: the prefix picks the namespace and the registration scope ──────
((let_statement . (scoped_identifier (scope) @_s (identifier) @definition))
 (#eq? @_s "s:")
 (#set! definition.namespace "script")
 (#set! definition.scope "global")
 (#set! definition.visibility "after"))
((let_statement . (scoped_identifier (scope) @_s (identifier) @definition))
 (#eq? @_s "g:")
 (#set! definition.namespace "global")
 (#set! definition.scope "global")
 (#set! definition.visibility "after"))
((let_statement . (scoped_identifier (scope) @_s (identifier) @definition))
 (#eq? @_s "l:")
 (#set! definition.visibility "after"))
((let_statement . (identifier) @definition)
 (#has-ancestor? @definition "function_definition")
 (#set! definition.visibility "after"))
((let_statement . (identifier) @definition)
 (#not-has-ancestor? @definition "function_definition")
 (#set! definition.namespace "global")
 (#set! definition.scope "global")
 (#set! definition.visibility "after"))

; ── Functions ────────────────────────────────────────────────────────────
; Script-local functions share the "script" namespace with s: variables
; (a same-named pair in one script is pathological).
((function_declaration name: (scoped_identifier (scope) @_s (identifier) @definition.function))
 (#eq? @_s "s:")
 (#set! definition.namespace "script")
 (#set! definition.scope "global")
 (#set! definition.visibility "after"))
((function_declaration name: (identifier) @definition.function)
 (#set! definition.namespace "global")
 (#set! definition.scope "global")
 (#set! definition.visibility "after"))

; Arguments live in the "a" namespace: declared bare, read as a:name.
((parameters (identifier) @definition.parameter)
 (#set! definition.namespace "a"))

((for_loop variable: (identifier) @definition)
 (#set! definition.visibility "after"))

; ── References ───────────────────────────────────────────────────────────
((scoped_identifier (scope) @_s (identifier) @reference)
 (#eq? @_s "s:")
 (#set! reference.namespace "script"))
((scoped_identifier (scope) @_s (identifier) @reference)
 (#eq? @_s "g:")
 (#set! reference.namespace "global"))
((scoped_identifier (scope) @_s (identifier) @reference)
 (#eq? @_s "l:"))
((argument (identifier) @reference)
 (#set! reference.namespace "a"))
; A bare identifier is a local if one exists, else a global.
((identifier) @reference
 (#not-has-parent? @reference "scoped_identifier" "argument" "parameters")
 (#set! reference.namespace "default global"))
