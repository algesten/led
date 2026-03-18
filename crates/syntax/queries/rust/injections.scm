; Comments
(line_comment) @injection.content
  (#set! injection.language "comment")

(block_comment) @injection.content
  (#set! injection.language "comment")

; Macro token trees inject Rust (combined — all macro bodies in one parse)
(macro_invocation
  (token_tree) @injection.content
  (#set! injection.language "rust")
  (#set! injection.combined))
