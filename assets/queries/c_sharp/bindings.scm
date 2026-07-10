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
; local function's name likewise belongs to the enclosing block and is
; visible across it (C# local functions may be called before their
; declaration), so it keeps whole-scope visibility.
((method_declaration name: (identifier) @definition.method)
 (#set! definition.scope "parent"))
((local_function_statement name: (identifier) @definition.function)
 (#set! definition.scope "parent"))

(field_declaration
  (variable_declaration (variable_declarator name: (identifier) @definition.field)))
(property_declaration name: (identifier) @definition.field)

; Class-level generics bind into the body by label; method generics land
; in the method scope. ACCEPTED LIMITATION: a generic in the base list
; (`: Base<T>`, outside the body) is not covered — silence, or a rare wrong
; answer only if an outer type shares the parameter name. Scoping the whole
; declaration would swallow the class's own name, so body-only is kept.
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

; ── Locals: point of declaration ─────────────────────────────────────────
; Visible from the declarator onward, so `int a = 1, b = a;` sees the first
; declarator from the second and `int a = a;` binds the new local.
((local_declaration_statement
   (variable_declaration (variable_declarator name: (identifier) @definition)))
 (#set! definition.visibility "declaration"))
; `for (int i = 0; …)` — the initializer holds a bare variable_declaration
; (no local_declaration_statement wrapper); i is scoped to the for and
; visible from its declarator (condition, update, body).
((for_statement
   initializer: (variable_declaration (variable_declarator name: (identifier) @definition)))
 (#set! definition.visibility "declaration"))
; `M(out var x)` declares x from the expression onward.
((declaration_expression name: (identifier) @definition) @_decl
 (#set! definition.visibility "after"))

; ── Parameters and binders ───────────────────────────────────────────────
(parameter name: (identifier) @definition.parameter)
; The foreach binder is visible in the body but not while evaluating the
; iterable (which is evaluated before the binding): anchor `after` to the
; iterable so `foreach (var x in make(x))` reads the outer x.
((foreach_statement left: (identifier) @definition right: (_) @_it)
 (#set! definition.visibility "after"))
(catch_declaration name: (identifier) @definition.parameter)

; ── References ───────────────────────────────────────────────────────────
; The name after a dot is member access; the object before it resolves.
((identifier) @reference
 (#not-has-parent? @reference "member_access_expression" "member_binding_expression" "qualified_name" "using_directive"))
(member_access_expression expression: (identifier) @reference)
