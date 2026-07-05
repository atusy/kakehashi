; Lexical name bindings for HCL (lexical-name-resolution ADR).
;
; One flat scope: HCL addresses are file-global. Block labels define
; into per-address-root namespaces — variable "x" answers var.x,
; locals attributes answer local.x, resource/data/module labels answer
; their dotted addresses. A definition site can be the content of a
; quoted label (template_literal), so rename touches the label text and
; every dotted reference together. Attribute traversal past the name
; (aws_instance.web.id → id) is never captured, and unknown roots
; (each, count, self, path, terraform) stay silent.

; ── Definitions ──────────────────────────────────────────────────────────
((block . (identifier) @_b (body (attribute . (identifier) @definition)))
 (#eq? @_b "locals")
 (#set! definition.namespace "local"))
((block . (identifier) @_b . (string_lit (template_literal) @definition))
 (#eq? @_b "variable")
 (#set! definition.namespace "variable"))
((block . (identifier) @_b . (string_lit (template_literal) @definition))
 (#eq? @_b "module")
 (#set! definition.namespace "module"))
((block . (identifier) @_b . (string_lit) . (string_lit (template_literal) @definition))
 (#eq? @_b "resource")
 (#set! definition.namespace "resource"))
((block . (identifier) @_b . (string_lit) . (string_lit (template_literal) @definition))
 (#eq? @_b "data")
 (#set! definition.namespace "data"))

; ── References: the first attribute after a known root ──────────────────
((expression . (variable_expr (identifier) @_root) . (get_attr (identifier) @reference))
 (#eq? @_root "var")
 (#set! reference.namespace "variable"))
((expression . (variable_expr (identifier) @_root) . (get_attr (identifier) @reference))
 (#eq? @_root "local")
 (#set! reference.namespace "local"))
((expression . (variable_expr (identifier) @_root) . (get_attr (identifier) @reference))
 (#eq? @_root "module")
 (#set! reference.namespace "module"))
; data.<type>.<name>: the name is the second attribute.
((expression . (variable_expr (identifier) @_root) . (get_attr) . (get_attr (identifier) @reference))
 (#eq? @_root "data")
 (#set! reference.namespace "data"))
; <type>.<name> with no reserved root is a resource address.
((expression . (variable_expr (identifier) @_root) . (get_attr (identifier) @reference))
 (#not-eq? @_root "var")
 (#not-eq? @_root "local")
 (#not-eq? @_root "module")
 (#not-eq? @_root "data")
 (#not-eq? @_root "each")
 (#not-eq? @_root "count")
 (#not-eq? @_root "self")
 (#not-eq? @_root "path")
 (#not-eq? @_root "terraform")
 (#set! reference.namespace "resource"))
