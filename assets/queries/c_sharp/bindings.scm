; Lexical name bindings for C# (lexical-name-resolution ADR).
;
; Class members are order-independent (whole-scope visibility in the
; declaration list); locals are sequential ("after"). Types and values
; share the default namespace: C#'s grammar writes both as (identifier).
; Member access (o.Field, o.Method()) resolves only the object — the
; name after the dot is never captured. Silence over a wrong answer.

; ── Scopes ──────────────────────────────────────────────────────────────
(block) @scope
(declaration_list) @scope
(method_declaration) @scope.function
(constructor_declaration) @scope.function
(local_function_statement) @scope.function
(lambda_expression) @scope
(anonymous_method_expression) @scope
(for_statement) @scope
(foreach_statement) @scope
(catch_clause) @scope

; ── Type and member declarations (order-independent) ─────────────────────
(class_declaration name: (identifier) @definition.type)
(interface_declaration name: (identifier) @definition.type)
(struct_declaration name: (identifier) @definition.type)
(record_declaration name: (identifier) @definition.type)
(enum_declaration name: (identifier) @definition.type)
(delegate_declaration name: (identifier) @definition.type)

; A method's name belongs to the declaration list, not its own scope; a
; local function's name likewise belongs to the enclosing block.
((method_declaration name: (identifier) @definition.method)
 (#set! definition.scope "parent"))
((local_function_statement name: (identifier) @definition.function)
 (#set! definition.scope "parent")
 (#set! definition.visibility "declaration"))

(field_declaration
  (variable_declaration (variable_declarator name: (identifier) @definition.field)))
(property_declaration name: (identifier) @definition.field)

; Class-level generics bind into the body by label (the parameter list
; precedes the body node); method generics land in the method scope.
((class_declaration
   (type_parameter_list (type_parameter name: (identifier) @definition.type))
   body: (declaration_list) @scope.body)
 (#set! definition.scope "body"))
((interface_declaration
   (type_parameter_list (type_parameter name: (identifier) @definition.type))
   body: (declaration_list) @scope.body)
 (#set! definition.scope "body"))
(method_declaration
  (type_parameter_list (type_parameter name: (identifier) @definition.type)))

; ── Locals: sequential ───────────────────────────────────────────────────
((local_declaration_statement
   (variable_declaration (variable_declarator name: (identifier) @definition))) @_decl
 (#set! definition.visibility "after"))
; `for (int i = 0; …)` — the initializer holds a bare variable_declaration
; (no local_declaration_statement wrapper); i is scoped to the for.
; Anchor `after` to the declaration, not the whole loop, so i is visible
; in the condition, update, and body.
((for_statement
   initializer: (variable_declaration (variable_declarator name: (identifier) @definition)) @_decl)
 (#set! definition.visibility "after"))
; `M(out var x)` declares x from the expression onward.
((declaration_expression name: (identifier) @definition) @_decl
 (#set! definition.visibility "after"))

; ── Parameters and binders ───────────────────────────────────────────────
(parameter name: (identifier) @definition.parameter)
(foreach_statement left: (identifier) @definition)
(catch_declaration name: (identifier) @definition.parameter)

; ── References ───────────────────────────────────────────────────────────
; The name after a dot is member access; the object before it resolves.
((identifier) @reference
 (#not-has-parent? @reference "member_access_expression" "qualified_name" "using_directive"))
(member_access_expression expression: (identifier) @reference)
