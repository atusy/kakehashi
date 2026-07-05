; Lexical name bindings for Dockerfile (lexical-name-resolution ADR).
;
; A Dockerfile is one flat, strictly sequential scope: every definition
; is visible only after its instruction. Multi-stage build names
; (`FROM … AS builder`) live in the "stage" namespace; a FROM base name
; that matches no stage is a registry image and stays silent. Known
; grammar limits (silence, bridge-owned): `COPY --from=stage` is a
; single (param) token, and `$VAR` inside RUN shell text is an opaque
; shell_fragment — neither position can be captured. Per-stage ARG/ENV
; scoping is flattened (stages are not container nodes in the tree).

; ── Build stages ─────────────────────────────────────────────────────────
((from_instruction as: (image_alias) @definition.stage) @_from
 (#set! definition.namespace "stage")
 (#set! definition.visibility "after"))
((from_instruction (image_spec name: (image_name) @reference))
 (#set! reference.namespace "stage"))

; ── ARG / ENV variables ──────────────────────────────────────────────────
((arg_instruction name: (unquoted_string) @definition) @_arg
 (#set! definition.visibility "after"))
((env_pair name: (unquoted_string) @definition) @_env
 (#set! definition.visibility "after"))

; ${VAR} / $VAR expansions in image tags, paths, and values.
(expansion (variable) @reference)
