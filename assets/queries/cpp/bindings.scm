; inherits: c
; Lexical name bindings for C++ (lexical-name-resolution ADR).
;
; On top of the C rules: classes and aliases define type names, template
; type parameters bind into the templated function via scope-label
; targeting (a class template's parameters stay silent — class
; declarations carry no scope, mirroring the TypeScript posture), lambdas
; and range-for open scopes. Qualified names (ns::x, Box::area) and
; member accesses are never captured — silence over a wrong answer.

; ── Scopes ──────────────────────────────────────────────────────────────
(lambda_expression) @scope
(for_range_loop) @scope
; Namespace bodies confine their declarations.
(declaration_list) @scope

; ── Types ────────────────────────────────────────────────────────────────
((class_specifier name: (type_identifier) @definition.type body: (_))
 (#set! definition.namespace "type")
 (#set! definition.visibility "declaration"))
((alias_declaration name: (type_identifier) @definition.type) @_a
 (#set! definition.namespace "type")
 (#set! definition.visibility "after"))

; Template type parameters live in the templated function, registered by
; label regardless of containment (the parameter list precedes the
; function node). Class templates never match: silence over a root leak.
((template_declaration
   parameters: (template_parameter_list
     (type_parameter_declaration (type_identifier) @definition.type))
   (function_definition) @scope.function)
 (#set! definition.scope "function")
 (#set! definition.namespace "type"))

; ── Lambdas and range-for ────────────────────────────────────────────────
(lambda_expression
  declarator: (abstract_function_declarator
    parameters: (parameter_list
      (parameter_declaration
        declarator: [
          (identifier) @definition.parameter
          (pointer_declarator declarator: (identifier) @definition.parameter)
          (reference_declarator (identifier) @definition.parameter)
        ]))))

(for_range_loop
  declarator: [
    (identifier) @definition
    (pointer_declarator declarator: (identifier) @definition)
    (reference_declarator (identifier) @definition)
  ])

; ── Reference variables: `int &r = a;` ───────────────────────────────────
((declaration
   declarator: (init_declarator
     declarator: (reference_declarator (identifier) @definition))) @_decl
 (#set! definition.visibility "after"))
