; Lexical name bindings for Go (lexical-name-resolution ADR).
;
; Package-level declarations are order-independent (whole-scope
; visibility); function-local declarations are sequential ("after").
; Selector fields and method names are member access and stay uncaptured.

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
 (#set! definition.scope "parent"))
(source_file (var_declaration (var_spec name: (identifier) @definition)))
(source_file (const_declaration (const_spec name: (identifier) @definition.constant)))
((type_declaration (type_spec name: (type_identifier) @definition.type))
 (#set! definition.namespace "type"))

; ── Function-local declarations: sequential ──────────────────────────────
((short_var_declaration left: (expression_list (identifier) @definition)) @_d
 (#set! definition.visibility "after"))
((var_declaration (var_spec name: (identifier) @definition)) @_d
 (#set! definition.visibility "after"))
((const_declaration (const_spec name: (identifier) @definition.constant)) @_d
 (#set! definition.visibility "after"))
; `switch v := x.(type)` binds v in every case.
(type_switch_statement alias: (expression_list (identifier) @definition))

; ── Parameters (incl. method receivers) ──────────────────────────────────
(parameter_declaration name: (identifier) @definition.parameter)
(variadic_parameter_declaration name: (identifier) @definition.parameter)

; ── References ───────────────────────────────────────────────────────────
; Selector fields are (field_identifier) — never captured by this blanket.
(identifier) @reference
((type_identifier) @reference
 (#set! reference.namespace "type"))
