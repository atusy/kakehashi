; Lexical name bindings for C (lexical-name-resolution ADR).
;
; Declare-before-use throughout: functions and types are visible from
; their declaration onward (a prototype and its definition merge into one
; binding), variables after their declaration statement. Member access
; (s.field, p->field) uses field_identifier nodes and is never captured —
; silence over a wrong answer.
;
; Tags (struct/union/enum) and typedefs share one `type` namespace by
; design, so the ubiquitous `typedef struct S { … } S;` idiom links the
; tag and the alias as one navigable type. C technically keeps tags in a
; separate namespace from ordinary identifiers; the accepted cost is that
; an unrelated reuse (`struct S {}; typedef int S;`) merges — rare in
; practice, and the alternative would break the common idiom's linkage.

; ── Scopes ──────────────────────────────────────────────────────────────
(compound_statement) @scope
(function_definition) @scope.function
(for_statement) @scope

; ── Functions ────────────────────────────────────────────────────────────
; A definition's name belongs to the scope enclosing the function, visible
; from the name onward (recursion works, earlier calls stay silent).
((function_definition
   declarator: [
     (function_declarator declarator: (identifier) @definition.function)
     (pointer_declarator declarator: (function_declarator declarator: (identifier) @definition.function))
   ])
 (#set! definition.scope "parent")
 (#set! definition.visibility "declaration"))
; A prototype declares the same name; the default merge folds it into the
; definition's binding.
((declaration
   declarator: [
     (function_declarator declarator: (identifier) @definition.function)
     (pointer_declarator declarator: (function_declarator declarator: (identifier) @definition.function))
   ])
 (#set! definition.visibility "declaration"))

; ── Parameters ───────────────────────────────────────────────────────────
; Only a definition's parameters bind (a prototype's would leak into the
; file scope).
(function_definition
  declarator: (function_declarator
    parameters: (parameter_list
      (parameter_declaration
        declarator: [
          (identifier) @definition.parameter
          (pointer_declarator declarator: (identifier) @definition.parameter)
          (array_declarator declarator: (identifier) @definition.parameter)
        ]))))

; ── Variables: visible after their declaration ───────────────────────────
; Visible from the declarator onward (C's point-of-declaration rule: the
; scope begins just after the declarator, so `int a = 1, b = a;` sees the
; first declarator from the second, and `int a = a;` binds the new local).
((declaration
   declarator: [
     (identifier) @definition
     (pointer_declarator declarator: (identifier) @definition)
     (array_declarator declarator: (identifier) @definition)
     (init_declarator
       declarator: [
         (identifier) @definition
         (pointer_declarator declarator: (identifier) @definition)
         (array_declarator declarator: (identifier) @definition)
       ])
   ])
 (#set! definition.visibility "declaration"))

; ── Types ────────────────────────────────────────────────────────────────
; Tags with a body are definitions (self-references inside the body work);
; a bodiless `struct list` mention is a reference via the type blanket.
((struct_specifier name: (type_identifier) @definition.type body: (_))
 (#set! definition.namespace "type")
 (#set! definition.visibility "declaration"))
((union_specifier name: (type_identifier) @definition.type body: (_))
 (#set! definition.namespace "type")
 (#set! definition.visibility "declaration"))
((enum_specifier name: (type_identifier) @definition.type body: (_))
 (#set! definition.namespace "type")
 (#set! definition.visibility "declaration"))
((type_definition declarator: (type_identifier) @definition.type) @_td
 (#set! definition.namespace "type")
 (#set! definition.visibility "after"))

; Enumerators bind as constants in the enclosing scope.
((enumerator name: (identifier) @definition.constant) @_e
 (#set! definition.visibility "after"))

; ── Preprocessor macros: file-visible from their #define onward ──────────
((preproc_def name: (identifier) @definition.macro)
 (#set! definition.scope "global")
 (#set! definition.visibility "declaration"))
((preproc_function_def name: (identifier) @definition.macro)
 (#set! definition.scope "global")
 (#set! definition.visibility "declaration"))

; ── goto labels: function-wide (goto jumps forward too) ──────────────────
((labeled_statement label: (statement_identifier) @definition.label)
 (#set! definition.scope "nearest:function")
 (#set! definition.namespace "label"))
((goto_statement label: (statement_identifier) @reference)
 (#set! reference.namespace "label"))

; ── References ───────────────────────────────────────────────────────────
; Fields are (field_identifier) — never captured by these blankets.
(identifier) @reference
((type_identifier) @reference
 (#set! reference.namespace "type"))
