; inherits: typescript
; Lexical name bindings for TSX (lexical-name-resolution ADR).
;
; JSX needs no extra vocabulary: element names (<Widget />) are plain
; identifiers covered by the inherited reference blanket, expression
; containers ({count}) likewise, and JSX attribute names are
; property_identifier nodes that stay uncaptured — silence over a wrong
; answer.
