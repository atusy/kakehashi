; Lexical name bindings for Rust (lexical-name-resolution ADR).
;
; Accuracy posture: silence over a wrong answer. Constructs this file does
; not capture (deeply nested patterns, generics, lifetimes, `use` aliases,
; loop labels) stay unresolved and fall through to a bridge server.

; ── Scopes ──────────────────────────────────────────────────────────────
(block) @scope
(function_item) @scope.function
(closure_expression) @scope
(match_arm) @scope
; Module / impl / trait bodies confine their items.
(declaration_list) @scope

; ── Items (hoisted: visible in their whole scope) ───────────────────────
; A function's name belongs to the scope enclosing the item, not to the
; function's own scope.
((function_item name: (identifier) @definition.function)
 (#set! definition.scope "parent"))

((struct_item name: (type_identifier) @definition.type)
 (#set! definition.namespace "type"))
((enum_item name: (type_identifier) @definition.type)
 (#set! definition.namespace "type"))
((union_item name: (type_identifier) @definition.type)
 (#set! definition.namespace "type"))
((trait_item name: (type_identifier) @definition.type)
 (#set! definition.namespace "type"))
((type_item name: (type_identifier) @definition.type)
 (#set! definition.namespace "type"))

(mod_item name: (identifier) @definition.module)
(const_item name: (identifier) @definition.constant)
(static_item name: (identifier) @definition.constant)

; ── let: each declaration is a fresh shadow, visible after the statement ─
; `let x = x + 1` reads the prior binding; rename/references never cross
; the shadow boundary.
((let_declaration
   pattern: [
     (identifier) @definition
     (mut_pattern (identifier) @definition)
     (ref_pattern (identifier) @definition)
     (tuple_pattern (identifier) @definition)
     (tuple_pattern (mut_pattern (identifier) @definition))
   ]) @_let
 (#set! definition.visibility "after")
 (#set! definition.rebind "fresh"))

; ── Parameters: visible throughout the function / closure ───────────────
(parameter pattern: (identifier) @definition.parameter)
(parameter pattern: (mut_pattern (identifier) @definition.parameter))
(closure_parameters (identifier) @definition.parameter)

; ── Branch-scoped bindings ───────────────────────────────────────────────
; `if let` reaches the then-branch alone; `while let` and `for` reach the
; loop body. The lowercase guard keeps unit enum variants (`None`) from
; being read as bindings.
((if_expression
   condition: (let_condition
     pattern: [
       (identifier) @definition
       (tuple_struct_pattern (identifier) @definition)
       (mut_pattern (identifier) @definition)
     ])
   consequence: (block) @scope.then)
 (#lua-match? @definition "^[a-z_]")
 (#set! definition.scope "then"))

((while_expression
   condition: (let_condition
     pattern: [
       (identifier) @definition
       (tuple_struct_pattern (identifier) @definition)
       (mut_pattern (identifier) @definition)
     ])
   body: (block) @scope.body)
 (#lua-match? @definition "^[a-z_]")
 (#set! definition.scope "body"))

((for_expression
   pattern: [
     (identifier) @definition
     (mut_pattern (identifier) @definition)
     (tuple_pattern (identifier) @definition)
   ]
   body: (block) @scope.body)
 (#set! definition.scope "body"))

; Match-arm patterns bind within their arm (the arm is a scope above).
((match_arm
   pattern: (match_pattern [
     (identifier) @definition
     (tuple_struct_pattern (identifier) @definition)
   ]))
 (#lua-match? @definition "^[a-z_]"))

; ── References ───────────────────────────────────────────────────────────
(identifier) @reference
((type_identifier) @reference
 (#set! reference.namespace "type"))
