("{" @open "}" @close)
("(" @open ")" @close)
("[" @open "]" @close)

(template_string
  "`" @open
  "`" @close)

(("\"" @open "\"" @close)
  (#set! rainbow.exclude))

(("'" @open "'" @close)
  (#set! rainbow.exclude))
