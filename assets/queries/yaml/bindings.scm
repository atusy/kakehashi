; Lexical name bindings for YAML (lexical-name-resolution ADR).
;
; Anchors and aliases: `&base` defines, `*base` references. The YAML
; spec forbids forward aliases, so an anchor is visible only after its
; definition, and re-defining an anchor name shadows it for later
; aliases (each `&name` is a fresh binding). Everything else in a YAML
; document — keys, values, tags — carries no name binding and stays
; silent.

((anchor (anchor_name) @definition.anchor) @_a
 (#set! definition.namespace "anchor")
 (#set! definition.visibility "after")
 (#set! definition.rebind "fresh"))

((alias (alias_name) @reference)
 (#set! reference.namespace "anchor"))
