; Lexical name bindings for Lua (lexical-name-resolution ADR).
;
; Silence over a wrong answer: member access (t.x), method calls, and
; bracket indexing are never captured; a global read with no captured
; definition stays unresolved and falls through to a bridge server.

; ── Scopes ──────────────────────────────────────────────────────────────
(block) @scope
(function_declaration) @scope.function
(function_definition) @scope.function

; ── local declarations ───────────────────────────────────────────────────
; `local x = x`: the right-hand side precedes the match end and resolves
; outward (visibility "after"); each `local x` is a fresh shadow.
((variable_declaration
   (assignment_statement (variable_list name: (identifier) @definition))) @_decl
 (#set! definition.visibility "after")
 (#set! definition.rebind "fresh"))
; `local x` without a value.
((variable_declaration
   (variable_list name: (identifier) @definition)) @_decl
 (#set! definition.visibility "after")
 (#set! definition.rebind "fresh"))

; ── Functions ────────────────────────────────────────────────────────────
; `local function f` is visible inside its own body (recursion) but not
; above the statement: neither "scope" nor "after" can express that.
((function_declaration name: (identifier) @definition.function) @_fd
 (#lua-match? @_fd "^local")
 (#set! definition.visibility "declaration")
 (#set! definition.rebind "fresh"))
; `function f() end` assigns a global.
((function_declaration name: (identifier) @definition.function) @_fd
 (#not-lua-match? @_fd "^local")
 (#set! definition.scope "global"))

(parameters name: (identifier) @definition.parameter)

; ── Loop clauses bind into the loop body ─────────────────────────────────
((for_statement
   clause: (for_numeric_clause name: (identifier) @definition)
   body: (block) @scope.body)
 (#set! definition.scope "body"))
((for_statement
   clause: (for_generic_clause (variable_list name: (identifier) @definition))
   body: (block) @scope.body)
 (#set! definition.scope "body"))

; ── Assignment writes a visible local, else introduces a binding ─────────
; (Lua really creates a global; registering locally keeps same-scope
; navigation working while distant readers stay silent.)
((assignment_statement (variable_list name: (identifier) @definition)) @_assign
 (#not-has-parent? @_assign "variable_declaration")
 (#set! definition.visibility "after")
 (#set! definition.rebind "outer-or-local"))

; ── References ───────────────────────────────────────────────────────────
; Member names (t.x, t:m) are not lexical references; the table itself is.
((identifier) @reference
 (#not-has-parent? @reference "dot_index_expression" "method_index_expression"))
(dot_index_expression table: (identifier) @reference)
(method_index_expression table: (identifier) @reference)
