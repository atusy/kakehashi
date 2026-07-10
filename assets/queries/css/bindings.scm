; Lexical name bindings for CSS (lexical-name-resolution ADR).
;
; Two document-wide namespaces: custom properties (`--x: …` answers
; `var(--x)`) and @keyframes names (answering `animation`/
; `animation-name` values). Cascade scoping (a custom property defined
; on a narrower selector) is flattened — resolution is name-based, not
; selector-based. Ordinary properties, selectors, and values carry no
; binding and stay silent. An `animation` shorthand keyword (`ease`,
; `linear`) is captured but resolves only if a same-named @keyframes
; exists.

; ── Custom properties ────────────────────────────────────────────────────
((declaration (property_name) @definition.property)
 (#lua-match? @definition.property "^%-%-")
 (#set! definition.namespace "property"))
((call_expression
   (function_name) @_fn
   (arguments (plain_value) @reference))
 (#eq? @_fn "var")
 (#lua-match? @reference "^%-%-")
 (#set! reference.namespace "property"))

; ── @keyframes ───────────────────────────────────────────────────────────
((keyframes_statement (keyframes_name) @definition.keyframes)
 (#set! definition.namespace "keyframes"))
((declaration (property_name) @_p (plain_value) @reference)
 (#lua-match? @_p "^animation$")
 (#set! reference.namespace "keyframes"))
((declaration (property_name) @_p (plain_value) @reference)
 (#eq? @_p "animation-name")
 (#set! reference.namespace "keyframes"))
