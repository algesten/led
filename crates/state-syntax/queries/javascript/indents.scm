; Block delimiters
(_ "{" "}" @end) @indent
(_ "(" ")" @end) @indent
(_ "[" "]" @end) @indent

; Switch case blocks
(switch_case) @indent
(switch_default) @indent

; Continuation constructs (method chains, multi-line expressions)
[
  (member_expression)
  (call_expression)
  (assignment_expression)
  (variable_declarator)
  (await_expression)
] @indent
