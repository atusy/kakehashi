; Lexical name bindings for Julia (lexical-name-resolution ADR).
;
; Functions, loops, let blocks, comprehensions, and modules open scopes;
; if/begin blocks do not. An assignment binds into the nearest enclosing
; function (hard scope), so a name assigned anywhere in a function is
; local throughout it: reassigning inside a loop updates the function's
; binding rather than splitting a loop-local one, while assigning a name
; that only exists as a global creates a function-local instead of
; writing the global (a top-level assignment falls back to the layer
; root). Loops/let/comprehensions are soft scopes for their own binders
; (for/let variables), which stay confined.
;
; ACCEPTED APPROXIMATION (see issue #566): Julia's true rule is "assign an
; existing enclosing *local* if one is visible, else create a local in
; this hard scope" — so re-assigning inside a `let` or a nested closure a
; name the enclosing function already binds should update that binding.
; Binding into the nearest hard scope instead leaves such a reassignment
; as a *separate* binding (conservative under-linking) rather than the
; alternative (outer-or-local), which would merge a function-local with a
; same-named global — a wrong cross-scope rename. Silence over a wrong
; answer, per the ADR. The proper fix — separate `local`/`global`
; namespaces selected by `#has-ancestor?`, so outer-or-local finds
; enclosing locals but never a global while reads search both — is a
; namespace re-architecture tracked in #566.
; Types
; and values share the default namespace (annotations and constructor
; calls are plain identifiers). Field access (obj.size) resolves only
; the value — the member stays uncaptured. Short-form definitions
; (`f(x) = x`) are deliberately silent for now.

; ── Scopes ──────────────────────────────────────────────────────────────
; Hard scopes — an assignment inside one is local to it: functions,
; macros, arrow functions, do-blocks, let blocks, comprehensions, and
; modules. `nearest:hard` targets the closest of these.
(function_definition) @scope.hard
(macro_definition) @scope.hard
(arrow_function_expression) @scope.hard
(do_clause) @scope.hard
(let_statement) @scope.hard
(comprehension_expression) @scope.hard
(module_definition) @scope.hard
; Loops are soft scopes: an assignment writes an enclosing hard-scope
; binding when one exists, so they are not `hard`.
(for_statement) @scope
(while_statement) @scope

; ── Functions, macros, structs, modules ──────────────────────────────────
; The name is the head of the signature's call expression; it belongs to
; the scope enclosing the definition.
((function_definition
   (signature (call_expression . (identifier) @definition.function)))
 (#set! definition.scope "parent"))
((macro_definition
   (signature (call_expression . (identifier) @definition.function)))
 (#set! definition.scope "parent"))

(struct_definition (type_head . (identifier) @definition.type))
(struct_definition (type_head (binary_expression . (identifier) @definition.type)))
(abstract_definition (type_head . (identifier) @definition.type))
((module_definition name: (identifier) @definition.module)
 (#set! definition.scope "parent"))

; ── Parameters ───────────────────────────────────────────────────────────
(function_definition
  (signature (call_expression (argument_list (identifier) @definition.parameter))))
(function_definition
  (signature (call_expression (argument_list
    (typed_expression . (identifier) @definition.parameter)))))
(function_definition
  (signature (call_expression (argument_list
    (named_argument . (identifier) @definition.parameter)))))
(macro_definition
  (signature (call_expression (argument_list (identifier) @definition.parameter))))
(arrow_function_expression . (identifier) @definition.parameter)
(arrow_function_expression . (tuple_expression (identifier) @definition.parameter))

; ── Assignments and binders ──────────────────────────────────────────────
((assignment . (identifier) @definition)
 (#set! definition.scope "nearest:hard"))
((assignment . (tuple_expression (identifier) @definition))
 (#set! definition.scope "nearest:hard"))
((assignment . (open_tuple (identifier) @definition))
 (#set! definition.scope "nearest:hard"))
; The for binder is visible after the whole `x in make(x)` binding, so the
; iterable's own references read outward, not the new binder.
((for_binding . (identifier) @definition) @_it
 (#set! definition.visibility "after"))
((for_binding . (tuple_expression (identifier) @definition)) @_it
 (#set! definition.visibility "after"))
((let_binding . (identifier) @definition)
 (#set! definition.visibility "after"))

; ── References ───────────────────────────────────────────────────────────
; The member after a dot is never a lexical reference; the value before
; it is. Named-argument names at call sites name parameters, not locals.
((identifier) @reference
 (#not-has-parent? @reference "field_expression" "named_argument"))
(field_expression value: (identifier) @reference)
