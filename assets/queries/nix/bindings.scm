; Lexical name bindings for Nix (lexical-name-resolution ADR).
;
; Every binding group is mutually recursive (letrec): default
; whole-scope visibility. let-in and rec attrsets bind their attribute
; names; a plain attrset's keys are data, not bindings, and stay
; silent. `inherit name;` is a pure reference to the enclosing binding
; (an attrset attribute is never referenced lexically, so no definition
; is registered). `with expr;` is dynamic — silence. Only the first
; segment of a dotted attrpath (`a.b = …`) binds.

; ── Scopes ──────────────────────────────────────────────────────────────
(let_expression) @scope
(rec_attrset_expression) @scope
(function_expression) @scope.function

; ── Definitions ──────────────────────────────────────────────────────────
(let_expression
  (binding_set
    (binding attrpath: (attrpath . attr: (identifier) @definition))))
(rec_attrset_expression
  (binding_set
    (binding attrpath: (attrpath . attr: (identifier) @definition))))

; Function parameters: `x: …` and `{ pkgs, lib ? default, ... }: …`.
(function_expression universal: (identifier) @definition.parameter)
(formal name: (identifier) @definition.parameter)

; ── References ───────────────────────────────────────────────────────────
(variable_expression name: (identifier) @reference)
; `inherit size;` reads the enclosing binding; the from-set form
; (`inherit (src) attr;`) names attributes of src and stays silent.
(inherit
  !expression
  attrs: (inherited_attrs attr: (identifier) @reference))
