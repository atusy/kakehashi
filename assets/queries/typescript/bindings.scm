; inherits: javascript
; Lexical name bindings for TypeScript (lexical-name-resolution ADR).
;
; Inherits the JavaScript rules; the inherited (class_declaration
; name: (identifier)) pattern is impossible against this grammar and is
; dropped by tolerant compilation — the type_identifier form below
; replaces it. Type-shaped resolution (members, generics instantiation)
; stays with the bridge; this file only adds the "type" namespace names.

; ── Type-namespace definitions ───────────────────────────────────────────
((interface_declaration name: (type_identifier) @definition.type)
 (#set! definition.namespace "type"))
((type_alias_declaration name: (type_identifier) @definition.type)
 (#set! definition.namespace "type"))
; Classes and enums are both a type and a value.
((class_declaration name: (type_identifier) @definition.type)
 (#set! definition.namespace "type")
 (#set! definition.visibility "declaration"))
((class_declaration name: (type_identifier) @definition.class)
 (#set! definition.visibility "declaration"))
((enum_declaration name: (identifier) @definition.type)
 (#set! definition.namespace "type"))
(enum_declaration name: (identifier) @definition.enum)

; Generic type parameters resolve only where the declaring construct is
; already a @scope (the function-likes): the definition registers into
; that scope by containment. Class / interface / type-alias generics stay
; uncaptured — those declarations carry no scope, and a root-registered
; binding would merge every same-named <T> in the file into one
; rename/reference set (silence over a wrong answer).
((function_declaration
   type_parameters: (type_parameters (type_parameter name: (type_identifier) @definition.type)))
 (#set! definition.namespace "type"))
((generator_function_declaration
   type_parameters: (type_parameters (type_parameter name: (type_identifier) @definition.type)))
 (#set! definition.namespace "type"))
((function_expression
   type_parameters: (type_parameters (type_parameter name: (type_identifier) @definition.type)))
 (#set! definition.namespace "type"))
((arrow_function
   type_parameters: (type_parameters (type_parameter name: (type_identifier) @definition.type)))
 (#set! definition.namespace "type"))
((method_definition
   type_parameters: (type_parameters (type_parameter name: (type_identifier) @definition.type)))
 (#set! definition.namespace "type"))

; ── TypeScript parameter shapes ──────────────────────────────────────────
(required_parameter pattern: (identifier) @definition.parameter)
(required_parameter pattern: (rest_pattern (identifier) @definition.parameter))
(required_parameter pattern: (object_pattern (shorthand_property_identifier_pattern) @definition.parameter))
(required_parameter pattern: (array_pattern (identifier) @definition.parameter))
(optional_parameter pattern: (identifier) @definition.parameter)

; ── Type references ──────────────────────────────────────────────────────
((type_identifier) @reference
 (#set! reference.namespace "type"))
