; Lexical name bindings for PHP (lexical-name-resolution ADR).
;
; PHP functions do not capture enclosing local variables, but they do see
; global function and class names: function-like scopes stop the default
; namespace with `scope.inherits "function class"`. Arrow functions
; capture automatically, so they inherit everything. `global $x` routes a
; function's writes to the top level via @redirect. Member and static
; access ($o->x, K::x) is never captured — silence over a wrong answer.

; ── Scopes ──────────────────────────────────────────────────────────────
((function_definition) @scope
 (#set! scope.inherits "function class"))
((method_declaration) @scope
 (#set! scope.inherits "function class"))
((anonymous_function) @scope
 (#set! scope.inherits "function class"))
; Arrow functions capture by value automatically: full inheritance.
(arrow_function) @scope
; Class bodies confine their members.
(declaration_list) @scope

; ── Functions and classes: global and hoisted ─────────────────────────────
((function_definition name: (name) @definition.function)
 (#set! definition.scope "global")
 (#set! definition.namespace "function"))
((class_declaration name: (name) @definition.type)
 (#set! definition.scope "global")
 (#set! definition.namespace "class"))
((interface_declaration name: (name) @definition.type)
 (#set! definition.scope "global")
 (#set! definition.namespace "class"))
((trait_declaration name: (name) @definition.type)
 (#set! definition.scope "global")
 (#set! definition.namespace "class"))

; ── Variables ─────────────────────────────────────────────────────────────
; Assignment binds into the function scope (PHP has no block scope; the
; name is function-wide, mirroring the runtime's undefined-until-assigned
; behavior rather than reading outward).
(assignment_expression left: (variable_name (name) @definition))
(reference_assignment_expression left: (variable_name (name) @definition))

; Parameters.
(simple_parameter name: (variable_name (name) @definition.parameter))
(variadic_parameter name: (variable_name (name) @definition.parameter))
(property_promotion_parameter name: (variable_name (name) @definition.parameter))

; foreach binders: `foreach ($xs as $k => $v)` / `foreach ($xs as $v)`.
(foreach_statement (pair (variable_name (name) @definition)))
(foreach_statement (variable_name) (variable_name (name) @definition))

; A closure's `use` clause imports the outer variable as a fresh
; function-scoped binding.
(anonymous_function_use_clause (variable_name (name) @definition))

; `global $x` re-routes $x within this function to the top level.
((global_declaration (variable_name (name) @redirect))
 (#set! redirect.target "global"))

; ── References ───────────────────────────────────────────────────────────
; Every $variable read; member/static names after -> or :: are (name)
; nodes in other positions and stay uncaptured.
(variable_name (name) @reference)
((function_call_expression function: (name) @reference)
 (#set! reference.namespace "function"))
((named_type (name) @reference)
 (#set! reference.namespace "class"))
((object_creation_expression (name) @reference)
 (#set! reference.namespace "class"))
((base_clause (name) @reference)
 (#set! reference.namespace "class"))
((class_interface_clause (name) @reference)
 (#set! reference.namespace "class"))
