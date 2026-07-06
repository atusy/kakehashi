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
; (for/let variables), which stay confined. Types
; and values share the default namespace (annotations and constructor
; calls are plain identifiers). Field access (obj.size) resolves only
; the value — the member stays uncaptured. Short-form definitions
; (`f(x) = x`) are deliberately silent for now.

; ── Scopes ──────────────────────────────────────────────────────────────
(function_definition) @scope.function
(macro_definition) @scope.function
; Arrow functions and do-blocks are hard scopes (functions) too.
(arrow_function_expression) @scope.function
(do_clause) @scope.function
(for_statement) @scope
(while_statement) @scope
(let_statement) @scope
(comprehension_expression) @scope
(module_definition) @scope

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
 (#set! definition.scope "nearest:function"))
((assignment . (tuple_expression (identifier) @definition))
 (#set! definition.scope "nearest:function"))
((assignment . (open_tuple (identifier) @definition))
 (#set! definition.scope "nearest:function"))
(for_binding . (identifier) @definition)
(for_binding . (tuple_expression (identifier) @definition))
(let_binding . (identifier) @definition)

; ── References ───────────────────────────────────────────────────────────
; The member after a dot is never a lexical reference; the value before
; it is. Named-argument names at call sites name parameters, not locals.
((identifier) @reference
 (#not-has-parent? @reference "field_expression" "named_argument"))
(field_expression value: (identifier) @reference)
