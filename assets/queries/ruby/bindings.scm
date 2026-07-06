; Lexical name bindings for Ruby (lexical-name-resolution ADR).
;
; Methods do not capture enclosing locals (their scope stops the default
; namespace) while blocks do; a block assignment writes an outer local
; when one exists and introduces a block-local otherwise (outer-or-local
; — the ADR's Ruby idiom). Bare identifiers search locals first, then
; methods, mirroring Ruby's local-else-method resolution. Receiver calls
; (obj.run) and setter definitions are member access — never captured.

; ── Scopes ──────────────────────────────────────────────────────────────
; Method scopes stop enclosing locals but see methods, constants, and
; instance variables outward. Class/module scopes open a fresh local
; scope too (a bare name in a class body is a method call, not the outer
; local) but pass only *constants* outward: a nested class does not
; lexically inherit the enclosing class's instance methods or ivars, so
; those namespaces stop at the class boundary (a name defined in the same
; class is still found there, before the gate applies). Class/module
; scopes are also labelled for ivar registration.
((method) @scope
 (#set! scope.inherits "method constant ivar"))
((singleton_method) @scope
 (#set! scope.inherits "method constant ivar"))
((class) @scope.class
 (#set! scope.inherits "constant"))
((module) @scope.class
 (#set! scope.inherits "constant"))
((singleton_class) @scope.class
 (#set! scope.inherits "constant"))
(block) @scope
(do_block) @scope
(lambda) @scope

; ── Methods, classes, modules, constants ─────────────────────────────────
; A definition's name belongs to the scope enclosing the definition node.
((method name: (identifier) @definition.method)
 (#set! definition.scope "parent")
 (#set! definition.namespace "method"))
((singleton_method name: (identifier) @definition.method)
 (#set! definition.scope "parent")
 (#set! definition.namespace "method"))
((class name: (constant) @definition.type)
 (#set! definition.scope "parent")
 (#set! definition.namespace "constant"))
((module name: (constant) @definition.type)
 (#set! definition.scope "parent")
 (#set! definition.namespace "constant"))
((assignment left: (constant) @definition.constant)
 (#set! definition.namespace "constant"))

; ── Locals ────────────────────────────────────────────────────────────────
; Visible from the assignment onward; a block writes outward when the
; name already exists (Ruby's write-outer-or-declare-local rule).
((assignment left: (identifier) @definition) @_a
 (#set! definition.visibility "after")
 (#set! definition.rebind "outer-or-local"))
((left_assignment_list (identifier) @definition) @_a
 (#set! definition.visibility "after")
 (#set! definition.rebind "outer-or-local"))
((operator_assignment left: (identifier) @definition) @_a
 (#set! definition.visibility "after")
 (#set! definition.rebind "outer-or-local"))

; ── Parameters ───────────────────────────────────────────────────────────
(method_parameters (identifier) @definition.parameter)
(block_parameters (identifier) @definition.parameter)
(lambda_parameters (identifier) @definition.parameter)
(optional_parameter name: (identifier) @definition.parameter)
(keyword_parameter name: (identifier) @definition.parameter)
(splat_parameter name: (identifier) @definition.parameter)
(hash_splat_parameter name: (identifier) @definition.parameter)
(block_parameter name: (identifier) @definition.parameter)
(destructured_parameter (identifier) @definition.parameter)

; ── Instance variables: object-wide, registered on the class ────────────
; ACCEPTED APPROXIMATION: every @x registers on the nearest class, so a
; class-body / `def self.` @x (class-object state) and an instance-method
; @x (instance state) merge into one binding though they are different
; object state. Separating them would need a has-ancestor split on
; singleton vs instance context; class-level ivars are uncommon, so this
; is left as-is.
((assignment left: (instance_variable) @definition.ivar)
 (#set! definition.scope "nearest:class")
 (#set! definition.namespace "ivar"))
((instance_variable) @reference
 (#set! reference.namespace "ivar"))

; ── References ───────────────────────────────────────────────────────────
; A bare identifier is a local if one is visible, else a parenless method
; call; a call with a receiver names a member and stays silent.
((identifier) @reference
 (#not-has-parent? @reference "call")
 (#set! reference.namespace "default method"))
((call !receiver method: (identifier) @reference)
 (#set! reference.namespace "method"))
(call receiver: (identifier) @reference)
((constant) @reference
 (#set! reference.namespace "constant"))
