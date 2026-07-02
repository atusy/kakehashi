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

; Generic type parameters live in the declaring function/class scope.
((type_parameter name: (type_identifier) @definition.type)
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
