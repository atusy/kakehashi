; Lexical name bindings for JavaScript (lexical-name-resolution ADR).
;
; Property access uses (property_identifier), so the blanket identifier
; reference never captures member names. `var` hoists to the enclosing
; function; `let`/`const` keep whole-block visibility (mirroring the
; temporal-dead-zone shadowing the runtime's errors imply).

; ── Scopes ──────────────────────────────────────────────────────────────
(statement_block) @scope
(function_declaration) @scope.function
(function_expression) @scope.function
(generator_function_declaration) @scope.function
(arrow_function) @scope.function
(method_definition) @scope.function
(catch_clause) @scope
(for_statement) @scope
(for_in_statement) @scope

; ── var: function-scoped, hoisted ────────────────────────────────────────
((variable_declaration
   (variable_declarator name: (identifier) @definition))
 (#set! definition.scope "nearest:function"))
((variable_declaration
   (variable_declarator name: (object_pattern (shorthand_property_identifier_pattern) @definition)))
 (#set! definition.scope "nearest:function"))
((variable_declaration
   (variable_declarator name: (object_pattern (pair_pattern value: (identifier) @definition))))
 (#set! definition.scope "nearest:function"))
((variable_declaration
   (variable_declarator name: (array_pattern (identifier) @definition)))
 (#set! definition.scope "nearest:function"))

; ── let / const: block-scoped ────────────────────────────────────────────
(lexical_declaration
  (variable_declarator name: (identifier) @definition))
(lexical_declaration
  (variable_declarator name: (object_pattern (shorthand_property_identifier_pattern) @definition)))
(lexical_declaration
  (variable_declarator name: (object_pattern (pair_pattern value: (identifier) @definition))))
(lexical_declaration
  (variable_declarator name: (array_pattern (identifier) @definition)))

; ── Functions and classes ────────────────────────────────────────────────
; Function declarations hoist into the enclosing scope; a named function
; expression's name is visible only inside itself (stays local).
((function_declaration name: (identifier) @definition.function)
 (#set! definition.scope "parent"))
((generator_function_declaration name: (identifier) @definition.function)
 (#set! definition.scope "parent"))
(function_expression name: (identifier) @definition.function)
; Classes are not hoisted-usable: visible from the declaration onward.
((class_declaration name: (identifier) @definition.class)
 (#set! definition.visibility "declaration"))

; ── Parameters ───────────────────────────────────────────────────────────
(formal_parameters (identifier) @definition.parameter)
(formal_parameters (assignment_pattern left: (identifier) @definition.parameter))
(formal_parameters (rest_pattern (identifier) @definition.parameter))
(formal_parameters (object_pattern (shorthand_property_identifier_pattern) @definition.parameter))
(formal_parameters (object_pattern (pair_pattern value: (identifier) @definition.parameter)))
(formal_parameters (array_pattern (identifier) @definition.parameter))
(arrow_function parameter: (identifier) @definition.parameter)
(catch_clause parameter: (identifier) @definition)

; ── Imports ──────────────────────────────────────────────────────────────
(import_clause (identifier) @definition.import)
(import_specifier !alias name: (identifier) @definition.import)
(import_specifier alias: (identifier) @definition.import)
(namespace_import (identifier) @definition.import)

; ── References ───────────────────────────────────────────────────────────
(identifier) @reference
; `{ shorthand }` in an object literal reads the variable.
(shorthand_property_identifier) @reference
