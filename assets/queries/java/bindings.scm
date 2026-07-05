; Lexical name bindings for Java (lexical-name-resolution ADR).
;
; Class members are order-independent (whole-scope visibility in the
; class body); locals are sequential ("after"). Member access (o.field,
; o.method()) resolves only the object: the name after the dot is never
; captured — silence over a wrong answer. Bare calls resolve to methods
; via the `!object` form.

; ── Scopes ──────────────────────────────────────────────────────────────
(block) @scope
(class_body) @scope
(interface_body) @scope
(enum_body) @scope
(method_declaration) @scope.function
(constructor_declaration) @scope.function
(lambda_expression) @scope
(for_statement) @scope
(enhanced_for_statement) @scope
(catch_clause) @scope

; ── Type declarations (order-independent) ────────────────────────────────
((class_declaration name: (identifier) @definition.type)
 (#set! definition.namespace "type"))
((interface_declaration name: (identifier) @definition.type)
 (#set! definition.namespace "type"))
((record_declaration name: (identifier) @definition.type)
 (#set! definition.namespace "type"))
((enum_declaration name: (identifier) @definition.type)
 (#set! definition.namespace "type"))

; Class-level generics bind into the class body by label (the parameter
; list precedes the body node); method-level generics land in the method
; scope by containment.
((class_declaration
   type_parameters: (type_parameters (type_parameter (type_identifier) @definition.type))
   body: (class_body) @scope.body)
 (#set! definition.scope "body")
 (#set! definition.namespace "type"))
((method_declaration
   type_parameters: (type_parameters (type_parameter (type_identifier) @definition.type)))
 (#set! definition.namespace "type"))

; ── Members: methods and fields hoist within the class body ─────────────
; A method's name belongs to the class body, not to the method's own scope.
((method_declaration name: (identifier) @definition.method)
 (#set! definition.scope "parent"))
(field_declaration
  declarator: (variable_declarator name: (identifier) @definition.field))

; ── Locals: sequential ───────────────────────────────────────────────────
((local_variable_declaration
   declarator: (variable_declarator name: (identifier) @definition)) @_decl
 (#set! definition.visibility "after"))

; ── Parameters ───────────────────────────────────────────────────────────
(formal_parameter name: (identifier) @definition.parameter)
(spread_parameter (variable_declarator name: (identifier) @definition.parameter))
(catch_formal_parameter name: (identifier) @definition.parameter)
(enhanced_for_statement name: (identifier) @definition)
(inferred_parameters (identifier) @definition.parameter)
(lambda_expression parameters: (identifier) @definition.parameter)

; ── Imports bind the terminal name as a type at the file level ──────────
((import_declaration (scoped_identifier name: (identifier) @definition.type))
 (#set! definition.namespace "type"))

; ── References ───────────────────────────────────────────────────────────
; The name after a dot is member access; the object before it resolves.
((identifier) @reference
 (#not-has-parent? @reference "field_access" "method_invocation" "scoped_identifier"))
(field_access object: (identifier) @reference)
(method_invocation object: (identifier) @reference)
; A bare call (no object) resolves to a method in scope.
(method_invocation
  !object
  name: (identifier) @reference)
((type_identifier) @reference
 (#set! reference.namespace "type"))
