; Injections for .strand. Strings hold no other language (docs/design.md:
; no mini-languages inside strings), so only comments are injected, for
; editors that highlight TODO-style notes in them.

((comment) @injection.content
  (#set! injection.language "comment"))
