; Lexical name bindings for Go (lexical-name-resolution ADR).
;
; Package-level declarations are order-independent (whole-scope
; visibility); function-local declarations are sequential ("after").
; Selector fields and method names are member access and stay uncaptured.
; The blank identifier `_` neither introduces nor references a binding.

; ── Scopes ──────────────────────────────────────────────────────────────
(block) @scope
(function_declaration) @scope.function
(method_declaration) @scope.function
(func_literal) @scope.function
(if_statement) @scope
(for_statement) @scope
(expression_switch_statement) @scope
(type_switch_statement) @scope
(expression_case) @scope
(type_case) @scope
(communication_case) @scope

; ── Package-level declarations: order-independent ────────────────────────
((function_declaration name: (identifier) @definition.function)
 (#not-eq? @definition.function "_")
 (#set! definition.scope "parent"))
((source_file (var_declaration (var_spec name: (identifier) @definition)))
 (#not-eq? @definition "_"))
((source_file (const_declaration (const_spec name: (identifier) @definition.constant)))
 (#not-eq? @definition.constant "_"))
((type_declaration (type_spec name: (type_identifier) @definition.type))
 (#not-eq? @definition.type "_")
 (#set! definition.namespace "type"))

; ── Function-local declarations: sequential ──────────────────────────────
((short_var_declaration left: (expression_list (identifier) @definition)) @_d
 (#not-eq? @definition "_")
 (#set! definition.visibility "after"))
((var_declaration (var_spec name: (identifier) @definition)) @_d
 (#not-eq? @definition "_")
 (#set! definition.visibility "after"))
((const_declaration (const_spec name: (identifier) @definition.constant)) @_d
 (#not-eq? @definition.constant "_")
 (#set! definition.visibility "after"))
; `switch v := x.(type)` binds v in every case.
((type_switch_statement alias: (expression_list (identifier) @definition))
 (#not-eq? @definition "_"))

; ── Parameters (incl. method receivers) ──────────────────────────────────
((parameter_declaration name: (identifier) @definition.parameter)
 (#not-eq? @definition.parameter "_"))
((variadic_parameter_declaration name: (identifier) @definition.parameter)
 (#not-eq? @definition.parameter "_"))

; ── References ───────────────────────────────────────────────────────────
; Selector fields are (field_identifier) — never captured by this blanket.
((identifier) @reference
 (#not-eq? @reference "_"))
((type_identifier) @reference
 (#not-eq? @reference "_")
 (#set! reference.namespace "type"))
