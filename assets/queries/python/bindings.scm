; Lexical name bindings for Python (lexical-name-resolution ADR).
;
; A name assigned anywhere in a scope is local to the *entire* scope
; (mirroring UnboundLocalError), so every definition keeps the default
; "scope" visibility. Attribute access (a.b) and keyword arguments are
; never treated as lexical references.

; ── Scopes ──────────────────────────────────────────────────────────────
(function_definition) @scope.function
(lambda) @scope
(list_comprehension) @scope
(set_comprehension) @scope
(dictionary_comprehension) @scope
(generator_expression) @scope
; Class bodies are invisible to nested scopes (methods, comprehensions);
; statements directly in the body still see class-level names.
((class_definition body: (block) @scope)
 (#set! scope.visible-to-nested "false"))

; ── Definitions ──────────────────────────────────────────────────────────
; A def's name belongs to the scope enclosing the function.
((function_definition name: (identifier) @definition.function)
 (#set! definition.scope "parent"))
(class_definition name: (identifier) @definition.class)

(assignment left: (identifier) @definition)
(assignment left: (pattern_list (identifier) @definition))
(assignment left: (tuple_pattern (identifier) @definition))
(augmented_assignment left: (identifier) @definition)
(named_expression name: (identifier) @definition)

(parameters (identifier) @definition.parameter)
(parameters (default_parameter name: (identifier) @definition.parameter))
(parameters (typed_parameter (identifier) @definition.parameter))
(parameters (typed_default_parameter name: (identifier) @definition.parameter))
(parameters (list_splat_pattern (identifier) @definition.parameter))
(parameters (dictionary_splat_pattern (identifier) @definition.parameter))
(lambda_parameters (identifier) @definition.parameter)

(for_statement left: (identifier) @definition)
(for_statement left: (pattern_list (identifier) @definition))
(for_statement left: (tuple_pattern (identifier) @definition))
(for_in_clause left: (identifier) @definition)
(for_in_clause left: (tuple_pattern (identifier) @definition))

; `with ... as x` and `except E as x`.
(as_pattern alias: (as_pattern_target (identifier) @definition))

; Imports bind the local alias (single-segment names only — `import a.b`
; binds `a`, and dotted paths are module resolution, not lexical).
(import_statement name: (dotted_name . (identifier) @definition.import .))
(import_from_statement name: (dotted_name . (identifier) @definition.import .))
(aliased_import alias: (identifier) @definition.import)

; ── Redirects ────────────────────────────────────────────────────────────
((global_statement (identifier) @redirect)
 (#set! redirect.target "global"))
((nonlocal_statement (identifier) @redirect)
 (#set! redirect.target "nearest-binding:function"))

; ── References ───────────────────────────────────────────────────────────
((identifier) @reference
 (#not-has-parent? @reference "attribute" "keyword_argument"))
(attribute object: (identifier) @reference)
