; inherits: typescript
; Lexical name bindings for TSX (lexical-name-resolution ADR).
;
; JSX needs no extra vocabulary: element names (<Widget />) are plain
; identifiers covered by the inherited reference blanket, expression
; containers ({count}) likewise, and JSX attribute names are
; property_identifier nodes that stay uncaptured — silence over a wrong
; answer.
;
; ACCEPTED LIMITATION: the inherited blanket does not distinguish an
; uppercase component name (<Widget/>, a real reference) from a lowercase
; intrinsic tag (<div/>, an HTML element). The grammar gives both as bare
; identifiers, and the reference rule lives in the inherited javascript
; asset, so tsx cannot narrow it to uppercase-only without overriding the
; parent. A lowercase intrinsic therefore resolves to a same-named local
; if one is in scope (e.g. `const div = 1`) — rare, since intrinsic names
; are seldom also local bindings. Correctly gating this needs the shared
; reference rule to exclude lowercase jsx element names.
