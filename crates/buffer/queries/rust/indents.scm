; Block delimiters
(_ "{" "}" @end) @indent
(_ "(" ")" @end) @indent
(_ "[" "]" @end) @indent

; Continuation constructs
[
  (where_clause)
  (field_expression)
  (call_expression)
  (assignment_expression)
  (let_declaration)
  (let_chain)
  (await_expression)
] @indent
